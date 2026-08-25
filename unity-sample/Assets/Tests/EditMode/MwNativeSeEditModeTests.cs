using System;
using Mw.Native;
using NUnit.Framework;

namespace Mw.Native.Tests
{
    /// <summary>
    /// M1(SE 再生)の終了条件を検証する EditMode テスト。
    /// 初期構築仕様 §10「Editor と実機で SE が鳴る」の Editor 側の入口部分:
    /// コードで生成した wav バイト列を load → play → stop → release した際、
    /// すべての呼び出しが <see cref="MwResult.Ok"/> を返すことを確認する。
    /// wav バイナリはコミットしない(§8: ゴールデン波形はテスト時にコードで生成する方針)。
    /// </summary>
    public class MwNativeSeEditModeTests
    {
        [Test]
        public void LoadPlayStopRelease_AllReturnOk()
        {
            MwResult initResult = MwNative.Init(out ulong handle);
            Assert.AreEqual(MwResult.Ok, initResult, "mw_init should succeed on a machine with a default audio output device");

            try
            {
                byte[] wavBytes = TestWavBuilder.BuildPcm16Wav(sampleRate: 48_000, channels: 2, samples: new short[]
                {
                    0, 0,
                    8000, -8000,
                    16000, -16000,
                    24000, -24000,
                });

                MwResult loadResult = MwNative.LoadSound(handle, wavBytes, SoundMode.Se, out ulong soundId);
                Assert.AreEqual(MwResult.Ok, loadResult, "loading a valid 48kHz/16bit/stereo wav must succeed");
                Assert.AreNotEqual(0ul, soundId, "a successful load must yield a non-zero opaque sound id");

                MwResult playResult = MwNative.PlaySe(handle, soundId, Bus.Se, volume: 0.8f, out ulong voiceId);
                Assert.AreEqual(MwResult.Ok, playResult, "playing a just-loaded SE must succeed (next callback guarantee)");
                Assert.AreNotEqual(0ul, voiceId, "a successful play must yield a non-zero opaque voice id");

                Assert.AreEqual(MwResult.Ok, MwNative.VoiceSetVolume(handle, voiceId, 0.4f));
                Assert.AreEqual(MwResult.Ok, MwNative.VoiceStop(handle, voiceId));
                Assert.AreEqual(MwResult.Ok, MwNative.ReleaseSound(handle, soundId));
            }
            finally
            {
                Assert.AreEqual(MwResult.Ok, MwNative.Shutdown(handle));
            }
        }

        [Test]
        public void LoadMonoWav_ExpandsWithoutError()
        {
            MwResult initResult = MwNative.Init(out ulong handle);
            Assert.AreEqual(MwResult.Ok, initResult);

            try
            {
                byte[] monoWav = TestWavBuilder.BuildPcm16Wav(sampleRate: 48_000, channels: 1, samples: new short[] { 1000, -1000, 2000 });
                MwResult loadResult = MwNative.LoadSound(handle, monoWav, SoundMode.Se, out ulong soundId);
                Assert.AreEqual(MwResult.Ok, loadResult, "mono 48kHz/16bit wav must decode via equal-power stereo expansion");

                Assert.AreEqual(MwResult.Ok, MwNative.ReleaseSound(handle, soundId));
            }
            finally
            {
                Assert.AreEqual(MwResult.Ok, MwNative.Shutdown(handle));
            }
        }

        /// <summary>
        /// M1 では 48kHz 以外を明示的に拒否していたが、M2-4 でロード時のリサンプル(rubato)が
        /// 入ったため 44.1kHz は通るようになった。<see cref="MwResult.ErrUnsupportedSampleRate"/>
        /// が残っているのは「サンプルレートとして解釈できない値」を弾くためで、
        /// リサンプルできない壊れた wav とは区別される。
        /// </summary>
        [Test]
        public void LoadNon48kWav_IsResampledOnLoad()
        {
            MwResult initResult = MwNative.Init(out ulong handle);
            Assert.AreEqual(MwResult.Ok, initResult);

            try
            {
                byte[] wav44k = TestWavBuilder.BuildPcm16Wav(sampleRate: 44_100, channels: 2, samples: new short[] { 0, 0, 1000, -1000 });
                MwResult loadResult = MwNative.LoadSound(handle, wav44k, SoundMode.Se, out ulong soundId);
                Assert.AreEqual(MwResult.Ok, loadResult, "44.1kHz is resampled to the output rate at load time since M2-4");
                Assert.AreNotEqual(0ul, soundId);
                Assert.AreEqual(MwResult.Ok, MwNative.ReleaseSound(handle, soundId));
            }
            finally
            {
                Assert.AreEqual(MwResult.Ok, MwNative.Shutdown(handle));
            }
        }

        [Test]
        public void LoadZeroSampleRateWav_ReturnsUnsupportedSampleRate()
        {
            MwResult initResult = MwNative.Init(out ulong handle);
            Assert.AreEqual(MwResult.Ok, initResult);

            try
            {
                byte[] wavZeroRate = TestWavBuilder.BuildPcm16Wav(sampleRate: 0, channels: 2, samples: new short[] { 0, 0 });
                MwResult loadResult = MwNative.LoadSound(handle, wavZeroRate, SoundMode.Se, out ulong soundId);
                Assert.AreEqual(MwResult.ErrUnsupportedSampleRate, loadResult, "a sample rate of 0 cannot be resampled from; it must be rejected explicitly");
            }
            finally
            {
                Assert.AreEqual(MwResult.Ok, MwNative.Shutdown(handle));
            }
        }

        /// <summary>
        /// M2-7 で <see cref="SoundMode.Music"/> が実装されたため、
        /// <see cref="MwResult.ErrUnsupportedSoundMode"/> が返るのは 0 / 1 以外を渡した場合だけになった。
        /// 楽曲モードそのものの検証は <see cref="MwNativeMusicEditModeTests"/> 側にある。
        /// </summary>
        [Test]
        public void LoadUnknownMode_ReturnsUnsupportedSoundMode()
        {
            MwResult initResult = MwNative.Init(out ulong handle);
            Assert.AreEqual(MwResult.Ok, initResult);

            try
            {
                byte[] wavBytes = TestWavBuilder.BuildPcm16Wav(sampleRate: 48_000, channels: 2, samples: new short[] { 0, 0 });
                MwResult loadResult = MwNative.LoadSound(handle, wavBytes, (SoundMode)99, out ulong soundId);
                Assert.AreEqual(MwResult.ErrUnsupportedSoundMode, loadResult, "an unknown sound mode must be rejected with a dedicated code");
                Assert.AreEqual(0ul, soundId, "a rejected load must not hand out an id");
            }
            finally
            {
                Assert.AreEqual(MwResult.Ok, MwNative.Shutdown(handle));
            }
        }

    }
}
