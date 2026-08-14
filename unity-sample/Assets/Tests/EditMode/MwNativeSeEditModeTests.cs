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
                byte[] wavBytes = BuildPcm16Wav(sampleRate: 48_000, channels: 2, samples: new short[]
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
                byte[] monoWav = BuildPcm16Wav(sampleRate: 48_000, channels: 1, samples: new short[] { 1000, -1000, 2000 });
                MwResult loadResult = MwNative.LoadSound(handle, monoWav, SoundMode.Se, out ulong soundId);
                Assert.AreEqual(MwResult.Ok, loadResult, "mono 48kHz/16bit wav must decode via equal-power stereo expansion");

                Assert.AreEqual(MwResult.Ok, MwNative.ReleaseSound(handle, soundId));
            }
            finally
            {
                Assert.AreEqual(MwResult.Ok, MwNative.Shutdown(handle));
            }
        }

        [Test]
        public void LoadNon48kWav_ReturnsUnsupportedSampleRate()
        {
            MwResult initResult = MwNative.Init(out ulong handle);
            Assert.AreEqual(MwResult.Ok, initResult);

            try
            {
                byte[] wav44k = BuildPcm16Wav(sampleRate: 44_100, channels: 2, samples: new short[] { 0, 0 });
                MwResult loadResult = MwNative.LoadSound(handle, wav44k, SoundMode.Se, out ulong soundId);
                Assert.AreEqual(MwResult.ErrUnsupportedSampleRate, loadResult, "44.1kHz must be rejected explicitly (M1 does not resample; see M2/rubato)");
            }
            finally
            {
                Assert.AreEqual(MwResult.Ok, MwNative.Shutdown(handle));
            }
        }

        [Test]
        public void LoadMusicMode_ReturnsUnsupportedSoundMode()
        {
            MwResult initResult = MwNative.Init(out ulong handle);
            Assert.AreEqual(MwResult.Ok, initResult);

            try
            {
                byte[] wavBytes = BuildPcm16Wav(sampleRate: 48_000, channels: 2, samples: new short[] { 0, 0 });
                MwResult loadResult = MwNative.LoadSound(handle, wavBytes, SoundMode.Music, out ulong soundId);
                Assert.AreEqual(MwResult.ErrUnsupportedSoundMode, loadResult, "Music mode is not implemented until M2");
            }
            finally
            {
                Assert.AreEqual(MwResult.Ok, MwNative.Shutdown(handle));
            }
        }

        /// <summary>
        /// テスト専用の最小 RIFF/WAVE(PCM16)バイト列ビルダ。バイナリ資産をコミットしない方針
        /// (初期構築仕様 §8)に合わせ、波形は常にコードで生成する。
        /// </summary>
        private static byte[] BuildPcm16Wav(uint sampleRate, ushort channels, short[] samples)
        {
            const ushort bitsPerSample = 16;
            ushort blockAlign = (ushort)(channels * (bitsPerSample / 8));
            uint byteRate = sampleRate * blockAlign;
            uint dataSize = (uint)(samples.Length * sizeof(short));
            const uint fmtSize = 16;
            uint riffSize = 4 + (8 + fmtSize) + (8 + dataSize);

            using (var stream = new System.IO.MemoryStream())
            using (var writer = new System.IO.BinaryWriter(stream))
            {
                writer.Write(new[] { 'R', 'I', 'F', 'F' });
                writer.Write(riffSize);
                writer.Write(new[] { 'W', 'A', 'V', 'E' });

                writer.Write(new[] { 'f', 'm', 't', ' ' });
                writer.Write(fmtSize);
                writer.Write((ushort)1); // PCM
                writer.Write(channels);
                writer.Write(sampleRate);
                writer.Write(byteRate);
                writer.Write(blockAlign);
                writer.Write(bitsPerSample);

                writer.Write(new[] { 'd', 'a', 't', 'a' });
                writer.Write(dataSize);
                foreach (short sample in samples)
                {
                    writer.Write(sample);
                }

                writer.Flush();
                return stream.ToArray();
            }
        }
    }
}
