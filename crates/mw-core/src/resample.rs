//! サンプルレート変換(初期構築仕様『§4.7 デコードとリサンプリング』, M2)。
//!
//! 素材のサンプルレートと出力デバイスのサンプルレートが一致しない場合に、両者を
//! **rubato** で吸収する。用途が2つあり、要求される特性が違うため、あえて別のリサンプラを
//! 選んでいる(用途ごとに必要な特性が異なるため)。
//!
//! - **楽曲(ストリーミング・固定ブロック)**: [`StreamResampler`] が
//!   [`rubato::Fft`] を `FixedSync::Both` で使う。入力・出力とも**固定フレーム数**(サンプルレートの
//!   比から決まる)で処理できる同期(FFT ベース)リサンプラで、比を実行時に変える必要が
//!   無い今回の用途(§4.7 は再生速度変更を範囲外としている。MU5 は別機能)に対して
//!   計算コストが小さい。デコードスレッド上で `pump()` のたびに何度も呼ばれる経路なので、
//!   非同期 sinc の畳み込みよりこちらを優先した。
//!   欠点: 内部 FFT サイズは `gcd(source_rate, output_rate)` に依存するため、互いに素に近い
//!   レート同士(非標準のサンプルレート)だと巨大な FFT になりうる。48kHz/44.1kHz/32kHz/
//!   22.05kHz/16kHz といった一般的な組み合わせでは gcd が大きく実用上問題にならないが、
//!   非標準レートの素材を扱うようになった場合はここを見直すこと(判断理由として報告)。
//! - **SE(ロード時・一括)**: [`resample_oneshot`] が [`rubato::Async`] を `FixedAsync::Input` と
//!   高品質な sinc 設定で
//!   (長いシンク長・高いオーバーサンプリング係数)で使う。ロード時の一度きりのコストなので
//!   計算量よりも品質(ストップバンド減衰・エイリアシング抑制)を優先する。非同期 sinc は
//!   FFT ベースと違って比が `gcd` に縛られないため、どんなレートの組でも安全に使える
//!   (SE は楽曲よりゲームプレイに紐づく短い素材が多く、想定外レートの持ち込みも
//!   起こりやすいため、こちらは頑健さを優先する)。
//!
//! # ブロック境界の連続性
//!
//! [`StreamResampler`] はストリーム(1曲)につき1個だけ生成し、`pump()` が呼ばれるたびに
//! **同じインスタンスを使い回す**。`Fft` はブロックをまたぐオーバーラップを
//! 内部状態(`overlaps`)として保持しており、これが毎回のブロック処理をまたいで
//! 引き継がれることで継ぎ目の不連続(プチノイズ)を防いでいる。呼び出し側が
//! ブロックのたびに新しいリサンプラを作ってしまうとこの保証が崩れるため、
//! **絶対にやってはいけない**。
//!
//! シーク(`StreamResampler::reset`)は逆に**意図的な不連続**なので、
//! オーバーラップ状態を素の 0 へ戻す(rubato 自身が提供する `Resampler::reset()`)。
//! シーク前後の音を混ぜてしまうバグを避けるための必須ステップ。
//!
//! # 遅延(レイテンシ)の扱い(設計判断2・3にまたがる)
//!
//! FFT ベース・sinc ベースいずれのリサンプラも、フィルタのウォームアップ分だけ
//! 出力側に遅延(`Resampler::output_delay()`)を持つ(rubato 公式の
//! `examples/process_f64.rs` も出力を書き出す際にこの分だけ先頭を捨てている)。
//! ここでは呼び出し側(`decode.rs`)にこの遅延を一切見せない方針にした: 生成直後・
//! `reset()` 直後の出力から `output_delay()` フレームぶんを内部で読み捨ててから
//! 呼び出し側へ渡す。こうすることで「素材の時刻 0 が出力の時刻 0 に対応する」という
//! 単純な前提を上位層(`decode.rs`・`stream.rs`・`music.rs`)がそのまま使い続けられる。
//!
//! # 総フレーム数・シーク位置は出力レート基準(設計判断3)
//!
//! [`convert_frame_count`] は `from_rate` 基準のフレーム数を `to_rate` 基準へ
//! 四捨五入で変換する。`decode.rs::WavDecoder` はこれを使って
//! 「素材の総フレーム数 → 出力レート換算の総フレーム数」「シーク要求(出力レート)→
//! 素材側のシーク位置(素材レート)」の両方向を変換する。リサンプラはブロック単位でしか
//! 出力できないため実際に生成できるフレーム数は端数ぶん `total_frames` を超えうるが、
//! 呼び出し側は `total_frames` に達した時点で読み出しを止める(既存の自然終了ロジックと
//! 同じ考え方)ため、超過分は単に配られない。

