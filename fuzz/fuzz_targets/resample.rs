//! `mw_core::resample` のファズターゲット(ADR-0003 の2段目)。
//!
//! `StreamResampler`(楽曲ストリーミング用、自前ポリフェーズ sinc)を1周(全チャンク処理 →
//! flush → reset)ぶん回してからもう1周し、`resample_oneshot`(SE 一括変換用、同じ
//! ポリフェーズ sinc エンジンを高品質な係数で使う)も同じ入力で呼ぶ。狙うのは自前実装
//! (`resample.rs`)の境界(バッファサイズの不整合・NaN/inf の伝播・パニック・
//! `MAX_POLYPHASE_FACTOR` 近辺のレート対での確保)。
//!
//! レート 0・極端なレート比・NaN/inf/極端な値を含む入力は、いずれも `resample.rs` 自身が
//! 入口(`validate_rates`/`sanitize_sample`)で弾く・正規化するため、ここでは避けずにそのまま
//! ライブラリへ渡す(`Err` になること・出力が有限であることはライブラリ側のテストで固定化済み)。
#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use mw_core::CHANNELS;
use mw_core::resample::{StreamResampler, resample_oneshot};

/// 入力サンプル数(インターリーブ・ステレオ)の上限。大きな入力そのものを禁じたいわけではなく、
/// 実行時間を切ってファザーの1本あたりの回転数を確保するための上限。
const MAX_SAMPLES: usize = 8_192;
/// サンプルレートの clamp 上限。ライブラリ自身がレート 0・極端な比を `Err` で弾くため、
/// ここでの上限はもっぱら実行時間を切るためのもの(`u32::MAX` 付近の値を毎回試す意味は薄い)。
const MAX_RATE: u32 = 384_000;

#[derive(Debug, Arbitrary)]
struct Input {
    source_rate_raw: u32,
    output_rate_raw: u32,
    samples: Vec<f32>,
}

/// `1..=MAX_RATE` へ clamp する。ときどき 0 を素通しさせ、`StreamResampler::new`/
/// `resample_oneshot` の両方が `Err(InvalidRates)` を返すこと(0 除算・無限大比を起こさない
/// こと)を確認する。
fn pick_rate(raw: u32) -> u32 {
    if raw % 101 == 0 {
        0
    } else {
        raw.clamp(1, MAX_RATE)
    }
}

fuzz_target!(|input: Input| {
    let source_rate = pick_rate(input.source_rate_raw);
    let output_rate = pick_rate(input.output_rate_raw);

    let mut samples = input.samples;
    samples.truncate(MAX_SAMPLES);
    samples.truncate(samples.len() - samples.len() % CHANNELS);

    if let Ok(mut resampler) = StreamResampler::new(source_rate, output_rate) {
        let need_frames = resampler.input_frames_needed();
        let need_samples = need_frames * CHANNELS;

        let mut out = Vec::new();
        let mut pos = 0usize;
        if need_samples > 0 {
            while pos + need_samples <= samples.len() {
                let _ =
                    resampler.process_full_chunk_into(&samples[pos..pos + need_samples], &mut out);
                pos += need_samples;
            }
        }
        let _ = resampler.flush_into(&samples[pos..], &mut out);
        resampler.reset();

        // reset 後もう1周。
        let mut out2 = Vec::new();
        let mut pos2 = 0usize;
        if need_samples > 0 {
            while pos2 + need_samples <= samples.len() {
                let _ = resampler
                    .process_full_chunk_into(&samples[pos2..pos2 + need_samples], &mut out2);
                pos2 += need_samples;
            }
        }
        let _ = resampler.flush_into(&samples[pos2..], &mut out2);
    }

    let frames = samples.len() / CHANNELS;
    let _ = resample_oneshot(&samples, frames, source_rate, output_rate);
});
