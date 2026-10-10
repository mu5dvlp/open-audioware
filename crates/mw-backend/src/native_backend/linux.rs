//! Linux 用の自前 ALSA バックエンド(AUDIOWARE-DEPS-PLAN.md ステップ4-4 の前提)。
//!
//! 4-1〜4-3([`crate::native_backend::apple`] / [`crate::native_backend::android`])と同じく
//! **新しい外部クレートは追加していない**。`libasound`(alsa-lib)の C API を自前の
//! `extern "C"` 宣言で直接叩き、`#[link(name = "asound")]` でリンクする
//! (`alsa` / `alsa-sys` クレートは使わない)。実行時には `libasound.so.2` が要る。
//!
//! # 方式: 専用スレッドで `snd_pcm_writei` を回す
//!
//! ALSA にはコールバック駆動の出力 API が無い(非同期ハンドラは signal 経由で使いづらい)
//! ので、cpal の ALSA ホストと同じく**専用スレッド**が「`Renderer::render` → ブロッキングの
//! `snd_pcm_writei`」を回す。デバイスのバッファが一杯のあいだ `writei` がブロックする
//! ことがそのままペース配分になる。手順は `snd_pcm_open("default", PLAYBACK)` →
//! `snd_pcm_set_params`(FLOAT_LE・RW_INTERLEAVED・2ch・48kHz・`soft_resample=1`・
//! 低遅延)→ `snd_pcm_get_params`(実際のバッファ/ピリオド長)→ スレッド起動。
//! `snd_pcm_set_params` は要求したレートと実際のレートが一致しないと失敗する
//! (`soft_resample=1` なら `plug` が変換するため、実際には 48kHz が通る)ので、
//! 採用されたレートは要求値そのものになる。
//!
//! # タイムスタンプ
//!
//! 今回書くバッファの先頭フレームが DAC から出る時刻は、
//! 「いま」+「書き込み済みでまだ再生されていないフレーム数(`snd_pcm_delay`)」
//! のぶん先(= [`playback_start_ns`])。「いま」は他の OS と同じ
//! [`host_time_ns`](`CLOCK_MONOTONIC`)なので、`mw_host_time_ns()` と直接比較できる。
//! 出力レイテンシ(`output_latency_ns`)はこの2つの差、つまり `snd_pcm_delay` を ns に
//! 直したもの(Android の `playback - callback` と同じ意味)。ストリームがまだ走り出して
//! いない間(`PREPARED` で `snd_pcm_delay` が値を返さない)は、書き込み済みのフレーム数を
//! 遅延の見積もりとする。
//!
//! # アンダーラン
//!
//! xrun(`-EPIPE`)・サスペンド(`-ESTRPIPE`)は `snd_pcm_recover` で復旧し、
//! [`OutputUnderrunTracker::record_reported_underrun`] で1回として数える。それとは別に
//! 他のバックエンドと同じ「ループの間隔が想定より開いた」検知(`observe`)も毎回回す。
//!
//! # 時計を持たないデバイス(`null` 等)
//!
//! `type null` の PCM は `snd_pcm_writei` が待たずに返り続けるため、そのままでは
//! スレッドが CPU を空回りし、`Renderer` が実時間の何千倍も速く進んでしまう。
//! 実デバイスではこの制限は発動しない([`throttle_sleep_ns`])が、**書いたフレーム数が
//! 実時間 + デバイスのバッファ長を超えて先行したらその分だけ眠る**ようにしてある。
//!
//! # 既知の差分
//!
//! - 音声スレッドの優先度は上げない(リアルタイムスケジューリングには rtkit 等の
//!   権限・追加の依存が要る)。cpal の `realtime` feature 相当は持たない。
//! - デバイスの抜き差し・既定出力の変更の監視は無い。書き込みが致命的に失敗したときだけ
//!   `Event::StreamError` を積んでスレッドを終える(`mw-ffi` の内部再オープンに乗る)。
//!
//! 純粋ロジック(タイムスタンプ計算・スロットル・エラー分類・ピリオド長の検証)は
//! `target_os` を問わずコンパイルし、ホストの `cargo test` で固定している
//! ([`crate::native_backend::android`] と同じ流儀)。

use mw_core::StreamErrorReason;

#[cfg(target_os = "linux")]
use std::ffi::{CStr, c_char, c_int, c_uint, c_void};
#[cfg(target_os = "linux")]
use std::panic::{self, AssertUnwindSafe};
#[cfg(target_os = "linux")]
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
#[cfg(target_os = "linux")]
use std::thread::JoinHandle;
#[cfg(target_os = "linux")]
use std::time::Duration;

