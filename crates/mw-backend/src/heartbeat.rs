//! 出力コールバックの進みを一定間隔でログへ出す「ハートビート」(診断用。既定では無効)。
//!
//! 通知なしに出力が止まる環境(アプリの背面遷移・デバイス切り替えなど)で、
//! 「いま出力コールバックが回っているか」を外から(ログだけで)判定できるようにする。
//! 1 行の書式:
//!
//! ```text
//! [mw-backend] audio heartbeat ticks=<n> last_frames=<f> backend=<label> peak=<p>
//! ```
//!
//! iOS / tvOS では末尾に ` session_rate=<Hz> session_channels=<n> session_out=[<portType>,..]
//! category=<c>` が付く(OS への問い合わせはこの専用スレッドで行う)。
//!
//! - `ticks`: 開いてからの出力コールバックの累計回数(単調増加。再オープンで 0 に戻る)。
//!   2 つの行で値が増えていれば、その間に出力が動いている。
//! - `last_frames`: 直近のコールバックが処理したフレーム数。
//! - `backend`: 出力の実装(`cpal` / `apple` / `android` / `windows` / `linux`)。
//! - `peak`: 前回の行からの間に出力バッファへ書かれた最大の絶対値(0 なら無音を出している)。
//!
//! # 有効にする
//!
//! 環境変数 [`ENV_VAR`](`MW_AUDIO_HEARTBEAT`)を、出力を開く(`Backend::open`)前に設定する。
//! `1` / `true` / `on` で既定の間隔(1000 ms)。50 以上の整数ならその値をミリ秒の間隔とする。
//! 未設定・`0`・`off`・`false` は無効。環境変数を使えない組み込み先のために、
//! 同じ効果の [`set_interval_ms`] も公開している(環境変数より優先される)。
//!
//! # リアルタイム安全性
//!
//! 音声コールバック側は、既存のアトミックなカウンタを進めるだけ(このモジュールは何も足さない)。
//! 読み出しとログの書き出し(`format!` のアロケーションとロック)は専用のスレッドで行う。
//! 間隔の待ちには `Condvar` を使うので、`Drop` で即座に止まる。

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// ハートビートを有効にする環境変数の名前。
pub const ENV_VAR: &str = "MW_AUDIO_HEARTBEAT";

/// 有効にしたときの既定の間隔(ミリ秒)。
pub const DEFAULT_INTERVAL_MS: u64 = 1000;

/// 数値で指定できる間隔の下限(ミリ秒)。これより小さい整数は無効の指定とみなす
/// (`1` は「有効」の意味で使うため)。
pub const MIN_INTERVAL_MS: u64 = 50;

/// [`set_interval_ms`] の上書き値。`u64::MAX` は未設定(環境変数に従う)。
static OVERRIDE_INTERVAL_MS: AtomicU64 = AtomicU64::new(u64::MAX);

/// 環境変数より優先して間隔を設定する。`0` で無効、`1..` でその間隔(ミリ秒。
/// [`MIN_INTERVAL_MS`] 未満は [`MIN_INTERVAL_MS`] に切り上げる)。次に出力を開くときから効く。
pub fn set_interval_ms(interval_ms: u64) {
    OVERRIDE_INTERVAL_MS.store(interval_ms, Ordering::Relaxed);
}

/// [`set_interval_ms`] の上書きを取り消し、環境変数に従う状態へ戻す。
pub fn clear_interval_override() {
    OVERRIDE_INTERVAL_MS.store(u64::MAX, Ordering::Relaxed);
}

/// 環境変数の値(`None` は未設定)から間隔を決める。`None` を返すと無効。
pub fn parse_interval(value: Option<&str>) -> Option<Duration> {
    let value = value?.trim();
    match value.to_ascii_lowercase().as_str() {
        "" | "0" | "off" | "false" | "no" => None,
        "1" | "true" | "on" | "yes" => Some(Duration::from_millis(DEFAULT_INTERVAL_MS)),
        other => {
            let ms: u64 = other.parse().ok()?;
            (ms >= MIN_INTERVAL_MS).then(|| Duration::from_millis(ms))
        }
    }
}