use std::fmt;

use rubato::audioadapter_buffers::direct::SequentialSliceOfVecs;
use rubato::{
    Async, Fft, FixedAsync, FixedSync, Indexing, Resampler, SincInterpolationParameters,
    SincInterpolationType, WindowFunction, calculate_cutoff,
};

use crate::format::CHANNELS;

/// 楽曲ストリーミング用リサンプラの目標入力チャンク長(フレーム数)。
///
/// 実際の内部 FFT サイズは `gcd(source_rate, output_rate)` の倍数に丸められるため
/// この値そのものにはならないが、一般的なサンプルレートの組み合わせでは
/// 数十ミリ秒程度のブロックに収まる(モジュール doc 参照)。rubato 自身の例
/// (`examples/process_f64.rs`)が使っているデフォルト値と揃えてある。
const STREAM_CHUNK_TARGET_FRAMES: usize = 1024;

/// SE 一括リサンプル用 `Async` sinc の入力チャンク長(フレーム数)。
/// ロード時の一度きりの処理なので大きめに取って呼び出し回数を減らす。
const ONESHOT_CHUNK_FRAMES: usize = 4096;
/// SE 一括リサンプルのシンク長。大きいほど高品質・低速(ロード時のみのコストなので許容)。
const ONESHOT_SINC_LEN: usize = 256;
/// SE 一括リサンプルのオーバーサンプリング係数(中間点の細かさ)。
const ONESHOT_OVERSAMPLING_FACTOR: usize = 256;
/// SE 一括リサンプルの窓関数。ロールオフより減衰(エイリアシング抑制)を優先する。
const ONESHOT_WINDOW: WindowFunction = WindowFunction::BlackmanHarris2;
/// SE 一括リサンプルの補間方式。線形/最近傍より高品質な三次補間を選ぶ。
const ONESHOT_INTERPOLATION: SincInterpolationType = SincInterpolationType::Cubic;

/// 許容するサンプルレート比(大きい方 / 小さい方)の上限。
///
/// `StreamResampler`(`rubato::Fft`、`FixedSync::Both`)は出力側の FFT サイズがおおよそ
/// `STREAM_CHUNK_TARGET_FRAMES * 比`、`resample_oneshot`(`rubato::Async` sinc、
/// `FixedAsync::Input`)は出力バッファがおおよそ `ONESHOT_CHUNK_FRAMES * 比` になる。
/// 比が極端(例: 1Hz と 384,000Hz)だとどちらも確保が破綻するため、`validate_rates` で
/// リサンプラを構築する前に弾く。現実的な音声のサンプルレート(8kHz〜384kHz程度)の
/// 組み合わせはこの比に十分収まる。
const MAX_RATE_RATIO: u32 = 256;

/// レートの組み合わせが安全かを検査する。`StreamResampler::new` と `resample_oneshot` の
/// 両方が、リサンプラを構築する・比を計算する前に必ずこれを通す。
///
/// - どちらかが 0 だと比が定義できない(0 除算、または無限大の比になる)。
/// - 比が `MAX_RATE_RATIO` を超えると、リサンプラ内部のバッファ確保が比に比例して
///   膨れ上がり、OOM やタイムアウトを起こしうる(`MAX_RATE_RATIO` のドキュメント参照)。
fn validate_rates(source_rate: u32, output_rate: u32) -> Result<(), ResampleError> {
    let invalid = || ResampleError::InvalidRates {
        source_rate,
        output_rate,
    };
    if source_rate == 0 || output_rate == 0 {
        return Err(invalid());
    }
    let (hi, lo) = if source_rate >= output_rate {
        (source_rate, output_rate)
    } else {
        (output_rate, source_rate)
    };
    if hi / lo > MAX_RATE_RATIO {
        return Err(invalid());
    }
    Ok(())
}