#[cfg(target_os = "linux")]
use mw_core::{CHANNELS, Event, EventQueue, Renderer};

#[cfg(target_os = "linux")]
use crate::backend::{Backend, BackendError};
#[cfg(target_os = "linux")]
use crate::host_time::host_time_ns;
#[cfg(target_os = "linux")]
use crate::underrun::OutputUnderrunTracker;

// ============================================================================
// 純関数部分(ハードウェア無しで `cargo test`(ホスト)から固定化できる)
// ============================================================================
//
// 以下の `pub` はモジュール外へ公開する意図ではなく、Linux 以外では呼び出し元が
// `#[cfg]` で存在せず `dead_code` になるのを避けるため
// (`native_backend::android::project_frame_to_ns` と同じ事情)。

/// 目標サンプルレート。`snd_pcm_set_params` の `soft_resample=1` により、
/// デバイスのネイティブレートが違っても ALSA の `plug` が変換する。
pub const TARGET_SAMPLE_RATE_HZ: u32 = 48_000;

/// `snd_pcm_set_params` へ渡す総レイテンシ(µs)。【仮】他のバックエンド
/// (iOS の I/O バッファ長・Android の LowLatency)と同程度の低遅延に寄せた値で、
/// 実際のバッファ/ピリオド長はこれを元に ALSA が決め、`snd_pcm_get_params` で読み直す。
/// アンダーランが多発する環境ではここを大きくする(書き換えるのはこの1箇所)。
pub const TARGET_LATENCY_US: u32 = 20_000;

/// `get_params` が 0 や異常値を返したときの 1 回あたりのフレーム数のフォールバック
/// (10ms 相当)の分母。
const FALLBACK_PERIOD_DIVISOR: u32 = 100;

/// 1 回の `render` + `writei` で扱うフレーム数の上限。バッファをスレッド開始前に
/// 確保するので、デバイスが異常に大きいピリオドを申告しても確保量を抑える。
pub const MAX_PERIOD_FRAMES: usize = 16_384;

/// errno(Linux の値。ALSA は `-errno` を返す)。
pub const EPERM: i32 = 1;
pub const ENOENT: i32 = 2;
pub const EINTR: i32 = 4;
pub const EAGAIN: i32 = 11;
pub const EACCES: i32 = 13;
pub const EBUSY: i32 = 16;
pub const ENODEV: i32 = 19;
pub const EPIPE: i32 = 32;
pub const ESTRPIPE: i32 = 86;

/// 今回書くバッファの先頭フレームが出力される(と予測される)ホスト単調時刻(ns)。
///
/// `callback_ns` は問い合わせ時の [`host_time_ns`]、`delay_frames` は `snd_pcm_delay` が返す
/// 「書き込み済みで未再生のフレーム数」。負(xrun 中など)は 0 とみなす。
/// 計算結果は `u64` へ飽和させる。`sample_rate == 0` は除算を避けて `callback_ns`。
pub fn playback_start_ns(callback_ns: u64, delay_frames: i64, sample_rate: u32) -> u64 {
    callback_ns.saturating_add(delay_ns(delay_frames, sample_rate))
}

/// `snd_pcm_delay` のフレーム数を ns へ(出力レイテンシ。負は 0)。
pub fn delay_ns(delay_frames: i64, sample_rate: u32) -> u64 {
    if sample_rate == 0 || delay_frames <= 0 {
        return 0;
    }
    let ns = delay_frames as u128 * 1_000_000_000 / sample_rate as u128;
    ns.min(u64::MAX as u128) as u64
}

/// 時計を持たないデバイスで `writei` が即座に返り続けるのを抑える睡眠時間(ns)。
///
/// `frames_written` は基準時刻から書いた総フレーム数、`elapsed_ns` は基準時刻からの
/// 経過、`buffer_frames` はデバイスのバッファ長。実時間で消費された量に
/// バッファ長を足したぶんまでは先行して書いてよい(= 実デバイスで `writei` がブロック
/// せずに済む範囲)。それを超えた分が消費されるまでの時間を返す。超えていなければ 0。
pub fn throttle_sleep_ns(
    frames_written: u64,
    elapsed_ns: u64,
    sample_rate: u32,
    buffer_frames: u64,
) -> u64 {
    if sample_rate == 0 {
        return 0;
    }
    let consumed = elapsed_ns as u128 * sample_rate as u128 / 1_000_000_000;
    let allowed = consumed + buffer_frames as u128;
    let written = frames_written as u128;
    if written <= allowed {
        return 0;
    }
    let excess = written - allowed;
    (excess * 1_000_000_000 / sample_rate as u128).min(u64::MAX as u128) as u64
}

