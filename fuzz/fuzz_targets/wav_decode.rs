//! `mw_core::wav::decode` のファズターゲット(ADR-0003 の2段目)。
//!
//! 入力バイト列をそのまま渡す。狙うのはパニック・abort・巨大確保だけで、
//! `Err` は「壊れた入力を正しく拒否できている」ことを意味するので正常系として扱う。
//! 出力レートはレート一致(バイパス経路)・不一致(rubato の一括リサンプル経路)の
//! 両方を踏むよう、同じバイト列で48kHzと44.1kHzの両方を呼ぶ。
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = mw_core::wav::decode(data, 48_000);
    let _ = mw_core::wav::decode(data, 44_100);
});
