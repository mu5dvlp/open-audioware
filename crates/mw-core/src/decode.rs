//! 楽曲のストリーミングデコード(初期構築仕様『§4.7 デコードとリサンプリング』, M7/M2)。
//!
//! メモリ上の WAV バイト列を同期的にストリーミングデコードする。
//! **ファイル IO は一切持ち込まない**(入力は常にメモリ上のバイト列)。出力は
//! `crate::format::CHANNELS` に固定した f32 インターリーブ・ステレオで、モノラル素材は
//! `wav.rs` と同じ等パワー(`1/√2`)で両ch展開する。
//!
//! ここで行うのは**同期デコードのみ**。リングバッファへの供給・スレッド分離は
//! `stream.rs` の責務であり、このモジュールはスレッドや `pump` の概念を一切知らない
//! (`MusicDecoder` トレイトを満たす、ただの同期デコーダ)。
//!
//! # リサンプル(初期構築仕様『§4.7』, `resample.rs`)
//!
//! 出力(デバイス)サンプルレートと素材のレートが一致しない場合、[`WavDecoder`] は
//! `resample.rs::StreamResampler`(rubato `Fft` ベース)で自動的に変換する。
//! **レートが一致する場合はリサンプラを構築すらしない**(`resampler: Option<_>` が
//! `None` のままバイパスする。レート一致時に無用な計算・レイテンシを
//! 持ち込まない)。
//!
//! この変換は [`MusicDecoder`] トレイトの内側で完結しており、`stream.rs`(`pump()`)・
//! `music.rs`(`MusicVoice`)はどちらも一切関知しない。両モジュールにとって
//! `read`/`seek`/`total_frames` の「フレーム」は常に**出力レート基準**になる
//! 素材レートとの変換は [`resample::convert_frame_count`] に
//! 一本化してあり、総フレーム数もシーク位置もこの1つの関数だけで換算する。
//!
//! # シークとサンプル精度
//!
//! WAV の PCM はフレームがバイト境界と一致するため、[`NativeReader::seek`] は
//! データオフセットをフレーム単位で直接変更し、サンプル精度で着地する。
//! `WavDecoder::seek` が受け取るのは出力レート基準のフレームなので、素材レートへ
//! 変換してから `NativeReader::seek` に渡す。

use std::fmt;
use std::sync::Arc;

use crate::format::CHANNELS;
use crate::resample::{self, ResampleError, StreamResampler};
use crate::wav::{WavError, WavStreamReader};

