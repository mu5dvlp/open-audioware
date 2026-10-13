//! Windows 用の自前 WASAPI バックエンド(AUDIOWARE-DEPS-PLAN.md ステップ4-3c)。
//!
//! 4-1〜4-3([`crate::native_backend::apple`] / [`crate::native_backend::android`] /
//! [`crate::native_backend::linux`])と同じく**新しい外部クレートは追加していない**
//! (`windows` / `windows-sys` / `wasapi` は使わない)。COM のインターフェースは
//! 自前の vtable 構造体で宣言し、`CoCreateInstance` などの関数は `extern "system"` で
//! 宣言して `ole32` / `kernel32` / `avrt` へリンクする。いずれも Windows に標準で入っている DLL。
//!
//! # 方式: 共有モード・イベント駆動
//!
//! 専用スレッドが `WaitForSingleObject(イベント)` で WASAPI の「バッファに空きができた」
//! 通知を待ち、`IAudioClient::GetCurrentPadding` で空きフレーム数を取り、
//! `IAudioRenderClient::GetBuffer` が返したバッファへ**直接** `Renderer::render` して
//! `ReleaseBuffer` する。
//!
//! 手順: `CoInitializeEx(MTA)` → 既定のレンダーエンドポイント(`eRender` / `eConsole`)→
//! `IAudioClient::Initialize(SHARED, EVENTCALLBACK | AUTOCONVERTPCM | SRC_DEFAULT_QUALITY,
//! 既定のデバイス周期)`。形式は **f32・2ch・48kHz を要求**する(cpal 版が 48kHz を要求する
//! のと同じ。エンドポイントのミックス形式が別のレートでも `AUTOCONVERTPCM` で
//! オーディオエンジンが変換する)。
//!
//! 🔴 COM の呼び出し(初期化・開始・音声ループ・終了)は**すべて 1 本のスレッド**で行う。
//! `open` を呼んだゲームスレッド(Unity ならメインスレッド。STA のことがある)のアパートメントに
//! 依存しないためで、結果だけをチャネルで `open` へ返す。
//!
//! # タイムスタンプ
//!
//! 今回書くバッファの先頭が装置位置に届く時刻は、`IAudioClock::GetPosition` が返す
//! (位置, その位置を読んだ QPC)と、開始からここまでに書いたフレーム数から求める
//! ([`clock_playback_start_ns`]): QPC 時刻 + (書き込み済み − 再生済み)のフレームぶん。
//! QPC は [`crate::host_time::host_time_ns`] と同じ時計なので `mw_host_time_ns()` と
//! 直接比較できる。出力レイテンシ(`output_latency_ns`)はこの時刻とコールバックに入った時刻の差。
//!
//! 🔴 cpal の WASAPI ホスト(と、この実装の初版)は「バッファに残っているフレーム
//! (`GetCurrentPadding`)+ `GetStreamLatency`」で求めていたが、エンジン周期とドライバのぶん
//! (実機で約 20ms)を含まず実際より小さい。`IAudioClock` が読めないときだけ、その式
//! ([`output_latency_ns`])に落とす。
//!
//! # アンダーラン
//!
//! WASAPI は xrun の回数を出さない。他の OS と同じ「コールバックの間隔が想定(= デバイス周期)
//! より開いた」検知([`OutputUnderrunTracker::observe`])だけを回す。
//!
//! # 既知の差分
//!
//! - 音声スレッドは MMCSS(`AvSetMmThreadCharacteristicsW("Pro Audio")`)に登録する
//!   (cpal の WASAPI ホストが `audio_thread_priority` で行うのと同じ目的)。失敗しても続行する。
//! - デバイスの変化は `IMMNotificationClient`(自前の COM)で監視する: 出力の既定
//!   (`eRender` の `eConsole` / `eMultimedia`)が別のデバイスへ変わった、使用中のデバイスが
//!   無効化された・取り除かれた、のどちらかで音声スレッドがループを抜け、
//!   `Event::StreamError { DeviceUnavailable }` を積む(`mw-ffi` の内部再オープンが新しい既定へ
//!   開き直す)。通知は任意のスレッドから来るので、コールバックは合図を立てて `SetEvent` するだけ。
//!   通知が取れなくても、無効化されたデバイスへの `GetCurrentPadding` / `GetBuffer` が
//!   `AUDCLNT_E_DEVICE_INVALIDATED` を返したときの同じエラーは残っている。
//! - 低遅延の `IAudioClient3`(共有モードの最小周期)は使わない。
//!
//! 純粋ロジック(形式の組み立て・遅延計算・エラー分類・vtable の配置)は `target_os` を問わず
//! コンパイルし、ホストの `cargo test` で固定している([`crate::native_backend::linux`] と同じ流儀)。

use std::ffi::c_void;

use mw_core::{CHANNELS, StreamErrorReason};

#[cfg(target_os = "windows")]
use std::panic::{self, AssertUnwindSafe};
#[cfg(target_os = "windows")]
use std::sync::Arc;
#[cfg(target_os = "windows")]
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
#[cfg(target_os = "windows")]
use std::sync::mpsc::{SyncSender, sync_channel};
#[cfg(target_os = "windows")]
use std::thread::JoinHandle;
#[cfg(target_os = "windows")]
use std::time::Duration;

#[cfg(target_os = "windows")]
use mw_core::{Event, EventQueue, Renderer};

#[cfg(target_os = "windows")]
use crate::backend::{Backend, BackendError};
#[cfg(target_os = "windows")]
use crate::host_time::host_time_ns;
#[cfg(target_os = "windows")]
use crate::underrun::OutputUnderrunTracker;

// ============================================================================
// 純関数・定数(ハードウェア無しで `cargo test`(ホスト)から固定化できる)
// ============================================================================
//
// `pub` はモジュール外へ公開する意図ではなく、Windows 以外では呼び出し元が `#[cfg]` で
// 存在せず `dead_code` になるのを避けるため([`crate::native_backend::linux`] と同じ事情)。

/// 目標サンプルレート。エンドポイントのミックス形式と違っても `AUTOCONVERTPCM` で変換される。
pub const TARGET_SAMPLE_RATE_HZ: u32 = 48_000;

/// 1 回の処理フレーム数の上限(`GetBuffer` が返した空きがこれを超えても、一度に扱うのはここまで)。
pub const MAX_RENDER_FRAMES: u32 = 16_384;

/// 初期化の完了を待つ上限。COM の初期化・デバイスの起動が詰まっても `open` が戻るようにする。
pub const INIT_TIMEOUT_MS: u64 = 10_000;

/// イベント待ちのタイムアウトがこの回数続いたら、装置の状態を問い合わせる(約 1 秒)。
pub const TIMEOUTS_BEFORE_PROBE: u32 = 5;

/// イベント待ちのタイムアウト(ms)。`close` は `SetEvent` で起こすので、これは保険。
pub const WAIT_TIMEOUT_MS: u32 = 200;

/// `HRESULT`(符号付き 32 ビット。負が失敗)。
pub type Hresult = i32;

/// `HRESULT_FROM_WIN32(ERROR_NOT_FOUND)` = `E_NOTFOUND`。既定のエンドポイントが無いときに返る。
pub const E_NOTFOUND: Hresult = 0x8007_0490_u32 as i32;
/// `E_ACCESSDENIED`。
pub const E_ACCESSDENIED: Hresult = 0x8007_0005_u32 as i32;
/// `AUDCLNT_E_DEVICE_INVALIDATED`(抜かれた・無効化された)。
pub const AUDCLNT_E_DEVICE_INVALIDATED: Hresult = 0x8889_0004_u32 as i32;
/// `AUDCLNT_E_UNSUPPORTED_FORMAT`。
pub const AUDCLNT_E_UNSUPPORTED_FORMAT: Hresult = 0x8889_0008_u32 as i32;
/// `AUDCLNT_E_DEVICE_IN_USE`。
pub const AUDCLNT_E_DEVICE_IN_USE: Hresult = 0x8889_000A_u32 as i32;
/// `AUDCLNT_E_ENDPOINT_CREATE_FAILED`。
pub const AUDCLNT_E_ENDPOINT_CREATE_FAILED: Hresult = 0x8889_000F_u32 as i32;
/// `AUDCLNT_E_SERVICE_NOT_RUNNING`(Windows Audio サービスが止まっている)。
pub const AUDCLNT_E_SERVICE_NOT_RUNNING: Hresult = 0x8889_0010_u32 as i32;
/// `AUDCLNT_E_RESOURCES_INVALIDATED`。
pub const AUDCLNT_E_RESOURCES_INVALIDATED: Hresult = 0x8889_0026_u32 as i32;

/// `GUID`(COM の識別子)。
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Guid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

