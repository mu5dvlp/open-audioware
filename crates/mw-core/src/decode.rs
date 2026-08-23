//! 楽曲のストリーミングデコード(初期構築仕様『§4.7 デコードとリサンプリング』, M7)。
//!
//! Symphonia を使い、メモリ上のバイト列(wav / ogg vorbis)を同期的にデコードする。
//! **ファイル IO は一切持ち込まない**(入力は常にメモリ上のバイト列)。出力は
//! `crate::format::CHANNELS` に固定した f32 インターリーブ・ステレオで、モノラル素材は
//! `wav.rs` と同じ等パワー(`1/√2`)で両ch展開する。
//!
//! ここで行うのは**同期デコードのみ**。リングバッファへの供給・スレッド分離は
//! `stream.rs` の責務であり、このモジュールはスレッドや `pump` の概念を一切知らない
//! (`MusicDecoder` トレイトを満たす、ただの同期デコーダ)。
//!
//! # リサンプルは範囲外
//!
//! 出力(デバイス)サンプルレートと素材のレートが一致しない場合は
//! [`DecodeError::UnsupportedSampleRate`] を返す。`wav.rs` の 48kHz 固定チェックと同じ流儀で、
//! リサンプルは後続作業(rubato)で対応予定であり、ここでは実装しない。
//!
//! # シークとサンプル精度
//!
//! Symphonia の `FormatReader::seek` はコンテナのパケット境界までしか位置決めできない
//! (初期構築仕様『§4.7』: 「ogg のシークはサンプル精度でない」)。wav の PCM は
//! パケット境界がバイト精度と一致するため実質サンプル精度になるが、ogg vorbis は
//! パケット(vorbis のオーディオパケット)境界までしか戻せないことがある。
//! [`SymphoniaDecoder::seek`] はシーク後に `actual_ts`(実際に着地した位置)と
//! 要求位置の差分だけデコード結果を読み捨て、常にサンプル境界ちょうどへ合わせ込む。
//! この読み捨てはコーデックによらず同じコードパスを通るため、wav でも常に検証できる
//! (テストでこの精度を固定化する)。

use std::fmt;
use std::io::Cursor;

use symphonia::core::audio::{SampleBuffer, SignalSpec};
use symphonia::core::codecs::{CODEC_TYPE_NULL, Decoder, DecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::format::CHANNELS;

/// モノ→ステレオ展開の等パワー係数(1/√2)。`wav.rs` の `EQUAL_POWER_GAIN` と同じ根拠
/// (両chへ同一係数を掛けることで合成パワーが元のモノラル信号のパワーと一致する。
/// 初期構築仕様 M12)。モジュールを跨いで公開する意味は薄いため、ここでも独立して定義する。
const EQUAL_POWER_GAIN: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// パケット単位のデコード結果を受け取る `SampleBuffer` の初期容量(フレーム数)。
/// 実際のパケットがこれより大きければその場で確保し直す([`grow_sample_buf`])ため、
/// この値は「たいていのケースで再確保が起きない」程度の目安でしかない。
const INITIAL_SAMPLE_BUF_FRAMES: u64 = 4096;

/// デコードで発生しうるエラー(`wav.rs::WavError` と同じ設計方針: 具体的なバリアントで返す)。
#[derive(Debug)]
pub enum DecodeError {
    /// フォーマット検出・デコーダ生成・パケット読み出し等、Symphonia 内部で発生したエラー。
    /// Symphonia のエラー型はそのままでは `Clone`/`PartialEq` にできないため文字列化して保持する。
    Symphonia(String),
    /// 対応コーデック(wav / ogg vorbis)の音声トラックが見つからなかった。
    NoAudioTrack,
    /// モノラル・ステレオ以外のチャンネル数。
    UnsupportedChannelCount(usize),
    /// 出力(デバイス)サンプルレートと素材のレートが一致しない。
    /// リサンプルは後続作業(rubato)で対応予定(`wav.rs::WavError::UnsupportedSampleRate` と同じ流儀)。
    UnsupportedSampleRate { found: u32, expected: u32 },
    /// トラック構成の再検出が必要になるケース(chained ogg 物理ストリーム等)。
    /// 初期構築仕様『§4.7』のスコープ外のため非対応として扱う。
    ResetRequired,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Symphonia(msg) => write!(f, "symphonia decode error: {msg}"),
            DecodeError::NoAudioTrack => {
                write!(f, "no supported audio track found (wav / ogg vorbis only)")
            }
            DecodeError::UnsupportedChannelCount(channels) => write!(
                f,
                "unsupported channel count {channels} (only mono or stereo is supported)"
            ),
            DecodeError::UnsupportedSampleRate { found, expected } => write!(
                f,
                "unsupported sample rate {found} Hz (expected {expected} Hz); \
                 resampling is planned for a later task (rubato) but not implemented yet"
            ),
            DecodeError::ResetRequired => write!(
                f,
                "stream requires a decoder reset (e.g. chained ogg physical streams), \
                 which is not supported yet"
            ),
        }
    }
}

