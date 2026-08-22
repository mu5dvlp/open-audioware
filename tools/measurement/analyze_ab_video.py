#!/usr/bin/env python3
"""M1 A/B 計測動画の自動解析(docs/measurement-m1.md §3)。

スロー動画(リアルタイム pts のオリジナルファイル)から
  - 白フラッシュ(タップ時刻)を映像輝度 YAVG の立ち上がりで検出
  - クリック SE(出力時刻)を 2kHz バンドパス後の RMS 立ち上がりで検出
し、フラッシュごとの遅延 [ms] と統計(中央値・最小・最大・標準偏差)を出力する。

使い方:
  python3 tools/measurement/analyze_ab_video.py <video> [--label A]
  1モード1動画で撮影する(A の動画、B の動画を別々に渡す)。

注意: iPhone のスローモーション動画は「オリジナル」のまま AirDrop すること。
Photos アプリでスロー再生編集を適用して書き出すと時間軸が歪み、解析できない。

依存: ffmpeg / ffprobe(PATH 上にあること)。Python は標準ライブラリのみ使用。

自己検証(合成動画で既知の遅延を復元できるかのテスト):
  python3 tools/measurement/analyze_ab_video.py --self-test
"""

import argparse
import math
import re
import statistics
import subprocess
import sys
import tempfile
import wave
from pathlib import Path

AUDIO_RATE = 48000
RMS_WINDOW_SEC = 0.001  # 1ms 窓
SEARCH_AFTER_FLASH_SEC = 0.4  # フラッシュ後、SE を探す最大時間
# フラッシュ「前」も探す。アプリはタップと同一フレームで白フラッシュと発音を要求するが、
# 画面に実際に出るまでにはレンダリング+垂直同期+パネルの応答があり、遅延の小さい実装では
# 音のほうが先に出る(第2回計測でミドルウェア側が中央値 -31.5ms になった)。
# 前を探さないと「SE を検出できなかった」= 無音、という致命的な偽陰性になる。
SEARCH_BEFORE_FLASH_SEC = 0.15
MIN_FLASH_GAP_SEC = 0.3  # フラッシュ同士の最小間隔(チャタリング除去)
MIN_FLASH_JUMP = 40.0  # フラッシュと見なす baseline→peak の最小輝度差(--min-flash-jump で変更可)
FLASH_THRESHOLD_RATIO = 0.5  # baseline と peak の間のどこを「明るい」の境目にするか


def run(cmd):
    proc = subprocess.run(cmd, capture_output=True, text=True)
    if proc.returncode != 0:
        raise RuntimeError(f"command failed: {' '.join(cmd)}\n{proc.stderr[-2000:]}")
    return proc


def frame_luma_series(video):
    """各フレームの (pts_time, YAVG) を返す。"""
    proc = run([
        "ffmpeg", "-hide_banner", "-i", str(video),
        "-vf", "signalstats,metadata=print:key=lavfi.signalstats.YAVG:file=-",
        "-f", "null", "-",
    ])
    series = []
    pts = None
    for line in proc.stdout.splitlines():
        m = re.search(r"pts_time:([0-9.]+)", line)
        if m:
            pts = float(m.group(1))
            continue
        m = re.search(r"lavfi\.signalstats\.YAVG=([0-9.]+)", line)
        if m and pts is not None:
            series.append((pts, float(m.group(1))))
            pts = None
    if not series:
        raise RuntimeError("映像フレームの輝度を取得できなかった(動画ファイルを確認)")
    return series


