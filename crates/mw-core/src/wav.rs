//! wav(RIFF/PCM)ローダ(初期構築仕様 M12, §4.7)。
//!
//! 対応: PCM 8/16/24/32bit、IEEE float 32/64bit、およびそれらの
//! WAVE_FORMAT_EXTENSIBLE 版 / ステレオ・モノラルの wav。モノは等パワーで両ch展開する。
//! サンプルレートは**出力デバイスのレートに一致しなくてよい** —
//! `decode` に渡された `output_sample_rate` と異なる場合、ロード時に一括で
//! `resample.rs::resample_oneshot`(rubato `Async` sinc)へ通して出力レート化する
//! (初期構築仕様『§4.7』: 「SE はロード時に全デコード + 必要ならロード時に
//! リサンプルして出力レート化(再生時コストゼロ)」)。一致する場合はリサンプラを
//! 構築すらしない(レート一致時に無用な計算を持ち込まない)。
//!
//! パーサは自前実装(外部クレートへ依存しない。§1 の実装方針: 「パーサは自前実装か、
//! 依存を足すなら deny.toml のライセンス allow と整合させること」)。
//! ゲームスレッド上でのロード時にのみ呼ばれ、音声コールバック経路(§5.3)からは
//! 呼ばれない — ここでのアロケーション(出力 PCM バッファ、リサンプル)は許容される。

use std::fmt;
use std::sync::Arc;

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
    /// 対応していない WAV フォーマットタグ。
    UnsupportedFormatTag(u16),
    /// 対応していない WAVE_FORMAT_EXTENSIBLE の SubFormat GUID。
    UnsupportedExtensibleSubFormat,
    /// フォーマットに対して対応していないビット深度。
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
            WavError::UnsupportedFormatTag(tag) => write!(f, "unsupported wav format tag {tag}"),
            WavError::UnsupportedExtensibleSubFormat => {
                write!(f, "unsupported WAVE_FORMAT_EXTENSIBLE SubFormat GUID")
            }
            WavError::UnsupportedBitsPerSample(bits) => {
                write!(f, "unsupported bits-per-sample {bits}")
            }
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

#[cfg(test)]
mod display_tests {
    use super::*;

    /// `Resample` 以外の各バリアントのログ識別語を列挙する。
    /// `Resample` は内側の `ResampleError` へ委譲するため、重複検査から除外する。
    /// ワイルドカード無しの match により、バリアント追加時はこのテストも更新が必要になる。
    fn describe(error: &WavError) -> Option<&'static str> {
        match error {
            WavError::Truncated => Some("wav data is truncated"),
            WavError::NotRiff => Some("not a RIFF file"),
            WavError::NotWave => Some("not a WAVE file"),
            WavError::MissingFmtChunk => Some("missing the 'fmt ' chunk"),
            WavError::MissingDataChunk => Some("missing the 'data' chunk"),
            WavError::UnsupportedFormatTag(_) => Some("unsupported wav format tag"),
            WavError::UnsupportedExtensibleSubFormat => {
                Some("unsupported WAVE_FORMAT_EXTENSIBLE SubFormat GUID")
            }
            WavError::UnsupportedBitsPerSample(_) => Some("unsupported bits-per-sample"),
            WavError::UnsupportedChannelCount(_) => Some("unsupported channel count"),
            WavError::InvalidSampleRate(_) => Some("invalid sample rate"),
            WavError::Resample(_) => None,
        }
    }

    #[test]
    fn wav_error_display_identifies_every_outer_variant_without_duplicates() {
        let errors = [
            WavError::Truncated,
            WavError::NotRiff,
            WavError::NotWave,
            WavError::MissingFmtChunk,
            WavError::MissingDataChunk,
            WavError::UnsupportedFormatTag(3),
            WavError::UnsupportedExtensibleSubFormat,
            WavError::UnsupportedBitsPerSample(24),
            WavError::UnsupportedChannelCount(5),
            WavError::InvalidSampleRate(0),
            // This variant delegates its complete message to the inner error.
            WavError::Resample(ResampleError::Processing("buffer mismatch".into())),
        ];

        let outer_messages: Vec<String> = errors
            .iter()
            .filter_map(|error| {
                let message = error.to_string();
                let key = describe(error)?;
                assert!(
                    message.contains(key),
                    "{error:?} must retain its identifying phrase: {message}"
                );
                Some(message)
            })
            .collect();

        assert!(
            outer_messages
                .iter()
                .enumerate()
                .all(|(index, message)| outer_messages
                    .iter()
                    .enumerate()
                    .all(|(other_index, other)| index == other_index || message != other)),
            "each outer WavError variant must have a distinct display message: {outer_messages:?}"
        );
        // 引数つきのバリアントは、**引数そのものが文言に出ていること**も見る ——
        // 出し忘れると実機のログから「何が駄目だったのか」が分からなくなる。
        // 🔴 添字ではなく `match` で取り出すこと。添字で書くと、上の配列の順番を
        // 変えたときに**別のバリアントを検査して偶然通る**(数字が他の文言に
        // たまたま含まれる)ようになり、検査が黙って無意味になる。
        for error in &errors {
            let expected_argument = match error {
                WavError::UnsupportedFormatTag(tag) => Some(tag.to_string()),
                WavError::UnsupportedBitsPerSample(bits) => Some(bits.to_string()),
                WavError::UnsupportedChannelCount(channels) => Some(channels.to_string()),
                WavError::InvalidSampleRate(rate) => Some(rate.to_string()),
                WavError::Truncated
                | WavError::NotRiff
                | WavError::NotWave
                | WavError::MissingFmtChunk
                | WavError::MissingDataChunk
                | WavError::UnsupportedExtensibleSubFormat
                | WavError::Resample(_) => None,
            };
            let Some(argument) = expected_argument else {
                continue;
            };
            let message = error.to_string();
            assert!(
                message.contains(&argument),
                "{error:?} must print its argument ({argument}) so the device log says \
                 what was actually wrong: {message}"
            );
        }

        // 委譲するバリアントは内側のエラーの文言をそのまま出す
        // (これが上の重複検査から除外してある理由)。
        assert_eq!(
            WavError::Resample(ResampleError::Processing("buffer mismatch".into())).to_string(),
            "resampling failed: buffer mismatch",
            "WavError::Resample は内側の ResampleError へ丸ごと委譲する"
        );
    }
}

