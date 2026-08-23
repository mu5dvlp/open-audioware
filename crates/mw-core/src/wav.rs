//! wav(RIFF/PCM)ローダ(初期構築仕様 M12, §4.7)。
//!
//! 対応: 16bit / ステレオ・モノラルの PCM wav。モノは等パワーで両ch展開する。
//! サンプルレートは**出力デバイスのレートに一致しなくてよい** —
//! `decode` に渡された `output_sample_rate` と異なる場合、ロード時に一括で
//! `resample.rs::resample_oneshot`(rubato `SincFixedIn`)へ通して出力レート化する
//! (初期構築仕様『§4.7』: 「SE はロード時に全デコード + 必要ならロード時に
//! リサンプルして出力レート化(再生時コストゼロ)」)。一致する場合はリサンプラを
//! 構築すらしない(依頼書の設計判断4。無用な計算を持ち込まない)。
//!
//! パーサは自前実装(外部クレートへ依存しない。§1 の実装方針: 「パーサは自前実装か、
//! 依存を足すなら deny.toml のライセンス allow と整合させること」)。
//! ゲームスレッド上でのロード時にのみ呼ばれ、音声コールバック経路(§5.3)からは
//! 呼ばれない — ここでのアロケーション(出力 PCM バッファ、リサンプル)は許容される。

use std::fmt;

use crate::format::CHANNELS;
use crate::resample::{self, ResampleError};
use crate::sound::SoundData;

/// wav デコードで発生しうるエラー。
#[derive(Debug, Clone, PartialEq)]
pub enum WavError {
    /// 入力がそもそも短すぎて RIFF ヘッダすら読めない。
    Truncated,
    /// `RIFF` マジックが見つからない。
    NotRiff,
    /// RIFF の形式タグが `WAVE` でない。
    NotWave,
    /// `fmt ` チャンクが見つからない。
    MissingFmtChunk,
    /// `data` チャンクが見つからない。
    MissingDataChunk,
    /// PCM(フォーマットタグ 1)以外(拡張フォーマット等)。M1 は非対応。
    UnsupportedFormatTag(u16),
    /// 16bit PCM 以外(8bit / 24bit / 32bit float 等)。M1 は非対応。
    UnsupportedBitsPerSample(u16),
    /// モノラル・ステレオ以外のチャンネル数。
    UnsupportedChannelCount(u16),
    /// `fmt ` チャンクのサンプルレートが 0(壊れたファイル)。通常の入力では起こらない
    /// 防御的チェック(0 だとリサンプラの比が定義できず、ゼロ除算になってしまう)。
    InvalidSampleRate(u32),
    /// リサンプラの構築・変換処理そのものが失敗した(`resample.rs` 参照。
    /// 通常のサンプルレートの組み合わせでは起こらない異常系)。
    Resample(ResampleError),
}