/// デコードで発生しうるエラー(`wav.rs::WavError` と同じ設計方針: 具体的なバリアントで返す)。
#[derive(Debug)]
pub enum DecodeError {
    /// WAV ヘッダ解析またはデータ読み出しで発生したエラー。
    Wav(WavError),
    /// 音声トラックを持たない入力。ストリーミング供給側のエラー契約を維持する。
    NoAudioTrack,
    /// モノラル・ステレオ以外のチャンネル数。
    UnsupportedChannelCount(usize),
    /// 素材のサンプルレートが 0(壊れたメタデータ)。通常の入力では起こらない防御的チェック。
    InvalidSampleRate(u32),
    /// リサンプラの構築・変換処理そのものが失敗した(`resample.rs` 参照。
    /// 通常のサンプルレートの組み合わせでは起こらない異常系)。
    Resample(ResampleError),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Wav(err) => write!(f, "wav decode error: {err}"),
            DecodeError::NoAudioTrack => write!(f, "no audio track found"),
            DecodeError::UnsupportedChannelCount(channels) => write!(
                f,
                "unsupported channel count {channels} (only mono or stereo is supported)"
            ),
            DecodeError::InvalidSampleRate(rate) => {
                write!(f, "invalid source sample rate {rate} Hz (must be > 0)")
            }
            DecodeError::Resample(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for DecodeError {}

#[cfg(test)]
mod display_tests {
    use super::*;

    /// `Resample` は内側のエラーへ表示を委譲するため、重複検査から除外する。
    /// それ以外はワイルドカード無しで列挙し、バリアント追加時に更新を要求する。
    fn describe(error: &DecodeError) -> Option<&'static str> {
        match error {
            DecodeError::Wav(_) => Some("wav decode error"),
            DecodeError::NoAudioTrack => Some("no audio track"),
            DecodeError::UnsupportedChannelCount(_) => Some("unsupported channel count"),
            DecodeError::InvalidSampleRate(_) => Some("invalid source sample rate"),
            DecodeError::Resample(_) => None,
        }
    }

    #[test]
    fn decode_error_display_identifies_every_outer_variant_without_duplicates() {
        let errors = [
            DecodeError::Wav(WavError::NotRiff),
            DecodeError::NoAudioTrack,
            DecodeError::UnsupportedChannelCount(5),
            DecodeError::InvalidSampleRate(0),
            // This variant delegates its complete message to the inner error.
            DecodeError::Resample(ResampleError::Construction("zero rate".into())),
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
            "each outer DecodeError variant must have a distinct display message: {outer_messages:?}"
        );
        // 引数つきのバリアントは、**引数そのものが文言に出ていること**も見る。
        // 🔴 添字ではなく `match` で取り出すこと(理由は `wav.rs` の同じ検査のコメント参照
        // —— 添字だと配列の順番を変えたときに別のバリアントを検査して偶然通る)。
        for error in &errors {
            let expected_argument = match error {
                DecodeError::Wav(_) => None,
                DecodeError::NoAudioTrack => None,
                DecodeError::UnsupportedChannelCount(channels) => Some(channels.to_string()),
                DecodeError::InvalidSampleRate(rate) => Some(rate.to_string()),
                DecodeError::Resample(_) => None,
            };
            let Some(argument) = expected_argument else {
                continue;
            };
            let rendered = error.to_string();
            assert!(
                rendered.contains(&argument),
                "{error:?} must print its argument ({argument}) so the device log says \
                 what was actually wrong: {rendered}"
            );
        }

        // 委譲するバリアントは内側のエラーの文言をそのまま出す
        // (これが上の重複検査から除外してある理由)。
        assert_eq!(
            DecodeError::Resample(ResampleError::Construction("zero rate".into())).to_string(),
            "resampler construction failed: zero rate",
            "DecodeError::Resample は内側の ResampleError へ丸ごと委譲する"
        );
    }
}

impl From<WavError> for DecodeError {
    fn from(err: WavError) -> Self {
        match err {
            WavError::InvalidSampleRate(rate) => DecodeError::InvalidSampleRate(rate),
            WavError::UnsupportedChannelCount(channels) => {
                DecodeError::UnsupportedChannelCount(channels as usize)
            }
            WavError::Resample(err) => DecodeError::Resample(err),
            other => DecodeError::Wav(other),
        }
    }
}

/// `stream.rs::MusicStreamProducer::pump` に渡す、同期デコーダの最小契約。
///
/// 本番実装は [`WavDecoder`]。テストではこの trait を満たすフェイクを使うことで、
/// リングバッファ・エポック調停のロジックを実デコードから切り離して検証できる
/// (`music.rs::tests::FakeSource` と同じ考え方)。
///
/// **フレーム単位は常に出力レート基準**(初期構築仕様『§4.7』設計判断3)。
/// 実装がレート変換を行う場合、`read`/`seek`/`total_frames` の外側にそれを一切見せない
/// こと(`WavDecoder` 参照)。
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

/// WAV の生デコード結果(素材ネイティブレート)だけを扱う内部リーダー。
///
/// [`WavDecoder`] とフィールドを分けてあるのは、`WavDecoder::refill_resampled_pending`
/// が「素材レートで読み出す(`self.native.read(..)`)」のと「リサンプル結果を積む
/// (`self.resampled_pending`)」のを同じ呼び出しの中で行う必要があり、両方とも
/// `WavDecoder` の別フィールドとして持たせないと借用が衝突するため
/// ([`grow_sample_buf`] を自由関数にしてある理由と同じ。`self.native.read(&mut self.scratch)`
/// のように**別フィールド越しの呼び出し**にすれば、借用チェッカーは互いに素なフィールドの
/// 同時借用として認めてくれる)。
struct NativeReader {
    wav: WavStreamReader,
}

impl NativeReader {
    fn read(&mut self, out: &mut [f32]) -> Result<usize, DecodeError> {
        Ok(self.wav.read(out))
    }

