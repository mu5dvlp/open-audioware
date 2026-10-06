//! Android 用の自前 AAudio バックエンド(AUDIOWARE-DEPS-PLAN.md ステップ4-3)。
//!
//! 4-1(macOS)・4-2(iOS/tvOS、[`crate::native_backend::apple`])と同じく**新しい
//! 外部クレートは追加していない**。`libaaudio.so`(API 26+)の C API を自前の
//! `extern "C"` 宣言で直接叩く——`ndk`/`ndk-sys` クレート(`cpal_backend` が
//! Android で使っている)は使わない。
//!
//! 手順は計画書の記述そのまま: `AAudio_createStreamBuilder` →
//! `AAudioStreamBuilder_setPerformanceMode`(`AAUDIO_PERFORMANCE_MODE_LOW_LATENCY`)/
//! `setDataCallback` / `setErrorCallback` → `AAudioStreamBuilder_openStream` →
//! `AAudioStream_requestStart`。`cpal` の Android 実装(`cpal/realtime` feature)が
//! 低遅延を得るためだけに performance mode を設定していた経緯
//! (`crates/mw-backend/CLAUDE.md` の「Android ビルド時の依存について」参照)が、
//! ここでは1行の `extern "C"` 呼び出しに置き換わる——`audio_thread_priority`
//! (MPL)も `jni`/`ndk-context`(`crate::android_context`)も、この実装のコード自体は
//! 使わない(`backend-cpal` feature を選ぶ限り cpal 版は引き続き両方使う。
//! どちらも 4-4 で `cpal` を消すまで残す)。
//!
//! # タイムスタンプの扱い
//!
//! AAudio は macOS/iOS の `AudioTimeStamp.mHostTime` のような「このコールバックの
//! 基準時刻」を直接は渡してこない。代わりに `AAudioStream_getTimestamp` で
//! 「フレーム位置 `anchor_frame` がホスト単調時刻 `anchor_time_ns` に出力される
//! (または出力された)」という対応点を問い合わせできる。[`project_frame_to_ns`]
//! がこれを線形に外挿し、今回のコールバックが書き込むバッファの先頭フレーム位置
//! (`AAudioStream_getFramesWritten` が返す、このコールバックが呼ばれた時点での
//! 既書き込みフレーム数)の予測出力時刻を求める——cpal 0.18.1 の AAudio 実装
//! (`cpal::host::aaudio::convert::{output_stream_instant, stream_instant_from_anchor}`、
//! `~/.cargo/registry/src/*/cpal-0.18.1/src/host/aaudio/convert.rs`)と**全く同じ式**。
//! 4-2(iOS)のモジュール doc が説明している「cpal の意味を意図的に変える」事情とは
//! 異なり、cpal の Android 実装はこの時点で既に「予測される DAC 出力時刻」を
//! 正しく計算しているため、ここでは式をそのまま持ち込んでいる——意味を変える
//! 必要が無い(4-4 で cpal を消した後も挙動を変えない、という引き継ぎとしても
//! 都合がよい)。
//!
//! `AAudioStream_getTimestamp` が未確定(ストリーム開始直後等)を返した場合は
//! このコールバックの呼び出し時刻そのもの(相関点)にフォールバックする——cpal が
//! `Err(_) => now_stream_instant()` にフォールバックするのと同じ扱い(出力レイテンシ
//! 0 = 予測不能、として扱う)。
//!
//! macOS/iOS の「補正項」(`device_extra_latency_ns`)のような、音声スレッドの外から
//! 非同期に書き換えられる持続的な状態は Android には無い——`AAudioStream_getTimestamp`
//! は毎コールバック内でその場で問い合わせる値であり、ルート変化のような外部要因は
//! 自然にこの問い合わせ結果へ反映される。そのため `MusicClockPublisher::
//! bump_generation` の呼び出しは不要(`cpal_backend::build_output_stream` も同じ理由で
//! 呼んでいない)。
//!
//! # cpal 版との既知の差分
//!
//! 切断(`AAUDIO_ERROR_DISCONNECTED`)の通知は実装している——`error_proc` が
//! `Event::StreamError { reason: DeviceUnavailable }` を積み、`mw-ffi` 側の既存の
//! 内部再オープン(`handle.rs::Instance::attempt_reopen`)がそれを受けて
//! close→open をやり直す(cpal 版の Android 切断経路と同じ仕組みに乗る。
//! `cpal_backend.rs` の `device_not_available_classifies_...` テスト doc参照)。
//! 一方、cpal 版が持つ xrun 発生時の動的バッファ長調整(`cpal::host::aaudio::mod.rs`
//! の `tune_dynamically` 分岐)は実装していない——`AAUDIO_PERFORMANCE_MODE_LOW_LATENCY`
//! が選ぶ既定のバッファで十分という前提(4-1/4-2 が macOS/iOS 側の自動チューニングに
//! 踏み込まなかったのと同じ判断)。出力コールバック自体の間隔異常
//! (「アンダーラン(の疑い)」)は cpal 版と同じ [`crate::underrun::OutputUnderrunTracker`]
//! で検知する。

