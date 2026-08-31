//! 出力コールバックのアンダーラン検知(初期構築仕様『§2』M3「アンダーラン検知・テレメトリ」)。
//!
//! # これは何を検知するか(`mw_core::Event::Underrun` との違い)
//!
//! `mw_core::Event::Underrun`(`mixer.rs::Mixer::report_underrun`)は**楽曲/BGM の
//! デコードリングバッファがデータ供給に追いつかず、`Mixer::render` が已むを得ず
//! 無音で埋めた**ことの検知であり、M2 で実装済み。あちらは「音声コールバックに
//! 渡された出力バッファは常に最後まで書き切られる」——つまり cpal/OS 側から見れば
//! 正常に完了したコールバックである(`crates/mw-core/src/renderer.rs` の
//! `render_writes_silence_when_nothing_is_playing` が示すとおり、`Renderer::render`
//! は常に `output` 全体を埋めて返る)。
//!
//! 本モジュールが検知するのはそれとは別の障害モード:**音声コールバック自体が
//! 想定より遅く/間隔が開いて呼ばれた**こと。これは OS 側のオーディオバッファが
//! 実際に枯渇した(音が途切れた)ことの直接的な兆候、またはその一歩手前の状態を示す
//! (初期構築仕様 M3 の「アンダーラン検知・テレメトリ」が指すのはこちら)。
//!
//! # なぜ「コールバック間隔」で検知するか(候補の検証結果)
//!
//! 依頼時点での候補を実際に調べた結果:
//!
//! 1. **cpal 自身のアンダーラン通知**(`cpal::ErrorKind::Xrun`, `err_fn` 経由)——
//!    cpal 0.18.1 のソース(`~/.cargo/registry/.../cpal-0.18.1/src/host/`)を全ホスト
//!    横断で確認したところ、`Xrun` を実際に生成しているのは `pulseaudio`/`alsa`/
//!    `jack`/`asio` の4ホストのみ。本プロジェクトが実際に使う3プラットフォーム
//!    (macOS Editor / iOS の `coreaudio` ホスト、Android の `aaudio` ホスト)は
//!    **どちらも `Xrun` を一度も生成しない**(`coreaudio/macos/device.rs` の
//!    レンダーコールバックはエラー分岐を持たず、`aaudio/mod.rs` は
//!    `stream.x_run_count()` を内部の動的バッファ調整にのみ使い、`error_callback`
//!    には流していない)。したがってこの経路は**実在しない**——採用できない。
//! 2. **出力バッファを埋めきれなかったこと**——`mw_core::Renderer::render`(＝
//!    `Mixer::render`)は必ず `output` を最後まで埋めて返る設計(足りないデータは
//!    無音で埋める、上記)。cpal 側のコールバッククロージャ
//!    (`cpal_backend::build_output_stream`)も `renderer.render(data, ..)` を
//!    呼ぶだけで早期リターンしない。つまり「バッファに穴が空いたまま返る」という
//!    事象はこの実装には**存在しない**——採用できない。
//! 3. **コールバック間隔が想定より大きく開いた**——唯一、実際に観測可能な信号。
//!    `cpal::OutputCallbackInfo::timestamp().callback`(このコールバックが呼ばれた
//!    ホスト単調時刻。`host_time.rs` の調査結果どおり `mach_absolute_time`/
//!    `clock_gettime(CLOCK_MONOTONIC)` を cpal と全く同じ式で読んだ値と等価)を
//!    毎コールバック記録し、直前コールバックが処理したフレーム数から見積もった
//!    「想定される次コールバックまでの間隔」と比較する。想定を大きく超えていれば、
//!    音声スレッドが実時間に追いつけなかった(スケジューリング遅延・処理過多等)
//!    ことを意味し、OS 側バッファが実際に枯渇した/枯渇しかけたと推定できる。
//!    これを本モジュールでは便宜上「アンダーラン(の疑い)」と呼ぶ——正確には
//!    「コールバック間隔異常」の検知であり、実際に聴感上のグリッチが起きたことを
//!    100% 保証するものではないヒューリスティックである(閾値の根拠は
//!    [`GAP_THRESHOLD_NUMERATOR`] のドキュメント参照)。
//!
//! # リアルタイム安全性(初期構築仕様 §5.3)
//!
//! [`OutputUnderrunTracker::observe`] は整数演算とアトミック操作のみで構成される
//! (ヒープ確保・ロック・IO 皆無)。`prev_host_time_ns`/`prev_frames`/`consecutive`
//! は音声コールバックスレッドが単独で所有する非アトミックフィールド(単一の書き手、
//! `Arc`/`Mutex` 不要)。ゲームスレッドが読む3値だけを `Arc` の atomic として
//! 公開する——`cpal_backend::CpalBackend` の `callback_frames`/`output_latency_ns`
//! と同じ設計(モジュール doc・`CpalBackend` のフィールド doc参照)。
//!
//! # なぜプロセス全体で共有される `static` にしなかったか
//!
//! `crates/mw-core/tests/realtime_safety.rs` のモジュール doc「CI フレークの真因」
//! (`e08eec4`)の教訓どおり、プロセス全体で共有される状態は複数インスタンス・
//! 複数テストの並列実行で混ざる。[`OutputUnderrunTracker`] が公開用に持つ3本の
//! `Arc<Atomic*>` は `CpalBackend::new()` のたびに新規生成され(`CpalBackend` の
//! 他のカウンタ〔`callback_frames` 等〕と全く同じ流儀)、`CpalBackend` インスタンス
//! ごとに独立する。テスト側も([`tests`] モジュール参照)`OutputUnderrunTracker`
//! を直接構築して観測系列を注入するため、テスト同士が状態を共有することもない。

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// 想定されるコールバック間隔に対する許容倍率の分子(【仮】)。
///
/// `GAP_THRESHOLD_NUMERATOR / GAP_THRESHOLD_DENOMINATOR` = 1.5倍。実測されている
/// コールバック間隔のジッタは `docs/measurement-m1.md` §11(iPhone 14 実機)で
/// 0.402ms 程度と非常に小さく、想定間隔(バッファ長ぶん、通常数msから数十ms)の
/// 1.5倍を正常な範囲で超えることは無い——超えた場合は実時間に追いつけなかった
/// (スケジューリング遅延・過負荷)可能性が高いと判断できる、という程度の
/// マージン。整数演算のみで済ませるため浮動小数点ではなく分数(分子/分母)で持つ。
pub const GAP_THRESHOLD_NUMERATOR: u64 = 3;
/// [`GAP_THRESHOLD_NUMERATOR`] の分母。
pub const GAP_THRESHOLD_DENOMINATOR: u64 = 2;