impl std::error::Error for DecodeError {}

impl From<SymphoniaError> for DecodeError {
    fn from(err: SymphoniaError) -> Self {
        match err {
            SymphoniaError::ResetRequired => DecodeError::ResetRequired,
            other => DecodeError::Symphonia(other.to_string()),
        }
    }
}

/// `stream.rs::MusicStreamProducer::pump` に渡す、同期デコーダの最小契約。
///
/// 本番実装は [`SymphoniaDecoder`]。テストではこの trait を満たすフェイクを使うことで、
/// リングバッファ・エポック調停のロジックを実デコードから切り離して検証できる
/// (`music.rs::tests::FakeSource` と同じ考え方)。
pub trait MusicDecoder {
    /// インターリーブ f32 ステレオへ書けるだけ書いて、実際に書いたフレーム数を返す。
    /// 要求より少ない値(0 を含む)は EOF を意味する。デコードエラーは `Err` で返す。
    fn read(&mut self, out: &mut [f32]) -> Result<usize, DecodeError>;
    /// 指定フレームへシークする。コーデックによってはサンプル精度でない場合があるため、
    /// 実装はシーク後にデコード読み捨てでサンプル境界へ合わせ込むこと(モジュール doc 参照)。
    fn seek(&mut self, frame: u64) -> Result<(), DecodeError>;
    /// 総フレーム数(コンテナのメタデータから取得できない場合は `None`)。
    fn total_frames(&self) -> Option<u64>;
}

/// Symphonia ベースの同期デコーダ(wav / ogg vorbis)。
///
/// 入力はメモリ上のバイト列(`Vec<u8>`)を所有する形で受け取る(実行時のファイル IO を
/// 持ち込まない。初期構築仕様『§4.7』)。スレッドや `pump` の概念はここでは扱わない
/// (`stream.rs` が薄い皮を被せる)。
pub struct SymphoniaDecoder {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    total_frames: Option<u64>,
    /// 直近デコードしたパケットの生サンプル(ネイティブチャンネル数)を一時的に受ける
    /// スクラッチバッファ。パケットごとに容量不足なら [`grow_sample_buf`] で確保し直す。
    sample_buf: SampleBuffer<f32>,
    /// ステレオ展開済みで、まだ `read` に渡していない残りサンプル(インターリーブ)。
    pending: Vec<f32>,
    /// `pending` の読み出し位置(サンプル単位)。
    pending_pos: usize,
    /// デコーダがコンテナ末尾に達したか。`seek` でクリアされる。
    eof: bool,
}