use std::ffi::c_void;

use mw_core::{CHANNELS, StreamErrorReason};

#[cfg(target_os = "android")]
use std::panic::{self, AssertUnwindSafe};
#[cfg(target_os = "android")]
use std::sync::Arc;
#[cfg(target_os = "android")]
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

#[cfg(target_os = "android")]
use mw_core::{Event, EventQueue, Renderer};

#[cfg(target_os = "android")]
use crate::backend::{Backend, BackendError};
#[cfg(target_os = "android")]
use crate::host_time::host_time_ns;
#[cfg(target_os = "android")]
use crate::underrun::OutputUnderrunTracker;

// ============================================================================
// 純関数部分(ハードウェア無しで `cargo test`(ホスト)から固定化できる)
// ============================================================================

/// `<aaudio/AAudio.h>` の `AAUDIO_ERROR_DISCONNECTED`。[`classify_aaudio_error`]
/// (ホストでも実行される純関数)と、実機でのみ呼ばれる [`error_proc`] の両方から
/// 参照するため `target_os` を問わずコンパイルする。
///
/// `pub` にしてある理由は実装上の都合: Android 以外のホスト(macOS 等)では
/// 実際の呼び出し元([`error_proc`])が `#[cfg]` で存在しないため、非公開のままだと
/// `dead_code` が立つ(`native_backend::apple::seconds_to_ns` が同じ理由で `pub` に
/// してあるのと同じ事情。以下の [`project_frame_to_ns`]/[`validated_sample_count`]/
/// [`classify_aaudio_error`] も同様)。
pub const AAUDIO_ERROR_DISCONNECTED: i32 = -899;

/// `AAudioStream_getTimestamp` が返す基準点(フレーム位置 `anchor_frame` が
/// ホスト単調時刻 `anchor_time_ns` に出力される対応)から、別のフレーム位置
/// `target_frame` の予測出力時刻を線形に外挿する。
///
/// cpal 0.18.1 の AAudio 実装(`cpal::host::aaudio::convert::
/// stream_instant_from_anchor`)と同じ式(モジュール doc「タイムスタンプの扱い」
/// 参照)。`sample_rate == 0`(未確定)は除算を避けて `anchor_time_ns` をそのまま
/// 返す——呼び出し元([`render_proc`])はオープン時に確定した非0のサンプルレートしか
/// 渡さない想定だが、音声スレッドを絶対にパニックさせないための防御
/// (`crates/mw-core/CLAUDE.md` のリアルタイム安全性規約、§4.8 の思想)。計算結果が
/// 負、または `u64` の範囲を超える場合は両端にクランプする(cpal 側の `.max(0)` に
/// 加え、上限クランプも足してオーバーフローを防ぐ——`host_time::
/// ticks_to_ns_with_timebase` と同じ saturating の方針)。
pub fn project_frame_to_ns(
    anchor_frame: i64,
    anchor_time_ns: i64,
    target_frame: i64,
    sample_rate: u32,
) -> u64 {
    if sample_rate == 0 {
        return anchor_time_ns.max(0) as u64;
    }
    let offset_ns =
        (target_frame as i128 - anchor_frame as i128) * 1_000_000_000i128 / sample_rate as i128;
    (anchor_time_ns as i128 + offset_ns).clamp(0, u64::MAX as i128) as u64
}

