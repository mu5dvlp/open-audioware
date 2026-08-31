using System;
using System.Diagnostics;
using System.Runtime.InteropServices;
using System.Threading;
using Mw.Native;
using NUnit.Framework;

namespace Mw.Native.Tests
{
    /// <summary>
    /// M2(楽曲とクロック)の C# ラッパ経路を検証する EditMode テスト。
    ///
    /// <para>
    /// <b>ここで検証したいのは Rust 側のロジックではない。</b>FFI 境界そのものの振る舞い
    /// (状態遷移・ID 空間分離・ループ区間の拒否など)は Rust 側の
    /// <c>run_music_lifecycle</c> が実ハンドル越しに検証済みで、こちらで重ねて確かめても
    /// 同じことを2回やるだけになる。EditMode テストにしかできないのは
    /// <b>「C# 側の薄いラッパが、生成バインディング越しに正しくマーシャリングできているか」</b>
    /// の確認なので、次の3点に絞ってある:
    /// </para>
    /// <list type="number">
    /// <item>Unity の実行環境から dylib を P/Invoke でき、楽曲の状態遷移が観測できること
    /// (= 同梱されているネイティブバイナリが M2 の実装であること。M1 のままだと
    /// <see cref="SoundMode.Music"/> のロード時点で落ちる)</item>
    /// <item><see cref="MusicPosition"/> の詰め替えが正しいこと —— ネイティブ側は
    /// <c>is_playing</c> を <c>u8</c> で持っており、C# の <c>bool</c> と幅が違うため
    /// ここを取り違えるとフィールドが丸ごとずれる</item>
    /// <item><see cref="MwEventData"/> のレイアウトがネイティブ側と一致していること ——
    /// <see cref="MwNative.PollEvents"/> は詰め替えをせずポインタを再解釈するので、
    /// ずれると静かに壊れた値を読む</item>
    /// </list>
    /// </summary>
    public class MwNativeMusicEditModeTests
    {
        /// <summary>
        /// プリロール完了・再生開始・曲末到達を待つときの上限。デコードスレッドの
        /// ポーリング周期(10ms)とオーディオコールバック周期に対して十分長く取る。
        /// </summary>
        private static readonly TimeSpan WaitTimeout = TimeSpan.FromSeconds(10);

        /// <summary>テスト素材の長さ。曲末(<see cref="EventKind.MusicEnded"/>)まで待つので短くする。</summary>
        private const double MusicSeconds = 0.35;

        [Test]
        public void MusicLifecycle_ThroughWrapper_ReachesReadyThenPlaysThenEnds()
        {
            Assert.AreEqual(MwResult.Ok, MwNative.Init(out ulong handle), "mw_init should succeed on a machine with a default audio output device");

            try
            {
                byte[] musicBytes = TestWavBuilder.BuildStereoToneWav(MusicSeconds);

                Assert.AreEqual(
                    MwResult.Ok,
                    MwNative.LoadSound(handle, musicBytes, SoundMode.Music, out ulong musicId),
                    "Music mode has been implemented since M2-7; a failure here usually means the bundled native binary is stale");
                Assert.AreNotEqual(0ul, musicId);

                Assert.AreEqual(MwResult.Ok, MwNative.SetMusic(handle, musicId), "opening the decoder must succeed for a valid wav");

                // SetMusic は非ブロッキングで、プリロール完了は待たない契約。
                // Ready へ遷移するのはオーディオコールバックがコマンドを処理した後なので、
                // ここが通ること自体が「Unity 実行下でもネイティブのストリームが回っている」証拠になる。
                MusicState observedState = MusicState.Loading;
                bool becameReady = WaitUntil(() =>
                {
                    Assert.AreEqual(MwResult.Ok, MwNative.GetMusicState(handle, out observedState));
                    return observedState == MusicState.Ready;
                });
                Assert.IsTrue(becameReady, $"music must become Ready once preroll completes; last observed state={observedState}");

                Assert.AreEqual(MwResult.Ok, MwNative.GetMusicPosition(handle, out MusicPosition readyPosition));
                Assert.AreEqual(MusicState.Ready, readyPosition.State);
                Assert.IsFalse(readyPosition.IsPlaying, "IsPlaying must be false while merely Ready (u8 -> bool conversion)");
                Assert.Greater(readyPosition.SampleRate, 0u, "the sample rate must be settled once the stream has rendered at least one callback");

                // 直前までに溜まったイベントを捨ててから再生する。ここを流さないと
                // このテストの前に発生したイベント(初期化時のアンダーラン等)を
                // MusicEnded と取り違えかねない。
                DrainEvents(handle);

                Assert.AreEqual(MwResult.Ok, MwNative.MusicPlayScheduled(handle, MwNative.HostTimeNs()));

                MusicPosition playingPosition = default;
                bool startedPlaying = WaitUntil(() =>
                {
                    Assert.AreEqual(MwResult.Ok, MwNative.GetMusicPosition(handle, out playingPosition));
                    return playingPosition.IsPlaying;
                });
                Assert.IsTrue(startedPlaying, $"scheduled playback must start; last observed state={playingPosition.State}");
                Assert.AreEqual(MusicState.Playing, playingPosition.State, "IsPlaying and State must agree — they come from the same seqlock snapshot");
                Assert.AreNotEqual(0ul, playingPosition.HostTimeNs, "a playing snapshot must carry the host time it corresponds to");

                // 曲末まで再生されると MusicEnded が飛ぶ。ここまで通れば
                // MwEventData のレイアウトが実際に噛み合っていることまで確認できる。
                var buffer = new MwEventData[16];
                bool sawEnded = WaitUntil(() =>
                {
                    Assert.AreEqual(MwResult.Ok, MwNative.PollEvents(handle, buffer, out int count, out uint dropped));
                    Assert.AreEqual(0u, dropped, "the event queue must not overflow during this short test");
                    for (int i = 0; i < count; i++)
                    {
                        if (buffer[i].Kind == EventKind.MusicEnded)
                        {
                            Assert.AreEqual(0ul, buffer[i].Payload, "MusicEnded carries no payload");
                            return true;
                        }
                    }

                    return false;
                });
                Assert.IsTrue(sawEnded, "playing a short song to completion must raise MusicEnded");

                Assert.AreEqual(MwResult.Ok, MwNative.ReleaseSound(handle, musicId));
            }
            finally
            {
                Assert.AreEqual(MwResult.Ok, MwNative.Shutdown(handle));
            }
        }