impl fmt::Display for WavError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WavError::Truncated => write!(f, "wav data is truncated (cannot read RIFF header)"),
            WavError::NotRiff => write!(f, "not a RIFF file (missing 'RIFF' magic)"),
            WavError::NotWave => write!(f, "not a WAVE file (missing 'WAVE' format tag)"),
            WavError::MissingFmtChunk => write!(f, "wav is missing the 'fmt ' chunk"),
            WavError::MissingDataChunk => write!(f, "wav is missing the 'data' chunk"),
            WavError::UnsupportedFormatTag(tag) => write!(
                f,
                "unsupported wav format tag {tag} (only PCM = 1 is supported in M1)"
            ),
            WavError::UnsupportedBitsPerSample(bits) => write!(
                f,
                "unsupported bits-per-sample {bits} (only 16-bit PCM is supported in M1)"
            ),
            WavError::UnsupportedChannelCount(channels) => write!(
                f,
                "unsupported channel count {channels} (only mono or stereo is supported)"
            ),
            WavError::InvalidSampleRate(rate) => {
                write!(f, "invalid sample rate {rate} Hz (must be > 0)")
            }
            WavError::Resample(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for WavError {}

/// モノ→ステレオ展開の等パワー係数(1/sqrt(2))。両chへ同一係数を掛けることで、
/// 合成パワーが元のモノラル信号のパワーと一致する(初期構築仕様 M12)。
const EQUAL_POWER_GAIN: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// wav バイト列を f32 ステレオ PCM ([`SoundData`])へデコードする。
///
/// `output_sample_rate` は出力(デバイス)側のサンプルレート。wav のサンプルレートと
/// 一致しない場合はロード時に一括でリサンプルする(モジュール doc)。
pub fn decode(bytes: &[u8], output_sample_rate: u32) -> Result<SoundData, WavError> {
    let mut cursor = Cursor::new(bytes);

    if cursor.remaining() < 12 {
        return Err(WavError::Truncated);
    }
    let riff_magic = cursor.take(4).ok_or(WavError::Truncated)?;
    if riff_magic != b"RIFF" {
        return Err(WavError::NotRiff);
    }
    let _riff_size = cursor.read_u32_le().ok_or(WavError::Truncated)?;
    let wave_magic = cursor.take(4).ok_or(WavError::Truncated)?;
    if wave_magic != b"WAVE" {
        return Err(WavError::NotWave);
    }

    let mut format_tag: Option<u16> = None;
    let mut channels: Option<u16> = None;
    let mut sample_rate: Option<u32> = None;
    let mut bits_per_sample: Option<u16> = None;
    let mut data: Option<&[u8]> = None;

    // チャンクを順に走査する。未知のチャンクはサイズぶんスキップする
    // (RIFF はチャンクを word(偶数バイト)境界に揃えるため、奇数サイズは 1 バイトのパディングを読み飛ばす)。
    while cursor.remaining() >= 8 {
        let chunk_id = cursor.take(4).ok_or(WavError::Truncated)?;
        let chunk_size = cursor.read_u32_le().ok_or(WavError::Truncated)? as usize;
        let chunk_body = cursor.take(chunk_size).ok_or(WavError::Truncated)?;

        match chunk_id {
            b"fmt " => {
                let mut fmt_cursor = Cursor::new(chunk_body);
                format_tag = Some(fmt_cursor.read_u16_le().ok_or(WavError::MissingFmtChunk)?);
                channels = Some(fmt_cursor.read_u16_le().ok_or(WavError::MissingFmtChunk)?);
                sample_rate = Some(fmt_cursor.read_u32_le().ok_or(WavError::MissingFmtChunk)?);
                let _byte_rate = fmt_cursor.read_u32_le().ok_or(WavError::MissingFmtChunk)?;
                let _block_align = fmt_cursor.read_u16_le().ok_or(WavError::MissingFmtChunk)?;
                bits_per_sample = Some(fmt_cursor.read_u16_le().ok_or(WavError::MissingFmtChunk)?);
            }
            b"data" => {
                data = Some(chunk_body);
            }
            _ => {
                // 既知でないチャンク(LIST/fact 等)は読み飛ばす。
            }
        }

        // word 境界パディング。
        if chunk_size % 2 == 1 {
            let _ = cursor.take(1);
        }
    }

    let format_tag = format_tag.ok_or(WavError::MissingFmtChunk)?;
    let channels = channels.ok_or(WavError::MissingFmtChunk)?;
    let sample_rate = sample_rate.ok_or(WavError::MissingFmtChunk)?;
    let bits_per_sample = bits_per_sample.ok_or(WavError::MissingFmtChunk)?;
    let data = data.ok_or(WavError::MissingDataChunk)?;

    // WAVE_FORMAT_PCM = 1 のみ対応。拡張フォーマット(0xFFFE 等)は M1 非対応。
    if format_tag != 1 {
        return Err(WavError::UnsupportedFormatTag(format_tag));
    }
    if bits_per_sample != 16 {
        return Err(WavError::UnsupportedBitsPerSample(bits_per_sample));
    }
    if channels != 1 && channels != 2 {
        return Err(WavError::UnsupportedChannelCount(channels));
    }
    if sample_rate == 0 {
        return Err(WavError::InvalidSampleRate(sample_rate));
    }

    let bytes_per_sample = 2usize; // 16bit
    let frame_size = bytes_per_sample * channels as usize;
    // channels は上で 1/2 に検証済みのため frame_size は 0 にならないが、
    // ゼロ除算の可能性を型の上でも消しておく(パニック経路禁止の規約 §5.3 とも整合)
    let frames = data.len().checked_div(frame_size).unwrap_or(0);

    let mut interleaved = Vec::with_capacity(frames * CHANNELS);
    for frame_index in 0..frames {
        let base = frame_index * frame_size;
        if channels == 2 {
            let l = read_i16_le(data, base).ok_or(WavError::Truncated)?;
            let r = read_i16_le(data, base + bytes_per_sample).ok_or(WavError::Truncated)?;
            interleaved.push(i16_to_f32(l));
            interleaved.push(i16_to_f32(r));
        } else {
            let m = read_i16_le(data, base).ok_or(WavError::Truncated)?;
            let sample = i16_to_f32(m) * EQUAL_POWER_GAIN;
            interleaved.push(sample);
            interleaved.push(sample);
        }
    }

    // レートが一致する場合はリサンプラを構築すらしない(依頼書の設計判断4)。
    if sample_rate == output_sample_rate {
        return Ok(SoundData {
            sample_rate,
            frames,
            interleaved,
        });
    }
    let (resampled, resampled_frames) =
        resample::resample_oneshot(&interleaved, frames, sample_rate, output_sample_rate)
            .map_err(WavError::Resample)?;
    Ok(SoundData {
        sample_rate: output_sample_rate,
        frames: resampled_frames,
        interleaved: resampled,
    })
}

fn i16_to_f32(sample: i16) -> f32 {
    sample as f32 / 32_768.0
}

fn read_i16_le(bytes: &[u8], offset: usize) -> Option<i16> {
    let a = *bytes.get(offset)?;
    let b = *bytes.get(offset + 1)?;
    Some(i16::from_le_bytes([a, b]))
}

/// 極小のバイトカーソル(自前実装。外部クレート非依存)。
struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.pos)
    }

    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        if self.remaining() < len {
            return None;
        }
        let slice = self.bytes.get(self.pos..self.pos + len)?;
        self.pos += len;
        Some(slice)
    }

    fn read_u16_le(&mut self) -> Option<u16> {
        let bytes = self.take(2)?;
        Some(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn read_u32_le(&mut self) -> Option<u32> {
        let bytes = self.take(4)?;
        Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }
}

#[cfg(test)]
pub mod golden {
    //! テスト用の wav バイト列生成(ゴールデンテスト用。バイナリはコミットせずコードで生成する。
    //! 初期構築仕様 §8)。

    /// 既知の PCM16 サンプル列から最小限の wav(RIFF/PCM)バイト列を組み立てる。
    pub fn make_pcm16_wav(sample_rate: u32, channels: u16, samples: &[i16]) -> Vec<u8> {
        let bytes_per_sample = 2u32;
        let block_align = bytes_per_sample as u16 * channels;
        let byte_rate = sample_rate * block_align as u32;
        let data_bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let data_size = data_bytes.len() as u32;
        let fmt_size: u32 = 16;
        let riff_size = 4 /* WAVE */ + (8 + fmt_size) + (8 + data_size);

        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&riff_size.to_le_bytes());
        out.extend_from_slice(b"WAVE");

        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&fmt_size.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes()); // PCM
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&sample_rate.to_le_bytes());
        out.extend_from_slice(&byte_rate.to_le_bytes());
        out.extend_from_slice(&block_align.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample

        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_size.to_le_bytes());
        out.extend_from_slice(&data_bytes);

        out
    }

    /// 非対応ビット深度(8bit PCM)の wav を組み立てる(拒否テスト用)。
    pub fn make_pcm8_wav(sample_rate: u32, channels: u16, samples: &[u8]) -> Vec<u8> {
        let block_align = channels; // 1 byte/sample
        let byte_rate = sample_rate * block_align as u32;
        let data_size = samples.len() as u32;
        let fmt_size: u32 = 16;
        let riff_size = 4 + (8 + fmt_size) + (8 + data_size);

        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&riff_size.to_le_bytes());
        out.extend_from_slice(b"WAVE");

        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&fmt_size.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&sample_rate.to_le_bytes());
        out.extend_from_slice(&byte_rate.to_le_bytes());
        out.extend_from_slice(&block_align.to_le_bytes());
        out.extend_from_slice(&8u16.to_le_bytes());

        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_size.to_le_bytes());
        out.extend_from_slice(samples);

        out
    }
}