/// `snd_pcm_get_params` が返したピリオド長を、1 回の処理フレーム数へ検証する。
/// 0 は(`sample_rate / 100` = 10ms にフォールバック)、`MAX_PERIOD_FRAMES` 超は切り詰める。
pub fn sanitize_period_frames(period: u64, sample_rate: u32) -> usize {
    let frames = if period == 0 {
        (sample_rate / FALLBACK_PERIOD_DIVISOR).max(1) as u64
    } else {
        period
    };
    frames.min(MAX_PERIOD_FRAMES as u64) as usize
}

/// `snd_pcm_open` / `snd_pcm_set_params` の失敗(`-errno`)を `BackendError` へ分類する。
/// `detail` は `snd_strerror` の文言。デバイスが無い(`ENOENT`/`ENODEV`)は
/// `NoOutputDevice`、それ以外は `BuildStreamFailed`。
pub fn classify_open_error(error: i32, detail: &str) -> crate::backend::BackendError {
    match -error {
        ENOENT | ENODEV => crate::backend::BackendError::NoOutputDevice,
        _ => crate::backend::BackendError::BuildStreamFailed(format!(
            "ALSA open/configure failed: {detail} ({error})"
        )),
    }
}

/// `snd_pcm_writei` が負で返したとき、スレッドが取るべき行動。
#[derive(Debug, PartialEq, Eq)]
pub enum WriteErrorAction {
    /// `snd_pcm_recover` で復旧を試み、`true` ならアンダーランとして数える。
    Recover { underrun: bool },
    /// 少し待って同じデータを書き直す。
    Retry,
    /// 復旧不能。イベントを積んでスレッドを終える。
    Fatal(StreamErrorReason),
}

/// `writei` / `recover` の失敗(`-errno`)を行動へ分類する。
pub fn classify_write_error(error: i32) -> WriteErrorAction {
    match -error {
        EPIPE | ESTRPIPE => WriteErrorAction::Recover { underrun: true },
        EINTR => WriteErrorAction::Recover { underrun: false },
        EAGAIN => WriteErrorAction::Retry,
        ENODEV => WriteErrorAction::Fatal(StreamErrorReason::DeviceUnavailable),
        EACCES | EPERM => WriteErrorAction::Fatal(StreamErrorReason::PermissionDenied),
        _ => WriteErrorAction::Fatal(StreamErrorReason::Backend),
    }
}

// ============================================================================
// ALSA の FFI 宣言(自前。新しいクレートは追加しない。Linux のみ)
// ============================================================================

/// `snd_pcm_t`(不透明)。
#[cfg(target_os = "linux")]
#[repr(C)]
struct SndPcm {
    _private: [u8; 0],
}

#[cfg(target_os = "linux")]
const SND_PCM_STREAM_PLAYBACK: c_int = 0;
#[cfg(target_os = "linux")]
const SND_PCM_FORMAT_FLOAT_LE: c_int = 14;
#[cfg(target_os = "linux")]
const SND_PCM_ACCESS_RW_INTERLEAVED: c_int = 3;

/// `snd_pcm_uframes_t`(`unsigned long`)/ `snd_pcm_sframes_t`(`long`)。
#[cfg(target_os = "linux")]
type SndPcmUframes = std::ffi::c_ulong;
#[cfg(target_os = "linux")]
type SndPcmSframes = std::ffi::c_long;