/// リサンプル処理で発生しうるエラー。
///
/// rubato のエラー型(`ResamplerConstructionError`/`ResampleError`)は `Clone`/`PartialEq`
/// を実装しないため、`decode.rs::DecodeError`/`wav.rs::WavError` と同じ流儀で文字列化して
/// 保持する。
#[derive(Debug, Clone, PartialEq)]
pub enum ResampleError {
    /// リサンプラの構築に失敗した(サンプルレートが 0 等。通常の入力では起こらない)。
    Construction(String),
    /// 変換処理そのものが失敗した(バッファサイズ不一致等。通常の入力では起こらない)。
    Processing(String),
    /// レートの組み合わせが不正(どちらかが 0、または比〔大きい方 / 小さい方〕が
    /// `MAX_RATE_RATIO` を超える)。`validate_rates` がリサンプラの構築より前に弾く。
    InvalidRates { source_rate: u32, output_rate: u32 },
}

impl fmt::Display for ResampleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResampleError::Construction(msg) => write!(f, "resampler construction failed: {msg}"),
            ResampleError::Processing(msg) => write!(f, "resampling failed: {msg}"),
            ResampleError::InvalidRates {
                source_rate,
                output_rate,
            } => write!(
                f,
                "invalid resample rates (source: {source_rate} Hz, output: {output_rate} Hz): \
                 both must be > 0 and their ratio must not exceed {MAX_RATE_RATIO}x"
            ),
        }
    }
}

impl std::error::Error for ResampleError {}

#[cfg(test)]
mod display_tests {
    use super::*;

    /// `ResampleError` にバリアントを追加したときの表示固定をコンパイル時に要求する。
    fn describe(error: &ResampleError) -> &'static str {
        match error {
            ResampleError::Construction(_) => "resampler construction failed",
            ResampleError::Processing(_) => "resampling failed",
            ResampleError::InvalidRates { .. } => "invalid resample rates",
        }
    }

    #[test]
    fn resample_error_display_identifies_every_variant_without_duplicates() {
        let errors = [
            ResampleError::Construction("zero rate".into()),
            ResampleError::Processing("short input".into()),
            ResampleError::InvalidRates {
                source_rate: 0,
                output_rate: 48_000,
            },
        ];
        let rendered: Vec<String> = errors
            .iter()
            .map(|error| {
                let message = error.to_string();
                assert!(
                    message.contains(describe(error)),
                    "{error:?} must retain its identifying phrase: {message}"
                );
                message
            })
            .collect();

        assert!(
            rendered.iter().enumerate().all(|(index, message)| rendered
                .iter()
                .enumerate()
                .all(|(other_index, other)| index == other_index || message != other)),
            "each ResampleError variant must have a distinct display message: {rendered:?}"
        );
        // 引数(内側の詳細メッセージ)が文言に出ていることも見る。
        // 🔴 添字ではなく `match` で取り出すこと(理由は `wav.rs` の同じ検査のコメント参照)。
        for error in &errors {
            let message = error.to_string();
            match error {
                ResampleError::Construction(detail) | ResampleError::Processing(detail) => {
                    assert!(
                        message.contains(detail.as_str()),
                        "{error:?} must print the underlying detail ({detail}) so the device \
                         log says what was actually wrong: {message}"
                    );
                }
                ResampleError::InvalidRates {
                    source_rate,
                    output_rate,
                } => {
                    assert!(
                        message.contains(&source_rate.to_string())
                            && message.contains(&output_rate.to_string()),
                        "{error:?} must print both rates so the device log says what was \
                         actually wrong: {message}"
                    );
                }
            }
        }
    }
}

/// `from_rate` 基準のフレーム数を `to_rate` 基準へ変換する(四捨五入)。
///
/// `total_frames`・シーク位置の両方でこの1つの関数だけを使うことで、丸め規則を
/// 常に一致させる(シーク位置の指定も出力レート基準で一貫させる)。
/// `u128` で計算してから戻すのは、長時間の楽曲(数億フレーム)でも `u64` の乗算が
/// オーバーフローしないようにするため。
pub fn convert_frame_count(frames: u64, from_rate: u32, to_rate: u32) -> u64 {
    if from_rate == to_rate || frames == 0 {
        return frames;
    }
    let numerator = frames as u128 * to_rate as u128;
    let denominator = from_rate as u128;
    ((numerator + denominator / 2) / denominator) as u64
}

