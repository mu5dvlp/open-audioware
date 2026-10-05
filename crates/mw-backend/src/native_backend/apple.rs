//! macOS / iOS / tvOS 用の自前 AudioUnit バックエンド(AUDIOWARE-DEPS-PLAN.md
//! ステップ4-1〔macOS〕・4-2〔iOS/tvOS〕)。
//!
//! **新しい外部クレートは追加していない。** AudioToolbox / CoreAudio フレームワークの
//! C API を自前の `extern "C"` 宣言(下の「AudioToolbox の FFI 宣言」節)で直接呼ぶ——
//! `objc2-audio-toolbox` 等は使わない(計画書 §4 の決定どおり。それらは `objc2`/
//! `objc2-foundation` を引き込むため、依存排除の到達点〔表記ゼロ〕に反する)。
//! iOS/tvOS のルート変化監視だけは、`ios_interruption.rs` が既に使っている
//! `objc2-foundation`/`block2`(NSNotificationCenter)と `objc2-avf-audio`
//! (AVAudioSession)をそのまま再利用する——いずれも既存の iOS/tvOS 向け依存で、
//! 新規追加はない。
//!
//! 手順は計画書の記述そのまま: `AudioComponentFindNext` → `AudioComponentInstanceNew` →
//! `AudioUnitSetProperty`(StreamFormat / SetRenderCallback)→ `AudioUnitInitialize` →
//! `AudioOutputUnitStart`。使うコンポーネントは macOS では `kAudioUnitSubType_DefaultOutput`
//! (既定の出力デバイスへ自動的に追従する)、iOS/tvOS では `kAudioUnitSubType_RemoteIO`
//! (`AUDIO_UNIT_SUBTYPE`)——差分はサブタイプと、`ios_session::configure`(セッション設定。
//! [`Backend::open`] の冒頭で呼ぶ)・補正項の更新(`IosOutputLatencyWatcher`)だけに
//! 絞れた(4-1 の見込みどおり)。
//!
//! # cpal 版との既知の差分(4-1/4-2 の時点で未対応)
//!
//! デバイスの切断・既定出力デバイスの変更(macOS)、および電話・Siri 等の割り込みからの
//! ストリーム復帰(iOS/tvOS、`ios_interruption.rs` が cpal 版に対して持つ復帰ロジック)は
//! どちらも実装していない。そのため [`Backend::open`] が受け取る `events` はこの実装では
//! 使わず、`StreamError`/`AudioInterruptionEnded` 等のイベントは発行されない。
//! 4-1 は「macOS Editor 専用の開発機バックエンド」、4-2 は「`backend-native` を明示的に
//! 選んだときだけ有効になる、既定〔`backend-cpal`〕に影響しない実装」という前提の範囲で
//! 許容する判断とした。iOS/tvOS で実装したのは**補正項(出力遅延)をルート変化のたびに
//! 最新化すること**だけ(`IosOutputLatencyWatcher`)——電話着信・バックグラウンド遷移
//! からのストリーム再始動は未実装で、本番導入(cpal 削除、ステップ4-4)前には
//! `ios_interruption.rs` 相当の復帰ロジックの移植が別途要る。出力コールバック自体の
//! 間隔異常(「アンダーラン(の疑い)」)は cpal 版と同じ
//! [`crate::underrun::OutputUnderrunTracker`] で検知する(macOS/iOS/tvOS 共通)。
//!
//! # タイムスタンプの扱い——相関点と補正項を分けて持つ(HANDOFF 0f の統合)
//!
//! `AURenderCallback` が渡す `AudioTimeStamp.mHostTime` は `mach_absolute_time()` と
//! 同じ時計源(`host_time.rs` のモジュール doc の調査結果どおり)なので、
//! [`crate::host_time::mach_ticks_to_ns`](同じ関数を cpal 版の時計整合性でも使っている)
//! で ns 化する——これが**相関点**(このコールバックが呼ばれたホスト時刻そのもの。
//! 毎コールバック読み直すだけで、どこにも保存しない)。そこへ**補正項**
//! (`CallbackContext::device_extra_latency_ns`。出力レイテンシの推定値)を足して
//! `Renderer::render` に渡す `buffer_start_host_time_ns`(予測される DAC 出力時刻)を得る。
//! 式は [`compute_timestamps`] のドキュメント参照——この関数は macOS/iOS/tvOS で共通
//! (相関点・補正項・バッファ長〔このコールバックの実測フレーム数〕を分けて受け取り、
//! 内部で合算するだけの純関数)。
//!
//! 補正項の**出し方**だけが OS で違う:
//!
//! - **macOS**: `open()` の中で `query_device_extra_latency_frames` を1度だけ呼び、
//!   以後は固定(デバイス切断の監視が無いのと同じ理由で、4-1 の時点では更新しない)。
//! - **iOS/tvOS**: `open()` の時点で `query_ios_output_latency_ns`
//!   (`AVAudioSession.outputLatency()`)を読んで初期化し、以後は
//!   `IosOutputLatencyWatcher` が `AVAudioSessionRouteChangeNotification` を監視して
//!   ルート(スピーカー/有線/Bluetooth)が変わるたびに読み直す——これが HANDOFF 0f の
//!   「補正項をルート変化で更新する」。
//!
//! **cpal 0.18.1(固定中)が iOS で返している値の意味、および 0.18.2 でそれが壊れた理由**
//! (ソースを確認済み。`~/.cargo/registry/.../cpal-0.18.{1,2}/src/host/coreaudio/ios/mod.rs`):
//! 0.18.1 の `OutputStreamTimestamp::playback` は `callback + (AVAudioSession.
//! IOBufferDuration() を起動時に1度だけ frame 数へ換算した値)`——**`outputLatency()`
//! は一切読んでいない**。0.18.2 はそこへ `AVAudioSession.outputLatency()` を追加で
//! 足すようになり、それを相関点として使っている音楽クロックが実機で約65ms 遅れた
//! (`9fdf287` のコミットメッセージ、Cargo.toml のコメント参照)。
//!
//! **この実装は 0.18.1 の意味を保たず、意図的に変える。** `mw-core::mixer::Mixer::render`
//! の doc と client 側 `NativeMusicClockCore.cs` の doc(「相関点は"今スピーカーから
//! 聞こえている"曲位置を指す——出力レイテンシが織り込まれている」)が最初から要求している
//! のは **DAC 出力時刻の予測**であって、0.18.1 の iOS 実装はそれを部分的にしか満たして
//! いなかった(`IOBufferDuration` だけで `outputLatency` を欠く)。0.18.2 が壊したのは
//! 「正しさ」ではなく「依存の patch 更新という不透明な経路で、校正済みの値が無断で
//! 変わったこと」——そのため、ここでは**自分で定義した意味を持つ新バックエンドとして**
//! `outputLatency()` を正しく織り込む。安全策は2つ: (1) 既定は `backend-cpal` のまま
//! (この変更は `backend-native` を明示選択しない限り本番に影響しない)。(2) 実際に
//! iOS を `backend-native` へ切り替える段では、`AudioOffsetSeconds` 等の実機校正を
//! **出力レイテンシが変わった前提で**取り直す必要がある——これは client 側
//! `NativeMusicClockCore.cs` 自身のクラス doc が最初から明記している帰結であり、
//! この変更が新たに生む落とし穴ではない。二重補正になり得る箇所(client 側に
//! `outputLatency` 由来の手動オフセットが別途存在しないか)は確認済み
//! (`AudioOffsetSeconds` はプレイヤー入力遅延の校正専用で、出力レイテンシ由来の値は
//! 一切含まない)。

use std::ffi::c_void;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use mw_core::{CHANNELS, EventQueue, MusicClockPublisher, Renderer};

use crate::backend::{Backend, BackendError};
use crate::host_time::mach_ticks_to_ns;
use crate::underrun::OutputUnderrunTracker;

// ============================================================================
// AudioToolbox の FFI 宣言(自前。新しいクレートは追加しない)
// ============================================================================

/// `AudioComponent`(`struct ComponentRecord *` の不透明ポインタ)。
type AudioComponent = *mut c_void;
/// `AudioComponentInstance` / `AudioUnit`(どちらも `struct
/// OpaqueAudioComponentInstance *` の不透明ポインタ。CoreAudio ヘッダでは
/// `typedef AudioComponentInstance AudioUnit;`)。
type AudioUnit = *mut c_void;

/// `AudioComponentDescription`(`AudioComponent.h`)。
#[repr(C)]
#[allow(dead_code)] // レイアウトを合わせるためだけに持つフィールドがある(常に 0 を書くだけ)
struct AudioComponentDescription {
    component_type: u32,
    component_sub_type: u32,
    component_manufacturer: u32,
    component_flags: u32,
    component_flags_mask: u32,
}

