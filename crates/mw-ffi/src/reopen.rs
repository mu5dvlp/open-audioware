//! Android(AAudio)切断からの内部再オープン——**いつ試すか/いつ諦めるか**の判定
//! (初期構築仕様『§6』【確定】: 「ストリーム自体の切断(AAudio の disconnect 等)は
//! ミドルウェア内部で再オープンし、イベントで通知する」)。
//!
//! ## 経緯・設計調査
//!
//! `docs/history/03-2026-08-31.md`「M3 最後の残り」で調査済み: cpal の AAudio ホストは
//! 切断を `err_fn` 経由で `Event::StreamError { reason: DeviceUnavailable }` として
//! 既に通知できている(`crates/mw-backend/src/cpal_backend.rs::classify_stream_error`)。
//! 足りなかったのは、それを受けて実際に内部で再オープンする処理そのもの——本モジュールは
//! その「試行の是非」を判定する部分だけを担う。実際の副作用(バックエンドの
//! close/open・コマンド再送)は `crate::handle::Instance::attempt_reopen` が行う。
//!
//! `crates/mw-backend/src/ios_interruption.rs::InterruptionState` /
//! `confirm_recovery_progress` と同じ設計方針を踏襲した: **OS/バックエンド呼び出しを
//! 一切含まない純粋な値**として判定ロジックを切り出し、副作用を伴わずに単体テストで
//! 固定化する。
//!
//! ## 無限リトライを禁止する(依頼書「再オープンに失敗したときどうするか」)
//!
//! [`REOPEN_BACKOFF_SCHEDULE_MS`] を使い切るまでは指数的に間隔を伸ばしながら再試行し、
//! 使い切ったら諦める([`ReopenPolicy::is_exhausted`])。**無限に試行し続けることは
//! しない**——理由: 音声デバイスが恒久的に失われた場合(例: Android 実機が
//! Bluetooth 専用出力しか持たず、それが二度と繋がらない)、`mw_poll_events` を
//! 毎フレーム呼ぶゲームスレッドで際限なく `CpalBackend::open`(デバイス列挙・
//! ストリーム構築という相応のコストを伴う処理)を回し続けるのは実害がある一方、
//! 諦めた後も新しい切断(`DeviceUnavailable`)を観測すれば [`ReopenPolicy::mark_pending`]
//! が新規サイクルとして自動的に仕切り直す設計にしてあるため、「一度失敗したら
//! 二度と直らない」にはならない(次に何か変化があれば再挑戦する)。
//!
//! 合計の自動リトライ待機時間は 250+500+1000+2000+4000 = 7750ms(約7.75秒)。
//! `ios_interruption.rs` の `RECOVERY_WAIT_SCHEDULE_MS`(合計370ms、割り込み復帰用)より
//! 大きくしてある理由: あちらは「OS が既に切り替え終えたルートへの `pause()`→`play()`
//! 再試行」で数十〜数百msのタイミング差の問題だが、こちらは「デバイスの列挙・
//! ストリーム構築が現実に間に合うようになるまで」を待つ必要があり、
//! `default_output_device()` が切断直後すぐに新しいデバイスを返す保証が無い
//! (依頼書の指摘、実機でしか確認できない)ため、より長い時間軸を許容する。

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

/// 再オープン試行の待機スケジュール(ミリ秒)。**【仮】**——実機(Android/AAudio)の
/// 「切断してから再接続可能になるまで」の実測分布を見ずに決めた値
/// (`docs/history/04-2026-08-31.md`「判断に迷い、勝手に確定させなかった点」参照。
/// 長すぎる/短すぎるかは実機でしか判断できない)。[`ReopenPolicy`] が
/// [`ios_interruption::RECOVERY_WAIT_SCHEDULE_MS`](../../mw-backend/src/ios_interruption.rs)
/// と同じ「定数1箇所に集約」方針で、これを使い切ったら [`ReopenPolicy::is_exhausted`] が
/// `true` になる(無限リトライ禁止)。
///
/// 実機計測後に見直す場合は、この配列(1箇所)を書き換えるだけで済む。
/// **意図的に `mw_core::config::Config` へは移していない**(判断): このプロジェクトの
/// 流儀では `Config` は `Renderer::build` へ実際に注入される mw-core 内部の描画パス
/// 寄りの値の置き場所であり(`config.rs` モジュール doc)、バックエンド固有の復旧
/// タイミング定数は対象クレートにローカルな `pub const` として持たせる先例が既にある
/// (`ios_interruption::RECOVERY_WAIT_SCHEDULE_MS` も同様に `mw_core::config::Config` の
/// フィールドではない)。加えて `mw_init` は現時点で C# 側から設定値を注入する経路
/// 自体を持たない(`Config` のドキュメント「M1 時点では FFI からの上書きは未実装。
/// M8 相当」)ため、ここだけ設定構造体へ格上げしても実際には誰も上書きできず、
/// 複雑さが増えるだけで得るものが無い。初期構築仕様の凡例【仮】が要求する
/// 「1箇所を書き換えれば済む構造(設定構造体 / 定数)」は、この `pub const` 1個で
/// 既に満たしている。
pub const REOPEN_BACKOFF_SCHEDULE_MS: [u64; 5] = [250, 500, 1_000, 2_000, 4_000];

