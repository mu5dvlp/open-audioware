//! 最初の出力ユニットの起動直後に、報告されない遅延が乗ることがある端末への対策
//! (Apple の出力だけが使う。判定の部分は OS 非依存の関数にして単体テストできるようにしてある)。
//!
//! # 背景
//!
//! 一部の端末では、`open()` が作った最初の出力ユニットを動かしたままにしていると、
//! OS が報告する出力遅延(`AVAudioSession.outputLatency` など)に現れない遅れが
//! 音に乗り続け、予測した出力時刻より実際の音が遅れて出ることがある。出力ユニットを
//! 一度止めて動かし直す(背面からの復帰と同じ手順)と、報告値は変わらないまま遅れだけが消える。
//!
//! # 対策
//!
//! `open()` の後で最初の曲の予約をする前(`restart_epoch == 0` のまま)に、一度だけ
//! pause → play を行う。曲はまだ鳴っていないので、世代(`restart_epoch`)が進んでも害は無い。
//! `play()` が世代を進めるので、同じ `open()` の間は二度目以降は走らない。
//!
//! 切り替え: [`FirstStartRestart::from_env`]([`ENV_VAR`] が `1` / `true` / `on` で有効)。
//! 既定は無効。
//!
//! # 出力遅延が 0 と読めたとき
//!
//! `outputLatency` はセッションが落ち着く前に 0 と読めることがある。0 は「不明」として
//! 扱い、前の値を保つ([`keep_known_latency`])。

use std::sync::atomic::{AtomicU64, Ordering};

use crate::ios_interruption::RecoverableOutput;

/// 有効にする環境変数の名前。`1` / `true` / `on`(大文字小文字は問わない)で有効。
pub const ENV_VAR: &str = "MW_FIRST_START_RESTART";

/// 「最初の予約の前に出力を動かし直す」設定。既定は無効。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FirstStartRestart {
    pub enabled: bool,
}

impl FirstStartRestart {
    /// 環境変数 [`ENV_VAR`] から読む。未設定・空・認識できない値は無効。
    pub fn from_env() -> Self {
        Self::from_value(std::env::var(ENV_VAR).ok().as_deref())
    }

    pub fn from_value(value: Option<&str>) -> Self {
        let enabled = match value.map(|v| v.trim().to_ascii_lowercase()) {
            Some(v) => matches!(v.as_str(), "1" | "true" | "on"),
            None => false,
        };
        Self { enabled }
    }
}

/// [`restart_if_first_start`] の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartOutcome {
    /// 設定で無効。
    Disabled,
    /// 既に世代が進んでいる(動かし直し・復帰を経ている)ので何もしなかった。
    AlreadyRestarted,
    /// pause → play を行った。
    Restarted,
    /// 行ったが、どちらかが失敗した(文言はログ用)。
    Failed(String),
}

/// `restart_epoch == 0` のときだけ、出力を pause → play で一度動かし直す。
///
/// 非リアルタイムスレッド(ゲームスレッド)から呼ぶこと。`play()` が世代を進めるため、
/// 続けて呼んでも二度目は [`RestartOutcome::AlreadyRestarted`] になる。
pub fn restart_if_first_start<O: RecoverableOutput + ?Sized>(
    output: &O,
    restart_epoch: &AtomicU64,
    config: FirstStartRestart,
) -> RestartOutcome {
    if !config.enabled {
        return RestartOutcome::Disabled;
    }
    if restart_epoch.load(Ordering::Relaxed) != 0 {
        return RestartOutcome::AlreadyRestarted;
    }
    let paused = output.pause();
    // pause が失敗しても play は試みる(止まっていなければ play は無害)。
    let played = output.play();
    match (paused, played) {
        (Ok(()), Ok(())) => RestartOutcome::Restarted,
        (Err(e), Ok(())) => RestartOutcome::Failed(format!("pause: {e}")),
        (_, Err(e)) => RestartOutcome::Failed(format!("play: {e}")),
    }
}