/// rubato へ渡す入力サンプルの絶対値上限。
///
/// rubato の `Fft`(realfft 経由)は NaN / inf や極端に大きい値を含む入力に対して、
/// 内部の `unwrap()` で abort することがある(戻り値の `Result` では受け止められない)。
/// 音声として意味のある値はこの範囲に収まるため、rubato へ渡す前にここで正規化する。
const MAX_ABS_INPUT_SAMPLE: f32 = 1.0e4;

/// rubato へ渡す前にサンプル1個を正規化する(`deinterleave` からのみ呼ぶ)。
///
/// 非有限(NaN / ±inf)は無音(0.0)へ、有限でも [`MAX_ABS_INPUT_SAMPLE`] を超える値は
/// その範囲へ clamp する。rubato の `Fft` が内部の `unwrap()` で abort するのを防ぐための
/// 唯一の正規化ポイント(`process_full_chunk_into`/`flush_into`/`resample_oneshot` の
/// いずれも入力はここを通ってから rubato へ渡る)。
fn sanitize_sample(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(-MAX_ABS_INPUT_SAMPLE, MAX_ABS_INPUT_SAMPLE)
    } else {
        0.0
    }
}

/// インターリーブ PCM を平面(チャンネルごとの `Vec`)へ書き出す。
/// `planar` の各要素は少なくとも `frames` フレームぶんの長さを持っていること。
///
/// rubato へ渡す入力はすべてこの関数を経由するため、[`sanitize_sample`] による正規化を
/// ここで一括して行う(呼び出し側ごとに個別の正規化パスを増やさない)。
fn deinterleave(input: &[f32], frames: usize, planar: &mut [Vec<f32>]) {
    debug_assert_eq!(planar.len(), CHANNELS);
    for i in 0..frames {
        for (ch, plane) in planar.iter_mut().enumerate() {
            plane[i] = sanitize_sample(input[i * CHANNELS + ch]);
        }
    }
}

/// 平面バッファの `[skip, n_out)` 区間をインターリーブしながら `out` へ積む
/// (`skip` はリサンプラの起動直後の遅延を読み捨てるための量。モジュール doc 参照)。
fn append_resampled_output(
    planar: &[Vec<f32>],
    n_out: usize,
    delay_to_skip: &mut usize,
    out: &mut Vec<f32>,
) {
    let skip = (*delay_to_skip).min(n_out);
    *delay_to_skip -= skip;
    out.reserve((n_out - skip) * CHANNELS);
    for i in skip..n_out {
        for plane in planar {
            out.push(plane[i]);
        }
    }
}

/// 楽曲ストリーミング用のリサンプラ(`Fft` + `FixedSync::Both` ベース)。
///
/// `decode.rs::WavDecoder` が1曲につき1個だけ保持し、`pump()` の呼び出しを
/// またいで使い回す(モジュール doc「ブロック境界の連続性」)。
pub struct StreamResampler {
    inner: Fft<f32>,
    /// 素材レートの入力を書き込む平面バッファ(固定長 = `inner.input_frames_next()`)。
    chan_in: Vec<Vec<f32>>,
    /// 出力レートの結果を受け取る平面バッファ(固定長 = `inner.output_frames_max()`)。
    chan_out: Vec<Vec<f32>>,
    /// 起動直後・`reset()` 直後にまだ読み捨てていない遅延フレーム数。
    delay_to_skip: usize,
}

impl StreamResampler {
    /// `source_rate != output_rate` のときだけ呼ぶこと(一致する場合は
    /// `decode.rs` 側でバイパスし、このリサンプラ自体を作らない)。
    ///
    /// レートが 0、または比(大きい方 / 小さい方)が `MAX_RATE_RATIO` を超える場合は
    /// `ResampleError::InvalidRates` を返す([`validate_rates`] 参照)。
    pub fn new(source_rate: u32, output_rate: u32) -> Result<Self, ResampleError> {
        validate_rates(source_rate, output_rate)?;
        let inner = Fft::<f32>::new(
            source_rate as usize,
            output_rate as usize,
            STREAM_CHUNK_TARGET_FRAMES,
            CHANNELS,
            FixedSync::Both,
        )
        .map_err(|e| ResampleError::Construction(e.to_string()))?;

        let chan_in = vec![vec![0.0; inner.input_frames_next()]; CHANNELS];
        let chan_out = vec![vec![0.0; inner.output_frames_max()]; CHANNELS];
        let delay_to_skip = inner.output_delay();

        Ok(Self {
            inner,
            chan_in,
            chan_out,
            delay_to_skip,
        })
    }

