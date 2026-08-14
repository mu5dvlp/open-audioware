using Mw.Native;
using NUnit.Framework;

namespace Mw.Native.Tests
{
    /// <summary>
    /// M0(基盤)の終了条件そのものを検証する EditMode テスト。
    /// 初期構築仕様 §10: 「macOS Editor 上で init/shutdown が動く」。
    /// </summary>
    public class MwNativeEditModeTests
    {
        [Test]
        public void AbiVersion_MatchesExpected()
        {
            Assert.AreEqual(MwNative.ExpectedAbiVersion, MwNative.AbiVersion());
        }

        [Test]
        public void InitThenShutdown_ReturnsOk()
        {
            MwResult initResult = MwNative.Init(out ulong handle);
            Assert.AreEqual(
                MwResult.Ok,
                initResult,
                "mw_init should succeed on a machine with a default audio output device");
            Assert.AreNotEqual(0ul, handle, "a successful init must yield a non-zero opaque handle");

            MwResult shutdownResult = MwNative.Shutdown(handle);
            Assert.AreEqual(
                MwResult.Ok,
                shutdownResult,
                "mw_shutdown should succeed for a handle that was just returned by mw_init");
        }
    }
}
