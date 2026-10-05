//! macOS 用の自前 AudioUnit(AUHAL)バックエンド(AUDIOWARE-DEPS-PLAN.md ステップ4-1)。
//!
//! **新しい外部クレートは追加していない。** AudioToolbox フレームワークの C API を
//! 自前の `extern "C"` 宣言(下の「AudioToolbox の FFI 宣言」節)で直接呼ぶ——
//! `objc2-audio-toolbox` 等は使わない(計画書 §4 の決定どおり。それらは `objc2`/
//! `objc2-foundation` を引き込むため、依存排除の到達点〔表記ゼロ〕に反する)。
//!
//! 手順は計画書の記述そのまま: `AudioComponentFindNext` → `AudioComponentInstanceNew` →
//! `AudioUnitSetProperty`(StreamFormat / SetRenderCallback)→ `AudioUnitInitialize` →
//! `AudioOutputUnitStart`。使うコンポーネントは `kAudioUnitSubType_DefaultOutput`
//! (既定の出力デバイスへ自動的に追従する。iOS の RemoteIO とほぼ同じ構えで、
//! 4-2 で iOS を追加するときの差分はサブタイプと `ios_session`/`ios_interruption` との
//! 接続だけに絞れる見込み)。
//!
//! # cpal 版との既知の差分(4-1 の時点で未対応)
//!
//! デバイスの切断・既定出力デバイスの変更を監視する `AudioObjectPropertyListener` は
//! 実装していない。そのため [`Backend::open`] が受け取る `events` はこの実装では
//! 使わず、`StreamError` イベントは発行されない。macOS Editor 専用の開発機バックエンド
//! という 4-1 の前提(出荷対象ではなく、実機も Unity ロックも要らない)の範囲では
//! 許容する判断とした——必要になれば 4-2(iOS の `ios_interruption` 統合)に合わせて
//! 検討する。出力コールバック自体の間隔異常(「アンダーラン(の疑い)」)は
//! cpal 版と同じ [`crate::underrun::OutputUnderrunTracker`] で検知する。
//!
//! # タイムスタンプの扱い
//!
//! `AURenderCallback` が渡す `AudioTimeStamp.mHostTime` は `mach_absolute_time()` と
//! 同じ時計源(`host_time.rs` のモジュール doc の調査結果どおり)なので、
//! [`crate::host_time::mach_ticks_to_ns`](同じ関数を cpal 版の時計整合性でも使っている)
//! で ns 化する。そこから `Renderer::render` に渡す `buffer_start_host_time_ns`
//! (予測される出力時刻)を導く式は [`compute_timestamps`] のドキュメント参照。

use std::ffi::c_void;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use mw_core::{CHANNELS, EventQueue, Renderer};

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
const AUDIO_UNIT_SUBTYPE_DEFAULT_OUTPUT: u32 = 0x6465_6620; // 'def '
const AUDIO_UNIT_MANUFACTURER_APPLE: u32 = 0x6170_706c; // 'appl'

const AUDIO_UNIT_SCOPE_GLOBAL: u32 = 0;
const AUDIO_UNIT_SCOPE_INPUT: u32 = 1;
const AUDIO_UNIT_SCOPE_OUTPUT: u32 = 2;

const AUDIO_UNIT_PROPERTY_STREAM_FORMAT: u32 = 8;
const AUDIO_UNIT_PROPERTY_LATENCY: u32 = 12;
const AUDIO_UNIT_PROPERTY_SET_RENDER_CALLBACK: u32 = 23;

const AUDIO_FORMAT_LINEAR_PCM: u32 = 0x6c70_636d; // 'lpcm'
const AUDIO_FORMAT_FLAG_IS_FLOAT: u32 = 1 << 0;
const AUDIO_FORMAT_FLAG_IS_PACKED: u32 = 1 << 3;