/// モノ→ステレオ展開の等パワー係数(1/sqrt(2))。両chへ同一係数を掛けることで、
/// 合成パワーが元のモノラル信号のパワーと一致する(初期構築仕様 M12)。
const EQUAL_POWER_GAIN: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// wav バイト列を f32 ステレオ PCM ([`SoundData`])へデコードする。
///
/// `output_sample_rate` は出力(デバイス)側のサンプルレート。wav のサンプルレートと
/// 一致しない場合はロード時に一括でリサンプルする(モジュール doc)。
pub fn decode(bytes: &[u8], output_sample_rate: u32) -> Result<SoundData, WavError> {
    let mut reader = WavStreamReader::open(bytes.to_vec())?;
    let frames = reader.total_frames() as usize;
    let mut interleaved = vec![0.0; frames * CHANNELS];
    let decoded_frames = reader.read(&mut interleaved);
    interleaved.truncate(decoded_frames * CHANNELS);

    // レートが一致する場合はリサンプラを構築すらしない。
    if reader.sample_rate() == output_sample_rate {
        return Ok(SoundData {
            sample_rate: reader.sample_rate(),
            frames: decoded_frames,
            interleaved,
        });
    }
    let (resampled, resampled_frames) = resample::resample_oneshot(
        &interleaved,
        decoded_frames,
        reader.sample_rate(),
        output_sample_rate,
    )
    .map_err(WavError::Resample)?;
    Ok(SoundData {
        sample_rate: output_sample_rate,
        frames: resampled_frames,
        interleaved: resampled,
    })
}

const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xfffe;

const PCM_SUBFORMAT_GUID: [u8; 16] = [
    0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71,
];
const IEEE_FLOAT_SUBFORMAT_GUID: [u8; 16] = [
    0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71,
];

#[derive(Clone, Copy)]
enum SampleFormat {
    Pcm { bits_per_sample: u16 },
    IeeeFloat { bits_per_sample: u16 },
}

impl SampleFormat {
    fn bits_per_sample(self) -> u16 {
        match self {
            Self::Pcm { bits_per_sample } | Self::IeeeFloat { bits_per_sample } => bits_per_sample,
        }
    }

    fn bytes_per_sample(self) -> usize {
        usize::from(self.bits_per_sample() / 8)
    }
}