/// レンダーコールバックが受け取った出力バッファへ安全に書き込める要素数(f32 の個数)を
/// 返す。null ポインタ・0 以下のフレーム数・非アラインポインタはいずれも `None`
/// (呼び出し側は書き込みをスキップし、音声スレッドを絶対にパニックさせない。§4.8 の
/// 思想。`native_backend::apple::validated_sample_count` と同じ考え方)。
///
/// **`apple::validated_sample_count` と異なりバイトサイズ引数を取らない**——AAudio の
/// データコールバック(`AAudioStream_dataCallback`)は渡すバッファのバイト数を一切
/// 教えてくれず、`numFrames * channelCount * sizeof(format)` 以上を確保済みであることが
/// API の契約(Android NDK `AAudio.h` の `AAudioStream_dataCallback` doc)。ここでは
/// その契約を信用しつつ、`numFrames` とポインタ自体の明らかな異常だけを機械的に弾く。
pub fn validated_sample_count(frames: i32, data: *mut c_void) -> Option<usize> {
    if frames <= 0 {
        return None;
    }
    if data.is_null() {
        return None;
    }
    if !(data as usize).is_multiple_of(std::mem::align_of::<f32>()) {
        return None;
    }
    (frames as usize).checked_mul(CHANNELS)
}

/// AAudio のエラーコールバックが渡す `aaudio_result_t` を `mw_core::
/// StreamErrorReason`(初期構築仕様『§4.6』)へ分類する。
///
/// `cpal_backend::classify_stream_error` と同じ考え方(C# 側が実用的に分岐できる
/// 粒度へ丸める)だが、分類元が cpal の `ErrorKind`(14種)ではなく AAudio が
/// エラーコールバックへ実際に渡しうる値に絞られる——Android NDK のドキュメントは
/// 「切断(`AAUDIO_ERROR_DISCONNECTED`)で呼ばれる」とだけ述べており、それ以外の値が
/// 実際に渡ることは想定されていない。`AAUDIO_ERROR_DISCONNECTED` だけを明示的に分類し、
/// 残りは `cpal_backend::classify_stream_error` の `_ =>` と同じ
/// `StreamErrorReason::Backend` へ丸める。
pub fn classify_aaudio_error(error: i32) -> StreamErrorReason {
    match error {
        AAUDIO_ERROR_DISCONNECTED => StreamErrorReason::DeviceUnavailable,
        _ => StreamErrorReason::Backend,
    }
}

// ============================================================================
// AAudio の FFI 宣言(自前。新しいクレートは追加しない。Android のみ)
// ============================================================================

/// `AAudioStreamBuilder`(`<aaudio/AAudio.h>` の不透明構造体)。
#[cfg(target_os = "android")]
#[repr(C)]
struct AAudioStreamBuilder {
    _private: [u8; 0],
}

/// `AAudioStream`(同上)。
#[cfg(target_os = "android")]
#[repr(C)]
struct AAudioStream {
    _private: [u8; 0],
}

/// `AAudioStream_dataCallback`(`<aaudio/AAudio.h>`)。戻り値は
/// `aaudio_data_callback_result_t`(`AAUDIO_CALLBACK_RESULT_CONTINUE`/`_STOP`)。
#[cfg(target_os = "android")]
type AAudioDataCallback =
    unsafe extern "C" fn(*mut AAudioStream, *mut c_void, *mut c_void, i32) -> i32;

/// `AAudioStream_errorCallback`(同上)。戻り値を持たない。
#[cfg(target_os = "android")]
type AAudioErrorCallback = unsafe extern "C" fn(*mut AAudioStream, *mut c_void, i32);

/// `<aaudio/AAudio.h>` の `AAUDIO_OK`。
#[cfg(target_os = "android")]
const AAUDIO_OK: i32 = 0;
/// `AAUDIO_DIRECTION_OUTPUT`。
#[cfg(target_os = "android")]
const AAUDIO_DIRECTION_OUTPUT: i32 = 0;
/// `AAUDIO_FORMAT_PCM_FLOAT`。
#[cfg(target_os = "android")]
const AAUDIO_FORMAT_PCM_FLOAT: i32 = 2;
/// `AAUDIO_PERFORMANCE_MODE_LOW_LATENCY`——cpal の `realtime` feature が Android で
/// 設定していたのと同じ値(モジュール doc参照)。
#[cfg(target_os = "android")]
const AAUDIO_PERFORMANCE_MODE_LOW_LATENCY: i32 = 12;
/// `AAUDIO_CALLBACK_RESULT_CONTINUE`。
#[cfg(target_os = "android")]
const AAUDIO_CALLBACK_RESULT_CONTINUE: i32 = 0;
/// `AAudioStream_getTimestamp` の `clockid_t` 引数(`CLOCK_MONOTONIC`)。
/// `host_time::host_time_ns` が Android で読んでいるのと同じ時計
/// (`host_time.rs` のモジュール doc参照)——同じ時計を指定することで、
/// [`project_frame_to_ns`] が返す予測時刻と `host_time_ns()` が直接比較可能になる。
#[cfg(target_os = "android")]
const CLOCK_MONOTONIC: i32 = 1;

