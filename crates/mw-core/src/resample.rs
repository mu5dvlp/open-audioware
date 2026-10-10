//! サンプルレート変換(初期構築仕様『§4.7 デコードとリサンプリング』, M2)。
//!
//! 素材のサンプルレートと出力デバイスのサンプルレートが一致しない場合に、両者を
//! **自前のポリフェーズ窓付き sinc フィルタ**で吸収する(依存排除ステップ2。
//! `docs/adr/0004-polyphase-sinc-resampler.md` に設計判断の詳細)。
//!
//! 用途に関わらず必要な比は **固定の有理数比 L:M**(例: 44.1k↔48k は 147:160)だけであり、
//! 実行時に比を変える機能は無い(§4.7 は再生速度変更を範囲外としている。MU5 は別機能)。
//! そのため FFT や可変比補間は不要で、レート対ごとに1回だけ設計した固定のポリフェーズ係数
//! (`PolyphaseCoeffs`)を、**一括変換([`resample_oneshot`])とストリーミング
//! ([`StreamResampler`])の両方が同じ [`PolyphaseEngine`] で消費する**。
//!
//! # アルゴリズム(補間 L 倍 → デシメーション M 倍)
//!
//! 標準的な多重レート信号処理の構成(Crochiere & Rabiner の polyphase interpolator/decimator):
//! 素材レート `S` を `L` 倍に補間(ゼロ詰め)してから低域フィルタを掛け、`M` 倍に間引く
//! (`S*L == O*M` となる中間レートを介する)。`L = O/gcd(S,O)`, `M = S/gcd(S,O)`。
//! ゼロ詰めを実際には行わず、位相 `p = (n_out*M) mod L` ごとに束ねたポリフェーズ部分フィルタ
//! (タップ数 `K` = [`STREAM_TAPS_PER_PHASE`]/[`ONESHOT_TAPS_PER_PHASE`])を直接、
//! 入力サンプル `x[base], x[base-1], ..., x[base-(K-1)]`(`base = floor(n_out*M/L)`)へ
//! 畳み込む。rubato の非同期 sinc(補間テーブルの近似)と違い、**比が固定である前提を使って
//! 各位相の係数を厳密に事前計算する**ため、位相補間による誤差が原理的に無い。
//!
//! # 窓関数とタップ数(ADR-0004)
//!
//! Kaiser 窓を使う(目標ストップバンド減衰量からベータを一意に決められ、実効タップ数から
//! 遷移帯域幅を Kaiser の近似式で見積もれるため。理由・比較した他の窓は ADR 参照)。
//! - **ストリーミング**([`STREAM_TAPS_PER_PHASE`] = 64、[`STREAM_ATTENUATION_DB`] = 80dB):
//!   デコードスレッドで `pump()` のたびに何度も評価されるため、CPU コストと遅延を抑える。
//! - **一括変換**([`ONESHOT_TAPS_PER_PHASE`] = 256、[`ONESHOT_ATTENUATION_DB`] = 100dB):
//!   ロード時の一度きりのコストなので、計算量よりストップバンド減衰(エイリアシング抑制)を
//!   優先する。
//!
//! 🔴 **間引き(`M > L`。素材レートが出力レートより高いとき)は、上記のタップ数を
//! `ceil(M/L)` 倍する**(`PolyphaseCoeffs::design`)。遷移帯域幅(Hz)は素材レート `S` と
//! タップ数 `K` だけで決まり `L` に依存しないため、`K` を固定したまま `M` だけ大きくすると
//! 出力ナイキスト(`O/2`)に対する遷移帯域幅の割合が `M/L` に比例して悪化し、可聴域高域が
//! 大きく落ちる(実測: 192k→48k で 16kHz -4.9dB・20kHz -20.4dB)。`ceil(M/L)` 倍することで
//! 素材レートで見たフィルタの実効長を間引き量によらず一定に保つ。
//!
//! カットオフ周波数は「小さい方のレートのナイキスト周波数」を目標に、Kaiser の遷移帯域幅
//! 近似式(`(A-8) / (2.285 * 2π * N)`, `N` はポリフェーズ展開前の全体タップ数
//! `K*L`。`K` は上記の間引きスケーリング後の値)から逆算したガードバンドぶんだけ手前に
//! 置く(`PolyphaseCoeffs::design` 参照)。
//!
//! # レート比の安全域(`validate_rates`)
//!
//! [`MAX_RATE_RATIO`] に加え、`gcd` で約分した `L`/`M` の大きい方が [`MAX_POLYPHASE_FACTOR`]
//! を超える組み合わせも拒否する。比自体は小さくても(例 48000:48001 ≈ 1.0)、
//! 互いに素に近いレート同士だと `L`/`M` が数万に達し、`K*L` のタップ表が肥大化して
//! 構築コスト・メモリが破綻しうるため(rubato `Fft` の「`gcd` が小さいと FFT が巨大化する」
//! 弱点と同じ種類の懸念。旧実装のモジュール doc にあった注記を引き継ぐ)。
//!
//! # 遅延(レイテンシ)の扱い(設計判断2・3にまたがる)
//!
//! ポリフェーズ sinc フィルタは対称窓の群遅延ぶん(出力レート基準で `(K*L-1)/(2M)`
//! フレーム、`PolyphaseCoeffs::design` の `delay_output_frames`)、出力側に遅延を持つ
//! (🔴 分子は `K*L-1`。`(K-1)*L` ではない——ポリフェーズ展開前の中間レート換算での
//! 対称窓の中心タップ位置 `(K*L-1)/2` を出力レートへ換算した値であり、先に `-1` した
//! ものを `L` 倍するのとは異なる)。
//! **この遅延は rubato のときと同じく、呼び出し側(`decode.rs`)に一切見せない**:
//! 生成直後・`reset()` 直後の出力から `delay_output_frames` フレームぶんを内部で読み捨てて
//! から呼び出し側へ渡す(`PolyphaseEngine::convolve` 参照)。旧実装(rubato)からの
//! 置き換えで遅延の**値**は変わるが、この「呼び出し側には一切見せない」という契約自体は
//! 変わらない。`convert_frame_count`・FFI のいずれもこの遅延を織り込んでいる箇所は無い
//! (grep 済み。`mw_get_output_latency_ns` は出力デバイスのレイテンシで、リサンプラの
//! 群遅延とは無関係)。
//!
//! # ブロック境界の連続性
//!
//! [`StreamResampler`] はストリーム(1曲)につき1個だけ生成し、`pump()` が呼ばれるたびに
//! **同じインスタンスを使い回す**。直前チャンクの末尾 `K-1` サンプル(`PolyphaseEngine::history`)
//! を次チャンクの先頭に連結してから畳み込むことで、チャンク境界をまたぐ連続性を保証する
//! (rubato `Fft` の内部オーバーラップ状態に相当)。呼び出し側がブロックのたびに新しい
//! リサンプラを作ってしまうとこの保証が崩れるため、**絶対にやってはいけない**。
//!
//! シーク(`StreamResampler::reset`)は逆に**意図的な不連続**なので、履歴・位相状態を
//! 素の0へ戻す。シーク前後の音を混ぜてしまうバグを避けるための必須ステップ。
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

use crate::format::CHANNELS;

/// 楽曲ストリーミング用リサンプラの目標入力チャンク長(フレーム数)。
///
/// 実際のチャンク長は `M`(レート比を約分した分母)の倍数に丸められるため
/// この値そのものにはならないが、一般的なサンプルレートの組み合わせでは
/// 数十ミリ秒程度のブロックに収まる。
const STREAM_CHUNK_TARGET_FRAMES: u64 = 1024;

