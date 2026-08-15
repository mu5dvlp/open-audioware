using Measurement;
using Mw.Native;
using NUnit.Framework;
using UnityEngine;

namespace Mw.Native.Tests
{
    /// <summary>
    /// M1 A/B 計測(docs/measurement-m1.md)で A・B 両実装に使う <see cref="ClickSeGenerator"/> の
    /// EditMode テスト。波形そのものの健全性と、生成した wav バイト列がネイティブ側の
    /// ロード要件(48kHz/16bit PCM/モノラル)を満たして実際に load/play できることを確認する。
    /// </summary>
    public class ClickSeGeneratorEditModeTests
    {
        [Test]
        public void GenerateMonoSamples_StartsAtPeakAndStaysInRange()
        {
            float[] samples = ClickSeGenerator.GenerateMonoSamples();

            Assert.Greater(samples.Length, 0, "click SE must contain at least one sample");

            // 位相を π/2 ずらしたコサイン減衰バーストのため、サンプル0で振幅がほぼ最大になる
            // (docs/measurement-m1.md §2.2: 明瞭な立ち上がり)。
            Assert.Greater(samples[0], 0.8f, "sample 0 should already be near peak amplitude for a sharp attack");

            foreach (float sample in samples)
            {
                Assert.IsFalse(float.IsNaN(sample), "generated waveform must not contain NaN");
                Assert.LessOrEqual(Mathf.Abs(sample), 1f, "generated waveform must stay within [-1, 1]");
            }
        }

        [Test]
        public void BuildAudioClip_MatchesGeneratedSampleCountAndRate()
        {
            float[] samples = ClickSeGenerator.GenerateMonoSamples();
            AudioClip clip = ClickSeGenerator.BuildAudioClip();

            Assert.AreEqual(samples.Length, clip.samples, "A-side AudioClip must carry the same sample count as the generator");
            Assert.AreEqual(1, clip.channels, "A-side AudioClip is mono (matches the wav sent to the native side)");
            Assert.AreEqual(ClickSeGenerator.SampleRate, clip.frequency, "A-side AudioClip must be generated at the native-required sample rate");
        }

        [Test]
        public void BuildWavBytes_LoadsSuccessfullyViaMwNative()
        {
            MwResult initResult = MwNative.Init(out ulong handle);
            Assert.AreEqual(MwResult.Ok, initResult);

            try
            {
                byte[] wavBytes = ClickSeGenerator.BuildWavBytes();
                Assert.Greater(wavBytes.Length, 44, "wav must contain more than just the RIFF/fmt header");

                MwResult loadResult = MwNative.LoadSound(handle, wavBytes, SoundMode.Se, out ulong soundId);
                Assert.AreEqual(MwResult.Ok, loadResult, "the generated click SE must satisfy mw_sound_load's 48kHz/16bit/mono-or-stereo requirement");

                MwResult playResult = MwNative.PlaySe(handle, soundId, Bus.Se, volume: 1f, out ulong voiceId);
                Assert.AreEqual(MwResult.Ok, playResult, "B-side playback (native) must succeed with the generated click SE");
                Assert.AreNotEqual(0ul, voiceId);

                Assert.AreEqual(MwResult.Ok, MwNative.ReleaseSound(handle, soundId));
            }
            finally
            {
                Assert.AreEqual(MwResult.Ok, MwNative.Shutdown(handle));
            }
        }
    }
}