/// `CLSID_MMDeviceEnumerator`。
pub const CLSID_MM_DEVICE_ENUMERATOR: Guid = Guid {
    data1: 0xBCDE_0395,
    data2: 0xE52F,
    data3: 0x467C,
    data4: [0x8E, 0x3D, 0xC4, 0x57, 0x92, 0x91, 0x69, 0x2E],
};
/// `IID_IMMDeviceEnumerator`。
pub const IID_IMM_DEVICE_ENUMERATOR: Guid = Guid {
    data1: 0xA956_64D2,
    data2: 0x9614,
    data3: 0x4F35,
    data4: [0xA7, 0x46, 0xDE, 0x8D, 0xB6, 0x36, 0x17, 0xE6],
};
/// `IID_IUnknown`。
pub const IID_IUNKNOWN: Guid = Guid {
    data1: 0x0000_0000,
    data2: 0x0000,
    data3: 0x0000,
    data4: [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
};
/// `IID_IMMNotificationClient`。
pub const IID_IMM_NOTIFICATION_CLIENT: Guid = Guid {
    data1: 0x7991_EEC9,
    data2: 0x7E89,
    data3: 0x4D85,
    data4: [0x83, 0x90, 0x6C, 0x70, 0x3C, 0xEC, 0x60, 0xC0],
};
/// `IID_IAudioClient`。
pub const IID_IAUDIO_CLIENT: Guid = Guid {
    data1: 0x1CB9_AD4C,
    data2: 0xDBFA,
    data3: 0x4C32,
    data4: [0xB1, 0x78, 0xC2, 0xF5, 0x68, 0xA7, 0x03, 0xB2],
};
/// `IID_IAudioRenderClient`。
pub const IID_IAUDIO_RENDER_CLIENT: Guid = Guid {
    data1: 0xF294_ACFC,
    data2: 0x3146,
    data3: 0x4483,
    data4: [0xA7, 0xBF, 0xAD, 0xDC, 0xA7, 0xC2, 0x60, 0xE2],
};
/// `IID_IAudioClock`。
pub const IID_IAUDIO_CLOCK: Guid = Guid {
    data1: 0xCD63_314F,
    data2: 0x3FBA,
    data3: 0x4A1B,
    data4: [0x81, 0x2C, 0xEF, 0x96, 0x35, 0x87, 0x28, 0xE7],
};
/// `KSDATAFORMAT_SUBTYPE_IEEE_FLOAT`。
pub const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: Guid = Guid {
    data1: 0x0000_0003,
    data2: 0x0000,
    data3: 0x0010,
    data4: [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71],
};

/// `WAVEFORMATEX`(Windows のヘッダと同じ 1 バイト詰め)。
#[repr(C, packed(1))]
#[derive(Clone, Copy)]
pub struct WaveFormatEx {
    pub format_tag: u16,
    pub channels: u16,
    pub samples_per_sec: u32,
    pub avg_bytes_per_sec: u32,
    pub block_align: u16,
    pub bits_per_sample: u16,
    pub cb_size: u16,
}

/// `WAVEFORMATEXTENSIBLE`。
#[repr(C, packed(1))]
#[derive(Clone, Copy)]
pub struct WaveFormatExtensible {
    pub format: WaveFormatEx,
    pub valid_bits_per_sample: u16,
    pub channel_mask: u32,
    pub sub_format: Guid,
}

/// `WAVE_FORMAT_EXTENSIBLE`。
pub const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
/// `SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT`。
pub const SPEAKER_MASK_STEREO: u32 = 0x3;

/// 要求する出力形式(f32・インターリーブ・`CHANNELS` ch・`sample_rate` Hz)。
pub fn float_stereo_format(sample_rate: u32) -> WaveFormatExtensible {
    let channels = CHANNELS as u16;
    let bits = 32u16;
    let block_align = channels * (bits / 8);
    WaveFormatExtensible {
        format: WaveFormatEx {
            format_tag: WAVE_FORMAT_EXTENSIBLE,
            channels,
            samples_per_sec: sample_rate,
            avg_bytes_per_sec: sample_rate.saturating_mul(block_align as u32),
            block_align,
            bits_per_sample: bits,
            // `WAVEFORMATEXTENSIBLE` が `WAVEFORMATEX` の後ろに足す分(2 + 4 + 16)。
            cb_size: 22,
        },
        valid_bits_per_sample: bits,
        channel_mask: SPEAKER_MASK_STEREO,
        sub_format: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
    }
}

/// 100ns 単位(WASAPI の `REFERENCE_TIME`)を ns へ。負は 0、`u64` を超える分は飽和する。
pub fn hns_to_ns(hns: i64) -> u64 {
    if hns <= 0 {
        return 0;
    }
    (hns as u64).saturating_mul(100)
}

/// フレーム数を ns へ。`sample_rate == 0` は 0。
pub fn frames_to_ns(frames: u64, sample_rate: u32) -> u64 {
    if sample_rate == 0 {
        return 0;
    }
    let ns = frames as u128 * 1_000_000_000 / sample_rate as u128;
    ns.min(u64::MAX as u128) as u64
}

/// 100ns 単位の時間をフレーム数へ(切り捨て。`sample_rate` を掛けるので `u128` で計算する)。
pub fn hns_to_frames(hns: i64, sample_rate: u32) -> u32 {
    if hns <= 0 {
        return 0;
    }
    let frames = hns as u128 * sample_rate as u128 / 10_000_000;
    frames.min(u32::MAX as u128) as u32
}

/// 出力レイテンシ(ns)= バッファに残っている未再生分 + `GetStreamLatency`。
pub fn output_latency_ns(padding_frames: u32, sample_rate: u32, stream_latency_hns: i64) -> u64 {
    frames_to_ns(padding_frames as u64, sample_rate).saturating_add(hns_to_ns(stream_latency_hns))
}

/// 今回書くバッファの先頭が出力される(と予測される)ホスト単調時刻(ns)。
pub fn playback_start_ns(callback_ns: u64, latency_ns: u64) -> u64 {
    callback_ns.saturating_add(latency_ns)
}

/// `IAudioClock` から、次に書く先頭フレームが出力される(と予測される)ホスト単調時刻(ns)を求める。
///
/// `position` / `frequency` は `GetPosition` / `GetFrequency`(位置 ÷ 周波数 = 秒)、
/// `position_qpc_100ns` は位置を読んだ時刻(QPC を 100ns 単位にしたもの)、
/// `written_frames` は開始からここまでに `ReleaseBuffer` したフレーム数。
/// 再生済みのフレーム数との差が「書き込み済みで未再生」で、そのぶん先が書く先頭の出力時刻。
/// `frequency == 0` や `sample_rate == 0` は `None`。
pub fn clock_playback_start_ns(
    position: u64,
    frequency: u64,
    position_qpc_100ns: u64,
    written_frames: u64,
    sample_rate: u32,
) -> Option<u64> {
    if frequency == 0 || sample_rate == 0 {
        return None;
    }
    let played =
        (position as u128 * sample_rate as u128 / frequency as u128).min(u64::MAX as u128) as u64;
    let unplayed = written_frames.saturating_sub(played);
    Some(
        position_qpc_100ns
            .saturating_mul(100)
            .saturating_add(frames_to_ns(unplayed, sample_rate)),
    )
}

/// 今回書ける(空いている)フレーム数。`padding` が `buffer` を超える異常値は 0、
/// 上限は [`MAX_RENDER_FRAMES`]。
pub fn writable_frames(buffer_frames: u32, padding_frames: u32) -> u32 {
    buffer_frames
        .saturating_sub(padding_frames)
        .min(MAX_RENDER_FRAMES)
}

/// `HRESULT` を `0x88890004` の形に整形する(ログ・エラー文言用)。
pub fn hresult_hex(hr: Hresult) -> String {
    format!("0x{:08X}", hr as u32)
}

/// 開く途中(エンドポイント取得〜`Initialize`)の失敗を `BackendError` へ分類する。
/// `stage` は失敗した呼び出しの名前。デバイスが無い(`E_NOTFOUND` /
/// `AUDCLNT_E_ENDPOINT_CREATE_FAILED`)は `NoOutputDevice`、それ以外は `BuildStreamFailed`。
pub fn classify_open_error(stage: &str, hr: Hresult) -> crate::backend::BackendError {
    match hr {
        E_NOTFOUND | AUDCLNT_E_ENDPOINT_CREATE_FAILED => {
            crate::backend::BackendError::NoOutputDevice
        }
        _ => crate::backend::BackendError::BuildStreamFailed(format!(
            "WASAPI {stage} failed: {} ({})",
            describe_hresult(hr),
            hresult_hex(hr)
        )),
    }
}

/// 動いている最中の失敗を `Event::StreamError` の理由へ分類する。
/// 切断・サービス停止・使用中・リソース無効化は `DeviceUnavailable`(`mw-ffi` の内部再オープンの対象)、
/// アクセス拒否は `PermissionDenied`、他は `Backend`。
pub fn classify_stream_error(hr: Hresult) -> StreamErrorReason {
    match hr {
        AUDCLNT_E_DEVICE_INVALIDATED
        | AUDCLNT_E_SERVICE_NOT_RUNNING
        | AUDCLNT_E_ENDPOINT_CREATE_FAILED
        | AUDCLNT_E_DEVICE_IN_USE
        | AUDCLNT_E_RESOURCES_INVALIDATED => StreamErrorReason::DeviceUnavailable,
        E_ACCESSDENIED => StreamErrorReason::PermissionDenied,
        _ => StreamErrorReason::Backend,
    }
}

/// 既知の `HRESULT` の短い名前(ログ用。未知は `unknown`)。
pub fn describe_hresult(hr: Hresult) -> &'static str {
    match hr {
        E_NOTFOUND => "E_NOTFOUND",
        E_ACCESSDENIED => "E_ACCESSDENIED",
        AUDCLNT_E_DEVICE_INVALIDATED => "AUDCLNT_E_DEVICE_INVALIDATED",
        AUDCLNT_E_UNSUPPORTED_FORMAT => "AUDCLNT_E_UNSUPPORTED_FORMAT",
        AUDCLNT_E_DEVICE_IN_USE => "AUDCLNT_E_DEVICE_IN_USE",
        AUDCLNT_E_ENDPOINT_CREATE_FAILED => "AUDCLNT_E_ENDPOINT_CREATE_FAILED",
        AUDCLNT_E_SERVICE_NOT_RUNNING => "AUDCLNT_E_SERVICE_NOT_RUNNING",
        AUDCLNT_E_RESOURCES_INVALIDATED => "AUDCLNT_E_RESOURCES_INVALIDATED",
        _ => "unknown",
    }
}

// ============================================================================
// COM の宣言(自前。新しいクレートは追加しない)
// ============================================================================
//
// vtable は C++ の仮想関数テーブルと同じ並び(`IUnknown` の 3 つ + 各インターフェース固有)。
// 使わないスロットも位置を保つために `*const c_void` で残す。並びは下のテストで
// `offset_of!` により固定している。

type ThisPtr = *mut c_void;

/// `IUnknown` の vtable。
#[repr(C)]
pub struct IUnknownVtbl {
    pub query_interface: *const c_void,
    pub add_ref: unsafe extern "system" fn(ThisPtr) -> u32,
    pub release: unsafe extern "system" fn(ThisPtr) -> u32,
}

/// `IMMDeviceEnumerator` の vtable。
#[repr(C)]
pub struct IMmDeviceEnumeratorVtbl {
    pub unknown: IUnknownVtbl,
    pub enum_audio_endpoints: *const c_void,
    pub get_default_audio_endpoint:
        unsafe extern "system" fn(ThisPtr, i32, i32, *mut ThisPtr) -> Hresult,
    pub get_device: *const c_void,
    pub register_endpoint_notification_callback:
        unsafe extern "system" fn(ThisPtr, ThisPtr) -> Hresult,
    pub unregister_endpoint_notification_callback:
        unsafe extern "system" fn(ThisPtr, ThisPtr) -> Hresult,
}

/// `IMMDevice` の vtable。
#[repr(C)]
pub struct IMmDeviceVtbl {
    pub unknown: IUnknownVtbl,
    pub activate: unsafe extern "system" fn(
        ThisPtr,
        *const Guid,
        u32,
        *const c_void,
        *mut ThisPtr,
    ) -> Hresult,
    pub open_property_store: *const c_void,
    pub get_id: unsafe extern "system" fn(ThisPtr, *mut *mut u16) -> Hresult,
    pub get_state: *const c_void,
}

/// `IAudioClient` の vtable。
#[repr(C)]
pub struct IAudioClientVtbl {
    pub unknown: IUnknownVtbl,
    pub initialize: unsafe extern "system" fn(
        ThisPtr,
        i32,
        u32,
        i64,
        i64,
        *const WaveFormatExtensible,
        *const Guid,
    ) -> Hresult,
    pub get_buffer_size: unsafe extern "system" fn(ThisPtr, *mut u32) -> Hresult,
    pub get_stream_latency: unsafe extern "system" fn(ThisPtr, *mut i64) -> Hresult,
    pub get_current_padding: unsafe extern "system" fn(ThisPtr, *mut u32) -> Hresult,
    pub is_format_supported: *const c_void,
    pub get_mix_format: unsafe extern "system" fn(ThisPtr, *mut *mut WaveFormatEx) -> Hresult,
    pub get_device_period: unsafe extern "system" fn(ThisPtr, *mut i64, *mut i64) -> Hresult,
    pub start: unsafe extern "system" fn(ThisPtr) -> Hresult,
    pub stop: unsafe extern "system" fn(ThisPtr) -> Hresult,
    pub reset: unsafe extern "system" fn(ThisPtr) -> Hresult,
    pub set_event_handle: unsafe extern "system" fn(ThisPtr, *mut c_void) -> Hresult,
    pub get_service: unsafe extern "system" fn(ThisPtr, *const Guid, *mut ThisPtr) -> Hresult,
}

/// `IAudioRenderClient` の vtable。
#[repr(C)]
pub struct IAudioRenderClientVtbl {
    pub unknown: IUnknownVtbl,
    pub get_buffer: unsafe extern "system" fn(ThisPtr, u32, *mut *mut u8) -> Hresult,
    pub release_buffer: unsafe extern "system" fn(ThisPtr, u32, u32) -> Hresult,
}

/// `IAudioClock` の vtable。
#[repr(C)]
pub struct IAudioClockVtbl {
    pub unknown: IUnknownVtbl,
    pub get_frequency: unsafe extern "system" fn(ThisPtr, *mut u64) -> Hresult,
    pub get_position: unsafe extern "system" fn(ThisPtr, *mut u64, *mut u64) -> Hresult,
    pub get_characteristics: *const c_void,
}

/// `AUDCLNT_SHAREMODE_SHARED`。
pub const AUDCLNT_SHAREMODE_SHARED: i32 = 0;
/// `AUDCLNT_STREAMFLAGS_EVENTCALLBACK`。
pub const AUDCLNT_STREAMFLAGS_EVENTCALLBACK: u32 = 0x0004_0000;
/// `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM`。
pub const AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM: u32 = 0x8000_0000;
/// `AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY`。
pub const AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY: u32 = 0x0800_0000;
/// `AUDCLNT_BUFFERFLAGS_SILENT`。
pub const AUDCLNT_BUFFERFLAGS_SILENT: u32 = 0x2;
/// `EDataFlow::eRender`。
pub const E_RENDER: i32 = 0;
/// `ERole::eConsole`。
pub const E_CONSOLE: i32 = 0;
/// `ERole::eMultimedia`。
pub const E_MULTIMEDIA: i32 = 1;
/// `DEVICE_STATE_ACTIVE`(これ以外はデバイスが使えない: 無効・未接続・抜かれた)。
pub const DEVICE_STATE_ACTIVE: u32 = 0x1;
/// `E_NOINTERFACE`。
pub const E_NOINTERFACE: Hresult = 0x8000_4002_u32 as i32;
/// `CLSCTX_ALL`。
pub const CLSCTX_ALL: u32 = 0x17;
/// `COINIT_MULTITHREADED`。
pub const COINIT_MULTITHREADED: u32 = 0;

/// `PROPERTYKEY`。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PropertyKey {
    pub fmtid: Guid,
    pub pid: u32,
}

/// `IMMNotificationClient` の vtable。実装は自前のオブジェクト(`NotificationClient`)が持つ。
#[repr(C)]
pub struct IMmNotificationClientVtbl {
    pub query_interface:
        unsafe extern "system" fn(ThisPtr, *const Guid, *mut *mut c_void) -> Hresult,
    pub add_ref: unsafe extern "system" fn(ThisPtr) -> u32,
    pub release: unsafe extern "system" fn(ThisPtr) -> u32,
    pub on_device_state_changed: unsafe extern "system" fn(ThisPtr, *const u16, u32) -> Hresult,
    pub on_device_added: unsafe extern "system" fn(ThisPtr, *const u16) -> Hresult,
    pub on_device_removed: unsafe extern "system" fn(ThisPtr, *const u16) -> Hresult,
    pub on_default_device_changed:
        unsafe extern "system" fn(ThisPtr, i32, i32, *const u16) -> Hresult,
    pub on_property_value_changed:
        unsafe extern "system" fn(ThisPtr, *const u16, PropertyKey) -> Hresult,
}