/// 出力コールバックの間隔異常(「アンダーラン(の疑い)」、モジュール doc参照)を
/// 検知し、ゲームスレッドから読める3値([`count`](Self::count)・
/// [`last_host_time_ns`](Self::last_host_time_ns)・
/// [`consecutive`](Self::consecutive))として公開する。
///
/// 音声コールバックスレッドが `observe` を毎コールバック1回だけ呼ぶ想定
/// (`cpal_backend::build_output_stream` のクロージャから使う)。`Arc` 越しに
/// 公開値を共有するだけで、この構造体自体は `Send` だが `Sync` である必要はない
/// (単一スレッド〔音声コールバック〕だけが所有・変更する。§5.3 の「単一の書き手」
/// 原則どおり)。
pub struct OutputUnderrunTracker {
    /// 直前コールバックのホスト単調時刻(ns)。0 は「まだ1度もコールバックが
    /// 無かった」ことを表す特殊値(実測上 0ns ちょうどになることはまず無いため、
    /// `CpalBackend::output_latency_ns` と同じ理由でこの特殊値を安全に使える)。
    prev_host_time_ns: u64,
    /// 直前コールバックが処理したフレーム数。「次のコールバックまでに想定される
    /// 間隔」の見積もりに使う(`prev_frames / sample_rate`)。
    prev_frames: u32,
    /// 直近まで連続して間隔異常を検知した回数(異常なしのコールバックで 0 に戻る)。
    consecutive: u32,
    /// ゲームスレッドへ公開する累計検知回数。
    count: Arc<AtomicU64>,
    /// ゲームスレッドへ公開する、直近に検知したコールバックのホスト単調時刻(ns)。
    /// まだ検知していなければ 0。
    last_host_time_ns: Arc<AtomicU64>,
    /// ゲームスレッドへ公開する [`consecutive`](Self::consecutive) の現在値。
    consecutive_published: Arc<AtomicU32>,
}