    fn seek(&mut self, frame: u64) -> Result<(), DecodeError> {
        self.wav.seek(frame);
        Ok(())
    }
}

/// WAV ベースの同期デコーダ。
///
/// 入力はメモリ上のバイト列(`Vec<u8>`)を所有する形で受け取る(実行時のファイル IO を
/// 持ち込まない。初期構築仕様『§4.7』)。スレッドや `pump` の概念はここでは扱わない
/// (`stream.rs` が薄い皮を被せる)。
///
/// 素材レートと `output_sample_rate` が一致しない場合は `resampler` を通してから
/// [`MusicDecoder::read`] が返す(モジュール doc 参照)。**トレイト境界の外(`read`/`seek`/
/// `total_frames`)では常に出力レート基準のフレーム数**として振る舞う。
pub struct WavDecoder {
    native: NativeReader,
    source_sample_rate: u32,
    output_sample_rate: u32,
    /// 出力レート換算の総フレーム数([`resample::convert_frame_count`] で変換済み)。
    total_frames: Option<u64>,
    /// `source_sample_rate != output_sample_rate` のときだけ `Some`
    /// (レート一致時はリサンプラを構築すらせずバイパスする)。
    resampler: Option<StreamResampler>,
    /// リサンプラへ供給するための、素材レートの読み出しスクラッチ(固定長・再利用)。
    resample_scratch_in: Vec<f32>,
    /// リサンプル済み(出力レート)で、まだ `read` に渡していない分。
    resampled_pending: Vec<f32>,
    /// `resampled_pending` の読み出し位置(サンプル単位)。
    resampled_pending_pos: usize,
    /// 素材側が末尾まで読み切り、リサンプラのフラッシュ(`StreamResampler::flush_into`)も
    /// 完了したか。true になった後は `refill_resampled_pending` を呼ばない。
    resample_finished: bool,
    /// 出力レート基準の現在位置。`total_frames` を超えて配らないための安全弁として使う
    /// (リサンプラはブロック単位でしか出力できないため、末尾ブロックは `total_frames` を
    /// 僅かに超えて生成されることがある。`resample.rs` モジュール doc 参照)。
    /// シーク直後は要求されたフレーム番号そのものを正とする(設計判断3)。
    position_frames: u64,
}

/// `Arc<Vec<u8>>` をデコーダと共有するための薄いラッパ。
///
/// 曲を切り替えるたびに音源バイト列を複製しないため、デコーダはこの共有所有権を
/// そのまま保持する。
#[derive(Debug, Clone)]
pub struct SharedBytes(Arc<Vec<u8>>);

impl SharedBytes {
    /// 共有バイト列を包む(複製しない)。
    pub fn new(bytes: Arc<Vec<u8>>) -> Self {
        Self(bytes)
    }
}

impl AsRef<[u8]> for SharedBytes {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl From<Arc<Vec<u8>>> for SharedBytes {
    fn from(bytes: Arc<Vec<u8>>) -> Self {
        Self(bytes)
    }
}

impl From<Vec<u8>> for SharedBytes {
    fn from(bytes: Vec<u8>) -> Self {
        Self(Arc::new(bytes))
    }
}

impl WavDecoder {
    /// メモリ上のバイト列を開き、デコード可能かを確認する。
    ///
    /// `output_sample_rate` は出力(デバイス)側のサンプルレート。素材のレートと一致しない
    /// 場合は内部でリサンプラ(`resample.rs::StreamResampler`)を構築する(モジュール doc)。
    ///
    /// 📌 **既に `Arc<Vec<u8>>` を持っているなら [`WavDecoder::open_shared`] を使うこと**
    /// —— こちらは `Vec<u8>` を受け取るので、呼び出し側が複製を作る羽目になる
    /// (`mw-ffi` が実際にそうなっていた。P3-13)。
    pub fn open(bytes: Vec<u8>, output_sample_rate: u32) -> Result<Self, DecodeError> {
        Self::open_shared(Arc::new(bytes), output_sample_rate)
    }