/// `IMMNotificationClient` が受ける通知のうち、出力の追従に関わるもの。
/// デバイス ID は NUL を含まない UTF-16。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceEvent<'a> {
    /// 既定のデバイスが変わった(`device_id` が `None` なら既定が無くなった)。
    DefaultChanged {
        flow: i32,
        role: i32,
        device_id: Option<&'a [u16]>,
    },
    /// デバイスの状態が変わった(`DEVICE_STATE_*`)。
    StateChanged { device_id: &'a [u16], state: u32 },
    /// デバイスが取り除かれた。
    Removed { device_id: &'a [u16] },
}

/// 通知を受けたとき、開いているストリームを開き直すべきか。
///
/// - 出力(`eRender`)の既定が、`eConsole` か `eMultimedia` で、いま使っているのとは別の
///   デバイスへ変わった。
/// - いま使っているデバイスが、使えない状態(無効・未接続・抜かれた)になった・取り除かれた。
///
/// それ以外(他のデバイス・入力・`eCommunications`・追加・プロパティ変更)は開き直さない。
pub fn should_reopen(event: DeviceEvent<'_>, current_id: &[u16]) -> bool {
    match event {
        DeviceEvent::DefaultChanged {
            flow,
            role,
            device_id,
        } => {
            flow == E_RENDER
                && (role == E_CONSOLE || role == E_MULTIMEDIA)
                && device_id.is_some_and(|id| id != current_id)
        }
        DeviceEvent::StateChanged { device_id, state } => {
            device_id == current_id && state != DEVICE_STATE_ACTIVE
        }
        DeviceEvent::Removed { device_id } => device_id == current_id,
    }
}

/// NUL 終端の UTF-16 文字列を、NUL を除いたスライスとして借りる。
/// 長さが `MAX_DEVICE_ID_UNITS` を超える(終端が見つからない)ものは `None`。
///
/// # Safety
/// `ptr` は NULL か、少なくとも NUL まで(または `MAX_DEVICE_ID_UNITS` 要素)読める領域を指し、
/// 戻り値の寿命のあいだ書き換えられないこと。
pub unsafe fn wide_str_from_ptr<'a>(ptr: *const u16) -> Option<&'a [u16]> {
    if ptr.is_null() {
        return None;
    }
    for len in 0..MAX_DEVICE_ID_UNITS {
        // SAFETY: 呼び出し側の保証により、NUL までの各要素は読める。
        if unsafe { *ptr.add(len) } == 0 {
            // SAFETY: 先頭から `len` 要素は上で読めることを確かめた。
            return Some(unsafe { std::slice::from_raw_parts(ptr, len) });
        }
    }
    None
}

/// デバイス ID の長さの上限(UTF-16 の要素数)。
pub const MAX_DEVICE_ID_UNITS: usize = 1024;

/// 共有モードの初期化フラグ(イベント駆動 + 形式変換)。
pub const INIT_STREAM_FLAGS: u32 = AUDCLNT_STREAMFLAGS_EVENTCALLBACK
    | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
    | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;

#[cfg(target_os = "windows")]
const WAIT_OBJECT_0: u32 = 0;
#[cfg(target_os = "windows")]
const WAIT_TIMEOUT: u32 = 0x0000_0102;

#[cfg(target_os = "windows")]
#[link(name = "ole32")]
unsafe extern "system" {
    fn CoInitializeEx(reserved: *const c_void, co_init: u32) -> Hresult;
    fn CoUninitialize();
    fn CoCreateInstance(
        rclsid: *const Guid,
        outer: *mut c_void,
        cls_context: u32,
        riid: *const Guid,
        out: *mut *mut c_void,
    ) -> Hresult;
    fn CoTaskMemFree(ptr: *mut c_void);
}

#[cfg(target_os = "windows")]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateEventW(
        attributes: *const c_void,
        manual_reset: i32,
        initial_state: i32,
        name: *const u16,
    ) -> *mut c_void;
    fn SetEvent(handle: *mut c_void) -> i32;
    fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
    fn CloseHandle(handle: *mut c_void) -> i32;
}

#[cfg(target_os = "windows")]
#[link(name = "avrt")]
unsafe extern "system" {
    fn AvSetMmThreadCharacteristicsW(task_name: *const u16, task_index: *mut u32) -> *mut c_void;
    fn AvRevertMmThreadCharacteristics(handle: *mut c_void) -> i32;
}

/// `"Pro Audio"` の UTF-16(NUL 終端)。
#[cfg(target_os = "windows")]
const MMCSS_TASK_NAME: [u16; 10] = [
    b'P' as u16,
    b'r' as u16,
    b'o' as u16,
    b' ' as u16,
    b'A' as u16,
    b'u' as u16,
    b'd' as u16,
    b'i' as u16,
    b'o' as u16,
    0,
];

/// COM オブジェクトの先頭(vtable へのポインタ)。
#[repr(C)]
pub struct ComObject<V> {
    vtbl: *const V,
}

/// COM インターフェースの所有ポインタ。`Drop` で `Release` する。
///
/// 不変条件: `ptr` は非 NULL で、`Release` するまで有効な `V` 形の COM オブジェクトを指す。
/// スレッド間で移さない(`Send` を実装しない)——COM の呼び出しは作ったスレッドだけで行う。
#[cfg(target_os = "windows")]
struct Com<V> {
    ptr: *mut ComObject<V>,
}

#[cfg(target_os = "windows")]
impl<V> Com<V> {
    /// # Safety
    /// `ptr` は NULL か、参照を 1 つ所有する `V` 形の COM インターフェースであること。
    unsafe fn from_raw(ptr: *mut c_void) -> Option<Self> {
        if ptr.is_null() {
            None
        } else {
            Some(Self { ptr: ptr.cast() })
        }
    }

    fn this(&self) -> *mut c_void {
        self.ptr.cast()
    }

    fn vtbl(&self) -> &V {
        // SAFETY: 不変条件により `ptr` は有効で、その先頭は有効な vtable へのポインタ。
        // `Com` が生きている間は `Release` されない。
        unsafe { &*(*self.ptr).vtbl }
    }
}

#[cfg(target_os = "windows")]
impl<V> Drop for Com<V> {
    fn drop(&mut self) {
        // SAFETY: すべての COM インターフェースの vtable は `IUnknown` の 3 スロットで始まるので、
        // `IUnknownVtbl` として読んでよい。参照を 1 つ所有しているので `Release` は 1 回。
        unsafe {
            let unknown = (*self.ptr).vtbl.cast::<IUnknownVtbl>();
            ((*unknown).release)(self.this());
        }
    }
}

/// `CoInitializeEx` / `CoUninitialize` の対。
#[cfg(target_os = "windows")]
struct ComApartment;

#[cfg(target_os = "windows")]
impl ComApartment {
    fn enter() -> Result<Self, Hresult> {
        // SAFETY: 予約引数は NULL。成功(`S_OK` / `S_FALSE`)したら `Drop` で対になる
        // `CoUninitialize` を呼ぶ。
        let hr = unsafe { CoInitializeEx(std::ptr::null(), COINIT_MULTITHREADED) };
        if hr < 0 { Err(hr) } else { Ok(Self) }
    }
}

#[cfg(target_os = "windows")]
impl Drop for ComApartment {
    fn drop(&mut self) {
        // SAFETY: `enter` が成功した同じスレッドで、1 回だけ呼ぶ。
        unsafe { CoUninitialize() }
    }
}

/// MMCSS への登録。`Drop` で解除する。失敗(`None`)しても動作は続ける。
#[cfg(target_os = "windows")]
struct MmcssRegistration(*mut c_void);

#[cfg(target_os = "windows")]
impl MmcssRegistration {
    fn enter() -> Option<Self> {
        let mut task_index = 0u32;
        // SAFETY: タスク名は NUL 終端の UTF-16、`task_index` はスタック上の有効な書き込み先。
        let handle =
            unsafe { AvSetMmThreadCharacteristicsW(MMCSS_TASK_NAME.as_ptr(), &mut task_index) };
        if handle.is_null() {
            None
        } else {
            Some(Self(handle))
        }
    }
}

#[cfg(target_os = "windows")]
impl Drop for MmcssRegistration {
    fn drop(&mut self) {
        // SAFETY: `enter` が返したハンドルを登録したスレッドで 1 回だけ解除する。
        unsafe {
            AvRevertMmThreadCharacteristics(self.0);
        }
    }
}

/// 音声スレッドを起こす/待たせるためのイベントハンドル(自動リセット)。
///
/// `Backend` が作って所有し、`close` が join した後に閉じる。音声スレッドは待つだけで閉じない。
#[cfg(target_os = "windows")]
#[derive(Clone, Copy)]
struct EventHandle(*mut c_void);

// SAFETY: Win32 のイベントハンドルはプロセス内のどのスレッドからでも
// `SetEvent` / `WaitForSingleObject` できる。閉じるのは `close`(`Drop` からも呼ぶ)が
// 音声スレッドを join した後の 1 回だけ(`open` が初期化を待ちきれなかったときは閉じずに残す)。
#[cfg(target_os = "windows")]
unsafe impl Send for EventHandle {}

// ============================================================================
// デバイスの変化の監視(IMMNotificationClient)
// ============================================================================
//
// 既定の出力の切り替え・使用中のデバイスの無効化/取り外しを `IMMNotificationClient` で受け、
// 音声スレッドへ「開き直す」合図を出す。音声スレッドは合図を見るとループを抜けて
// `Event::StreamError { DeviceUnavailable }` を積み、`mw-ffi` の内部再オープンが新しい既定へ
// 開き直す(他のバックエンドが `DeviceUnavailable` で再オープンに乗るのと同じ経路)。
//
// 🔴 通知は OS が用意した**任意のスレッド**から来る。コールバックの中ではロックも COM 呼び出しも
// せず、フラグを立てて音声スレッドのイベントを `SetEvent` するだけにする(音声スレッドは
// 止めない・待たせない)。
//
// 🔴 寿命: オブジェクトは参照カウント付きでヒープに置く。作った時点の参照(1)は
// `DeviceWatcher` が持ち、`RegisterEndpointNotificationCallback` が OS 側の参照を足す。
// `DeviceWatcher::drop` は先に `Unregister...` する(戻った後は通知が来ない)→ 自分の参照を
// 手放す、の順。最後の参照が消えたときにだけ解放する。`DeviceWatcher` は音声スレッドの中で、
// ストリーム・`wake` ハンドルより先に落とす(コールバックが閉じたハンドルを触らない)。

/// 音声スレッドへの合図。立ったら開き直す。
#[cfg(target_os = "windows")]
type DeviceChangedFlag = Arc<AtomicBool>;

/// `IMMNotificationClient` の実体。先頭が vtable ポインタ(COM の ABI)。
#[cfg(target_os = "windows")]
#[repr(C)]
struct NotificationClient {
    vtbl: *const IMmNotificationClientVtbl,
    refs: AtomicU32,
    /// 開いているデバイスの ID(NUL なし)。作った後は変えない。
    current_id: Vec<u16>,
    changed: DeviceChangedFlag,
    wake: EventHandle,
}

#[cfg(target_os = "windows")]
static NOTIFICATION_VTBL: IMmNotificationClientVtbl = IMmNotificationClientVtbl {
    query_interface: nc_query_interface,
    add_ref: nc_add_ref,
    release: nc_release,
    on_device_state_changed: nc_on_device_state_changed,
    on_device_added: nc_on_device_added,
    on_device_removed: nc_on_device_removed,
    on_default_device_changed: nc_on_default_device_changed,
    on_property_value_changed: nc_on_property_value_changed,
};

#[cfg(target_os = "windows")]
impl NotificationClient {
    /// 参照カウント 1 のオブジェクトを作り、所有ポインタを返す。
    fn new_raw(current_id: Vec<u16>, changed: DeviceChangedFlag, wake: EventHandle) -> *mut Self {
        Box::into_raw(Box::new(Self {
            vtbl: &NOTIFICATION_VTBL,
            refs: AtomicU32::new(1),
            current_id,
            changed,
            wake,
        }))
    }

    /// 開き直しの合図を立てて音声スレッドを起こす。ロックも確保もしない。
    fn raise(&self) {
        self.changed.store(true, Ordering::Release);
        // SAFETY: `wake` は音声スレッドを join するまで閉じられず、`DeviceWatcher` は
        // その前に落ちる(登録解除済みなので、これ以降コールバックは来ない)。
        unsafe {
            SetEvent(self.wake.0);
        }
    }

    fn handle(&self, event: DeviceEvent<'_>) {
        if should_reopen(event, &self.current_id) {
            self.raise();
        }
    }
}

/// COM の境界で panic を外へ出さない(出すと未定義動作)。
#[cfg(target_os = "windows")]
fn guard_callback(f: impl FnOnce()) -> Hresult {
    let _ = panic::catch_unwind(AssertUnwindSafe(f));
    0
}

/// `E_POINTER`。
#[cfg(target_os = "windows")]
const E_POINTER: Hresult = 0x8000_4003_u32 as i32;

#[cfg(target_os = "windows")]
unsafe extern "system" fn nc_query_interface(
    this: ThisPtr,
    iid: *const Guid,
    out: *mut *mut c_void,
) -> Hresult {
    if out.is_null() {
        return E_POINTER;
    }
    // SAFETY: `out` は非 NULL(上で確認)で、呼び出し側が書き込み可能な領域。
    unsafe { *out = std::ptr::null_mut() };
    // SAFETY: `iid` は NULL か、呼び出し側が渡す有効な GUID。
    let Some(iid) = (unsafe { iid.as_ref() }) else {
        return E_NOINTERFACE;
    };
    if *iid == IID_IUNKNOWN || *iid == IID_IMM_NOTIFICATION_CLIENT {
        // SAFETY: `this` は `NotificationClient`(参照を持つ者がいる間は生きている)。
        unsafe {
            nc_add_ref(this);
            *out = this;
        }
        0
    } else {
        E_NOINTERFACE
    }
}

#[cfg(target_os = "windows")]
unsafe extern "system" fn nc_add_ref(this: ThisPtr) -> u32 {
    // SAFETY: `this` は `NotificationClient` で、呼び出し側が参照を持っている。
    let client = unsafe { &*this.cast::<NotificationClient>() };
    client.refs.fetch_add(1, Ordering::Relaxed) + 1
}

#[cfg(target_os = "windows")]
unsafe extern "system" fn nc_release(this: ThisPtr) -> u32 {
    // SAFETY: 同上。
    let client = unsafe { &*this.cast::<NotificationClient>() };
    let remaining = client.refs.fetch_sub(1, Ordering::Release) - 1;
    if remaining == 0 {
        std::sync::atomic::fence(Ordering::Acquire);
        // SAFETY: 最後の参照が消えた。`new_raw` の `Box::into_raw` と対。以後 `this` は使われない。
        drop(unsafe { Box::from_raw(this.cast::<NotificationClient>()) });
    }
    remaining
}