/// `AudioStreamBasicDescription`(`CoreAudioBaseTypes.h`)。f32 インターリーブ・
/// ステレオの固定フォーマットを表すためだけに使う(`stereo_f32_asbd` 参照)。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
struct AudioStreamBasicDescription {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

/// `SMPTETime`(`CoreAudioBaseTypes.h`)。`AudioTimeStamp` のレイアウトを合わせるためだけに
/// 持つ——内容は一切読まない(レンダーコールバックは `mHostTime` だけを使う)。
#[repr(C)]
#[allow(dead_code)]
struct SmpteTime {
    subframes: i16,
    subframe_divisor: i16,
    counter: u32,
    time_type: u32,
    flags: u32,
    hours: i16,
    minutes: i16,
    seconds: i16,
    frames: i16,
}

/// `AudioTimeStamp`(`CoreAudioBaseTypes.h`)。読むのは `host_time` のみ
/// (他のフィールドはレイアウトを合わせるためだけに持つ)。
#[repr(C)]
#[allow(dead_code)]
struct AudioTimeStamp {
    sample_time: f64,
    host_time: u64,
    rate_scalar: f64,
    word_clock_time: u64,
    smpte_time: SmpteTime,
    flags: u32,
    reserved: u32,
}

/// `AudioBuffer`(`CoreAudioBaseTypes.h`)。読むのは `data`/`data_byte_size` のみ
/// (`number_channels` はインターリーブ形式では使わない——`AudioBufferList` 1個に
/// チャンネル分すべてが入っている)。
#[repr(C)]
#[allow(dead_code)]
struct AudioBuffer {
    number_channels: u32,
    data_byte_size: u32,
    data: *mut c_void,
}

/// `AudioBufferList`(`CoreAudioBaseTypes.h`)。本来は可変長配列だが、インターリーブ形式
/// (`kAudioFormatFlagIsNonInterleaved` を立てない)では常に `number_buffers == 1` になる
/// (`render_proc` が念のため確認する)。
#[repr(C)]
struct AudioBufferList {
    number_buffers: u32,
    buffers: [AudioBuffer; 1],
}

/// `AudioObjectPropertyAddress`(`AudioHardwareBase.h`、CoreAudio フレームワーク)。
/// [`AudioObjectGetPropertyData`] でデバイス側のプロパティ(出力遅延の見積もり、
/// `query_device_extra_latency_frames` 参照)を読むためだけに使う。
///
/// **macOS のみ。** iOS/tvOS には `AudioObject`(複数デバイスを管理する HAL)の概念が無く、
/// 出力レイテンシは `AVAudioSession.outputLatency()` で読む(`query_ios_output_latency_ns`)。
#[cfg(target_os = "macos")]
#[repr(C)]
struct AudioObjectPropertyAddress {
    selector: u32,
    scope: u32,
    element: u32,
}

/// `AURenderCallback`(`AUComponent.h`)。戻り値は `OSStatus`。
type AURenderCallback = unsafe extern "C" fn(
    *mut c_void,
    *mut u32,
    *const AudioTimeStamp,
    u32,
    u32,
    *mut AudioBufferList,
) -> i32;

/// `AURenderCallbackStruct`(`AudioUnitProperties.h`)。
#[repr(C)]
struct AURenderCallbackStruct {
    input_proc: AURenderCallback,
    input_proc_ref_con: *mut c_void,
}

const AUDIO_UNIT_TYPE_OUTPUT: u32 = 0x6175_6f75; // 'auou'

/// 使うコンポーネントのサブタイプ。macOS は既定の出力デバイスへ自動追従する
/// `kAudioUnitSubType_DefaultOutput`、iOS/tvOS はハードウェア I/O への唯一の経路である
/// `kAudioUnitSubType_RemoteIO`(モジュール doc 参照。4-1→4-2 の差分はこの値だけ)。
#[cfg(target_os = "macos")]
const AUDIO_UNIT_SUBTYPE: u32 = 0x6465_6620; // 'def '
#[cfg(any(target_os = "ios", target_os = "tvos"))]
const AUDIO_UNIT_SUBTYPE: u32 = 0x7269_6f63; // 'rioc'

const AUDIO_UNIT_MANUFACTURER_APPLE: u32 = 0x6170_706c; // 'appl'

/// `kAudioUnitScope_Global`。**macOS のみ**(`query_current_device_id` の
/// `kAudioOutputUnitProperty_CurrentDevice` 読み出しにしか使わない)。
#[cfg(target_os = "macos")]
const AUDIO_UNIT_SCOPE_GLOBAL: u32 = 0;
const AUDIO_UNIT_SCOPE_INPUT: u32 = 1;
const AUDIO_UNIT_SCOPE_OUTPUT: u32 = 2;

const AUDIO_UNIT_PROPERTY_STREAM_FORMAT: u32 = 8;
const AUDIO_UNIT_PROPERTY_SET_RENDER_CALLBACK: u32 = 23;
/// `kAudioOutputUnitProperty_CurrentDevice`(`AudioUnitProperties.h`)。この AUHAL が
/// 実際に使っている `AudioDeviceID`(スコープ Global、値型 `AudioObjectID`)を読む——
/// `query_device_extra_latency_frames` がデバイス側のプロパティを読むために使う。
/// **macOS のみ**(iOS/tvOS の RemoteIO には「現在のデバイス」という概念が無い)。
#[cfg(target_os = "macos")]
const AUDIO_OUTPUT_UNIT_PROPERTY_CURRENT_DEVICE: u32 = 2000;

const AUDIO_FORMAT_LINEAR_PCM: u32 = 0x6c70_636d; // 'lpcm'
const AUDIO_FORMAT_FLAG_IS_FLOAT: u32 = 1 << 0;
const AUDIO_FORMAT_FLAG_IS_PACKED: u32 = 1 << 3;

/// `<MacTypes.h>` の `noErr`。
const NO_ERR: i32 = 0;

// `AudioObjectGetPropertyData` で読むデバイス側のプロパティ(`AudioHardwareBase.h`、
// CoreAudio フレームワーク)。cpal 0.18.1 の `get_device_extra_latency_frames`
// (`cpal::host::coreaudio::macos::device`)が読んでいるのと同じ2つ——
// `query_device_extra_latency_frames` のドキュメント参照。**macOS のみ。**
#[cfg(target_os = "macos")]
const AUDIO_DEVICE_PROPERTY_LATENCY: u32 = 0x6c74_6e63; // 'ltnc'
#[cfg(target_os = "macos")]
const AUDIO_DEVICE_PROPERTY_SAFETY_OFFSET: u32 = 0x7361_6674; // 'saft'
#[cfg(target_os = "macos")]
const AUDIO_OBJECT_PROPERTY_SCOPE_OUTPUT: u32 = 0x6f75_7470; // 'outp'
#[cfg(target_os = "macos")]
const AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN: u32 = 0;

#[allow(non_snake_case)] // シンボル名は AudioToolbox の実際の C API 名そのまま
#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioComponentFindNext(
        in_component: AudioComponent,
        in_desc: *const AudioComponentDescription,
    ) -> AudioComponent;

    fn AudioComponentInstanceNew(in_component: AudioComponent, out_instance: *mut AudioUnit)
    -> i32;

    fn AudioComponentInstanceDispose(in_instance: AudioUnit) -> i32;

    fn AudioUnitInitialize(in_unit: AudioUnit) -> i32;
    fn AudioUnitUninitialize(in_unit: AudioUnit) -> i32;

    fn AudioUnitGetProperty(
        in_unit: AudioUnit,
        in_id: u32,
        in_scope: u32,
        in_element: u32,
        out_data: *mut c_void,
        io_data_size: *mut u32,
    ) -> i32;

    fn AudioUnitSetProperty(
        in_unit: AudioUnit,
        in_id: u32,
        in_scope: u32,
        in_element: u32,
        in_data: *const c_void,
        in_data_size: u32,
    ) -> i32;

    fn AudioOutputUnitStart(ci: AudioUnit) -> i32;
    fn AudioOutputUnitStop(ci: AudioUnit) -> i32;
}

// デバイス(`AudioObject`)側のプロパティを読むためだけの別フレームワーク
// (`AudioObjectGetPropertyData` は CoreAudio.framework が export する——AudioToolbox
// ではない。`nm` で確認済み)。**macOS のみ**(iOS/tvOS には `AudioObject` の HAL が無い)。
#[cfg(target_os = "macos")]
#[allow(non_snake_case)]
#[link(name = "CoreAudio", kind = "framework")]
unsafe extern "C" {
    fn AudioObjectGetPropertyData(
        in_object_id: u32,
        in_address: *const AudioObjectPropertyAddress,
        in_qualifier_data_size: u32,
        in_qualifier_data: *const c_void,
        io_data_size: *mut u32,
        out_data: *mut c_void,
    ) -> i32;
}

// ============================================================================
// 純関数部分(ハードウェア無しで単体テストできる)
// ============================================================================

/// デバイスが見つからない等でハードウェアのレートを読めなかったときのフォールバック。
const FALLBACK_SAMPLE_RATE_HZ: f64 = 48_000.0;

/// f32 インターリーブ・ステレオの `AudioStreamBasicDescription` を組み立てる。
///
/// ネイティブエンディアン(Apple Silicon / Intel のいずれも little-endian)を前提に
/// `kAudioFormatFlagIsBigEndian` は立てない。`kAudioFormatFlagIsNonInterleaved` も
/// 立てないため、レンダーコールバックに渡る `AudioBufferList` は常に
/// `number_buffers == 1`(全チャンネルが1本のバッファにインターリーブされる)。
fn stereo_f32_asbd(sample_rate: f64) -> AudioStreamBasicDescription {
    let bytes_per_frame = (CHANNELS * std::mem::size_of::<f32>()) as u32;
    AudioStreamBasicDescription {
        sample_rate,
        format_id: AUDIO_FORMAT_LINEAR_PCM,
        format_flags: AUDIO_FORMAT_FLAG_IS_FLOAT | AUDIO_FORMAT_FLAG_IS_PACKED,
        // リニア PCM は1フレーム = 1パケット。
        bytes_per_packet: bytes_per_frame,
        frames_per_packet: 1,
        bytes_per_frame,
        channels_per_frame: CHANNELS as u32,
        bits_per_channel: 32,
        reserved: 0,
    }
}

/// レンダーコールバックが受け取った `AudioBuffer` へ安全に書き込める要素数(f32 の個数)を
/// 返す。null ポインタ・サイズ不足・0 フレーム・非アラインポインタはいずれも `None`
/// (呼び出し側は書き込みをスキップし、音声スレッドを絶対にパニックさせない。§4.8 の思想)。
///
/// `data` はポインタそのものを受け取る(値は読まない——アドレス値の null/アラインメント
/// チェックのみに使う。安全性は呼び出し側〔`render_proc`〕が呼ぶまで保たれる)。
///
/// 0 フレームは `data` の値を一切見ずに早期 `None` にする——書き込むものが無い以上、
/// 後続の `slice::from_raw_parts_mut`(ゼロ長でも非 null・アラインメント済みを要求する)
/// へその条件を満たしているか確認する必要すら無い。非 0 フレームでは、CoreAudio が通常
/// 16byte 境界に揃えて渡す前提はあるものの、契約違反(ダングリング・非アラインポインタ)を
/// 機械的にも弾いておく。
fn validated_sample_count(frames: u32, data: *mut c_void, data_byte_size: u32) -> Option<usize> {
    if frames == 0 {
        return None;
    }
    if data.is_null() {
        return None;
    }
    if !(data as usize).is_multiple_of(std::mem::align_of::<f32>()) {
        return None;
    }
    let samples = (frames as usize).checked_mul(CHANNELS)?;
    let needed_bytes = samples.checked_mul(std::mem::size_of::<f32>())?;
    if (data_byte_size as usize) < needed_bytes {
        return None;
    }
    Some(samples)
}