/// WAV の各サンプルを f32 へ変換する唯一の処理箇所。
fn sample_to_f32(format: SampleFormat, bytes: &[u8]) -> Option<f32> {
    match format {
        SampleFormat::Pcm { bits_per_sample: 8 } => {
            Some((f32::from(*bytes.first()?) - 128.0) / 128.0)
        }
        SampleFormat::Pcm {
            bits_per_sample: 16,
        } => {
            let bytes = bytes.get(..2)?;
            // 既存の PCM16 出力を維持するため、従来と同じ式を使う。
            Some(i16::from_le_bytes([bytes[0], bytes[1]]) as f32 / 32_768.0)
        }
        SampleFormat::Pcm {
            bits_per_sample: 24,
        } => {
            let bytes = bytes.get(..3)?;
            let raw = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0]);
            let signed = if raw & 0x0080_0000 != 0 {
                (raw | 0xff00_0000) as i32
            } else {
                raw as i32
            };
            Some(signed as f32 / 8_388_608.0)
        }
        SampleFormat::Pcm {
            bits_per_sample: 32,
        } => {
            let bytes = bytes.get(..4)?;
            Some(
                i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as f32
                    / 2_147_483_648.0,
            )
        }
        SampleFormat::IeeeFloat {
            bits_per_sample: 32,
        } => {
            let bytes = bytes.get(..4)?;
            Some(f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
        }
        SampleFormat::IeeeFloat {
            bits_per_sample: 64,
        } => {
            let bytes = bytes.get(..8)?;
            Some(f64::from_le_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
            ]) as f32)
        }
        SampleFormat::Pcm { .. } | SampleFormat::IeeeFloat { .. } => None,
    }
}

/// ヘッダ解析後の WAV 形式情報と data チャンクの位置。
#[derive(Clone, Copy)]
struct WavHeader {
    channels: u16,
    sample_rate: u32,
    sample_format: SampleFormat,
    data_offset: usize,
    data_len: usize,
}

/// WAV のヘッダを解析し、データ本体をコピーせずに位置だけを返す。
fn parse_header(bytes: &[u8]) -> Result<WavHeader, WavError> {
    let mut cursor = Cursor::new(bytes);

    if cursor.remaining() < 12 {
        return Err(WavError::Truncated);
    }
    if cursor.take(4).ok_or(WavError::Truncated)? != b"RIFF" {
        return Err(WavError::NotRiff);
    }
    let _riff_size = cursor.read_u32_le().ok_or(WavError::Truncated)?;
    if cursor.take(4).ok_or(WavError::Truncated)? != b"WAVE" {
        return Err(WavError::NotWave);
    }

    let mut format_tag: Option<u16> = None;
    let mut channels: Option<u16> = None;
    let mut sample_rate: Option<u32> = None;
    let mut bits_per_sample: Option<u16> = None;
    let mut extensible_sub_format: Option<[u8; 16]> = None;
    let mut data: Option<(usize, usize)> = None;

    // チャンクを順に走査する。未知のチャンクはサイズぶんスキップする
    // (RIFF はチャンクを word(偶数バイト)境界に揃えるため、奇数サイズは 1 バイトの
    // パディングを読み飛ばす)。
    while cursor.remaining() >= 8 {
        let chunk_id = cursor.take(4).ok_or(WavError::Truncated)?;
        let chunk_size = cursor.read_u32_le().ok_or(WavError::Truncated)? as usize;
        let chunk_offset = cursor.position();
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
                if format_tag == Some(WAVE_FORMAT_EXTENSIBLE) {
                    let cb_size = fmt_cursor.read_u16_le().ok_or(WavError::MissingFmtChunk)?;
                    if cb_size < 22 || fmt_cursor.remaining() < 22 {
                        return Err(WavError::MissingFmtChunk);
                    }
                    let _valid_bits_per_sample =
                        fmt_cursor.read_u16_le().ok_or(WavError::MissingFmtChunk)?;
                    let _channel_mask =
                        fmt_cursor.read_u32_le().ok_or(WavError::MissingFmtChunk)?;
                    let guid = fmt_cursor.take(16).ok_or(WavError::MissingFmtChunk)?;
                    let mut sub_format = [0u8; 16];
                    sub_format.copy_from_slice(guid);
                    extensible_sub_format = Some(sub_format);
                }
            }
            b"data" => data = Some((chunk_offset, chunk_size)),
            _ => {
                // 既知でないチャンク(LIST/fact 等)は読み飛ばす。
            }
        }

        if chunk_size % 2 == 1 {
            let _ = cursor.take(1);
        }
    }

    let format_tag = format_tag.ok_or(WavError::MissingFmtChunk)?;
    let channels = channels.ok_or(WavError::MissingFmtChunk)?;
    let sample_rate = sample_rate.ok_or(WavError::MissingFmtChunk)?;
    let bits_per_sample = bits_per_sample.ok_or(WavError::MissingFmtChunk)?;
    let (data_offset, data_len) = data.ok_or(WavError::MissingDataChunk)?;

    let sample_format = match format_tag {
        WAVE_FORMAT_PCM => SampleFormat::Pcm { bits_per_sample },
        WAVE_FORMAT_IEEE_FLOAT => SampleFormat::IeeeFloat { bits_per_sample },
        WAVE_FORMAT_EXTENSIBLE => match extensible_sub_format {
            Some(guid) if guid == PCM_SUBFORMAT_GUID => SampleFormat::Pcm { bits_per_sample },
            Some(guid) if guid == IEEE_FLOAT_SUBFORMAT_GUID => {
                SampleFormat::IeeeFloat { bits_per_sample }
            }
            _ => return Err(WavError::UnsupportedExtensibleSubFormat),
        },
        tag => return Err(WavError::UnsupportedFormatTag(tag)),
    };
    let supported_bits = match sample_format {
        SampleFormat::Pcm { .. } => matches!(bits_per_sample, 8 | 16 | 24 | 32),
        SampleFormat::IeeeFloat { .. } => matches!(bits_per_sample, 32 | 64),
    };
    if !supported_bits {
        return Err(WavError::UnsupportedBitsPerSample(bits_per_sample));
    }
    if channels != 1 && channels != 2 {
        return Err(WavError::UnsupportedChannelCount(channels));
    }
    if sample_rate == 0 {
        return Err(WavError::InvalidSampleRate(sample_rate));
    }

    Ok(WavHeader {
        channels,
        sample_rate,
        sample_format,
        data_offset,
        data_len,
    })
}