#[cfg(test)]
mod tests {
    use super::golden::*;
    use super::*;

    #[test]
    fn decodes_48k_stereo_pcm16() {
        // L: 0, 16384, -16384, 32767 / R: 同じ列を逆順で使い L≠R を確認する。
        let samples: Vec<i16> = vec![0, 0, 16384, -16384, -16384, 16384, 32767, -32768];
        let bytes = make_pcm16_wav(48_000, 2, &samples);
        let sound = decode(&bytes, 48_000).expect("valid 48k stereo pcm16 wav must decode");

        assert_eq!(sound.sample_rate, 48_000);
        assert_eq!(sound.frames, 4);
        assert_eq!(sound.interleaved.len(), 8);

        let (l0, r0) = sound.frame(0).unwrap();
        assert_eq!(l0, 0.0);
        assert_eq!(r0, 0.0);

        let (l1, r1) = sound.frame(1).unwrap();
        assert!((l1 - (16384.0 / 32768.0)).abs() < 1e-6);
        assert!((r1 - (-16384.0 / 32768.0)).abs() < 1e-6);

        let (l3, r3) = sound.frame(3).unwrap();
        assert!((l3 - (32767.0 / 32768.0)).abs() < 1e-6);
        assert_eq!(r3, -1.0); // -32768 / 32768 = -1.0 exactly
    }

