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
    }

    /// <summary>
    /// audio-middleware-sample のネイティブ層への薄いラッパ。
    /// csbindgen が生成した <c>Mw.Native.Generated.NativeMethods</c>(internal)を直接
    /// 呼ばず、必ずこのクラスを経由すること(初期構築仕様 §5.4)。
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
        /// <param name="handle">成功時、以後 <see cref="Shutdown"/> に渡す不透明ハンドル。</param>
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
