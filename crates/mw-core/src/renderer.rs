//! 音声コールバックから駆動される最上位レンダラ。
//!
//! 初期構築仕様 §5.2(確定): 音声スレッド(OS のオーディオコールバック)は
//! 「コマンド消化 → デコード済みリングバッファからミックス → 出力」を行う。
//! 実体([`crate::mixer::Mixer`])はミキサ・ボイスプール・バス・クリッパを持ち、
//! [`Renderer::render`] の中で §5.3 のリアルタイム安全性規約に従って駆動される。
//!
//! ## 所有権の設計(M0 からの変更点)
//!
//! M0 では `Arc<Renderer>` をゲームスレッドと音声スレッドの双方で共有していたが、
//! M1 ではミキサ状態(ボイスプール・バス・コマンドキューの受信側)は音声コールバック
//! スレッドの排他所有物とし、`Backend::open` へ**値渡し(ムーブ)**する設計に変更した。
//! ゲームスレッド側は [`crate::mixer::CommandSender`](コマンド送信)と
//! [`crate::mixer::ReclaimReceiver`](Arc 回収)という別ハンドル経由でのみ音声スレッドと
//! やり取りする。これにより、音声コールバック内で `Mutex`/`UnsafeCell` 等の
//! 追加の同期プリミティブが一切不要になる(単一の書き手のみが `&mut self` で触る)。

use crate::clock::RenderedFrameCounter;
use crate::config::Config;
use crate::format::CHANNELS;
use crate::mixer::{self, CommandSender, Mixer, ReclaimReceiver};

/// mw-core の最上位レンダラ。
///
/// `mw-backend` の `Backend` 実装がオーディオコールバックからこれを排他所有し、
/// [`Renderer::render`] を呼んでインターリーブ済み f32 ステレオバッファを埋める。
pub struct Renderer {
    mixer: Mixer,
    frame_counter: RenderedFrameCounter,
}

impl Renderer {
    /// `Renderer` と、ゲームスレッド側で使う2つのハンドル
    /// (コマンド送信 [`CommandSender`] / Arc 回収 [`ReclaimReceiver`])を構築する。
    ///
    /// `sample_rate` は出力デバイスが実際に開いたレート(バス/ボイスのランプの
    /// ミリ秒 → サンプル数換算に使う。§4.1)。デバイスオープン前は §4.7 推奨の
    /// 48kHz 等、暫定値を渡しておき、オープン後に [`Renderer::set_sample_rate`] で
    /// 確定させてよい(音声コールバックが動き出す前に呼ぶこと)。
    pub fn build(config: Config, sample_rate: u32) -> (Renderer, CommandSender, ReclaimReceiver) {
        let (mixer, sender, reclaim) = mixer::build(config, sample_rate);
        (
            Renderer {
                mixer,
                frame_counter: RenderedFrameCounter::new(),
            },
            sender,
            reclaim,
        )
    }

    /// `output` はインターリーブされた f32 ステレオバッファ(`len` は `frames * CHANNELS`)。
    ///
    /// # リアルタイム安全性
    ///
    /// この関数はオーディオコールバックから直接呼ばれる想定。
    /// ロック取得・ヒープアロケーション・ブロッキング IO・パニック経路は禁止(§5.3)。
    pub fn render(&mut self, output: &mut [f32]) {
        self.mixer.render(output);

        // `output.len()` が CHANNELS の倍数でない場合でも、整数除算で切り捨てるだけで
        // パニックはしない。
        let frames = (output.len() / CHANNELS) as u64;
        self.frame_counter.add(frames);
    }

    /// これまでにレンダリングした総フレーム数(音楽クロックの土台。§4.4。M2 で拡張)。
    pub fn rendered_frames(&self) -> u64 {
        self.frame_counter.get()
    }

    /// 出力デバイスのサンプルレートを確定させる。音声コールバックが動き出す前
    /// (`Backend::open` がストリームを `play()` する前)に一度だけ呼ぶこと。
    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        self.mixer.set_sample_rate(sample_rate);
    }

    pub fn sample_rate(&self) -> u32 {
        self.mixer.sample_rate()
    }

    /// Master 段のソフトクリッパが動作(閾値超過)した累計回数(§4.1 の動作検知)。
    pub fn clipper_engaged_count(&self) -> u64 {
        self.mixer.clipper_engaged_count()
    }

    /// 現在アクティブな(primary スロットの)ボイス数。テスト・診断用。
    pub fn active_voice_count(&self) -> usize {
        self.mixer.active_voice_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn renderer_for_test() -> Renderer {
        let (renderer, _sender, _reclaim) = Renderer::build(Config::default(), 48_000);
        renderer
    }

    #[test]
    fn render_writes_silence_when_nothing_is_playing() {
        let mut renderer = renderer_for_test();
        let mut buffer = vec![1.0_f32; 256 * CHANNELS];
        renderer.render(&mut buffer);
        assert!(buffer.iter().all(|&sample| sample == 0.0));
    }

    #[test]
    fn render_advances_frame_counter_by_frame_count_not_sample_count() {
        let mut renderer = renderer_for_test();
        let mut buffer = vec![0.0_f32; 128 * CHANNELS];
        renderer.render(&mut buffer);
        assert_eq!(renderer.rendered_frames(), 128);
    }

    #[test]
    fn render_accumulates_across_multiple_callbacks() {
        let mut renderer = renderer_for_test();
        let mut buffer = vec![0.0_f32; 64 * CHANNELS];
        for _ in 0..10 {
            renderer.render(&mut buffer);
        }
        assert_eq!(renderer.rendered_frames(), 640);
    }

    #[test]
    fn render_handles_empty_buffer_without_panicking() {
        let mut renderer = renderer_for_test();
        let mut buffer: Vec<f32> = Vec::new();
        renderer.render(&mut buffer);
        assert_eq!(renderer.rendered_frames(), 0);
    }

    #[test]
    fn render_handles_buffer_length_not_a_multiple_of_channel_count() {
        let mut renderer = renderer_for_test();
        let mut buffer = vec![0.0_f32; 5]; // 5 は CHANNELS(2) の倍数ではない
        renderer.render(&mut buffer);
        assert_eq!(renderer.rendered_frames(), 2);
    }
}