/// ヘッダ解析済みの WAV ストリーミングリーダー。
///
/// バイト列を所有するが、data チャンクの PCM 全体を f32 へ展開することはせず、
/// `read` の要求範囲だけを変換する。SE の一括ロードもこのリーダーを最後まで読む
/// ことで同じヘッダ解析・サンプル変換を共有する。
pub(crate) struct WavStreamReader {
    bytes: Arc<Vec<u8>>,
    channels: u16,
    sample_rate: u32,
    sample_format: SampleFormat,
    data_offset: usize,
    frame_size: usize,
    total_frames: u64,
    position: u64,
}

impl WavStreamReader {
    pub(crate) fn open(bytes: Vec<u8>) -> Result<Self, WavError> {
        Self::open_shared(Arc::new(bytes))
    }

    pub(crate) fn open_shared(bytes: Arc<Vec<u8>>) -> Result<Self, WavError> {
        let header = parse_header(&bytes)?;
        let bytes_per_sample = header.sample_format.bytes_per_sample();
        let frame_size = bytes_per_sample * header.channels as usize;
        let frames = header.data_len.checked_div(frame_size).unwrap_or(0);
        Ok(Self {
            bytes,
            channels: header.channels,
            sample_rate: header.sample_rate,
            sample_format: header.sample_format,
            data_offset: header.data_offset,
            frame_size,
            total_frames: frames as u64,
            position: 0,
        })
    }