/// 再オープンをバックオフ付きで試行するかどうかを判定する、副作用を持たない状態機械。
///
/// `crate::handle::Instance` が1個だけ保持する(`Instance::reopen` フィールド)。
/// フィールドはすべて atomic なので `&Instance`(共有参照)からでも呼べる——
/// `mw_poll_events`(`with_instance` 経由、`&Instance` のみ)が
/// [`ReopenPolicy::mark_pending`]/[`ReopenPolicy::is_due`] を安価に呼べる必要があるため
/// (実際に再オープンを試みる重い処理 `Instance::attempt_reopen` だけは `&mut Instance`
/// 経路〔レジストリの排他ロック、`init`/`shutdown` と同じ〕を要求する)。
#[derive(Debug)]
pub struct ReopenPolicy {
    /// 未解決の切断がある(=いずれ再オープンを試みるべき状態)か。
    pending: AtomicBool,
    /// 直近の新規切断サイクル以降に失敗した連続試行回数。
    attempts: AtomicU32,
    /// 次に試みてよい最短のホスト単調時刻(ns)。
    next_earliest_ns: AtomicU64,
    /// バックオフを使い切り、これ以上自動では試みない状態になったか。
    exhausted: AtomicBool,
}

impl ReopenPolicy {
    pub const fn new() -> Self {
        Self {
            pending: AtomicBool::new(false),
            attempts: AtomicU32::new(0),
            next_earliest_ns: AtomicU64::new(0),
            exhausted: AtomicBool::new(false),
        }
    }

    /// 切断イベント(`Event::StreamError { reason: DeviceUnavailable }`)を観測した。
    ///
    /// 既に未解決の切断が続いている間(`pending` が既に `true`)、かつ**まだ諦めて
    /// いない**間は、連続試行回数・バックオフを**維持したまま**にする——同じ問題が
    /// 続いているだけなら、既に伸びているバックオフを初期化し直さない(でないと、
    /// 切断イベントが連続して積まれるたびにバックオフが実質無効化されてしまう)。
    ///
    /// **新規の切断サイクル**(直前は解決済み、または諦めた後)であれば、試行回数を
    /// 0 から仕切り直し、`now_ns` の時点で即座に試せるようにする。「諦めた後」も
    /// 新規サイクル扱いにする点が要——そうしないと `exhausted` になった `pending`
    /// フラグが `record_result(true, _)`(成功)以外で二度と `false` に戻らないため、
    /// 諦めた後に届く新しい `DeviceUnavailable` イベントが未来永劫無視されてしまう
    /// (`ReopenPolicy` の唯一の目的である「新しい切断が来れば必ずまた試す」が
    /// 壊れる。`tests::a_fresh_disconnect_after_giving_up_starts_a_new_cycle` が
    /// この不変条件を固定化している)。
    pub fn mark_pending(&self, now_ns: u64) {
        let was_pending = self.pending.swap(true, Ordering::Relaxed);
        let was_exhausted = self.exhausted.load(Ordering::Relaxed);
        if !was_pending || was_exhausted {
            self.attempts.store(0, Ordering::Relaxed);
            self.exhausted.store(false, Ordering::Relaxed);
            self.next_earliest_ns.store(now_ns, Ordering::Relaxed);
        }
    }