def detect_flashes(series, min_jump=MIN_FLASH_JUMP, ratio=FLASH_THRESHOLD_RATIO):
    """輝度の立ち上がりエッジ = フラッシュ開始時刻のリストを返す。

    `min_jump` は「フラッシュらしい輝度ジャンプ」と見なす baseline→peak の最小差。
    画面がフレーム内で小さい・周囲が明るいと差が縮むため、撮影条件によっては
    既定値では弾かれる。その場合は `--min-flash-jump` で下げる(下げすぎると
    手ブレや被写体の動きをフラッシュと誤検出するので、検出回数がタップ回数と
    一致するか必ず確認すること)。"""
    lumas = [y for _, y in series]
    baseline = statistics.median(lumas)
    peak = max(lumas)
    if peak - baseline < min_jump:
        raise RuntimeError(
            f"白フラッシュらしい輝度ジャンプがない(baseline={baseline:.1f}, peak={peak:.1f}, "
            f"必要な差={min_jump})。画面がフレーム内に映っているか確認するか、"
            "--min-flash-jump で閾値を下げる")
    threshold = baseline + (peak - baseline) * ratio
    flashes = []
    prev_bright = True  # 冒頭から明るい場合はエッジ扱いしない
    for t, y in series:
        bright = y >= threshold
        if bright and not prev_bright:
            if not flashes or t - flashes[-1] >= MIN_FLASH_GAP_SEC:
                flashes.append(t)
        prev_bright = bright
    return flashes


def extract_bandpassed_audio(video, tmpdir):
    """2kHz バンドパス済みモノラル wav を書き出してサンプル列を返す。"""
    out = Path(tmpdir) / "bp.wav"
    run([
        "ffmpeg", "-hide_banner", "-y", "-i", str(video),
        "-af", "highpass=f=1500,lowpass=f=2500",
        "-ac", "1", "-ar", str(AUDIO_RATE), "-sample_fmt", "s16", str(out),
    ])
    with wave.open(str(out), "rb") as w:
        raw = w.readframes(w.getnframes())
    samples = []
    for i in range(0, len(raw) - 1, 2):
        v = int.from_bytes(raw[i:i + 2], "little", signed=True)
        samples.append(v / 32768.0)
    if not samples:
        raise RuntimeError("音声トラックを取得できなかった")
    return samples


def rms_envelope(samples):
    win = int(AUDIO_RATE * RMS_WINDOW_SEC)
    env = []
    for i in range(0, len(samples) - win, win):
        acc = 0.0
        for s in samples[i:i + win]:
            acc += s * s
        env.append(math.sqrt(acc / win))
    return env, RMS_WINDOW_SEC


def detect_onset_in(env, step, t_from, t_to, noise_floor, peak):
    """[t_from, t_to] 内で最初に有意に立ち上がる時刻を返す(なければ None)。"""
    threshold = max(noise_floor * 6.0, peak * 0.15)
    i0 = max(0, int(t_from / step))
    i1 = min(len(env), int(t_to / step))
    for i in range(i0, i1):
        if env[i] >= threshold:
            return i * step
    return None


def analyze(video, label, min_jump=MIN_FLASH_JUMP, flash_ratio=FLASH_THRESHOLD_RATIO,
            search_before=SEARCH_BEFORE_FLASH_SEC):
    series = frame_luma_series(video)
    flashes = detect_flashes(series, min_jump, flash_ratio)
    with tempfile.TemporaryDirectory() as tmpdir:
        samples = extract_bandpassed_audio(video, tmpdir)
    env, step = rms_envelope(samples)
    noise_floor = sorted(env)[int(len(env) * 0.5)]
    peak = max(env)

    latencies = []
    misses = []
    for t in flashes:
        onset = detect_onset_in(env, step,
                                max(0.0, t - search_before), t + SEARCH_AFTER_FLASH_SEC,
                                noise_floor, peak)
        if onset is None:
            misses.append(t)
        else:
            latencies.append((t, (onset - t) * 1000.0))

    print(f"== {label}: {video}")
    print(f"フラッシュ検出: {len(flashes)} 回 / SE 対応づけ成功: {len(latencies)} 回")
    for t, ms in latencies:
        print(f"  flash @ {t:8.3f}s -> latency {ms:7.1f} ms")
    if misses:
        print(f"  ⚠ SE を検出できなかったフラッシュ: {['%.3f' % t for t in misses]}")
    if latencies:
        vals = [ms for _, ms in latencies]
        med = statistics.median(vals)
        sd = statistics.pstdev(vals) if len(vals) > 1 else 0.0
        print(f"  中央値 {med:.1f} ms / 最小 {min(vals):.1f} / 最大 {max(vals):.1f}"
              f" / 標準偏差 {sd:.1f} (n={len(vals)})")
        if med < 0:
            print("  ※ 負値 = 音がフラッシュより先に出ている。白フラッシュは画面に出るまでに"
                  "レンダリング+垂直同期+パネル応答ぶん遅れるため、この方式の基準点としては"
                  "この実装の遅延より遅い。A/B の『差』は有効だが、絶対値は求まらない"
                  "(必要なら §2.2 の方式B: ライン録音へ切り替える)。")
        return med
    return None


