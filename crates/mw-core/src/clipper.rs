//! Master 段のソフトクリッパ(初期構築仕様 §4.1, 【仮】)。
//!
//! 「通常運用でクリッパが動作しないゲインステージングを既定とし、動作した場合は
//! 開発ビルドでイベント(§4.6)として観測できるようにする」。動作検知は内部カウンタで行い
//! (M1)、実際の `ClipperEngaged` イベント化は `mixer.rs::Mixer::render` が
//! このコールバック前後のカウンタ差分を見て行う(M2-6)。
//!
//! 設計: 閾値以下は完全に透過(線形、無変化)。閾値を超えた分だけ滑らかに飽和させる
//! (`excess / (1 + excess)` で `[閾値, 閾値+1)` へ写像する連続関数)。ハードクリップの
//! ような矩形波化(= 折り返し歪み)を避けつつ、通常運用域の数値検証は素通しのまま保てる。

use std::sync::atomic::{AtomicU64, Ordering};

/// Master 段のソフトクリッパ。
#[derive(Debug, Default)]
pub struct SoftClipper {
    threshold: f32,
    engaged_count: AtomicU64,
}

impl SoftClipper {
    pub const fn new(threshold: f32) -> Self {
        Self {
            threshold,
            engaged_count: AtomicU64::new(0),
        }
    }

    /// 1サンプル分をソフトクリップする。閾値以下は無変化(線形領域)。
    ///
    /// リアルタイム安全: アロケーション・ロック無し(`AtomicU64::fetch_add` のみ)。
    pub fn process(&self, x: f32) -> f32 {
        let ax = x.abs();
        if ax <= self.threshold {
            return x;
        }
        self.engaged_count.fetch_add(1, Ordering::Relaxed);
        let sign = x.signum();
        let excess = ax - self.threshold;
        let saturated = self.threshold + excess / (1.0 + excess);
        sign * saturated
    }

    /// ステレオ1フレーム分をソフトクリップする。
    pub fn process_stereo(&self, l: f32, r: f32) -> (f32, f32) {
        (self.process(l), self.process(r))
    }

    /// クリッパが実際に動作(閾値超過)した回数。開発ビルドでの動作検知に使う
    /// (`ClipperEngaged` イベントへの昇格は `mixer.rs::Mixer::render` 側、M2-6)。
    pub fn engaged_count(&self) -> u64 {
        self.engaged_count.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_through_unchanged_below_threshold() {
        let clipper = SoftClipper::new(1.0);
        assert_eq!(clipper.process(0.5), 0.5);
        assert_eq!(clipper.process(-0.9), -0.9);
        assert_eq!(clipper.process(1.0), 1.0);
        assert_eq!(clipper.engaged_count(), 0);
    }

    #[test]
    fn saturates_smoothly_above_threshold_and_counts_engagement() {
        let clipper = SoftClipper::new(1.0);
        let y = clipper.process(2.0);
        // excess = 1.0 -> saturated = 1.0 + 1.0/2.0 = 1.5
        assert!((y - 1.5).abs() < 1e-6);
        assert_eq!(clipper.engaged_count(), 1);

        let y_neg = clipper.process(-3.0);
        // excess = 2.0 -> saturated = 1.0 + 2.0/3.0
        assert!((y_neg - -(1.0 + 2.0 / 3.0)).abs() < 1e-6);
        assert_eq!(clipper.engaged_count(), 2);
    }

    #[test]
    fn never_exceeds_threshold_plus_one_asymptote() {
        let clipper = SoftClipper::new(1.0);
        for magnitude in [10.0_f32, 100.0, 1_000.0, 1_000_000.0] {
            let y = clipper.process(magnitude);
            assert!(
                y < 2.0,
                "soft clipper must approach but never reach the +1 asymptote above threshold"
            );
        }
    }

    #[test]
    fn is_continuous_at_the_threshold_boundary() {
        let clipper = SoftClipper::new(1.0);
        let just_below = clipper.process(0.999_999);
        let at = clipper.process(1.0);
        let just_above = clipper.process(1.000_001);
        assert!((just_below - at).abs() < 1e-5);
        assert!((just_above - at).abs() < 1e-5);
    }
}