    #[test]
    fn mono_expands_to_stereo_with_equal_power_gain() {
        let samples: Vec<i16> = vec![32767, -32768, 0];
        let bytes = make_pcm16_wav(48_000, 1, &samples);
        let sound = decode(&bytes, 48_000).expect("valid 48k mono pcm16 wav must decode");

        assert_eq!(sound.frames, 3);
        let expected_gain = std::f32::consts::FRAC_1_SQRT_2;

        let (l0, r0) = sound.frame(0).unwrap();
        assert_eq!(
            l0, r0,
            "mono source must expand identically to both channels"
        );
        assert!((l0 - (32767.0 / 32768.0) * expected_gain).abs() < 1e-6);

        // 等パワー: 展開後の (L^2 + R^2) は元のモノラルサンプルの2乗と一致する。
        let (l1, r1) = sound.frame(1).unwrap();
        let original = -32768.0_f32 / 32768.0;
        let expanded_power = l1 * l1 + r1 * r1;
        assert!((expanded_power - original * original).abs() < 1e-5);
    }

    #[test]
    fn rejects_zero_sample_rate() {
        let bytes = make_pcm16_wav(0, 2, &[0, 0]);
        let err = decode(&bytes, 48_000).unwrap_err();
        assert_eq!(err, WavError::InvalidSampleRate(0));
    }

    #[test]
    fn rejects_unsupported_bit_depth() {
        let bytes = make_pcm8_wav(48_000, 1, &[128, 200, 10]);
        let err = decode(&bytes, 48_000).unwrap_err();
        assert_eq!(err, WavError::UnsupportedBitsPerSample(8));
    }

    #[test]
    fn rejects_truncated_input() {
        let err = decode(&[0x52, 0x49], 48_000).unwrap_err();
        assert_eq!(err, WavError::Truncated);
    }

    #[test]
    fn rejects_missing_riff_magic() {
        let mut bytes = make_pcm16_wav(48_000, 1, &[0]);
        bytes[0] = b'X';
        let err = decode(&bytes, 48_000).unwrap_err();
        assert_eq!(err, WavError::NotRiff);
    }

    #[test]
    fn odd_sized_chunk_padding_is_skipped_correctly() {
        // 奇数サイズの未知チャンク(例: 'LIST' 相当)を fmt/data の前に挟んでもデコードできること。
        let samples: Vec<i16> = vec![100, -100];
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        let mut body = Vec::new();
        body.extend_from_slice(b"WAVE");
        // 奇数サイズの未知チャンク "junk" (3 bytes body + 1 padding byte)
        body.extend_from_slice(b"junk");
        body.extend_from_slice(&3u32.to_le_bytes());
        body.extend_from_slice(&[1, 2, 3, 0 /* padding */]);
        // fmt チャンク
        body.extend_from_slice(b"fmt ");
        body.extend_from_slice(&16u32.to_le_bytes());
        body.extend_from_slice(&1u16.to_le_bytes());
        body.extend_from_slice(&1u16.to_le_bytes()); // mono
        body.extend_from_slice(&48_000u32.to_le_bytes());
        body.extend_from_slice(&(48_000u32 * 2).to_le_bytes());
        body.extend_from_slice(&2u16.to_le_bytes());
        body.extend_from_slice(&16u16.to_le_bytes());
        // data チャンク
        let data_bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        body.extend_from_slice(b"data");
        body.extend_from_slice(&(data_bytes.len() as u32).to_le_bytes());
        body.extend_from_slice(&data_bytes);

        bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&body);

