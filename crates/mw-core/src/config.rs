//! mw-core の【仮】既定値を1箇所に集約する設定構造体。
//!
//! 初期構築仕様 凡例「【仮】」: 妥当な既定値として設定したもの。実装は進めてよいが、
//! 後から変わりうる前提で「1箇所を書き換えれば済む」構造にしておくこと。
//! ボイス数・ランプ長・キュー容量はすべてここに集約する。

/// mw-core の実行時設定。`mw_init(config)`(初期構築仕様 §5.5)から将来差し替え可能にする
/// 想定の置き場所(M1 時点では FFI からの上書きは未実装。既定値のみを使う。M8 相当)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Config {
    /// 同時発音ボイス数(初期構築仕様 §4.2 M14 直下、M1: 既定 64【仮】)。
    pub max_voices: usize,
    /// ボイススティール時、盗まれた側のボイスをフェードアウトさせつつ生かしておく
    /// 「尾」スロットの本数【仮】。理論上同時に発生しうるスティール数(= `max_voices`)を
    /// 上限として確保しておけば枯渇しない。
    pub steal_tail_capacity: usize,
    /// すべての音量変化・停止が既定で通る短いランプの長さ(ms)。
    /// 初期構築仕様 M13(確定): 「すべての音量変化・停止はランプを通す」。
    /// 既定値 5ms は【仮】(§4.1)。
    pub default_ramp_ms: f32,
    /// ゲームスレッド → 音声スレッドのコマンドキュー容量(SPSC, rtrb)。【仮】。
    pub command_queue_capacity: usize,
    /// 音声スレッド → ゲームスレッドの回収キュー容量(SPSC, rtrb)。【仮】。
    /// `max_voices + steal_tail_capacity` を上回る余裕を持たせ、通常運用では
    /// 溢れない(= 音声スレッドでの Arc ドロップが発生しない)ようにする。
    pub reclaim_queue_capacity: usize,
    /// ソフトクリッパが線形領域からニー(saturation)へ切り替わる閾値(絶対値)。
    /// 初期構築仕様 §4.1: 「通常運用でクリッパが動作しないゲインステージングを既定とする」。
    pub clipper_threshold: f32,
    /// 楽曲ストリーミングのプリロール長(ms、`stream.rs`)。初期構築仕様『§4.3 楽曲再生』の
    /// 「準備(プリロール込み)」で、`is_ready` が真になるまでに最低限バッファへ
    /// 溜めておくべき量をここで指定する。
    ///
    /// 既定値 [`Config::DEFAULT_PREROLL_MS`] の根拠(【仮】): デコードは mw-core の外側
    /// (mw-ffi が回すデコードスレッド、初期構築仕様『§5.2 スレッドモデル』)が `pump()` を
    /// 呼ぶことで進むため、音声コールバックの周期とデコードスレッドの起床周期は独立している。
    /// 100ms は典型的な出力バッファ長(数〜十数 ms)の数倍にあたり、デコードスレッドが
    /// 1〜2周期分スケジューリングで遅延しても音声側が枯渇しないだけの余裕を持たせつつ、
    /// 選曲プレビュー等での体感開始遅延としても許容できる範囲として選んだ。
    /// 実測(M2 後半のドリフト・アンダーラン計測)で見直す。
    pub preroll_ms: f32,
    /// 予約発音(初期構築仕様『§4.5 スケジュール発音』)のソート済みキュー容量。【仮】。
    ///
    /// 対象は SE 予約(`mw_se_schedule`)専用(楽曲側は同時に1本しかないため単一スロットで
    /// 足り、キューを持たない。`mixer.rs::MusicSchedule` 参照)。既定値
    /// [`Config::DEFAULT_SCHEDULE_QUEUE_CAPACITY`] の根拠: 用途はメトロノーム/
    /// キャリブレーション用クリックのみ(初期構築仕様『§4.5』)で、ゲーム側は
    /// 通常「次の数拍ぶん」を先行してまとめて予約する程度(例: 4/4 拍子で8小節分でも
    /// 32 発)を想定している。それでもキューの実体は `(u64, ScheduledSe)` を並べた
    /// 固定長 `Vec` に過ぎず 32 件でも数百バイト程度なので、余裕を持たせても
    /// メモリ的な負担にはならない。溢れた場合は黙って捨てず
    /// `Mixer::se_schedule_overflow_count` で計測できるようにしてある
    /// (`clipper.rs` の動作回数カウントと同じ流儀)。
    pub schedule_queue_capacity: usize,
    /// イベント通知(初期構築仕様『§4.6 イベント通知』)のキュー容量。【仮】既定 64。
    ///
    /// `mw-core::event::EventQueue` の音声スレッド系列・非リアルタイム系列(`StreamError`
    /// 用)の両方がこの容量を共有する(`event.rs` モジュール doc 参照)。溢れた場合は
    /// 古いものから破棄し、破棄数をポーリング側へ報告する(黙って落とさない)。
    pub event_queue_capacity: usize,
    /// アンダーランの集約報告閾値(フレーム数)。【仮】。
    ///
    /// `Mixer::render` は `MusicRenderOutcome::underrun_frames` を毎コールバック
    /// そのままイベント化せず、連続するコールバックをまたいで蓄積する
    /// (`mixer.rs::Mixer::report_underrun` 参照)。デコードスレッドの遅延等で
    /// アンダーランが延々と続くケースを「収まるまで一切報告しない」ままにしないための
    /// 安全弁として、蓄積量がこの閾値に達したら収まっていなくても一度報告する。
    /// 既定値 [`Config::DEFAULT_UNDERRUN_REPORT_THRESHOLD_FRAMES`] はサンプルレート
    /// 非依存の固定フレーム数だが、48kHz 環境を基準に「約1秒ぶん」を選んだ
    /// (=最悪でも1秒に1回程度しかイベントを積まないため、固定容量64のキューを
    /// アンダーラン単体で溢れさせることは無い)。
    pub underrun_report_threshold_frames: u32,
}