        /// <summary>
        /// ハンドルを1本だけ開いて済ませられる軽い検証をまとめたもの。
        /// (Init/Shutdown はデコードスレッドの起動・join を伴うため、テストごとに
        /// 繰り返すより1本にまとめたほうが速く、フレークの余地も減る。)
        /// </summary>
        [Test]
        public void MusicApi_RejectsBadInputAndReportsLatency()
        {
            Assert.AreEqual(MwResult.Ok, MwNative.Init(out ulong handle));

            try
            {
                // ループ区間: begin >= end は拒否、0/0 は「解除」として通る。
                Assert.AreEqual(MwResult.ErrInvalidLoopRegion, MwNative.SetMusicLoop(handle, 5, 3));
                Assert.AreEqual(MwResult.ErrInvalidLoopRegion, MwNative.SetMusicLoop(handle, 7, 7));
                Assert.AreEqual(MwResult.Ok, MwNative.SetMusicLoop(handle, 0, 200));
                Assert.AreEqual(MwResult.Ok, MwNative.ClearMusicLoop(handle), "(0, 0) means \"clear the loop\", not an invalid region");

                // ID 空間の分離: SE としてロードした ID は楽曲 API に渡せない。
                byte[] seBytes = TestWavBuilder.BuildPcm16Wav(sampleRate: 48_000, channels: 2, samples: new short[] { 1000, -1000 });
                Assert.AreEqual(MwResult.Ok, MwNative.LoadSound(handle, seBytes, SoundMode.Se, out ulong seId));
                Assert.AreEqual(MwResult.ErrInvalidSoundId, MwNative.SetMusic(handle, seId), "an SE id must not be usable as a music id");
                Assert.AreEqual(MwResult.Ok, MwNative.ReleaseSound(handle, seId));

                // 出力レイテンシ: 0 は「まだ未計測」を意味するだけで失敗ではない。
                Assert.AreEqual(MwResult.Ok, MwNative.GetOutputLatencyNs(handle, out ulong latencyNs));
                Assert.LessOrEqual(latencyNs, 1_000_000_000ul, "an output latency of over a second would mean the value is being read from the wrong field");

                // 空バッファでのポーリングはネイティブ側が buf を触らない経路。
                Assert.AreEqual(MwResult.Ok, MwNative.PollEvents(handle, null, out int nullCount, out uint _));
                Assert.AreEqual(0, nullCount);
                Assert.AreEqual(MwResult.Ok, MwNative.PollEvents(handle, Array.Empty<MwEventData>(), out int emptyCount, out uint _));
                Assert.AreEqual(0, emptyCount);
            }
            finally
            {
                Assert.AreEqual(MwResult.Ok, MwNative.Shutdown(handle));
            }
        }

