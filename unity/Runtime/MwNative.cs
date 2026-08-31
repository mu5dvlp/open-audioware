using System.Runtime.InteropServices;
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
        /// <see cref="MwNative.LoadSound"/> の <c>mode</c> が未知の値だった
        /// (初期構築仕様『§5.2』)。<see cref="SoundMode.Music"/> は M2-7 で実装済みのため、
        /// 現在このコードが返るのは 0 / 1 以外を渡した場合のみ。
        /// </summary>
        ErrUnsupportedSoundMode = -6,

        /// <summary>wav のパースに失敗した(RIFF/WAVE 構造が壊れている等)。</summary>
        ErrDecodeFailed = -7,

        /// <summary>
        /// wav のサンプルレートがサンプルレートとして解釈できなかった(0 等)。
        /// <para>
        /// M1 では「48kHz 以外」がすべてこのコードだったが、M2-4 でロード時のリサンプル
        /// (rubato)が入ったため、44.1kHz などは変換されて成功するようになっている
        /// (初期構築仕様 §4.7)。
        /// </para>
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

        /// <summary>
        /// <see cref="MwNative.SetMusicLoop"/> の区間が不正だった(<c>begin &gt;= end</c> で、
        /// かつループ解除(<c>begin == 0 &amp;&amp; end == 0</c>)でもない)。
        /// ネイティブ側の楽曲ボイスは同じ状況を音声スレッドで黙ってループ無しへ丸めるが
        /// (リアルタイム安全性のためパニックできない設計)、FFI 境界では黙って捨てず
        /// 明示的に拒否する(初期構築仕様『§4.3 楽曲再生』)。
        /// </summary>
        ErrInvalidLoopRegion = -13,
    }

    /// <summary>
    /// <see cref="MwNative.LoadSound"/> の <c>mode</c> 引数(初期構築仕様 §5.2, §5.5)。
    /// <see cref="Se"/> は全デコード常駐(M1)、<see cref="Music"/> は圧縮のまま保持して
    /// ストリーミングデコード(M2-7)。**保持のしかたが違うので ID 空間も分かれている**。
    /// </summary>
    public enum SoundMode
    {
        /// <summary>効果音。ロード時に全デコードしてメモリ常駐させる(初期構築仕様 §4.2)。</summary>
        Se = 0,

        /// <summary>
        /// 楽曲。デコードせず圧縮バイト列のまま保持し、再生時にストリーミングデコードする
        /// (初期構築仕様『§5.5』。M2-7 で実装済み)。
        /// <para>
        /// ロード時点ではフォーマットの妥当性検証を行わない(デコーダを開くのは
        /// <see cref="MwNative.SetMusic"/> の中)。壊れたバイト列は
        /// <see cref="MwNative.SetMusic"/> が <see cref="MwResult.ErrDecodeFailed"/> を返す形で
        /// 検出される。
        /// </para>
        /// <para>
        /// SE とは ID 空間が分離されているため、ここで得た ID を
        /// <see cref="MwNative.PlaySe"/> に渡すと <see cref="MwResult.ErrInvalidSoundId"/> になる
        /// (逆も同様)。
        /// </para>
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
    /// 楽曲ボイスの再生状態(初期構築仕様『§4.3 楽曲再生』)。
    /// 生成コード側の <c>MwMusicState</c> は internal のため、公開する値はこの列挙を経由する
    /// (<see cref="MwResult"/> と同じ流儀)。判別子はネイティブ側と1:1で同期させること。
    /// </summary>
    public enum MusicState
    {
        /// <summary>
        /// 楽曲を差し替えた直後。プリロールがまだ済んでいない。
        /// <see cref="MwNative.SetMusic"/> は完了を待たずに戻るため、呼び出し側は
        /// <see cref="Ready"/> になるまで <see cref="MwNative.GetMusicState"/> をポーリングする。
        /// </summary>
        Loading = 0,

        /// <summary>プリロール完了。<see cref="MwNative.MusicPlayScheduled"/> で発音予約できる。</summary>
        Ready = 1,

        /// <summary>再生中。</summary>
        Playing = 2,

        /// <summary>一時停止中(位置は保持されている)。</summary>
        Paused = 3,
    }

    /// <summary>
    /// <see cref="MwNative.PollEvents"/> が返すイベント種別(初期構築仕様『§4.6 イベント通知』)。
    /// </summary>
    public enum EventKind
    {
        /// <summary>
        /// 出力ルートが変化した(イヤホン抜け / BT 切替)。
        /// <b>iOS/tvOS では M3 で発火するようになっている</b>(reason を問わずルート変化の
        /// たびに1回、付随データ無し)。Android では未配線(値だけ予約されている)。
        /// </summary>
        RouteChanged = 0,

        /// <summary>アンダーラン発生。付随データは集約されたアンダーランフレーム数。</summary>
        Underrun = 1,

        /// <summary>楽曲が最後まで再生された。付随データ無し(常に 0)。</summary>
        MusicEnded = 2,

        /// <summary>ループ区間で折り返した。付随データは折り返し先(曲頭からのフレーム位置)。</summary>
        MusicLooped = 3,

        /// <summary>
        /// ストリーミングデコードが失敗した。付随データは理由コード
        /// (<see cref="Mw.Native.StreamErrorReason"/> の判別子を <c>ulong</c> 化したもの)。
        /// </summary>
        StreamError = 4,

        /// <summary>
        /// ソフトクリッパが作動した(音が歪む水準に達した)。付随データ無し(常に 0)。
        /// **開発ビルドのみ発火**する。
        /// </summary>
        ClipperEngaged = 5,

        /// <summary>
        /// OS 主導のオーディオ割り込みが始まった(M3, iOS/tvOS 専用)。電話着信・Siri・
        /// 他アプリの音声に加え、Background Audio を持たないアプリがバックグラウンドへ
        /// 遷移した場合もここに含まれる。付随データ無し(常に 0)。この時点で出力
        /// ストリームは(OS 側の都合で)鳴らなくなっている可能性が高い。
        /// </summary>
        AudioInterruptionBegan = 6,

        /// <summary>
        /// ストリームレベルの復帰を試みた結果(M3)。付随データはネイティブ側の
        /// <c>bool</c>(0 or 1)を <c>ulong</c> 化したもので、<c>payload != 0</c> なら
        /// 復帰(音が戻った)、<c>0</c> なら復帰していない(この先自動では戻らない)。
        /// <para>
        /// <b>発生源はプラットフォームによって異なるが、意味は共通</b>: iOS/tvOS では
        /// OS 主導の割り込み(<see cref="AudioInterruptionBegan"/>)が終わった、または
        /// ミドルウェアが独自に復帰を試みた結果。それ以外のプラットフォームでは
        /// Android(AAudio)切断からのミドルウェア内部再オープンが成功した、または
        /// バックオフを使い切って断念した結果(この場合 <see cref="RouteChanged"/> ではなく
        /// <see cref="StreamError"/> が切断そのものの通知を兼ねる)。ビルドごとに
        /// どちらか一方の経路しか発火しないため、受信側がどちらが発生源かを気にする
        /// 必要は無い——<c>payload == 0</c>(復帰していない)を見たら、呼び出し側で
        /// 「音が戻っていない」表示を出す、といった対応で両方に共通して対処できる。
        /// </para>
        /// </summary>
        AudioInterruptionEnded = 7,
    }

    /// <summary>
    /// ストリーミングデコード失敗の理由(<see cref="EventKind.StreamError"/> の付随データ、
    /// 初期構築仕様『§4.6 イベント通知』)。ネイティブ側 <c>mw_core::StreamErrorReason</c>
    /// (<c>#[repr(u32)]</c>)に対応する公開用の列挙。
    /// <para>
    /// 特定のオーディオバックエンドの詳細な分類をそのまま持ち込まず、呼び出し側が
    /// 実用的に分岐できる粒度へ意図的に丸めてある(詳細な原因は開発ビルドのネイティブ
    /// ログに残る)。
    /// </para>
    /// <para>
    /// <b>この型は csbindgen の入力に含まれない</b>——ネイティブ側の定義は
    /// <c>mw-core</c>(<c>mw-ffi</c> ではなく)にあり、かつ <c>extern "C"</c> 関数の
    /// シグネチャにもどの構造体のフィールド型にも直接現れない(<c>u64</c> へ変換済みの
    /// 値としてしか FFI 境界を越えない)ため、自動生成の対象にならない
    /// (<see cref="MusicState"/> が生成される仕掛けの逆のケース)。**値はネイティブ側と
    /// 1:1 で手動同期させること**——`cargo test` の C# ⇔ Rust 突き合わせテスト
    /// (`crates/mw-ffi/src/csharp_abi_sync.rs`)がズレを検出する。
    /// </para>
    /// <para>
    /// <see cref="MwEventData.Payload"/> を <c>(StreamErrorReason)(int)payload</c> として
    /// 解釈する。
    /// </para>
    /// </summary>
    public enum StreamErrorReason
    {
        /// <summary>上記以外、またはバックエンド固有で分類しきれないエラー。</summary>
        Unknown = 0,

        /// <summary>出力デバイス/ホストに到達できない(切断・使用中・ホスト不在)。</summary>
        DeviceUnavailable = 1,

        /// <summary>ルート変化等でストリーム構成が無効になり、再構築が必要。</summary>
        Reconfigured = 2,

        /// <summary>OS がデバイスへのアクセスを拒否した。</summary>
        PermissionDenied = 3,

        /// <summary>上記以外のバックエンド内部エラー。</summary>
        Backend = 4,
    }

    /// <summary>
    /// <see cref="MwNative.PollEvents"/> が呼び出し側のバッファへ書き込む1件分のイベント
    /// (初期構築仕様『§4.6 イベント通知』)。
    /// <para>
    /// <b>この構造体はネイティブ側 <c>MwEvent</c> とレイアウトが一致していなければならない。</b>
    /// <see cref="MwNative.PollEvents"/> はネイティブ側に呼び出し側の配列へ直接書き込ませる
    /// (GC アロケーションゼロ。初期構築仕様『§5.4』)ため、要素ごとの詰め替えを行わない。
    /// フィールドの順序・型を変えてはならない。一致は EditMode テストで固定してある。
    /// </para>
    /// </summary>
    [StructLayout(LayoutKind.Sequential)]
    public struct MwEventData
    {
        /// <summary>イベント種別。</summary>
        public EventKind Kind;

        /// <summary>
        /// 種別ごとに意味の異なる付随データ(いずれも 64bit 符号無し整数1個に収まるため
        /// 1本にまとめてある)。意味は <see cref="EventKind"/> の各値のドキュメントを参照。
        /// </summary>
        public ulong Payload;
    }

    /// <summary>
    /// 音楽クロックのスナップショット(初期構築仕様『§4.4 音楽クロック』)。
    /// <para>
    /// <see cref="MwEventData"/> と違い、こちらはネイティブ側 <c>MwMusicPosition</c> と
    /// レイアウトが一致していない(あちらは C# の <c>bool</c> マーシャリング幅の問題を避けるため
    /// <c>is_playing</c> を <c>u8</c> で持つ)。<see cref="MwNative.GetMusicPosition"/> が
    /// スタック上の一時変数を経由して詰め替える —— 「<c>u8</c> の bool 解釈は薄いラッパの責務」
    /// というネイティブ側の設計意図どおりの分担で、詰め替えはスタック上で完結するため
    /// GC アロケーションは発生しない。
    /// </para>
    /// <para>
    /// <b><see cref="Generation"/> を跨いだ外挿・補間をしてはならない。</b>
    /// シーク・停止・巻き戻し再開・ループ折り返しのたびに 1 増える。
    /// </para>
    /// </summary>
    public struct MusicPosition
    {
        /// <summary>楽曲ボイスをこれまでにレンダリングしたフレーム数(= 曲位置)。</summary>
        public ulong SongFrames;

        /// <summary>
        /// <see cref="SongFrames"/> に対応するホスト単調時刻(ナノ秒)。
        /// <see cref="MwNative.HostTimeNs"/> と同じ時計。
        /// </summary>
        public ulong HostTimeNs;

        /// <summary>出力サンプルレート [Hz]。<b>0 は「まだ確定していない」</b>(ゼロ除算に注意)。</summary>
        public uint SampleRate;

        /// <summary>楽曲ボイスの再生状態。</summary>
        public MusicState State;

        /// <summary>楽曲が進行中か。<see cref="State"/> が <see cref="MusicState.Playing"/> であることと等価。</summary>
        public bool IsPlaying;

        /// <summary>
        /// 世代カウンタ。不連続(シーク・停止・巻き戻し再開・ループ折り返し)のたびに 1 増える。
        /// </summary>
        public uint Generation;

        /// <summary>
        /// 曲頭からの経過秒。<see cref="SampleRate"/> が 0(未確定)のときは 0 を返す。
        /// </summary>
        public double SongSeconds => SampleRate == 0 ? 0.0 : (double)SongFrames / SampleRate;
    }

    /// <summary>
    /// 出力コールバックのアンダーラン(の疑い)統計(初期構築仕様『§2』M3
    /// 「アンダーラン検知・テレメトリ」)。
    /// <para>
    /// <see cref="EventKind.Underrun"/>(<see cref="MwNative.PollEvents"/> 経由)とは別物。
    /// あちらは楽曲/BGM のデコードリングバッファ側のアンダーラン、こちらは音声コールバック
    /// 自体の間隔異常(OS 側出力バッファの枯渇の兆候)。
    /// </para>
    /// <para>
    /// <see cref="MusicPosition"/> と同じ理由でネイティブ側 <c>MwOutputUnderrunStats</c> と
    /// レイアウトを一致させていない(<see cref="MwNative.GetOutputUnderrunStats"/> が
    /// スタック上の一時変数を経由して詰め替える)。
    /// </para>
    /// </summary>
    public struct OutputUnderrunStats
    {
        /// <summary>累計検知回数。</summary>
        public ulong Count;

        /// <summary>
        /// 直近に検知したコールバックのホスト単調時刻(ナノ秒)。
        /// <see cref="MwNative.HostTimeNs"/> と同じ時計。まだ1度も検知していなければ 0。
        /// </summary>
        public ulong LastHostTimeNs;

        /// <summary>
        /// 直近まで連続して検知した回数。検知されなかったコールバックが1回でも
        /// 挟まると 0 に戻る。
        /// </summary>
        public uint ConsecutiveCount;
    }

    /// <summary>
    /// open-audioware のネイティブ層への薄いラッパ。
    /// csbindgen が生成した <c>Mw.Native.Generated.NativeMethods</c>(internal)を直接
    /// 呼ばず、必ずこのクラスを経由すること(初期構築仕様 §5.4)。
    /// <para>
    /// 公開している関数はすべてゲームスレッドから呼ぶ想定で、内部ではコマンドを
    /// ロックフリーキューへ積むだけの非ブロッキング呼び出し(初期構築仕様 §5.2)。
    /// 実際の発音・停止・音量変化・楽曲の状態遷移は次のオーディオコールバックで処理される。
    /// <b>したがって「呼んだ直後に状態が変わっている」ことを期待してはならない</b> ——
    /// 楽曲の準備完了は <see cref="GetMusicState"/> のポーリングで待つ。
    /// </para>
    /// <para>
    /// 例外は値を読むだけの <see cref="AbiVersion"/> / <see cref="HostTimeNs"/> /
    /// <see cref="GetMusicPosition"/> / <see cref="GetMusicState"/> /
    /// <see cref="GetOutputLatencyNs"/> / <see cref="PollEvents"/> で、これらは
    /// コマンドを積まずその場で値を返す。
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
        /// 音源のバイト列をロードする(初期構築仕様 §5.5, §4.2)。
        /// <para>
        /// <see cref="SoundMode.Se"/>: wav を全デコードしてメモリ常駐させる。対応フォーマットは
        /// 48kHz / 16bit PCM / モノラルまたはステレオの wav のみ。
        /// </para>
        /// <para>
        /// <see cref="SoundMode.Music"/>: デコードせず圧縮バイト列のまま保持する(M2-7)。
        /// ここではフォーマットを検証しないため、壊れたデータでも成功しうる
        /// (デコーダを開くのは <see cref="SetMusic"/>)。
        /// </para>
        /// </summary>
        /// <param name="handle"><see cref="Init"/> が返したハンドル。</param>
        /// <param name="bytes">音源ファイルの生バイト列。</param>
        /// <param name="mode">SE(全デコード常駐)か楽曲(圧縮のまま保持)か。</param>
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

        /// <summary>
        /// SE を<b>サンプル精度で予約発音</b>する(初期構築仕様『§4.5 スケジュール発音』, M2-5)。
        /// <para>
        /// 用途はメトロノームとキャリブレーション用クリック。該当バッファのレンダリング時に
        /// <b>バッファ内オフセットのサンプル位置</b>から発音する(バッファ境界へ丸めない)ため、
        /// <see cref="PlaySe"/> の「次のコールバックで鳴る」より細かく置ける。
        /// </para>
        /// <para>
        /// <paramref name="hostTimeNs"/> は <see cref="HostTimeNs"/> と<b>同じ時計</b>の値を渡すこと。
        /// Unity 側の <c>AudioSettings.dspTime</c> とは別の時計なので、混ぜると鳴る位置がずれる。
        /// </para>
        /// <para>
        /// 予約時刻が既に過去だった場合は取りこぼさず、そのバッファの先頭で即座に発音する。
        /// また予約キューは固定容量で、<b>この呼び出しが成功しても音声スレッド側で満杯だった予約は
        /// 発音されない</b>(<c>se_schedule_overflow_count</c> で検知できる)。
        /// </para>
        /// </summary>
        /// <param name="handle"><see cref="Init"/> が返したハンドル。</param>
        /// <param name="id"><see cref="LoadSound"/> が返したサウンド ID(<see cref="SoundMode.Se"/>)。</param>
        /// <param name="bus">再生先バス。</param>
        /// <param name="volume">再生音量(linear)。</param>
        /// <param name="hostTimeNs">発音時刻(<see cref="HostTimeNs"/> と同じ時計)。</param>
        /// <param name="voice">成功時、以後 <see cref="VoiceStop"/> / <see cref="VoiceSetVolume"/> に渡す不透明 ID。</param>
        public static unsafe MwResult ScheduleSe(ulong handle, ulong id, Bus bus, float volume, ulong hostTimeNs, out ulong voice)
        {
            ulong outVoice = 0;
            Generated.MwResult native = NativeMethods.mw_se_schedule(handle, id, (int)bus, volume, hostTimeNs, &outVoice);
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
        /// バスの直近設定音量を取得する(R38「ツールバー連打で無音化」調査用に追加、
        /// 2026-08-31)。
        /// <para>
        /// <see cref="BusSetVolume"/>/<see cref="BusFade"/> に渡した最後の目標値をそのまま
        /// 返す。音声スレッドのランプがまだ収束していなくても、ここで返るのは
        /// 「最終的にどこへ向かっているか」の値であり、ランプ中の瞬間値ではない。
        /// 「SE/BGM/マスターのどれかが意図せず 0 になっていないか」を確認する
        /// 診断用途にはこれで十分。コマンドを発行しないロックフリー読み出しのため、
        /// ゲームスレッドから任意の頻度で呼んでよい。
        /// </para>
        /// </summary>
        public static unsafe MwResult BusGetVolume(ulong handle, Bus bus, out float volume)
        {
            float v = 0f;
            Generated.MwResult native = NativeMethods.mw_bus_get_volume(handle, (int)bus, &v);
            volume = v;
            return ToPublicResult(native);
        }

        // --- 楽曲・クロック(M2)------------------------------------------------

        /// <summary>
        /// ホスト単調時刻をナノ秒で取得する(初期構築仕様『§4.4 音楽クロック』)。
        /// <para>
        /// 呼び出し側は起動時にこの値と Unity 側の時刻(<c>Time.realtimeSinceStartupAsDouble</c> 等)を
        /// 1回ずつサンプリングし、その差を定数オフセットとして保持することで両者を橋渡しする。
        /// <see cref="MusicPosition.HostTimeNs"/> および <see cref="MusicPlayScheduled"/> の
        /// 引数はすべてこの時計の値。
        /// </para>
        /// <para>ハンドル不要(<see cref="Init"/> 前でも呼べる)。</para>
        /// </summary>
        public static ulong HostTimeNs()
        {
            return NativeMethods.mw_host_time_ns();
        }

        /// <summary>
        /// 出力レイテンシ(ns)の実測値を取得する。
        /// <para>
        /// <b>0 は「まだ不明」を意味する</b>(オーディオコールバックが1度も走っていない)。
        /// </para>
        /// <para>
        /// これは「バッファ先頭がデバイスへ届くまで」の値であり、タップから音が出るまでの
        /// end-to-end レイテンシではない。オーディオオフセットの初期推定に使えるが、
        /// 実測キャリブレーションの代わりにはならない。
        /// </para>
        /// </summary>
        public static unsafe MwResult GetOutputLatencyNs(ulong handle, out ulong nanoseconds)
        {
            ulong ns = 0;
            Generated.MwResult native = NativeMethods.mw_get_output_latency_ns(handle, &ns);
            nanoseconds = ns;
            return ToPublicResult(native);
        }

        /// <summary>
        /// 出力コールバックのアンダーラン(の疑い)統計を取得する(初期構築仕様『§2』M3
        /// 「アンダーラン検知・テレメトリ」)。<see cref="OutputUnderrunStats"/> のドキュメント
        /// 「<see cref="EventKind.Underrun"/> とは別物」を参照。
        /// </summary>
        public static unsafe MwResult GetOutputUnderrunStats(ulong handle, out OutputUnderrunStats stats)
        {
            Generated.MwOutputUnderrunStats native = default;
            Generated.MwResult result = NativeMethods.mw_get_output_underrun_stats(handle, &native);
            stats = new OutputUnderrunStats
            {
                Count = native.count,
                LastHostTimeNs = native.last_host_time_ns,
                ConsecutiveCount = native.consecutive_count,
            };
            return ToPublicResult(result);
        }

        /// <summary>
        /// 楽曲のストリーミング再生を準備する(初期構築仕様『§4.3 楽曲再生』)。
        /// <para>
        /// <paramref name="soundId"/> は <see cref="LoadSound"/> に
        /// <see cref="SoundMode.Music"/> を渡して得た ID であること(SE の ID を渡すと
        /// <see cref="MwResult.ErrInvalidSoundId"/>)。デコーダはこの呼び出しの中で開くため、
        /// 壊れたバイト列はここで <see cref="MwResult.ErrDecodeFailed"/> として現れる。
        /// </para>
        /// <para>
        /// <b>非ブロッキング。</b>プリロールの完了を待たずに戻り、状態は
        /// <see cref="MusicState.Loading"/> のまま。<see cref="GetMusicState"/> が
        /// <see cref="MusicState.Ready"/> を返すまでポーリングしてから
        /// <see cref="MusicPlayScheduled"/> を呼ぶこと。
        /// </para>
        /// <para>
        /// 前の曲が再生中でも、そのまま呼んでよい(前曲の PCM がリングバッファに残って
        /// 曲頭に漏れないよう、ネイティブ側が決まった順序で仕切り直す)。
        /// </para>
        /// </summary>
        public static MwResult SetMusic(ulong handle, ulong soundId)
        {
            Generated.MwResult native = NativeMethods.mw_music_set(handle, soundId);
            return ToPublicResult(native);
        }

        /// <summary>
        /// 楽曲を予約再生する(初期構築仕様『§4.3 楽曲再生』)。
        /// <paramref name="hostTimeNs"/> は <see cref="HostTimeNs"/> と同じ時計の値を渡すこと。
        /// <para>
        /// プリロール完了前に予約時刻が到来した場合はエラーにせず「準備完了後、可能な最速時刻」へ
        /// 繰り下げる【仮】。繰り下げの発生は現時点では C# 側から観測できない。
        /// </para>
        /// </summary>
        public static MwResult MusicPlayScheduled(ulong handle, ulong hostTimeNs)
        {
            Generated.MwResult native = NativeMethods.mw_music_play_scheduled(handle, hostTimeNs);
            return ToPublicResult(native);
        }

        /// <summary>
        /// 楽曲ボイスの再生状態を取得する(初期構築仕様『§4.3 楽曲再生』)。
        /// <see cref="SetMusic"/> の後、<see cref="MusicState.Ready"/> になるまでポーリングする。
        /// <para>
        /// 状態だけでなく曲位置も要るなら <see cref="GetMusicPosition"/> を使う
        /// (1回の呼び出しで整合の取れたスナップショットが得られる)。
        /// </para>
        /// </summary>
        public static unsafe MwResult GetMusicState(ulong handle, out MusicState state)
        {
            int raw = 0;
            Generated.MwResult native = NativeMethods.mw_music_state(handle, &raw);
            state = (MusicState)raw;
            return ToPublicResult(native);
        }

        /// <summary>
        /// 楽曲を一時停止する。既定ランプでフェードアウトしてから
        /// <see cref="MusicState.Paused"/> へ収束する。
        /// <para>
        /// <see cref="MusicState.Playing"/> 以外からの呼び出しは音声スレッド側で無視される
        /// (エラーにはならない)。
        /// </para>
        /// </summary>
        public static MwResult MusicPause(ulong handle)
        {
            Generated.MwResult native = NativeMethods.mw_music_pause(handle);
            return ToPublicResult(native);
        }

        /// <summary>
        /// 巻き戻し付きで再開する(中断復帰の「数秒巻き戻し + カウントダウン再開」の受け皿)。
        /// <paramref name="frames"/> へ再位置決めしたうえで既定ランプでフェードインする。
        /// <see cref="MusicState.Loading"/> 中は音声スレッド側で無視される。
        /// </summary>
        public static MwResult MusicResumeAt(ulong handle, ulong frames)
        {
            Generated.MwResult native = NativeMethods.mw_music_resume_at(handle, frames);
            return ToPublicResult(native);
        }

        /// <summary>
        /// 楽曲ボイスをシークする。ランプを経由しない不連続そのもので、
        /// <see cref="MusicPosition.Generation"/> が進む。
        /// </summary>
        public static MwResult MusicSeek(ulong handle, ulong frames)
        {
            Generated.MwResult native = NativeMethods.mw_music_seek(handle, frames);
            return ToPublicResult(native);
        }

        /// <summary>
        /// 楽曲を停止する。<see cref="MusicState.Playing"/> 中は既定ランプでフェードアウトして
        /// から位置を 0 に戻し <see cref="MusicState.Ready"/> へ、<see cref="MusicState.Paused"/>
        /// 中はランプ無しでその場で <see cref="MusicState.Ready"/> へ戻る。
        /// </summary>
        public static MwResult MusicStop(ulong handle)
        {
            Generated.MwResult native = NativeMethods.mw_music_stop(handle);
            return ToPublicResult(native);
        }

        /// <summary>
        /// 楽曲のループ区間を設定する(選曲プレビュー用。フェードイン / アウト付き)。
        /// <para>
        /// 解除は <see cref="ClearMusicLoop"/> を使うこと。
        /// <paramref name="beginFrames"/> が <paramref name="endFrames"/> 以上の場合は
        /// <see cref="MwResult.ErrInvalidLoopRegion"/>(解除を意味する 0/0 を除く)。
        /// </para>
        /// </summary>
        public static MwResult SetMusicLoop(ulong handle, ulong beginFrames, ulong endFrames)
        {
            Generated.MwResult native = NativeMethods.mw_music_set_loop(handle, beginFrames, endFrames);
            return ToPublicResult(native);
        }

        /// <summary>
        /// ループ区間を解除する。ネイティブ側は「区間 0/0」を解除の合図として解釈する【仮】ため、
        /// 呼び出し側にその規約を意識させないよう専用のメソッドにしてある。
        /// </summary>
        public static MwResult ClearMusicLoop(ulong handle)
        {
            return SetMusicLoop(handle, 0, 0);
        }

        // --- BGM(M4-3)---------------------------------------------------------
        //
        // 初期構築仕様『§2』M14: 楽曲ボイスは同時に1本のみという原則を守ったまま、
        // BGM 用の2本目の楽曲ボイスを追加したもの。楽曲(上のセクション)との違い:
        // <b>クロックを持たない</b>(<see cref="GetMusicPosition"/> 相当の関数が無い。
        // 発行元は楽曲ボイスに固定)。ループ再生とフェードだけを扱う——サンプル精度の
        // 予約再生・巻き戻し付き再開も無い。バス音量は楽曲と<b>同じ Bgm バス</b>を
        // 共有する(<see cref="BusSetVolume"/>/<see cref="BusFade"/> に
        // <see cref="Bus.Bgm"/> を渡せばよく、BGM 専用のバス操作 API は無い)。

        /// <summary>
        /// BGM のストリーミング再生を準備する(<see cref="SetMusic"/> の BGM 版)。
        /// <para>
        /// <paramref name="soundId"/> は <see cref="LoadSound"/> に
        /// <see cref="SoundMode.Music"/> を渡して得た ID であること——楽曲・BGM は
        /// 圧縮バイト列のストレージと ID 空間を共有する(SE の ID を渡すと
        /// <see cref="MwResult.ErrInvalidSoundId"/>)。
        /// </para>
        /// <para>
        /// <b>非ブロッキング。</b><see cref="GetBgmState"/> が <see cref="MusicState.Ready"/>
        /// を返すまでポーリングしてから <see cref="PlayBgm"/> を呼ぶこと。
        /// </para>
        /// </summary>
        public static MwResult SetBgm(ulong handle, ulong soundId)
        {
            Generated.MwResult native = NativeMethods.mw_bgm_set(handle, soundId);
            return ToPublicResult(native);
        }

        /// <summary>
        /// BGM ボイスの再生状態を取得する(<see cref="GetMusicState"/> の BGM 版)。
        /// <see cref="SetBgm"/> の後、<see cref="MusicState.Ready"/> になるまでポーリングする。
        /// <b>曲位置は取得できない</b>(BGM はクロックを持たない。初期構築仕様『§2』M14)。
        /// </summary>
        public static unsafe MwResult GetBgmState(ulong handle, out MusicState state)
        {
            int raw = 0;
            Generated.MwResult native = NativeMethods.mw_bgm_state(handle, &raw);
            state = (MusicState)raw;
            return ToPublicResult(native);
        }

        /// <summary>
        /// BGM を再生する。<see cref="MusicState.Ready"/> からのみ有効で、既定ランプで
        /// フェードインする(初期構築仕様『§2』M14「フェード」)。
        /// <para>
        /// 楽曲ボイス(<see cref="MusicPlayScheduled"/>)と同時に再生してもよい——
        /// 両者は同じ Bgm バスで単純に加算されるため、片方をフェードアウトしつつ
        /// もう片方をフェードインする<b>画面遷移をまたぐクロスフェード</b>がこれだけで成立する。
        /// </para>
        /// </summary>
        public static MwResult PlayBgm(ulong handle)
        {
            Generated.MwResult native = NativeMethods.mw_bgm_play(handle);
            return ToPublicResult(native);
        }

        /// <summary>
        /// BGM を停止する(<see cref="MusicStop"/> の BGM 版)。<see cref="MusicState.Playing"/>
        /// 中は既定ランプでフェードアウトしてから位置を 0 に戻し <see cref="MusicState.Ready"/> へ。
        /// </summary>
        public static MwResult StopBgm(ulong handle)
        {
            Generated.MwResult native = NativeMethods.mw_bgm_stop(handle);
            return ToPublicResult(native);
        }

        /// <summary>
        /// BGM のループ区間を設定する(<see cref="SetMusicLoop"/> の BGM 版)。
        /// <para>
        /// トラック全体をループさせたい場合は、呼び出し側が把握している総フレーム数を使って
        /// <c>(0, totalFrames)</c> を渡すこと——ネイティブ側は総フレーム数を問い合わせる
        /// API を持たない(素材メタデータ等から呼び出し側が把握している前提。
        /// <see cref="SetMusicLoop"/> と同じ設計)。
        /// </para>
        /// <para>解除は <see cref="ClearBgmLoop"/> を使うこと。</para>
        /// </summary>
        public static MwResult SetBgmLoop(ulong handle, ulong beginFrames, ulong endFrames)
        {
            Generated.MwResult native = NativeMethods.mw_bgm_set_loop(handle, beginFrames, endFrames);
            return ToPublicResult(native);
        }

        /// <summary>BGM のループ区間を解除する(<see cref="ClearMusicLoop"/> の BGM 版)。</summary>
        public static MwResult ClearBgmLoop(ulong handle)
        {
            return SetBgmLoop(handle, 0, 0);
        }

        /// <summary>
        /// 音楽クロックのスナップショットを取得する(初期構築仕様『§4.4 音楽クロック』)。
        /// <b>毎フレーム呼ぶ想定の関数で、GC アロケーションは発生しない</b>
        /// (詰め替えはスタック上で完結する。<see cref="MusicPosition"/> のドキュメント参照)。
        /// </summary>
        public static unsafe MwResult GetMusicPosition(ulong handle, out MusicPosition position)
        {
            Generated.MwMusicPosition native = default;
            Generated.MwResult result = NativeMethods.mw_music_get_position(handle, &native);
            position = new MusicPosition
            {
                SongFrames = native.song_frames,
                HostTimeNs = native.host_time_ns,
                SampleRate = native.sample_rate,
                State = (MusicState)(int)native.state,
                IsPlaying = native.is_playing != 0,
                Generation = native.generation,
            };
            return ToPublicResult(result);
        }

        /// <summary>
        /// 溜まっているイベントを呼び出し側のバッファへ取り出す(初期構築仕様『§4.6 イベント通知』)。
        /// C → C# のコールバックはしない設計のため、毎フレームこれを呼ぶ。
        /// <para>
        /// <b>GC アロケーションゼロ</b>: ネイティブ側が <paramref name="buffer"/> へ直接書き込む。
        /// バッファは呼び出し側で使い回すこと(毎フレーム確保しない)。
        /// </para>
        /// <para>
        /// <paramref name="dropped"/> には、キューが固定容量(【仮】64)を超えて溢れたために
        /// <b>今回の呼び出しで新たに判明した</b>破棄件数が入る(前回までに報告済みの分は含まない)。
        /// 0 以外なら取りこぼしが起きているので、ポーリング頻度かバッファ長を見直す。
        /// </para>
        /// </summary>
        /// <param name="handle"><see cref="Init"/> が返したハンドル。</param>
        /// <param name="buffer">書き込み先。null / 長さ 0 でもよい(その場合は取り出さない)。</param>
        /// <param name="count">実際に書き込まれた件数。失敗時は 0。</param>
        /// <param name="dropped">今回新たに判明した破棄件数。</param>
        public static unsafe MwResult PollEvents(ulong handle, MwEventData[] buffer, out int count, out uint dropped)
        {
            count = 0;
            dropped = 0;

            int capacity = buffer == null ? 0 : buffer.Length;
            uint droppedRaw = 0;
            int written;

            if (capacity == 0)
            {
                // cap <= 0 のときネイティブ側は buf を触らない(mw_sound_load の len == 0 と同じ流儀)。
                written = NativeMethods.mw_poll_events(handle, null, 0, &droppedRaw);
            }
            else
            {
                // MwEventData はネイティブ側 MwEvent とレイアウトが一致している前提で
                // 再解釈キャストする(要素ごとの詰め替えをしない = アロケーションも走査も無い)。
                // 一致は EditMode テストで固定してある(MwEventData のドキュメント参照)。
                fixed (MwEventData* p = buffer)
                {
                    written = NativeMethods.mw_poll_events(handle, (Generated.MwEvent*)p, capacity, &droppedRaw);
                }
            }

            // この関数だけ戻り値の意味が他と違う(成功時は件数、失敗時のみ負のエラーコード)。
            // 呼び出し側にその例外を持ち込ませないよう、ここで件数と MwResult に分離する。
            if (written < 0)
            {
                return ToPublicResult((Generated.MwResult)written);
            }

            count = written;
            dropped = droppedRaw;
            return MwResult.Ok;
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