    /// 今すぐ再オープンを試みるべきか(`Instance::attempt_reopen` を実際に呼ぶ前の
    /// 安価なゲート)。
    pub fn is_due(&self, now_ns: u64) -> bool {
        self.pending.load(Ordering::Relaxed)
            && !self.exhausted.load(Ordering::Relaxed)
            && now_ns >= self.next_earliest_ns.load(Ordering::Relaxed)
    }

    /// 諦めた(自動では二度と試みない)状態かどうか。診断・テスト用。
    pub fn is_exhausted(&self) -> bool {
        self.exhausted.load(Ordering::Relaxed)
    }

    /// 直近の新規切断サイクル以降の連続失敗回数。診断・テスト用。
    pub fn attempts(&self) -> u32 {
        self.attempts.load(Ordering::Relaxed)
    }

    /// 未解決の切断が保留中かどうか。テスト専用の診断アクセサ
    /// (`crate::handle::Instance::reopen_diagnostics` 経由でのみ使う。本番コードは
    /// `pending` を直接見る必要が無い設計——`is_due`/`mark_pending`/`record_result`
    /// だけで実際の判定・遷移が完結する)。
    #[cfg(test)]
    pub fn is_pending(&self) -> bool {
        self.pending.load(Ordering::Relaxed)
    }

    /// 再オープンの試行結果を記録する。
    ///
    /// - 成功: `pending`/`attempts`/`exhausted` をすべてリセットする(次の切断は
    ///   新規サイクルとして扱われる)。
    /// - 失敗: 連続失敗回数を進め、[`REOPEN_BACKOFF_SCHEDULE_MS`] にまだ次の待機時間が
    ///   残っていればそこまで `next_earliest_ns` を進める。**使い切ったら `exhausted` を
    ///   立てて自動再試行を止める**(無限リトライ禁止。モジュール doc 参照)。
    ///   `pending` はどちらの場合も `true` のまま維持する——`exhausted` になった後に
    ///   新しい切断イベントが来れば `mark_pending` が新規サイクルとして仕切り直す。
    pub fn record_result(&self, success: bool, now_ns: u64) {
        if success {
            self.pending.store(false, Ordering::Relaxed);
            self.attempts.store(0, Ordering::Relaxed);
            self.exhausted.store(false, Ordering::Relaxed);
            return;
        }

        let attempts = self.attempts.fetch_add(1, Ordering::Relaxed) + 1;
        match REOPEN_BACKOFF_SCHEDULE_MS.get(attempts as usize - 1) {
            Some(&wait_ms) => {
                self.next_earliest_ns.store(
                    now_ns.saturating_add(wait_ms.saturating_mul(1_000_000)),
                    Ordering::Relaxed,
                );
            }
            None => {
                self.exhausted.store(true, Ordering::Relaxed);
            }
        }
    }
}

impl Default for ReopenPolicy {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_with_nothing_pending() {
        let policy = ReopenPolicy::new();
        assert!(!policy.is_pending());
        assert!(!policy.is_due(0));
        assert!(!policy.is_exhausted());
        assert_eq!(policy.attempts(), 0);
    }

    #[test]
    fn default_matches_new_initial_policy_state() {
        let default = ReopenPolicy::default();
        let new = ReopenPolicy::new();

        assert_eq!(default.is_pending(), new.is_pending());
        assert_eq!(default.attempts(), new.attempts());
        assert_eq!(default.is_exhausted(), new.is_exhausted());
    }

    #[test]
    fn mark_pending_makes_it_immediately_due() {
        let policy = ReopenPolicy::new();
        policy.mark_pending(1_000);
        assert!(policy.is_pending());
        assert!(policy.is_due(1_000));
        assert!(
            policy.is_due(2_000),
            "still due later, not just at the exact instant"
        );
    }

    #[test]
    fn record_failure_backs_off_before_the_next_attempt_is_due() {
        let policy = ReopenPolicy::new();
        policy.mark_pending(0);
        assert!(policy.is_due(0));

        policy.record_result(false, 0);
        assert_eq!(policy.attempts(), 1);
        assert!(
            policy.is_pending(),
            "must stay pending across a failed attempt"
        );
        assert!(!policy.is_exhausted());
        assert!(
            !policy.is_due(0),
            "must not be due again immediately after a failure"
        );

        let first_wait_ns = REOPEN_BACKOFF_SCHEDULE_MS[0] * 1_000_000;
        assert!(!policy.is_due(first_wait_ns - 1));
        assert!(policy.is_due(first_wait_ns));
    }