/// `callback_host_time_ns`(このレンダーコールバックの `AudioTimeStamp.mHostTime` を
/// ns 化した値)から、`Renderer::render` に渡す `buffer_start_host_time_ns`
/// (予測される出力時刻)と診断用の `output_latency_ns` を導く。
///
/// cpal 版(`cpal_backend::build_output_stream`)は `OutputStreamTimestamp` の
/// `playback`(予測出力時刻)と `callback`(コールバック自体の時刻)の差を、
/// デバイスの実バッファ長 + 追加レイテンシから見積もっている。AUHAL の
/// `AURenderCallback` は `mHostTime` をコールバック起動の基準時刻として渡してくるだけで
/// 予測出力時刻そのものは返さないため、ここでは **(1) このバッファの再生に要る時間
/// (`frames / sample_rate`)+ (2) デバイス側が申告する追加レイテンシ
/// (`open()` 時に1度だけ `query_device_extra_latency_frames` で読んだ
/// `device_extra_latency_ns`。cpal 0.18.1 の `get_device_extra_latency_frames` が読む
/// のと同じ `kAudioDevicePropertyLatency` + `kAudioDevicePropertySafetyOffset` の合計)**
/// を足したものを予測出力時刻として扱う——cpal が「デバイスの実バッファ長を取れない
/// ときはこのコールバックのフレーム数へフォールバックする」のと同じ考え方
/// (`cpal_backend.rs::build_output_stream` のコメント参照)。
fn compute_timestamps(
    callback_host_time_ns: u64,
    device_extra_latency_ns: u64,
    frames: u32,
    sample_rate: u32,
) -> (u64, u64) {
    let buffer_duration_ns = if sample_rate > 0 {
        (frames as u64 * 1_000_000_000) / sample_rate as u64
    } else {
        0
    };
    let output_latency_ns = device_extra_latency_ns.saturating_add(buffer_duration_ns);
    let buffer_start_host_time_ns = callback_host_time_ns.saturating_add(output_latency_ns);
    (buffer_start_host_time_ns, output_latency_ns)
}

/// frame 数を ns へ変換する(cpal 0.18.1 `host::frames_to_duration` と同じ式——
/// 分母が立たない場合〔`sample_rate == 0`〕は 0 を返すのも含めて一致させてある)。
/// 純関数——ハードウェア不要で単体テストできる(macOS でのみコンパイルされる。
/// `query_device_extra_latency_frames` が返す frame 数を ns 化するために使う——
/// iOS/tvOS の補正項は [`seconds_to_ns`] 経由で秒から直接 ns 化するため、
/// こちらは呼ばない)。
#[cfg(target_os = "macos")]
fn extra_latency_frames_to_ns(frames: u32, sample_rate: u32) -> u64 {
    if sample_rate == 0 {
        return 0;
    }
    (frames as u64 * 1_000_000_000) / sample_rate as u64
}

/// 秒(`f64`)を ns(`u64`)へ変換する。iOS/tvOS の `query_ios_output_latency_ns` が
/// `AVAudioSession.outputLatency()`(秒)を ns 化するために使う——`AVAudioSession` の
/// ゲッタは NSError を返さないが、`f64` の異常値(負・NaN・無限大)は音声スレッドに渡る
/// 前にここで弾く(§4.8 の思想。マイナスの出力レイテンシは意味を持たないので 0 にする)。
/// 純関数——ハードウェア不要で単体テストできる(OS を問わずコンパイルされる。
/// iOS/tvOS の実機呼び出し元〔`query_ios_output_latency_ns`〕だけが target_os 限定)。
///
/// `pub` にしてある理由は実装上の都合: macOS ビルドでは実際の呼び出し元
/// (`query_ios_output_latency_ns`)が `#[cfg]` で存在しないため、非公開のままだと
/// `dead_code` が立つ(`ios_interruption::confirm_recovery_progress` 等が同じ理由で
/// `pub` にしてあるのと同じ事情)。
pub fn seconds_to_ns(seconds: f64) -> u64 {
    if !seconds.is_finite() || seconds <= 0.0 {
        return 0;
    }
    (seconds * 1_000_000_000.0) as u64
}

// ============================================================================
// オープン済みユニットへの問い合わせ(実デバイス/実セッション必須。単体テスト対象外)
// ============================================================================

/// 既定出力デバイスの実際の(ハードウェア側)サンプルレートを読む。取得できなければ
/// [`FALLBACK_SAMPLE_RATE_HZ`] にフォールバックする(致命傷にしない。§4.8 の思想)。
///
/// `kAudioUnitScope_Output`(ハードウェア側)の `StreamFormat` を読むことで、アプリ側
/// (`kAudioUnitScope_Input` 側)に設定するフォーマットのサンプルレートをハードウェアへ
/// 合わせる——不一致のまま進めると AUHAL が暗黙にサンプルレート変換を挟むことになり、
/// 不要な遅延・音質劣化の余地を生む。
fn query_output_sample_rate(unit: AudioUnit) -> f64 {
    let mut asbd = stereo_f32_asbd(0.0);
    let mut size = std::mem::size_of::<AudioStreamBasicDescription>() as u32;
    // SAFETY: `unit` は呼び出し元が生成した有効なインスタンス。`asbd`/`size` は
    // スタック上の有効な書き込み先で、`size` は `asbd` の実サイズと一致する。
    let status = unsafe {
        AudioUnitGetProperty(
            unit,
            AUDIO_UNIT_PROPERTY_STREAM_FORMAT,
            AUDIO_UNIT_SCOPE_OUTPUT,
            0,
            &mut asbd as *mut _ as *mut c_void,
            &mut size,
        )
    };
    if status == NO_ERR && asbd.sample_rate.is_finite() && asbd.sample_rate > 0.0 {
        asbd.sample_rate
    } else {
        FALLBACK_SAMPLE_RATE_HZ
    }
}

/// AUHAL が実際に使っている `AudioDeviceID`(`kAudioOutputUnitProperty_CurrentDevice`、
/// スコープ Global)を読む。取得できなければ `None`
/// (`query_device_extra_latency_frames` はこれを「申告なし」として extra latency を
/// 0 にする)。**macOS のみ**(iOS/tvOS は `query_ios_output_latency_ns` を使う)。
#[cfg(target_os = "macos")]
fn query_current_device_id(unit: AudioUnit) -> Option<u32> {
    let mut device_id: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    // SAFETY: `unit` は呼び出し元が生成した有効なインスタンス。`device_id`/`size` は
    // スタック上の有効な書き込み先で、`size` は `device_id` の実サイズと一致する。
    let status = unsafe {
        AudioUnitGetProperty(
            unit,
            AUDIO_OUTPUT_UNIT_PROPERTY_CURRENT_DEVICE,
            AUDIO_UNIT_SCOPE_GLOBAL,
            0,
            &mut device_id as *mut _ as *mut c_void,
            &mut size,
        )
    };
    if status == NO_ERR {
        Some(device_id)
    } else {
        None
    }
}

/// `device_id` の device-level プロパティ(frame 数)を `AudioObjectGetPropertyData` で
/// 読む。スコープは常に出力側(`AUDIO_OBJECT_PROPERTY_SCOPE_OUTPUT`)。取得できなければ
/// 0(cpal 0.18.1 の `get_device_extra_latency_frames` が `.unwrap_or(0)` にしているのと
/// 同じ扱い)。**macOS のみ。**
#[cfg(target_os = "macos")]
fn query_device_property_frames(device_id: u32, selector: u32) -> u32 {
    let address = AudioObjectPropertyAddress {
        selector,
        scope: AUDIO_OBJECT_PROPERTY_SCOPE_OUTPUT,
        element: AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
    };
    let mut value: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    // SAFETY: `address`/`value`/`size` はスタック上の有効な値。`AudioObjectGetPropertyData`
    // はこれらを読み書きするだけで、所有権の移動は無い。`size` は `value` の実サイズと
    // 一致する。
    let status = unsafe {
        AudioObjectGetPropertyData(
            device_id,
            &address,
            0,
            std::ptr::null(),
            &mut size,
            &mut value as *mut _ as *mut c_void,
        )
    };
    if status == NO_ERR { value } else { 0 }
}

/// cpal 0.18.1 の `get_device_extra_latency_frames`
/// (`cpal::host::coreaudio::macos::device`、`~/.cargo/registry/src/*/cpal-0.18.1/
/// src/host/coreaudio/macos/device.rs`)と**同じ2プロパティの合計**
/// (`kAudioDevicePropertyLatency` + `kAudioDevicePropertySafetyOffset`、いずれも
/// 出力側 = `kAudioObjectPropertyScopeOutput`)を返す。cpal はこれを AUHAL の
/// `AudioUnitGetProperty` へ device-level のプロパティ ID のまま渡している(AUHAL が
/// アドレス変換して実デバイスへ転送する挙動に乗っている)が、ここでは
/// `kAudioOutputUnitProperty_CurrentDevice` で実デバイスの `AudioObjectID` を取り、
/// それへ直接 `AudioObjectGetPropertyData` を呼ぶ——読む対象のプロパティは同じなので
/// 値は一致するはずだが、AUHAL の転送という間接層を経由しない分、より素直に対応づけられる。
///
/// **`AudioUnitInitialize` の後**に呼ぶこと(`kAudioOutputUnitProperty_CurrentDevice` は
/// 初期化前は未確定の場合がある)。取得できなければ 0(致命傷にしない。§4.8 の思想)。
/// **macOS のみ。**
#[cfg(target_os = "macos")]
fn query_device_extra_latency_frames(unit: AudioUnit) -> u32 {
    let Some(device_id) = query_current_device_id(unit) else {
        return 0;
    };
    let device_latency_frames =
        query_device_property_frames(device_id, AUDIO_DEVICE_PROPERTY_LATENCY);
    let safety_offset_frames =
        query_device_property_frames(device_id, AUDIO_DEVICE_PROPERTY_SAFETY_OFFSET);
    device_latency_frames.saturating_add(safety_offset_frames)
}

/// `AVAudioSession.outputLatency()`(秒)を ns 化して返す——iOS/tvOS の「補正項」の
/// 出し方そのもの(モジュール doc「タイムスタンプの扱い」参照)。[`AppleBackend::open`]
/// が初期化時に1度読み、`IosOutputLatencyWatcher` がルート変化のたびに読み直して
/// `device_extra_latency_ns` を更新する。
///
/// **`IOBufferDuration` はここでは読まない**(`+ IOBufferDuration` を別途足すと二重計上に
/// なる)——このバッファの長さは [`compute_timestamps`] がレンダーコールバックの
/// **実測フレーム数**(`in_number_frames`)から求めており、RemoteIO が実際に渡してくる
/// フレーム数は採用された `IOBufferDuration` をそのまま反映する(cpal 0.18.1 のように
/// 起動時に1度だけ `AVAudioSession.IOBufferDuration()` をキャッシュするより、実測の方が
/// ルート変化後のバッファ長変化にも自然に追従できる)。
///
/// `AVAudioSession` の呼び出しは NSError を返さない単純なゲッタ(`ios_session.rs` が
/// 同じ前提で呼んでいる)なので、失敗しうる分岐は無い——異常値のケア(負・NaN・無限大)は
/// [`seconds_to_ns`] が担う。実セッションが要るため単体テスト対象外。
#[cfg(any(target_os = "ios", target_os = "tvos"))]
fn query_ios_output_latency_ns() -> u64 {
    // SAFETY: `AVAudioSession::sharedInstance()` はプロセス唯一の共有インスタンスを返す
    // だけの呼び出しで、`outputLatency()` は NSError を返さない単純なゲッタ。
    let seconds = unsafe { objc2_avf_audio::AVAudioSession::sharedInstance().outputLatency() };
    seconds_to_ns(seconds)
}

