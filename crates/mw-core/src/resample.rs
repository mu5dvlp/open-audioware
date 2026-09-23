//! サンプルレート変換(初期構築仕様『§4.7 デコードとリサンプリング』, M2)。
//!
//! 素材のサンプルレートと出力デバイスのサンプルレートが一致しない場合に、両者を
//! **rubato** で吸収する。用途が2つあり、要求される特性が違うため、あえて別のリサンプラを
//! 選んでいる(依頼書の設計判断1)。
//!
//! - **楽曲(ストリーミング・固定ブロック)**: [`StreamResampler`] が
//!   [`rubato::FftFixedInOut`] を使う。入力・出力とも**固定フレーム数**(サンプルレートの
//!   比から決まる)で処理できる同期(FFT ベース)リサンプラで、比を実行時に変える必要が
//!   無い今回の用途(§4.7 は再生速度変更を範囲外としている。MU5 は別機能)に対して
//!   計算コストが小さい。デコードスレッド上で `pump()` のたびに何度も呼ばれる経路なので、
//!   `SincFixedIn` の畳み込みよりこちらを優先した。
//!   欠点: 内部 FFT サイズは `gcd(source_rate, output_rate)` に依存するため、互いに素に近い
//!   レート同士(非標準のサンプルレート)だと巨大な FFT になりうる。48kHz/44.1kHz/32kHz/
//!   22.05kHz/16kHz といった一般的な組み合わせでは gcd が大きく実用上問題にならないが、
//!   非標準レートの素材を扱うようになった場合はここを見直すこと(判断理由として報告)。
//! - **SE(ロード時・一括)**: [`resample_oneshot`] が [`rubato::SincFixedIn`] を高品質設定
//!   (長いシンク長・高いオーバーサンプリング係数)で使う。ロード時の一度きりのコストなので
//!   計算量よりも品質(ストップバンド減衰・エイリアシング抑制)を優先する。`SincFixedIn` は
//!   `FftFixedInOut` と違って比が `gcd` に縛られないため、どんなレートの組でも安全に使える
//!   (SE は楽曲よりゲームプレイに紐づく短い素材が多く、想定外レートの持ち込みも
//!   起こりやすいため、こちらは頑健さを優先する)。
//!
//! # ブロック境界の連続性(依頼書の設計判断2)
//!
//! [`StreamResampler`] はストリーム(1曲)につき1個だけ生成し、`pump()` が呼ばれるたびに
//! **同じインスタンスを使い回す**。`FftFixedInOut` はブロックをまたぐオーバーラップを
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
//! 四捨五入で変換する。`decode.rs::SymphoniaDecoder` はこれを使って
//! 「素材の総フレーム数 → 出力レート換算の総フレーム数」「シーク要求(出力レート)→
//! 素材側のシーク位置(素材レート)」の両方向を変換する。リサンプラはブロック単位でしか
//! 出力できないため実際に生成できるフレーム数は端数ぶん `total_frames` を超えうるが、
//! 呼び出し側は `total_frames` に達した時点で読み出しを止める(既存の自然終了ロジックと
//! 同じ考え方)ため、超過分は単に配られない。

use std::fmt;

use rubato::{
    FftFixedInOut, Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType,
    WindowFunction, calculate_cutoff,
};

use crate::format::CHANNELS;

/// 楽曲ストリーミング用リサンプラの目標入力チャンク長(フレーム数)。
///
/// 実際の内部 FFT サイズは `gcd(source_rate, output_rate)` の倍数に丸められるため
/// この値そのものにはならないが、一般的なサンプルレートの組み合わせでは
/// 数十ミリ秒程度のブロックに収まる(モジュール doc 参照)。rubato 自身の例
/// (`examples/process_f64.rs`)が使っているデフォルト値と揃えてある。
const STREAM_CHUNK_TARGET_FRAMES: usize = 1024;

/// SE 一括リサンプル用 `SincFixedIn` の入力チャンク長(フレーム数)。
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
}

