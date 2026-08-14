//! サンプル単位の線形ランプ(初期構築仕様 M13, §4.1, 確定)。
//!
//! 「すべての音量変化・停止はランプ(短いフェード)を通す」を実現する最小のプリミティブ。
//! バス音量・ボイス音量・停止・スティールのすべてがこれを経由する。
//!
//! リアルタイム安全性(§5.3): アロケーション・ロック・パニック経路を一切持たない。
//! `advance()` は音声コールバックのサンプルごとのホットパスから呼ばれる想定
//! (`Iterator::next` と紛らわしくなるため意図的にこの名前にしている)。

/// 現在値から目標値へ、指定サンプル数かけて線形に遷移する値。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ramp {
    current: f32,
    target: f32,
    remaining_samples: u32,
    step: f32,
}

impl Ramp {
    /// ランプ無し(即値)で初期化する。ボイスの発音開始音量などに使う
    /// (初期構築仕様 §4.2: 発音開始そのものはランプの対象ではない。対象は
    /// 「変化」と「停止」のみ、M13)。
    pub const fn new(initial: f32) -> Self {
        Self {
            current: initial,
            target: initial,
            remaining_samples: 0,
            step: 0.0,
        }
    }

    /// 現在値。
    pub fn value(&self) -> f32 {
        self.current
    }

    /// 目標値。
    pub fn target(&self) -> f32 {
        self.target
    }

    /// ランプが完了している(現在値 == 目標値に到達済み)か。
    pub fn is_settled(&self) -> bool {
        self.remaining_samples == 0
    }

    /// 値を即座に書き換える(ランプを経由しない)。テストや初期化専用。
    /// 音声コールバック経路からの音量「変化」には使わないこと(M13 違反になる)。
    pub fn set_immediate(&mut self, value: f32) {
        self.current = value;
        self.target = value;
        self.remaining_samples = 0;
        self.step = 0.0;
    }

    /// `ramp_samples` サンプルかけて目標値まで線形に遷移するよう設定する。
    /// `ramp_samples == 0` の場合は即値遷移(ランプ無し)として扱う。
    pub fn set_target(&mut self, target: f32, ramp_samples: u32) {
        if ramp_samples == 0 {
            self.set_immediate(target);
            return;
        }
        self.target = target;
        self.remaining_samples = ramp_samples;
        self.step = (target - self.current) / ramp_samples as f32;
    }

    /// 1サンプル分進めて現在値を返す。
    ///
    /// リアルタイム安全: アロケーション・パニック経路無し。
    /// 最終サンプルでは浮動小数点誤差の蓄積を避けるため目標値を厳密に代入する
    /// (テストでのサンプル単位の期待値照合を成立させるための不変条件)。
    pub fn advance(&mut self) -> f32 {
        if self.remaining_samples == 0 {
            return self.current;
        }
        self.remaining_samples -= 1;
        if self.remaining_samples == 0 {
            self.current = self.target;
        } else {
            self.current += self.step;
        }
        self.current
    }
}

/// ミリ秒 → サンプル数の変換(初期構築仕様 §4.1: 「目標値+時間、ブロック内はサンプル単位の
/// 線形ランプ」)。`ms <= 0.0` は即値(0 サンプル)。丸めにより 0 になってしまう極小の
/// 正の ms は 1 サンプルへ切り上げ、「ランプを指定したのに即値になる」事故を避ける。
pub fn ms_to_samples(ms: f32, sample_rate: u32) -> u32 {
    if ms <= 0.0 || sample_rate == 0 {
        return 0;
    }
    let samples = (ms / 1000.0 * sample_rate as f32).round() as u32;
    samples.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_has_no_pending_ramp() {
        let ramp = Ramp::new(0.5);
        assert_eq!(ramp.value(), 0.5);
        assert!(ramp.is_settled());
    }

    #[test]
    fn linear_ramp_reaches_target_exactly_after_n_samples() {
        let mut ramp = Ramp::new(0.0);
        ramp.set_target(1.0, 4);
        let values: Vec<f32> = (0..4).map(|_| ramp.advance()).collect();
        assert_eq!(values, vec![0.25, 0.5, 0.75, 1.0]);
        assert!(ramp.is_settled());
        // ランプ完了後は目標値に張り付いたまま推移する。
        assert_eq!(ramp.advance(), 1.0);
    }

    #[test]
    fn zero_duration_ramp_is_immediate() {
        let mut ramp = Ramp::new(0.2);
        ramp.set_target(0.9, 0);
        assert!(ramp.is_settled());
        assert_eq!(ramp.advance(), 0.9);
    }

    #[test]
    fn ramp_to_zero_never_jumps_discontinuously() {
        let mut ramp = Ramp::new(1.0);
        ramp.set_target(0.0, 8);
        let mut prev = 1.0;
        for _ in 0..8 {
            let v = ramp.advance();
            assert!(
                v <= prev,
                "ramp toward 0 must be monotonically non-increasing"
            );
            assert!(
                (prev - v).abs() <= 0.125 + f32::EPSILON,
                "step must not exceed 1/8"
            );
            prev = v;
        }
        assert_eq!(prev, 0.0);
    }

    #[test]
    fn ms_to_samples_default_five_ms_at_48k() {
        assert_eq!(ms_to_samples(5.0, 48_000), 240);
    }

    #[test]
    fn ms_to_samples_zero_is_immediate() {
        assert_eq!(ms_to_samples(0.0, 48_000), 0);
        assert_eq!(ms_to_samples(-1.0, 48_000), 0);
    }

    #[test]
    fn ms_to_samples_rounds_up_tiny_positive_durations() {
        // 極小の正の ms が丸めで 0 サンプルになり「即値」に化けてしまわないようにする。
        assert_eq!(ms_to_samples(0.001, 8_000), 1);
    }
}