/// 現在の設定から有効な間隔を返す。`None` は無効。
pub fn configured_interval() -> Option<Duration> {
    match OVERRIDE_INTERVAL_MS.load(Ordering::Relaxed) {
        u64::MAX => parse_interval(std::env::var(ENV_VAR).ok().as_deref()),
        0 => None,
        ms => Some(Duration::from_millis(ms.max(MIN_INTERVAL_MS))),
    }
}

/// ログ 1 行の本文(`[mw-backend]` の接頭辞付き)を作る。
pub fn format_line(label: &str, ticks: u64, last_frames: u32, peak: f32) -> String {
    format!(
        "[mw-backend] audio heartbeat ticks={ticks} last_frames={last_frames} backend={label} \
         peak={peak:.6}"
    )
}

/// ハートビートのスレッド。`Drop` で止めて join する。
pub struct Heartbeat {
    shutdown: Arc<(Mutex<bool>, Condvar)>,
    thread: Option<JoinHandle<()>>,
}

impl Heartbeat {
    /// 設定が有効なら、`ticks` / `last_frames` を読んでログへ出すスレッドを起こす。
    /// 無効、またはスレッドを作れなければ `None`(出力の動作には影響させない)。
    pub fn start_if_enabled(
        label: &'static str,
        ticks: Arc<AtomicU64>,
        last_frames: Arc<AtomicU32>,
    ) -> Option<Self> {
        Self::start_if_enabled_with_peak(label, ticks, last_frames, Arc::new(AtomicU32::new(0)))
    }

    /// [`Self::start_if_enabled`] に、出力の最大の絶対値(`f32` のビット列。読むたびに 0 へ
    /// 戻す)も渡す版。`peak=` の値になる。
    pub fn start_if_enabled_with_peak(
        label: &'static str,
        ticks: Arc<AtomicU64>,
        last_frames: Arc<AtomicU32>,
        peak: Arc<AtomicU32>,
    ) -> Option<Self> {
        let interval = configured_interval()?;
        Self::start_with(
            interval,
            label,
            ticks,
            last_frames,
            peak,
            crate::platform_log::write,
        )
    }

    /// 間隔と出力先を指定して起こす(テスト用の入口でもある)。
    pub fn start_with<F>(
        interval: Duration,
        label: &'static str,
        ticks: Arc<AtomicU64>,
        last_frames: Arc<AtomicU32>,
        peak: Arc<AtomicU32>,
        mut emit: F,
    ) -> Option<Self>
    where
        F: FnMut(&str) + Send + 'static,
    {
        let shutdown = Arc::new((Mutex::new(false), Condvar::new()));
        let worker_shutdown = Arc::clone(&shutdown);
        let thread = std::thread::Builder::new()
            .name("mw-audio-heartbeat".into())
            .spawn(move || {
                let (lock, cvar) = &*worker_shutdown;
                loop {
                    let Ok(guard) = lock.lock() else { return };
                    let Ok((guard, _)) = cvar.wait_timeout_while(guard, interval, |stop| !*stop)
                    else {
                        return;
                    };
                    if *guard {
                        return;
                    }
                    drop(guard);
                    let peak_now = f32::from_bits(peak.swap(0, Ordering::Relaxed));
                    let mut line = format_line(
                        label,
                        ticks.load(Ordering::Relaxed),
                        last_frames.load(Ordering::Relaxed),
                        peak_now,
                    );
                    let session = crate::ios_session::session_summary();
                    if !session.is_empty() {
                        line.push(' ');
                        line.push_str(&session);
                    }
                    emit(&line);
                }
            })
            .ok()?;
        Some(Self {
            shutdown,
            thread: Some(thread),
        })
    }
}