/// `<MacTypes.h>` の `noErr`。
const NO_ERR: i32 = 0;

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
/// 返す。null ポインタ・サイズ不足はいずれも `None`(呼び出し側は書き込みをスキップし、
/// 音声スレッドを絶対にパニックさせない。§4.8 の思想)。
fn validated_sample_count(frames: u32, data_is_null: bool, data_byte_size: u32) -> Option<usize> {
    if data_is_null {
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
/// (`frames / sample_rate`)+ (2) `AudioUnit` 自身が申告する処理レイテンシ
/// (`kAudioUnitProperty_Latency` を `open()` 時に1度だけ読んだ `unit_latency_ns`)**
/// を足したものを予測出力時刻として扱う——cpal が「デバイスの実バッファ長を取れない
/// ときはこのコールバックのフレーム数へフォールバックする」のと同じ考え方
/// (`cpal_backend.rs::build_output_stream` のコメント参照)。
fn compute_timestamps(
    callback_host_time_ns: u64,
    unit_latency_ns: u64,
    frames: u32,
    sample_rate: u32,
) -> (u64, u64) {
    let buffer_duration_ns = if sample_rate > 0 {
        (frames as u64 * 1_000_000_000) / sample_rate as u64
    } else {
        0
    };
    let output_latency_ns = unit_latency_ns.saturating_add(buffer_duration_ns);
    let buffer_start_host_time_ns = callback_host_time_ns.saturating_add(output_latency_ns);
    (buffer_start_host_time_ns, output_latency_ns)
}

// ============================================================================
// オープン済みユニットへの問い合わせ(実デバイス必須。単体テスト対象外)
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

/// `kAudioUnitProperty_Latency`(秒)を読んで ns へ変換する。取得できなければ 0
/// (`compute_timestamps` はこれを「申告なし」として扱い、バッファ長ぶんの見積もりだけ
/// 使う)。**`AudioUnitInitialize` の後**に呼ぶこと(この値は初期化済みのユニットの
/// 処理レイテンシを表すプロパティ)。
fn query_unit_latency_ns(unit: AudioUnit) -> u64 {
    let mut latency_seconds: f64 = 0.0;
    let mut size = std::mem::size_of::<f64>() as u32;
    // SAFETY: `unit` は呼び出し元が初期化済みの有効なインスタンス。
    let status = unsafe {
        AudioUnitGetProperty(
            unit,
            AUDIO_UNIT_PROPERTY_LATENCY,
            AUDIO_UNIT_SCOPE_GLOBAL,
            0,
            &mut latency_seconds as *mut f64 as *mut c_void,
            &mut size,
        )
    };
    if status == NO_ERR && latency_seconds.is_finite() && latency_seconds >= 0.0 {
        (latency_seconds * 1_000_000_000.0) as u64
    } else {
        0
    }
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
    /// `open()` が `AudioUnitInitialize` の直後に1度だけ読んだ
    /// `kAudioUnitProperty_Latency`(ns)。[`compute_timestamps`] 参照。
    unit_latency_ns: u64,
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
        if io_data.number_buffers == 0 {
            return;
        }
        let buffer_data = io_data.buffers[0].data;
        let buffer_data_byte_size = io_data.buffers[0].data_byte_size;
        let Some(sample_count) = validated_sample_count(
            in_number_frames,
            buffer_data.is_null(),
            buffer_data_byte_size,
        ) else {
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

        let (buffer_start_host_time_ns, output_latency_ns) = compute_timestamps(
            callback_host_time_ns,
            context.unit_latency_ns,
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

        // 音声スレッド上で呼ぶのは `Renderer::render` のみに保つ(`mw-backend/CLAUDE.md`
        // の設計意図。上の数行は CoreAudio が渡した構造体を読んでアトミックストアする
        // だけで、mw-core の別関数を追加で呼んではいない)。
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

/// macOS の既定出力デバイスへ AudioUnit(AUHAL、`kAudioUnitSubType_DefaultOutput`)で
/// 直接出力する `Backend` 実装(AUDIOWARE-DEPS-PLAN.md ステップ4-1)。
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
        }
    }
}

impl Backend for AppleBackend {
    fn open(&mut self, renderer: Renderer, _events: Arc<EventQueue>) -> Result<(), BackendError> {
        if self.unit.is_some() {
            return Err(BackendError::AlreadyOpen);
        }

        let description = AudioComponentDescription {
            component_type: AUDIO_UNIT_TYPE_OUTPUT,
            component_sub_type: AUDIO_UNIT_SUBTYPE_DEFAULT_OUTPUT,
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

        let context = Box::new(CallbackContext {
            renderer,
            sample_rate: sample_rate as u32,
            // `AudioUnitInitialize` の後でないと意味のある値を返さないため、
            // いったん 0 にしておき、初期化が成功した直後に確定させる(下記)。
            unit_latency_ns: 0,
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

        let unit_latency_ns = query_unit_latency_ns(unit);
        // SAFETY: `context_ptr` はまだレンダースレッドから触られていない
        // (`AudioOutputUnitStart` を呼ぶ前なので、単独の書き手としてここで確定させる)。
        unsafe {
            (*context_ptr).unit_latency_ns = unit_latency_ns;
        }

        // SAFETY: `unit` は初期化済み。
        let status = unsafe { AudioOutputUnitStart(unit) };
        if status != NO_ERR {
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
             unit_latency={:.3} ms",
            sample_rate,
            CHANNELS,
            unit_latency_ns as f64 / 1_000_000.0,
        );

        self.unit = Some(unit);
        self.context_ptr = Some(context_ptr);
        self.sample_rate = sample_rate as u32;
        Ok(())
    }

    fn close(&mut self) -> Result<(), BackendError> {
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
            "[mw-backend] (native/apple) output latency (measured, buffer duration + AudioUnit \
             latency): {latency_ns} ns = {latency_ms:.3} ms"
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

    #[test]
    fn validated_sample_count_accepts_an_exactly_sized_buffer() {
        assert_eq!(
            validated_sample_count(512, false, 512 * 2 * 4),
            Some(512 * 2)
        );
    }

    #[test]
    fn validated_sample_count_rejects_a_null_buffer() {
        assert_eq!(validated_sample_count(512, true, 512 * 2 * 4), None);
    }

    #[test]
    fn validated_sample_count_rejects_an_undersized_buffer() {
        assert_eq!(validated_sample_count(512, false, 512 * 2 * 4 - 1), None);
    }

    #[test]
    fn validated_sample_count_accepts_zero_frames() {
        assert_eq!(validated_sample_count(0, false, 0), Some(0));
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
}