/// ストリーミング用ポリフェーズフィルタの1位相あたりのタップ数(`K`)。
///
/// デコードスレッドで `pump()` のたびに評価される(§4.7)ため、CPU コストと遅延
/// (正確な式はモジュール doc「遅延(レイテンシ)の扱い」・`PolyphaseCoeffs::design` 参照。
/// 概ね `K/2` 程度の入力サンプル)を抑える設定にする。64 タップは一般的な「中品質」sinc
/// リサンプラ(例: libsamplerate の Medium Quality 相当)と同程度で、[`STREAM_ATTENUATION_DB`]
/// との組み合わせでゲーム音声として十分な折り返し抑圧が得られる(ADR-0004 参照)。
///
/// 🔴 **Miri では 8 に落とす**(`decode.rs`/`wav.rs` 経由でリサンプルを通す既存テストの
/// ために)。係数表のサイズ・畳み込みの繰り返し回数は `K*L`(`L` はレート対から決まる
/// 補間係数)に比例し、64 タップだと `decode.rs` の単体テスト1本を Miri で解釈するだけで
/// 数分かかる(ADR-0003 が許容する「Miri では回数を減らす」対応)。
///
/// この定数を直接使うテスト(`resample.rs::tests`)への影響:
/// - CI が呼ぶ `make miri`(`MIRI_CI_FILTER` = `ring_buffer wav decode stream`)は
///   `resample::` 自体をフィルタで除外しているため、この定数が 8 に落ちても CI の
///   合否には影響しない。
/// - 🔴 一方 `make miri-all`(フィルタ無しで `--lib` を丸ごと実行)は `resample::tests`
///   も実行する。精度そのものを検査するテスト(`oneshot_attenuates_frequencies_above_the_output_nyquist_when_downsampling`
///   等)は K=8 では目標減衰量(dB)を満たせず本物の失敗として落ちるため、
///   `#[cfg_attr(miri, ignore)]` で個別に対象外にしてある(各テストの属性を参照)。
#[cfg(not(miri))]
const STREAM_TAPS_PER_PHASE: usize = 64;
#[cfg(miri)]
const STREAM_TAPS_PER_PHASE: usize = 8;
/// ストリーミング用フィルタの目標ストップバンド減衰量(dB)。Kaiser のベータを一意に決める。
const STREAM_ATTENUATION_DB: f64 = 80.0;

/// SE 一括リサンプル用チャンク長(フレーム数)。ロード時の一度きりの処理なので大きめに
/// 取って呼び出し回数を減らす。
const ONESHOT_CHUNK_FRAMES: usize = 4096;
/// SE 一括リサンプル用ポリフェーズフィルタの1位相あたりのタップ数(`K`)。
///
/// ロード時の一度きりのコストなので、計算量よりストップバンド減衰(エイリアシング抑制)を
/// 優先し、ストリーミングより大きい値にする(旧 rubato 実装の `sinc_len = 256` を踏襲)。
///
/// 🔴 Miri では [`STREAM_TAPS_PER_PHASE`] と同じ理由で 8 に落とす。
#[cfg(not(miri))]
const ONESHOT_TAPS_PER_PHASE: usize = 256;
#[cfg(miri)]
const ONESHOT_TAPS_PER_PHASE: usize = 8;
/// SE 一括リサンプル用フィルタの目標ストップバンド減衰量(dB)。
const ONESHOT_ATTENUATION_DB: f64 = 100.0;

/// カットオフ周波数の下限(小さい方のレートのナイキスト周波数に対する割合)。
///
/// `PolyphaseCoeffs::design` が Kaiser の遷移帯域幅からガードバンドを逆算する際の安全弁。
/// 実際の対応レート(8kHz〜384kHz程度の一般的な組み合わせ)ではガードバンドが
/// ナイキストの数%程度に収まり、この下限に触れることは無い。**間引き(`M > L`)時に
/// タップ数を `ceil(M/L)` 倍していること**(モジュール doc「窓関数とタップ数」参照)が
/// この性質を保つ前提——スケーリングしなければ `M` が大きいほどガードバンドが出力
/// ナイキストに対して肥大化する(実測: 192k→48k で放置すると 16kHz で -4.9dB)。
/// 触れるのはタップ数に対して `L` が極端に大きい病的な組み合わせのときだけで、
/// [`MAX_POLYPHASE_FACTOR`] が先に弾く。
const MIN_CUTOFF_FRACTION_OF_NYQUIST: f64 = 0.05;

/// 許容するサンプルレート比(大きい方 / 小さい方)の上限。
///
/// 比が極端(例: 1Hz と 384,000Hz)だとリサンプラ内部のバッファ確保が比に比例して
/// 膨れ上がり、OOM やタイムアウトを起こしうるため、`validate_rates` でリサンプラを
/// 構築する前に弾く。現実的な音声のサンプルレート(8kHz〜384kHz程度)の組み合わせは
/// この比に十分収まる。
const MAX_RATE_RATIO: u32 = 256;

/// `gcd(source_rate, output_rate)` で約分した `L`/`M` の大きい方の上限。
///
/// ポリフェーズ係数表のサイズは `K * L` に比例する(`K` はタップ数/位相)。比(大きい方/
/// 小さい方)が小さくても、2つのレートが互いに素に近い(`gcd` が小さい)と `L`/`M` は
/// 独立に大きくなりうる(例: 44100Hz と 44099Hz は比≈1.0 だが `gcd=1` で `L`/`M` が
/// 万単位になる)。実在するレート同士(8kHz〜384kHz の一般的な組み合わせ)では `L`/`M` は
/// 数千に収まる(実測: 11025Hz↔64000Hz で 2560 が最大)ため、8倍の余裕を見た値にしてある。
/// これを超える組み合わせは壊れたメタデータ等の異常値とみなし拒否する
/// (`ResampleError::InvalidRates`)。
const MAX_POLYPHASE_FACTOR: u32 = 8_192;

/// レートの組み合わせが安全かを検査する。`StreamResampler::new` と `resample_oneshot` の
/// 両方が、リサンプラを構築する・比を計算する前に必ずこれを通す。
///
/// - どちらかが 0 だと比が定義できない(0 除算、または無限大の比になる)。
/// - 比が `MAX_RATE_RATIO` を超えると、リサンプラ内部のバッファ確保が比に比例して
///   膨れ上がり、OOM やタイムアウトを起こしうる(`MAX_RATE_RATIO` のドキュメント参照)。
/// - 約分後の `L`/`M` が `MAX_POLYPHASE_FACTOR` を超えると、ポリフェーズ係数表が
///   肥大化する(`MAX_POLYPHASE_FACTOR` のドキュメント参照)。
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
    let g = gcd_u32(source_rate, output_rate);
    let l = output_rate / g;
    let m = source_rate / g;
    if l.max(m) > MAX_POLYPHASE_FACTOR {
        return Err(invalid());
    }
    Ok(())
}

/// ユークリッドの互除法(`source_rate`/`output_rate` は [`validate_rates`] が
/// 事前に 0 でないことを保証済み)。
fn gcd_u32(a: u32, b: u32) -> u32 {
    let (mut a, mut b) = (a, b);
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    a
}

/// リサンプル処理で発生しうるエラー。
#[derive(Debug, Clone, PartialEq)]
pub enum ResampleError {
    /// リサンプラの構築に失敗した(サンプルレートが 0 等。通常の入力では起こらない)。
    Construction(String),
    /// 変換処理そのものが失敗した(バッファサイズ不一致等。通常の入力では起こらない)。
    Processing(String),
    /// レートの組み合わせが不正(どちらかが 0、比〔大きい方 / 小さい方〕が
    /// `MAX_RATE_RATIO` を超える、または約分後の `L`/`M` が `MAX_POLYPHASE_FACTOR` を
    /// 超える)。`validate_rates` がリサンプラの構築より前に弾く。
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
                 both must be > 0, their ratio must not exceed {MAX_RATE_RATIO}x, and their \
                 reduced ratio must not exceed {MAX_POLYPHASE_FACTOR}x"
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
                        "{error:?} must print the underlying detail ({detail}) so the device log \
                         says what was actually wrong: {message}"
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

/// リサンプラへ渡す入力サンプルの絶対値上限。
///
/// 非有限(NaN / ±inf)や極端に大きい値をそのまま畳み込むと、フィルタの出力も
/// 非有限・極端になり呼び出し側(音声パス)へ伝播する。音声として意味のある値は
/// この範囲に収まるため、畳み込みの前にここで正規化する(fuzz が発見した回帰。
/// `51b5853` で rubato 向けに導入した入口ガードを自前実装でも同じ契約のまま維持する)。
const MAX_ABS_INPUT_SAMPLE: f32 = 1.0e4;

/// 畳み込みへ渡す前にサンプル1個を正規化する。
///
/// 非有限(NaN / ±inf)は無音(0.0)へ、有限でも [`MAX_ABS_INPUT_SAMPLE`] を超える値は
/// その範囲へ clamp する。`PolyphaseEngine::process_chunk`/`flush` の唯一の入力正規化ポイント。
fn sanitize_sample(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(-MAX_ABS_INPUT_SAMPLE, MAX_ABS_INPUT_SAMPLE)
    } else {
        0.0
    }
}

/// 変形ベッセル関数 `I0(x)` の級数展開(Kaiser 窓の計算に使う)。
///
/// `beta`(本モジュールが使う範囲では最大でも 15 程度、ADR-0004 参照)に対し、
/// 64 項もあれば f64 の精度で収束する(項の相対値が `1e-16` を下回った時点で打ち切る)。
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let half_x = x / 2.0;
    for k in 1..=64u32 {
        term *= (half_x / k as f64).powi(2);
        sum += term;
        if term < sum * 1e-16 {
            break;
        }
    }
    sum
}