// ============================================================================
// レンダーコールバック(音声スレッド)
// ============================================================================

/// レンダーコールバック(音声スレッド)が排他的に触る状態。[`AppleBackend::open`] が
/// `Box::into_raw` でリーク相当にしたポインタを `AURenderCallbackStruct::
/// input_proc_ref_con` として渡し、[`AppleBackend::close`] が `Box::from_raw` で回収する
/// (cpal 版の `renderer` ムーブと同じ「単一の書き手」設計。§5.3)。
struct CallbackContext {
    renderer: Renderer,
    /// オープン時に確定したサンプルレート(オープン中は変わらない)。
    sample_rate: u32,
    /// 補正項(出力レイテンシ、ns)。[`AppleBackend::device_extra_latency_ns`] と同じ
    /// `Arc` を指す——macOS は `open()` が `AudioUnitInitialize` の直後に1度だけ
    /// `query_device_extra_latency_frames` で書き、以後固定(4-1 の既知の差分)。
    /// iOS/tvOS は `open()` が `query_ios_output_latency_ns` で初期化し、
    /// `IosOutputLatencyWatcher` がルート変化のたびに書き直す(4-2、HANDOFF 0f)。
    /// [`compute_timestamps`] の「相関点」(このコールバックの `AudioTimeStamp.
    /// mHostTime`)とは別に持つ——音声コールバックは毎回 `Relaxed` で読むだけ。
    device_extra_latency_ns: Arc<AtomicU64>,
    /// `render_proc` がこのコールバックの直前までに観測していた
    /// `device_extra_latency_ns` の値。**音声スレッド専用**(他スレッドからは一切
    /// 触らない単純なフィールドで、atomic にしていない——単一の書き手である
    /// `render_proc` 自身が毎コールバック読み書きするだけなので § 5.3 の対象にはならない)。
    ///
    /// レビュー【高】の修正: 補正項(`device_extra_latency_ns`)は iOS/tvOS では
    /// `IosOutputLatencyWatcher` がルート変化のたびに別スレッドから書き換える
    /// (`AppleBackend::open` のドキュメント参照)。`render_proc` はこの値を毎コールバック
    /// 読んで [`compute_timestamps`] の「補正項」として使うが、変化を検知しないまま
    /// 使うと `host_time_ns` が同じ世代のまま飛んでしまい、`clock.rs::
    /// MusicClockSnapshot` の契約(「世代を跨いだ外挿をしてはならない」)に反する。
    /// この直前値と毎コールバック比較し、変化していれば [`MusicClockPublisher::
    /// bump_generation`] を呼んでから新しい補正項で `buffer_start_host_time_ns` を
    /// 計算する(`render_proc` 本体参照)。macOS はここが実質的に no-op——
    /// `device_extra_latency_ns` は `open()` 時に1度書いたあと固定なので、この値と
    /// 毎回一致し続け、世代は一度も進まない(4-1 の既知の差分は変えない)。
    last_device_extra_latency_ns: u64,
    /// 音楽クロックの発行ハンドル(`Mixer::music_clock_handle` と同じ `Arc` を指す)。
    /// `render_proc` が補正項の変化を検知したときに [`MusicClockPublisher::
    /// bump_generation`] を呼ぶためだけに使う——`renderer.render()` 自身が内部で
    /// 行う `bump_generation` 呼び出し(シーク等)と同じ音声スレッドから呼ぶので、
    /// seqlock の単一書き手前提は崩れない([`Mixer::music_clock_handle`] のドキュメント
    /// 参照)。
    music_clock: Arc<MusicClockPublisher>,
    underrun_tracker: OutputUnderrunTracker,
    /// [`AppleBackend::callback_frames`] へ渡す `Arc`。
    callback_frames: Arc<AtomicU32>,
    /// [`AppleBackend::output_latency_ns`] へ渡す `Arc`。
    output_latency_ns: Arc<AtomicU64>,
    /// [`AppleBackend::close`] がストリーム停止後に `Acquire` で読むための同期点
    /// (`cpal_backend::CpalBackend::render_completions` と同じ理由)。
    render_completions: Arc<AtomicU64>,
}

/// CoreAudio の音声スレッドから直接呼ばれる `AURenderCallback`。
///
/// # SAFETY(呼び出し元が守る契約)
///
/// - `in_ref_con` は [`AppleBackend::open`] が `Box::into_raw::<CallbackContext>` で渡した
///   ポインタのまま、[`AppleBackend::close`] が回収するまで有効——CoreAudio は
///   `AudioOutputUnitStop` が返った後にはこのコールバックを呼ばないことが前提
///   (`AppleBackend::close` のドキュメント参照)。
/// - `in_time_stamp`/`io_data` はこの呼び出しの間だけ有効な、CoreAudio 所有のポインタ。
///
/// パニックは FFI 境界(CoreAudio の C フレーム)の外へ絶対に漏らさない
/// (`mw-ffi/CLAUDE.md` の「不変条件」と同じ方針をここでも適用する)。`Renderer::render`
/// 自体はパニックしない契約(§5.3)だが、ここでは契約が破られた場合の最後の防波堤として
/// `catch_unwind` で包む——巻き戻りが実際に起きない限りアロケーション・ロックを
/// 行わないため、リアルタイム安全性規約には抵触しない。
unsafe extern "C" fn render_proc(
    in_ref_con: *mut c_void,
    _io_action_flags: *mut u32,
    in_time_stamp: *const AudioTimeStamp,
    _in_bus_number: u32,
    in_number_frames: u32,
    io_data: *mut AudioBufferList,
) -> i32 {
    let caught = panic::catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: 呼び出し元の契約(関数 doc)により有効なポインタ。
        let context = unsafe { &mut *(in_ref_con as *mut CallbackContext) };

        // SAFETY: `io_data` は CoreAudio が渡す有効なポインタのはずだが、
        // defensively `as_mut` で確認してから使う(§4.8: 絶対にパニックしない)。
        let Some(io_data) = (unsafe { io_data.as_mut() }) else {
            return;
        };
        // インターリーブ形式(`kAudioFormatFlagIsNonInterleaved` を立てない、
        // `stereo_f32_asbd` 参照)なので `number_buffers` は常に1のはず。1以外は
        // (0 を含め)前提が崩れているとみなし、`buffers[0]` には一切触れずに書き込みを
        // スキップする。
        if io_data.number_buffers != 1 {
            return;
        }
        let buffer_data = io_data.buffers[0].data;
        let buffer_data_byte_size = io_data.buffers[0].data_byte_size;
        let Some(sample_count) =
            validated_sample_count(in_number_frames, buffer_data, buffer_data_byte_size)
        else {
            return;
        };
        // SAFETY: `validated_sample_count` が `sample_count * size_of::<f32>() <=
        // buffer_data_byte_size` を確認済み。`buffer_data` は CoreAudio が確保した
        // 16byte 境界に揃えられたバッファ(`AURenderCallback` のヘッダ doc)で、
        // インターリーブ形式(`kAudioFormatFlagIsNonInterleaved` 無し)なので
        // `number_buffers == 1` の単一バッファに全チャンネルが収まっている
        // (`stereo_f32_asbd` のドキュメント参照)。このコールバックの実行中のみ有効。
        let output =
            unsafe { std::slice::from_raw_parts_mut(buffer_data as *mut f32, sample_count) };

        context
            .callback_frames
            .store(in_number_frames, Ordering::Relaxed);

        // SAFETY: CoreAudio が渡す有効なポインタ(関数 doc の契約)。
        let callback_host_time_ns = unsafe { in_time_stamp.as_ref() }
            .map(|ts| mach_ticks_to_ns(ts.host_time))
            .unwrap_or(0);

        let device_extra_latency_ns = context.device_extra_latency_ns.load(Ordering::Relaxed);
        if device_extra_latency_ns != context.last_device_extra_latency_ns {
            // 補正項が変わった(iOS/tvOS: `IosOutputLatencyWatcher` が別スレッドで
            // ルート変化を検知し、書き換えた)。新しい相関点(このすぐ下の
            // `compute_timestamps`)が確定する前に世代を進める——`MusicClockPublisher::
            // bump_generation` のドキュメント「新しい相関点が確定する前に世代を進める」と
            // 同じ順序(`Mixer::render` 内部が discontinuity を処理する順序とも揃える)。
            // 閾値は設けていない: この値は `IosOutputLatencyWatcher` が実際にルート変化を
            // 検知して読み直した結果だけが書き込む(毎コールバック揺れる値ではない)ため、
            // 変化がどれだけ小さくても、それを同じ世代のまま跨いで外挿すると
            // `MusicClockSnapshot` の契約違反になる(`CallbackContext::
            // last_device_extra_latency_ns` のドキュメント参照)。
            context.music_clock.bump_generation();
            context.last_device_extra_latency_ns = device_extra_latency_ns;
        }

        let (buffer_start_host_time_ns, output_latency_ns) = compute_timestamps(
            callback_host_time_ns,
            device_extra_latency_ns,
            in_number_frames,
            context.sample_rate,
        );
        context
            .output_latency_ns
            .store(output_latency_ns, Ordering::Relaxed);

        context.underrun_tracker.observe(
            callback_host_time_ns,
            in_number_frames,
            context.sample_rate,
        );

        // 音声スレッド上で呼ぶ mw-core 側の経路は `Renderer::render` と、直前の
        // `MusicClockPublisher::bump_generation`(呼ぶのはこのコールバック自身、つまり
        // 音声スレッドからのみ)に限る(`mw-backend/CLAUDE.md` の設計意図)。
        // いずれもロック・アロケーションを伴わない atomic ストアのみで、
        // リアルタイム安全性規約(§5.3)に抵触しない。
        context.renderer.render(output, buffer_start_host_time_ns);

        context.render_completions.fetch_add(1, Ordering::Release);
    }));

    match caught {
        Ok(()) => NO_ERR,
        Err(_) => {
            // 契約が破られた場合の最後の防波堤。バッファへは書き込まない(無音ではなく
            // 直前の内容が残る可能性があるが、クラッシュさせないことを優先する
            // ——§4.8 の思想と同じ)。
            NO_ERR
        }
    }
}