    /// [`WavDecoder::open`] と同じだが、**共有された**バイト列を複製せずに開く。
    ///
    /// 🔴 曲の切り替え(`mw_music_set`)と再オープン後の復元は、音源ストレージが持つ
    /// `Arc<Vec<u8>>` をそのまま渡せる。数十MB の WAV でゲームスレッドが `memcpy` に
    /// 費やしていた時間と、ピークメモリの倍増が無くなる(P3-13)。
    pub fn open_shared(bytes: Arc<Vec<u8>>, output_sample_rate: u32) -> Result<Self, DecodeError> {
        let wav = WavStreamReader::open_shared(Arc::clone(&bytes)).map_err(DecodeError::from)?;
        let source_sample_rate = wav.sample_rate();
        let total_frames_source = wav.total_frames();
        let native = NativeReader { wav };

        // レートが一致する場合はリサンプラを構築しない(設計判断4: 無用な計算・
        // レイテンシを持ち込まない)。`convert_frame_count` はレート一致時に恒等変換になる
        // ため、`total_frames`/`seek` 側は分岐せず同じ式で扱える。
        let resampler = if source_sample_rate == output_sample_rate {
            None
        } else {
            Some(
                StreamResampler::new(source_sample_rate, output_sample_rate)
                    .map_err(DecodeError::Resample)?,
            )
        };
        let total_frames = Some(resample::convert_frame_count(
            total_frames_source,
            source_sample_rate,
            output_sample_rate,
        ));

        Ok(Self {
            native,
            source_sample_rate,
            output_sample_rate,
            total_frames,
            resampler,
            resample_scratch_in: Vec::new(),
            resampled_pending: Vec::new(),
            resampled_pending_pos: 0,
            resample_finished: false,
            position_frames: 0,
        })
    }

    /// `resampled_pending` に溜まっている、まだ配っていないフレーム数。
    fn resampled_pending_frames(&self) -> usize {
        (self.resampled_pending.len() - self.resampled_pending_pos) / CHANNELS
    }

    /// リサンプル経路での `read`(`self.resampler.is_some()` のときのみ呼ぶ)。
    fn read_resampled(
        &mut self,
        want_frames: usize,
        out: &mut [f32],
    ) -> Result<usize, DecodeError> {
        let mut frames_written = 0usize;

        while frames_written < want_frames {
            let avail = self.resampled_pending_frames();
            if avail > 0 {
                let take = avail.min(want_frames - frames_written);
                let src_start = self.resampled_pending_pos;
                let src_end = src_start + take * CHANNELS;
                let dst_start = frames_written * CHANNELS;
                let dst_end = dst_start + take * CHANNELS;
                out[dst_start..dst_end]
                    .copy_from_slice(&self.resampled_pending[src_start..src_end]);
                self.resampled_pending_pos = src_end;
                frames_written += take;
                continue;
            }

            if self.resample_finished {
                break;
            }
            self.refill_resampled_pending()?;
        }

        Ok(frames_written)
    }