/// 目標ストップバンド減衰量(dB)から Kaiser 窓のベータを求める近似式
/// (Kaiser 自身の式。Oppenheim & Schafer 等の標準的な教科書に載る経験式)。
fn kaiser_beta(attenuation_db: f64) -> f64 {
    if attenuation_db > 50.0 {
        0.1102 * (attenuation_db - 8.7)
    } else if attenuation_db >= 21.0 {
        0.5842 * (attenuation_db - 21.0).powf(0.4) + 0.07886 * (attenuation_db - 21.0)
    } else {
        0.0
    }
}

/// Kaiser 窓の `k` 番目(`0..n`)の値。
///
/// `bessel_i0_beta` は分母 `bessel_i0(beta)`(`k` に依存せずループ全体で一定)を
/// 呼び出し側が1回だけ計算して渡す。タップ数ぶん(数千〜数万回)呼ばれるため、
/// ループの内側で毎回計算し直すと無駄な `bessel_i0` 呼び出しが倍になる。
fn kaiser_window(k: usize, n: usize, beta: f64, bessel_i0_beta: f64) -> f64 {
    if n <= 1 {
        return 1.0;
    }
    let center = (n - 1) as f64 / 2.0;
    let ratio = (k as f64 - center) / center;
    let arg = (1.0 - ratio * ratio).max(0.0).sqrt();
    bessel_i0(beta * arg) / bessel_i0_beta
}

/// 固定有理数比 `L:M` のポリフェーズ窓付き sinc フィルタ係数。
///
/// レート対ごとに構築時へ1回だけ設計する(`PolyphaseCoeffs::design`)。以降の畳み込みは
/// この係数表を読むだけで、比の再計算やフィルタの再設計は一切発生しない。
struct PolyphaseCoeffs {
    /// 補間係数(`output_rate / gcd(source_rate, output_rate)`)。
    l: u32,
    /// 間引き係数(`source_rate / gcd(source_rate, output_rate)`)。
    m: u32,
    /// 1位相あたりのタップ数(`K`)。
    taps_per_phase: usize,
    /// `[phase * taps_per_phase + tap]` へフラット化した係数表。長さ `l * taps_per_phase`。
    taps: Vec<f32>,
    /// 出力レート基準の群遅延(丸め済み)。モジュール doc「遅延の扱い」参照。
    delay_output_frames: u64,
}

impl PolyphaseCoeffs {
    /// `source_rate`/`output_rate` は [`validate_rates`] を通過済みであること
    /// (0 除算・`L`/`M` の暴走を防ぐ前提)。
    fn design(
        source_rate: u32,
        output_rate: u32,
        base_taps_per_phase: usize,
        attenuation_db: f64,
    ) -> Self {
        let g = gcd_u32(source_rate, output_rate);
        let l = output_rate / g;
        let m = source_rate / g;

        // 🔴 レビュー指摘2の修正。間引き(`M > L`)のとき、1位相あたりのタップ数
        // (`base_taps_per_phase`)をそのまま使うと、全体のタップ数 `K*L` は `M` に
        // 追従せず、素材レートで見たフィルタ長(= `K*L / (S*L) = K/S` 秒)が
        // `M`(間引き量)によらず一定のままになる。間引きは出力レートのナイキストを
        // ずっと下げるので、遷移帯域幅(Kaiser 近似式)を出力ナイキストの一定割合に
        // 保つには、素材レートで見た長さを `M/L` に比例させる必要がある
        // (実測: 192k→48k は `M/L=4`、96k→48k は `M/L=2`)。`ceil(M/L)` 倍だけ
        // `taps_per_phase` を増やし、全体のタップ数を `K*L*ceil(M/L)` にすることで、
        // 出力ナイキストに対する遷移帯域幅の割合を `L` の値によらずほぼ一定に保つ
        // (導出: 遷移帯域幅(Hz)は `(A-8)*S / (2.285*2π*K)` で `L` に依存しないため、
        // 出力ナイキスト `O/2` に対する割合は `S/O`(= `M/L`)に比例して悪化する。
        // `K` を `M/L` 倍すれば打ち消せる)。
        // アップサンプル寄り(`M <= L`)のときは `n_total = K*L` が既に `S*L` に対して
        // 十分な密度を持つ(`L` 自体が大きいため)ので増やさない
        // (実測: 44.1k↔48k は ADR-0004 の比較で既に 50dB 超)。
        let decimation_factor = if m > l { m.div_ceil(l) } else { 1 };
        let taps_per_phase = base_taps_per_phase * decimation_factor as usize;
        let n_total = taps_per_phase * l as usize;

        let beta = kaiser_beta(attenuation_db);
        // 中間レート(補間後・間引き前。S*L == O*M)に対する正規化周波数(サイクル/サンプル)
        // で遷移帯域幅を Kaiser の近似式から見積もり、カットオフをナイキスト目標の手前へ
        // 置く(モジュール doc「窓関数とタップ数」参照)。
        let f_int = source_rate as f64 * l as f64;
        let delta_f_norm =
            (attenuation_db - 8.0) / (2.285 * 2.0 * std::f64::consts::PI * n_total as f64);
        let delta_f_hz = delta_f_norm * f_int;
        let nyquist_target_hz = source_rate.min(output_rate) as f64 / 2.0;
        let min_cutoff_hz = nyquist_target_hz * MIN_CUTOFF_FRACTION_OF_NYQUIST;
        let cutoff_hz = (nyquist_target_hz - delta_f_hz / 2.0).max(min_cutoff_hz);
        let fc_norm = cutoff_hz / f_int;

        let center = (n_total - 1) as f64 / 2.0;
        let bessel_i0_beta = bessel_i0(beta);
        let mut raw = vec![0.0f64; n_total];
        let mut sum = 0.0f64;
        for (k, slot) in raw.iter_mut().enumerate() {
            let x = k as f64 - center;
            let ideal = if x.abs() < 1e-9 {
                2.0 * fc_norm
            } else {
                (2.0 * std::f64::consts::PI * fc_norm * x).sin() / (std::f64::consts::PI * x)
            };
            let h = ideal * kaiser_window(k, n_total, beta, bessel_i0_beta);
            *slot = h;
            sum += h;
        }
        // DC ゲインを `L` に正規化する(補間のゼロ詰めで生じる 1/L の振幅損失を打ち消す。
        // `M=1`〔純粋な補間〕なら `L` そのもの、`L=1`〔純粋な間引き〕なら 1 になり、
        // どちらの極端でも標準的な多重レートフィルタの正規化と一致する)。
        let scale = l as f64 / sum;

        let mut taps = vec![0.0f32; l as usize * taps_per_phase];
        for (k, &h) in raw.iter().enumerate() {
            let phase = k % l as usize;
            let tap = k / l as usize;
            taps[phase * taps_per_phase + tap] = (h * scale) as f32;
        }

        // 🔴 レビュー指摘3の修正。群遅延は出力レート換算で `(K*L-1)/(2M)`。
        // 対称窓の中心タップ位置は、ポリフェーズ展開前の中間レート(`f_int`)換算で
        // `(n_total-1)/2 = (K*L-1)/2`(上の `center` と同じ定義)であり、これを
        // 出力レートへ変換(`/M` して `f_int` と出力レートの比ぶん調整すると `/(2M)` に
        // まとまる)したものが `delay_output_frames`。旧実装は `(K-1)*L/(2M)` になって
        // おり、`-1` を `L` 倍する前に引いてしまっていた(`L` が大きいレート対ほど
        // ずれが大きくなる)。四捨五入は `convert_frame_count` と同じ「分母の半分を
        // 足してから整数除算する」丸め方。
        let numerator = (taps_per_phase as u64 * l as u64).saturating_sub(1);
        let delay_output_frames = (numerator + m as u64) / (2 * m as u64);

        Self {
            l,
            m,
            taps_per_phase,
            taps,
            delay_output_frames,
        }
    }
}