#[cfg(target_os = "linux")]
#[link(name = "asound")]
unsafe extern "C" {
    fn snd_pcm_open(
        pcm: *mut *mut SndPcm,
        name: *const c_char,
        stream: c_int,
        mode: c_int,
    ) -> c_int;
    fn snd_pcm_close(pcm: *mut SndPcm) -> c_int;
    fn snd_pcm_drop(pcm: *mut SndPcm) -> c_int;
    fn snd_pcm_set_params(
        pcm: *mut SndPcm,
        format: c_int,
        access: c_int,
        channels: c_uint,
        rate: c_uint,
        soft_resample: c_int,
        latency_us: c_uint,
    ) -> c_int;
    fn snd_pcm_get_params(
        pcm: *mut SndPcm,
        buffer_size: *mut SndPcmUframes,
        period_size: *mut SndPcmUframes,
    ) -> c_int;
    fn snd_pcm_writei(
        pcm: *mut SndPcm,
        buffer: *const c_void,
        size: SndPcmUframes,
    ) -> SndPcmSframes;
    fn snd_pcm_delay(pcm: *mut SndPcm, delay: *mut SndPcmSframes) -> c_int;
    fn snd_pcm_recover(pcm: *mut SndPcm, err: c_int, silent: c_int) -> c_int;
    fn snd_strerror(errnum: c_int) -> *const c_char;
}

/// `snd_strerror` の文言(静的文字列)。ゲームスレッド/エラー経路専用(アロケーションする)。
#[cfg(target_os = "linux")]
fn strerror(error: c_int) -> String {
    // SAFETY: `snd_strerror` は常に有効な NUL 終端の静的文字列を返す。
    unsafe {
        let ptr = snd_strerror(error);
        if ptr.is_null() {
            return String::from("unknown ALSA error");
        }
        CStr::from_ptr(ptr).to_string_lossy().into_owned()
    }
}

// ============================================================================
// 書き込みスレッド
// ============================================================================

/// `snd_pcm_t` のポインタをスレッドへ渡すための薄い包み。
///
/// 🔴 クロージャへは**必ずこの値ごと**ムーブすること(`.0` を直接触ると edition 2021
/// 以降の分割キャプチャで生ポインタだけが捕まり、`Send` でなくなる)。
#[cfg(target_os = "linux")]
struct PcmHandle(*mut SndPcm);

// SAFETY: ALSA の PCM ハンドルは 1 本のスレッドから使う限りスレッド間で移して構わない。
// 書き込みスレッドが動いている間はそのスレッドだけが触り、`close` は join した後に
// 触る(同時に触る場面が無い)。
#[cfg(target_os = "linux")]
unsafe impl Send for PcmHandle {}

#[cfg(target_os = "linux")]
impl PcmHandle {
    fn get(&self) -> *mut SndPcm {
        self.0
    }
}

/// 書き込みスレッドが排他的に所有する状態。
#[cfg(target_os = "linux")]
struct WriterContext {
    pcm: PcmHandle,
    renderer: Renderer,
    sample_rate: u32,
    /// デバイスのバッファ長(フレーム)。スロットルの上限に使う。
    buffer_frames: u64,
    /// 1 回の処理フレーム数(ピリオド長)。
    period_frames: usize,
    underrun_tracker: OutputUnderrunTracker,
    events: Arc<EventQueue>,
    stop: Arc<AtomicBool>,
    callback_frames: Arc<AtomicU32>,
    output_latency_ns: Arc<AtomicU64>,
}