#[cfg(target_os = "android")]
#[allow(non_snake_case)] // シンボル名は AAudio の実際の C API 名そのまま
#[link(name = "aaudio")]
unsafe extern "C" {
    fn AAudio_createStreamBuilder(builder: *mut *mut AAudioStreamBuilder) -> i32;
    fn AAudioStreamBuilder_setDirection(builder: *mut AAudioStreamBuilder, direction: i32);
    fn AAudioStreamBuilder_setFormat(builder: *mut AAudioStreamBuilder, format: i32);
    fn AAudioStreamBuilder_setChannelCount(builder: *mut AAudioStreamBuilder, channel_count: i32);
    fn AAudioStreamBuilder_setSampleRate(builder: *mut AAudioStreamBuilder, sample_rate: i32);
    fn AAudioStreamBuilder_setPerformanceMode(builder: *mut AAudioStreamBuilder, mode: i32);
    fn AAudioStreamBuilder_setDataCallback(
        builder: *mut AAudioStreamBuilder,
        callback: AAudioDataCallback,
        user_data: *mut c_void,
    );
    fn AAudioStreamBuilder_setErrorCallback(
        builder: *mut AAudioStreamBuilder,
        callback: AAudioErrorCallback,
        user_data: *mut c_void,
    );
    fn AAudioStreamBuilder_openStream(
        builder: *mut AAudioStreamBuilder,
        stream: *mut *mut AAudioStream,
    ) -> i32;
    fn AAudioStreamBuilder_delete(builder: *mut AAudioStreamBuilder) -> i32;

    fn AAudioStream_requestStart(stream: *mut AAudioStream) -> i32;
    fn AAudioStream_requestStop(stream: *mut AAudioStream) -> i32;
    fn AAudioStream_close(stream: *mut AAudioStream) -> i32;
    fn AAudioStream_getSampleRate(stream: *mut AAudioStream) -> i32;
    fn AAudioStream_getFramesWritten(stream: *mut AAudioStream) -> i64;
    fn AAudioStream_getTimestamp(
        stream: *mut AAudioStream,
        clockid: i32,
        frame_position: *mut i64,
        time_nanoseconds: *mut i64,
    ) -> i32;
}

/// デバイスが見つからない等でハードウェアのレートを読めなかったときのフォールバック
/// (`native_backend::apple::FALLBACK_SAMPLE_RATE_HZ` と同じ役割)。
#[cfg(target_os = "android")]
const FALLBACK_SAMPLE_RATE_HZ: u32 = 48_000;

// ============================================================================
// レンダーコールバック(音声スレッド)/ エラーコールバック(別スレッド)
// ============================================================================

/// レンダーコールバック(音声スレッド)が排他的に触る状態。[`AndroidBackend::open`] が
/// `Box::into_raw` でリーク相当にしたポインタを `AAudioStreamBuilder_setDataCallback`
/// の `user_data` として渡し、[`AndroidBackend::close`] が `Box::from_raw` で回収する
/// (`native_backend::apple::CallbackContext` と同じ「単一の書き手」設計、§5.3)。
#[cfg(target_os = "android")]
struct CallbackContext {
    renderer: Renderer,
    /// オープン後に確定したサンプルレート(オープン中は変わらない)。
    sample_rate: u32,
    underrun_tracker: OutputUnderrunTracker,
    /// [`AndroidBackend::callback_frames`] へ渡す `Arc`。
    callback_frames: Arc<AtomicU32>,
    /// [`AndroidBackend::output_latency_ns`] へ渡す `Arc`。
    output_latency_ns: Arc<AtomicU64>,
    /// [`AndroidBackend::close`] がストリーム停止後に `Acquire` で読むための同期点
    /// (`cpal_backend::CpalBackend::render_completions` と同じ理由)。
    render_completions: Arc<AtomicU64>,
}

/// エラーコールバック(音声コールバックとは別スレッド、モジュール doc参照)が読む状態。
/// `CallbackContext` とは別の `Box` として持つ——音声スレッドが `&mut CallbackContext`
/// を排他的に握っている間、別スレッドから同じメモリへ `&ErrorContext` を作ると
/// 借用規約に反するため(`cpal_backend::build_output_stream` の `err_fn` が
/// `renderer` を一切捕まえず `events` だけを独立に持つのと同じ理由)。
#[cfg(target_os = "android")]
struct ErrorContext {
    events: Arc<EventQueue>,
}