// ============================================================================
// Backend 実装
// ============================================================================

/// macOS(AUHAL、`kAudioUnitSubType_DefaultOutput`)/ iOS・tvOS(RemoteIO、
/// `kAudioUnitSubType_RemoteIO`)の既定出力へ AudioUnit で直接出力する `Backend` 実装
/// (AUDIOWARE-DEPS-PLAN.md ステップ4-1〔macOS〕・4-2〔iOS/tvOS〕)。
pub struct AppleBackend {
    unit: Option<AudioUnit>,
    context_ptr: Option<*mut CallbackContext>,
    callback_frames: Arc<AtomicU32>,
    output_latency_ns: Arc<AtomicU64>,
    render_completions: Arc<AtomicU64>,
    logged_output_latency: AtomicBool,
    sample_rate: u32,
    output_underrun_count: Arc<AtomicU64>,
    last_output_underrun_host_time_ns: Arc<AtomicU64>,
    consecutive_output_underrun_count: Arc<AtomicU32>,
    logged_output_underrun_count: AtomicU64,
    /// 補正項(出力レイテンシ、ns)。[`CallbackContext::device_extra_latency_ns`] と
    /// 同じ `Arc` ——音声コールバックはこちら経由ではなく `CallbackContext` 側の
    /// クローンを読むが、書き込み(macOS は `open()` で1度だけ、iOS/tvOS は `open()` +
    /// ルート変化のたびに`IosOutputLatencyWatcher`)はこちらを通す。他の `Arc<Atomic*>`
    /// 群と同じく `AppleBackend::new()` で1度だけ生成し、`open()`/`close()` を跨いで
    /// 再利用する。
    device_extra_latency_ns: Arc<AtomicU64>,
    /// iOS/tvOS: ルート変化のたびに `device_extra_latency_ns` を更新するウォッチャ。
    /// macOS では常に `Some` だが中身は no-op(構築・破棄のコストも無い)。
    /// `open()`/`close()` と1対1(`cpal_backend::CpalBackend::ios_interruption` と同じ配線)。
    ios_output_latency_watcher: Option<IosOutputLatencyWatcher>,
}

// SAFETY: `unit`/`context_ptr` は生ポインタだが、`mw-ffi::handle::Instance` がグローバル
// レジストリの `Mutex` 経由で単一所有を保証するため、`AppleBackend` の `&mut self`
// メソッドが複数スレッドから同時に呼ばれることは無い(`cpal_backend::CpalBackend` と同じ
// 理由。`mw-ffi/CLAUDE.md` の「バックエンドをトレイトオブジェクトで持つ理由」参照)。
// `Sync` は要らない。
unsafe impl Send for AppleBackend {}

impl Default for AppleBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl AppleBackend {
    pub fn new() -> Self {
        Self {
            unit: None,
            context_ptr: None,
            callback_frames: Arc::new(AtomicU32::new(0)),
            output_latency_ns: Arc::new(AtomicU64::new(0)),
            render_completions: Arc::new(AtomicU64::new(0)),
            logged_output_latency: AtomicBool::new(false),
            sample_rate: 0,
            output_underrun_count: Arc::new(AtomicU64::new(0)),
            last_output_underrun_host_time_ns: Arc::new(AtomicU64::new(0)),
            consecutive_output_underrun_count: Arc::new(AtomicU32::new(0)),
            logged_output_underrun_count: AtomicU64::new(0),
            device_extra_latency_ns: Arc::new(AtomicU64::new(0)),
            ios_output_latency_watcher: None,
        }
    }
}

impl Backend for AppleBackend {
    fn open(&mut self, renderer: Renderer, _events: Arc<EventQueue>) -> Result<(), BackendError> {
        if self.unit.is_some() {
            return Err(BackendError::AlreadyOpen);
        }

        // iOS/tvOS では RemoteIO がハードウェアとネゴシエートする前にセッション
        // (カテゴリ・希望サンプルレート・希望 I/O バッファ長・setActive)を設定しておく
        // 必要がある(`cpal_backend::CpalBackend::open` と同じ順序の理由)。macOS では
        // no-op(`ios_session::configure` のドキュメント参照)。
        crate::ios_session::configure();

        let description = AudioComponentDescription {
            component_type: AUDIO_UNIT_TYPE_OUTPUT,
            component_sub_type: AUDIO_UNIT_SUBTYPE,
            component_manufacturer: AUDIO_UNIT_MANUFACTURER_APPLE,
            component_flags: 0,
            component_flags_mask: 0,
        };

        // SAFETY: `description` はスタック上の有効な値。`AudioComponentFindNext` は
        // システムのコンポーネント登録簿を検索するだけで、所有権の移動は無い
        // (見つからなければ null を返す。コンポーネント自体は dispose 不要)。
        let component = unsafe { AudioComponentFindNext(std::ptr::null_mut(), &description) };
        if component.is_null() {
            return Err(BackendError::NoOutputDevice);
        }

        let mut unit: AudioUnit = std::ptr::null_mut();
        // SAFETY: `component` は直前に取得した有効なハンドル。`&mut unit` はスタック上の
        // 有効な出力先。
        let status = unsafe { AudioComponentInstanceNew(component, &mut unit) };
        if status != NO_ERR || unit.is_null() {
            return Err(BackendError::BuildStreamFailed(format!(
                "AudioComponentInstanceNew failed: OSStatus {status}"
            )));
        }

        let sample_rate = query_output_sample_rate(unit);
        let asbd = stereo_f32_asbd(sample_rate);

        // SAFETY: `unit` は有効なインスタンス。`asbd` はスタック上の値で、サイズを
        // 正しく渡している。`kAudioUnitScope_Input` 側(アプリがデータを渡す側)への
        // 設定であり、ハードウェア側(`kAudioUnitScope_Output`)のフォーマットは
        // AUHAL が自動的に変換する(関数先頭のモジュール doc 参照)。
        let status = unsafe {
            AudioUnitSetProperty(
                unit,
                AUDIO_UNIT_PROPERTY_STREAM_FORMAT,
                AUDIO_UNIT_SCOPE_INPUT,
                0,
                &asbd as *const _ as *const c_void,
                std::mem::size_of::<AudioStreamBasicDescription>() as u32,
            )
        };
        if status != NO_ERR {
            // SAFETY: `unit` は `AudioComponentInstanceNew` が返した有効なインスタンスで、
            // まだ誰にも共有していない(エラーで抜ける前にここで確実に破棄する)。
            unsafe {
                AudioComponentInstanceDispose(unit);
            }
            return Err(BackendError::NoSupportedStreamConfig);
        }

        // `renderer` をムーブする**前に**音楽クロックの発行ハンドルを取る
        // (`Mixer::music_clock_handle` のドキュメント「ムーブする前に取ること」どおり。
        // `se_schedule_overflow_counter` を mw-ffi 側が同じ理由で先取りしているのと同じ
        // パターン)。
        let music_clock = renderer.music_clock_handle();

        let context = Box::new(CallbackContext {
            renderer,
            sample_rate: sample_rate as u32,
            // 初期値は 0(「まだ不明」)。初期化が成功した直後に確定させる(下記)——
            // `self.device_extra_latency_ns` と同じ `Arc` なので、以後の書き込みは
            // この `context_ptr` を経由しない(`self` 側の `Arc` へ直接 `store` する)。
            device_extra_latency_ns: Arc::clone(&self.device_extra_latency_ns),
            // 実際の初期値は `device_extra_latency_ns` と同じタイミングでは確定できない
            // (後述の算出は `AudioUnitInitialize` の後にしか行えない。macOS の
            // `query_device_extra_latency_frames` のドキュメント参照)——いったん 0 で置き、
            // 確定した直後に `context_ptr` 経由で書き直す(下記)。
            last_device_extra_latency_ns: 0,
            music_clock,
            underrun_tracker: OutputUnderrunTracker::new(
                Arc::clone(&self.output_underrun_count),
                Arc::clone(&self.last_output_underrun_host_time_ns),
                Arc::clone(&self.consecutive_output_underrun_count),
            ),
            callback_frames: Arc::clone(&self.callback_frames),
            output_latency_ns: Arc::clone(&self.output_latency_ns),
            render_completions: Arc::clone(&self.render_completions),
        });
        let context_ptr = Box::into_raw(context);

        let callback_struct = AURenderCallbackStruct {
            input_proc: render_proc,
            input_proc_ref_con: context_ptr as *mut c_void,
        };
        // SAFETY: `callback_struct` はスタック上の有効な値。`context_ptr` は直前に
        // `Box::into_raw` したばかりで、CoreAudio はまだこれを知らない
        // (`AudioOutputUnitStart` を呼ぶまでレンダーコールバックは起動しない)。
        let status = unsafe {
            AudioUnitSetProperty(
                unit,
                AUDIO_UNIT_PROPERTY_SET_RENDER_CALLBACK,
                AUDIO_UNIT_SCOPE_INPUT,
                0,
                &callback_struct as *const _ as *const c_void,
                std::mem::size_of::<AURenderCallbackStruct>() as u32,
            )
        };
        if status != NO_ERR {
            // SAFETY: `context_ptr` は直前にこの関数が `Box::into_raw` したばかりで、
            // CoreAudio を含め他のどこからも参照されていない(上のコメント参照)。
            drop(unsafe { Box::from_raw(context_ptr) });
            unsafe {
                AudioComponentInstanceDispose(unit);
            }
            return Err(BackendError::BuildStreamFailed(format!(
                "AudioUnitSetProperty(SetRenderCallback) failed: OSStatus {status}"
            )));
        }

        // SAFETY: `unit` はここまで有効なまま構成済み。
        let status = unsafe { AudioUnitInitialize(unit) };
        if status != NO_ERR {
            // SAFETY: `context_ptr` はまだ誰も(CoreAudio も)参照していない
            // (`AudioOutputUnitStart` を呼ぶ前)。
            drop(unsafe { Box::from_raw(context_ptr) });
            unsafe {
                AudioComponentInstanceDispose(unit);
            }
            return Err(BackendError::BuildStreamFailed(format!(
                "AudioUnitInitialize failed: OSStatus {status}"
            )));
        }

        // 補正項(出力レイテンシ)の出し方は OS で異なる(モジュール doc「タイムスタンプの
        // 扱い」参照)。`self.device_extra_latency_ns`(`CallbackContext` 側のクローンと
        // 同じ `Arc`)へ直接 `store` するだけなので、`context_ptr` の生ポインタ越しに
        // 書く必要が無い——レンダースレッドがまだ走っていない今この時点でも、
        // 走り出した後(ルート変化での更新)でも、同じ1行で安全に書ける。
        #[cfg(target_os = "macos")]
        let device_extra_latency_ns = {
            let device_extra_latency_frames = query_device_extra_latency_frames(unit);
            extra_latency_frames_to_ns(device_extra_latency_frames, sample_rate as u32)
        };
        #[cfg(any(target_os = "ios", target_os = "tvos"))]
        let device_extra_latency_ns = query_ios_output_latency_ns();
        self.device_extra_latency_ns
            .store(device_extra_latency_ns, Ordering::Relaxed);
        // SAFETY: `AudioOutputUnitStart` を呼ぶ前なので、レンダーコールバックは一度も
        // 起動していない(直後の SAFETY コメントと同じ前提)——`context_ptr` はまだ
        // この関数が排他的に所有している。ここで書くのは `render_proc` が最初の
        // コールバックで比較する基準値(`CallbackContext::last_device_extra_latency_ns`
        // のドキュメント参照)。0 のまま残すと、ルート変化が一度も起きていなくても
        // 最初のコールバックで「補正項が変わった」と誤検知し、無意味な世代の繰り上げが
        // 1回起きてしまう。
        unsafe {
            (*context_ptr).last_device_extra_latency_ns = device_extra_latency_ns;
        }

        // レビュー【低】の修正: ルート変化監視(iOS/tvOS)は `AudioOutputUnitStart` より
        // **前**に組み立てる——レンダーコールバックが実際に動き出す前に監視を始めることで、
        // 「Start した直後、監視がまだ無い間にルート変化が来て取り逃す」窓を塞ぐ
        // (macOS では `IosOutputLatencyWatcher::new` 自体が no-op なのでコストは無い)。
        // `close()` は依然これを一番最初に止める(逆順で対称)。
        self.ios_output_latency_watcher = Some(IosOutputLatencyWatcher::new(Arc::clone(
            &self.device_extra_latency_ns,
        )));

        // SAFETY: `unit` は初期化済み。
        let status = unsafe { AudioOutputUnitStart(unit) };
        if status != NO_ERR {
            // 直前に組み立てた監視を手放す——`open()` 自体が失敗して返るため、
            // `self.unit`/`self.context_ptr` は `None` のままで `is_open()` も
            // `false` のままになる(= 開いていない状態の一部として監視も無い状態に
            // 揃える)。
            self.ios_output_latency_watcher = None;
            // SAFETY: `AudioUnitInitialize` は成功したが `AudioOutputUnitStart` が
            // 失敗したため、レンダーコールバックは一度も呼ばれていない
            // (`context_ptr` はまだ排他的にこの関数が所有している)。
            unsafe {
                AudioUnitUninitialize(unit);
            }
            drop(unsafe { Box::from_raw(context_ptr) });
            unsafe {
                AudioComponentInstanceDispose(unit);
            }
            return Err(BackendError::PlayStreamFailed(format!(
                "AudioOutputUnitStart failed: OSStatus {status}"
            )));
        }

        crate::mw_log!(
            "[mw-backend] (native/apple) output stream started: sample_rate={} Hz, channels={}, \
             device_extra_latency={:.3} ms",
            sample_rate,
            CHANNELS,
            device_extra_latency_ns as f64 / 1_000_000.0,
        );

        self.unit = Some(unit);
        self.context_ptr = Some(context_ptr);
        self.sample_rate = sample_rate as u32;
        Ok(())
    }