/// 書き込みスレッドの本体。
///
/// 音声スレッドの規約(`mw-core/COMMON.md`): ループの中でロック・ヒープ確保をしない
/// (バッファは入る前に確保済み。`mw_log!` も呼ばない)。例外は終了時の致命的エラー通知
/// だけ(`push_side_channel` は `Mutex` を使うが、そのあとスレッドは終わる)。
#[cfg(target_os = "linux")]
fn writer_main(mut context: WriterContext) {
    let caught = panic::catch_unwind(AssertUnwindSafe(|| writer_loop(&mut context)));
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

/// ループ本体。止められたら `None`、復旧不能のエラーで抜けたら `Some(理由)`。
#[cfg(target_os = "linux")]
fn writer_loop(context: &mut WriterContext) -> Option<StreamErrorReason> {
    let pcm = context.pcm.get();
    let rate = context.sample_rate;
    let frames = context.period_frames;
    // スレッド開始前に確保(`open` が `period_frames` を確定させてから起動する)。
    let mut buffer = vec![0.0f32; frames * CHANNELS];

    let mut origin_ns = host_time_ns();
    let mut frames_since_origin: u64 = 0;

    while !context.stop.load(Ordering::Acquire) {
        // 時計を持たないデバイス向けのペース配分(実デバイスでは通常発動しない)。
        let elapsed_ns = host_time_ns().saturating_sub(origin_ns);
        let sleep_ns =
            throttle_sleep_ns(frames_since_origin, elapsed_ns, rate, context.buffer_frames);
        if sleep_ns > 0 {
            std::thread::sleep(Duration::from_nanos(sleep_ns));
            continue;
        }

        let callback_ns = host_time_ns();
        let mut delay: SndPcmSframes = 0;
        // SAFETY: `pcm` は `open` が開いた有効なハンドル(このスレッドが唯一の使用者)。
        // `delay` はスタック上の有効な書き込み先。
        let status = unsafe { snd_pcm_delay(pcm, &mut delay) };
        let delay_frames = if status == 0 {
            delay as i64
        } else {
            // まだ走り出していない(`PREPARED`)等で値が取れない。書き込み済みの
            // フレームがそのまま未再生とみなす(上限はバッファ長)。
            frames_since_origin.min(context.buffer_frames) as i64
        };
        let start_ns = playback_start_ns(callback_ns, delay_frames, rate);
        context
            .output_latency_ns
            .store(delay_ns(delay_frames, rate), Ordering::Relaxed);
        context
            .callback_frames
            .store(frames as u32, Ordering::Relaxed);
        context
            .underrun_tracker
            .observe(callback_ns, frames as u32, rate);

        // 音声スレッド上で呼ぶ mw-core 側の経路は `Renderer::render` のみ。
        context.renderer.render(&mut buffer, start_ns);

        // 書き切るまで繰り返す(部分書き込み・復旧後の再書き込みがある)。
        let mut written_frames = 0usize;
        while written_frames < frames {
            if context.stop.load(Ordering::Acquire) {
                return None;
            }
            let remaining = &buffer[written_frames * CHANNELS..];
            // SAFETY: `remaining` は `(frames - written_frames) * CHANNELS` 個の有効な f32
            // (インターリーブ f32 ステレオ = `SND_PCM_FORMAT_FLOAT_LE`)。
            let result = unsafe {
                snd_pcm_writei(
                    pcm,
                    remaining.as_ptr().cast(),
                    (frames - written_frames) as SndPcmUframes,
                )
            };
            if result >= 0 {
                let n = result as usize;
                written_frames += n;
                frames_since_origin += n as u64;
                if n == 0 {
                    std::thread::sleep(Duration::from_millis(1));
                }
                continue;
            }
            let error = result as c_int;
            match classify_write_error(error) {
                WriteErrorAction::Retry => std::thread::sleep(Duration::from_millis(1)),
                WriteErrorAction::Recover { underrun } => {
                    // SAFETY: 同上。`silent=1` で ALSA 自身の stderr 出力を抑える。
                    let recovered = unsafe { snd_pcm_recover(pcm, error, 1) };
                    if recovered < 0 {
                        return Some(fatal_reason(recovered));
                    }
                    if underrun {
                        context
                            .underrun_tracker
                            .record_reported_underrun(host_time_ns());
                    }
                    // 復旧でデバイスが止まっていた時間のぶん、スロットルの基準を取り直す。
                    origin_ns = host_time_ns();
                    frames_since_origin = 0;
                }
                WriteErrorAction::Fatal(reason) => return Some(reason),
            }
        }
    }
    None
}

/// 復旧に失敗した `-errno` から通知する理由を決める。
#[cfg(target_os = "linux")]
fn fatal_reason(error: i32) -> StreamErrorReason {
    match classify_write_error(error) {
        WriteErrorAction::Fatal(reason) => reason,
        _ => StreamErrorReason::Backend,
    }
}

// ============================================================================
// Backend 実装(Linux のみ)
// ============================================================================

/// Linux の既定出力(ALSA `default`)へ直接出力する `Backend` 実装。
#[cfg(target_os = "linux")]
pub struct LinuxBackend {
    pcm: Option<*mut SndPcm>,
    thread: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    callback_frames: Arc<AtomicU32>,
    output_latency_ns: Arc<AtomicU64>,
    logged_output_latency: AtomicBool,
    sample_rate: u32,
    output_underrun_count: Arc<AtomicU64>,
    last_output_underrun_host_time_ns: Arc<AtomicU64>,
    consecutive_output_underrun_count: Arc<AtomicU32>,
    logged_output_underrun_count: AtomicU64,
}

// SAFETY: `pcm` は生ポインタだが、`mw-ffi::handle::Instance` がグローバルレジストリの
// `Mutex` 経由で単一所有を保証するため、`&mut self` メソッドが複数スレッドから同時に
// 呼ばれることは無い(`AndroidBackend` と同じ理由)。書き込みスレッドへ渡した後は
// `close` が join するまでそちらだけが触る。
#[cfg(target_os = "linux")]
unsafe impl Send for LinuxBackend {}

#[cfg(target_os = "linux")]
impl Default for LinuxBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_os = "linux")]
impl LinuxBackend {
    pub fn new() -> Self {
        Self {
            pcm: None,
            thread: None,
            stop: Arc::new(AtomicBool::new(false)),
            callback_frames: Arc::new(AtomicU32::new(0)),
            output_latency_ns: Arc::new(AtomicU64::new(0)),
            logged_output_latency: AtomicBool::new(false),
            sample_rate: 0,
            output_underrun_count: Arc::new(AtomicU64::new(0)),
            last_output_underrun_host_time_ns: Arc::new(AtomicU64::new(0)),
            consecutive_output_underrun_count: Arc::new(AtomicU32::new(0)),
            logged_output_underrun_count: AtomicU64::new(0),
        }
    }
}

