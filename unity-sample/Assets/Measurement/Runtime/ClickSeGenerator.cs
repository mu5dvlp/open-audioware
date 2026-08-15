using System;
using System.IO;
using UnityEngine;

namespace Measurement
{
    /// <summary>
    /// M1 A/B 計測(docs/measurement-m1.md)で A・B 両実装に共通して使う、短いアタックの
    /// クリック SE をコードで合成する。権利的にクリーンな自作波形にするため、バイナリ資産は
    /// 一切コミットせず常に実行時に生成する(初期構築仕様 §8 のゴールデン波形と同じ方針)。
    /// <para>
    /// 波形は「サンプル0で振幅が瞬間的に最大へ立ち上がるコサイン減衰バースト」。
    /// sin(0)=0 のトーンバーストは立ち上がりが緩やかになるが、位相を π/2 ずらして
    /// cos を使うことで無音→最大振幅への1サンプルジャンプ(=鋭いアタック)になり、
    /// 外部録音の波形解析でトリガー時刻を特定しやすくなる。
    /// </para>
    /// </summary>
    public static class ClickSeGenerator
    {
        /// <summary>ネイティブミドルウェアが M1 で対応する唯一のサンプルレート(初期構築仕様 §4.7)。</summary>
        public const int SampleRate = 48_000;

        private const float FrequencyHz = 2000f;
        private const float DurationSeconds = 0.03f;
        private const float DecayTauSeconds = 0.006f;
        private const float PeakAmplitude = 0.9f;

        /// <summary>モノラル・[-1,1] 範囲の float サンプル列を生成する(サンプル0 = 最大振幅)。</summary>
        public static float[] GenerateMonoSamples()
        {
            int sampleCount = (int)(DurationSeconds * SampleRate);
            var samples = new float[sampleCount];
            for (int i = 0; i < sampleCount; i++)
            {
                float t = i / (float)SampleRate;
                float envelope = Mathf.Exp(-t / DecayTauSeconds);
                samples[i] = PeakAmplitude * envelope * Mathf.Cos(2f * Mathf.PI * FrequencyHz * t);
            }

            return samples;
        }

        /// <summary>
        /// A実装(Unity <c>AudioSource.PlayOneShot</c>)用に <see cref="GenerateMonoSamples"/> と
        /// 同一波形の <see cref="AudioClip"/> を組み立てる。
        /// </summary>
        public static AudioClip BuildAudioClip()
        {
            float[] samples = GenerateMonoSamples();
            var clip = AudioClip.Create("MeasurementClickSe", samples.Length, channels: 1, frequency: SampleRate, stream: false);
            clip.SetData(samples, 0);
            return clip;
        }

        /// <summary>
        /// B実装(<c>Mw.Native.MwNative.PlaySe</c>)用に <see cref="GenerateMonoSamples"/> と
        /// 同一波形を 48kHz/16bit PCM モノラルの RIFF/WAVE バイト列として組み立てる
        /// (<c>MwNative.LoadSound</c> が要求するフォーマット)。
        /// </summary>
        public static byte[] BuildWavBytes()
        {
            return BuildPcm16MonoWav(GenerateMonoSamples());
        }

        private static byte[] BuildPcm16MonoWav(float[] floatSamples)
        {
            const ushort channels = 1;
            const ushort bitsPerSample = 16;
            const uint fmtSize = 16;
            ushort blockAlign = (ushort)(channels * (bitsPerSample / 8));
            uint byteRate = (uint)SampleRate * blockAlign;
            uint dataSize = (uint)(floatSamples.Length * sizeof(short));
            uint riffSize = 4 + (8 + fmtSize) + (8 + dataSize);

            using (var stream = new MemoryStream())
            using (var writer = new BinaryWriter(stream))
            {
                writer.Write(new[] { 'R', 'I', 'F', 'F' });
                writer.Write(riffSize);
                writer.Write(new[] { 'W', 'A', 'V', 'E' });

                writer.Write(new[] { 'f', 'm', 't', ' ' });
                writer.Write(fmtSize);
                writer.Write((ushort)1); // PCM
                writer.Write(channels);
                writer.Write((uint)SampleRate);
                writer.Write(byteRate);
                writer.Write(blockAlign);
                writer.Write(bitsPerSample);

                writer.Write(new[] { 'd', 'a', 't', 'a' });
                writer.Write(dataSize);
                foreach (float sample in floatSamples)
                {
                    int scaled = Mathf.RoundToInt(sample * short.MaxValue);
                    short clamped = (short)Math.Clamp(scaled, short.MinValue, short.MaxValue);
                    writer.Write(clamped);
                }

                writer.Flush();
                return stream.ToArray();
            }
        }
    }
}