    #[test]
    fn record_success_resets_everything() {
        let policy = ReopenPolicy::new();
        policy.mark_pending(0);
        policy.record_result(false, 0);
        assert_eq!(policy.attempts(), 1);

        policy.record_result(true, 999);
        assert!(!policy.is_pending());
        assert!(!policy.is_due(999));
        assert_eq!(policy.attempts(), 0);
        assert!(!policy.is_exhausted());
    }

    /// 依頼書「無限リトライは禁止」——バックオフを使い切ったら自動では二度と試みない。
    ///
    /// [`REOPEN_BACKOFF_SCHEDULE_MS`] の `N` 要素は「連続する `N+1` 回の試行」の間に
    /// 挟まる待機時間を表す(1回目の失敗直後に1つ目の待機、……、`N` 回目の失敗直後に
    /// `N` 個目の待機、`N+1` 回目の失敗で `.get()` が `None` を返し諦める)。
    #[test]
    fn gives_up_after_exhausting_the_backoff_schedule_and_never_loops_forever() {
        let policy = ReopenPolicy::new();
        policy.mark_pending(0);

        let total_attempts = REOPEN_BACKOFF_SCHEDULE_MS.len() + 1;
        let mut now = 0u64;
        for attempt in 1..=total_attempts {
            assert!(
                policy.is_due(now),
                "attempt {attempt} must be due at {now}ns"
            );
            policy.record_result(false, now);
            if attempt < total_attempts {
                assert!(
                    !policy.is_exhausted(),
                    "must not give up before the backoff schedule \
                     ({} entries -> {total_attempts} attempts) is exhausted (attempt {attempt})",
                    REOPEN_BACKOFF_SCHEDULE_MS.len()
                );
                now += REOPEN_BACKOFF_SCHEDULE_MS[attempt - 1] * 1_000_000;
            }
        }

        // スケジュールを使い切った直後(合計 `total_attempts` 回失敗した後)、まだ
        // 「保留中」ではあるが諦めた状態になり、どれだけ時間が経っても
        // (=どれだけ `mw_poll_events` が呼ばれ続けても)二度と自動では試行しない。
        assert!(policy.is_pending());
        assert!(policy.is_exhausted());
        assert!(!policy.is_due(now));
        assert!(
            !policy.is_due(u64::MAX),
            "must never become due again on its own, no matter how much time passes \
             (this is the 'no infinite retry loop' guarantee)"
        );
    }

    /// 諦めた後でも、新しい切断サイクル(新たな `DeviceUnavailable` イベント)が来れば
    /// 自動的に仕切り直す——「一度失敗したら二度と直らない」にはならない。
    #[test]
    fn a_fresh_disconnect_after_giving_up_starts_a_new_cycle() {
        let policy = ReopenPolicy::new();
        policy.mark_pending(0);
        for _ in 0..=REOPEN_BACKOFF_SCHEDULE_MS.len() {
            policy.record_result(false, 0);
        }
        assert!(policy.is_exhausted());

        policy.mark_pending(10_000_000_000);
        assert!(
            !policy.is_exhausted(),
            "a new cycle must clear the give-up flag"
        );
        assert_eq!(policy.attempts(), 0);
        assert!(policy.is_due(10_000_000_000));
    }

    /// 未解決のまま追加の `DeviceUnavailable` イベントが積まれても
    /// (=同じ問題が続いているだけ)、進行中のバックオフを初期化し直さない。
    #[test]
    fn repeated_mark_pending_while_already_pending_does_not_reset_backoff() {
        let policy = ReopenPolicy::new();
        policy.mark_pending(0);
        policy.record_result(false, 0);
        assert_eq!(policy.attempts(), 1);
        let earliest_before = {
            // まだ due ではないはずの時刻で確認する。
            assert!(!policy.is_due(0));
            REOPEN_BACKOFF_SCHEDULE_MS[0] * 1_000_000
        };

        // 同じ問題を反映した追加のイベントが、待機の途中で観測される。
        policy.mark_pending(1);

        assert_eq!(
            policy.attempts(),
            1,
            "attempts must not reset while still pending"
        );
        assert!(!policy.is_due(earliest_before - 1));
        assert!(policy.is_due(earliest_before));
    }
}