/// [`PolyphaseCoeffs`] を消費する畳み込みエンジン。
///
/// [`StreamResampler`]・[`resample_oneshot`] のどちらも、この構造体の
/// `process_chunk`(定常呼び出し)・`flush`(終端呼び出し)だけを呼ぶ(一括変換と
/// ストリーミングを1実装で賄う、という要件の核)。相違点は「同じインスタンスを使い回すか
/// (ストリーミング)」「毎回作り直すか(一括、実質は同じインスタンスをループで使い回す)」の
/// 呼び出し方だけで、畳み込みロジック(`convolve`)は共有する。
///
/// 🔴 **定常呼び出し(`process_chunk`/`reset`)はヒープ確保・ロックを一切行わない**
/// (`history`/`scratch` は構築時に固定長で確保し、以後は書き換えるだけ)。
/// `mw-core/COMMON.md`「依存」に記載のとおり、このモジュールの呼び出し元
/// (ゲームスレッドのロード時・デコードスレッドの `pump()` 内)は音声コールバック経路
/// そのものではないため §5.3 のアロケーション禁止規約が直接は及ばないが、
/// デコードスレッドは楽曲バッファの供給を途切れさせない実時間性が要る経路であり、
/// 呼び出しのたびに確保が発生するとジッタの原因になる。既存(rubato)実装も
/// 固定長バッファを使い回す設計だったため、同じ特性を維持する。
///
/// 一方 **`flush`(ストリーム終端・一括変換の末尾で1回だけ呼ばれる)はヒープ確保を伴う**
/// (フィルタの尾を出し切るのに必要な長さぶんのローカル作業領域を都度確保する。
/// `flush` のドキュメント参照)。ストリームにつき1回・曲の終端でしか起こらないため
/// ジッタの懸念はなく、`process_chunk`(`pump()` のたびに何度も呼ばれる定常経路)を
/// アロケーション無しに保つこととは両立する。
struct PolyphaseEngine {
    /// 直前チャンクの末尾 `taps_per_phase - 1` サンプル(チャンネルごと)。
    history: Vec<Vec<f32>>,
    /// `history ++ 今回の入力(不足分は0埋め)` を保持する作業領域。長さは常に
    /// `(taps_per_phase - 1) + chunk_len` で固定。
    scratch: Vec<Vec<f32>>,
    /// 次に生成する出力サンプルの通し番号(`reset()` で 0 に戻す)。
    next_out_n: u64,
    /// これまでに(このチャンクを含めず)投入した入力サンプル数の通し番号。
    total_input_fed: u64,
    /// 起動直後・`reset()` 直後にまだ読み捨てていない遅延フレーム数。
    delay_to_skip: u64,
    /// 1回の `process_chunk` が扱う新規入力フレーム数(構築後は不変)。
    chunk_len: usize,
}

impl PolyphaseEngine {
    fn new(coeffs: &PolyphaseCoeffs, chunk_len: usize) -> Self {
        let k1 = coeffs.taps_per_phase - 1;
        Self {
            history: vec![vec![0.0f32; k1]; CHANNELS],
            scratch: vec![vec![0.0f32; k1 + chunk_len]; CHANNELS],
            next_out_n: 0,
            total_input_fed: 0,
            delay_to_skip: coeffs.delay_output_frames,
            chunk_len,
        }
    }

    /// シーク直後に呼ぶ。履歴・位相状態を素の0へ戻す(モジュール doc
    /// 「ブロック境界の連続性」: シークは意図的な不連続であり、直前までの履歴を
    /// 引き継ぐとシーク前後の音が混ざってしまうため、必ずリセットする)。
    fn reset(&mut self, coeffs: &PolyphaseCoeffs) {
        for channel in &mut self.history {
            channel.iter_mut().for_each(|v| *v = 0.0);
        }
        self.next_out_n = 0;
        self.total_input_fed = 0;
        self.delay_to_skip = coeffs.delay_output_frames;
    }

    /// `input` の先頭 `frames` フレームぶん(`frames <= self.chunk_len`)を実データとして
    /// 畳み込み、残り(`self.chunk_len - frames`)は無音として扱う。`frames == self.chunk_len`
    /// が通常の呼び出し、`frames < self.chunk_len` が一括変換の末尾ブロックに相当する
    /// (ストリーム終端は [`Self::flush`] を使う。理由はそちらのドキュメント参照)。
    ///
    /// 出力(出力レート基準のインターリーブ PCM)は `out` の末尾へ積む。
    fn process_chunk(
        &mut self,
        coeffs: &PolyphaseCoeffs,
        input: &[f32],
        frames: usize,
        out: &mut Vec<f32>,
    ) {
        let k1 = coeffs.taps_per_phase - 1;
        let chunk_len = self.chunk_len;

        for ch in 0..CHANNELS {
            let (hist_part, new_part) = self.scratch[ch].split_at_mut(k1);
            hist_part.copy_from_slice(&self.history[ch]);
            for (i, slot) in new_part.iter_mut().enumerate() {
                *slot = if i < frames {
                    sanitize_sample(input[i * CHANNELS + ch])
                } else {
                    0.0
                };
            }
        }

        // `self.scratch` と `self.history`/`self.next_out_n` 等は互いに素なフィールドなので、
        // 別々に借用すれば同時アクセスできる(`decode.rs::NativeReader` のドキュメントと
        // 同じ考え方)。
        Self::convolve(
            coeffs,
            &self.scratch,
            chunk_len,
            &mut self.next_out_n,
            &mut self.delay_to_skip,
            &mut self.total_input_fed,
            &mut self.history,
            out,
        );
    }

    /// ストリーム終端(または一括変換の末尾)を処理する。
    ///
    /// 🔴 **レビュー指摘1の修正。** [`Self::process_chunk`] は常に固定長
    /// `self.chunk_len` の「仮想入力」(実データ `frames` + 無音パディング
    /// `chunk_len - frames`)しか進めない。畳み込みは `base`(参照する入力位置)が
    /// `chunk_start + chunk_len - 1` を超えられないため、フィルタの尾
    /// (`taps_per_phase - 1` サンプルぶんの畳み込み支持域)を出し切るのに必要な無音
    /// (`taps_per_phase - 1` サンプル)が、その回のパディング(`chunk_len - frames`)
    /// だけでは足りないことがある(実測: 44.1k→48k でチャンク長 882・`K`=64 のとき、
    /// 残り実データが 881 フレームだとパディングは 1 サンプルしか無く、63 サンプルぶん
    /// 不足していた)。
    ///
    /// 直し方: `process_chunk` のような固定長チャンクの繰り返しではなく、
    /// **この呼び出し1回だけ**で「実データ(`frames`)+ 尾を出し切るのに必要な無音
    /// (`taps_per_phase - 1` サンプル)」ぴったりの長さの仮想入力を組み立てて畳み込む
    /// (`process_chunk` のように毎回 `chunk_len` ぶんの余分な無音チャンクを繰り返し
    /// 処理すると、本来必要な尾より遥かに多い「正しくはあるが無駄な」無音出力まで
    /// 生成してしまい、ストリーミングの総出力が `convert_frame_count` を大きく超えて
    /// しまう——一度この実装を試して回帰させたため、コメントとして残す)。
    /// これで `base` がちょうど実データ終端 + `taps_per_phase - 1` まで到達し、
    /// それ以降の出力は正確に 0 になる(それ以上パディングしても値は変わらない)ため、
    /// ここで安全に打ち切れる。呼び出し側(`resample_oneshot`)が行う
    /// `out.resize(target_frames.., 0.0)` は、この時点からは「本当に 0 であるべき」
    /// フレームだけを埋める形になり、まだ計算し切れていない本物の尾を無音で
    /// 上書きしてしまうことは無い。
    ///
    /// ヒープアロケーションを伴う(`local` の確保・`out.reserve`/`push`)が、
    /// `flush`/`flush_into` はデコードスレッド(`pump()`)・ロード時一括変換からしか
    /// 呼ばれず、音声コールバック経路(§5.3)には入らない(`resample.rs` モジュール doc
    /// 「ブロック境界の連続性」・`decode.rs` モジュール doc 参照)。
    fn flush(
        &mut self,
        coeffs: &PolyphaseCoeffs,
        remaining_input: &[f32],
        frames: usize,
        out: &mut Vec<f32>,
    ) {
        let k1 = coeffs.taps_per_phase - 1;
        // 実データ(frames)+ 尾を出し切るのに必要十分な無音(k1)ぴったりの仮想長。
        let virtual_len = frames + k1;

        let mut local = vec![vec![0.0f32; k1 + virtual_len]; CHANNELS];
        for ch in 0..CHANNELS {
            let (hist_part, rest) = local[ch].split_at_mut(k1);
            hist_part.copy_from_slice(&self.history[ch]);
            for (i, slot) in rest[..frames].iter_mut().enumerate() {
                *slot = sanitize_sample(remaining_input[i * CHANNELS + ch]);
            }
            // `rest[frames..]`(尾を出し切るための無音ぶん)は `vec![0.0; ..]` の初期値の
            // ままでよい。
        }

        Self::convolve(
            coeffs,
            &local,
            virtual_len,
            &mut self.next_out_n,
            &mut self.delay_to_skip,
            &mut self.total_input_fed,
            &mut self.history,
            out,
        );
    }