    /// 次の [`Self::process_full_chunk_into`] が要求する、素材レートの入力フレーム数。
    /// `Fft` を `FixedSync::Both` で構築しているため、ストリーム全体を通じて一定の値を返す。
    pub fn input_frames_needed(&self) -> usize {
        self.inner.input_frames_next()
    }

    /// `input_frames_needed()` フレームぶんの素材レート PCM(インターリーブ)を変換し、
    /// 出力レート PCM を `out` の末尾へ積む。
    pub fn process_full_chunk_into(
        &mut self,
        input: &[f32],
        out: &mut Vec<f32>,
    ) -> Result<(), ResampleError> {
        let need = self.input_frames_needed();
        debug_assert_eq!(input.len(), need * CHANNELS);
        deinterleave(input, need, &mut self.chan_in);

        let input = SequentialSliceOfVecs::new(&self.chan_in, CHANNELS, need)
            .map_err(|e| ResampleError::Processing(e.to_string()))?;
        let output_frames = self.inner.output_frames_max();
        let mut output =
            SequentialSliceOfVecs::new_mut(&mut self.chan_out, CHANNELS, output_frames)
                .map_err(|e| ResampleError::Processing(e.to_string()))?;
        let (_, n_out) = self
            .inner
            .process_into_buffer(&input, &mut output, None)
            .map_err(|e| ResampleError::Processing(e.to_string()))?;
        append_resampled_output(&self.chan_out, n_out, &mut self.delay_to_skip, out);
        Ok(())
    }

    /// 素材側が末尾に到達した(`remaining_input` フレームぶんしか残っていない。
    /// 0 フレームでもよい)ときに一度だけ呼ぶ。残りを無音でパディングして最後の
    /// ブロックを変換し、リサンプラの内部に残っていたオーバーラップの尾も
    /// 一緒に吐き出す(rubato 5 の `Indexing::partial_len` の契約。モジュール doc 参照)。
    pub fn flush_into(
        &mut self,
        remaining_input: &[f32],
        out: &mut Vec<f32>,
    ) -> Result<(), ResampleError> {
        let valid_frames = remaining_input.len() / CHANNELS;
        if valid_frames != 0 {
            deinterleave(remaining_input, valid_frames, &mut self.chan_in);
        }
        // `partial_len = Some(0)` は rubato 5 の契約で「入力を全て無音として
        // パディングする」を表す。旧 API の `None` 入力相当であり、空のスライスを
        // 毎回組み立てる必要がない。
        let input = SequentialSliceOfVecs::new(&self.chan_in, CHANNELS, self.input_frames_needed())
            .map_err(|e| ResampleError::Processing(e.to_string()))?;
        let output_frames = self.inner.output_frames_max();
        let mut output =
            SequentialSliceOfVecs::new_mut(&mut self.chan_out, CHANNELS, output_frames)
                .map_err(|e| ResampleError::Processing(e.to_string()))?;
        let indexing = Indexing::new().partial_len(valid_frames);
        let (_, n_out) = self
            .inner
            .process_into_buffer(&input, &mut output, Some(&indexing))
            .map_err(|e| ResampleError::Processing(e.to_string()))?;
        append_resampled_output(&self.chan_out, n_out, &mut self.delay_to_skip, out);
        Ok(())
    }

    /// シーク直後に呼ぶ。オーバーラップ状態を 0 へ戻し、遅延読み捨てをやり直す
    /// (モジュール doc「ブロック境界の連続性」: シークは意図的な不連続であり、
    /// 直前までの重なりを引き継ぐと無関係な音が混ざってしまう)。
    pub fn reset(&mut self) {
        self.inner.reset();
        self.delay_to_skip = self.inner.output_delay();
    }
}