#[cfg(target_os = "linux")]
impl Backend for LinuxBackend {
    fn open(&mut self, renderer: Renderer, events: Arc<EventQueue>) -> Result<(), BackendError> {
        if self.pcm.is_some() {
            return Err(BackendError::AlreadyOpen);
        }
        let mut renderer = renderer;

        let mut pcm: *mut SndPcm = std::ptr::null_mut();
        // SAFETY: `&mut pcm` は有効な出力先。`c"default"` は NUL 終端の静的文字列。
        let status =
            unsafe { snd_pcm_open(&mut pcm, c"default".as_ptr(), SND_PCM_STREAM_PLAYBACK, 0) };
        if status < 0 || pcm.is_null() {
            return Err(classify_open_error(status, &strerror(status)));
        }

        // SAFETY: `pcm` は直前に開いた有効なハンドル。
        let status = unsafe {
            snd_pcm_set_params(
                pcm,
                SND_PCM_FORMAT_FLOAT_LE,
                SND_PCM_ACCESS_RW_INTERLEAVED,
                CHANNELS as c_uint,
                TARGET_SAMPLE_RATE_HZ,
                1,
                TARGET_LATENCY_US,
            )
        };
        if status < 0 {
            let error = classify_open_error(status, &strerror(status));
            // SAFETY: 上で開いたハンドルをここで閉じる(以降使わない)。
            unsafe {
                snd_pcm_close(pcm);
            }
            return Err(error);
        }

        let mut buffer_size: SndPcmUframes = 0;
        let mut period_size: SndPcmUframes = 0;
        // SAFETY: `pcm` は有効。出力先はスタック上の有効な変数。
        let status = unsafe { snd_pcm_get_params(pcm, &mut buffer_size, &mut period_size) };
        if status < 0 {
            let error = BackendError::BuildStreamFailed(format!(
                "snd_pcm_get_params failed: {} ({status})",
                strerror(status)
            ));
            // SAFETY: 同上。
            unsafe {
                snd_pcm_close(pcm);
            }
            return Err(error);
        }

        let sample_rate = TARGET_SAMPLE_RATE_HZ;
        let period_frames = sanitize_period_frames(period_size as u64, sample_rate);
        // ランプのミリ秒→サンプル数換算(初期構築仕様 §4.1)が正しいレートを使えるよう、
        // スレッドを起こす前に確定させる。
        renderer.set_sample_rate(sample_rate);

        self.stop.store(false, Ordering::Release);
        let context = WriterContext {
            pcm: PcmHandle(pcm),
            renderer,
            sample_rate,
            buffer_frames: buffer_size as u64,
            period_frames,
            underrun_tracker: OutputUnderrunTracker::new(
                Arc::clone(&self.output_underrun_count),
                Arc::clone(&self.last_output_underrun_host_time_ns),
                Arc::clone(&self.consecutive_output_underrun_count),
            ),
            events,
            stop: Arc::clone(&self.stop),
            callback_frames: Arc::clone(&self.callback_frames),
            output_latency_ns: Arc::clone(&self.output_latency_ns),
        };
        let thread = std::thread::Builder::new()
            .name("mw-alsa-writer".into())
            .spawn(move || writer_main(context));
        let thread = match thread {
            Ok(thread) => thread,
            Err(error) => {
                // SAFETY: スレッドが起動しなかったので、`pcm` はこの関数だけが持っている。
                unsafe {
                    snd_pcm_close(pcm);
                }
                return Err(BackendError::PlayStreamFailed(format!(
                    "failed to spawn the ALSA writer thread: {error}"
                )));
            }
        };

        crate::mw_log!(
            "[mw-backend] (native/linux) output stream started: sample_rate={sample_rate} Hz, \
             channels={CHANNELS}, period={period_frames} frames, buffer={buffer_size} frames"
        );

        self.pcm = Some(pcm);
        self.thread = Some(thread);
        self.sample_rate = sample_rate;
        Ok(())
    }