    /// `process_chunk`/`flush` に共通の畳み込み本体。
    ///
    /// `scratch`(`history ++ 今回の仮想入力`、長さ `k1 + virtual_len`)を読み、生成できる
    /// 出力(`base <= self.total_input_fed + virtual_len - 1` を満たす間)を `out` へ積む。
    /// 呼び出し側のフィールドを個別の引数として受け取る設計にしてあるのは、
    /// `scratch` が `self.scratch`(定常呼び出し)と `flush` 専用のローカル `Vec`(終端呼び出し)
    /// のどちらでもよいようにするため——`&mut self` 1本で受けると `self.scratch` を
    /// 読みながら `self.history` 等を書けず、借用が衝突してしまう
    /// (`process_chunk` のコメント参照)。
    #[allow(clippy::too_many_arguments)]
    fn convolve(
        coeffs: &PolyphaseCoeffs,
        scratch: &[Vec<f32>],
        virtual_len: usize,
        next_out_n: &mut u64,
        delay_to_skip: &mut u64,
        total_input_fed: &mut u64,
        history: &mut [Vec<f32>],
        out: &mut Vec<f32>,
    ) {
        let k = coeffs.taps_per_phase;
        let k1 = k - 1;
        let chunk_start = *total_input_fed;

        // 今回生成しうる出力フレーム数のおおまかな上限を見積もり、`out` の再確保回数を
        // 減らす(正確な値である必要は無い。ヒントに過ぎない)。
        out.reserve(
            (virtual_len as u64 * coeffs.l as u64 / coeffs.m as u64 + 2) as usize * CHANNELS,
        );

        loop {
            let base = (*next_out_n * coeffs.m as u64) / coeffs.l as u64;
            if base > chunk_start + virtual_len as u64 - 1 {
                break;
            }
            // `base >= chunk_start` は不変条件(前回の呼び出しが「これ以上は今回の仮想入力
            // では計算できない」という同じ条件で止まっているため)。この不変条件があるおかげで
            // `local_base - j`(`j` は 0..k)が常に `scratch` の範囲内に収まる。
            let phase = ((*next_out_n * coeffs.m as u64) % coeffs.l as u64) as usize;
            let local_base = (base - chunk_start) as usize + k1;
            let tap_base = phase * k;

            if *delay_to_skip > 0 {
                *delay_to_skip -= 1;
            } else {
                for s in scratch.iter() {
                    let mut acc = 0.0f32;
                    for j in 0..k {
                        acc += coeffs.taps[tap_base + j] * s[local_base - j];
                    }
                    out.push(acc);
                }
            }
            *next_out_n += 1;
        }

        for ch in 0..CHANNELS {
            let len = scratch[ch].len();
            history[ch].copy_from_slice(&scratch[ch][len - k1..]);
        }
        *total_input_fed += virtual_len as u64;
    }
}

/// 楽曲ストリーミング用のリサンプラ(固定比ポリフェーズ sinc ベース)。
///
/// `decode.rs::WavDecoder` が1曲につき1個だけ保持し、`pump()` の呼び出しを
/// またいで使い回す(モジュール doc「ブロック境界の連続性」)。
pub struct StreamResampler {
    coeffs: PolyphaseCoeffs,
    engine: PolyphaseEngine,
}

impl StreamResampler {
    /// `source_rate != output_rate` のときだけ呼ぶこと(一致する場合は
    /// `decode.rs` 側でバイパスし、このリサンプラ自体を作らない)。
    ///
    /// レートが 0、比(大きい方 / 小さい方)が `MAX_RATE_RATIO` を超える、または約分後の
    /// `L`/`M` が `MAX_POLYPHASE_FACTOR` を超える場合は `ResampleError::InvalidRates` を
    /// 返す([`validate_rates`] 参照)。
    pub fn new(source_rate: u32, output_rate: u32) -> Result<Self, ResampleError> {
        validate_rates(source_rate, output_rate)?;
        let coeffs = PolyphaseCoeffs::design(
            source_rate,
            output_rate,
            STREAM_TAPS_PER_PHASE,
            STREAM_ATTENUATION_DB,
        );
        // 入力チャンク長は `M`(約分した分母)の倍数にする —— こうすると比の性質上、
        // 定常状態では毎回ちょうど `chunk_len * L / M` フレームの出力が得られる
        // (`M` 個の新規入力を消費すると必ず `L` 個の出力が生成される、という
        // 有理数比ならではの厳密な関係。モジュール doc 参照)。倍率は
        // `STREAM_CHUNK_TARGET_FRAMES` に最も近くなるよう選ぶ(最低1倍)。
        let multiplier = (STREAM_CHUNK_TARGET_FRAMES / coeffs.m as u64).max(1);
        let chunk_len = (multiplier * coeffs.m as u64) as usize;
        let engine = PolyphaseEngine::new(&coeffs, chunk_len);
        Ok(Self { coeffs, engine })
    }

    /// 次の [`Self::process_full_chunk_into`] が要求する、素材レートの入力フレーム数。
    /// 構築時に決まる固定値で、ストリーム全体を通じて一定([`StreamResampler::new`] 参照)。
    pub fn input_frames_needed(&self) -> usize {
        self.engine.chunk_len
    }

    /// `input_frames_needed()` フレームぶんの素材レート PCM(インターリーブ)を変換し、
    /// 出力レート PCM を `out` の末尾へ積む。
    pub fn process_full_chunk_into(
        &mut self,
        input: &[f32],
        out: &mut Vec<f32>,
    ) -> Result<(), ResampleError> {
        let need = self.engine.chunk_len;
        debug_assert_eq!(input.len(), need * CHANNELS);
        self.engine.process_chunk(&self.coeffs, input, need, out);
        Ok(())
    }

    /// 素材側が末尾に到達した(`remaining_input` フレームぶんしか残っていない。
    /// 0 フレームでもよい)ときに一度だけ呼ぶ。残りを無音でパディングして最後の
    /// ブロックを変換し、フィルタの内部履歴に残っていた尾も一緒に吐き出す。
    ///
    /// 🔴 実データがチャンク長ぎりぎりのときは1回の呼び出しだけではパディングが足りず
    /// フィルタの尾が出し切れないことがあるため、内部で必要な回数ぶん無音チャンクを
    /// 追加で処理する(`PolyphaseEngine::flush` 参照。レビュー指摘1)。
    pub fn flush_into(
        &mut self,
        remaining_input: &[f32],
        out: &mut Vec<f32>,
    ) -> Result<(), ResampleError> {
        let valid_frames = remaining_input.len() / CHANNELS;
        self.engine
            .flush(&self.coeffs, remaining_input, valid_frames, out);
        Ok(())
    }

    /// シーク直後に呼ぶ(モジュール doc「ブロック境界の連続性」参照)。
    pub fn reset(&mut self) {
        self.engine.reset(&self.coeffs);
    }
}

