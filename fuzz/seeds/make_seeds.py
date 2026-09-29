#!/usr/bin/env python3
"""cargo-fuzz のシードコーパスを生成する(`make fuzz-seeds` から呼ぶ)。

python3 標準ライブラリだけで完結させる(外部依存を増やさない)。`wave` モジュールは
PCM 8/16/24/32bit の RIFF/WAVE しか書けない(フォーマットタグは常に PCM 固定)ため、
float32(IEEE float フォーマットタグ)は `struct` で RIFF を手書きする。

`wav_decode` は生の WAV バイト列をそのまま fuzz_target の引数に渡す設計
(`fuzz_targets/wav_decode.rs`)なので、そのままのバイト列をコーパスに置けばよい。

`wav_decoder_pump`/`resample` は `#[derive(Arbitrary)]` の構造体を引数に取るため、
コーパスは構造体のフィールドを `arbitrary` クレートの実際の消費順序どおりに並べた
バイト列でなければならない(でたらめな配置だとフィールドの区切りがズレて無意味な値に
なる)。`arbitrary 1.4.2`(このリポジトリで実際に解決されたバージョン)の実装を
`~/.cargo/registry/src/*/arbitrary-1.4.2/` で直接確認して固定した規則:

- 構造体の**最後のフィールド以外**は `Arbitrary::arbitrary` で消費される。
  固定長の整数型(`u8`/`u32`)は `Unstructured::fill_buffer` でリトルエンディアンの
  生バイト列をそのまま前から読む(範囲外や不足があっても 0 埋めで必ず成功する)。
- 構造体の**最後のフィールド**(`Vec<u8>`/`Vec<f32>`)は `Arbitrary::arbitrary_take_rest`
  で消費される。`Vec<T>` はどちらの経路でも同じ「要素ごとに継続フラグ(`bool`、
  実体は `u8` を読んで LSB を見る)→ 要素本体」という組を繰り返すイテレータを使う
  (`arbitrary_take_rest` でも `take_rest` 特有の高速パスは無い)。入力データを
  使い切ると `fill_buffer` が 0 埋めを返すため継続フラグが偽になり、そこで止まる
  ——余分な終端バイトを足す必要はない。
"""

import math
import struct
import sys
from pathlib import Path

SEEDS_DIR = Path(__file__).resolve().parent
FUZZ_DIR = SEEDS_DIR.parent
CORPUS_DIR = FUZZ_DIR / "corpus"

WAVE_FORMAT_PCM = 1
WAVE_FORMAT_IEEE_FLOAT = 3


# --- WAV バイト列の組み立て(crates/mw-core/src/wav.rs の parse_header と対応) -------


def build_wav(
    sample_rate: int,
    channels: int,
    bits_per_sample: int,
    format_tag: int,
    data_bytes: bytes,
) -> bytes:
    bytes_per_sample = bits_per_sample // 8
    block_align = bytes_per_sample * channels
    byte_rate = sample_rate * block_align
    fmt_size = 16

    out = bytearray()
    out += b"RIFF"
    riff_size = 4 + (8 + fmt_size) + (8 + len(data_bytes))
    out += struct.pack("<I", riff_size)
    out += b"WAVE"

    out += b"fmt "
    out += struct.pack("<I", fmt_size)
    out += struct.pack("<H", format_tag)
    out += struct.pack("<H", channels)
    out += struct.pack("<I", sample_rate)
    out += struct.pack("<I", byte_rate)
    out += struct.pack("<H", block_align)
    out += struct.pack("<H", bits_per_sample)

    out += b"data"
    out += struct.pack("<I", len(data_bytes))
    out += data_bytes
    return bytes(out)


def sine_i16(frames: int, channels: int, freq_hz: float, sample_rate: int, amplitude: int) -> bytes:
    out = bytearray()
    for i in range(frames):
        v = int(amplitude * math.sin(2.0 * math.pi * freq_hz * i / sample_rate))
        for _ in range(channels):
            out += struct.pack("<h", v)
    return bytes(out)


def sine_pcm24(frames: int, channels: int, freq_hz: float, sample_rate: int, amplitude: int) -> bytes:
    out = bytearray()
    for i in range(frames):
        v = int(amplitude * math.sin(2.0 * math.pi * freq_hz * i / sample_rate))
        raw = v & 0xFFFFFF  # 24bit 2の補数をそのままリトルエンディアン3バイトへ。
        for _ in range(channels):
            out += raw.to_bytes(3, byteorder="little", signed=False)
    return bytes(out)


def sine_pcm32(frames: int, channels: int, freq_hz: float, sample_rate: int, amplitude: int) -> bytes:
    out = bytearray()
    for i in range(frames):
        v = int(amplitude * math.sin(2.0 * math.pi * freq_hz * i / sample_rate))
        for _ in range(channels):
            out += struct.pack("<i", v)
    return bytes(out)


def sine_f32(frames: int, channels: int, freq_hz: float, sample_rate: int, amplitude: float) -> bytes:
    out = bytearray()
    for i in range(frames):
        v = amplitude * math.sin(2.0 * math.pi * freq_hz * i / sample_rate)
        for _ in range(channels):
            out += struct.pack("<f", v)
    return bytes(out)