    fn close(&mut self) -> Result<(), BackendError> {
        let (Some(pcm), Some(thread)) = (self.pcm.take(), self.thread.take()) else {
            return Err(BackendError::NotOpen);
        };
        self.stop.store(true, Ordering::Release);
        // スレッドは `writei` のブロック(最大でバッファ長ぶん)を抜けた次の周回で止まる。
        // パニックで終わっていても `Err` が返るだけなので無視してよい。
        let _ = thread.join();
        // SAFETY: 書き込みスレッドは join 済みで、`pcm` を触る者はもういない。
        // `drop` は再生待ちのデータを捨てて即座に止める(`drain` は待つので使わない)。
        unsafe {
            snd_pcm_drop(pcm);
            snd_pcm_close(pcm);
        }

        self.sample_rate = 0;
        self.callback_frames.store(0, Ordering::Relaxed);
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
        self.pcm.is_some()
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
            "[mw-backend] (native/linux) output latency (measured, snd_pcm_delay): \
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
            "[mw-backend] (native/linux) output underrun suspected: +{new_count} since \
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

    #[test]
    fn playback_start_adds_the_queued_frames_to_the_callback_time() {
        // 48kHz で 960 フレーム = 20ms 先。
        assert_eq!(playback_start_ns(1_000_000_000, 960, 48_000), 1_020_000_000);
    }

    #[test]
    fn playback_start_treats_a_negative_delay_as_zero() {
        assert_eq!(playback_start_ns(5_000, -10, 48_000), 5_000);
        assert_eq!(delay_ns(-1, 48_000), 0);
    }

    #[test]
    fn playback_start_falls_back_to_the_callback_time_when_the_rate_is_zero() {
        assert_eq!(playback_start_ns(5_000, 960, 0), 5_000);
    }

    #[test]
    fn playback_start_saturates_instead_of_overflowing() {
        assert_eq!(playback_start_ns(u64::MAX - 1, 48_000, 48_000), u64::MAX);
        assert_eq!(delay_ns(i64::MAX, 1), u64::MAX);
    }

    #[test]
    fn delay_ns_converts_frames_at_the_given_rate() {
        assert_eq!(delay_ns(48, 48_000), 1_000_000);
        assert_eq!(delay_ns(441, 44_100), 10_000_000);
    }

    #[test]
    fn throttle_does_not_sleep_while_within_the_buffer_allowance() {
        // 経過 0 でもバッファ長ぶんは先行して書いてよい。
        assert_eq!(throttle_sleep_ns(960, 0, 48_000, 960), 0);
        // 実時間 1 秒 = 48000 フレーム消費。48000 + 960 まで OK。
        assert_eq!(throttle_sleep_ns(48_960, 1_000_000_000, 48_000, 960), 0);
    }

    #[test]
    fn throttle_sleeps_for_the_excess_frames() {
        // 許容 960 に対して 1440 書いた = 480 フレーム超過 = 10ms。
        assert_eq!(throttle_sleep_ns(1_440, 0, 48_000, 960), 10_000_000);
    }

    #[test]
    fn throttle_never_sleeps_when_the_rate_is_unknown() {
        assert_eq!(throttle_sleep_ns(u64::MAX, 0, 0, 0), 0);
    }

    #[test]
    fn sanitize_period_frames_falls_back_for_zero_and_caps_huge_values() {
        assert_eq!(sanitize_period_frames(0, 48_000), 480);
        assert_eq!(sanitize_period_frames(0, 0), 1);
        assert_eq!(sanitize_period_frames(240, 48_000), 240);
        assert_eq!(sanitize_period_frames(u64::MAX, 48_000), MAX_PERIOD_FRAMES);
    }

    #[test]
    fn open_errors_for_a_missing_device_map_to_no_output_device() {
        assert!(matches!(
            classify_open_error(-ENOENT, "No such file"),
            BackendError::NoOutputDevice
        ));
        assert!(matches!(
            classify_open_error(-ENODEV, "No such device"),
            BackendError::NoOutputDevice
        ));
    }

    #[test]
    fn other_open_errors_keep_the_detail() {
        match classify_open_error(-EBUSY, "Device or resource busy") {
            BackendError::BuildStreamFailed(detail) => {
                assert!(detail.contains("Device or resource busy"), "{detail}");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn write_errors_are_classified_by_how_the_thread_should_react() {
        assert_eq!(
            classify_write_error(-EPIPE),
            WriteErrorAction::Recover { underrun: true }
        );
        assert_eq!(
            classify_write_error(-ESTRPIPE),
            WriteErrorAction::Recover { underrun: true }
        );
        assert_eq!(
            classify_write_error(-EINTR),
            WriteErrorAction::Recover { underrun: false }
        );
        assert_eq!(classify_write_error(-EAGAIN), WriteErrorAction::Retry);
        assert_eq!(
            classify_write_error(-ENODEV),
            WriteErrorAction::Fatal(StreamErrorReason::DeviceUnavailable)
        );
        assert_eq!(
            classify_write_error(-EACCES),
            WriteErrorAction::Fatal(StreamErrorReason::PermissionDenied)
        );
        assert_eq!(
            classify_write_error(-12345),
            WriteErrorAction::Fatal(StreamErrorReason::Backend)
        );
    }

    /// ハードウェア不要: 開いていないバックエンドを閉じようとすると `NotOpen`。
    #[cfg(target_os = "linux")]
    #[test]
    fn closing_an_unopened_backend_reports_not_open() {
        let mut backend = LinuxBackend::new();
        assert!(matches!(backend.close(), Err(BackendError::NotOpen)));
    }

    /// デバイスが無い環境(CI・コンテナ)ではパニックせず `Err` で返る。デバイスが
    /// ある環境では開いて(無音で)すぐ閉じる。どちらでも落ちないことだけを見る。
    #[cfg(target_os = "linux")]
    #[test]
    fn open_fails_gracefully_or_opens_and_closes_cleanly() {
        let (renderer, _sender, _reclaim, _music_producer, _music_clock, events, _bgm) =
            mw_core::Renderer::build(mw_core::Config::default(), 48_000);
        let mut backend = LinuxBackend::new();
        match backend.open(renderer, events) {
            Ok(()) => {
                assert!(backend.is_open());
                backend.close().expect("close should succeed");
            }
            Err(_) => assert!(!backend.is_open()),
        }
        assert!(!backend.is_open());
    }

    /// プロセスが使った CPU 時間(ユーザ + システム)を clock tick 単位で返す。
    #[cfg(target_os = "linux")]
    fn process_cpu_ticks() -> u64 {
        let stat = std::fs::read_to_string("/proc/self/stat").expect("/proc/self/stat");
        // `comm` が括弧で括られ空白を含みうるので、最後の `)` の後ろから数える。
        let rest = &stat[stat.rfind(')').expect("comm") + 2..];
        let fields: Vec<&str> = rest.split_whitespace().collect();
        // rest[0] は state(全体の3番目)。utime = 14番目、stime = 15番目。
        fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap()
    }

    /// 時計を持たない `null` デバイスで、スレッドが CPU を空回りせず実時間で進むこと。
    /// `ALSA_CONFIG_PATH` が `pcm.!default { type null }` だけの設定ファイルを指す環境で
    /// `cargo test -p mw-backend -- --ignored` で手動実行する。
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "ALSA の default を null デバイスへ差し替えた環境で手動実行する"]
    fn runs_on_a_null_device_without_spinning_the_cpu() {
        let (renderer, _sender, _reclaim, _music_producer, music_clock, events, _bgm) =
            mw_core::Renderer::build(mw_core::Config::default(), 48_000);
        let mut backend = LinuxBackend::new();
        backend
            .open(renderer, events)
            .expect("open should succeed on the null device");

        let cpu_before = process_cpu_ticks();
        std::thread::sleep(Duration::from_millis(1_000));
        let cpu_ticks = process_cpu_ticks() - cpu_before;

        assert!(backend.last_callback_frames() > 0, "the writer never ran");
        assert_eq!(backend.sample_rate(), TARGET_SAMPLE_RATE_HZ);
        assert!(!music_clock.snapshot().is_playing);
        // 空回りすると 1 秒でほぼ 100 tick(1 コア)を使う。
        assert!(
            cpu_ticks < 20,
            "writer thread spun the CPU: {cpu_ticks} ticks"
        );

        backend.close().expect("close should succeed");
        assert!(!backend.is_open());
    }
}