#[cfg(target_os = "windows")]
unsafe extern "system" fn nc_on_device_state_changed(
    this: ThisPtr,
    device_id: *const u16,
    state: u32,
) -> Hresult {
    guard_callback(|| {
        // SAFETY: OS が渡す NUL 終端の文字列で、コールバックの間は有効。`this` は生きている。
        let (client, id) = unsafe {
            (
                &*this.cast::<NotificationClient>(),
                wide_str_from_ptr(device_id),
            )
        };
        if let Some(device_id) = id {
            client.handle(DeviceEvent::StateChanged { device_id, state });
        }
    })
}

#[cfg(target_os = "windows")]
unsafe extern "system" fn nc_on_device_added(_this: ThisPtr, _device_id: *const u16) -> Hresult {
    0
}

#[cfg(target_os = "windows")]
unsafe extern "system" fn nc_on_device_removed(this: ThisPtr, device_id: *const u16) -> Hresult {
    guard_callback(|| {
        // SAFETY: `nc_on_device_state_changed` と同じ。
        let (client, id) = unsafe {
            (
                &*this.cast::<NotificationClient>(),
                wide_str_from_ptr(device_id),
            )
        };
        if let Some(device_id) = id {
            client.handle(DeviceEvent::Removed { device_id });
        }
    })
}

#[cfg(target_os = "windows")]
unsafe extern "system" fn nc_on_default_device_changed(
    this: ThisPtr,
    flow: i32,
    role: i32,
    device_id: *const u16,
) -> Hresult {
    guard_callback(|| {
        // SAFETY: `nc_on_device_state_changed` と同じ(NULL は「既定が無い」)。
        let (client, id) = unsafe {
            (
                &*this.cast::<NotificationClient>(),
                wide_str_from_ptr(device_id),
            )
        };
        client.handle(DeviceEvent::DefaultChanged {
            flow,
            role,
            device_id: id,
        });
    })
}

#[cfg(target_os = "windows")]
unsafe extern "system" fn nc_on_property_value_changed(
    _this: ThisPtr,
    _device_id: *const u16,
    _key: PropertyKey,
) -> Hresult {
    0
}

/// `IMMDevice::GetId` の結果(NUL なしの UTF-16)。
#[cfg(target_os = "windows")]
fn device_id_of(device: &Com<IMmDeviceVtbl>) -> Option<Vec<u16>> {
    let mut raw: *mut u16 = std::ptr::null_mut();
    // SAFETY: `device` は有効、出力先はスタック上のポインタ。
    let hr = unsafe { (device.vtbl().get_id)(device.this(), &mut raw) };
    if hr < 0 || raw.is_null() {
        return None;
    }
    // SAFETY: 成功した `GetId` は NUL 終端の文字列を返す。コピーしてから `CoTaskMemFree` で解放する。
    let id = unsafe { wide_str_from_ptr(raw) }.map(<[u16]>::to_vec);
    // SAFETY: `GetId` が確保した領域を 1 回だけ解放する。
    unsafe { CoTaskMemFree(raw.cast()) };
    id
}

/// 現在の既定のレンダーエンドポイント(`eConsole`)の ID。
#[cfg(target_os = "windows")]
fn default_render_id(enumerator: &Com<IMmDeviceEnumeratorVtbl>) -> Option<Vec<u16>> {
    let mut raw: *mut c_void = std::ptr::null_mut();
    // SAFETY: `enumerator` は有効、出力先はスタック上のポインタ。
    let hr = unsafe {
        (enumerator.vtbl().get_default_audio_endpoint)(
            enumerator.this(),
            E_RENDER,
            E_CONSOLE,
            &mut raw,
        )
    };
    if hr < 0 {
        return None;
    }
    // SAFETY: 成功した呼び出しは参照を 1 つ所有する `IMMDevice` を返す。
    let device = unsafe { Com::<IMmDeviceVtbl>::from_raw(raw) }?;
    device_id_of(&device)
}

/// `IMMNotificationClient` の登録。`Drop` で解除する。音声スレッド(MTA)で作って落とす。
#[cfg(target_os = "windows")]
struct DeviceWatcher {
    enumerator: Com<IMmDeviceEnumeratorVtbl>,
    /// 参照を 1 つ所有している(`Drop` で手放す)。
    client: *mut NotificationClient,
}

#[cfg(target_os = "windows")]
impl DeviceWatcher {
    /// `device_id` のデバイスを開いている間の変化を `changed` で知らせる。
    /// 開いた後・登録前に既定が変わっていても取りこぼさないよう、登録後に既定を確かめ直す。
    fn start(
        device_id: Vec<u16>,
        changed: DeviceChangedFlag,
        wake: EventHandle,
    ) -> Result<Self, Hresult> {
        let mut raw: *mut c_void = std::ptr::null_mut();
        // SAFETY: GUID は有効な静的値、出力先はスタック上のポインタ。
        let hr = unsafe {
            CoCreateInstance(
                &CLSID_MM_DEVICE_ENUMERATOR,
                std::ptr::null_mut(),
                CLSCTX_ALL,
                &IID_IMM_DEVICE_ENUMERATOR,
                &mut raw,
            )
        };
        if hr < 0 {
            return Err(hr);
        }
        // SAFETY: 成功した `CoCreateInstance` は参照を 1 つ所有するインターフェースを返す。
        let enumerator = unsafe { Com::<IMmDeviceEnumeratorVtbl>::from_raw(raw) }.ok_or(-1)?;

        let client = NotificationClient::new_raw(device_id, changed, wake);
        // SAFETY: `enumerator` は有効。`client` は参照カウント 1 の有効なオブジェクトで、
        // 登録は OS 側の参照を足す。
        let hr = unsafe {
            (enumerator.vtbl().register_endpoint_notification_callback)(
                enumerator.this(),
                client.cast(),
            )
        };
        if hr < 0 {
            // SAFETY: 登録に失敗したので、持っているのは自分の参照だけ。それを手放す。
            unsafe { nc_release(client.cast()) };
            return Err(hr);
        }
        let watcher = Self { enumerator, client };

        if let Some(now) = default_render_id(&watcher.enumerator) {
            // SAFETY: `client` は `watcher` が参照を持っているので生きている。
            unsafe { &*watcher.client }.handle(DeviceEvent::DefaultChanged {
                flow: E_RENDER,
                role: E_CONSOLE,
                device_id: Some(&now),
            });
        }
        Ok(watcher)
    }
}

#[cfg(target_os = "windows")]
impl Drop for DeviceWatcher {
    fn drop(&mut self) {
        // SAFETY: `enumerator` は有効。解除が戻った後はコールバックが来ない(実行中のものは
        // 終わるまで待つ)ので、そのあとで自分の参照を手放してよい。解除に失敗しても手放す。
        unsafe {
            let unregister = self
                .enumerator
                .vtbl()
                .unregister_endpoint_notification_callback;
            unregister(self.enumerator.this(), self.client.cast());
            nc_release(self.client.cast());
        }
    }
}

// ============================================================================
// ストリーム(音声スレッドが排他的に所有する)
// ============================================================================

/// 開いた結果(`open` のログとフィールドへ返す)。
#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug)]
struct StreamInfo {
    sample_rate: u32,
    buffer_frames: u32,
    /// 既定のデバイス周期をフレームへ直したもの(アンダーランの間隔検知の基準)。
    period_frames: u32,
    default_period_hns: i64,
    min_period_hns: i64,
    stream_latency_hns: i64,
    /// エンドポイントのミックス形式のレート(0 は取得失敗)。
    mix_sample_rate: u32,
    mix_channels: u16,
}

#[cfg(target_os = "windows")]
struct Stream {
    // フィールドの宣言順 = 解放順(レンダークライアントを先に、`IAudioClient` を後に)。
    render: Com<IAudioRenderClientVtbl>,
    /// 出力時刻の予測に使う。取れなければ `None`(padding + `GetStreamLatency` に落とす)。
    clock: Option<ClockReader>,
    client: Com<IAudioClientVtbl>,
    info: StreamInfo,
    /// 開いたデバイスの ID(NUL なし。取れなければ空で、その場合は変化を監視しない)。
    device_id: Vec<u16>,
}

/// `IAudioClock` と、その周波数(開いたときに 1 回読む)。
#[cfg(target_os = "windows")]
struct ClockReader {
    clock: Com<IAudioClockVtbl>,
    frequency: u64,
}

#[cfg(target_os = "windows")]
impl ClockReader {
    /// 装置位置を読み、次に書く先頭フレームの出力時刻(ホスト単調 ns)を返す。
    /// 読めない・位置が時刻に結び付いていない(QPC が 0)ときは `None`。
    fn playback_start_ns(&self, written_frames: u64, sample_rate: u32) -> Option<u64> {
        let mut position = 0u64;
        let mut qpc = 0u64;
        // SAFETY: `clock` は有効、出力先はスタック上の `u64`。
        let hr =
            unsafe { (self.clock.vtbl().get_position)(self.clock.this(), &mut position, &mut qpc) };
        if hr < 0 || qpc == 0 {
            return None;
        }
        clock_playback_start_ns(position, self.frequency, qpc, written_frames, sample_rate)
    }
}

#[cfg(target_os = "windows")]
impl Drop for Stream {
    fn drop(&mut self) {
        // SAFETY: `client` は有効。装置が無効になった後などで `Stop` が失敗しても、
        // 戻り値は捨てて解放を続ける。
        unsafe {
            (self.client.vtbl().stop)(self.client.this());
        }
    }
}

/// `$call` が負の `HRESULT` なら `classify_open_error($stage, hr)` を返す。
#[cfg(target_os = "windows")]
macro_rules! check_open {
    ($stage:expr, $call:expr) => {{
        let hr: Hresult = $call;
        if hr < 0 {
            return Err(classify_open_error($stage, hr));
        }
    }};
}

#[cfg(target_os = "windows")]
impl Stream {
    /// 既定のレンダーエンドポイントを共有モード・イベント駆動で初期化する(開始はしない)。
    fn open(event: EventHandle) -> Result<Self, BackendError> {
        let mut raw: *mut c_void = std::ptr::null_mut();
        // SAFETY: GUID は有効な静的値、出力先はスタック上のポインタ。
        let hr = unsafe {
            CoCreateInstance(
                &CLSID_MM_DEVICE_ENUMERATOR,
                std::ptr::null_mut(),
                CLSCTX_ALL,
                &IID_IMM_DEVICE_ENUMERATOR,
                &mut raw,
            )
        };
        check_open!("CoCreateInstance(MMDeviceEnumerator)", hr);
        // SAFETY: 成功した `CoCreateInstance` は参照を 1 つ所有する要求どおりのインターフェースを返す。
        let enumerator = unsafe { Com::<IMmDeviceEnumeratorVtbl>::from_raw(raw) }
            .ok_or(BackendError::NoOutputDevice)?;

        let mut raw: *mut c_void = std::ptr::null_mut();
        // SAFETY: `enumerator` は有効、出力先はスタック上のポインタ。
        let hr = unsafe {
            (enumerator.vtbl().get_default_audio_endpoint)(
                enumerator.this(),
                E_RENDER,
                E_CONSOLE,
                &mut raw,
            )
        };
        check_open!("GetDefaultAudioEndpoint", hr);
        // SAFETY: 成功した呼び出しは参照を 1 つ所有する `IMMDevice` を返す。
        let device =
            unsafe { Com::<IMmDeviceVtbl>::from_raw(raw) }.ok_or(BackendError::NoOutputDevice)?;
        let device_id = device_id_of(&device).unwrap_or_default();

        let mut raw: *mut c_void = std::ptr::null_mut();
        // SAFETY: `device` は有効。`IID_IAUDIO_CLIENT` を要求しているので結果は `IAudioClient`。
        let hr = unsafe {
            (device.vtbl().activate)(
                device.this(),
                &IID_IAUDIO_CLIENT,
                CLSCTX_ALL,
                std::ptr::null(),
                &mut raw,
            )
        };
        check_open!("IMMDevice::Activate(IAudioClient)", hr);
        // SAFETY: 成功した `Activate` は参照を 1 つ所有する `IAudioClient` を返す。
        let client = unsafe { Com::<IAudioClientVtbl>::from_raw(raw) }.ok_or_else(|| {
            BackendError::BuildStreamFailed("IMMDevice::Activate returned NULL".into())
        })?;

        let mut default_period_hns = 0i64;
        let mut min_period_hns = 0i64;
        // SAFETY: 出力先はスタック上の有効な `i64`。
        let hr = unsafe {
            (client.vtbl().get_device_period)(
                client.this(),
                &mut default_period_hns,
                &mut min_period_hns,
            )
        };
        check_open!("GetDevicePeriod", hr);

        // エンドポイントのミックス形式(ログ用。取れなくても続行する)。
        let (mix_sample_rate, mix_channels) = {
            let mut mix: *mut WaveFormatEx = std::ptr::null_mut();
            // SAFETY: 出力先はスタック上のポインタ。成功時の戻りは `CoTaskMemFree` で解放する。
            let hr = unsafe { (client.vtbl().get_mix_format)(client.this(), &mut mix) };
            if hr >= 0 && !mix.is_null() {
                // SAFETY: `mix` は `GetMixFormat` が返した有効な `WAVEFORMATEX`(1 バイト詰めなので
                // 値は `read_unaligned` で読む)。読み終えたら解放する。
                unsafe {
                    let format = std::ptr::read_unaligned(mix);
                    CoTaskMemFree(mix.cast());
                    (format.samples_per_sec, format.channels)
                }
            } else {
                (0, 0)
            }
        };

        let format = float_stereo_format(TARGET_SAMPLE_RATE_HZ);
        // SAFETY: `format` はこの呼び出しのあいだ有効。セッション GUID は NULL。
        let hr = unsafe {
            (client.vtbl().initialize)(
                client.this(),
                AUDCLNT_SHAREMODE_SHARED,
                INIT_STREAM_FLAGS,
                default_period_hns,
                0,
                &format,
                std::ptr::null(),
            )
        };
        check_open!("IAudioClient::Initialize", hr);

        // SAFETY: `event` は `Backend` が作った有効なイベントハンドル(close まで閉じられない)。
        let hr = unsafe { (client.vtbl().set_event_handle)(client.this(), event.0) };
        check_open!("SetEventHandle", hr);

        let mut buffer_frames = 0u32;
        // SAFETY: 出力先はスタック上の有効な `u32`。
        let hr = unsafe { (client.vtbl().get_buffer_size)(client.this(), &mut buffer_frames) };
        check_open!("GetBufferSize", hr);

        let mut stream_latency_hns = 0i64;
        // SAFETY: 出力先はスタック上の有効な `i64`。
        let hr =
            unsafe { (client.vtbl().get_stream_latency)(client.this(), &mut stream_latency_hns) };
        check_open!("GetStreamLatency", hr);

        let mut raw: *mut c_void = std::ptr::null_mut();
        // SAFETY: `IID_IAUDIO_RENDER_CLIENT` を要求しているので結果は `IAudioRenderClient`。
        let hr = unsafe {
            (client.vtbl().get_service)(client.this(), &IID_IAUDIO_RENDER_CLIENT, &mut raw)
        };
        check_open!("GetService(IAudioRenderClient)", hr);
        // SAFETY: 成功した `GetService` は参照を 1 つ所有する `IAudioRenderClient` を返す。
        let render = unsafe { Com::<IAudioRenderClientVtbl>::from_raw(raw) }.ok_or_else(|| {
            BackendError::BuildStreamFailed("GetService(IAudioRenderClient) returned NULL".into())
        })?;

        // 出力時刻の予測用の時計(取れなくても続行する)。
        let mut raw: *mut c_void = std::ptr::null_mut();
        // SAFETY: `IID_IAUDIO_CLOCK` を要求しているので結果は `IAudioClock`。
        let hr = unsafe { (client.vtbl().get_service)(client.this(), &IID_IAUDIO_CLOCK, &mut raw) };
        let clock = if hr >= 0 {
            // SAFETY: 成功した `GetService` は参照を 1 つ所有する `IAudioClock` を返す。
            unsafe { Com::<IAudioClockVtbl>::from_raw(raw) }.and_then(|clock| {
                let mut frequency = 0u64;
                // SAFETY: 出力先はスタック上の `u64`。
                let hr = unsafe { (clock.vtbl().get_frequency)(clock.this(), &mut frequency) };
                (hr >= 0 && frequency > 0).then_some(ClockReader { clock, frequency })
            })
        } else {
            None
        };

        let period_frames =
            hns_to_frames(default_period_hns, TARGET_SAMPLE_RATE_HZ).clamp(1, MAX_RENDER_FRAMES);
        Ok(Self {
            render,
            clock,
            client,
            info: StreamInfo {
                sample_rate: TARGET_SAMPLE_RATE_HZ,
                buffer_frames,
                period_frames,
                default_period_hns,
                min_period_hns,
                stream_latency_hns,
                mix_sample_rate,
                mix_channels,
            },
            device_id,
        })
    }

