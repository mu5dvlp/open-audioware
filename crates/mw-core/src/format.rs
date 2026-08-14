//! 内部ミックスフォーマットの定義。
//!
//! 初期構築仕様 M12(【仮】): ミックスは **f32 ステレオ固定**で行い、サンプルレートは
//! 出力デバイスに追従する(素材は 48kHz 推奨、不一致はリサンプルで吸収)。
//! この定数を変更すればフォーマット前提を1箇所で変えられるようにしておく。

/// 内部ミックスのチャンネル数(ステレオ固定)。
pub const CHANNELS: usize = 2;

/// 内部ミックスのサンプル型。
pub type Sample = f32;

/// 出力フォーマットの記述。サンプルレートは実行時(出力デバイスオープン時)に確定する。
///
/// チャンネル数は [`CHANNELS`] で固定のためここには含めない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFormat {
    pub sample_rate: u32,
}

impl AudioFormat {
    pub fn new(sample_rate: u32) -> Self {
        Self { sample_rate }
    }

    /// 指定フレーム数に対応するインターリーブ済みバッファのサンプル数(要素数)。
    pub fn samples_for_frames(&self, frames: usize) -> usize {
        frames * CHANNELS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channels_is_stereo() {
        assert_eq!(CHANNELS, 2);
    }

    #[test]
    fn samples_for_frames_multiplies_by_channel_count() {
        let format = AudioFormat::new(48_000);
        assert_eq!(format.samples_for_frames(0), 0);
        assert_eq!(format.samples_for_frames(1), 2);
        assert_eq!(format.samples_for_frames(256), 512);
    }
}
