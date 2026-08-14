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
}

impl Config {
    /// 初期構築仕様 §4.2/§4.1 の【仮】既定値。
    pub const DEFAULT_MAX_VOICES: usize = 64;
    pub const DEFAULT_RAMP_MS: f32 = 5.0;
    pub const DEFAULT_COMMAND_QUEUE_CAPACITY: usize = 256;
    pub const DEFAULT_CLIPPER_THRESHOLD: f32 = 1.0;
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
    }
}