        let sound = decode(&bytes, 48_000)
            .expect("unknown odd-sized chunk must be skipped, not break parsing");
        assert_eq!(sound.frames, 2);
    }

    // --- ここから先はリサンプル(初期構築仕様『§4.7』, 依頼書『テスト』6)の検証 ---

    /// 指定周波数の正弦波(両ch同一)を PCM16 wav として合成する。
    fn make_sine_wave_wav(
        sample_rate: u32,
        freq_hz: f32,
        frames: usize,
        amplitude: i16,
    ) -> Vec<u8> {
        let mut samples = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            let t = i as f32 / sample_rate as f32;
            let v = (amplitude as f32 * (2.0 * std::f32::consts::PI * freq_hz * t).sin()) as i16;
            samples.push(v);
            samples.push(v);
        }
        make_pcm16_wav(sample_rate, 2, &samples)
    }

    fn estimate_frequency_hz(left_channel: &[f32], sample_rate: u32) -> f32 {
        let margin = left_channel.len() / 20;
        let core = &left_channel[margin..left_channel.len() - margin];
        let mut crossings = 0usize;
        for w in core.windows(2) {
            if (w[0] <= 0.0 && w[1] > 0.0) || (w[0] >= 0.0 && w[1] < 0.0) {
                crossings += 1;
            }
        }
        let cycles = crossings as f32 / 2.0;
        let duration_s = core.len() as f32 / sample_rate as f32;
        cycles / duration_s
    }

    #[test]
    fn decodes_a_non_48k_wav_by_resampling_to_the_output_rate() {
        // 依頼書『テスト』6: 「SE 側(wav.rs)も 48kHz 以外の wav が読めるようになること」。
        // あわせて『テスト』1・2(周波数が保たれること・長さが正しいこと)も見る。
        const SOURCE_RATE: u32 = 44_100;
        const OUTPUT_RATE: u32 = 48_000;
        const FREQ_HZ: f32 = 1_000.0;
        const FRAME_COUNT: usize = 4_410; // 100ms

        let bytes = make_sine_wave_wav(SOURCE_RATE, FREQ_HZ, FRAME_COUNT, 20_000);
        let sound =
            decode(&bytes, OUTPUT_RATE).expect("non-48k wav must now decode via resampling");

        assert_eq!(sound.sample_rate, OUTPUT_RATE);
        let expected_frames =
            crate::resample::convert_frame_count(FRAME_COUNT as u64, SOURCE_RATE, OUTPUT_RATE);
        assert_eq!(sound.frames as u64, expected_frames);
        assert_eq!(sound.interleaved.len(), sound.frames * CHANNELS);

        let left: Vec<f32> = (0..sound.frames)
            .map(|i| sound.frame(i).unwrap().0)
            .collect();
        let estimated = estimate_frequency_hz(&left, OUTPUT_RATE);
        assert!(
            (estimated - FREQ_HZ).abs() / FREQ_HZ < 0.02,
            "resampled SE frequency should stay close to {FREQ_HZ} Hz, got {estimated} Hz"
        );
    }

    #[test]
    fn bypasses_resampling_when_rates_already_match() {
        // 依頼書『テスト』4: 「レート一致時のバイパス」。SE 側でも一致時は
        // リサンプラの丸め誤差を持ち込まず、素材の PCM がそのまま出てくることを見る。
        let samples: Vec<i16> = vec![0, 0, 16384, -16384, 32767, -32768];
        let bytes = make_pcm16_wav(48_000, 2, &samples);
        let sound = decode(&bytes, 48_000).expect("valid 48k wav must decode");

        assert_eq!(sound.frames, 3);
        let (l1, r1) = sound.frame(1).unwrap();
        assert_eq!(l1, 16384.0 / 32768.0);
        assert_eq!(r1, -16384.0 / 32768.0);
        let (l2, r2) = sound.frame(2).unwrap();
        assert_eq!(l2, 32767.0 / 32768.0);
        assert_eq!(r2, -1.0);
    }
}
