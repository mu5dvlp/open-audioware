using Mw.Native.Generated;

namespace Mw.Native
{
    /// <summary>
    /// FFI 呼び出しの結果コード。ネイティブ側 <c>MwResult</c>(初期構築仕様 §4.8)に対応する
    /// 公開用の列挙。値はネイティブ側と1:1で同期させること。
    /// <para>
    /// 生成コード側の <see cref="Generated.MwResult"/> は internal のため、パッケージ外へ
    /// 公開する値はこの列挙を経由する(薄いラッパの役割。生成物の変更がそのまま
    /// パッケージの公開 API に漏れ出さないようにする)。
    /// </para>
    /// </summary>
    public enum MwResult
    {
        /// <summary>成功。</summary>
        Ok = 0,

        /// <summary>出力引数として渡されたポインタが null だった。</summary>
        ErrNullPointer = -1,

        /// <summary>ハンドルが無効(未初期化 / 既に shutdown 済み / 他インスタンスのハンドル)。</summary>
        ErrInvalidHandle = -2,

        /// <summary>出力バックエンド(cpal ストリーム)のオープンに失敗した。</summary>
        ErrBackendOpenFailed = -3,

        /// <summary>出力バックエンドのクローズに失敗した。</summary>
        ErrBackendCloseFailed = -4,

        /// <summary>FFI 境界内で Rust panic を捕捉した。呼び出し元に漏れることはない。</summary>
        ErrPanic = -5,

        /// <summary>
        /// <see cref="MwNative.LoadSound"/> の <c>mode</c> が M1 未実装(<see cref="SoundMode.Music"/>)、
        /// または未知の値だった。楽曲モードは M2 で実装予定(初期構築仕様 §5.2)。
        /// </summary>
        ErrUnsupportedSoundMode = -6,

        /// <summary>wav のパースに失敗した(RIFF/WAVE 構造が壊れている等)。</summary>
        ErrDecodeFailed = -7,

        /// <summary>
        /// wav が 48kHz 以外だった。リサンプルは M2(rubato)で対応予定(初期構築仕様 §4.7)。
        /// </summary>
        ErrUnsupportedSampleRate = -8,

        /// <summary>wav が 16bit PCM でない、またはチャンネル数がモノ/ステレオでない。</summary>
        ErrUnsupportedFormat = -9,

        /// <summary>指定されたサウンド ID が存在しない(未ロード / 既に解放済み)。</summary>
        ErrInvalidSoundId = -10,

        /// <summary>
        /// コマンドキューが満杯で発行できなかった。初期構築仕様 §4.2 の
        /// 「次のコールバックで必ず発音される」という保証はコマンドがキューへ積まれたことが
        /// 前提のため、黙って捨てずにこのエラーを返す。
        /// </summary>
        ErrCommandQueueFull = -11,

        /// <summary>
        /// <c>bus</c> 引数が固定4本(<see cref="Bus.Master"/>/<see cref="Bus.Bgm"/>/
        /// <see cref="Bus.Se"/>/<see cref="Bus.Voice"/>)のいずれにも対応しない値だった。
        /// </summary>
        ErrInvalidBus = -12,
    }

    /// <summary>
    /// <see cref="MwNative.LoadSound"/> の <c>mode</c> 引数(初期構築仕様 §5.2, §5.5)。
    /// M1 は <see cref="Se"/>(全デコード常駐)のみ実装する。
    /// </summary>
    public enum SoundMode
    {
        /// <summary>効果音。ロード時に全デコードしてメモリ常駐させる(初期構築仕様 §4.2)。</summary>
        Se = 0,

        /// <summary>
        /// 楽曲。圧縮のまま保持しストリーミングデコードする(M2 で実装予定。
        /// M1 では <see cref="MwResult.ErrUnsupportedSoundMode"/> になる)。
        /// </summary>
        Music = 1,
    }

    /// <summary>固定4本のバス(初期構築仕様 §4.1)。</summary>
    public enum Bus
    {
        Master = 0,
        Bgm = 1,
        Se = 2,
        Voice = 3,
    }

    /// <summary>
    /// audio-middleware-sample のネイティブ層への薄いラッパ。
    /// csbindgen が生成した <c>Mw.Native.Generated.NativeMethods</c>(internal)を直接
    /// 呼ばず、必ずこのクラスを経由すること(初期構築仕様 §5.4)。
    /// <para>
    /// M1(SE 再生)で追加した関数はすべてゲームスレッドから呼ぶ想定で、内部ではコマンドを
    /// ロックフリーキューへ積むだけの非ブロッキング呼び出し(初期構築仕様 §5.2)。
    /// 実際の発音・停止・音量変化は次のオーディオコールバックで処理される。
    /// </para>
    /// </summary>
    public static class MwNative
    {
        /// <summary>このラッパが前提とする ABI バージョン。<see cref="AbiVersion"/> と照合すること。</summary>
        public const uint ExpectedAbiVersion = 1;

        /// <summary>
        /// ネイティブ側の ABI バージョンを返す。呼び出し側は起動時に
        /// <see cref="ExpectedAbiVersion"/> と一致するか確認すること(初期構築仕様 §4.8)。
        /// ABI 互換の破壊は semver メジャーバージョンでのみ許可される。
        /// </summary>
        public static uint AbiVersion()
        {
            return NativeMethods.mw_abi_version();
        }