impl Drop for Heartbeat {
    fn drop(&mut self) {
        let (lock, cvar) = &*self.shutdown;
        if let Ok(mut stop) = lock.lock() {
            *stop = true;
        }
        cvar.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn parse_interval_accepts_flags_and_millisecond_values() {
        assert_eq!(parse_interval(None), None);
        assert_eq!(parse_interval(Some("")), None);
        assert_eq!(parse_interval(Some("0")), None);
        assert_eq!(parse_interval(Some("off")), None);
        assert_eq!(parse_interval(Some("FALSE")), None);
        assert_eq!(parse_interval(Some("abc")), None);
        assert_eq!(parse_interval(Some("49")), None);
        assert_eq!(
            parse_interval(Some("1")),
            Some(Duration::from_millis(DEFAULT_INTERVAL_MS))
        );
        assert_eq!(
            parse_interval(Some(" On ")),
            Some(Duration::from_millis(DEFAULT_INTERVAL_MS))
        );
        assert_eq!(
            parse_interval(Some("250")),
            Some(Duration::from_millis(250))
        );
    }

    #[test]
    fn format_line_has_the_documented_shape() {
        assert_eq!(
            format_line("windows", 42, 480, 0.25),
            "[mw-backend] audio heartbeat ticks=42 last_frames=480 backend=windows peak=0.250000"
        );
    }

    #[test]
    fn override_beats_environment_and_can_be_cleared() {
        // 他のテストと共有する静的値。これを触るのはこのテストだけ。
        set_interval_ms(0);
        assert_eq!(configured_interval(), None);
        set_interval_ms(10);
        assert_eq!(
            configured_interval(),
            Some(Duration::from_millis(MIN_INTERVAL_MS))
        );
        set_interval_ms(300);
        assert_eq!(configured_interval(), Some(Duration::from_millis(300)));
        clear_interval_override();
    }

    #[test]
    fn emits_lines_at_the_interval_and_reads_the_live_counters() {
        let ticks = Arc::new(AtomicU64::new(0));
        let frames = Arc::new(AtomicU32::new(512));
        let lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = Arc::clone(&lines);
        let started = Instant::now();
        let heartbeat = Heartbeat::start_with(
            Duration::from_millis(40),
            "test",
            Arc::clone(&ticks),
            Arc::clone(&frames),
            Arc::new(AtomicU32::new(0)),
            move |line| sink.lock().unwrap().push(line.to_string()),
        )
        .expect("thread spawn");

        let deadline = Instant::now() + Duration::from_secs(5);
        while lines.lock().unwrap().len() < 3 {
            ticks.fetch_add(10, Ordering::Relaxed);
            assert!(Instant::now() < deadline, "heartbeat lines never arrived");
            std::thread::sleep(Duration::from_millis(5));
        }
        // 間隔(40 ms)より速くは出ない: 3 行目は 120 ms 以降。
        assert!(started.elapsed() >= Duration::from_millis(100));
        drop(heartbeat);

        let lines = lines.lock().unwrap().clone();
        let mut previous = 0u64;
        for line in &lines {
            assert!(line.starts_with("[mw-backend] audio heartbeat ticks="));
            assert!(line.ends_with("last_frames=512 backend=test peak=0.000000"));
            let value: u64 = line
                .split("ticks=")
                .nth(1)
                .and_then(|rest| rest.split(' ').next())
                .and_then(|n| n.parse().ok())
                .expect("ticks field");
            assert!(value >= previous, "ticks must be monotonic");
            previous = value;
        }
        assert!(previous > 0, "the counter advance was never observed");
    }

    #[test]
    fn drop_stops_the_thread_promptly_and_no_more_lines_follow() {
        let lines = Arc::new(Mutex::new(0usize));
        let sink = Arc::clone(&lines);
        let heartbeat = Heartbeat::start_with(
            Duration::from_secs(3600),
            "test",
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU32::new(0)),
            Arc::new(AtomicU32::new(0)),
            move |_| *sink.lock().unwrap() += 1,
        )
        .expect("thread spawn");
        let started = Instant::now();
        drop(heartbeat);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(*lines.lock().unwrap(), 0);
    }
}