        /// <summary>
        /// <see cref="MwNative.HostTimeNs"/> はハンドル不要で、予約再生の時刻軸の基準になる。
        /// 単調であること(巻き戻らないこと)が予約再生の前提。
        /// </summary>
        [Test]
        public void HostTimeNs_IsNonZeroAndMonotonic()
        {
            ulong first = MwNative.HostTimeNs();
            Assert.AreNotEqual(0ul, first, "the host clock must be readable without a handle");

            ulong previous = first;
            for (int i = 0; i < 50; i++)
            {
                ulong current = MwNative.HostTimeNs();
                Assert.GreaterOrEqual(current, previous, "the host clock must never go backwards");
                previous = current;
            }

            Thread.Sleep(20);
            Assert.Greater(MwNative.HostTimeNs(), first, "the host clock must actually advance");
        }

        /// <summary>
        /// <see cref="MwNative.PollEvents"/> は要素ごとの詰め替えをせず、呼び出し側の配列を
        /// ネイティブ側の <c>MwEvent</c> の配列として再解釈させる(GC アロケーションゼロ)。
        /// そのためレイアウトの一致が前提条件になっている。ここで期待値として書いている
        /// 数値はネイティブ側の <c>#[repr(C)] struct MwEvent { kind: i32, payload: u64 }</c> の
        /// ABI そのもので、<see cref="MwEventData"/> のフィールドを並べ替えると落ちる。
        /// </summary>
        [Test]
        public void MwEventDataLayout_MatchesNativeAbi()
        {
            Assert.AreEqual(16, Marshal.SizeOf<MwEventData>(), "kind(i32) + 4 bytes padding + payload(u64)");
            Assert.AreEqual(IntPtr.Zero, Marshal.OffsetOf<MwEventData>(nameof(MwEventData.Kind)));
            Assert.AreEqual(new IntPtr(8), Marshal.OffsetOf<MwEventData>(nameof(MwEventData.Payload)));
            Assert.AreEqual(4, Marshal.SizeOf(Enum.GetUnderlyingType(typeof(EventKind))), "MwEventKind is #[repr(i32)] on the native side");
        }

        /// <summary>
        /// <see cref="EventKind"/> の判別子はネイティブ側 <c>MwEventKind</c>
        /// (<c>crates/mw-ffi/src/event.rs</c>)と1:1で同期させる契約になっている
        /// (csbindgen が生成しない手書きの列挙のため、ズレを機械的には検出できない)。
        /// <see cref="EventKind.AudioInterruptionEnded"/> は M3 で iOS/tvOS 用に
        /// 追加された後、C# 側の列挙に反映されないまま残っていた欠落を埋めたもの
        /// (Android 内部再オープンの結果通知〔このテストが書かれた作業〕で
        /// 再利用するために必要になった)。
        /// </summary>
        [Test]
        public void EventKind_ValuesMatchNativeDiscriminants()
        {
            Assert.AreEqual(0, (int)EventKind.RouteChanged);
            Assert.AreEqual(1, (int)EventKind.Underrun);
            Assert.AreEqual(2, (int)EventKind.MusicEnded);
            Assert.AreEqual(3, (int)EventKind.MusicLooped);
            Assert.AreEqual(4, (int)EventKind.StreamError);
            Assert.AreEqual(5, (int)EventKind.ClipperEngaged);
            Assert.AreEqual(6, (int)EventKind.AudioInterruptionBegan);
            Assert.AreEqual(7, (int)EventKind.AudioInterruptionEnded);
        }

        /// <summary>溜まっているイベントを空になるまで読み捨てる。</summary>
        private static void DrainEvents(ulong handle)
        {
            var buffer = new MwEventData[32];
            for (int guard = 0; guard < 16; guard++)
            {
                Assert.AreEqual(MwResult.Ok, MwNative.PollEvents(handle, buffer, out int count, out uint _));
                if (count == 0)
                {
                    return;
                }
            }

            Assert.Fail("the event queue kept producing events while draining; something is generating them continuously");
        }

        /// <summary>
        /// <paramref name="condition"/> が true を返すまで短い間隔でポーリングする
        /// (タイムアウト付き)。EditMode テストにはフレーム進行が無いため、
        /// コルーチンではなくスリープで待つ。
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