/// 読み直した出力遅延(ns)が 0 なら「不明」として前の値を保つ。0 でなければ新しい値。
/// 前の値も 0 なら 0 のまま。
pub fn keep_known_latency(previous_ns: u64, new_ns: u64) -> u64 {
    if new_ns == 0 { previous_ns } else { new_ns }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Mock<'a> {
        epoch: &'a AtomicU64,
        calls: Mutex<Vec<&'static str>>,
        fail_play: bool,
    }

    impl RecoverableOutput for Mock<'_> {
        fn pause(&self) -> Result<(), String> {
            self.calls.lock().unwrap().push("pause");
            Ok(())
        }
        fn play(&self) -> Result<(), String> {
            self.calls.lock().unwrap().push("play");
            // 実装の UnitControl::play と同じく世代を進める。
            self.epoch.fetch_add(1, Ordering::Relaxed);
            if self.fail_play {
                Err("boom".into())
            } else {
                Ok(())
            }
        }
    }

    fn mock(epoch: &AtomicU64, fail_play: bool) -> Mock<'_> {
        Mock {
            epoch,
            calls: Mutex::new(Vec::new()),
            fail_play,
        }
    }

    #[test]
    fn runs_once_when_epoch_is_zero() {
        let epoch = AtomicU64::new(0);
        let m = mock(&epoch, false);
        let c = FirstStartRestart { enabled: true };
        assert_eq!(
            restart_if_first_start(&m, &epoch, c),
            RestartOutcome::Restarted
        );
        assert_eq!(*m.calls.lock().unwrap(), vec!["pause", "play"]);
        assert_eq!(
            restart_if_first_start(&m, &epoch, c),
            RestartOutcome::AlreadyRestarted
        );
        assert_eq!(m.calls.lock().unwrap().len(), 2);
    }

    #[test]
    fn skipped_when_epoch_already_advanced() {
        let epoch = AtomicU64::new(3);
        let m = mock(&epoch, false);
        let out = restart_if_first_start(&m, &epoch, FirstStartRestart { enabled: true });
        assert_eq!(out, RestartOutcome::AlreadyRestarted);
        assert!(m.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn skipped_when_disabled() {
        let epoch = AtomicU64::new(0);
        let m = mock(&epoch, false);
        let out = restart_if_first_start(&m, &epoch, FirstStartRestart::default());
        assert_eq!(out, RestartOutcome::Disabled);
        assert!(m.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn reports_failure_but_still_tries_play() {
        let epoch = AtomicU64::new(0);
        let m = mock(&epoch, true);
        let out = restart_if_first_start(&m, &epoch, FirstStartRestart { enabled: true });
        assert_eq!(out, RestartOutcome::Failed("play: boom".into()));
        assert_eq!(*m.calls.lock().unwrap(), vec!["pause", "play"]);
    }

    #[test]
    fn env_value_parsing() {
        assert!(!FirstStartRestart::default().enabled);
        assert!(!FirstStartRestart::from_value(None).enabled);
        assert!(!FirstStartRestart::from_value(Some("")).enabled);
        assert!(!FirstStartRestart::from_value(Some("0")).enabled);
        assert!(!FirstStartRestart::from_value(Some("false")).enabled);
        assert!(!FirstStartRestart::from_value(Some("yes")).enabled);
        assert!(FirstStartRestart::from_value(Some("1")).enabled);
        assert!(FirstStartRestart::from_value(Some("true")).enabled);
        assert!(FirstStartRestart::from_value(Some(" ON ")).enabled);
        assert!(FirstStartRestart::from_value(Some("True")).enabled);
    }

    #[test]
    fn zero_latency_keeps_previous_value() {
        assert_eq!(keep_known_latency(17_900_000, 0), 17_900_000);
        assert_eq!(keep_known_latency(17_900_000, 20_000_000), 20_000_000);
        assert_eq!(keep_known_latency(0, 0), 0);
        assert_eq!(keep_known_latency(0, 5), 5);
    }
}