/// AAudio の音声スレッドから直接呼ばれる `AAudioStream_dataCallback`。
///
/// # SAFETY(呼び出し元が守る契約)
///
/// - `user_data` は [`AndroidBackend::open`] が `Box::into_raw::<CallbackContext>` で
///   渡したポインタのまま、[`AndroidBackend::close`] が回収するまで有効——AAudio は
///   `AAudioStream_close` が返った後にはこのコールバックを呼ばないことが前提
///   (`AndroidBackend::close` のドキュメント参照)。
/// - `audio_data` はこの呼び出しの間だけ有効な、AAudio 所有のバッファ。
///
/// パニックは FFI 境界の外へ絶対に漏らさない(`native_backend::apple::render_proc` と
/// 同じ方針)。`Renderer::render` 自体はパニックしない契約(§5.3)だが、契約が破られた
/// 場合の最後の防波堤として `catch_unwind` で包む。
#[cfg(target_os = "android")]
unsafe extern "C" fn render_proc(
    stream: *mut AAudioStream,
    user_data: *mut c_void,
    audio_data: *mut c_void,
    num_frames: i32,
) -> i32 {
    let caught = panic::catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: 呼び出し元の契約(関数 doc)により有効なポインタ。
        let context = unsafe { &mut *(user_data as *mut CallbackContext) };

        let Some(sample_count) = validated_sample_count(num_frames, audio_data) else {
            return;
        };
        // SAFETY: `validated_sample_count` が null・非アライン・0以下のフレーム数を
        // 弾いている。バッファの実サイズは AAudio の API 契約
        // (`validated_sample_count` のドキュメント参照)。このコールバックの実行中
        // のみ有効。
        let output =
            unsafe { std::slice::from_raw_parts_mut(audio_data as *mut f32, sample_count) };

        context
            .callback_frames
            .store(num_frames as u32, Ordering::Relaxed);

        // 相関点(このコールバックが呼ばれたホスト単調時刻)。
        let callback_host_time_ns = host_time_ns();

        let mut anchor_frame: i64 = 0;
        let mut anchor_time_ns: i64 = 0;
        // SAFETY: `stream` は AAudio が渡す有効なポインタ(関数 doc の契約)。
        // `anchor_frame`/`anchor_time_ns` はスタック上の有効な書き込み先。
        let status = unsafe {
            AAudioStream_getTimestamp(
                stream,
                CLOCK_MONOTONIC,
                &mut anchor_frame,
                &mut anchor_time_ns,
            )
        };
        let buffer_start_host_time_ns = if status == AAUDIO_OK {
            // SAFETY: 同上。
            let app_frame = unsafe { AAudioStream_getFramesWritten(stream) };
            project_frame_to_ns(anchor_frame, anchor_time_ns, app_frame, context.sample_rate)
        } else {
            // タイムスタンプが未確定(ストリーム開始直後等)。cpal 0.18.1 の
            // `output_stream_instant` が `Err(_) => now_stream_instant()` に
            // フォールバックするのと同じ扱い(モジュール doc参照)。
            callback_host_time_ns
        };

        context.output_latency_ns.store(
            buffer_start_host_time_ns.saturating_sub(callback_host_time_ns),
            Ordering::Relaxed,
        );

        context.underrun_tracker.observe(
            callback_host_time_ns,
            num_frames as u32,
            context.sample_rate,
        );

        // 音声スレッド上で呼ぶ mw-core 側の経路は `Renderer::render` のみに保つ
        // (`mw-backend/CLAUDE.md` の設計意図)。
        context.renderer.render(output, buffer_start_host_time_ns);

        context.render_completions.fetch_add(1, Ordering::Release);
    }));

    if caught.is_err() {
        // 契約が破られた場合の最後の防波堤(`native_backend::apple::render_proc` と
        // 同じ方針)。バッファへは書き込まない(無音ではなく直前の内容が残る可能性が
        // あるが、クラッシュさせないことを優先する)。
    }
    AAUDIO_CALLBACK_RESULT_CONTINUE
}

