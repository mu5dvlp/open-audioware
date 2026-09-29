//! `mw_core::decode::WavDecoder` と、それを介した `stream.rs` のリングバッファ供給
//! (`MusicStreamProducer::pump`)のファズターゲット(ADR-0003 の2段目)。
//!
//! 2経路を踏む:
//! - `pump` 経路: `WavDecoder::open` が成功したら、`stream::channel` で作ったリングバッファへ
//!   上限回数だけ `pump` する(EOF・エラー・リングバッファ満杯のいずれかで抜ける。
//!   無限ループにしない)。
//! - `MusicDecoder` トレイト経路: 入力から得た適当なフレームへ `seek` してから `read` を
//!   数回呼び、`total_frames` も呼ぶ。
//!
//! 出力レートは入力から 48_000 / 44_100 / 22_050 のいずれかを選ぶ(素材レートと
//! 異なる場合は rubato の FFT リサンプラも経路に乗る)。
#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use mw_core::{CHANNELS, Config, MusicDecoder, WavDecoder};

/// `pump` を回す上限回数。EOF・エラー・満杯のいずれにも該当しない異常系(理論上到達しない)
/// でも無限ループにしないための保険。
const MAX_PUMP_CALLS: usize = 32;
/// `MusicDecoder::read` を呼ぶ回数(seek 後の読み出し確認用)。
const MAX_READ_CALLS: usize = 4;
/// 1回の `read` で要求するフレーム数。
const READ_CHUNK_FRAMES: usize = 256;

#[derive(Debug, Arbitrary)]
struct Input {
    rate_selector: u8,
    seek_frame: u32,
    bytes: Vec<u8>,
}

fn pick_output_rate(selector: u8) -> u32 {
    match selector % 3 {
        0 => 48_000,
        1 => 44_100,
        _ => 22_050,
    }
}

fuzz_target!(|input: Input| {
    let output_sample_rate = pick_output_rate(input.rate_selector);

    // pump 経路。
    if let Ok(mut decoder) = WavDecoder::open(input.bytes.clone(), output_sample_rate) {
        let (mut producer, _consumer) =
            mw_core::stream::channel(Config::default(), output_sample_rate);
        for _ in 0..MAX_PUMP_CALLS {
            match producer.pump(&mut decoder) {
                Ok(outcome) => {
                    if outcome.reached_eof || outcome.pushed_frames == 0 {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    }

    // MusicDecoder トレイト経路(seek → read の組)。
    if let Ok(mut decoder) = WavDecoder::open(input.bytes, output_sample_rate) {
        let total_frames = decoder.total_frames();
        let seek_target = match total_frames {
            Some(0) | None => u64::from(input.seek_frame),
            Some(total) => u64::from(input.seek_frame) % total,
        };
        let _ = decoder.seek(seek_target);

        let mut buf = [0.0f32; READ_CHUNK_FRAMES * CHANNELS];
        for _ in 0..MAX_READ_CALLS {
            match decoder.read(&mut buf) {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => break,
            }
        }
        let _ = decoder.total_frames();
    }
});