    fn close(&mut self) -> Result<(), BackendError> {
        // ルート変化の監視を先に止める(iOS/tvOS。macOS では no-op)——ストリームが
        // 止まりかけの状態で補正項を書き直されないようにする(`cpal_backend::
        // CpalBackend::close` が `ios_interruption` を先に止めるのと同じ理由)。
        self.ios_output_latency_watcher = None;

        match (self.unit.take(), self.context_ptr.take()) {
            (Some(unit), Some(context_ptr)) => {
                // Apple が推奨する解体順序(Stop → Uninitialize → Dispose)をそのまま守る。
                // SAFETY: `unit` はこの `AppleBackend` だけが所有しており(`take()` で
                // 既に自分からも外した)、`open()` が成功させたまま一度も dispose
                // していない有効なインスタンス。
                unsafe {
                    AudioOutputUnitStop(unit);
                    AudioUnitUninitialize(unit);
                    AudioComponentInstanceDispose(unit);
                }

                // `AudioOutputUnitStop` が実際に同期的にレンダースレッドの完了を待つかは
                // ヘッダのドキュメントに明記が無いため、`cpal_backend::CpalBackend::close`
                // と同じ保険を入れる——直近コールバックが `Release` で残した書き込みを
                // この `Acquire` で明示的に観測してから、直後の `Box::from_raw` の drop が
                // それを読む前に happens-before の辺を明示しておく。
                // SAFETY: `context_ptr` が指すメモリは `open()` 以来有効であり、上の
                // Stop/Uninitialize/Dispose 後もまだ `Box::from_raw` で回収していない。
                let _ = unsafe { (*context_ptr).render_completions.load(Ordering::Acquire) };
                // SAFETY: `open()` で `Box::into_raw` したポインタをそのまま回収する。
                // レンダースレッドは上の `AudioOutputUnitStop`/`AudioComponentInstanceDispose`
                // より後には呼ばれない。
                drop(unsafe { Box::from_raw(context_ptr) });

                self.sample_rate = 0;
                self.callback_frames.store(0, Ordering::Relaxed);
                self.render_completions.store(0, Ordering::Relaxed);
                self.output_latency_ns.store(0, Ordering::Relaxed);
                self.logged_output_latency.store(false, Ordering::Relaxed);
                self.output_underrun_count.store(0, Ordering::Relaxed);
                self.last_output_underrun_host_time_ns
                    .store(0, Ordering::Relaxed);
                self.consecutive_output_underrun_count
                    .store(0, Ordering::Relaxed);
                self.logged_output_underrun_count
                    .store(0, Ordering::Relaxed);
                self.device_extra_latency_ns.store(0, Ordering::Relaxed);
                Ok(())
            }
            _ => Err(BackendError::NotOpen),
        }
    }