/// SE ロード時の一括リサンプル(`Async` sinc ベース)。
///
/// `source_rate != output_rate` のときだけ呼ぶこと(一致する場合は `wav.rs` 側で
/// バイパスする。設計判断4)。戻り値は `(出力レートのインターリーブ PCM, 出力フレーム数)`。
/// 出力フレーム数は常に `convert_frame_count(frames, source_rate, output_rate)` に一致する
/// (末尾のブロック丸めによる超過分は切り詰め、逆に不足することがあれば無音で埋める。
/// `SoundData::frames == interleaved.len() / CHANNELS` の不変条件を壊さないため)。
///
/// レートが 0、または比(大きい方 / 小さい方)が `MAX_RATE_RATIO` を超える場合は
/// `ResampleError::InvalidRates` を返す([`validate_rates`] 参照。`source_rate == 0` を
/// 検査せず比を計算すると無限大になり、後段のバッファ確保が破綻するため必須)。
pub fn resample_oneshot(
    input: &[f32],
    frames: usize,
    source_rate: u32,
    output_rate: u32,
) -> Result<(Vec<f32>, usize), ResampleError> {
    validate_rates(source_rate, output_rate)?;
    let ratio = output_rate as f64 / source_rate as f64;
    let f_cutoff = calculate_cutoff::<f32>(ONESHOT_SINC_LEN, ONESHOT_WINDOW);
    let params = SincInterpolationParameters {
        sinc_len: ONESHOT_SINC_LEN,
        f_cutoff: Some(f_cutoff),
        interpolation: ONESHOT_INTERPOLATION,
        oversampling_factor: ONESHOT_OVERSAMPLING_FACTOR,
        window: ONESHOT_WINDOW,
    };
    // max_relative_ratio: SE ロード時に比を変える機能は無い(MU5 のエディタ再生速度変更は
    // 別機能・別スコープ)ため、構築時の比のまま固定してよい最小値の 1.0 を渡す。
    let mut resampler = Async::<f32>::new_sinc(
        ratio,
        1.0,
        &params,
        ONESHOT_CHUNK_FRAMES,
        CHANNELS,
        FixedAsync::Input,
    )
    .map_err(|e| ResampleError::Construction(e.to_string()))?;

    let mut chan_in = vec![vec![0.0f32; ONESHOT_CHUNK_FRAMES]; CHANNELS];
    let mut chan_out = vec![vec![0.0; resampler.output_frames_max()]; CHANNELS];
    let mut delay_to_skip = resampler.output_delay();
    let mut out = Vec::with_capacity(
        convert_frame_count(frames as u64, source_rate, output_rate) as usize * CHANNELS,
    );

    let mut pos = 0usize;
    while frames - pos >= ONESHOT_CHUNK_FRAMES {
        let chunk_start = pos * CHANNELS;
        let chunk_end = (pos + ONESHOT_CHUNK_FRAMES) * CHANNELS;
        deinterleave(
            &input[chunk_start..chunk_end],
            ONESHOT_CHUNK_FRAMES,
            &mut chan_in,
        );
        let input = SequentialSliceOfVecs::new(&chan_in, CHANNELS, ONESHOT_CHUNK_FRAMES)
            .map_err(|e| ResampleError::Processing(e.to_string()))?;
        let output_frames = resampler.output_frames_max();
        let mut output = SequentialSliceOfVecs::new_mut(&mut chan_out, CHANNELS, output_frames)
            .map_err(|e| ResampleError::Processing(e.to_string()))?;
        let (_, n_out) = resampler
            .process_into_buffer(&input, &mut output, None)
            .map_err(|e| ResampleError::Processing(e.to_string()))?;
        append_resampled_output(&chan_out, n_out, &mut delay_to_skip, &mut out);
        pos += ONESHOT_CHUNK_FRAMES;
    }

    // 最後の端数(0 フレームのこともある)。`partial_len` が内部で無音パディングした
    // うえで、リサンプラに残っていた尾も一緒に吐き出す。
    let remaining = frames - pos;
    let valid_frames = if remaining == 0 {
        0
    } else {
        deinterleave(
            &input[pos * CHANNELS..frames * CHANNELS],
            remaining,
            &mut chan_in,
        );
        remaining
    };
    let input = SequentialSliceOfVecs::new(&chan_in, CHANNELS, ONESHOT_CHUNK_FRAMES)
        .map_err(|e| ResampleError::Processing(e.to_string()))?;
    let output_frames = resampler.output_frames_max();
    let mut output = SequentialSliceOfVecs::new_mut(&mut chan_out, CHANNELS, output_frames)
        .map_err(|e| ResampleError::Processing(e.to_string()))?;
    let indexing = Indexing::new().partial_len(valid_frames);
    let (_, n_out) = resampler
        .process_into_buffer(&input, &mut output, Some(&indexing))
        .map_err(|e| ResampleError::Processing(e.to_string()))?;
    append_resampled_output(&chan_out, n_out, &mut delay_to_skip, &mut out);

    let target_frames = convert_frame_count(frames as u64, source_rate, output_rate) as usize;
    out.resize(target_frames * CHANNELS, 0.0);
    Ok((out, target_frames))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convert_frame_count_is_identity_when_rates_match() {
        assert_eq!(convert_frame_count(12_345, 48_000, 48_000), 12_345);
    }

    #[test]
    fn convert_frame_count_scales_by_rate_ratio() {
        // 48000 -> 44100: 比は 0.91875。丸めの誤差はたかだか1フレーム。
        let converted = convert_frame_count(48_000, 48_000, 44_100);
        assert_eq!(converted, 44_100);
        let converted_back = convert_frame_count(44_100, 44_100, 48_000);
        assert_eq!(converted_back, 48_000);
    }

    #[test]
    fn convert_frame_count_handles_zero() {
        assert_eq!(convert_frame_count(0, 44_100, 48_000), 0);
    }

    /// NaN / inf / 極端な値を混ぜた入力(`sanitize_sample` が正規化する対象)。
    fn adversarial_samples() -> Vec<f32> {
        vec![
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            1.0e9,
            -1.0e9,
            0.0,
            0.3,
            -0.3,
        ]
    }

    /// `StreamResampler` は `Debug`/`PartialEq` を実装しないため(音声コールバック経路の
    /// 型に不要な派生を持ち込まないため)、`assert_eq!` ではなく `match` で `Err` の中身だけ見る。
    fn assert_stream_resampler_new_err(
        source_rate: u32,
        output_rate: u32,
        expected: ResampleError,
    ) {
        match StreamResampler::new(source_rate, output_rate) {
            Err(err) => assert_eq!(err, expected),
            Ok(_) => panic!("expected {expected:?}, got Ok"),
        }
    }

    #[test]
    fn stream_resampler_new_rejects_zero_source_rate() {
        assert_stream_resampler_new_err(
            0,
            48_000,
            ResampleError::InvalidRates {
                source_rate: 0,
                output_rate: 48_000,
            },
        );
    }

    #[test]
    fn stream_resampler_new_rejects_zero_output_rate() {
        assert_stream_resampler_new_err(
            48_000,
            0,
            ResampleError::InvalidRates {
                source_rate: 48_000,
                output_rate: 0,
            },
        );
    }

    #[test]
    fn stream_resampler_new_rejects_rate_ratio_beyond_the_limit() {
        // 48000 / 100 = 480 > MAX_RATE_RATIO(256)。
        assert_stream_resampler_new_err(
            48_000,
            100,
            ResampleError::InvalidRates {
                source_rate: 48_000,
                output_rate: 100,
            },
        );
    }

    #[test]
    // Miri では対象外: rubato(外部クレート)の FFT リサンプラの構築を Miri で解釈すると数分かかる。
    // 検査したいのは自前コードの unsafe(ring_buffer / wav / decode の本体)で、rubato の中身ではない。
    // rubato を自前化(依存排除のステップ2)したら外して、Miri の対象に戻す。
    #[cfg_attr(miri, ignore = "rubato(外部)の FFT を Miri で解釈すると数分かかる")]
    fn stream_resampler_new_accepts_rate_ratio_at_the_limit() {
        // 25600 / 100 = 256 == MAX_RATE_RATIO。境界は許容する。
        assert!(StreamResampler::new(25_600, 100).is_ok());
    }

    #[test]
    fn resample_oneshot_rejects_zero_source_rate() {
        let samples = vec![0.0f32; 8];
        assert_eq!(
            resample_oneshot(&samples, 4, 0, 48_000),
            Err(ResampleError::InvalidRates {
                source_rate: 0,
                output_rate: 48_000,
            })
        );
    }

    #[test]
    fn resample_oneshot_rejects_zero_output_rate() {
        let samples = vec![0.0f32; 8];
        assert_eq!(
            resample_oneshot(&samples, 4, 48_000, 0),
            Err(ResampleError::InvalidRates {
                source_rate: 48_000,
                output_rate: 0,
            })
        );
    }

    #[test]
    fn resample_oneshot_rejects_rate_ratio_beyond_the_limit() {
        let samples = vec![0.0f32; 8];
        assert_eq!(
            resample_oneshot(&samples, 4, 48_000, 100),
            Err(ResampleError::InvalidRates {
                source_rate: 48_000,
                output_rate: 100,
            })
        );
    }

    #[test]
    // Miri では対象外: rubato(外部クレート)の FFT リサンプラを Miri で解釈すると1本で数分〜十数分かかる。
    // 検査したいのは自前コードの unsafe(ring_buffer / wav / decode の本体)で、rubato の中身ではない。
    // rubato を自前化(依存排除のステップ2)したら外して、Miri の対象に戻す。
    #[cfg_attr(miri, ignore = "rubato(外部)の FFT を Miri で解釈すると数分かかる")]
    fn stream_resampler_sanitizes_non_finite_and_extreme_input_upsampling() {
        let mut resampler = StreamResampler::new(44_100, 48_000).expect("valid rates");
        let need = resampler.input_frames_needed();
        let mut input = adversarial_samples();
        input.resize(need * CHANNELS, 0.1);

        let mut out = Vec::new();
        resampler
            .process_full_chunk_into(&input, &mut out)
            .expect("must not error on adversarial input");
        resampler
            .flush_into(&[], &mut out)
            .expect("flush must not error");

        assert!(!out.is_empty());
        assert!(
            out.iter().all(|v| v.is_finite()),
            "output must not contain NaN/inf: {out:?}"
        );
    }

    #[test]
    // Miri では対象外: rubato(外部クレート)の FFT リサンプラを Miri で解釈すると1本で数分〜十数分かかる。
    // 検査したいのは自前コードの unsafe(ring_buffer / wav / decode の本体)で、rubato の中身ではない。
    // rubato を自前化(依存排除のステップ2)したら外して、Miri の対象に戻す。
    #[cfg_attr(miri, ignore = "rubato(外部)の FFT を Miri で解釈すると数分かかる")]
    fn stream_resampler_sanitizes_non_finite_and_extreme_input_downsampling() {
        let mut resampler = StreamResampler::new(48_000, 44_100).expect("valid rates");
        let need = resampler.input_frames_needed();
        let mut input = adversarial_samples();
        input.resize(need * CHANNELS, 0.1);

        let mut out = Vec::new();
        resampler
            .process_full_chunk_into(&input, &mut out)
            .expect("must not error on adversarial input");
        resampler
            .flush_into(&[], &mut out)
            .expect("flush must not error");

        assert!(!out.is_empty());
        assert!(
            out.iter().all(|v| v.is_finite()),
            "output must not contain NaN/inf: {out:?}"
        );
    }

    #[test]
    // Miri では対象外: rubato(外部クレート)の sinc リサンプラを Miri で解釈すると数分かかる。
    // 検査したいのは自前コードの unsafe(ring_buffer / wav / decode の本体)で、rubato の中身ではない。
    // rubato を自前化(依存排除のステップ2)したら外して、Miri の対象に戻す。
    #[cfg_attr(
        miri,
        ignore = "rubato(外部)の sinc リサンプラを Miri で解釈すると数分かかる"
    )]
    fn resample_oneshot_sanitizes_non_finite_and_extreme_input() {
        let input = adversarial_samples();
        let frames = input.len() / CHANNELS;
        let (out, out_frames) = resample_oneshot(&input, frames, 44_100, 48_000)
            .expect("must not error on adversarial input");

        assert_eq!(out.len(), out_frames * CHANNELS);
        assert!(
            out.iter().all(|v| v.is_finite()),
            "output must not contain NaN/inf: {out:?}"
        );
    }
}