impl fmt::Display for ResampleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResampleError::Construction(msg) => write!(f, "resampler construction failed: {msg}"),
            ResampleError::Processing(msg) => write!(f, "resampling failed: {msg}"),
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
        }
    }

    #[test]
    fn resample_error_display_identifies_every_variant_without_duplicates() {
        let errors = [
            ResampleError::Construction("zero rate".into()),
            ResampleError::Processing("short input".into()),
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
            let detail = match error {
                ResampleError::Construction(detail) | ResampleError::Processing(detail) => detail,
            };
            let message = error.to_string();
            assert!(
                message.contains(detail.as_str()),
                "{error:?} must print the underlying detail ({detail}) so the device log \
                 says what was actually wrong: {message}"
            );
        }
    }
}

/// `from_rate` 基準のフレーム数を `to_rate` 基準へ変換する(四捨五入)。
///
/// `total_frames`・シーク位置の両方でこの1つの関数だけを使うことで、丸め規則を
/// 常に一致させる(依頼書の設計判断3: 「シーク位置の指定も出力レート基準で一貫させる」)。
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

/// インターリーブ PCM を平面(チャンネルごとの `Vec`)へ書き出す。
/// `planar` の各要素は少なくとも `frames` フレームぶんの長さを持っていること。
fn deinterleave(input: &[f32], frames: usize, planar: &mut [Vec<f32>]) {
    debug_assert_eq!(planar.len(), CHANNELS);
    for i in 0..frames {
        for (ch, plane) in planar.iter_mut().enumerate() {
            plane[i] = input[i * CHANNELS + ch];
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

/// 楽曲ストリーミング用のリサンプラ(`FftFixedInOut` ベース)。
///
/// `decode.rs::SymphoniaDecoder` が1曲につき1個だけ保持し、`pump()` の呼び出しを
/// またいで使い回す(モジュール doc「ブロック境界の連続性」)。
pub struct StreamResampler {
    inner: FftFixedInOut<f32>,
    /// 素材レートの入力を書き込む平面バッファ(固定長 = `inner.input_frames_next()`)。
    chan_in: Vec<Vec<f32>>,
    /// 出力レートの結果を受け取る平面バッファ(固定長 = `inner.output_frames_max()`)。
    chan_out: Vec<Vec<f32>>,
    /// 起動直後・`reset()` 直後にまだ読み捨てていない遅延フレーム数。
    delay_to_skip: usize,
}

impl StreamResampler {
    /// `source_rate != output_rate` のときだけ呼ぶこと(一致する場合は
    /// `decode.rs` 側でバイパスし、このリサンプラ自体を作らない。依頼書の設計判断4)。
    pub fn new(source_rate: u32, output_rate: u32) -> Result<Self, ResampleError> {
        let inner = FftFixedInOut::<f32>::new(
            source_rate as usize,
            output_rate as usize,
            STREAM_CHUNK_TARGET_FRAMES,
            CHANNELS,
        )
        .map_err(|e| ResampleError::Construction(e.to_string()))?;

        let chan_in = inner.input_buffer_allocate(true);
        let chan_out = inner.output_buffer_allocate(true);
        let delay_to_skip = inner.output_delay();

        Ok(Self {
            inner,
            chan_in,
            chan_out,
            delay_to_skip,
        })
    }

    /// 次の [`Self::process_full_chunk_into`] が要求する、素材レートの入力フレーム数。
    /// `FftFixedInOut` は固定ブロックなのでストリーム全体を通じて一定の値を返す。
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

        let (_, n_out) = self
            .inner
            .process_into_buffer(&self.chan_in, &mut self.chan_out, None)
            .map_err(|e| ResampleError::Processing(e.to_string()))?;
        append_resampled_output(&self.chan_out, n_out, &mut self.delay_to_skip, out);
        Ok(())
    }

    /// 素材側が末尾に到達した(`remaining_input` フレームぶんしか残っていない。
    /// 0 フレームでもよい)ときに一度だけ呼ぶ。残りを無音でパディングして最後の
    /// ブロックを変換し、リサンプラの内部に残っていたオーバーラップの尾も
    /// 一緒に吐き出す(rubato の `process_partial_into_buffer` の契約。モジュール doc 参照)。
    pub fn flush_into(
        &mut self,
        remaining_input: &[f32],
        out: &mut Vec<f32>,
    ) -> Result<(), ResampleError> {
        let valid_frames = remaining_input.len() / CHANNELS;
        let (_, n_out) = if valid_frames == 0 {
            // rubato 0.15 の `process_partial_into_buffer` 既定実装は、`Some` で渡した
            // チャンネルの有効長が 0 だと(パディング済みのゼロ埋めではなく)そのチャンネルの
            // バッファを `clear()` してしまい、後続の `process_into_buffer` の
            // 長さチェックに落ちる(空チャンネル入力を想定していない実装上のクセ)。
            // 「入力なし」を表す正規のパスである `None` を使えばこの分岐を踏まずに済む
            // (ドキュメントにも「入力バッファ無しで呼べる」と明記されている)。
            self.inner
                .process_partial_into_buffer::<&[f32], _>(None, &mut self.chan_out, None)
        } else {
            deinterleave(remaining_input, valid_frames, &mut self.chan_in);
            let sliced: Vec<&[f32]> = self
                .chan_in
                .iter()
                .map(|chan| &chan[..valid_frames])
                .collect();
            self.inner
                .process_partial_into_buffer(Some(&sliced), &mut self.chan_out, None)
        }
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

/// SE ロード時の一括リサンプル(`SincFixedIn` ベース、依頼書の設計判断1)。
///
/// `source_rate != output_rate` のときだけ呼ぶこと(一致する場合は `wav.rs` 側で
/// バイパスする。設計判断4)。戻り値は `(出力レートのインターリーブ PCM, 出力フレーム数)`。
/// 出力フレーム数は常に `convert_frame_count(frames, source_rate, output_rate)` に一致する
/// (末尾のブロック丸めによる超過分は切り詰め、逆に不足することがあれば無音で埋める。
/// `SoundData::frames == interleaved.len() / CHANNELS` の不変条件を壊さないため)。
pub fn resample_oneshot(
    input: &[f32],
    frames: usize,
    source_rate: u32,
    output_rate: u32,
) -> Result<(Vec<f32>, usize), ResampleError> {
    let ratio = output_rate as f64 / source_rate as f64;
    let f_cutoff = calculate_cutoff::<f32>(ONESHOT_SINC_LEN, ONESHOT_WINDOW);
    let params = SincInterpolationParameters {
        sinc_len: ONESHOT_SINC_LEN,
        f_cutoff,
        interpolation: ONESHOT_INTERPOLATION,
        oversampling_factor: ONESHOT_OVERSAMPLING_FACTOR,
        window: ONESHOT_WINDOW,
    };
    // max_relative_ratio: SE ロード時に比を変える機能は無い(MU5 のエディタ再生速度変更は
    // 別機能・別スコープ)ため、構築時の比のまま固定してよい最小値の 1.0 を渡す。
    let mut resampler = SincFixedIn::<f32>::new(ratio, 1.0, params, ONESHOT_CHUNK_FRAMES, CHANNELS)
        .map_err(|e| ResampleError::Construction(e.to_string()))?;

    let mut chan_in = vec![vec![0.0f32; ONESHOT_CHUNK_FRAMES]; CHANNELS];
    let mut chan_out = resampler.output_buffer_allocate(true);
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
        let (_, n_out) = resampler
            .process_into_buffer(&chan_in, &mut chan_out, None)
            .map_err(|e| ResampleError::Processing(e.to_string()))?;
        append_resampled_output(&chan_out, n_out, &mut delay_to_skip, &mut out);
        pos += ONESHOT_CHUNK_FRAMES;
    }

    // 最後の端数(0 フレームのこともある)。`process_partial_into_buffer` が
    // 内部で無音パディングしたうえで、リサンプラに残っていた尾も一緒に吐き出す。
    // `remaining == 0` のときは `Some(空スライス)` ではなく `None` を渡す
    // (`StreamResampler::flush_into` のコメント参照: rubato 0.15 の既定実装は
    // 有効長 0 のチャンネル入力をゼロ埋めではなく空バッファへ潰してしまうクセがある)。
    let remaining = frames - pos;
    let (_, n_out) = if remaining == 0 {
        resampler.process_partial_into_buffer::<&[f32], _>(None, &mut chan_out, None)
    } else {
        deinterleave(
            &input[pos * CHANNELS..frames * CHANNELS],
            remaining,
            &mut chan_in,
        );
        let sliced: Vec<&[f32]> = chan_in.iter().map(|chan| &chan[..remaining]).collect();
        resampler.process_partial_into_buffer(Some(&sliced), &mut chan_out, None)
    }
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
}