def self_test():
    """既知の遅延(50ms / 70ms)を合成動画から復元できるか検証する。"""
    with tempfile.TemporaryDirectory() as tmpdir:
        video = Path(tmpdir) / "synthetic.mov"
        flash_enable = "between(t,1.0,1.05)+between(t,2.0,2.05)"
        tone_enable = "between(t,1.05,1.10)+between(t,2.07,2.12)"
        # 音声フィルタの enable はオーディオフレーム単位で量子化されるため、
        # samples_per_frame を小さくして基準音の開始時刻を ~1.3ms 精度にする。
        # AAC はエンコーダ遅延で立ち上がりがずれるので PCM を使う
        sine = (f"sine=frequency=2000:duration=3:sample_rate={AUDIO_RATE}"
                ":samples_per_frame=64")
        run([
            "ffmpeg", "-hide_banner", "-y",
            "-f", "lavfi", "-i", "color=c=gray:s=320x240:r=60:d=3",
            "-f", "lavfi", "-i", "color=c=white:s=320x240:r=60:d=3",
            "-f", "lavfi", "-i", sine,
            "-filter_complex",
            f"[0:v][1:v]overlay=enable='{flash_enable}'[v];"
            f"[2:a]volume=volume=0:enable='not({tone_enable})'[a]",
            "-map", "[v]", "-map", "[a]",
            "-c:v", "libx264", "-preset", "ultrafast", "-c:a", "pcm_s16le",
            str(video),
        ])
        med = analyze(video, "self-test")
        assert med is not None, "self-test: SE を検出できなかった"
        # 期待値: 50ms と 70ms の中央値 60ms(フレーム・窓量子化ぶんを許容)
        assert abs(med - 60.0) <= 10.0, f"self-test: 中央値 {med:.1f}ms が期待範囲外"
        print("self-test OK")


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("video", nargs="?", help="計測動画(1モード1ファイル)")
    ap.add_argument("--label", default="?", help="表示ラベル(A / B など)")
    ap.add_argument("--self-test", action="store_true", help="合成動画で自己検証")
    ap.add_argument("--min-flash-jump", type=float, default=MIN_FLASH_JUMP,
                    help=f"フラッシュと見なす最小輝度差(既定 {MIN_FLASH_JUMP})")
    ap.add_argument("--flash-ratio", type=float, default=FLASH_THRESHOLD_RATIO,
                    help=f"明暗の境目の位置(既定 {FLASH_THRESHOLD_RATIO})")
    ap.add_argument("--search-before", type=float, default=SEARCH_BEFORE_FLASH_SEC,
                    help=f"フラッシュより前を探す秒数(既定 {SEARCH_BEFORE_FLASH_SEC})")
    args = ap.parse_args()
    if args.self_test:
        self_test()
        return
    if not args.video:
        ap.error("video を指定するか --self-test を使う")
    analyze(Path(args.video), args.label, args.min_flash_jump, args.flash_ratio,
            args.search_before)


if __name__ == "__main__":
    main()