    pub(crate) fn read(&mut self, out: &mut [f32]) -> usize {
        let want_frames = out.len() / CHANNELS;
        let remaining = self.total_frames.saturating_sub(self.position) as usize;
        let frames = want_frames.min(remaining);
        let data = self.bytes.as_slice();
        let mut decoded_frames = 0;

        for frame in 0..frames {
            let Some(frame_offset) = (self.position as usize)
                .checked_add(frame)
                .and_then(|index| index.checked_mul(self.frame_size))
                .and_then(|offset| self.data_offset.checked_add(offset))
            else {
                break;
            };
            if self.channels == 2 {
                let bytes_per_sample = self.sample_format.bytes_per_sample();
                let Some(left) = data.get(frame_offset..frame_offset + bytes_per_sample) else {
                    break;
                };
                let right_offset = frame_offset + bytes_per_sample;
                let Some(right) = data.get(right_offset..right_offset + bytes_per_sample) else {
                    break;
                };
                let Some(left) = sample_to_f32(self.sample_format, left) else {
                    break;
                };
                let Some(right) = sample_to_f32(self.sample_format, right) else {
                    break;
                };
                out[frame * CHANNELS] = left;
                out[frame * CHANNELS + 1] = right;
            } else {
                let bytes_per_sample = self.sample_format.bytes_per_sample();
                let Some(mono) = data.get(frame_offset..frame_offset + bytes_per_sample) else {
                    break;
                };
                let Some(sample) = sample_to_f32(self.sample_format, mono) else {
                    break;
                };
                let sample = sample * EQUAL_POWER_GAIN;
                out[frame * CHANNELS] = sample;
                out[frame * CHANNELS + 1] = sample;
            }
            decoded_frames += 1;
        }
        self.position += decoded_frames as u64;
        decoded_frames
    }

    pub(crate) fn seek(&mut self, frame: u64) {
        self.position = frame.min(self.total_frames);
    }

    pub(crate) fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub(crate) fn total_frames(&self) -> u64 {
        self.total_frames
    }
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

    fn position(&self) -> usize {
        self.pos
    }

    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        if self.remaining() < len {
            return None;
        }
        let end = self.pos.checked_add(len)?;
        let slice = self.bytes.get(self.pos..end)?;
        self.pos = end;
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

    use super::{
        IEEE_FLOAT_SUBFORMAT_GUID, PCM_SUBFORMAT_GUID, WAVE_FORMAT_EXTENSIBLE,
        WAVE_FORMAT_IEEE_FLOAT, WAVE_FORMAT_PCM,
    };