def make_pcm16_mono_44100() -> bytes:
    frames = int(0.05 * 44_100)
    data = sine_i16(frames, 1, 440.0, 44_100, 20_000)
    return build_wav(44_100, 1, 16, WAVE_FORMAT_PCM, data)


def make_pcm16_stereo_48000() -> bytes:
    frames = int(0.05 * 48_000)
    data = sine_i16(frames, 2, 440.0, 48_000, 20_000)
    return build_wav(48_000, 2, 16, WAVE_FORMAT_PCM, data)


def make_pcm24_stereo_48000() -> bytes:
    frames = int(0.05 * 48_000)
    data = sine_pcm24(frames, 2, 440.0, 48_000, 4_000_000)
    return build_wav(48_000, 2, 24, WAVE_FORMAT_PCM, data)


def make_pcm32_stereo_48000() -> bytes:
    frames = int(0.05 * 48_000)
    data = sine_pcm32(frames, 2, 440.0, 48_000, 1_000_000_000)
    return build_wav(48_000, 2, 32, WAVE_FORMAT_PCM, data)


def make_float32_stereo_48000() -> bytes:
    frames = int(0.05 * 48_000)
    data = sine_f32(frames, 2, 440.0, 48_000, 0.6)
    return build_wav(48_000, 2, 32, WAVE_FORMAT_IEEE_FLOAT, data)


# --- arbitrary クレートのバイト消費順序に合わせたエンコード ---------------------------


def encode_vec_bytes(data: bytes) -> bytes:
    """`Vec<u8>::arbitrary`/`arbitrary_take_rest` の消費順序(継続フラグ+要素)を再現する。"""
    out = bytearray()
    for b in data:
        out.append(1)  # 継続フラグ(奇数 = true)
        out.append(b)
    return bytes(out)


def encode_vec_f32(values) -> bytes:
    out = bytearray()
    for v in values:
        out.append(1)  # 継続フラグ(奇数 = true)
        out += struct.pack("<f", v)
    return bytes(out)


def encode_pump_input(rate_selector: int, seek_frame: int, wav_bytes: bytes) -> bytes:
    """`wav_decoder_pump.rs::Input { rate_selector: u8, seek_frame: u32, bytes: Vec<u8> }`。"""
    out = bytearray()
    out.append(rate_selector & 0xFF)
    out += struct.pack("<I", seek_frame & 0xFFFFFFFF)
    out += encode_vec_bytes(wav_bytes)
    return bytes(out)


def encode_resample_input(source_rate_raw: int, output_rate_raw: int, samples) -> bytes:
    """`resample.rs::Input { source_rate_raw: u32, output_rate_raw: u32, samples: Vec<f32> }`。"""
    out = bytearray()
    out += struct.pack("<I", source_rate_raw & 0xFFFFFFFF)
    out += struct.pack("<I", output_rate_raw & 0xFFFFFFFF)
    out += encode_vec_f32(samples)
    return bytes(out)


def write(path: Path, data: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)
    print(f"wrote {path.relative_to(FUZZ_DIR)} ({len(data)} bytes)")


def main() -> None:
    wavs = {
        "pcm16_mono_44100": make_pcm16_mono_44100(),
        "pcm16_stereo_48000": make_pcm16_stereo_48000(),
        "pcm24_stereo_48000": make_pcm24_stereo_48000(),
        "pcm32_stereo_48000": make_pcm32_stereo_48000(),
        "float32_stereo_48000": make_float32_stereo_48000(),
    }

    wav_decode_dir = CORPUS_DIR / "wav_decode"
    for name, wav_bytes in wavs.items():
        write(wav_decode_dir / f"{name}.wav", wav_bytes)

    pump_dir = CORPUS_DIR / "wav_decoder_pump"
    for i, (name, wav_bytes) in enumerate(wavs.items()):
        rate_selector = i % 3
        seek_frame = 10 * i
        write(
            pump_dir / f"{name}.bin",
            encode_pump_input(rate_selector, seek_frame, wav_bytes),
        )

    resample_dir = CORPUS_DIR / "resample"
    write(
        resample_dir / "44100_to_48000.bin",
        encode_resample_input(44_100, 48_000, [0.0, 0.25, -0.25, 0.5, -0.5, 0.9, -0.9, 0.1]),
    )
    write(
        resample_dir / "same_rate_bypass.bin",
        encode_resample_input(48_000, 48_000, [0.1, -0.1, 0.2, -0.2]),
    )
    # 🔴 source_rate=0 はここに置かない: `resample_oneshot` はゼロレートを検査せず
    # 比が +inf になって出力バッファの確保が破綻する(`fuzz_targets/resample.rs` の
    # `oneshot_is_safe` のコメント参照。ライブラリ側のバグとして別途報告済み)。
    # コーパスは「既知で安全な入力の回帰」なので、この組み合わせは載せない
    # (ハーネス自身がこの組み合わせを弾くため、これ単体は再現用の材料にもならない)。
    # output_rate=0 は安全(比が 0 になるだけ)なので、そちらで代わりに検査する。
    write(
        resample_dir / "nan_inf_and_zero_output_rate.bin",
        encode_resample_input(
            44_100,
            0,
            [float("nan"), float("inf"), float("-inf"), 0.0, -0.0],
        ),
    )


if __name__ == "__main__":
    sys.exit(main())