/// SE ロード時の一括リサンプル(固定比ポリフェーズ sinc ベース)。
///
/// `source_rate != output_rate` のときだけ呼ぶこと(一致する場合は `wav.rs` 側で
/// バイパスする。設計判断4)。戻り値は `(出力レートのインターリーブ PCM, 出力フレーム数)`。
/// 出力フレーム数は常に `convert_frame_count(frames, source_rate, output_rate)` に一致する
/// (末尾のブロック丸めによる超過分は切り詰め、逆に不足することがあれば無音で埋める。
/// `SoundData::frames == interleaved.len() / CHANNELS` の不変条件を壊さないため)。
///
/// レートが 0、比(大きい方 / 小さい方)が `MAX_RATE_RATIO` を超える、または約分後の
/// `L`/`M` が `MAX_POLYPHASE_FACTOR` を超える場合は `ResampleError::InvalidRates` を返す
/// ([`validate_rates`] 参照。`source_rate == 0` を検査せず比を計算すると無限大になり、
/// 後段のバッファ確保が破綻するため必須)。
pub fn resample_oneshot(
    input: &[f32],
    frames: usize,
    source_rate: u32,
    output_rate: u32,
) -> Result<(Vec<f32>, usize), ResampleError> {
    validate_rates(source_rate, output_rate)?;
    let coeffs = PolyphaseCoeffs::design(
        source_rate,
        output_rate,
        ONESHOT_TAPS_PER_PHASE,
        ONESHOT_ATTENUATION_DB,
    );
    let chunk_len = ONESHOT_CHUNK_FRAMES;
    let mut engine = PolyphaseEngine::new(&coeffs, chunk_len);

    let mut out = Vec::with_capacity(
        convert_frame_count(frames as u64, source_rate, output_rate) as usize * CHANNELS,
    );

    let mut pos = 0usize;
    while frames - pos >= chunk_len {
        let start = pos * CHANNELS;
        let end = (pos + chunk_len) * CHANNELS;
        engine.process_chunk(&coeffs, &input[start..end], chunk_len, &mut out);
        pos += chunk_len;
    }

    // 最後の端数(0 フレームのこともある)を無音パディングしつつ処理する。
    // フィルタに残っていた尾もここで一緒に吐き出す。端数がチャンク長ぎりぎりだと
    // 1回のパディングだけでは足りないことがあるため、`PolyphaseEngine::flush` が
    // 必要な回数だけ無音チャンクを追加で処理する(`StreamResampler::flush_into` と
    // 同じ考え方。レビュー指摘1)。
    let remaining = frames - pos;
    let tail_start = pos * CHANNELS;
    let tail_end = frames * CHANNELS;
    engine.flush(&coeffs, &input[tail_start..tail_end], remaining, &mut out);

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
    fn stream_resampler_new_accepts_rate_ratio_at_the_limit() {
        // 25600 / 100 = 256 == MAX_RATE_RATIO。gcd=100 なので L=256, M=1
        // (MAX_POLYPHASE_FACTOR にも十分収まる)。境界は許容する。
        assert!(StreamResampler::new(25_600, 100).is_ok());
    }

    #[test]
    fn stream_resampler_new_rejects_near_coprime_rates_beyond_the_polyphase_factor_limit() {
        // 比はほぼ 1.0(MAX_RATE_RATIO には掛からない)だが、44100 と 44101 は互いに素
        // (gcd=1)なので L=44101, M=44100 となり MAX_POLYPHASE_FACTOR(8192)を超える。
        assert_stream_resampler_new_err(
            44_100,
            44_101,
            ResampleError::InvalidRates {
                source_rate: 44_100,
                output_rate: 44_101,
            },
        );
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

    // --- ここから先はフィルタそのものの特性(現在の仕様)の固定 --------------------

    /// 正弦波を生成する(インターリーブ・両ch同一)。
    fn sine_wave(freq_hz: f32, sample_rate: u32, frames: usize, amplitude: f32) -> Vec<f32> {
        let mut out = Vec::with_capacity(frames * CHANNELS);
        for i in 0..frames {
            let t = i as f32 / sample_rate as f32;
            let v = amplitude * (2.0 * std::f32::consts::PI * freq_hz * t).sin();
            for _ in 0..CHANNELS {
                out.push(v);
            }
        }
        out
    }

    /// ゼロ交差から周波数を推定する(前後10%ずつ除いてフィルタの端の影響を避ける)。
    fn estimate_frequency_hz(signal: &[f32], sample_rate: u32) -> f32 {
        let margin = signal.len() / 10;
        let core = &signal[margin..signal.len() - margin];
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

    /// 実効値(RMS)を求める(前後10%ずつ除いて端の影響を避ける)。
    fn rms(signal: &[f32]) -> f32 {
        let margin = signal.len() / 10;
        let core = &signal[margin..signal.len() - margin];
        if core.is_empty() {
            return 0.0;
        }
        (core.iter().map(|v| v * v).sum::<f32>() / core.len() as f32).sqrt()
    }

    /// `resample_oneshot` の左ch(インターリーブの偶数インデックス)だけ抜き出す。
    fn left_channel(interleaved: &[f32]) -> Vec<f32> {
        interleaved.iter().step_by(CHANNELS).copied().collect()
    }

    #[test]
    fn oneshot_preserves_frequency_and_amplitude_when_upsampling() {
        const SOURCE_RATE: u32 = 44_100;
        const OUTPUT_RATE: u32 = 48_000;
        const FREQ_HZ: f32 = 1_000.0;
        const AMPLITUDE: f32 = 0.5;
        const FRAME_COUNT: usize = 4_410; // 100ms

        let input = sine_wave(FREQ_HZ, SOURCE_RATE, FRAME_COUNT, AMPLITUDE);
        let (out, out_frames) =
            resample_oneshot(&input, FRAME_COUNT, SOURCE_RATE, OUTPUT_RATE).expect("valid rates");
        let left = left_channel(&out[..out_frames * CHANNELS]);

        let estimated_freq = estimate_frequency_hz(&left, OUTPUT_RATE);
        assert!(
            (estimated_freq - FREQ_HZ).abs() / FREQ_HZ < 0.02,
            "frequency should be preserved within 2%, got {estimated_freq} Hz"
        );

        let output_rms = rms(&left);
        let expected_rms = AMPLITUDE / std::f32::consts::SQRT_2;
        assert!(
            (output_rms - expected_rms).abs() / expected_rms < 0.05,
            "passband amplitude should be preserved within 5%, got rms={output_rms}, \
             expected~={expected_rms}"
        );
    }

    #[test]
    fn oneshot_preserves_frequency_and_amplitude_when_downsampling() {
        const SOURCE_RATE: u32 = 48_000;
        const OUTPUT_RATE: u32 = 44_100;
        const FREQ_HZ: f32 = 1_000.0;
        const AMPLITUDE: f32 = 0.5;
        const FRAME_COUNT: usize = 4_800; // 100ms

        let input = sine_wave(FREQ_HZ, SOURCE_RATE, FRAME_COUNT, AMPLITUDE);
        let (out, out_frames) =
            resample_oneshot(&input, FRAME_COUNT, SOURCE_RATE, OUTPUT_RATE).expect("valid rates");
        let left = left_channel(&out[..out_frames * CHANNELS]);

        let estimated_freq = estimate_frequency_hz(&left, OUTPUT_RATE);
        assert!(
            (estimated_freq - FREQ_HZ).abs() / FREQ_HZ < 0.02,
            "frequency should be preserved within 2%, got {estimated_freq} Hz"
        );

        let output_rms = rms(&left);
        let expected_rms = AMPLITUDE / std::f32::consts::SQRT_2;
        assert!(
            (output_rms - expected_rms).abs() / expected_rms < 0.05,
            "passband amplitude should be preserved within 5%, got rms={output_rms}, \
             expected~={expected_rms}"
        );
    }

    #[test]
    // 🔴 レビュー指摘4。`make miri-all`(フィルタ無しで `--lib` を丸ごと実行)では
    // `ONESHOT_TAPS_PER_PHASE`/`STREAM_TAPS_PER_PHASE` が 8 に落ちるため、この
    // ストップバンド減衰量(dB)そのものを検査するテストは目標減衰量を満たせず
    // 本物の失敗として落ちる(精度を見るテストであり、Miri のタップ数削減とは
    // 原理的に両立しない)。CI が呼ぶ `make miri` は `resample::` をフィルタで
    // 除外しているためこの ignore の影響は受けない。
    #[cfg_attr(
        miri,
        ignore = "Miri はタップ数を8に減らすため目標減衰量(dB)を満たせない。\
                   make miri(CI)は resample:: をそもそも対象外にしている"
    )]
    fn oneshot_attenuates_frequencies_above_the_output_nyquist_when_downsampling() {
        // 折り返し(エイリアシング)の抑圧: 出力のナイキストより高い成分は、
        // 折り返し先の周波数にエネルギーを残してはならない。48kHz -> 16kHz
        // (出力ナイキスト8kHz)へ 14kHz のトーンを通し、出力の実効値が
        // 入力よりはるかに小さいこと(通過帯域を通した場合との比較で「ほぼ無音」)を見る。
        const SOURCE_RATE: u32 = 48_000;
        const OUTPUT_RATE: u32 = 16_000;
        const AMPLITUDE: f32 = 0.5;
        const FRAME_COUNT: usize = 4_800; // 100ms

        let passband = sine_wave(1_000.0, SOURCE_RATE, FRAME_COUNT, AMPLITUDE);
        let (passband_out, passband_frames) =
            resample_oneshot(&passband, FRAME_COUNT, SOURCE_RATE, OUTPUT_RATE)
                .expect("valid rates");
        let passband_rms = rms(&left_channel(&passband_out[..passband_frames * CHANNELS]));

        let aliased = sine_wave(14_000.0, SOURCE_RATE, FRAME_COUNT, AMPLITUDE);
        let (aliased_out, aliased_frames) =
            resample_oneshot(&aliased, FRAME_COUNT, SOURCE_RATE, OUTPUT_RATE).expect("valid rates");
        let aliased_rms = rms(&left_channel(&aliased_out[..aliased_frames * CHANNELS]));

        // 通過帯域(1kHz)は振幅がほぼ保たれ、遮断帯域外(14kHz)は
        // フィルタの目標減衰量(80dB以上、ADR-0004)に見合うだけ小さくなっているはず。
        // 数値誤差・窓の裾を見込んで -40dB(1/100)を要求する。
        assert!(
            aliased_rms < passband_rms * 0.01,
            "aliased content must be suppressed by at least 40dB relative to passband: \
             passband_rms={passband_rms}, aliased_rms={aliased_rms}"
        );
    }

    #[test]
    fn stream_resampler_output_has_no_discontinuity_at_chunk_boundaries() {
        // ブロック境界の連続性: 複数チャンクにまたがる定常正弦波を処理したとき、
        // チャンク境界(`PolyphaseEngine::history` の引き継ぎが起きる位置)で
        // 不連続(プチノイズ)が出ないことを見る。定常正弦波の隣接サンプル差分は
        // どこでもほぼ一定のはずなので、境界だけ突出していれば継ぎ目が壊れている。
        const SOURCE_RATE: u32 = 44_100;
        const OUTPUT_RATE: u32 = 48_000;
        const TOTAL_CHUNKS: usize = 6;

        let mut resampler = StreamResampler::new(SOURCE_RATE, OUTPUT_RATE).expect("valid rates");
        let need = resampler.input_frames_needed();
        let total_frames = need * TOTAL_CHUNKS;
        let input = sine_wave(1_000.0, SOURCE_RATE, total_frames, 0.5);

        let mut out = Vec::new();
        for i in 0..TOTAL_CHUNKS {
            let chunk = &input[i * need * CHANNELS..(i + 1) * need * CHANNELS];
            resampler
                .process_full_chunk_into(chunk, &mut out)
                .expect("must not error");
        }
        let left = left_channel(&out);

        let diffs: Vec<f32> = left.windows(2).map(|w| (w[1] - w[0]).abs()).collect();
        // 先頭・末尾(フィルタのウォームアップ・まだ届いていない未来入力の影響が残る領域)を
        // 除いた定常領域だけを見る。
        let margin = diffs.len() / 10;
        let steady = &diffs[margin..diffs.len() - margin];
        let max_diff = steady.iter().copied().fold(0.0f32, f32::max);
        let mean_diff = steady.iter().sum::<f32>() / steady.len() as f32;
        assert!(
            max_diff < mean_diff * 5.0,
            "output should not show a boundary discontinuity: max_diff={max_diff}, \
             mean_diff={mean_diff}"
        );
    }

    #[test]
    fn stream_resampler_reset_discards_history_like_a_fresh_instance() {
        // シーク相当の不連続性: ウォームアップしてから `reset()` した場合と、
        // 新規構築した場合とで、以降の出力が完全一致することを見る
        // (`PolyphaseEngine::history`/位相状態が正しく初期化されることの回帰)。
        const SOURCE_RATE: u32 = 44_100;
        const OUTPUT_RATE: u32 = 48_000;

        let warm_input = sine_wave(1_000.0, SOURCE_RATE, 8_820, 0.5);
        let probe_input = sine_wave(500.0, SOURCE_RATE, 4_410, 0.3);

        let mut warmed = StreamResampler::new(SOURCE_RATE, OUTPUT_RATE).expect("valid rates");
        let need = warmed.input_frames_needed();
        let mut scratch = Vec::new();
        let mut pos = 0usize;
        while pos + need <= warm_input.len() / CHANNELS {
            warmed
                .process_full_chunk_into(
                    &warm_input[pos * CHANNELS..(pos + need) * CHANNELS],
                    &mut scratch,
                )
                .expect("must not error");
            pos += need;
        }
        warmed.reset();

        let mut warmed_out = Vec::new();
        let mut fresh = StreamResampler::new(SOURCE_RATE, OUTPUT_RATE).expect("valid rates");
        let mut fresh_out = Vec::new();
        let mut pos = 0usize;
        while pos + need <= probe_input.len() / CHANNELS {
            let chunk = &probe_input[pos * CHANNELS..(pos + need) * CHANNELS];
            warmed
                .process_full_chunk_into(chunk, &mut warmed_out)
                .expect("must not error");
            fresh
                .process_full_chunk_into(chunk, &mut fresh_out)
                .expect("must not error");
            pos += need;
        }

        assert!(!warmed_out.is_empty());
        assert_eq!(
            warmed_out, fresh_out,
            "reset() must fully discard prior state regardless of playback history"
        );
    }

    #[test]
    fn oneshot_output_length_matches_convert_frame_count_for_common_rate_pairs() {
        for &(source_rate, output_rate) in &[
            (44_100u32, 48_000u32),
            (48_000, 44_100),
            (32_000, 48_000),
            (22_050, 48_000),
            (16_000, 48_000),
        ] {
            let frame_count = 2_000usize;
            let input = sine_wave(500.0, source_rate, frame_count, 0.4);
            let (out, out_frames) = resample_oneshot(&input, frame_count, source_rate, output_rate)
                .expect("valid rates");
            let expected = convert_frame_count(frame_count as u64, source_rate, output_rate);
            assert_eq!(
                out_frames, expected as usize,
                "{source_rate}->{output_rate}: output frame count must match convert_frame_count"
            );
            assert_eq!(out.len(), out_frames * CHANNELS);
        }
    }

    #[test]
    fn delay_is_a_deterministic_function_of_the_rate_pair() {
        // 遅延(内部でのみ読み捨てる群遅延)が、レート対に対して決定的であることを固定する。
        // インパルス応答のピーク位置は「起動直後に読み捨てたフレーム数(群遅延)」の
        // 直後に来るはずなので、ピークが常に出力の先頭に来ること
        // (= 群遅延の読み捨てが機能していること)を、複数のレート対で確認する。
        for &(source_rate, output_rate) in
            &[(44_100u32, 48_000u32), (48_000, 44_100), (32_000, 48_000)]
        {
            let frame_count = 2_000usize;
            let mut impulse = vec![0.0f32; frame_count * CHANNELS];
            impulse[0] = 1.0;
            impulse[1] = 1.0;
            let (out, out_frames) =
                resample_oneshot(&impulse, frame_count, source_rate, output_rate)
                    .expect("valid rates");
            let left = left_channel(&out[..out_frames * CHANNELS]);
            let (peak_index, _) = left
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.abs().partial_cmp(&b.1.abs()).unwrap())
                .expect("non-empty output");
            // 群遅延はここでは呼び出し側に一切見えない(モジュール doc)ので、
            // インパルス応答のピークは出力の先頭付近(数フレーム以内)に来るはずである。
            assert!(
                peak_index < 8,
                "{source_rate}->{output_rate}: impulse response peak should appear near the \
                 start of the output once the internal group delay has been skipped, got index \
                 {peak_index}"
            );
        }
    }

    #[test]
    fn identity_rate_pair_is_never_constructed_by_callers() {
        // `StreamResampler`/`resample_oneshot` はレート一致時に呼ばれない設計
        // (`decode.rs`/`wav.rs` がバイパスする)。ここでは `validate_rates` が
        // レート一致自体を拒否しないこと(呼び出し側の責務であり、ここでは弾かない)
        // だけを回帰として残す。
        assert!(StreamResampler::new(48_000, 48_000).is_ok());
    }

    // --- レビュー指摘の回帰テスト -------------------------------------------------

    /// 【高】レビュー指摘1: `flush_into` が無音埋めした `process_chunk` を1回しか
    /// 呼ばないため、残りの実データがチャンク長に近いとフィルタの尾
    /// (`taps_per_phase - 1` 入力サンプルぶんの畳み込み支持域)が出しきれない。
    ///
    /// 実測(修正前): 44.1k→48k で入力長 T=4409 のとき、ストリーミング出力の総フレーム数が
    /// `convert_frame_count` に届かなかった(T=4410 ではチャンクにちょうど収まるため
    /// たまたま通っていた——既存テストが割り切れる長さしか試していなかった見逃しの原因)。
    #[test]
    fn streaming_flush_delivers_the_full_tail_near_chunk_boundary_lengths() {
        for &(source_rate, output_rate) in &[
            (44_100u32, 48_000u32),
            (48_000, 44_100),
            (22_050, 48_000),
            (96_000, 48_000),
        ] {
            let need = StreamResampler::new(source_rate, output_rate)
                .expect("valid rates")
                .input_frames_needed();

            for &frame_count in &[need - 1, need, need + 1, 2 * need - 1, 2 * need] {
                let input = sine_wave(500.0, source_rate, frame_count, 0.4);
                let mut resampler =
                    StreamResampler::new(source_rate, output_rate).expect("valid rates");
                let mut out = Vec::new();

                let mut pos = 0usize;
                while frame_count - pos >= need {
                    resampler
                        .process_full_chunk_into(
                            &input[pos * CHANNELS..(pos + need) * CHANNELS],
                            &mut out,
                        )
                        .expect("must not error");
                    pos += need;
                }
                resampler
                    .flush_into(&input[pos * CHANNELS..frame_count * CHANNELS], &mut out)
                    .expect("flush must not error");

                // `resample.rs` モジュール doc「総フレーム数・シーク位置は出力レート基準」の
                // とおり、ブロック単位でしか出力できないリサンプラは端数ぶん
                // `convert_frame_count` を超えて生成することがあり、それは呼び出し側
                // (`WavDecoder` 等)が `total_frames` で打ち切る前提の正常な余剰
                // (decode.rs::decoder_delivers_exactly_total_frames_near_chunk_boundary_lengths
                // が実際の打ち切り込みで厳密な一致を検査する)。ここで検査すべきなのは
                // 「尾が出しきれず不足する」バグ(レビュー指摘1)なので、不足していないこと
                // (>=)を見る。
                let expected =
                    convert_frame_count(frame_count as u64, source_rate, output_rate) as usize;
                assert!(
                    out.len() / CHANNELS >= expected,
                    "{source_rate}->{output_rate}: streaming output must reach at least \
                     convert_frame_count for input length {frame_count} (chunk_len={need}); \
                     got {} frames, need >= {expected}",
                    out.len() / CHANNELS
                );
            }
        }
    }

    /// 【高】レビュー指摘1(一括変換側)。`resample_oneshot` は同じ `PolyphaseEngine` を
    /// 使うため、末尾の端数チャンクがちょうど `ONESHOT_CHUNK_FRAMES` の手前だと
    /// 同じ理由でフィルタの尾が出しきれず、`out.resize(target_frames.., 0.0)` が
    /// 本来の(DC信号ならほぼ入力振幅の)値の代わりに強制的な無音で埋めてしまう。
    ///
    /// 十分な無音(1チャンクぶん以上)を先に足した参照と比べる: フィルタは因果的
    /// (未来を見ない)ため、参照側の対応する範囲の出力は元の呼び出しと値が変わらない
    /// はずで、末尾が不正に 0 へ落ちていればここで食い違う。
    #[test]
    fn oneshot_does_not_replace_the_true_tail_with_hard_zero_for_a_dc_signal() {
        const SOURCE_RATE: u32 = 44_100;
        const OUTPUT_RATE: u32 = 48_000;
        const AMPLITUDE: f32 = 0.6;
        // `ONESHOT_CHUNK_FRAMES` のすぐ手前(端数チャンクがほぼ満杯になる境界条件)。
        let frame_count = ONESHOT_CHUNK_FRAMES - 1;

        let dc = vec![AMPLITUDE; frame_count * CHANNELS];
        let (out, out_frames) =
            resample_oneshot(&dc, frame_count, SOURCE_RATE, OUTPUT_RATE).expect("valid rates");

        let mut padded_dc = dc.clone();
        padded_dc.resize(padded_dc.len() + ONESHOT_CHUNK_FRAMES * CHANNELS, 0.0);
        let (padded_out, _) = resample_oneshot(
            &padded_dc,
            frame_count + ONESHOT_CHUNK_FRAMES,
            SOURCE_RATE,
            OUTPUT_RATE,
        )
        .expect("valid rates");

        assert!(
            out_frames * CHANNELS <= padded_out.len(),
            "padded reference must be at least as long as the original output"
        );
        let tail_len = 10.min(out_frames);
        let tail = &out[(out_frames - tail_len) * CHANNELS..out_frames * CHANNELS];
        let reference_tail = &padded_out[(out_frames - tail_len) * CHANNELS..out_frames * CHANNELS];
        for (i, (&got, &reference)) in tail.iter().zip(reference_tail.iter()).enumerate() {
            assert!(
                (got - reference).abs() < 1e-4,
                "tail sample {i} must match the fully-flushed reference (causal filter: \
                 appending more silence must not change already-computed samples), \
                 got {got}, reference {reference}"
            );
            assert!(
                got.abs() > AMPLITUDE * 0.5,
                "tail sample {i} must not have been replaced by hard zero padding, got {got} \
                 (DC input should stay close to amplitude {AMPLITUDE} in this region)"
            );
        }
    }

    /// 【中】レビュー指摘2: 大きく間引く(`M > L`)ときに全体のタップ数(`K*L`)が
    /// 素材レートで見て伸びず、可聴域の高域が大きく落ちる。
    ///
    /// 実測(修正前、ストリーミング `K=64`): 192k→48k で 16kHz -4.9dB・20kHz -20.4dB、
    /// 96k→48k で 20kHz -4.9dB(一括変換は `K=256` が元々余裕を持っていたため症状が出ない。
    /// 音楽ストリーミングで実際に問題になるのはこちら)。
    #[test]
    // 🔴 レビュー指摘4と同じ理由(Miri ではタップ数が8まで減るため、間引き時のタップ数
    // スケーリングを掛けても -1dB 基準を満たせない)で対象外にする。
    #[cfg_attr(
        miri,
        ignore = "Miri はタップ数を8に減らすため-1dB基準を満たせない。\
                   make miri(CI)は resample:: をそもそも対象外にしている"
    )]
    fn streaming_keeps_high_audible_frequencies_within_1db_when_heavily_downsampling() {
        const AMPLITUDE: f32 = 0.5;
        let expected_rms = AMPLITUDE / std::f32::consts::SQRT_2;

        for &(source_rate, output_rate, freq_hz) in &[
            (192_000u32, 48_000u32, 16_000.0f32),
            (96_000, 48_000, 20_000.0),
        ] {
            let frame_count = source_rate as usize / 10; // 100ms
            let input = sine_wave(freq_hz, source_rate, frame_count, AMPLITUDE);

            let mut resampler =
                StreamResampler::new(source_rate, output_rate).expect("valid rates");
            let need = resampler.input_frames_needed();
            let mut out = Vec::new();
            let mut pos = 0usize;
            while frame_count - pos >= need {
                resampler
                    .process_full_chunk_into(
                        &input[pos * CHANNELS..(pos + need) * CHANNELS],
                        &mut out,
                    )
                    .expect("must not error");
                pos += need;
            }
            resampler
                .flush_into(&input[pos * CHANNELS..frame_count * CHANNELS], &mut out)
                .expect("flush must not error");

            let measured_rms = rms(&left_channel(&out));
            let attenuation_db = 20.0 * (measured_rms / expected_rms).log10();
            assert!(
                attenuation_db > -1.0,
                "{source_rate}->{output_rate}: {freq_hz}Hz should be attenuated by less than \
                 1dB, got {attenuation_db}dB (measured_rms={measured_rms}, \
                 expected_rms={expected_rms})"
            );
        }
    }

    /// 【低】レビュー指摘3: 群遅延の式が `(K-1)*L/(2M)` になっていた
    /// (正しくは出力換算 `(K*L-1)/(2M)`)。入力の途中(`i0`)にインパルスを置き、
    /// 出力側の理想位置 `i0*O/S` から ±0.5フレーム以内にピークが来ることを見る
    /// (先頭付近に来ることしか見ていなかった既存の
    /// `delay_is_a_deterministic_function_of_the_rate_pair` より厳密な検査)。
    #[test]
    fn oneshot_impulse_peak_lands_within_half_a_frame_of_the_ideal_output_position() {
        for &(source_rate, output_rate) in
            &[(8_000u32, 48_000u32), (16_000, 48_000), (44_100, 48_000)]
        {
            let frame_count = 4_000usize;
            let i0 = frame_count / 2;
            let mut impulse = vec![0.0f32; frame_count * CHANNELS];
            impulse[i0 * CHANNELS] = 1.0;
            impulse[i0 * CHANNELS + 1] = 1.0;

            let (out, out_frames) =
                resample_oneshot(&impulse, frame_count, source_rate, output_rate)
                    .expect("valid rates");
            let left = left_channel(&out[..out_frames * CHANNELS]);
            let (peak_index, _) = left
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.abs().partial_cmp(&b.1.abs()).unwrap())
                .expect("non-empty output");

            let ideal = i0 as f64 * output_rate as f64 / source_rate as f64;
            let diff = (peak_index as f64 - ideal).abs();
            assert!(
                diff <= 0.5,
                "{source_rate}->{output_rate}: peak should land within 0.5 frame of the ideal \
                 position {ideal}, got index {peak_index} (diff {diff})"
            );
        }
    }
}