impl SymphoniaDecoder {
    /// メモリ上のバイト列を開き、デコード可能かを確認する。
    ///
    /// `output_sample_rate` は出力(デバイス)側のサンプルレート。素材のレートと一致しない
    /// 場合は [`DecodeError::UnsupportedSampleRate`] を返す(リサンプルは範囲外。モジュール doc 参照)。
    pub fn open(bytes: Vec<u8>, output_sample_rate: u32) -> Result<Self, DecodeError> {
        let cursor = Cursor::new(bytes);
        let mss = MediaSourceStream::new(Box::new(cursor), Default::default());

        // 拡張子等のヒントは持たない(入力はメモリ上のバイト列のみ。§4.7)。
        // マジックバイトのみでの自動判別に委ねる。
        let hint = Hint::new();
        let format_opts = FormatOptions::default();
        let metadata_opts = MetadataOptions::default();

        let probed = symphonia::default::get_probe()
            .format(&hint, mss, &format_opts, &metadata_opts)
            .map_err(DecodeError::from)?;
        let format = probed.format;

        let track = format
            .tracks()
            .iter()
            .find(|t| {
                t.codec_params.codec != CODEC_TYPE_NULL
                    && symphonia::default::get_codecs()
                        .get_codec(t.codec_params.codec)
                        .is_some()
            })
            .ok_or(DecodeError::NoAudioTrack)?;

        let track_id = track.id;
        let sample_rate = track
            .codec_params
            .sample_rate
            .ok_or(DecodeError::NoAudioTrack)?;
        if sample_rate != output_sample_rate {
            return Err(DecodeError::UnsupportedSampleRate {
                found: sample_rate,
                expected: output_sample_rate,
            });
        }
        let channels = track
            .codec_params
            .channels
            .ok_or(DecodeError::NoAudioTrack)?;
        let source_channels = channels.count();
        if source_channels != 1 && source_channels != 2 {
            return Err(DecodeError::UnsupportedChannelCount(source_channels));
        }
        let total_frames = track.codec_params.n_frames;
        let codec_params = track.codec_params.clone();

        let dec_opts = DecoderOptions::default();
        let decoder = symphonia::default::get_codecs()
            .make(&codec_params, &dec_opts)
            .map_err(DecodeError::from)?;

        let spec = SignalSpec::new(sample_rate, channels);
        let sample_buf = SampleBuffer::<f32>::new(INITIAL_SAMPLE_BUF_FRAMES, spec);

        Ok(Self {
            format,
            decoder,
            track_id,
            total_frames,
            sample_buf,
            pending: Vec::new(),
            pending_pos: 0,
            eof: false,
        })
    }

    /// `pending` に残っているフレーム数。
    fn pending_frames(&self) -> usize {
        (self.pending.len() - self.pending_pos) / CHANNELS
    }

    /// 次のパケットをデコードし、ステレオ展開して `pending` へ積む。
    /// `Ok(true)`: 新しいデータを積んだ。`Ok(false)`: コンテナ末尾(EOF)。
    fn decode_next_packet_into_pending(&mut self) -> Result<bool, DecodeError> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(packet) => packet,
                Err(SymphoniaError::IoError(e))
                    if e.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    return Ok(false);
                }
                Err(e) => return Err(DecodeError::from(e)),
            };

            if packet.track_id() != self.track_id {
                continue;
            }

            match self.decoder.decode(&packet) {
                Ok(audio_buf) => {
                    let spec = *audio_buf.spec();
                    let frames = audio_buf.frames();
                    if frames == 0 {
                        // 空パケット(理論上稀)。次のパケットへ進む。
                        continue;
                    }

                    // `self.decoder` を借用したままの `audio_buf` が生きている間は
                    // `&mut self` を取るメソッドを呼べない(borrow checker)。
                    // そのため容量確保はフィールド単位の関数呼び出しに留める。
                    grow_sample_buf(&mut self.sample_buf, frames, spec);
                    self.sample_buf.copy_interleaved_ref(audio_buf);
                    let native = self.sample_buf.samples();
                    let native_channels = spec.channels.count();

                    self.pending.clear();
                    self.pending_pos = 0;
                    match native_channels {
                        1 => {
                            self.pending.reserve(frames * CHANNELS);
                            for &s in native {
                                let v = s * EQUAL_POWER_GAIN;
                                self.pending.push(v);
                                self.pending.push(v);
                            }
                        }
                        2 => {
                            self.pending.extend_from_slice(native);
                        }
                        other => return Err(DecodeError::UnsupportedChannelCount(other)),
                    }
                    return Ok(true);
                }
                // getting-started.rs の例に準拠: パケット単位の回復可能なエラーはスキップする。
                Err(SymphoniaError::IoError(_)) | Err(SymphoniaError::DecodeError(_)) => {
                    continue;
                }
                Err(e) => return Err(DecodeError::from(e)),
            }
        }
    }

    /// `n` フレームぶんデコード結果を読み捨てる(シーク後のサンプル境界合わせ込み用)。
    fn discard_frames(&mut self, mut n: u64) -> Result<(), DecodeError> {
        while n > 0 {
            let avail = self.pending_frames() as u64;
            if avail > 0 {
                let take = avail.min(n);
                self.pending_pos += (take as usize) * CHANNELS;
                n -= take;
                continue;
            }
            if self.eof {
                break;
            }
            if !self.decode_next_packet_into_pending()? {
                self.eof = true;
                break;
            }
        }
        Ok(())
    }
}

