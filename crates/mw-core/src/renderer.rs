//! 音声コールバックから駆動される最上位レンダラ。
//!
//! 初期構築仕様 §5.2(確定): 音声スレッド(OS のオーディオコールバック)は
//! 「コマンド消化 → デコード済みリングバッファからミックス → 出力」を行う。
//! M0 では実際のミキサ・ボイス管理はまだ無く、**無音を書き込むだけ**だが、
//! 将来のミキサはこの [`Renderer::render`] の中身として実装される。
//!
//! 実装は §5.3 のリアルタイム安全性規約に従うこと:
//! ロック取得・ヒープアロケーション/デアロケーション・ファイル/ネットワーク IO・
//! パニック経路(`unwrap`/`expect`/添字パニック)を禁止する。

use crate::clock::RenderedFrameCounter;
use crate::format::CHANNELS;

/// mw-core の最上位レンダラ。
///
/// `mw-backend` の `Backend` 実装がオーディオコールバックから
/// [`Renderer::render`] を呼び、インターリーブ済み f32 ステレオバッファを埋める。
#[derive(Debug, Default)]
pub struct Renderer {
    frame_counter: RenderedFrameCounter,
}

impl Renderer {
    pub const fn new() -> Self {
        Self {
            frame_counter: RenderedFrameCounter::new(),
        }
    }

    /// `output` はインターリーブされた f32 ステレオバッファ(`len` は `frames * CHANNELS`)。
    ///
    /// # リアルタイム安全性
    ///
    /// この関数はオーディオコールバックから直接呼ばれる想定。
    /// ロック取得・ヒープアロケーション・ブロッキング IO・パニック経路は禁止(§5.3)。
    /// M0 では無音を書き込むのみ(将来: ミキサ・ボイス管理・バス処理をここに実装する)。
    pub fn render(&self, output: &mut [f32]) {
        output.fill(0.0);

        // `output.len()` が CHANNELS の倍数でない(バックエンドが壊れたバッファ長を渡した)
        // 場合でも、整数除算で切り捨てるだけでパニックはしない。
        let frames = (output.len() / CHANNELS) as u64;
        self.frame_counter.add(frames);
    }

    /// これまでにレンダリングした総フレーム数(音楽クロックの土台。§4.4)。
    pub fn rendered_frames(&self) -> u64 {
        self.frame_counter.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_writes_silence() {
        let renderer = Renderer::new();
        let mut buffer = vec![1.0_f32; 256 * CHANNELS];
        renderer.render(&mut buffer);
        assert!(buffer.iter().all(|&sample| sample == 0.0));
    }

    #[test]
    fn render_advances_frame_counter_by_frame_count_not_sample_count() {
        let renderer = Renderer::new();
        let mut buffer = vec![0.0_f32; 128 * CHANNELS];
        renderer.render(&mut buffer);
        assert_eq!(renderer.rendered_frames(), 128);
    }

    #[test]
    fn render_accumulates_across_multiple_callbacks() {
        let renderer = Renderer::new();
        let mut buffer = vec![0.0_f32; 64 * CHANNELS];
        for _ in 0..10 {
            renderer.render(&mut buffer);
        }
        assert_eq!(renderer.rendered_frames(), 640);
    }

    #[test]
    fn render_handles_empty_buffer_without_panicking() {
        let renderer = Renderer::new();
        let mut buffer: Vec<f32> = Vec::new();
        renderer.render(&mut buffer);
        assert_eq!(renderer.rendered_frames(), 0);
    }
}