    fn start(&self) -> Result<(), BackendError> {
        // SAFETY: `client` は初期化済みで有効。
        let hr = unsafe { (self.client.vtbl().start)(self.client.this()) };
        if hr < 0 {
            return Err(BackendError::PlayStreamFailed(format!(
                "WASAPI IAudioClient::Start failed: {} ({})",
                describe_hresult(hr),
                hresult_hex(hr)
            )));
        }
        Ok(())
    }
}

// ============================================================================
// 音声スレッド
// ============================================================================

/// 音声スレッドが排他的に所有する状態。
#[cfg(target_os = "windows")]
struct WorkerContext {
    renderer: Renderer,
    events: Arc<EventQueue>,
    wake: EventHandle,
    stop: Arc<AtomicBool>,
    underrun_tracker: OutputUnderrunTracker,
    callback_frames: Arc<AtomicU32>,
    callback_count: Arc<AtomicU64>,
    output_latency_ns: Arc<AtomicU64>,
    /// 既定の出力・使用中のデバイスの変化の合図(`DeviceWatcher` が立てる)。
    device_changed: DeviceChangedFlag,
}

/// 音声スレッドの本体。初期化の結果を `init_tx` で `open` へ返し、成功ならループに入る。
///
/// 音声スレッドの規約(`mw-core/COMMON.md`): ループの中でロック・ヒープ確保をしない
/// (`mw_log!` も呼ばない)。例外は終了時の致命的エラー通知だけ(`push_side_channel` は
/// `Mutex` を使うが、そのあとスレッドは終わる)。
#[cfg(target_os = "windows")]
fn worker_main(mut context: WorkerContext, init_tx: SyncSender<Result<StreamInfo, BackendError>>) {
    let caught = panic::catch_unwind(AssertUnwindSafe(|| worker_run(&mut context, &init_tx)));
    let fatal = match caught {
        Ok(reason) => reason,
        // 契約が破られた場合の最後の防波堤(他バックエンドの `catch_unwind` と同じ方針)。
        Err(_) => Some(StreamErrorReason::Backend),
    };
    if let Some(reason) = fatal {
        context
            .events
            .push_side_channel(Event::StreamError { reason });
    }
}

#[cfg(target_os = "windows")]
fn worker_run(
    context: &mut WorkerContext,
    init_tx: &SyncSender<Result<StreamInfo, BackendError>>,
) -> Option<StreamErrorReason> {
    // 宣言順の逆に解放される: MMCSS → ストリーム → COM。
    let _apartment = match ComApartment::enter() {
        Ok(apartment) => apartment,
        Err(hr) => {
            let _ = init_tx.send(Err(BackendError::BuildStreamFailed(format!(
                "CoInitializeEx failed: {}",
                hresult_hex(hr)
            ))));
            return None;
        }
    };
    let stream = match Stream::open(context.wake) {
        Ok(stream) => stream,
        Err(error) => {
            let _ = init_tx.send(Err(error));
            return None;
        }
    };
    let _mmcss = MmcssRegistration::enter();
    if let Err(error) = stream.start() {
        let _ = init_tx.send(Err(error));
        return None;
    }
    // 変化の監視(失敗しても鳴らし続ける。その場合は無効化のエラーだけが頼り)。
    // ストリームより後に宣言する = ストリームより先に落ちる。
    let _watcher = if stream.device_id.is_empty() {
        None
    } else {
        match DeviceWatcher::start(
            stream.device_id.clone(),
            Arc::clone(&context.device_changed),
            context.wake,
        ) {
            Ok(watcher) => Some(watcher),
            Err(hr) => {
                crate::mw_log!(
                    "[mw-backend] (native/windows) device change notifications are unavailable: {}",
                    hresult_hex(hr)
                );
                None
            }
        }
    };
    if init_tx.send(Ok(stream.info)).is_err() {
        // `open` が待ちきれずに諦めた。
        return None;
    }
    let reason = worker_loop(context, &stream);
    if context.device_changed.load(Ordering::Acquire) {
        crate::mw_log!(
            "[mw-backend] (native/windows) the default output or the device in use changed; asking for a reopen"
        );
    }
    reason
}

/// ループ本体。止められたら `None`、復旧不能のエラーで抜けたら `Some(理由)`。
#[cfg(target_os = "windows")]
fn worker_loop(context: &mut WorkerContext, stream: &Stream) -> Option<StreamErrorReason> {
    let info = stream.info;
    let client = &stream.client;
    let render = &stream.render;
    // 開始からここまでに `ReleaseBuffer` したフレーム数(`IAudioClock` の位置との差が未再生分)。
    let mut written_frames = 0u64;
    let mut consecutive_timeouts = 0u32;

    loop {
        // SAFETY: `wake` は `Backend` が所有する有効なハンドル(join まで閉じられない)。
        let waited = unsafe { WaitForSingleObject(context.wake.0, WAIT_TIMEOUT_MS) };
        if context.stop.load(Ordering::Acquire) {
            return None;
        }
        if context.device_changed.load(Ordering::Acquire) {
            return Some(StreamErrorReason::DeviceUnavailable);
        }
        match waited {
            WAIT_OBJECT_0 => {}
            WAIT_TIMEOUT => {
                // イベントを通知しないまま止まった装置を拾う: 一定回数続いたら状態を問い合わせる。
                consecutive_timeouts += 1;
                if consecutive_timeouts >= TIMEOUTS_BEFORE_PROBE {
                    consecutive_timeouts = 0;
                    let mut padding = 0u32;
                    // SAFETY: `client` は有効、出力先はスタック上の `u32`。
                    let hr =
                        unsafe { (client.vtbl().get_current_padding)(client.this(), &mut padding) };
                    if hr < 0 {
                        return Some(classify_stream_error(hr));
                    }
                }
                continue;
            }
            _ => return Some(StreamErrorReason::Backend),
        }
        consecutive_timeouts = 0;

        let callback_ns = host_time_ns();
        let mut padding = 0u32;
        // SAFETY: `client` は有効、出力先はスタック上の `u32`。
        let hr = unsafe { (client.vtbl().get_current_padding)(client.this(), &mut padding) };
        if hr < 0 {
            return Some(classify_stream_error(hr));
        }
        let frames = writable_frames(info.buffer_frames, padding);
        if frames == 0 {
            continue;
        }

        let mut data: *mut u8 = std::ptr::null_mut();
        // SAFETY: `render` は有効。`frames` は `buffer_frames - padding` 以下なので `GetBuffer` の契約内。
        let hr = unsafe { (render.vtbl().get_buffer)(render.this(), frames, &mut data) };
        if hr < 0 {
            return Some(classify_stream_error(hr));
        }

        // 出力時刻: 装置位置(`IAudioClock`)から求める。padding + `GetStreamLatency` は
        // エンジン周期・ドライバのぶんを含まず実際より小さいので、時計が読めないときだけの代用。
        let start_ns = stream
            .clock
            .as_ref()
            .and_then(|clock| clock.playback_start_ns(written_frames, info.sample_rate))
            .unwrap_or_else(|| {
                playback_start_ns(
                    callback_ns,
                    output_latency_ns(padding, info.sample_rate, info.stream_latency_hns),
                )
            });
        let latency_ns = start_ns.saturating_sub(callback_ns);
        context
            .output_latency_ns
            .store(latency_ns, Ordering::Relaxed);
        context.callback_frames.store(frames, Ordering::Relaxed);
        context.callback_count.fetch_add(1, Ordering::Relaxed);
        context
            .underrun_tracker
            .observe(callback_ns, info.period_frames, info.sample_rate);

        let mut flags = 0u32;
        let mut panicked = false;
        let sample_count = frames as usize * CHANNELS;
        if data.is_null() || !(data as usize).is_multiple_of(std::mem::align_of::<f32>()) {
            // 契約外のポインタ。書かずに無音として返す。
            flags = AUDCLNT_BUFFERFLAGS_SILENT;
        } else {
            // SAFETY: `GetBuffer` は `frames` フレームぶん(f32 × `CHANNELS` ch)の書き込み可能な
            // 領域を返す。`ReleaseBuffer` するまでこのスレッドだけが触る。アラインは上で確認済み。
            let buffer =
                unsafe { std::slice::from_raw_parts_mut(data.cast::<f32>(), sample_count) };
            // 音声スレッド上で呼ぶ mw-core 側の経路は `Renderer::render` のみ。
            let result = panic::catch_unwind(AssertUnwindSafe(|| {
                context.renderer.render(buffer, start_ns);
            }));
            if result.is_err() {
                panicked = true;
                buffer.fill(0.0);
                flags = AUDCLNT_BUFFERFLAGS_SILENT;
            }
        }
        // SAFETY: 直前の `GetBuffer` と同じ `frames`。
        let hr = unsafe { (render.vtbl().release_buffer)(render.this(), frames, flags) };
        written_frames = written_frames.saturating_add(frames as u64);
        if panicked {
            return Some(StreamErrorReason::Backend);
        }
        if hr < 0 {
            return Some(classify_stream_error(hr));
        }
    }
}

// ============================================================================
// Backend 実装(Windows のみ)
// ============================================================================

/// Windows の既定出力(WASAPI 共有モード)へ直接出力する `Backend` 実装。
#[cfg(target_os = "windows")]
pub struct WindowsBackend {
    thread: Option<JoinHandle<()>>,
    wake: Option<EventHandle>,
    stop: Arc<AtomicBool>,
    callback_frames: Arc<AtomicU32>,
    callback_count: Arc<AtomicU64>,
    output_latency_ns: Arc<AtomicU64>,
    logged_output_latency: AtomicBool,
    sample_rate: u32,
    output_underrun_count: Arc<AtomicU64>,
    last_output_underrun_host_time_ns: Arc<AtomicU64>,
    consecutive_output_underrun_count: Arc<AtomicU32>,
    logged_output_underrun_count: AtomicU64,
}

#[cfg(target_os = "windows")]
impl Drop for WindowsBackend {
    /// `close` せずに落とされても音声スレッドとハンドルを残さない。
    fn drop(&mut self) {
        let _ = self.close();
    }
}