    /// 素材レートで1ブロックぶん読み、リサンプルして `resampled_pending` を補充する。
    ///
    /// `self.native.read(&mut self.resample_scratch_in[..])` のように**別フィールド越し**に
    /// 呼ぶことで、続く `self.resampler`/`self.resampled_pending` への書き込みと借用が
    /// 衝突しないようにしてある(`NativeReader` のドキュメント参照)。
    fn refill_resampled_pending(&mut self) -> Result<(), DecodeError> {
        // 消費済みの先頭を捨てて無制限な肥大化を防ぐ(`NativeReader::pending` と同じ考え方)。
        if self.resampled_pending_pos > 0 {
            self.resampled_pending.drain(0..self.resampled_pending_pos);
            self.resampled_pending_pos = 0;
        }

        // 呼び出し元(`read`)は `self.resampler.is_some()` を確認してから
        // `read_resampled` に入る。ここで `None` に出会うことは理論上ないが、
        // `unwrap`/`expect` でパニックする代わりに「これ以上データは来ない」として
        // 安全に終わらせる(§5.3 の精神を音声スレッド以外のコードにも適用する)。
        let Some(need) = self
            .resampler
            .as_ref()
            .map(StreamResampler::input_frames_needed)
        else {
            self.resample_finished = true;
            return Ok(());
        };
        if self.resample_scratch_in.len() < need * CHANNELS {
            self.resample_scratch_in.resize(need * CHANNELS, 0.0);
        }

        let n = self
            .native
            .read(&mut self.resample_scratch_in[..need * CHANNELS])?;

        let Some(resampler) = self.resampler.as_mut() else {
            self.resample_finished = true;
            return Ok(());
        };
        if n == need {
            resampler
                .process_full_chunk_into(
                    &self.resample_scratch_in[..need * CHANNELS],
                    &mut self.resampled_pending,
                )
                .map_err(DecodeError::Resample)?;
        } else {
            resampler
                .flush_into(
                    &self.resample_scratch_in[..n * CHANNELS],
                    &mut self.resampled_pending,
                )
                .map_err(DecodeError::Resample)?;
            self.resample_finished = true;
        }
        Ok(())
    }
}

impl MusicDecoder for WavDecoder {
    fn read(&mut self, out: &mut [f32]) -> Result<usize, DecodeError> {
        let mut want_frames = out.len() / CHANNELS;
        // total_frames を超えて配らないための安全弁(構造体 doc 参照)。
        if let Some(total) = self.total_frames {
            let remaining = total.saturating_sub(self.position_frames) as usize;
            want_frames = want_frames.min(remaining);
        }
        if want_frames == 0 {
            return Ok(0);
        }
        let want_samples = want_frames * CHANNELS;

        let n = if self.resampler.is_some() {
            self.read_resampled(want_frames, &mut out[..want_samples])?
        } else {
            self.native.read(&mut out[..want_samples])?
        };
        self.position_frames += n as u64;
        Ok(n)
    }

