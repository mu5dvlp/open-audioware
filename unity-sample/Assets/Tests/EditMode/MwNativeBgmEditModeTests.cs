using System;
using System.Diagnostics;
using System.Threading;
using Mw.Native;
using NUnit.Framework;

namespace Mw.Native.Tests
{
    /// <summary>
    /// M4-3(BGM のネイティブ化)の C# ラッパ経路を検証する EditMode テスト。
    ///
    /// <para>
    /// <see cref="MwNativeMusicEditModeTests"/> と同じ立ち位置: Rust 側のロジック
    /// (フェード・ループの数値的な正しさ)は mw-core のオフラインレンダリングテストで
    /// 固定化済みなので、ここでは<b>「C# 側の薄いラッパが、生成バインディング越しに
    /// 正しく呼べているか」</b>だけに絞ってある——BGM が楽曲(M2)とは独立した
    /// もう1本のボイス・デコードスレッドとして実際に動くこと、クロック相当の API
    /// (曲位置)を意図的に持たないこと、ID 空間(<see cref="SoundMode.Music"/>)を
    /// 楽曲と共有していることの3点。
    /// </para>
    /// </summary>
    public class MwNativeBgmEditModeTests
    {
        /// <summary>
        /// プリロール完了・再生開始・曲末到達を待つときの上限。デコードスレッドの
        /// ポーリング周期(10ms)とオーディオコールバック周期に対して十分長く取る。
        /// </summary>
        private static readonly TimeSpan WaitTimeout = TimeSpan.FromSeconds(10);

        /// <summary>テスト素材の長さ。曲末(自然終了)まで待つので短くする。</summary>
        private const double BgmSeconds = 0.35;

        [Test]
        public void BgmLifecycle_ThroughWrapper_ReachesReadyThenPlaysThenReturnsToReadyOnNaturalEnd()
        {
            Assert.AreEqual(MwResult.Ok, MwNative.Init(out ulong handle), "mw_init should succeed on a machine with a default audio output device");

            try
            {
                byte[] bgmBytes = TestWavBuilder.BuildStereoToneWav(BgmSeconds);

                Assert.AreEqual(
                    MwResult.Ok,
                    MwNative.LoadSound(handle, bgmBytes, SoundMode.Music, out ulong bgmId),
                    "BGM tracks share the Music-mode load path with the song voice");
                Assert.AreNotEqual(0ul, bgmId);

                Assert.AreEqual(MwResult.Ok, MwNative.SetBgm(handle, bgmId), "opening the decoder must succeed for a valid wav");

                // SetBgm は非ブロッキングで、プリロール完了は待たない契約
                // (SetMusic と同じ。MwNativeMusicEditModeTests 参照)。
                MusicState observedState = MusicState.Loading;
                bool becameReady = WaitUntil(() =>
                {
                    Assert.AreEqual(MwResult.Ok, MwNative.GetBgmState(handle, out observedState));
                    return observedState == MusicState.Ready;
                });
                Assert.IsTrue(becameReady, $"BGM must become Ready once preroll completes; last observed state={observedState}");

                Assert.AreEqual(MwResult.Ok, MwNative.PlayBgm(handle));

                bool startedPlaying = WaitUntil(() =>
                {
                    Assert.AreEqual(MwResult.Ok, MwNative.GetBgmState(handle, out observedState));
                    return observedState == MusicState.Playing;
                });
                Assert.IsTrue(startedPlaying, $"PlayBgm must start playback; last observed state={observedState}");

                // BGM は自然終了(ループ未設定)で自動的に Ready へ戻る
                // (mw_core::MusicVoice::render の自然終了経路。MusicEnded 相当の
                // イベントは無いため、状態のポーリングだけで確認する)。
                bool returnedToReady = WaitUntil(() =>
                {
                    Assert.AreEqual(MwResult.Ok, MwNative.GetBgmState(handle, out observedState));
                    return observedState == MusicState.Ready;
                });
                Assert.IsTrue(returnedToReady, $"a short BGM clip without looping must reach its natural end and return to Ready; last observed state={observedState}");

                Assert.AreEqual(MwResult.Ok, MwNative.ReleaseSound(handle, bgmId));
            }
            finally
            {
                Assert.AreEqual(MwResult.Ok, MwNative.Shutdown(handle));
            }
        }