    fn make_wav(
        sample_rate: u32,
        channels: u16,
        bits_per_sample: u16,
        format_tag: u16,
        data_bytes: &[u8],
        sub_format: Option<[u8; 16]>,
    ) -> Vec<u8> {
        let bytes_per_sample = u32::from(bits_per_sample / 8);
        let block_align = bytes_per_sample as u16 * channels;
        let byte_rate = sample_rate * u32::from(block_align);
        let data_size = data_bytes.len() as u32;
        let fmt_size: u32 = if sub_format.is_some() { 40 } else { 16 };
        let riff_size = 4 /* WAVE */ + (8 + fmt_size) + (8 + data_size);

        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&riff_size.to_le_bytes());
        out.extend_from_slice(b"WAVE");

        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&fmt_size.to_le_bytes());
        out.extend_from_slice(&format_tag.to_le_bytes());
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&sample_rate.to_le_bytes());
        out.extend_from_slice(&byte_rate.to_le_bytes());
        out.extend_from_slice(&block_align.to_le_bytes());
        out.extend_from_slice(&bits_per_sample.to_le_bytes());
        if let Some(sub_format) = sub_format {
            out.extend_from_slice(&22u16.to_le_bytes()); // cbSize
            out.extend_from_slice(&bits_per_sample.to_le_bytes()); // valid bits
            out.extend_from_slice(&0u32.to_le_bytes()); // channel mask (unused by decoder)
            out.extend_from_slice(&sub_format);
        }

        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_size.to_le_bytes());
        out.extend_from_slice(data_bytes);

        out
    }

    /// 既知の PCM16 サンプル列から最小限の wav(RIFF/PCM)バイト列を組み立てる。
    pub fn make_pcm16_wav(sample_rate: u32, channels: u16, samples: &[i16]) -> Vec<u8> {
        let data_bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        make_wav(
            sample_rate,
            channels,
            16,
            WAVE_FORMAT_PCM,
            &data_bytes,
            None,
        )
    }

    /// PCM8(符号なし) wav を組み立てる。
    pub fn make_pcm8_wav(sample_rate: u32, channels: u16, samples: &[u8]) -> Vec<u8> {
        make_wav(sample_rate, channels, 8, WAVE_FORMAT_PCM, samples, None)
    }

    /// PCM24 サンプル列を組み立てる。各値の下位24bitが little-endian で格納される。
    pub fn make_pcm24_wav(sample_rate: u32, channels: u16, samples: &[i32]) -> Vec<u8> {
        let data_bytes: Vec<u8> = samples
            .iter()
            .flat_map(|sample| {
                let bytes = sample.to_le_bytes();
                [bytes[0], bytes[1], bytes[2]]
            })
            .collect();
        make_wav(
            sample_rate,
            channels,
            24,
            WAVE_FORMAT_PCM,
            &data_bytes,
            None,
        )
    }

    /// PCM32 サンプル列を組み立てる。
    pub fn make_pcm32_wav(sample_rate: u32, channels: u16, samples: &[i32]) -> Vec<u8> {
        let data_bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        make_wav(
            sample_rate,
            channels,
            32,
            WAVE_FORMAT_PCM,
            &data_bytes,
            None,
        )
    }

    /// IEEE float32 サンプル列を組み立てる。
    pub fn make_float32_wav(sample_rate: u32, channels: u16, samples: &[f32]) -> Vec<u8> {
        let data_bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        make_wav(
            sample_rate,
            channels,
            32,
            WAVE_FORMAT_IEEE_FLOAT,
            &data_bytes,
            None,
        )
    }

    /// IEEE float64 サンプル列を組み立てる。
    pub fn make_float64_wav(sample_rate: u32, channels: u16, samples: &[f64]) -> Vec<u8> {
        let data_bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        make_wav(
            sample_rate,
            channels,
            64,
            WAVE_FORMAT_IEEE_FLOAT,
            &data_bytes,
            None,
        )
    }

    /// WAVE_FORMAT_EXTENSIBLE の PCM16 wav を組み立てる。
    pub fn make_extensible_pcm16_wav(sample_rate: u32, channels: u16, samples: &[i16]) -> Vec<u8> {
        let data_bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        make_wav(
            sample_rate,
            channels,
            16,
            WAVE_FORMAT_EXTENSIBLE,
            &data_bytes,
            Some(PCM_SUBFORMAT_GUID),
        )
    }

    /// WAVE_FORMAT_EXTENSIBLE の IEEE float32 wav を組み立てる。
    pub fn make_extensible_float32_wav(
        sample_rate: u32,
        channels: u16,
        samples: &[f32],
    ) -> Vec<u8> {
        let data_bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        make_wav(
            sample_rate,
            channels,
            32,
            WAVE_FORMAT_EXTENSIBLE,
            &data_bytes,
            Some(IEEE_FLOAT_SUBFORMAT_GUID),
        )
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
        let mut bytes = make_pcm8_wav(48_000, 1, &[128, 200, 10]);
        bytes[34..36].copy_from_slice(&7u16.to_le_bytes());
        let err = decode(&bytes, 48_000).unwrap_err();
        assert_eq!(err, WavError::UnsupportedBitsPerSample(7));
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

    fn assert_samples_are_close(name: &str, actual: &[f32], expected: &[f32]) {
        assert_eq!(actual.len(), expected.len(), "{name}: sample count differs");
        for (index, (&actual, &expected)) in actual.iter().zip(expected).enumerate() {
            assert!(
                (actual - expected).abs() <= 1e-6,
                "{name}: sample {index} differs: actual={actual:?}, expected={expected:?}"
            );
        }
    }

    #[test]
    fn decodes_all_supported_sample_formats_in_bulk_and_streaming_paths() {
        let cases = [
            (
                "PCM8",
                make_pcm8_wav(48_000, 2, &[0, 255, 128, 64]),
                vec![-1.0, 127.0 / 128.0, 0.0, -64.0 / 128.0],
            ),
            (
                "PCM16",
                make_pcm16_wav(48_000, 2, &[-32_768, 32_767, 16_384, -16_384]),
                vec![-1.0, 32_767.0 / 32_768.0, 0.5, -0.5],
            ),
            (
                "PCM24",
                make_pcm24_wav(48_000, 2, &[-8_388_608, 8_388_607, 1, -1]),
                vec![
                    -1.0,
                    8_388_607.0 / 8_388_608.0,
                    1.0 / 8_388_608.0,
                    -1.0 / 8_388_608.0,
                ],
            ),
            (
                "PCM32",
                make_pcm32_wav(48_000, 2, &[i32::MIN, i32::MAX, 1, -1]),
                vec![
                    -1.0,
                    i32::MAX as f32 / 2_147_483_648.0,
                    1.0 / 2_147_483_648.0,
                    -1.0 / 2_147_483_648.0,
                ],
            ),
            (
                "IEEE float32",
                make_float32_wav(48_000, 2, &[-1.25, 0.25, 0.5, -0.75]),
                vec![-1.25, 0.25, 0.5, -0.75],
            ),
            (
                "IEEE float64",
                make_float64_wav(48_000, 2, &[-1.25, 0.25, 0.5, -0.75]),
                vec![-1.25, 0.25, 0.5, -0.75],
            ),
            (
                "WAVE_FORMAT_EXTENSIBLE PCM",
                make_extensible_pcm16_wav(48_000, 2, &[-16_384, 16_384, 32_767, -32_768]),
                vec![-0.5, 0.5, 32_767.0 / 32_768.0, -1.0],
            ),
            (
                "WAVE_FORMAT_EXTENSIBLE IEEE float",
                make_extensible_float32_wav(48_000, 2, &[-0.25, 0.75, 0.5, -0.5]),
                vec![-0.25, 0.75, 0.5, -0.5],
            ),
        ];

        for (name, bytes, expected) in cases {
            let sound = decode(&bytes, 48_000)
                .unwrap_or_else(|error| panic!("{name}: bulk decode failed: {error}"));
            assert_samples_are_close(name, &sound.interleaved, &expected);

            let mut reader = WavStreamReader::open(bytes).expect("streaming reader must open");
            let mut streamed = vec![0.0; expected.len()];
            let frames = reader.read(&mut streamed);
            assert_eq!(
                frames,
                expected.len() / CHANNELS,
                "{name}: frame count differs"
            );
            assert_samples_are_close(name, &streamed, &expected);
        }
    }

    #[test]
    fn extensible_channel_mask_is_ignored() {
        let mut bytes = make_extensible_pcm16_wav(48_000, 2, &[16_384, -16_384]);
        // WAVE_FORMAT_EXTENSIBLE の channel mask は fmt body の byte 20..24。
        bytes[40..44].copy_from_slice(&0xffff_ffffu32.to_le_bytes());
        let sound =
            decode(&bytes, 48_000).expect("channel mask must not affect channel-count decoding");
        assert_samples_are_close(
            &sound.sample_rate.to_string(),
            &sound.interleaved,
            &[0.5, -0.5],
        );
    }

    #[test]
    fn seeking_is_sample_accurate_for_each_supported_bit_depth() {
        let cases = [
            (
                "PCM8",
                make_pcm8_wav(48_000, 2, &[0, 255, 128, 64, 32, 224]),
                [-0.75, 0.75],
            ),
            (
                "PCM16",
                make_pcm16_wav(
                    48_000,
                    2,
                    &[-32_768, 32_767, 16_384, -16_384, 8_192, -8_192],
                ),
                [0.25, -0.25],
            ),
            (
                "PCM24",
                make_pcm24_wav(
                    48_000,
                    2,
                    &[-8_388_608, 8_388_607, 1, -1, 2_097_152, -2_097_152],
                ),
                [0.25, -0.25],
            ),
            (
                "PCM32",
                make_pcm32_wav(
                    48_000,
                    2,
                    &[i32::MIN, i32::MAX, 1, -1, 536_870_912, -536_870_912],
                ),
                [0.25, -0.25],
            ),
            (
                "IEEE float32",
                make_float32_wav(48_000, 2, &[-1.0, 1.0, 0.125, -0.125, 0.5, -0.5]),
                [0.5, -0.5],
            ),
            (
                "IEEE float64",
                make_float64_wav(48_000, 2, &[-1.0, 1.0, 0.125, -0.125, 0.5, -0.5]),
                [0.5, -0.5],
            ),
        ];

        for (name, bytes, expected) in cases {
            let mut reader = WavStreamReader::open(bytes).expect("streaming reader must open");
            reader.seek(2);
            let mut out = [0.0; CHANNELS];
            assert_eq!(reader.read(&mut out), 1, "{name}: seeked frame is missing");
            assert_samples_are_close(name, &out, &expected);
        }
    }

    // --- ここから先はリサンプル(初期構築仕様『§4.7』)の検証 -------------------------

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
        // SE 側(wav.rs)も 48kHz 以外の wav が読めること。
        // あわせて周波数が保たれること・長さが正しいことも見る。
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
        // レート一致時のバイパス。SE 側でも一致時は
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