/// パケットの実際のフレーム数に合わせて `SampleBuffer` を確保し直す。
///
/// `SampleBuffer::copy_interleaved_typed` は容量不足だと panic するため
/// (Symphonia 側の契約)、コピー前に必ず確認する。フィールド単位の自由関数にしてあるのは
/// [`SymphoniaDecoder::decode_next_packet_into_pending`] 内で `self.decoder` を借用したまま
/// (`AudioBufferRef` がその借用の生存期間を握っている)呼び出す必要があるため
/// (`&mut self` を取るメソッドにすると借用が衝突する)。
fn grow_sample_buf(buf: &mut SampleBuffer<f32>, frames: usize, spec: SignalSpec) {
    let needed = frames * spec.channels.count();
    if buf.capacity() < needed {
        *buf = SampleBuffer::<f32>::new(frames as u64, spec);
    }
}

impl MusicDecoder for SymphoniaDecoder {
    fn read(&mut self, out: &mut [f32]) -> Result<usize, DecodeError> {
        let want_frames = out.len() / CHANNELS;
        let mut frames_written = 0usize;

        while frames_written < want_frames {
            let avail = self.pending_frames();
            if avail > 0 {
                let take = avail.min(want_frames - frames_written);
                let src_start = self.pending_pos;
                let src_end = src_start + take * CHANNELS;
                let dst_start = frames_written * CHANNELS;
                let dst_end = dst_start + take * CHANNELS;
                out[dst_start..dst_end].copy_from_slice(&self.pending[src_start..src_end]);
                self.pending_pos = src_end;
                frames_written += take;
                continue;
            }

            if self.eof {
                break;
            }
            if !self.decode_next_packet_into_pending()? {
                self.eof = true;
                break;
            }
        }

        Ok(frames_written)
    }

    fn seek(&mut self, frame: u64) -> Result<(), DecodeError> {
        let seeked = self
            .format
            .seek(
                SeekMode::Accurate,
                SeekTo::TimeStamp {
                    ts: frame,
                    track_id: self.track_id,
                },
            )
            .map_err(DecodeError::from)?;

        // シークするとデコーダの内部状態(前後のパケットに依存する予測等)が無効になるため、
        // Symphonia の契約どおりリセットする(`Decoder::reset` のドキュメント参照)。
        self.decoder.reset();
        self.pending.clear();
        self.pending_pos = 0;
        self.eof = false;

        // Accurate モードでも `actual_ts` は要求位置以下にしかならない(コンテナの
        // パケット境界までしか位置決めできないため。モジュール doc 参照)。差分だけ
        // デコード結果を読み捨てて、常にサンプル境界ちょうどへ合わせ込む。
        let discard = frame.saturating_sub(seeked.actual_ts);
        self.discard_frames(discard)
    }