impl OutputUnderrunTracker {
    /// 新規トラッカーを構築する。渡す3本の `Arc` は呼び出し側(`CpalBackend`)が
    /// 所有し続け、ゲームスレッドからの読み出しに使う。
    pub fn new(
        count: Arc<AtomicU64>,
        last_host_time_ns: Arc<AtomicU64>,
        consecutive_published: Arc<AtomicU32>,
    ) -> Self {
        Self {
            prev_host_time_ns: 0,
            prev_frames: 0,
            consecutive: 0,
            count,
            last_host_time_ns,
            consecutive_published,
        }
    }

    /// 今回のコールバックを観測する。**音声コールバックスレッドから毎回呼ぶこと**
    /// (§5.3: ヒープ確保・ロック・IO を一切行わない)。
    ///
    /// `host_time_ns` はこのコールバックが呼ばれたホスト単調時刻
    /// (`cpal::OutputCallbackInfo::timestamp().callback` を ns 化した値)、
    /// `frames` はこのコールバックが処理するフレーム数、`sample_rate` は
    /// ネゴシエート済みの出力サンプルレート。
    pub fn observe(&mut self, host_time_ns: u64, frames: u32, sample_rate: u32) {
        let detected = self.prev_host_time_ns != 0 && self.prev_frames > 0 && sample_rate > 0 && {
            let expected_ns = (self.prev_frames as u64 * 1_000_000_000) / sample_rate as u64;
            let elapsed_ns = host_time_ns.saturating_sub(self.prev_host_time_ns);
            let threshold_ns =
                expected_ns.saturating_mul(GAP_THRESHOLD_NUMERATOR) / GAP_THRESHOLD_DENOMINATOR;
            expected_ns > 0 && elapsed_ns > threshold_ns
        };

        self.prev_host_time_ns = host_time_ns;
        self.prev_frames = frames;

        if detected {
            self.consecutive = self.consecutive.saturating_add(1);
            self.count.fetch_add(1, Ordering::Relaxed);
            self.last_host_time_ns
                .store(host_time_ns, Ordering::Relaxed);
        } else {
            self.consecutive = 0;
        }
        self.consecutive_published
            .store(self.consecutive, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// テスト用の3本組(トラッカーと、ゲームスレッド側が読む想定の `Arc` 複製)。
    fn tracker_with_handles() -> (
        OutputUnderrunTracker,
        Arc<AtomicU64>,
        Arc<AtomicU64>,
        Arc<AtomicU32>,
    ) {
        let count = Arc::new(AtomicU64::new(0));
        let last_host_time_ns = Arc::new(AtomicU64::new(0));
        let consecutive = Arc::new(AtomicU32::new(0));
        let tracker = OutputUnderrunTracker::new(
            Arc::clone(&count),
            Arc::clone(&last_host_time_ns),
            Arc::clone(&consecutive),
        );
        (tracker, count, last_host_time_ns, consecutive)
    }

    /// 「モックのバックエンド」に相当する人工的なコールバック系列: `CpalBackend` /
    /// 実デバイス無しで `OutputUnderrunTracker::observe` を直接駆動し、規則正しい
    /// 間隔のコールバック列の途中に1回だけ大きな間隔(想定の3倍)を混ぜて
    /// アンダーラン(の疑い)を人工的に起こす。
    #[test]
    fn a_large_gap_between_callbacks_increments_the_counter() {
        let (mut tracker, count, last_host_time_ns, consecutive) = tracker_with_handles();
        let sample_rate = 48_000u32;
        let frames = 512u32;
        let nominal_gap_ns = frames as u64 * 1_000_000_000 / sample_rate as u64;

        // 1 から始める(0 は「未観測」の特殊値。`OutputUnderrunTracker::observe` の
        // ドキュメント参照)。
        let mut host_time_ns = 1u64;
        // 正常な間隔のコールバックを何回か(検知されないこと)。
        for _ in 0..5 {
            tracker.observe(host_time_ns, frames, sample_rate);
            host_time_ns += nominal_gap_ns;
        }
        assert_eq!(count.load(Ordering::Relaxed), 0);
        assert_eq!(consecutive.load(Ordering::Relaxed), 0);

        // 想定間隔の3倍空けて呼ぶ(閾値1.5倍を明確に超える)。
        host_time_ns += nominal_gap_ns * 3;
        tracker.observe(host_time_ns, frames, sample_rate);

        assert_eq!(count.load(Ordering::Relaxed), 1);
        assert_eq!(consecutive.load(Ordering::Relaxed), 1);
        assert_eq!(last_host_time_ns.load(Ordering::Relaxed), host_time_ns);

        // 直後に正常な間隔へ戻れば consecutive は 0 に戻るが、累計は 1 のまま。
        host_time_ns += nominal_gap_ns;
        tracker.observe(host_time_ns, frames, sample_rate);
        assert_eq!(count.load(Ordering::Relaxed), 1);
        assert_eq!(consecutive.load(Ordering::Relaxed), 0);
    }

    /// 起きていないとき(常に規則正しい間隔)はカウンタが 0 のまま。
    #[test]
    fn regular_callback_cadence_never_increments_the_counter() {
        let (mut tracker, count, last_host_time_ns, consecutive) = tracker_with_handles();
        let sample_rate = 44_100u32;
        let frames = 256u32;
        let nominal_gap_ns = frames as u64 * 1_000_000_000 / sample_rate as u64;

        let mut host_time_ns = 1u64;
        for _ in 0..500 {
            tracker.observe(host_time_ns, frames, sample_rate);
            host_time_ns += nominal_gap_ns;
        }

        assert_eq!(count.load(Ordering::Relaxed), 0);
        assert_eq!(last_host_time_ns.load(Ordering::Relaxed), 0);
        assert_eq!(consecutive.load(Ordering::Relaxed), 0);
    }

    /// 連続して間隔異常が続いた場合、`consecutive` が積み上がること。
    #[test]
    fn consecutive_gaps_accumulate_until_a_normal_callback_resets_it() {
        let (mut tracker, count, _last_host_time_ns, consecutive) = tracker_with_handles();
        let sample_rate = 48_000u32;
        let frames = 512u32;
        let nominal_gap_ns = frames as u64 * 1_000_000_000 / sample_rate as u64;

        // 1 から始める(0 は「未観測」の特殊値なので、種付けの1回目にも使わない——
        // `OutputUnderrunTracker::observe` のドキュメント参照)。
        let mut host_time_ns = 1u64;
        tracker.observe(host_time_ns, frames, sample_rate); // seed(検知なし)

        for i in 1..=3u64 {
            host_time_ns += nominal_gap_ns * 3;
            tracker.observe(host_time_ns, frames, sample_rate);
            assert_eq!(count.load(Ordering::Relaxed), i);
            assert_eq!(consecutive.load(Ordering::Relaxed), i as u32);
        }

        host_time_ns += nominal_gap_ns;
        tracker.observe(host_time_ns, frames, sample_rate);
        assert_eq!(count.load(Ordering::Relaxed), 3);
        assert_eq!(consecutive.load(Ordering::Relaxed), 0);
    }

    /// 最初のコールバック(「直前」が無い)では検知しない——`prev_host_time_ns == 0`
    /// を「未観測」の特殊値として扱うため。
    #[test]
    fn first_callback_ever_is_never_flagged() {
        let (mut tracker, count, _last, _consecutive) = tracker_with_handles();
        tracker.observe(1, 512, 48_000);
        assert_eq!(count.load(Ordering::Relaxed), 0);
    }

    /// サンプルレート 0(未確定)では除算を避け、検知しない。
    #[test]
    fn zero_sample_rate_does_not_panic_or_detect() {
        let (mut tracker, count, _last, _consecutive) = tracker_with_handles();
        tracker.observe(1_000_000, 512, 0);
        tracker.observe(2_000_000, 512, 0);
        assert_eq!(count.load(Ordering::Relaxed), 0);
    }

    /// ホスト時刻が逆転しても(理論上ほぼ起こらないが)パニックしない。
    #[test]
    fn backwards_host_time_does_not_panic() {
        let (mut tracker, _count, _last, _consecutive) = tracker_with_handles();
        tracker.observe(1_000_000, 512, 48_000);
        tracker.observe(500_000, 512, 48_000); // 逆転
    }
}