    fn is_open(&self) -> bool {
        self.unit.is_some()
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
            "[mw-backend] (native/apple) output latency (measured, buffer duration + device \
             latency + safety offset): {latency_ns} ns = {latency_ms:.3} ms"
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
            "[mw-backend] (native/apple) output underrun suspected: +{new_count} since last \
             check (cumulative={current}, consecutive={consecutive})"
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

// ============================================================================
// 補正項(出力レイテンシ)のルート変化追従(iOS/tvOS のみ。AUDIOWARE-DEPS-PLAN.md
// ステップ4-2、HANDOFF 0f の統合)
// ============================================================================

/// `AVAudioSessionRouteChangeNotification` を監視し、ルート(スピーカー/有線/
/// Bluetooth)が変わるたびに `device_extra_latency_ns`(補正項)を
/// `query_ios_output_latency_ns` で読み直す。macOS では no-op。
///
/// **`ios_interruption::Watcher` とは独立**——あちらは cpal の `Stream` の
/// `pause()`/`play()` を再試行する割り込み復帰ロジック(電話・Siri・バックグラウンド
/// 遷移からの再始動)を持つが、こちらはそれを持たない。責務は「補正項を最新に保つ」
/// ことだけで、AudioUnit 自体には一切触れない(モジュール doc「cpal 版との既知の差分」
/// 参照——電話着信等からのストリーム復帰は本バックエンドでは未実装)。
pub(super) struct IosOutputLatencyWatcher {
    #[cfg(any(target_os = "ios", target_os = "tvos"))]
    #[allow(dead_code)] // Drop 経由で `removeObserver` させるためだけに保持する
    inner: ios_impl::Watcher,
}

impl IosOutputLatencyWatcher {
    /// `device_extra_latency_ns` はルート変化のたびに書き直す先(`AppleBackend`/
    /// `CallbackContext` と同じ `Arc` を指す)。
    #[cfg(any(target_os = "ios", target_os = "tvos"))]
    fn new(device_extra_latency_ns: Arc<AtomicU64>) -> Self {
        Self {
            inner: ios_impl::Watcher::new(device_extra_latency_ns),
        }
    }

    /// macOS では何もしない(常に `Some(IosOutputLatencyWatcher::new(..))` を構築しても
    /// コストが無いようにするための no-op。`ios_interruption::Watcher` と同じ設計)。
    #[cfg(not(any(target_os = "ios", target_os = "tvos")))]
    fn new(_device_extra_latency_ns: Arc<AtomicU64>) -> Self {
        Self {}
    }
}

#[cfg(any(target_os = "ios", target_os = "tvos"))]
mod ios_impl {
    use std::panic::{self, AssertUnwindSafe};
    use std::ptr::NonNull;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2::runtime::NSObjectProtocol;
    use objc2::runtime::ProtocolObject;
    use objc2_avf_audio::AVAudioSessionRouteChangeNotification;
    use objc2_foundation::{NSNotification, NSNotificationCenter};

    use super::query_ios_output_latency_ns;

    pub(super) struct Watcher {
        /// 通知を受け取らなくなったら即座に `removeObserver` できるよう保持する
        /// (`AVAudioSessionRouteChangeNotification` が取得できなかった場合は `None`
        /// ——その場合は補正項が初期値〔`open()` 時の1回だけ〕のまま固定される)。
        observer: Option<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
    }

    // SAFETY: `NSNotificationCenter` はスレッドセーフ。保持する `observer` は `Drop` で
    // `removeObserver` するためだけの不透明なトークンで、他スレッドと共有する可変状態は
    // 持たない(`ios_interruption::imp::Watcher` と同じ理由づけ)。
    unsafe impl Send for Watcher {}
    unsafe impl Sync for Watcher {}

    impl Watcher {
        pub(super) fn new(device_extra_latency_ns: Arc<AtomicU64>) -> Self {
            let nc = NSNotificationCenter::defaultCenter();
            let block = RcBlock::new(move |_: NonNull<NSNotification>| {
                // パニックは Objective-C ランタイムの外へ絶対に漏らさない
                // (`ios_interruption.rs` と同じ方針)。
                let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
                    let ns = query_ios_output_latency_ns();
                    device_extra_latency_ns.store(ns, Ordering::Relaxed);
                    crate::mw_log!(
                        "[mw-backend] (native/apple ios) output latency correction updated \
                         after a route change: {:.3} ms",
                        ns as f64 / 1_000_000.0,
                    );
                }));
                if outcome.is_err() {
                    crate::mw_log!(
                        "[mw-backend] (native/apple ios) panic while handling \
                         AVAudioSessionRouteChangeNotification (caught at the boundary)"
                    );
                }
            });
            // SAFETY: 通知名はプロセス生存中変化しない静的値。
            // `addObserverForName_object_queue_usingBlock` は block を Objective-C
            // ランタイムが retain する契約どおりに呼ぶ(`ios_interruption.rs` と同じ)。
            let observer = unsafe { AVAudioSessionRouteChangeNotification }.map(|name| unsafe {
                nc.addObserverForName_object_queue_usingBlock(Some(name), None, None, &block)
            });
            if observer.is_none() {
                crate::mw_log!(
                    "[mw-backend] (native/apple ios) AVAudioSessionRouteChangeNotification is \
                     unavailable; the output latency correction will not track route changes"
                );
            }
            Self { observer }
        }
    }

    impl Drop for Watcher {
        fn drop(&mut self) {
            if let Some(observer) = self.observer.take() {
                let nc = NSNotificationCenter::defaultCenter();
                // SAFETY: `observer` はこの `Watcher` が `addObserverForName_...` から
                // 受け取ったトークンそのもの。
                unsafe { nc.removeObserver(observer.as_ref()) };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stereo_f32_asbd_describes_interleaved_f32_stereo_pcm() {
        let asbd = stereo_f32_asbd(48_000.0);
        assert_eq!(asbd.sample_rate, 48_000.0);
        assert_eq!(asbd.format_id, AUDIO_FORMAT_LINEAR_PCM);
        assert_eq!(
            asbd.format_flags,
            AUDIO_FORMAT_FLAG_IS_FLOAT | AUDIO_FORMAT_FLAG_IS_PACKED
        );
        assert_eq!(asbd.frames_per_packet, 1, "PCM は1フレーム=1パケット");
        assert_eq!(asbd.channels_per_frame, 2);
        assert_eq!(asbd.bits_per_channel, 32);
        assert_eq!(asbd.bytes_per_frame, 8, "2ch * 4bytes");
        assert_eq!(asbd.bytes_per_packet, 8);
        assert_eq!(asbd.reserved, 0);
    }

    /// 実際に書き込んでよい、4byte アラインされた有効なバッファ(テスト用)。
    fn aligned_test_buffer() -> [f32; 4096] {
        [0.0; 4096]
    }

    #[test]
    fn validated_sample_count_accepts_an_exactly_sized_buffer() {
        let mut buf = aligned_test_buffer();
        assert_eq!(
            validated_sample_count(512, buf.as_mut_ptr().cast(), 512 * 2 * 4),
            Some(512 * 2)
        );
    }

    #[test]
    fn validated_sample_count_rejects_a_null_buffer() {
        assert_eq!(
            validated_sample_count(512, std::ptr::null_mut(), 512 * 2 * 4),
            None
        );
    }

    #[test]
    fn validated_sample_count_rejects_an_undersized_buffer() {
        let mut buf = aligned_test_buffer();
        assert_eq!(
            validated_sample_count(512, buf.as_mut_ptr().cast(), 512 * 2 * 4 - 1),
            None
        );
    }

    #[test]
    fn validated_sample_count_rejects_zero_frames_even_with_a_valid_buffer() {
        let mut buf = aligned_test_buffer();
        assert_eq!(validated_sample_count(0, buf.as_mut_ptr().cast(), 0), None);
    }

    /// 再現テスト(レビュー指摘2): 0 フレームのとき `data` の値を一切見ずに弾くはず
    /// だが、現状はアラインメントを見ないまま `Some(0)` を返してしまう。呼び出し側は
    /// それを基に(0 要素とはいえ)`slice::from_raw_parts_mut` を呼ぶため、ダングリング・
    /// 非アラインポインタでも前提違反になる。このポインタは一切 dereference しない
    /// (アドレス値の検査だけ)ので、作るだけなら未定義動作ではない。
    #[test]
    fn validated_sample_count_rejects_zero_frames_with_a_dangling_unaligned_pointer() {
        let dangling_unaligned = std::ptr::dangling_mut::<c_void>();
        assert_eq!(validated_sample_count(0, dangling_unaligned, 0), None);
    }

    /// 再現テスト(レビュー指摘2、非 0 フレーム側): アラインメント検査が無いため、
    /// 1byte しかずれていない non-null ポインタでもサイズ条件さえ満たせば
    /// 通ってしまう。
    #[test]
    fn validated_sample_count_rejects_a_misaligned_nonzero_buffer() {
        let mut buf = aligned_test_buffer();
        // 1byte ずらす(f32 のアラインメントは4byte なので非アライン化できる)。
        let misaligned = unsafe { buf.as_mut_ptr().cast::<u8>().add(1) }.cast::<c_void>();
        assert_eq!(validated_sample_count(4, misaligned, 4 * 2 * 4), None);
    }

    #[test]
    fn compute_timestamps_adds_buffer_duration_and_unit_latency() {
        // 512 frames @ 48kHz = 10_666_666ns(整数除算で切り捨て)。
        let (buffer_start, latency) = compute_timestamps(1_000_000_000, 2_000_000, 512, 48_000);
        assert_eq!(latency, 2_000_000 + 10_666_666);
        assert_eq!(buffer_start, 1_000_000_000 + latency);
    }

    #[test]
    fn compute_timestamps_is_zero_latency_when_sample_rate_is_unknown() {
        let (buffer_start, latency) = compute_timestamps(1_000_000_000, 0, 512, 0);
        assert_eq!(latency, 0);
        assert_eq!(buffer_start, 1_000_000_000);
    }

    #[test]
    fn compute_timestamps_saturates_instead_of_overflowing() {
        let (buffer_start, latency) = compute_timestamps(u64::MAX, u64::MAX, 512, 48_000);
        assert_eq!(buffer_start, u64::MAX);
        assert!(latency > 0);
    }

    /// AUDIOWARE-DEPS-PLAN.md ステップ4-2(HANDOFF 0f の統合)の核心を固定化する:
    /// **相関点(`callback_host_time_ns`)と補正項(`device_extra_latency_ns`)は
    /// 独立な状態として持たれ**、補正項だけが書き換わっても(iOS/tvOS の
    /// `IosOutputLatencyWatcher` がルート変化のたびに行う更新を模している)、
    /// 相関点の読み方(`AudioTimeStamp.mHostTime` を ns 化するだけ)は一切変えずに
    /// `buffer_start_host_time_ns` へ差分がそのまま反映されることを確認する。
    #[test]
    fn compute_timestamps_reflects_a_route_change_driven_correction_update_independently_of_the_correlation_point()
     {
        let correction_ns = Arc::new(AtomicU64::new(2_000_000)); // 2ms(例: 有線ルート)
        let callback_host_time_ns = 1_000_000_000u64; // 相関点——以下では一切変えない

        let (start_before, latency_before) = compute_timestamps(
            callback_host_time_ns,
            correction_ns.load(Ordering::Relaxed),
            512,
            48_000,
        );

        // ルート変化(例: Bluetooth A2DP へ切替)を模倣: `IosOutputLatencyWatcher` が
        // 行うのはこの1本のアトミックへの `store` だけで、相関点には触れない。
        correction_ns.store(80_000_000, Ordering::Relaxed); // 80ms(例: A2DP)

        let (start_after, latency_after) = compute_timestamps(
            callback_host_time_ns, // 相関点は不変
            correction_ns.load(Ordering::Relaxed),
            512,
            48_000,
        );

        assert!(
            latency_after > latency_before,
            "補正項の増加がそのまま反映されるはず"
        );
        assert_eq!(
            start_after - start_before,
            latency_after - latency_before,
            "相関点は変えず、補正項の差分だけが buffer_start_host_time_ns に反映されるはず"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn extra_latency_frames_to_ns_converts_using_the_sample_rate() {
        // 48 frames @ 48kHz = 1ms。
        assert_eq!(extra_latency_frames_to_ns(48, 48_000), 1_000_000);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn extra_latency_frames_to_ns_is_zero_when_sample_rate_is_unknown() {
        assert_eq!(extra_latency_frames_to_ns(48, 0), 0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn extra_latency_frames_to_ns_is_zero_for_zero_frames() {
        assert_eq!(extra_latency_frames_to_ns(0, 48_000), 0);
    }

    /// iOS/tvOS の `query_ios_output_latency_ns`
    /// (`AVAudioSession.outputLatency()` の ns 化)が使う純関数部分。
    #[test]
    fn seconds_to_ns_converts_a_typical_output_latency_value() {
        // 17.9ms(基準端末 iPhone 14 の実測出力レイテンシ、初期構築仕様『§1』)。
        assert_eq!(seconds_to_ns(0.0179), 17_900_000);
    }

    #[test]
    fn seconds_to_ns_floors_a_negative_value_to_zero() {
        assert_eq!(seconds_to_ns(-0.005), 0);
    }

    #[test]
    fn seconds_to_ns_is_zero_for_exactly_zero() {
        assert_eq!(seconds_to_ns(0.0), 0);
    }

    #[test]
    fn seconds_to_ns_is_zero_for_nan() {
        assert_eq!(seconds_to_ns(f64::NAN), 0);
    }

    #[test]
    fn seconds_to_ns_is_zero_for_infinity() {
        assert_eq!(seconds_to_ns(f64::INFINITY), 0);
        assert_eq!(seconds_to_ns(f64::NEG_INFINITY), 0);
    }

    /// ハードウェア不要の構築テスト: `AppleBackend::new()` はまだ閉じている。
    #[test]
    fn new_backend_starts_closed() {
        let backend = AppleBackend::new();
        assert!(!backend.is_open());
        assert_eq!(backend.sample_rate(), 0);
        assert_eq!(backend.last_callback_frames(), 0);
        assert_eq!(backend.output_latency_ns(), 0);
        assert_eq!(backend.output_underrun_count(), 0);
    }

    /// ハードウェア不要: 開いていないバックエンドを閉じようとすると `NotOpen`。
    #[test]
    fn closing_an_unopened_backend_reports_not_open() {
        let mut backend = AppleBackend::new();
        assert!(matches!(backend.close(), Err(BackendError::NotOpen)));
    }

    /// 実デバイスを開いて数百 ms 鳴らす(無音)。CI・ヘッドレス環境では実行しない。
    /// `cargo test --features backend-native -- --ignored` で手動実行する。
    #[test]
    #[ignore = "実デバイス(既定の出力)を開いて鳴らすため、手元の macOS でのみ実行する"]
    fn opens_the_real_default_output_device_and_renders_silently_for_a_while() {
        let (renderer, _sender, _reclaim, _music_producer, _music_clock, events, _bgm) =
            mw_core::Renderer::build(mw_core::Config::default(), 48_000);

        let mut backend = AppleBackend::new();
        backend
            .open(renderer, events)
            .expect("open should succeed on a machine with a real default output device");

        std::thread::sleep(std::time::Duration::from_millis(300));

        assert!(
            backend.last_callback_frames() > 0,
            "the render callback never ran"
        );
        assert!(backend.sample_rate() > 0);

        backend.close().expect("close should succeed");
        assert!(!backend.is_open());
    }

    /// テスト専用: `render_proc` を直接呼ぶための最小の `CallbackContext`。
    fn test_callback_context(sample_rate: u32) -> *mut CallbackContext {
        let (renderer, _sender, _reclaim, _music_producer, music_clock, _events, _bgm) =
            mw_core::Renderer::build(mw_core::Config::default(), sample_rate);
        Box::into_raw(Box::new(CallbackContext {
            renderer,
            sample_rate,
            device_extra_latency_ns: Arc::new(AtomicU64::new(0)),
            last_device_extra_latency_ns: 0,
            music_clock,
            underrun_tracker: OutputUnderrunTracker::new(
                Arc::new(AtomicU64::new(0)),
                Arc::new(AtomicU64::new(0)),
                Arc::new(AtomicU32::new(0)),
            ),
            callback_frames: Arc::new(AtomicU32::new(0)),
            output_latency_ns: Arc::new(AtomicU64::new(0)),
            render_completions: Arc::new(AtomicU64::new(0)),
        }))
    }

    /// ハードウェア不要・ダミー `AudioTimeStamp`(host_time 以外は読まれないので 0 で良い)。
    fn dummy_time_stamp(host_time: u64) -> AudioTimeStamp {
        AudioTimeStamp {
            sample_time: 0.0,
            host_time,
            rate_scalar: 1.0,
            word_clock_time: 0,
            smpte_time: SmpteTime {
                subframes: 0,
                subframe_divisor: 0,
                counter: 0,
                time_type: 0,
                flags: 0,
                hours: 0,
                minutes: 0,
                seconds: 0,
                frames: 0,
            },
            flags: 0,
            reserved: 0,
        }
    }

    /// 再現テスト(レビュー指摘4): `AudioBufferList::number_buffers` がインターリーブ形式の
    /// 前提(常に1)と異なる値のとき、実装はコメント(`AudioBufferList` の doc)が言う
    /// 「念のため確認する」を満たしていない——現状は `== 0` しか弾かず、2以上は
    /// そのまま `buffers[0]` を使って書き込んでしまう。
    #[test]
    fn render_proc_skips_writing_when_number_buffers_is_not_one() {
        let context_ptr = test_callback_context(48_000);

        const SENTINEL: f32 = 1.234_5;
        let mut buffer = [SENTINEL; 8]; // 4 frames * 2ch
        let mut buffer_list = AudioBufferList {
            // 本来インターリーブ形式では常に1のはずの値を、わざと2にして再現する。
            number_buffers: 2,
            buffers: [AudioBuffer {
                number_channels: 2,
                data_byte_size: (buffer.len() * std::mem::size_of::<f32>()) as u32,
                data: buffer.as_mut_ptr().cast(),
            }],
        };
        let time_stamp = dummy_time_stamp(1_000);

        let status = unsafe {
            render_proc(
                context_ptr.cast(),
                std::ptr::null_mut(),
                &time_stamp,
                0,
                4,
                &mut buffer_list,
            )
        };

        assert_eq!(status, NO_ERR);
        assert_eq!(
            buffer, [SENTINEL; 8],
            "number_buffers != 1 のときは書き込みをスキップするはず"
        );
        let context = unsafe { &*context_ptr };
        assert_eq!(
            context.callback_frames.load(Ordering::Relaxed),
            0,
            "スキップしたときは callback_frames も更新されないはず"
        );

        drop(unsafe { Box::from_raw(context_ptr) });
    }

    /// `number_buffers == 1`(正常系)では引き続き書き込むことの確認——上のテストが
    /// 「2以上なら弾く」を固定するのに対し、こちらは「1なら通る」を固定する。
    #[test]
    fn render_proc_writes_when_number_buffers_is_one() {
        let context_ptr = test_callback_context(48_000);

        const SENTINEL: f32 = 1.234_5;
        let mut buffer = [SENTINEL; 8]; // 4 frames * 2ch
        let mut buffer_list = AudioBufferList {
            number_buffers: 1,
            buffers: [AudioBuffer {
                number_channels: 2,
                data_byte_size: (buffer.len() * std::mem::size_of::<f32>()) as u32,
                data: buffer.as_mut_ptr().cast(),
            }],
        };
        let time_stamp = dummy_time_stamp(1_000);

        let status = unsafe {
            render_proc(
                context_ptr.cast(),
                std::ptr::null_mut(),
                &time_stamp,
                0,
                4,
                &mut buffer_list,
            )
        };

        assert_eq!(status, NO_ERR);
        assert_ne!(
            buffer, [SENTINEL; 8],
            "number_buffers == 1 では Renderer::render が書き込むはず"
        );
        let context = unsafe { &*context_ptr };
        assert_eq!(context.callback_frames.load(Ordering::Relaxed), 4);

        drop(unsafe { Box::from_raw(context_ptr) });
    }

    /// レビュー【高】の再現: `device_extra_latency_ns`(補正項)が書き換わっても、
    /// `render_proc` は現状これを検知せず、`MusicClockPublisher` の世代(generation)が
    /// 進まないまま次のコールバックで `host_time_ns` だけが飛ぶ。これは `clock.rs::
    /// MusicClockSnapshot` の契約(「世代を跨いだ外挿をしてはならない」)に反する——
    /// iOS/tvOS では `IosOutputLatencyWatcher` がルート変化のたびにこの値を書き換える
    /// (`AppleBackend::open` のドキュメント参照)。
    #[test]
    fn render_proc_bumps_music_clock_generation_when_the_latency_correction_changes() {
        let context_ptr = test_callback_context(48_000);

        let mut buffer = [0.0f32; 8]; // 4 frames * 2ch
        let mut buffer_list = AudioBufferList {
            number_buffers: 1,
            buffers: [AudioBuffer {
                number_channels: 2,
                data_byte_size: (buffer.len() * std::mem::size_of::<f32>()) as u32,
                data: buffer.as_mut_ptr().cast(),
            }],
        };
        let time_stamp = dummy_time_stamp(1_000);

        let status = unsafe {
            render_proc(
                context_ptr.cast(),
                std::ptr::null_mut(),
                &time_stamp,
                0,
                4,
                &mut buffer_list,
            )
        };
        assert_eq!(status, NO_ERR);

        let generation_before = unsafe { (*context_ptr).renderer.music_clock_handle() }
            .snapshot()
            .generation;

        // ルート変化を模す: 監視スレッド(`IosOutputLatencyWatcher`)が補正項を書き換える
        // 想定の再現。ここでは `render_proc` を直接呼ぶテストなので、同じスレッドから
        // store するだけで「検知すべき変化」自体は忠実に再現できる(検知ロジックは
        // この値を読むだけで、どのスレッドが書いたかを区別しない)。
        unsafe {
            (*context_ptr)
                .device_extra_latency_ns
                .store(80_000_000, Ordering::Relaxed) // 80ms(例: A2DP への切替)
        };

        let status = unsafe {
            render_proc(
                context_ptr.cast(),
                std::ptr::null_mut(),
                &time_stamp,
                0,
                4,
                &mut buffer_list,
            )
        };
        assert_eq!(status, NO_ERR);

        let generation_after = unsafe { (*context_ptr).renderer.music_clock_handle() }
            .snapshot()
            .generation;

        assert_ne!(
            generation_before, generation_after,
            "補正項が変わったのに世代が進んでいない(MusicClockSnapshot の契約違反)"
        );

        drop(unsafe { Box::from_raw(context_ptr) });
    }

    /// 上のテストと対になる固定化: 補正項が**変わらない**限り、世代は進まないはず
    /// (`AppleBackend::open` が最初のコールバックより前に `last_device_extra_latency_ns`
    /// を実際の初期値で埋めておく修正の固定化——ここを 0 のまま放置すると、ルート変化が
    /// 一度も起きていない macOS/通常運用でも最初のコールバックで無意味な世代の繰り上げが
    /// 起きてしまう)。複数回のコールバックをまたいでも成り立つことを確認する。
    #[test]
    fn render_proc_does_not_bump_music_clock_generation_when_the_latency_correction_is_unchanged() {
        let context_ptr = test_callback_context(48_000);
        // `test_callback_context` と同じ初期値(0)で明示的に揃えておく——以後一度も
        // 書き換えない。
        unsafe {
            (*context_ptr)
                .device_extra_latency_ns
                .store(0, Ordering::Relaxed);
            (*context_ptr).last_device_extra_latency_ns = 0;
        }

        let mut buffer = [0.0f32; 8]; // 4 frames * 2ch
        let mut buffer_list = AudioBufferList {
            number_buffers: 1,
            buffers: [AudioBuffer {
                number_channels: 2,
                data_byte_size: (buffer.len() * std::mem::size_of::<f32>()) as u32,
                data: buffer.as_mut_ptr().cast(),
            }],
        };
        let time_stamp = dummy_time_stamp(1_000);

        for _ in 0..3 {
            let status = unsafe {
                render_proc(
                    context_ptr.cast(),
                    std::ptr::null_mut(),
                    &time_stamp,
                    0,
                    4,
                    &mut buffer_list,
                )
            };
            assert_eq!(status, NO_ERR);
        }

        let generation = unsafe { (*context_ptr).renderer.music_clock_handle() }
            .snapshot()
            .generation;
        assert_eq!(
            generation, 0,
            "補正項が一度も変わっていないのに世代が進んでいる(誤検知)"
        );

        drop(unsafe { Box::from_raw(context_ptr) });
    }
}