#[cfg(target_os = "windows")]
impl Default for WindowsBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_os = "windows")]
impl WindowsBackend {
    pub fn new() -> Self {
        Self {
            thread: None,
            wake: None,
            stop: Arc::new(AtomicBool::new(false)),
            callback_frames: Arc::new(AtomicU32::new(0)),
            callback_count: Arc::new(AtomicU64::new(0)),
            output_latency_ns: Arc::new(AtomicU64::new(0)),
            logged_output_latency: AtomicBool::new(false),
            sample_rate: 0,
            output_underrun_count: Arc::new(AtomicU64::new(0)),
            last_output_underrun_host_time_ns: Arc::new(AtomicU64::new(0)),
            consecutive_output_underrun_count: Arc::new(AtomicU32::new(0)),
            logged_output_underrun_count: AtomicU64::new(0),
        }
    }

    /// 開いてからの音声ループの回数(実機確認用。0 は一度も回っていない)。
    pub fn callback_count(&self) -> u64 {
        self.callback_count.load(Ordering::Relaxed)
    }
}

#[cfg(target_os = "windows")]
impl Backend for WindowsBackend {
    fn open(&mut self, renderer: Renderer, events: Arc<EventQueue>) -> Result<(), BackendError> {
        if self.thread.is_some() {
            return Err(BackendError::AlreadyOpen);
        }
        let mut renderer = renderer;
        // ランプのミリ秒→サンプル数換算(初期構築仕様 §4.1)が正しいレートを使えるよう、
        // スレッドを起こす前に確定させる(`AUTOCONVERTPCM` で 48kHz が通る前提)。
        renderer.set_sample_rate(TARGET_SAMPLE_RATE_HZ);

        // SAFETY: 属性・名前は NULL、自動リセット・非シグナルで作る。
        let wake = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
        if wake.is_null() {
            return Err(BackendError::BuildStreamFailed(
                "CreateEventW failed".to_string(),
            ));
        }
        let wake = EventHandle(wake);

        self.stop.store(false, Ordering::Release);
        let context = WorkerContext {
            renderer,
            events,
            wake,
            stop: Arc::clone(&self.stop),
            underrun_tracker: OutputUnderrunTracker::new(
                Arc::clone(&self.output_underrun_count),
                Arc::clone(&self.last_output_underrun_host_time_ns),
                Arc::clone(&self.consecutive_output_underrun_count),
            ),
            callback_frames: Arc::clone(&self.callback_frames),
            callback_count: Arc::clone(&self.callback_count),
            output_latency_ns: Arc::clone(&self.output_latency_ns),
            device_changed: Arc::new(AtomicBool::new(false)),
        };
        let (init_tx, init_rx) = sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("mw-wasapi-render".into())
            .spawn(move || worker_main(context, init_tx));
        let thread = match thread {
            Ok(thread) => thread,
            Err(error) => {
                // SAFETY: スレッドが起動しなかったので、ハンドルはこの関数だけが持っている。
                unsafe {
                    CloseHandle(wake.0);
                }
                return Err(BackendError::PlayStreamFailed(format!(
                    "failed to spawn the WASAPI render thread: {error}"
                )));
            }
        };

        match init_rx.recv_timeout(Duration::from_millis(INIT_TIMEOUT_MS)) {
            Ok(Ok(info)) => {
                crate::mw_log!(
                    "[mw-backend] (native/windows) output stream started: shared mode, \
                     event-driven, sample_rate={} Hz (requested {} Hz, endpoint mix format {} Hz \
                     {} ch), channels={CHANNELS}, buffer={} frames, device period \
                     default={:.3} ms / min={:.3} ms ({} frames), stream latency={:.3} ms",
                    info.sample_rate,
                    TARGET_SAMPLE_RATE_HZ,
                    info.mix_sample_rate,
                    info.mix_channels,
                    info.buffer_frames,
                    info.default_period_hns as f64 / 10_000.0,
                    info.min_period_hns as f64 / 10_000.0,
                    info.period_frames,
                    info.stream_latency_hns as f64 / 10_000.0,
                );
                self.thread = Some(thread);
                self.wake = Some(wake);
                self.sample_rate = info.sample_rate;
                Ok(())
            }
            Ok(Err(error)) => {
                let _ = thread.join();
                // SAFETY: スレッドは join 済みで、ハンドルを使う者はもういない。
                unsafe {
                    CloseHandle(wake.0);
                }
                Err(error)
            }
            Err(_) => {
                // 初期化が終わらない(または音声スレッドが応答なしに死んだ)。止めを依頼して諦める。
                // スレッドがまだ生きている可能性があるため、ハンドルは閉じずに残す(リーク)。
                self.stop.store(true, Ordering::Release);
                // SAFETY: ハンドルは有効(閉じていない)。
                unsafe {
                    SetEvent(wake.0);
                }
                Err(BackendError::BuildStreamFailed(
                    "WASAPI stream initialization did not complete".to_string(),
                ))
            }
        }
    }

    fn close(&mut self) -> Result<(), BackendError> {
        let (Some(thread), Some(wake)) = (self.thread.take(), self.wake.take()) else {
            return Err(BackendError::NotOpen);
        };
        self.stop.store(true, Ordering::Release);
        // SAFETY: ハンドルは有効(まだ閉じていない)。待っている音声スレッドを起こす。
        unsafe {
            SetEvent(wake.0);
        }
        // パニックで終わっていても `Err` が返るだけなので無視してよい。
        let _ = thread.join();
        // SAFETY: 音声スレッドは join 済みで、ハンドルを使う者はもういない。
        unsafe {
            CloseHandle(wake.0);
        }

        self.sample_rate = 0;
        self.callback_frames.store(0, Ordering::Relaxed);
        self.callback_count.store(0, Ordering::Relaxed);
        self.output_latency_ns.store(0, Ordering::Relaxed);
        self.logged_output_latency.store(false, Ordering::Relaxed);
        self.output_underrun_count.store(0, Ordering::Relaxed);
        self.last_output_underrun_host_time_ns
            .store(0, Ordering::Relaxed);
        self.consecutive_output_underrun_count
            .store(0, Ordering::Relaxed);
        self.logged_output_underrun_count
            .store(0, Ordering::Relaxed);
        Ok(())
    }

    fn is_open(&self) -> bool {
        self.thread.is_some()
    }