    fn total_frames(&self) -> Option<u64> {
        self.total_frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wav::golden::make_pcm16_wav;

    const SAMPLE_RATE: u32 = 48_000;

    fn i16_to_f32(sample: i16) -> f32 {
        sample as f32 / 32_768.0
    }

    #[test]
    fn decodes_stereo_wav_in_order() {
        let samples: Vec<i16> = vec![0, 0, 16384, -16384, -16384, 16384, 32767, -32768];
        let bytes = make_pcm16_wav(SAMPLE_RATE, 2, &samples);
        let mut decoder = SymphoniaDecoder::open(bytes, SAMPLE_RATE).expect("valid wav must open");

        assert_eq!(decoder.total_frames(), Some(4));

        let mut out = vec![0.0f32; 4 * CHANNELS];
        let n = decoder.read(&mut out).expect("read must succeed");
        assert_eq!(n, 4);

        assert_eq!(out[0], 0.0);
        assert_eq!(out[1], 0.0);
        assert!((out[2] - i16_to_f32(16384)).abs() < 1e-6);
        assert!((out[3] - i16_to_f32(-16384)).abs() < 1e-6);
        assert_eq!(out[6], i16_to_f32(32767));
        assert_eq!(out[7], -1.0);

        // 末尾に到達したら以降は 0 フレーム(EOF)。
        let mut tail = vec![1.0f32; 2 * CHANNELS];
        let n2 = decoder.read(&mut tail).expect("read at eof must succeed");
        assert_eq!(n2, 0);
    }

    #[test]
    fn mono_expands_to_stereo_with_equal_power_gain() {
        let samples: Vec<i16> = vec![32767, -32768, 0];
        let bytes = make_pcm16_wav(SAMPLE_RATE, 1, &samples);
        let mut decoder =
            SymphoniaDecoder::open(bytes, SAMPLE_RATE).expect("valid mono wav must open");

        let mut out = vec![0.0f32; 3 * CHANNELS];
        let n = decoder.read(&mut out).expect("read must succeed");
        assert_eq!(n, 3);

        assert_eq!(
            out[0], out[1],
            "mono source must expand identically to both channels"
        );
        let expected = i16_to_f32(32767) * EQUAL_POWER_GAIN;
        assert!((out[0] - expected).abs() < 1e-6);

        // 等パワー: 展開後の (L^2 + R^2) は元のモノラルサンプルの2乗と一致する。
        let original = i16_to_f32(-32768);
        let l = out[2];
        let r = out[3];
        assert!(((l * l + r * r) - original * original).abs() < 1e-5);
    }

    #[test]
    fn sample_rate_mismatch_is_a_clear_error_mentioning_future_resampling_work() {
        let bytes = make_pcm16_wav(44_100, 2, &[0, 0]);
        // `SymphoniaDecoder` は内部に `Box<dyn FormatReader>` を持つため `Debug` を
        // 導出できず、`unwrap_err()` (Ok 側に `Debug` を要求する) は使えない。
        let err = match SymphoniaDecoder::open(bytes, 48_000) {
            Ok(_) => panic!("sample rate mismatch must be rejected"),
            Err(e) => e,
        };
        match err {
            DecodeError::UnsupportedSampleRate { found, expected } => {
                assert_eq!(found, 44_100);
                assert_eq!(expected, 48_000);
            }
            other => panic!("expected UnsupportedSampleRate, got {other:?}"),
        }
        assert!(
            err.to_string().contains("rubato"),
            "error message should mention the planned rubato-based resampling work"
        );
    }

    #[test]
    fn streams_across_multiple_packets_in_order() {
        // symphonia-format-riff は PCM を最大 1152 フレーム/パケットに区切る。
        // 3000 フレームなら 3 パケットに分割され、`pending` バッファの繰り越しを
        // 実際に踏む(単一パケットに収まる短い素材では検証できない)。
        const FRAME_COUNT: usize = 3_000;
        let mut samples = Vec::with_capacity(FRAME_COUNT * 2);
        for i in 0..FRAME_COUNT {
            let v = (i % 30_000) as i16;
            samples.push(v);
            samples.push(-v);
        }
        let bytes = make_pcm16_wav(SAMPLE_RATE, 2, &samples);
        let mut decoder = SymphoniaDecoder::open(bytes, SAMPLE_RATE).expect("valid wav must open");
        assert_eq!(decoder.total_frames(), Some(FRAME_COUNT as u64));

        // わざと半端な大きさ(パケット境界と揃わない)で少しずつ読み、繰り越しを踏む。
        let mut got = Vec::with_capacity(FRAME_COUNT);
        let chunk_frames = 700;
        loop {
            let mut buf = vec![0.0f32; chunk_frames * CHANNELS];
            let n = decoder.read(&mut buf).expect("read must succeed");
            if n == 0 {
                break;
            }
            for frame in 0..n {
                got.push(buf[frame * CHANNELS]);
            }
        }
        assert_eq!(got.len(), FRAME_COUNT);
        for (i, &v) in got.iter().enumerate() {
            let expected = i16_to_f32((i % 30_000) as i16);
            assert_eq!(v, expected, "frame {i} out of order or corrupted");
        }
    }

    #[test]
    fn seek_lands_exactly_on_the_requested_sample_boundary() {
        const FRAME_COUNT: usize = 2_000;
        let mut samples = Vec::with_capacity(FRAME_COUNT * 2);
        for i in 0..FRAME_COUNT {
            samples.push(i as i16);
            samples.push(-(i as i16));
        }
        let bytes = make_pcm16_wav(SAMPLE_RATE, 2, &samples);
        let mut decoder = SymphoniaDecoder::open(bytes, SAMPLE_RATE).expect("valid wav must open");

        // パケット境界(1152 の倍数)からずれた、中途半端な位置へシークする。
        // wav 自体は本来サンプル精度で seek できるはずだが、`discard_frames` の
        // コードパス自体はコーデックに依存しないため、ここで精度を固定化しておけば
        // ogg のようにコンテナ側がパケット境界までしか戻せない場合にも同じ保証が働く。
        let target = 1_337u64;
        decoder.seek(target).expect("seek must succeed");

        let mut out = vec![0.0f32; 4 * CHANNELS];
        let n = decoder
            .read(&mut out)
            .expect("read after seek must succeed");
        assert_eq!(n, 4);
        for i in 0..4u64 {
            let expected = (target + i) as i16;
            let idx = i as usize * CHANNELS;
            assert_eq!(out[idx], i16_to_f32(expected));
            assert_eq!(out[idx + 1], i16_to_f32(-expected));
        }
    }

    #[test]
    fn ogg_container_is_recognized_by_the_probe() {
        // vorbis エンコーダは依存に追加していないため(§7.1: 依存を無用に太らせない)、
        // 完全に有効な ogg vorbis ストリームをテストコードで合成することは非現実的
        // (setup ヘッダのコードブック等、vorbis 仕様の相当な部分の再実装が必要になる)。
        // そのため、ここでは「ogg フォーマットの検出自体は配線されている」ことだけを
        // 最小限に確認する: "OggS" マジックで始まる(が中身は不正な)バイト列を渡し、
        // エラーメッセージが「対応フォーマットが見つからない」という汎用エラーではなく、
        // ogg 用の `FormatReader` が実際にパースを試みた結果のエラーになっていることを見る。
        //
        // 判断: ogg vorbis の完全なラウンドトリップ(pump → read でのデコード検証)は
        // wav でのみ行い、ogg は「対応コーデックとして組み込まれていること」の
        // 最小限の証跡に留める(依頼書のとおり、この判断を報告する)。
        let mut bytes = vec![0u8; 64];
        bytes[0..4].copy_from_slice(b"OggS");
        let err = match SymphoniaDecoder::open(bytes, SAMPLE_RATE) {
            Ok(_) => panic!("a bare \"OggS\" marker with no valid page must not open successfully"),
            Err(e) => e,
        };
        let message = err.to_string();
        assert!(
            !message.contains("no suitable format reader found"),
            "ogg should be recognized by the probe via its \"OggS\" marker, got: {message}"
        );
    }

    #[test]
    fn vorbis_codec_is_registered() {
        // ogg のコンテナ検出(上のテスト)とは独立に、vorbis コーデックの feature が
        // 実際に有効化されていること自体も確認しておく(feature の指定ミスで
        // 静かに wav only になっていないことのチェック)。
        let registry = symphonia::default::get_codecs();
        assert!(
            registry
                .get_codec(symphonia::core::codecs::CODEC_TYPE_VORBIS)
                .is_some(),
            "vorbis codec must be registered (Cargo.toml feature = [\"vorbis\"])"
        );
    }
}