    fn seek(&mut self, frame: u64) -> Result<(), DecodeError> {
        // `frame` は出力レート基準(モジュール doc)。素材レートへ変換してから
        // WAV ストリームリーダーのシークへ渡す。
        let source_frame =
            resample::convert_frame_count(frame, self.output_sample_rate, self.source_sample_rate);
        self.native.seek(source_frame)?;

        self.resampled_pending.clear();
        self.resampled_pending_pos = 0;
        self.resample_finished = false;
        if let Some(resampler) = self.resampler.as_mut() {
            // シークは意図的な不連続(`music.rs::MusicVoice::seek` のドキュメント参照)。
            // 直前までのオーバーラップ状態を引き継ぐと、シーク前後の音が混ざってしまう
            // ため、必ずリセットする(`resample.rs` モジュール doc「ブロック境界の連続性」)。
            resampler.reset();
        }

        // 出力レート基準の位置は要求値そのものを正とする(設計判断3)。素材側の着地点は
        // レート変換の丸めによる素材側の着地点の誤差を持ちうるが、
        // 呼び出し側(`music.rs::MusicVoice::position_frames`)が管理する位置と
        // 常に一致させることを優先する。
        self.position_frames = frame;
        Ok(())
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

    /// 指定周波数の正弦波(両ch同一)を PCM16 wav として合成する(リサンプル検証用)。
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

    /// ゼロ交差の数から推定周波数を求める(周波数が保たれることの検証用)。
    /// 前後の縁(リサンプラの端の影響が出やすい)を除いてから数える。
    fn estimate_frequency_hz(left_channel: &[f32], sample_rate: u32) -> f32 {
        let margin = left_channel.len() / 20; // 前後5%ずつ除く
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

    /// EOF まで読み切って左chだけを抜き出す(テスト用ヘルパ)。
    fn drain_left_channel(decoder: &mut dyn MusicDecoder, read_chunk_frames: usize) -> Vec<f32> {
        let mut got = Vec::new();
        loop {
            let mut buf = vec![0.0f32; read_chunk_frames * CHANNELS];
            let n = decoder.read(&mut buf).expect("read must succeed");
            if n == 0 {
                break;
            }
            for i in 0..n {
                got.push(buf[i * CHANNELS]);
            }
        }
        got
    }

    /// 🔴 **P3-13 の退行防止。** `open_shared` が「共有」でなくなったら(= 内部で
    /// 複製を作るように戻したら)ここで落ちる。
    ///
    /// ⚠️ これを `total_frames` 等の「動く」確認だけで守ろうとしても無理 ——
    /// 複製しても動きは一切変わらず、**変わるのは曲切り替えの所要時間とピークメモリだけ**
    /// だからである(数十MB の WAV でゲームスレッドがヒッチする)。
    /// 参照カウントを直接見るのが、この性質を固定できる唯一の方法。
    #[test]
    fn open_shared_shares_the_compressed_bytes_instead_of_copying_them() {
        let samples: Vec<i16> = vec![0, 0, 16384, -16384];
        let bytes = Arc::new(make_pcm16_wav(SAMPLE_RATE, 2, &samples));
        let before = Arc::strong_count(&bytes);

        let decoder =
            WavDecoder::open_shared(Arc::clone(&bytes), SAMPLE_RATE).expect("valid wav must open");

        assert!(
            Arc::strong_count(&bytes) > before,
            "open_shared がバイト列を共有していない(複製して捨てている)。\n             呼び出し側〔mw-ffi の mw_music_set〕は曲を切り替えるたびに数十MB の memcpy を\n             ゲームスレッドで行うことになる(REFACTOR-PLAN P3-13)。"
        );

        drop(decoder);
        assert_eq!(
            Arc::strong_count(&bytes),
            before,
            "デコーダを落としても参照が残っている(バイト列が解放されない)。"
        );
    }

    /// `open`(`Vec<u8>` を値で受ける既存の入口)も `open_shared` 経由で動くこと。
    /// ⚠️ テストの大半がこちらを使っているので、委譲を壊すと広範囲に落ちる ——
    /// **落ちる前にここで分かるようにしておく。**
    #[test]
    fn open_still_works_and_is_equivalent_to_open_shared() {
        let samples: Vec<i16> = vec![0, 0, 16384, -16384, -16384, 16384];
        let bytes = make_pcm16_wav(SAMPLE_RATE, 2, &samples);

        let mut by_value =
            WavDecoder::open(bytes.clone(), SAMPLE_RATE).expect("valid wav must open");
        let mut shared =
            WavDecoder::open_shared(Arc::new(bytes), SAMPLE_RATE).expect("valid wav must open");

        assert_eq!(by_value.total_frames(), shared.total_frames());
        assert_eq!(
            drain_left_channel(&mut by_value, 4),
            drain_left_channel(&mut shared, 4)
        );
    }

    #[test]
    fn decodes_stereo_wav_in_order() {
        let samples: Vec<i16> = vec![0, 0, 16384, -16384, -16384, 16384, 32767, -32768];
        let bytes = make_pcm16_wav(SAMPLE_RATE, 2, &samples);
        let mut decoder = WavDecoder::open(bytes, SAMPLE_RATE).expect("valid wav must open");

        assert_eq!(decoder.total_frames(), Some(4));

        let mut out = vec![0.0f32; 4 * CHANNELS];
        let n = decoder.read(&mut out).expect("read must succeed");
        assert_eq!(n, 4);

        let expected = vec![
            0.0,
            0.0,
            i16_to_f32(16384),
            i16_to_f32(-16384),
            i16_to_f32(-16384),
            i16_to_f32(16384),
            i16_to_f32(32767),
            -1.0,
        ];
        assert_eq!(
            out, expected,
            "PCM16 WAV のストリーム出力ゴールデンが変わった"
        );

        // 末尾に到達したら以降は 0 フレーム(EOF)。
        let mut tail = vec![1.0f32; 2 * CHANNELS];
        let n2 = decoder.read(&mut tail).expect("read at eof must succeed");
        assert_eq!(n2, 0);
    }

    #[test]
    fn mono_expands_to_stereo_with_equal_power_gain() {
        let samples: Vec<i16> = vec![32767, -32768, 0];
        let bytes = make_pcm16_wav(SAMPLE_RATE, 1, &samples);
        let mut decoder = WavDecoder::open(bytes, SAMPLE_RATE).expect("valid mono wav must open");

        let mut out = vec![0.0f32; 3 * CHANNELS];
        let n = decoder.read(&mut out).expect("read must succeed");
        assert_eq!(n, 3);

        assert_eq!(
            out[0], out[1],
            "mono source must expand identically to both channels"
        );
        let expected = i16_to_f32(32767) * std::f32::consts::FRAC_1_SQRT_2;
        assert!((out[0] - expected).abs() < 1e-6);

        // 等パワー: 展開後の (L^2 + R^2) は元のモノラルサンプルの2乗と一致する。
        let original = i16_to_f32(-32768);
        let l = out[2];
        let r = out[3];
        assert!(((l * l + r * r) - original * original).abs() < 1e-5);
    }

    #[test]
    fn streams_across_multiple_packets_in_order() {
        // 3000 フレームを要求より小さい読み出しへ分割し、ストリームリーダーが
        // 連続したバイト範囲を正しい順序で供給することを検証する。
        const FRAME_COUNT: usize = 3_000;
        let mut samples = Vec::with_capacity(FRAME_COUNT * 2);
        for i in 0..FRAME_COUNT {
            let v = (i % 30_000) as i16;
            samples.push(v);
            samples.push(-v);
        }
        let bytes = make_pcm16_wav(SAMPLE_RATE, 2, &samples);
        let mut decoder = WavDecoder::open(bytes, SAMPLE_RATE).expect("valid wav must open");
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
        let mut decoder = WavDecoder::open(bytes, SAMPLE_RATE).expect("valid wav must open");

        // パケット境界(1152 の倍数)からずれた、中途半端な位置へシークする。
        // WAV のデータオフセットがフレーム単位で正しく計算されていることを、
        // フレーム境界からずれた位置へのシークで固定化する。
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

    // --- ここから先はリサンプル(初期構築仕様『§4.7』)の検証 -------------------------

    #[test]
    fn resample_preserves_frequency_when_upsampling_44_1k_to_48k() {
        // 周波数が保たれること。
        const SOURCE_RATE: u32 = 44_100;
        const OUTPUT_RATE: u32 = 48_000;
        const FREQ_HZ: f32 = 1_000.0;
        const FRAME_COUNT: usize = 4_410; // 100ms ぶん(1kHz の100周期)。

        let bytes = make_sine_wave_wav(SOURCE_RATE, FREQ_HZ, FRAME_COUNT, 20_000);
        let mut decoder = WavDecoder::open(bytes, OUTPUT_RATE).expect("valid wav must open");

        let left = drain_left_channel(&mut decoder, 512);
        let estimated = estimate_frequency_hz(&left, OUTPUT_RATE);
        assert!(
            (estimated - FREQ_HZ).abs() / FREQ_HZ < 0.02,
            "resampled frequency should stay close to {FREQ_HZ} Hz, got {estimated} Hz"
        );
    }

    #[test]
    fn resample_produces_the_expected_output_length() {
        // 長さが正しいこと。総フレーム数・実際に読み出せる
        // フレーム数の両方が `convert_frame_count` の換算式ちょうどに一致することを見る
        // (§4.7 設計判断3: 端数超過分は `total_frames` で切り詰める設計にしてある)。
        const SOURCE_RATE: u32 = 44_100;
        const OUTPUT_RATE: u32 = 48_000;
        const FRAME_COUNT: usize = 4_410;

        let bytes = make_sine_wave_wav(SOURCE_RATE, 1_000.0, FRAME_COUNT, 20_000);
        let mut decoder = WavDecoder::open(bytes, OUTPUT_RATE).expect("valid wav must open");

        let expected = resample::convert_frame_count(FRAME_COUNT as u64, SOURCE_RATE, OUTPUT_RATE);
        assert_eq!(decoder.total_frames(), Some(expected));

        let left = drain_left_channel(&mut decoder, 4_096);
        assert_eq!(left.len() as u64, expected);
    }

    #[test]
    fn resample_block_boundaries_are_seamless() {
        // ブロック境界の連続性。細切れに read() したときと、
        // 一括に近い大きさで read() したときとで、リサンプル結果が完全一致することを見る
        // (`StreamResampler` を `pump` 相当の呼び出しをまたいで使い回すことで、
        // 内部オーバーラップ状態が保たれ継ぎ目にプチノイズが出ない設計。`resample.rs` 参照)。
        const SOURCE_RATE: u32 = 44_100;
        const OUTPUT_RATE: u32 = 48_000;
        const FRAME_COUNT: usize = 4_410;

        let bytes = make_sine_wave_wav(SOURCE_RATE, 1_000.0, FRAME_COUNT, 20_000);

        let mut chunked =
            WavDecoder::open(bytes.clone(), OUTPUT_RATE).expect("valid wav must open");
        // 37 フレームというわざと半端な大きさで細切れに読み、内部リサンプラのブロック長
        // (STREAM_CHUNK_TARGET_FRAMES 由来)とは揃わない境界を何度も踏む。
        let chunked_out = drain_left_channel(&mut chunked, 37);

        let mut bulk = WavDecoder::open(bytes, OUTPUT_RATE).expect("valid wav must open");
        // 総フレーム数より大きい一括読み出し(1回で読み切れるサイズ)。
        let bulk_out = drain_left_channel(&mut bulk, 100_000);

        assert_eq!(
            chunked_out, bulk_out,
            "reading in small chunks must produce bit-identical output to a single bulk read"
        );
    }

    #[test]
    fn resample_bypasses_when_rates_match() {
        // レート一致時のバイパス。一致時は素材の PCM が
        // そのまま(丸め誤差の入り込む余地なく)出てくることを見る。
        let samples: Vec<i16> = vec![0, 0, 16384, -16384, -16384, 16384, 32767, -32768];
        let bytes = make_pcm16_wav(48_000, 2, &samples);
        let mut decoder = WavDecoder::open(bytes, 48_000).expect("valid wav must open");

        let mut out = vec![0.0f32; 4 * CHANNELS];
        let n = decoder.read(&mut out).expect("read must succeed");
        assert_eq!(n, 4);
        // バイパス経路はリサンプラの丸め誤差を一切持ち込まないので、ビット単位で一致する。
        assert_eq!(out[2], i16_to_f32(16384));
        assert_eq!(out[3], i16_to_f32(-16384));
        assert_eq!(out[6], i16_to_f32(32767));
        assert_eq!(out[7], -1.0);
    }

    #[test]
    fn seek_after_resample_matches_a_fresh_decoder_seeked_to_the_same_position() {
        // リサンプル併用時のシーク調停。
        // 「途中まで読んでからシークした場合」と「開いた直後にシークした場合」とで、
        // シーク後の出力が完全一致することを見る(`StreamResampler::reset` が
        // オーバーラップ状態を正しく破棄できているかの検証。混ざっていれば食い違う)。
        const SOURCE_RATE: u32 = 44_100;
        const OUTPUT_RATE: u32 = 48_000;
        const FRAME_COUNT: usize = 8_820; // 200ms

        let bytes = make_sine_wave_wav(SOURCE_RATE, 1_000.0, FRAME_COUNT, 20_000);
        let target = resample::convert_frame_count(2_000, SOURCE_RATE, OUTPUT_RATE);

        let mut warmed = WavDecoder::open(bytes.clone(), OUTPUT_RATE).expect("valid wav must open");
        let mut warm_buf = vec![0.0f32; 500 * CHANNELS];
        warmed
            .read(&mut warm_buf)
            .expect("warm-up read must succeed");
        warmed.seek(target).expect("seek must succeed");
        let after_warm = drain_left_channel(&mut warmed, 333);

        let mut fresh = WavDecoder::open(bytes, OUTPUT_RATE).expect("valid wav must open");
        fresh.seek(target).expect("seek must succeed");
        let after_fresh = drain_left_channel(&mut fresh, 333);

        assert_eq!(
            after_warm, after_fresh,
            "seeking must fully discard prior resampler state regardless of playback history"
        );
    }

    #[test]
    fn end_to_end_resamples_a_44_1k_wav_through_pump_and_read_without_error() {
        // `stream.rs` の pump/read 経路は `stream.rs` 側の
        // end-to-end テストで直接カバーする。ここでは decode.rs 単体として、
        // レート不一致の素材がエラーにならず最後まで読み切れることだけ最小限に確認する。
        const SOURCE_RATE: u32 = 44_100;
        const OUTPUT_RATE: u32 = 48_000;
        let bytes = make_sine_wave_wav(SOURCE_RATE, 1_000.0, 2_000, 10_000);
        let mut decoder =
            WavDecoder::open(bytes, OUTPUT_RATE).expect("mismatched sample rate must now succeed");
        let left = drain_left_channel(&mut decoder, 256);
        assert!(!left.is_empty());
    }
}