impl Config {
    /// 初期構築仕様 §4.2/§4.1 の【仮】既定値。
    pub const DEFAULT_MAX_VOICES: usize = 64;
    pub const DEFAULT_RAMP_MS: f32 = 5.0;
    pub const DEFAULT_COMMAND_QUEUE_CAPACITY: usize = 256;
    pub const DEFAULT_CLIPPER_THRESHOLD: f32 = 1.0;
    /// 【仮】既定値。根拠は [`Config::preroll_ms`] のドキュメントを参照。
    pub const DEFAULT_PREROLL_MS: f32 = 100.0;
    /// 【仮】既定値。根拠は [`Config::schedule_queue_capacity`] のドキュメントを参照。
    pub const DEFAULT_SCHEDULE_QUEUE_CAPACITY: usize = 32;
    /// 【仮】既定値。初期構築仕様『§4.6』が明記する既定値そのもの。
    pub const DEFAULT_EVENT_QUEUE_CAPACITY: usize = 64;
    /// 【仮】既定値。根拠は [`Config::underrun_report_threshold_frames`] のドキュメントを参照。
    pub const DEFAULT_UNDERRUN_REPORT_THRESHOLD_FRAMES: u32 = 48_000;
}

impl Default for Config {
    fn default() -> Self {
        let max_voices = Self::DEFAULT_MAX_VOICES;
        let steal_tail_capacity = max_voices;
        Self {
            max_voices,
            steal_tail_capacity,
            default_ramp_ms: Self::DEFAULT_RAMP_MS,
            command_queue_capacity: Self::DEFAULT_COMMAND_QUEUE_CAPACITY,
            // 同時に「回収待ち」になりうる最大数(通常終了 + スティール尾)に余裕を掛けておく。
            reclaim_queue_capacity: (max_voices + steal_tail_capacity) * 2,
            clipper_threshold: Self::DEFAULT_CLIPPER_THRESHOLD,
            preroll_ms: Self::DEFAULT_PREROLL_MS,
            schedule_queue_capacity: Self::DEFAULT_SCHEDULE_QUEUE_CAPACITY,
            event_queue_capacity: Self::DEFAULT_EVENT_QUEUE_CAPACITY,
            underrun_report_threshold_frames: Self::DEFAULT_UNDERRUN_REPORT_THRESHOLD_FRAMES,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_matches_spec_defaults() {
        let config = Config::default();
        assert_eq!(config.max_voices, 64);
        assert_eq!(config.default_ramp_ms, 5.0);
        assert!(config.reclaim_queue_capacity > config.max_voices + config.steal_tail_capacity);
        assert_eq!(config.preroll_ms, 100.0);
        assert_eq!(config.schedule_queue_capacity, 32);
        assert_eq!(config.event_queue_capacity, 64);
        assert_eq!(config.underrun_report_threshold_frames, 48_000);
    }
}