/// AAudio のエラー通知スレッド(音声コールバックとは別スレッド、モジュール doc参照)
/// から呼ばれる `AAudioStream_errorCallback`。
///
/// # SAFETY
///
/// `user_data` は [`AndroidBackend::open`] が `Box::into_raw::<ErrorContext>` で渡した
/// ポインタのまま、[`AndroidBackend::close`] が回収するまで有効。
#[cfg(target_os = "android")]
unsafe extern "C" fn error_proc(_stream: *mut AAudioStream, user_data: *mut c_void, error: i32) {
    let caught = panic::catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: 呼び出し元の契約(関数 doc)により有効なポインタ。
        let context = unsafe { &*(user_data as *const ErrorContext) };
        crate::mw_log!("[mw-backend] (native/android) output stream error: {error}");
        // 音声コールバックそのものとは別スレッドから呼ばれる(関数 doc参照)ので、
        // `EventQueue` の非リアルタイム経路(`push_side_channel`、内部で `Mutex` を
        // 使う)へ積んでよい(`cpal_backend::build_output_stream` の `err_fn` と同じ
        // 設計)。
        context.events.push_side_channel(Event::StreamError {
            reason: classify_aaudio_error(error),
        });
    }));
    if caught.is_err() {
        crate::mw_log!(
            "[mw-backend] (native/android) panic while handling the AAudio error callback \
             (caught at the boundary)"
        );
    }
}

// ============================================================================
// Backend 実装(Android のみ)
// ============================================================================