        /// <summary>
        /// ミドルウェアを初期化し、既定の出力デバイスへストリームを開いて再生を開始する。
        /// <para>
        /// 冪等: 既に初期化済みの場合は同一ハンドルが返る(Unity Editor のドメインリロード
        /// 対策、初期構築仕様 §6)。呼び出し側はドメインアンロード時に必ず
        /// <see cref="Shutdown"/> を呼ぶこと。
        /// </para>
        /// </summary>
        /// <param name="handle">成功時、以後の全呼び出しに渡す不透明ハンドル。</param>
        /// <returns><see cref="MwResult.Ok"/> 以外は失敗。</returns>
        public static unsafe MwResult Init(out ulong handle)
        {
            ulong h = 0;
            Generated.MwResult native = NativeMethods.mw_init(&h);
            handle = h;
            return ToPublicResult(native);
        }

        /// <summary>
        /// ミドルウェアを終了し、出力ストリームを停止する。
        /// 無効なハンドル(未初期化・二重 shutdown・他インスタンスのハンドル)は
        /// <see cref="MwResult.ErrInvalidHandle"/> を返す。クラッシュはしない。
        /// </summary>
        public static MwResult Shutdown(ulong handle)
        {
            Generated.MwResult native = NativeMethods.mw_shutdown(handle);
            return ToPublicResult(native);
        }

        /// <summary>
        /// wav バイト列から PCM をロードする(初期構築仕様 §5.5, §4.2)。
        /// M1 は <see cref="SoundMode.Se"/>(全デコード常駐)のみ実装する。
        /// 対応フォーマットは 48kHz / 16bit PCM / モノラルまたはステレオの wav のみ。
        /// </summary>
        /// <param name="handle"><see cref="Init"/> が返したハンドル。</param>
        /// <param name="bytes">wav ファイルの生バイト列。</param>
        /// <param name="mode">M1 では <see cref="SoundMode.Se"/> のみ有効。</param>
        /// <param name="id">成功時、以後 <see cref="PlaySe"/> / <see cref="ReleaseSound"/> に渡す不透明 ID。</param>
        public static unsafe MwResult LoadSound(ulong handle, byte[] bytes, SoundMode mode, out ulong id)
        {
            ulong outId = 0;
            Generated.MwResult native;
            if (bytes == null || bytes.Length == 0)
            {
                native = NativeMethods.mw_sound_load(handle, null, 0, (int)mode, &outId);
            }
            else
            {
                fixed (byte* p = bytes)
                {
                    native = NativeMethods.mw_sound_load(handle, p, (nuint)bytes.Length, (int)mode, &outId);
                }
            }
            id = outId;
            return ToPublicResult(native);
        }

        /// <summary>
        /// ロード済みサウンドを解放する。再生中のボイスがあれば既定ランプ経由で即座に
        /// 停止させたうえで解放する(初期構築仕様 M13/§4.1、「PCM データの所有権」)。
        /// 未知の <paramref name="id"/> は <see cref="MwResult.ErrInvalidSoundId"/> を返す。
        /// </summary>
        public static MwResult ReleaseSound(ulong handle, ulong id)
        {
            Generated.MwResult native = NativeMethods.mw_sound_release(handle, id);
            return ToPublicResult(native);
        }

        /// <summary>
        /// SE を即時発音する(初期構築仕様 §4.2)。次のオーディオコールバックで必ず発音される
        /// (遅延は1バッファ + 出力レイテンシのみ)。
        /// </summary>
        /// <param name="handle"><see cref="Init"/> が返したハンドル。</param>
        /// <param name="id"><see cref="LoadSound"/> が返したサウンド ID。</param>
        /// <param name="bus">再生先バス。</param>
        /// <param name="volume">再生音量(linear)。</param>
        /// <param name="voice">成功時、以後 <see cref="VoiceStop"/> / <see cref="VoiceSetVolume"/> に渡す不透明 ID。</param>
        public static unsafe MwResult PlaySe(ulong handle, ulong id, Bus bus, float volume, out ulong voice)
        {
            ulong outVoice = 0;
            Generated.MwResult native = NativeMethods.mw_se_play(handle, id, (int)bus, volume, &outVoice);
            voice = outVoice;
            return ToPublicResult(native);
        }

        /// <summary>ボイスを停止する(既定ランプ経由。初期構築仕様 M13/§4.2)。</summary>
        public static MwResult VoiceStop(ulong handle, ulong voice)
        {
            Generated.MwResult native = NativeMethods.mw_voice_stop(handle, voice);
            return ToPublicResult(native);
        }

        /// <summary>ボイスの音量を変更する(既定ランプ経由。初期構築仕様 M13/§4.2)。</summary>
        public static MwResult VoiceSetVolume(ulong handle, ulong voice, float volume)
        {
            Generated.MwResult native = NativeMethods.mw_voice_set_volume(handle, voice, volume);
            return ToPublicResult(native);
        }

        /// <summary>バス音量を変更する(既定ランプ経由。初期構築仕様 M13/§4.1)。</summary>
        public static MwResult BusSetVolume(ulong handle, Bus bus, float volume)
        {
            Generated.MwResult native = NativeMethods.mw_bus_set_volume(handle, (int)bus, volume);
            return ToPublicResult(native);
        }

        /// <summary>バスをフェードする(呼び出し側指定の時間、ms。初期構築仕様 §4.1)。</summary>
        public static MwResult BusFade(ulong handle, Bus bus, float target, float ms)
        {
            Generated.MwResult native = NativeMethods.mw_bus_fade(handle, (int)bus, target, ms);
            return ToPublicResult(native);
        }

        /// <summary>
        /// 生成コードの内部列挙(<see cref="Generated.MwResult"/>)を公開列挙
        /// (<see cref="MwResult"/>)へ変換する。両者は数値レベルで同一の契約
        /// (初期構築仕様 §4.8: エラーコードは負の整数)を持つため単純なキャストで済む。
        /// </summary>
        private static MwResult ToPublicResult(Generated.MwResult native)
        {
            return (MwResult)(int)native;
        }
    }
}
