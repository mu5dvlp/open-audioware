namespace Mw.Native.Tests
{
    /// <summary>
    /// テスト専用の最小 RIFF/WAVE(PCM16)バイト列ビルダ。バイナリ資産をコミットしない方針
    /// (初期構築仕様 §8)に合わせ、波形は常にコードで生成する。
    /// <para>
    /// SE(<see cref="SoundMode.Se"/>)と楽曲(<see cref="SoundMode.Music"/>)の両テストから
    /// 使うため独立したクラスに切り出してある。SE 経路は 48kHz / 16bit / モノラルまたは
    /// ステレオしか受け付けないが、楽曲経路は Symphonia がデコードするため制約が緩い。
    /// </para>
    /// </summary>
    internal static class TestWavBuilder
    {
        /// <summary>
        /// PCM16 の wav バイト列を組み立てる。
        /// </summary>
        /// <param name="sampleRate">サンプルレート [Hz]。</param>
        /// <param name="channels">チャンネル数。</param>
        /// <param name="samples">インターリーブ済みサンプル列(長さは channels の倍数であること)。</param>
        public static byte[] BuildPcm16Wav(uint sampleRate, ushort channels, short[] samples)
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

        /// <summary>
        /// 指定秒数ぶんの 48kHz ステレオ矩形波を生成する(楽曲テスト用)。
        /// 無音にしないのは、デコード結果が本当に流れていることを波形として区別できる
        /// 余地を残しておくため。
        /// </summary>
        public static byte[] BuildStereoToneWav(double seconds, uint sampleRate = 48_000)
        {
            int frames = (int)(seconds * sampleRate);
            var samples = new short[frames * 2];
            const int periodFrames = 128;
            for (int i = 0; i < frames; i++)
            {
                short value = (i % periodFrames) < (periodFrames / 2) ? (short)6000 : (short)-6000;
                samples[(i * 2) + 0] = value;
                samples[(i * 2) + 1] = value;
            }

            return BuildPcm16Wav(sampleRate, 2, samples);
        }
    }
}