/// Android の既定出力へ AAudio で直接出力する `Backend` 実装
/// (AUDIOWARE-DEPS-PLAN.md ステップ4-3)。
#[cfg(target_os = "android")]
pub struct AndroidBackend {
    stream: Option<*mut AAudioStream>,
    context_ptr: Option<*mut CallbackContext>,
    error_context_ptr: Option<*mut ErrorContext>,
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

// SAFETY: `stream`/`context_ptr`/`error_context_ptr` は生ポインタだが、
// `mw-ffi::handle::Instance` がグローバルレジストリの `Mutex` 経由で単一所有を
// 保証するため、`AndroidBackend` の `&mut self` メソッドが複数スレッドから同時に
// 呼ばれることは無い(`native_backend::apple::AppleBackend` と同じ理由)。`Sync` は
// 要らない。
#[cfg(target_os = "android")]
unsafe impl Send for AndroidBackend {}

#[cfg(target_os = "android")]
impl Default for AndroidBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_os = "android")]
impl AndroidBackend {
    pub fn new() -> Self {
        Self {
            stream: None,
            context_ptr: None,
            error_context_ptr: None,
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

#[cfg(target_os = "android")]
impl Backend for AndroidBackend {
    fn open(&mut self, renderer: Renderer, events: Arc<EventQueue>) -> Result<(), BackendError> {
        if self.stream.is_some() {
            return Err(BackendError::AlreadyOpen);
        }

        let mut builder: *mut AAudioStreamBuilder = std::ptr::null_mut();
        // SAFETY: `&mut builder` はスタック上の有効な出力先。
        let status = unsafe { AAudio_createStreamBuilder(&mut builder) };
        if status != AAUDIO_OK || builder.is_null() {
            return Err(BackendError::NoOutputDevice);
        }

        // SAFETY: `builder` は直前に取得した有効なハンドル。設定順は任意
        // (`AAudioStreamBuilder_openStream` を呼ぶまで反映されない)。
        unsafe {
            AAudioStreamBuilder_setDirection(builder, AAUDIO_DIRECTION_OUTPUT);
            AAudioStreamBuilder_setFormat(builder, AAUDIO_FORMAT_PCM_FLOAT);
            AAudioStreamBuilder_setChannelCount(builder, CHANNELS as i32);
            // サンプルレートは指定しない(`AAUDIO_UNSPECIFIED` = 0)——ハードウェアの
            // 既定値をそのまま使う(`native_backend::apple::query_output_sample_rate`
            // がハードウェア側のレートに合わせるのと同じ狙い)。実際に採用された値は
            // オープン後に `AAudioStream_getSampleRate` で読み直す(下記)。
            AAudioStreamBuilder_setSampleRate(builder, 0);
            AAudioStreamBuilder_setPerformanceMode(builder, AAUDIO_PERFORMANCE_MODE_LOW_LATENCY);
        }

        let context = Box::new(CallbackContext {
            renderer,
            // 実際の値は `AAudioStreamBuilder_openStream` が成功した後にしか分からない
            // (下記)。いったん 0 で置き、確定した直後に `context_ptr` 経由で書き直す。
            sample_rate: 0,
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

        let error_context = Box::new(ErrorContext { events });
        let error_context_ptr = Box::into_raw(error_context);

        // SAFETY: `builder` はまだ `openStream` していない。`context_ptr`/
        // `error_context_ptr` は直前に `Box::into_raw` したばかりで、AAudio はまだ
        // これらを知らない。
        unsafe {
            AAudioStreamBuilder_setDataCallback(builder, render_proc, context_ptr as *mut c_void);
            AAudioStreamBuilder_setErrorCallback(
                builder,
                error_proc,
                error_context_ptr as *mut c_void,
            );
        }

        let mut stream: *mut AAudioStream = std::ptr::null_mut();
        // SAFETY: `builder`/`&mut stream` は有効。
        let status = unsafe { AAudioStreamBuilder_openStream(builder, &mut stream) };
        // builder はストリームとは別の生存期間を持つので、成功・失敗を問わず必ず
        // delete する。
        // SAFETY: `builder` は `AAudio_createStreamBuilder` が返した有効なハンドルで、
        // まだ delete していない。
        unsafe {
            AAudioStreamBuilder_delete(builder);
        }
        if status != AAUDIO_OK || stream.is_null() {
            // SAFETY: `context_ptr`/`error_context_ptr` はまだ誰にも(AAudio にも)
            // 参照されていない(`openStream` が失敗したので `setDataCallback`/
            // `setErrorCallback` で渡した先は使われないまま)。
            drop(unsafe { Box::from_raw(context_ptr) });
            drop(unsafe { Box::from_raw(error_context_ptr) });
            return Err(BackendError::BuildStreamFailed(format!(
                "AAudioStreamBuilder_openStream failed: {status}"
            )));
        }

        // SAFETY: `stream` は直前に `openStream` が返した有効なハンドル。
        let negotiated_rate = unsafe { AAudioStream_getSampleRate(stream) };
        let sample_rate = if negotiated_rate > 0 {
            negotiated_rate as u32
        } else {
            FALLBACK_SAMPLE_RATE_HZ
        };

        // `AAudioStream_requestStart` を呼ぶ前なので、レンダーコールバックは一度も
        // 起動していない——`context_ptr` はまだこの関数が排他的に所有している
        // (`native_backend::apple::AppleBackend::open` の同種の書き込みと同じ前提)。
        // ランプのミリ秒→サンプル数換算(初期構築仕様 §4.1)が正しいサンプルレートを
        // 使えるよう、コールバックが動き出す前に確定させる。
        // SAFETY: 上記の理由により、他のどのスレッドもまだ `context_ptr` を読まない。
        unsafe {
            (*context_ptr).sample_rate = sample_rate;
            (*context_ptr).renderer.set_sample_rate(sample_rate);
        }

        // SAFETY: `stream` は有効なハンドル。
        let status = unsafe { AAudioStream_requestStart(stream) };
        if status != AAUDIO_OK {
            // SAFETY: `requestStart` が失敗したので、レンダーコールバックは一度も
            // 呼ばれていない。`close` を呼んでからリソースを回収する。
            unsafe {
                AAudioStream_close(stream);
            }
            drop(unsafe { Box::from_raw(context_ptr) });
            drop(unsafe { Box::from_raw(error_context_ptr) });
            return Err(BackendError::PlayStreamFailed(format!(
                "AAudioStream_requestStart failed: {status}"
            )));
        }

        crate::mw_log!(
            "[mw-backend] (native/android) output stream started: sample_rate={} Hz, \
             channels={}",
            sample_rate,
            CHANNELS,
        );

        self.stream = Some(stream);
        self.context_ptr = Some(context_ptr);
        self.error_context_ptr = Some(error_context_ptr);
        self.sample_rate = sample_rate;
        Ok(())
    }

    fn close(&mut self) -> Result<(), BackendError> {
        match (
            self.stream.take(),
            self.context_ptr.take(),
            self.error_context_ptr.take(),
        ) {
            (Some(stream), Some(context_ptr), Some(error_context_ptr)) => {
                // SAFETY: `stream` はこの `AndroidBackend` だけが所有しており
                // (`take()` で既に自分からも外した)、`open()` が成功させたまま一度も
                // close していない有効なハンドル。`requestStop` はベストエフォート
                // (`cpal_backend::CpalBackend::close` の `pause()` と同じ位置づけ)。
                unsafe {
                    AAudioStream_requestStop(stream);
                    AAudioStream_close(stream);
                }

                // `AAudioStream_close` が実際にコールバックスレッドの完了を同期的に
                // 待つかはドキュメントに明記が無いため、`native_backend::apple::
                // AppleBackend::close` / `cpal_backend::CpalBackend::close` と同じ
                // 保険を入れる——直近コールバックが `Release` で残した書き込みを
                // この `Acquire` で明示的に観測してから、直後の `Box::from_raw` の
                // drop がそれを読む前に happens-before の辺を明示しておく。
                // SAFETY: `context_ptr` が指すメモリは `open()` 以来有効で、上の
                // `close` 後もまだ `Box::from_raw` で回収していない。
                let _ = unsafe { (*context_ptr).render_completions.load(Ordering::Acquire) };
                // SAFETY: `open()` で `Box::into_raw` したポインタをそのまま回収する。
                // レンダースレッドは上の `AAudioStream_close` より後には呼ばれない。
                drop(unsafe { Box::from_raw(context_ptr) });
                drop(unsafe { Box::from_raw(error_context_ptr) });

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
        self.stream.is_some()
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
            "[mw-backend] (native/android) output latency (measured, \
             AAudioStream_getTimestamp projection - callback timestamp): {latency_ns} ns = \
             {latency_ms:.3} ms"
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
            "[mw-backend] (native/android) output underrun suspected: +{new_count} since \
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

    #[test]
    fn project_frame_to_ns_extrapolates_forward_from_the_anchor() {
        // 48kHz で基準点より 48 フレーム先(= 1ms 先)を予測する。
        let result = project_frame_to_ns(1_000, 1_000_000_000, 1_048, 48_000);
        assert_eq!(result, 1_001_000_000);
    }

    #[test]
    fn project_frame_to_ns_extrapolates_backward_from_the_anchor() {
        let result = project_frame_to_ns(1_048, 1_001_000_000, 1_000, 48_000);
        assert_eq!(result, 1_000_000_000);
    }

    #[test]
    fn project_frame_to_ns_clamps_to_zero_instead_of_going_negative() {
        // anchor_time_ns が小さいのに、大きく手前のフレームを予測すると負になる。
        let result = project_frame_to_ns(1_000_000, 500, 0, 48_000);
        assert_eq!(result, 0);
    }

    #[test]
    fn project_frame_to_ns_falls_back_to_the_anchor_time_when_sample_rate_is_zero() {
        let result = project_frame_to_ns(1_000, 123_456, 2_000, 0);
        assert_eq!(result, 123_456);
    }

    #[test]
    fn project_frame_to_ns_saturates_instead_of_overflowing() {
        let result = project_frame_to_ns(0, 0, i64::MAX, 1);
        assert_eq!(result, u64::MAX);
    }

    #[test]
    fn validated_sample_count_accepts_a_positive_frame_count_with_an_aligned_pointer() {
        let mut buf = [0.0f32; 8];
        assert_eq!(
            validated_sample_count(4, buf.as_mut_ptr().cast()),
            Some(4 * CHANNELS)
        );
    }

    #[test]
    fn validated_sample_count_rejects_a_zero_frame_count() {
        let mut buf = [0.0f32; 8];
        assert_eq!(validated_sample_count(0, buf.as_mut_ptr().cast()), None);
    }

    #[test]
    fn validated_sample_count_rejects_a_negative_frame_count() {
        let mut buf = [0.0f32; 8];
        assert_eq!(validated_sample_count(-1, buf.as_mut_ptr().cast()), None);
    }

    #[test]
    fn validated_sample_count_rejects_a_null_buffer() {
        assert_eq!(validated_sample_count(4, std::ptr::null_mut()), None);
    }

    #[test]
    fn validated_sample_count_rejects_a_misaligned_pointer() {
        let mut buf = [0.0f32; 8];
        // f32 の4byte境界からあえて1byteずらす(`buf` は f32 配列なので必ず4byte境界)。
        let misaligned = unsafe { (buf.as_mut_ptr() as *mut u8).add(1) };
        assert_eq!(validated_sample_count(4, misaligned.cast()), None);
    }

    #[test]
    fn classify_aaudio_error_maps_disconnected_to_device_unavailable() {
        assert_eq!(
            classify_aaudio_error(AAUDIO_ERROR_DISCONNECTED),
            StreamErrorReason::DeviceUnavailable
        );
    }

    #[test]
    fn classify_aaudio_error_maps_unmatched_codes_to_backend() {
        assert_eq!(classify_aaudio_error(-885), StreamErrorReason::Backend);
    }
}