        /// <summary>
        /// ハンドルを1本だけ開いて済ませられる軽い検証をまとめたもの
        /// (<see cref="MwNativeMusicEditModeTests.MusicApi_RejectsBadInputAndReportsLatency"/> と
        /// 同じ理由でここに集約する)。
        /// </summary>
        [Test]
        public void BgmApi_RejectsBadInputAndSharesIdSpaceWithMusic()
        {
            Assert.AreEqual(MwResult.Ok, MwNative.Init(out ulong handle));

            try
            {
                // ループ区間: begin >= end は拒否、0/0 は「解除」として通る
                // (SetMusicLoop と同じ規約)。
                Assert.AreEqual(MwResult.ErrInvalidLoopRegion, MwNative.SetBgmLoop(handle, 5, 3));
                Assert.AreEqual(MwResult.ErrInvalidLoopRegion, MwNative.SetBgmLoop(handle, 7, 7));
                Assert.AreEqual(MwResult.Ok, MwNative.SetBgmLoop(handle, 0, 200));
                Assert.AreEqual(MwResult.Ok, MwNative.ClearBgmLoop(handle), "(0, 0) means \"clear the loop\", not an invalid region");

                // ID 空間の共有: 楽曲用にロードした ID がそのまま BGM API へ渡せる
                // (どちらも Music モードの圧縮バイト列ストレージを共有する)。
                byte[] musicBytes = TestWavBuilder.BuildStereoToneWav(0.05);
                Assert.AreEqual(MwResult.Ok, MwNative.LoadSound(handle, musicBytes, SoundMode.Music, out ulong musicId));
                Assert.AreEqual(MwResult.Ok, MwNative.SetBgm(handle, musicId), "a Music-mode id must be usable as a BGM id");
                Assert.AreEqual(MwResult.Ok, MwNative.ReleaseSound(handle, musicId));

                // ID 空間の分離: SE としてロードした ID は BGM API に渡せない。
                byte[] seBytes = TestWavBuilder.BuildPcm16Wav(sampleRate: 48_000, channels: 2, samples: new short[] { 1000, -1000 });
                Assert.AreEqual(MwResult.Ok, MwNative.LoadSound(handle, seBytes, SoundMode.Se, out ulong seId));
                Assert.AreEqual(MwResult.ErrInvalidSoundId, MwNative.SetBgm(handle, seId), "an SE id must not be usable as a BGM id");
                Assert.AreEqual(MwResult.Ok, MwNative.ReleaseSound(handle, seId));

                // 未ロードのハンドルへの Play/Stop はコマンドとして素通りするだけで
                // 落ちない(音声スレッド側で無視される。SetBgm 未呼び出しなら Loading のまま)。
                Assert.AreEqual(MwResult.Ok, MwNative.PlayBgm(handle));
                Assert.AreEqual(MwResult.Ok, MwNative.StopBgm(handle));
            }
            finally
            {
                Assert.AreEqual(MwResult.Ok, MwNative.Shutdown(handle));
            }
        }

        /// <summary>
        /// <paramref name="condition"/> が true を返すまで短い間隔でポーリングする
        /// (タイムアウト付き)。EditMode テストにはフレーム進行が無いため、
        /// コルーチンではなくスリープで待つ(<see cref="MwNativeMusicEditModeTests"/> と同じ)。
        /// </summary>
        private static bool WaitUntil(Func<bool> condition)
        {
            var stopwatch = Stopwatch.StartNew();
            while (stopwatch.Elapsed < WaitTimeout)
            {
                if (condition())
                {
                    return true;
                }

                Thread.Sleep(5);
            }

            return condition();
        }
    }
}