    fn last_callback_frames(&self) -> u32 {
        self.callback_frames.load(Ordering::Relaxed)
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn output_latency_ns(&self) -> u64 {
        self.output_latency_ns.load(Ordering::Relaxed)
    }

    fn log_output_latency_once(&self) {
        if self.logged_output_latency.load(Ordering::Relaxed) {
            return;
        }
        let latency_ns = self.output_latency_ns();
        if latency_ns == 0 {
            return;
        }
        self.logged_output_latency.store(true, Ordering::Relaxed);
        let latency_ms = latency_ns as f64 / 1_000_000.0;
        crate::mw_log!(
            "[mw-backend] (native/windows) output latency (padding + GetStreamLatency): \
             {latency_ns} ns = {latency_ms:.3} ms"
        );
    }

    fn log_new_output_underruns(&self) {
        let current = self.output_underrun_count.load(Ordering::Relaxed);
        let last_logged = self.logged_output_underrun_count.load(Ordering::Relaxed);
        if current <= last_logged {
            return;
        }
        self.logged_output_underrun_count
            .store(current, Ordering::Relaxed);
        let new_count = current - last_logged;
        let consecutive = self
            .consecutive_output_underrun_count
            .load(Ordering::Relaxed);
        crate::mw_log!(
            "[mw-backend] (native/windows) output underrun suspected: +{new_count} since \
             last check (cumulative={current}, consecutive={consecutive})"
        );
    }

    fn output_underrun_count(&self) -> u64 {
        self.output_underrun_count.load(Ordering::Relaxed)
    }

    fn last_output_underrun_host_time_ns(&self) -> u64 {
        self.last_output_underrun_host_time_ns
            .load(Ordering::Relaxed)
    }

    fn consecutive_output_underrun_count(&self) -> u32 {
        self.consecutive_output_underrun_count
            .load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::BackendError;
    use std::mem::{offset_of, size_of};

    const PTR: usize = size_of::<usize>();

    #[test]
    fn float_stereo_format_describes_interleaved_f32_at_the_requested_rate() {
        let f = float_stereo_format(48_000);
        let format = f.format;
        assert_eq!({ format.format_tag }, WAVE_FORMAT_EXTENSIBLE);
        assert_eq!({ format.channels }, 2);
        assert_eq!({ format.samples_per_sec }, 48_000);
        assert_eq!({ format.bits_per_sample }, 32);
        assert_eq!({ format.block_align }, 8);
        assert_eq!({ format.avg_bytes_per_sec }, 384_000);
        assert_eq!({ format.cb_size }, 22);
        assert_eq!({ f.valid_bits_per_sample }, 32);
        assert_eq!({ f.channel_mask }, 0x3);
        assert_eq!({ f.sub_format }, KSDATAFORMAT_SUBTYPE_IEEE_FLOAT);
    }

    #[test]
    fn wave_format_structs_have_the_packed_win32_layout() {
        assert_eq!(size_of::<WaveFormatEx>(), 18);
        assert_eq!(size_of::<WaveFormatExtensible>(), 40);
        assert_eq!(size_of::<Guid>(), 16);
    }

    #[test]
    fn vtable_slots_match_the_com_interface_definitions() {
        // IUnknown が 3 スロット。以降は各インターフェースの宣言順(Windows SDK のヘッダ)。
        assert_eq!(size_of::<IUnknownVtbl>(), 3 * PTR);
        assert_eq!(
            offset_of!(IMmDeviceEnumeratorVtbl, get_default_audio_endpoint),
            4 * PTR
        );
        assert_eq!(size_of::<IMmDeviceEnumeratorVtbl>(), 8 * PTR);
        assert_eq!(offset_of!(IMmDeviceVtbl, activate), 3 * PTR);
        assert_eq!(size_of::<IMmDeviceVtbl>(), 7 * PTR);
        assert_eq!(offset_of!(IAudioClientVtbl, initialize), 3 * PTR);
        assert_eq!(offset_of!(IAudioClientVtbl, get_buffer_size), 4 * PTR);
        assert_eq!(offset_of!(IAudioClientVtbl, get_stream_latency), 5 * PTR);
        assert_eq!(offset_of!(IAudioClientVtbl, get_current_padding), 6 * PTR);
        assert_eq!(offset_of!(IAudioClientVtbl, get_mix_format), 8 * PTR);
        assert_eq!(offset_of!(IAudioClientVtbl, get_device_period), 9 * PTR);
        assert_eq!(offset_of!(IAudioClientVtbl, start), 10 * PTR);
        assert_eq!(offset_of!(IAudioClientVtbl, stop), 11 * PTR);
        assert_eq!(offset_of!(IAudioClientVtbl, set_event_handle), 13 * PTR);
        assert_eq!(offset_of!(IAudioClientVtbl, get_service), 14 * PTR);
        assert_eq!(size_of::<IAudioClientVtbl>(), 15 * PTR);
        assert_eq!(offset_of!(IAudioRenderClientVtbl, get_buffer), 3 * PTR);
        assert_eq!(offset_of!(IAudioRenderClientVtbl, release_buffer), 4 * PTR);
        assert_eq!(size_of::<IAudioRenderClientVtbl>(), 5 * PTR);
        assert_eq!(offset_of!(IAudioClockVtbl, get_frequency), 3 * PTR);
        assert_eq!(offset_of!(IAudioClockVtbl, get_position), 4 * PTR);
        assert_eq!(size_of::<IAudioClockVtbl>(), 6 * PTR);
    }

    #[test]
    fn hns_to_ns_scales_by_one_hundred_and_saturates() {
        assert_eq!(hns_to_ns(100_000), 10_000_000); // 10ms
        assert_eq!(hns_to_ns(0), 0);
        assert_eq!(hns_to_ns(-5), 0);
        assert_eq!(hns_to_ns(i64::MAX), u64::MAX);
    }

    #[test]
    fn hns_to_frames_truncates_at_the_given_rate() {
        assert_eq!(hns_to_frames(100_000, 48_000), 480); // 10ms
        assert_eq!(hns_to_frames(30_000, 48_000), 144); // 3ms
        assert_eq!(hns_to_frames(-1, 48_000), 0);
        assert_eq!(hns_to_frames(i64::MAX, u32::MAX), u32::MAX);
    }

    #[test]
    fn output_latency_adds_the_queued_frames_and_the_stream_latency() {
        // 480 フレーム(48kHz で 10ms)+ 10ms = 20ms。
        assert_eq!(output_latency_ns(480, 48_000, 100_000), 20_000_000);
        assert_eq!(output_latency_ns(0, 48_000, 0), 0);
    }

    #[test]
    fn output_latency_ignores_negative_stream_latency_and_unknown_rate() {
        assert_eq!(output_latency_ns(480, 48_000, -1), 10_000_000);
        assert_eq!(output_latency_ns(480, 0, 100_000), 10_000_000);
    }

    #[test]
    fn playback_start_adds_the_latency_and_saturates() {
        assert_eq!(playback_start_ns(1_000, 500), 1_500);
        assert_eq!(playback_start_ns(u64::MAX - 1, 10), u64::MAX);
    }

    #[test]
    fn clock_playback_start_is_the_position_time_plus_the_unplayed_frames() {
        // 位置 1 秒ぶん(48000 frames @ 48kHz)を、QPC 5 秒の時点で読んだ。
        // 書き込み済みは 48000 + 1536 frames = 未再生 1536 frames = 32ms。
        let start = clock_playback_start_ns(48_000, 48_000, 50_000_000, 49_536, 48_000);
        assert_eq!(start, Some(5_000_000_000 + 32_000_000));
    }

    #[test]
    fn clock_playback_start_converts_the_position_with_the_clock_frequency() {
        // 周波数が 96000 の時計で位置 96000 = 1 秒。
        let start = clock_playback_start_ns(96_000, 96_000, 0, 48_480, 48_000);
        assert_eq!(start, Some(10_000_000)); // 480 frames = 10ms
    }

    #[test]
    fn clock_playback_start_treats_a_position_ahead_of_the_written_frames_as_nothing_queued() {
        let start = clock_playback_start_ns(60_000, 48_000, 10, 48_000, 48_000);
        assert_eq!(start, Some(1_000));
    }

    #[test]
    fn clock_playback_start_is_none_without_a_usable_clock() {
        assert_eq!(clock_playback_start_ns(1, 0, 0, 1, 48_000), None);
        assert_eq!(clock_playback_start_ns(1, 48_000, 0, 1, 0), None);
    }

    #[test]
    fn writable_frames_is_the_free_space_capped_and_never_negative() {
        assert_eq!(writable_frames(960, 480), 480);
        assert_eq!(writable_frames(960, 0), 960);
        assert_eq!(writable_frames(960, 960), 0);
        assert_eq!(writable_frames(960, 2_000), 0);
        assert_eq!(writable_frames(u32::MAX, 0), MAX_RENDER_FRAMES);
    }

    #[test]
    fn hresult_hex_prints_the_unsigned_value() {
        assert_eq!(hresult_hex(AUDCLNT_E_DEVICE_INVALIDATED), "0x88890004");
        assert_eq!(hresult_hex(E_NOTFOUND), "0x80070490");
    }

    #[test]
    fn open_errors_for_a_missing_endpoint_map_to_no_output_device() {
        assert!(matches!(
            classify_open_error("GetDefaultAudioEndpoint", E_NOTFOUND),
            BackendError::NoOutputDevice
        ));
        assert!(matches!(
            classify_open_error("Activate", AUDCLNT_E_ENDPOINT_CREATE_FAILED),
            BackendError::NoOutputDevice
        ));
    }

    #[test]
    fn other_open_errors_keep_the_stage_and_the_hresult() {
        let error = classify_open_error("IAudioClient::Initialize", AUDCLNT_E_UNSUPPORTED_FORMAT);
        match error {
            BackendError::BuildStreamFailed(message) => {
                assert!(message.contains("IAudioClient::Initialize"), "{message}");
                assert!(
                    message.contains("AUDCLNT_E_UNSUPPORTED_FORMAT"),
                    "{message}"
                );
                assert!(message.contains("0x88890008"), "{message}");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn stream_errors_are_classified_like_the_other_backends() {
        for hr in [
            AUDCLNT_E_DEVICE_INVALIDATED,
            AUDCLNT_E_SERVICE_NOT_RUNNING,
            AUDCLNT_E_DEVICE_IN_USE,
            AUDCLNT_E_RESOURCES_INVALIDATED,
        ] {
            assert_eq!(
                classify_stream_error(hr),
                StreamErrorReason::DeviceUnavailable,
                "{}",
                hresult_hex(hr)
            );
        }
        assert_eq!(
            classify_stream_error(E_ACCESSDENIED),
            StreamErrorReason::PermissionDenied
        );
        assert_eq!(classify_stream_error(-1), StreamErrorReason::Backend);
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    const CURRENT: &str = "{0.0.0.00000000}.{aaaa}";
    const OTHER: &str = "{0.0.0.00000000}.{bbbb}";

    #[test]
    fn the_default_render_device_changing_to_another_one_reopens_for_console_and_multimedia() {
        let other = wide(OTHER);
        for role in [E_CONSOLE, E_MULTIMEDIA] {
            assert!(should_reopen(
                DeviceEvent::DefaultChanged {
                    flow: E_RENDER,
                    role,
                    device_id: Some(&other),
                },
                &wide(CURRENT),
            ));
        }
    }

    #[test]
    fn default_changes_that_do_not_move_the_output_are_ignored() {
        let current = wide(CURRENT);
        let other = wide(OTHER);
        // 通信用の役割・入力・同じデバイスへの変更・既定が無くなった通知。
        for (flow, role, id) in [
            (E_RENDER, 2, Some(&other)),
            (1, E_CONSOLE, Some(&other)),
            (E_RENDER, E_CONSOLE, Some(&current)),
            (E_RENDER, E_CONSOLE, None),
        ] {
            assert!(
                !should_reopen(
                    DeviceEvent::DefaultChanged {
                        flow,
                        role,
                        device_id: id.map(Vec::as_slice),
                    },
                    &current,
                ),
                "flow={flow} role={role} id={id:?}"
            );
        }
    }

    #[test]
    fn the_device_in_use_becoming_unusable_or_removed_reopens() {
        let current = wide(CURRENT);
        // DEVICE_STATE_DISABLED / NOTPRESENT / UNPLUGGED
        for state in [0x2, 0x4, 0x8] {
            assert!(should_reopen(
                DeviceEvent::StateChanged {
                    device_id: &current,
                    state,
                },
                &current,
            ));
        }
        assert!(should_reopen(
            DeviceEvent::Removed {
                device_id: &current
            },
            &current,
        ));
    }

    #[test]
    fn state_changes_of_other_devices_or_to_active_are_ignored() {
        let current = wide(CURRENT);
        let other = wide(OTHER);
        assert!(!should_reopen(
            DeviceEvent::StateChanged {
                device_id: &other,
                state: 0x2,
            },
            &current,
        ));
        assert!(!should_reopen(
            DeviceEvent::StateChanged {
                device_id: &current,
                state: DEVICE_STATE_ACTIVE,
            },
            &current,
        ));
        assert!(!should_reopen(
            DeviceEvent::Removed { device_id: &other },
            &current,
        ));
    }

    #[test]
    fn wide_str_from_ptr_stops_at_the_terminator() {
        // SAFETY (この関数内の全呼び出し): 指す先は NUL 終端、または上限まで読める配列。
        assert_eq!(unsafe { wide_str_from_ptr(std::ptr::null()) }, None);
        let text = [0x61u16, 0x62, 0, 0x63];
        assert_eq!(
            unsafe { wide_str_from_ptr(text.as_ptr()) },
            Some(&[0x61u16, 0x62][..])
        );
        let empty = [0u16];
        assert_eq!(unsafe { wide_str_from_ptr(empty.as_ptr()) }, Some(&[][..]));
        let unterminated = vec![1u16; MAX_DEVICE_ID_UNITS];
        assert_eq!(unsafe { wide_str_from_ptr(unterminated.as_ptr()) }, None);
    }

    #[test]
    fn notification_client_vtable_follows_the_com_interface_order() {
        assert_eq!(
            offset_of!(IMmNotificationClientVtbl, on_device_state_changed),
            3 * PTR
        );
        assert_eq!(
            offset_of!(IMmNotificationClientVtbl, on_device_added),
            4 * PTR
        );
        assert_eq!(
            offset_of!(IMmNotificationClientVtbl, on_device_removed),
            5 * PTR
        );
        assert_eq!(
            offset_of!(IMmNotificationClientVtbl, on_default_device_changed),
            6 * PTR
        );
        assert_eq!(
            offset_of!(IMmNotificationClientVtbl, on_property_value_changed),
            7 * PTR
        );
        assert_eq!(size_of::<IMmNotificationClientVtbl>(), 8 * PTR);
        assert_eq!(
            offset_of!(
                IMmDeviceEnumeratorVtbl,
                register_endpoint_notification_callback
            ),
            6 * PTR
        );
        assert_eq!(
            offset_of!(
                IMmDeviceEnumeratorVtbl,
                unregister_endpoint_notification_callback
            ),
            7 * PTR
        );
        assert_eq!(offset_of!(IMmDeviceVtbl, get_id), 5 * PTR);
        assert_eq!(size_of::<PropertyKey>(), 20);
    }

    #[cfg(target_os = "windows")]
    mod device {
        use super::super::*;
        use mw_core::{BusId, Command, SoundData};

        use std::sync::atomic::AtomicBool;

        fn wide(s: &str) -> Vec<u16> {
            s.encode_utf16().collect()
        }

        /// 実体を作り、vtable 経由で呼ぶ(OS が呼ぶのと同じ経路)。
        struct Fixture {
            client: *mut NotificationClient,
            flag: Arc<AtomicBool>,
            wake: EventHandle,
        }

        impl Fixture {
            fn new(current: &str) -> Self {
                // SAFETY: 属性・名前は NULL、自動リセット・非シグナル。
                let wake =
                    EventHandle(unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) });
                assert!(!wake.0.is_null());
                let flag = Arc::new(AtomicBool::new(false));
                let client = NotificationClient::new_raw(wide(current), Arc::clone(&flag), wake);
                Self { client, flag, wake }
            }
            fn vtbl(&self) -> &IMmNotificationClientVtbl {
                // SAFETY: `client` は参照を持っている間は有効。
                unsafe { &*(*self.client).vtbl }
            }
            fn this(&self) -> ThisPtr {
                self.client.cast()
            }
            fn signaled(&self) -> bool {
                // SAFETY: `wake` は有効。0ms で状態だけ見る(自動リセットなので消費する)。
                unsafe { WaitForSingleObject(self.wake.0, 0) == WAIT_OBJECT_0 }
            }
        }

        impl Drop for Fixture {
            fn drop(&mut self) {
                // SAFETY: テストが最後に持つ参照を手放し、そのあとハンドルを閉じる。
                unsafe {
                    (self.vtbl().release)(self.this());
                    CloseHandle(self.wake.0);
                }
            }
        }

        fn nul_terminated(s: &str) -> Vec<u16> {
            let mut v = wide(s);
            v.push(0);
            v
        }

        #[test]
        fn relevant_callbacks_raise_the_flag_and_wake_the_audio_thread() {
            let nul_a = nul_terminated("dev-a");
            let nul_b = nul_terminated("dev-b");

            let f = Fixture::new("dev-a");
            // SAFETY: 引数は NUL 終端の有効な文字列。
            unsafe {
                (f.vtbl().on_device_added)(f.this(), nul_b.as_ptr());
                (f.vtbl().on_device_state_changed)(f.this(), nul_b.as_ptr(), 0x2);
                (f.vtbl().on_device_state_changed)(f.this(), nul_a.as_ptr(), DEVICE_STATE_ACTIVE);
                (f.vtbl().on_default_device_changed)(f.this(), E_RENDER, 2, nul_b.as_ptr());
            }
            assert!(!f.flag.load(Ordering::Acquire));
            assert!(!f.signaled());

            // SAFETY: 同上。
            unsafe {
                (f.vtbl().on_default_device_changed)(f.this(), E_RENDER, E_CONSOLE, nul_b.as_ptr());
            }
            assert!(f.flag.load(Ordering::Acquire));
            assert!(f.signaled());

            f.flag.store(false, Ordering::Release);
            // SAFETY: 同上。
            unsafe {
                (f.vtbl().on_device_state_changed)(f.this(), nul_a.as_ptr(), 0x4);
            }
            assert!(f.flag.load(Ordering::Acquire));
            f.flag.store(false, Ordering::Release);
            // SAFETY: 同上。
            unsafe {
                (f.vtbl().on_device_removed)(f.this(), nul_a.as_ptr());
            }
            assert!(f.flag.load(Ordering::Acquire));
        }

        #[test]
        fn callbacks_with_a_null_id_do_not_crash_or_raise() {
            let f = Fixture::new("dev-a");
            // SAFETY: NULL は OS が「既定が無い」ときに渡しうる値。
            unsafe {
                (f.vtbl().on_default_device_changed)(
                    f.this(),
                    E_RENDER,
                    E_CONSOLE,
                    std::ptr::null(),
                );
                (f.vtbl().on_device_removed)(f.this(), std::ptr::null());
                (f.vtbl().on_device_state_changed)(f.this(), std::ptr::null(), 0x2);
            }
            assert!(!f.flag.load(Ordering::Acquire));
        }

        #[test]
        fn query_interface_answers_only_iunknown_and_the_notification_client() {
            let f = Fixture::new("dev-a");
            let mut out: *mut c_void = std::ptr::null_mut();
            // SAFETY: 出力先はスタック上のポインタ、IID は有効な静的値。
            unsafe {
                assert_eq!(
                    (f.vtbl().query_interface)(f.this(), &IID_IMM_NOTIFICATION_CLIENT, &mut out),
                    0
                );
                assert_eq!(out, f.this());
                // QueryInterface が足した参照を手放す。
                assert_eq!((f.vtbl().release)(f.this()), 1);
                assert_eq!(
                    (f.vtbl().query_interface)(f.this(), &IID_IUNKNOWN, &mut out),
                    0
                );
                assert_eq!((f.vtbl().release)(f.this()), 1);
                assert_eq!(
                    (f.vtbl().query_interface)(f.this(), &IID_IAUDIO_CLIENT, &mut out),
                    E_NOINTERFACE
                );
                assert!(out.is_null());
            }
        }

        #[test]
        fn the_object_is_freed_when_the_last_reference_is_released() {
            let flag = Arc::new(AtomicBool::new(false));
            let wake = EventHandle(std::ptr::null_mut());
            let client = NotificationClient::new_raw(wide("dev-a"), Arc::clone(&flag), wake);
            assert_eq!(Arc::strong_count(&flag), 2);
            // SAFETY: `client` は参照 1 の有効なオブジェクト。
            unsafe {
                assert_eq!(nc_add_ref(client.cast()), 2);
                assert_eq!(nc_release(client.cast()), 1);
                assert_eq!(Arc::strong_count(&flag), 2);
                assert_eq!(nc_release(client.cast()), 0);
            }
            assert_eq!(Arc::strong_count(&flag), 1);
        }

        // ------------------------------------------------------------------
        // 実機: 既定の出力を API で切り替えて、鳴らしている間に追従することを確かめる
        // ------------------------------------------------------------------

        #[repr(C)]
        struct IMmDeviceCollectionVtbl {
            unknown: IUnknownVtbl,
            get_count: unsafe extern "system" fn(ThisPtr, *mut u32) -> Hresult,
            item: unsafe extern "system" fn(ThisPtr, u32, *mut ThisPtr) -> Hresult,
        }

        type EnumAudioEndpointsFn =
            unsafe extern "system" fn(ThisPtr, i32, u32, *mut ThisPtr) -> Hresult;

        /// 非公開の `IPolicyConfig`(`SetDefaultEndpoint` は 13 番目)。テストの中だけで使う。
        #[repr(C)]
        struct IPolicyConfigVtbl {
            unknown: IUnknownVtbl,
            skipped: [*const c_void; 10],
            set_default_endpoint: unsafe extern "system" fn(ThisPtr, *const u16, i32) -> Hresult,
        }

        const CLSID_POLICY_CONFIG_CLIENT: Guid = Guid {
            data1: 0x870A_F99C,
            data2: 0x171D,
            data3: 0x4F9E,
            data4: [0xAF, 0x0D, 0xE6, 0x3D, 0xF4, 0x0C, 0x2B, 0xC9],
        };
        const IID_IPOLICY_CONFIG: Guid = Guid {
            data1: 0xF867_9F50,
            data2: 0x850A,
            data3: 0x41CF,
            data4: [0x9C, 0x72, 0x43, 0x0F, 0x29, 0x02, 0x90, 0xC8],
        };

        fn create_enumerator() -> Com<IMmDeviceEnumeratorVtbl> {
            let mut raw: *mut c_void = std::ptr::null_mut();
            // SAFETY: GUID は有効な静的値、出力先はスタック上のポインタ。
            let hr = unsafe {
                CoCreateInstance(
                    &CLSID_MM_DEVICE_ENUMERATOR,
                    std::ptr::null_mut(),
                    CLSCTX_ALL,
                    &IID_IMM_DEVICE_ENUMERATOR,
                    &mut raw,
                )
            };
            assert!(hr >= 0, "CoCreateInstance failed: {}", hresult_hex(hr));
            // SAFETY: 成功した呼び出しは参照を 1 つ所有する。
            unsafe { Com::from_raw(raw) }.expect("enumerator")
        }

        /// 有効なレンダーエンドポイントの ID を全部返す。
        fn active_render_ids() -> Vec<Vec<u16>> {
            let enumerator = create_enumerator();
            let mut raw: ThisPtr = std::ptr::null_mut();
            // SAFETY: スロット 3 は `EnumAudioEndpoints(EDataFlow, DWORD, IMMDeviceCollection**)`。
            let hr = unsafe {
                let f = std::mem::transmute::<*const c_void, EnumAudioEndpointsFn>(
                    enumerator.vtbl().enum_audio_endpoints,
                );
                f(enumerator.this(), E_RENDER, DEVICE_STATE_ACTIVE, &mut raw)
            };
            assert!(hr >= 0, "EnumAudioEndpoints failed: {}", hresult_hex(hr));
            // SAFETY: 成功した呼び出しは参照を 1 つ所有する。
            let collection =
                unsafe { Com::<IMmDeviceCollectionVtbl>::from_raw(raw) }.expect("collection");
            let mut count = 0u32;
            // SAFETY: 出力先はスタック上の `u32`。
            unsafe { (collection.vtbl().get_count)(collection.this(), &mut count) };
            let mut ids = Vec::new();
            for index in 0..count {
                let mut raw: ThisPtr = std::ptr::null_mut();
                // SAFETY: `index < count`、出力先はスタック上のポインタ。
                let hr = unsafe { (collection.vtbl().item)(collection.this(), index, &mut raw) };
                if hr < 0 {
                    continue;
                }
                // SAFETY: 成功した `Item` は参照を 1 つ所有する `IMMDevice` を返す。
                if let Some(device) = unsafe { Com::<IMmDeviceVtbl>::from_raw(raw) } {
                    ids.extend(device_id_of(&device));
                }
            }
            ids
        }

        fn set_default_render(id: &[u16]) {
            let mut raw: *mut c_void = std::ptr::null_mut();
            // SAFETY: GUID は有効な静的値、出力先はスタック上のポインタ。
            let hr = unsafe {
                CoCreateInstance(
                    &CLSID_POLICY_CONFIG_CLIENT,
                    std::ptr::null_mut(),
                    CLSCTX_ALL,
                    &IID_IPOLICY_CONFIG,
                    &mut raw,
                )
            };
            assert!(hr >= 0, "IPolicyConfig is unavailable: {}", hresult_hex(hr));
            // SAFETY: 成功した呼び出しは参照を 1 つ所有する。
            let policy = unsafe { Com::<IPolicyConfigVtbl>::from_raw(raw) }.expect("policy config");
            let mut nul = id.to_vec();
            nul.push(0);
            for role in [E_CONSOLE, E_MULTIMEDIA, 2] {
                // SAFETY: `nul` は NUL 終端。
                let hr = unsafe {
                    (policy.vtbl().set_default_endpoint)(policy.this(), nul.as_ptr(), role)
                };
                assert!(hr >= 0, "SetDefaultEndpoint failed: {}", hresult_hex(hr));
            }
        }

        /// 落ちたとき・panic したときも、既定の出力を元に戻す。
        struct RestoreDefault(Vec<u16>);

        impl Drop for RestoreDefault {
            fn drop(&mut self) {
                set_default_render(&self.0);
            }
        }

        fn open_backend() -> (WindowsBackend, Arc<EventQueue>) {
            let (renderer, _sender, _reclaim, _music_producer, _music_clock, events, _bgm) =
                mw_core::Renderer::build(mw_core::Config::default(), 48_000);
            let mut backend = WindowsBackend::new();
            backend
                .open(renderer, Arc::clone(&events))
                .expect("open should succeed on a machine with a default output device");
            (backend, events)
        }

        /// `StreamError { DeviceUnavailable }` が積まれるのを最大 `timeout` 待つ。
        fn wait_for_device_unavailable(events: &EventQueue, timeout: Duration) -> bool {
            let started = std::time::Instant::now();
            while started.elapsed() < timeout {
                let mut found = false;
                events.drain(64, |event| {
                    if matches!(
                        event,
                        Event::StreamError {
                            reason: StreamErrorReason::DeviceUnavailable
                        }
                    ) {
                        found = true;
                    }
                });
                if found {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            false
        }

        /// 既定の出力を別のデバイスへ切り替えると、鳴らしている間に `DeviceUnavailable` が上がり、
        /// 開き直した先でも鳴ること。最後に既定の出力を必ず元に戻す。
        /// 出力が 1 つしかない PC では何もせず終わる。
        /// `cargo test -p mw-backend follows_the_default_output -- --ignored --nocapture`
        #[test]
        #[ignore = "既定の出力を実際に切り替えるため、手元の Windows でのみ実行する(最後に元へ戻す)"]
        fn follows_the_default_output_when_it_is_switched_while_playing() {
            let _apartment = ComApartment::enter().expect("COM");
            let original = default_render_id(&create_enumerator()).expect("a default output");
            let Some(other) = active_render_ids().into_iter().find(|id| *id != original) else {
                println!("only one active output device; nothing to switch to");
                return;
            };
            let _restore = RestoreDefault(original.clone());

            let (mut backend, events) = open_backend();
            std::thread::sleep(Duration::from_millis(300));
            assert!(backend.callback_count() > 0, "the render loop never ran");
            assert!(
                !wait_for_device_unavailable(&events, Duration::from_millis(500)),
                "an error was raised without any change"
            );

            // 元 → 別のデバイス。
            let switched = std::time::Instant::now();
            set_default_render(&other);
            assert!(
                wait_for_device_unavailable(&events, Duration::from_secs(5)),
                "the switch to another default output was not noticed"
            );
            println!("switch noticed after {:?}", switched.elapsed());
            backend.close().expect("close");
            let (mut backend, events) = open_backend();
            std::thread::sleep(Duration::from_millis(300));
            assert!(
                backend.callback_count() > 0,
                "the reopened stream did not play"
            );

            // 別のデバイス → 元(開き直した先も追従する)。
            set_default_render(&original);
            assert!(
                wait_for_device_unavailable(&events, Duration::from_secs(5)),
                "the switch back to the original output was not noticed"
            );
            backend.close().expect("close");
        }

        /// 実デバイス(既定の出力)を開き、440Hz の正弦波を数秒鳴らして、
        /// コールバック回数・xrun 数・出力遅延を出力する。
        /// `cargo test -p mw-backend plays_a_tone -- --ignored --nocapture` で手動実行する。
        #[test]
        #[ignore = "実デバイス(既定の出力)を開いて数秒鳴らすため、手元の Windows でのみ実行する"]
        fn plays_a_tone_on_the_real_default_output_and_reports_the_numbers() {
            let (renderer, sender, _reclaim, _music_producer, _music_clock, events, _bgm) =
                mw_core::Renderer::build(mw_core::Config::default(), 48_000);
            let mut backend = WindowsBackend::new();
            backend
                .open(renderer, events)
                .expect("open should succeed on a machine with a default output device");
            assert!(backend.is_open());
            assert_eq!(backend.sample_rate(), TARGET_SAMPLE_RATE_HZ);

            let frames = 48_000usize;
            let mut interleaved = Vec::with_capacity(frames * CHANNELS);
            for n in 0..frames {
                let phase = 2.0 * std::f32::consts::PI * 440.0 * n as f32 / 48_000.0;
                let s = 0.1 * phase.sin();
                interleaved.push(s);
                interleaved.push(s);
            }
            let sound = Arc::new(SoundData {
                sample_rate: 48_000,
                frames,
                interleaved,
            });
            assert!(sender.send(Command::PlaySe {
                voice_serial: 1,
                sound_id: 1,
                sound,
                bus: BusId::Se,
                volume: 1.0,
            }));
            assert!(sender.send(Command::SetVoiceLoop {
                voice_serial: 1,
                region: Some((0, frames as u64)),
            }));

            let started = std::time::Instant::now();
            let mut latency_samples = Vec::new();
            while started.elapsed() < Duration::from_secs(4) {
                std::thread::sleep(Duration::from_millis(100));
                latency_samples.push(backend.output_latency_ns());
            }
            let elapsed = started.elapsed().as_secs_f64();

            let count = backend.callback_count();
            let frames_per_callback = backend.last_callback_frames();
            let underruns = backend.output_underrun_count();
            let min = latency_samples.iter().copied().min().unwrap_or(0);
            let max = latency_samples.iter().copied().max().unwrap_or(0);
            println!(
                "WASAPI real-device run: {elapsed:.2} s, callbacks={count} ({:.1}/s), \
                 last_callback_frames={frames_per_callback}, xruns(suspected)={underruns}, \
                 output_latency_ns min={min} max={max} ({:.3}..{:.3} ms)",
                count as f64 / elapsed,
                min as f64 / 1e6,
                max as f64 / 1e6,
            );

            assert!(count > 0, "the render loop never ran");
            assert!(frames_per_callback > 0);
            assert!(max > 0, "no output latency was reported");

            backend.close().expect("close should succeed");
            assert!(!backend.is_open());
            assert_eq!(backend.callback_count(), 0);
        }

        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentProcess() -> *mut std::ffi::c_void;
            fn GetProcessHandleCount(process: *mut std::ffi::c_void, count: *mut u32) -> i32;
        }

        fn process_handle_count() -> u32 {
            let mut count = 0u32;
            // SAFETY: 疑似ハンドルと、スタック上の有効な書き込み先。
            unsafe {
                GetProcessHandleCount(GetCurrentProcess(), &mut count);
            }
            count
        }

        /// `close` せずに drop しても、音声スレッドが止まりハンドルが残らないこと。
        /// `cargo test -p mw-backend dropping_an_open_backend -- --ignored --nocapture` で実行する。
        #[test]
        #[ignore = "実デバイス(既定の出力)を開くため、手元の Windows でのみ実行する"]
        fn dropping_an_open_backend_stops_the_render_thread_and_releases_handles() {
            let open = || {
                let (renderer, _sender, _reclaim, _music_producer, _music_clock, events, _bgm) =
                    mw_core::Renderer::build(mw_core::Config::default(), 48_000);
                let mut backend = WindowsBackend::new();
                backend
                    .open(renderer, events)
                    .expect("open should succeed on a machine with a default output device");
                backend
            };
            // ハンドル数の基準(1 回目の確保・COM の初期化ぶんを先に済ませる)。
            drop(open());
            let handles_before = process_handle_count();

            for _ in 0..20 {
                let backend = open();
                std::thread::sleep(Duration::from_millis(30));
                let count = Arc::clone(&backend.callback_count);
                assert!(
                    count.load(Ordering::Relaxed) > 0,
                    "the render loop never ran"
                );
                drop(backend);
                // drop の後は音声スレッドが回らない。
                let after_drop = count.load(Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(60));
                assert_eq!(
                    count.load(Ordering::Relaxed),
                    after_drop,
                    "the render thread kept running after drop"
                );
            }

            let handles_after = process_handle_count();
            println!("handles before={handles_before} after={handles_after}");
            assert!(
                handles_after <= handles_before + 5,
                "handles leaked: {handles_before} -> {handles_after}"
            );
        }
    }
}
