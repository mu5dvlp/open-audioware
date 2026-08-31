# mw-ffi

C ABI 境界。`mw-core` / `mw-backend` 両方に依存する唯一のクレート(依存の向きは
`mw-ffi → mw-core / mw-backend`。ワークスペース全体の方針は `../../CLAUDE.md` を参照)。
`crate-type = ["cdylib", "staticlib"]` — cdylib は macOS/Android、staticlib は iOS
(xcframework 経由)で使う。

## ファイル構成

- `src/result.rs` — `MwResult`(`#[repr(i32)]`)。全 FFI 関数の戻り値。`Ok = 0`、
  それ以外は負の整数のエラーコード(初期構築仕様 §4.8)。
- `src/types.rs` — `MwSoundMode` / `MwBus`(FFI 境界の `i32` 引数を検証・復元する
  内部専用の値型、`from_raw(i32) -> Option<Self>`)、`MwMusicState`
  (`mw_music_state()` の実体。M2-7)、`MwMusicPosition`(`#[repr(C)]`、blittable。
  `mw_music_get_position` の out 引数。M2-7)。**csbindgen の入力**(M2-7 で追加、
  `build.rs` 参照)——`MwSoundMode`/`MwBus` はどの extern 関数シグネチャにも
  直接現れないため相変わらず C# 側は生成されないが、`MwMusicPosition` が
  `mw_music_get_position` の実引数型として現れ、そのフィールド `state:
  MwMusicState` を辿って `MwMusicState` も生成される(`event.rs::MwEvent`/
  `MwEventKind` と同じ仕掛け。詳細は `types.rs` の `MwMusicState` ドキュメント参照)。
  M3 で `MwOutputUnderrunStats`(`#[repr(C)]`、blittable。`mw_get_output_underrun_stats`
  の out 引数)を追加した——`MwMusicPosition` と同じ設計方針(count/last_host_time_ns/
  consecutive_count の3値をひとまとめに返す)。
- `src/handle.rs` — init/shutdown のグローバルレジストリ(`OnceLock<Mutex<Option<Instance>>>`)。
  ハンドルは不透明な `u64`(ポインタを C# に渡さない)。二重 init は同一ハンドルを返す
  (冪等)、無効ハンドルの shutdown はエラーコードで検出する。`Instance` は M1 で
  `mw_core::CommandSender` / `mw_core::ReclaimReceiver` / `mw_core::SoundStorage` /
  ボイスシリアル採番器(`AtomicU64`)を持つ。M2-6 で `Arc<mw_core::EventQueue>`
  (`events` フィールド)を追加した——`mw_poll_events` がここから `drain` し、
  `CpalBackend::open` にも同じ `Arc` を渡してストリームのエラー通知経路
  (非リアルタイムスレッド)から `push_side_channel` させる。M2-7 で楽曲再生
  (`Arc<mw_core::MusicClockPublisher>`、楽曲バイト列ストレージ `music_bytes:
  Mutex<HashMap<u64, Arc<Vec<u8>>>>`、デコードスレッドの送信ハンドル・停止フラグ・
  join ハンドル)を追加した。**楽曲 ID は最上位ビット(`MUSIC_ID_FLAG`)を立てて
  SE の ID(`mw_core::SoundStorage` 採番)と空間を分離してある**(`is_music_id`
  で判別。理由は `MUSIC_ID_FLAG` のドキュメント参照)。M4-3 で BGM 用に
  `Arc<mw_core::BgmStatePublisher>`(`bgm_state` フィールド。`mw_bgm_state` が
  ここからロック無しで読む)と、BGM 専用のもう1本のデコードスレッド一式
  (`bgm_decoder_tx`/`bgm_decode_thread_stop`/`bgm_decode_thread`)を追加した。
  **`music_bytes` ストレージと ID 空間は楽曲と共有する**——`mw_bgm_set` は同じ
  `get_music_bytes` を呼ぶだけで、専用のストレージ・ID 空間を新設していない
  (「デコード前の圧縮バイト列を保持する」という表現そのものへの分離であり、
  どちらのボイスで鳴らすかとは無関係なため)。
- `src/decode_thread.rs` — 楽曲のデコードスレッド(M2-7)。`mw_core::stream` が
  意図的に持たないスレッドをここで1本立て、`mw_init`〜`mw_shutdown` の間
  生かし続ける。`mw_music_set` のたびに立て直すのではなく、
  `mpsc::Sender<Box<dyn mw_core::MusicDecoder + Send>>` 経由でデコーダを
  差し替える設計(理由・待ち方・ポーリング間隔・パニック安全性はモジュール doc
  参照)。**M4-3 で BGM 用にもう1本立てるようになった**——`spawn` 自体は
  「楽曲用」「BGM 用」を一切区別しない汎用実装なので、`handle::init` が
  独立した `MusicStreamProducer` を渡して2回呼ぶだけでそのまま使い回せている
  (`decode_thread.rs` 自体の変更は無し)。
- `src/event.rs` — `MwEvent`(`#[repr(C)]`、blittable)/ `MwEventKind`(`#[repr(i32)]`)。
  初期構築仕様『§4.6 イベント通知』の C ABI 表現(M2-6)。`mw_core::Event` からの
  変換(`MwEvent::from_core`)をここに置く。**csbindgen の入力**(`build.rs` が
  `ffi.rs`/`result.rs` と一緒にここも読む)——`MwEvent` は `mw_poll_events` の
  引数型として実際に extern 関数シグネチャに現れるため、C# 側の型を自動生成
  させる必要がある(`types.rs` が M2-7 で同じ仕組みに乗った経緯の先例)。
- `src/ffi.rs` — `#[unsafe(no_mangle)] pub extern "C" fn mw_*` 本体。
  **csbindgen の入力**(`build.rs` がここと `result.rs`/`event.rs`/`types.rs` を読む)。
- `build.rs` — csbindgen で `unity/Runtime/Generated/NativeMethods.g.cs` を生成する。

## 現状の公開 API(M1: SE 再生 / M2-5: ホスト時刻・予約発音 / M2-6: イベント通知 /
M2-7: 楽曲再生 / M4-3: BGM のネイティブ化)

```
mw_abi_version() -> u32                                          // 定数 1
mw_host_time_ns() -> u64                                          // ホスト単調時刻(ns)。ハンドル不要
mw_init(out_handle: *mut u64) -> MwResult                        // 冪等。既定出力デバイスにストリームを開く。デコードスレッドを1本立てる
mw_shutdown(handle: u64) -> MwResult                              // 冪等ではない(無効ハンドルはエラー)。ストリームを閉じ、デコードスレッドを止めて join する

mw_sound_load(handle, bytes: *const u8, len: usize, mode: i32, out_id: *mut u64) -> MwResult
    // mode=0(SE): wav を全デコードして常駐。mode=1(Music, M2-7): デコードせず圧縮バイト列のまま保持。
    // 発行される ID は mode によって空間が分離される(下記「楽曲 ID の空間分離」参照)
mw_sound_release(handle, id: u64) -> MwResult
    // id のタグを見て SE/楽曲を自動振り分け。SE は再生中ボイスがあれば既定ランプ経由で
    // 即停止させたうえで解放。楽曲は再生中でも拒否せず即座に解放する(理由は
    // handle.rs::Instance::remove_music_bytes のドキュメント参照)

mw_se_play(handle, id: u64, bus: i32, volume: f32, out_voice: *mut u64) -> MwResult
    // 次のオーディオコールバックで必ず発音される(初期構築仕様 §4.2)
mw_se_schedule(handle, id: u64, bus: i32, volume: f32, host_time_ns: u64, out_voice: *mut u64) -> MwResult
    // サンプル精度の予約発音(初期構築仕様 §4.5)。バッファ内オフセットへの丸めはしない。
    // 予約時刻が過去ならそのバッファの先頭で即座に発音する(取りこぼさない)
mw_voice_stop(handle, voice: u64) -> MwResult                     // 既定ランプ経由
mw_voice_set_volume(handle, voice: u64, volume: f32) -> MwResult  // 既定ランプ経由
mw_bus_set_volume(handle, bus: i32, volume: f32) -> MwResult      // 既定ランプ経由
mw_bus_fade(handle, bus: i32, target: f32, ms: f32) -> MwResult   // 呼び出し側指定の時間

mw_music_set(handle, sound_id: u64) -> MwResult
    // ストリーミング再生の準備(初期構築仕様 §4.3, M2-7)。sound_id は mode=Music の
    // mw_sound_load が返した ID であること(SE の ID を渡すと ErrInvalidSoundId)。
    // 非ブロッキング。プリロール完了は待たない——状態は Loading のままで、
    // mw_music_state が Ready を返すまで C# 側がポーリングする契約。
    // 内部で「デコーダ差し替え → MusicPrepare → MusicSeek{0}」の順に処理する
    // (前曲のリングバッファ内 PCM の掃除。順序厳守。ffi.rs のドキュメント参照)
mw_music_state(handle, out_state: *mut i32) -> MwResult
    // MwMusicState(Loading=0/Ready=1/Playing=2/Paused=3)の判別子を i32 で書き込む
mw_music_pause(handle) -> MwResult                                // 既定ランプでフェードアウトして Paused へ
mw_music_resume_at(handle, frames: u64) -> MwResult               // 巻き戻し付き再開(既定ランプでフェードイン)
mw_music_seek(handle, frames: u64) -> MwResult                    // ランプを経由しない不連続。クロックの世代カウンタが進む
mw_music_stop(handle) -> MwResult                                 // Playing はフェードアウト後 Ready へ、Paused は即座に Ready へ
mw_music_set_loop(handle, begin_frames: u64, end_frames: u64) -> MwResult
    // begin==0 && end==0 はループ解除(【仮】)。それ以外で begin>=end は
    // ErrInvalidLoopRegion(mw_core::MusicVoice::set_loop は同じ状況を黙って
    // ループ無しへ丸めるが、FFI 境界では明示的に拒否する)
mw_music_get_position(handle, out: *mut MwMusicPosition) -> MwResult
    // 音楽クロックのスナップショット(初期構築仕様 §4.4)。GC アロケーションゼロ。
    // MwMusicPosition は blittable(is_playing は u8。bool の 4byte マーシャリング問題を回避)
mw_music_play_scheduled(handle, host_time_ns: u64) -> MwResult
    // 楽曲の予約再生(初期構築仕様 §4.3)。プリロール未完了時の繰り下げは Rust 内部
    // (`mw_core::Renderer::music_schedule_deferred`)からのみ問い合わせ可能

mw_bgm_set(handle, sound_id: u64) -> MwResult
    // BGM 用の2本目の楽曲ボイス(初期構築仕様『§2』M14, M4-3)を準備する。
    // sound_id は mw_music_set と同じ mode=Music の ID(ストレージ・ID 空間を共有)。
    // 手順・非ブロッキング契約は mw_music_set と同一(対象がbgm_voiceに変わるだけ)
mw_bgm_state(handle, out_state: *mut i32) -> MwResult
    // BGM ボイスの状態(MwMusicState、mw_music_state と同じ判別子)。
    // クロックの一部ではない——曲位置・世代カウンタは持たない(mw_bgm_get_position 相当は無い)
mw_bgm_play(handle) -> MwResult      // Ready からのみ有効、既定ランプでフェードイン
mw_bgm_stop(handle) -> MwResult      // 既定ランプでフェードアウトしてから Ready へ
mw_bgm_set_loop(handle, begin_frames, end_frames) -> MwResult
    // mw_music_set_loop と同じ規約(0,0 はループ解除、begin>=end は ErrInvalidLoopRegion)

mw_get_output_latency_ns(handle, out_ns: *mut u64) -> MwResult
    // 出力レイテンシの実測値(ns)。0 は「まだ不明」(コールバック未実行)

mw_get_output_underrun_stats(handle, out: *mut MwOutputUnderrunStats) -> MwResult
    // 出力コールバックのアンダーラン(の疑い)統計(初期構築仕様『§2』M3)。
    // count(累計)/ last_host_time_ns(直近検知時刻、未検知なら0)/
    // consecutive_count(直近まで連続した検知回数)。mw_poll_events の
    // MwEventKind.Underrun(楽曲/BGM のデコードリングバッファ枯渇)とは別物——
    // こちらは音声コールバック自体の間隔異常(OS 側出力バッファの枯渇の兆候)。
    // 詳細は mw_backend::underrun モジュール doc 参照

mw_poll_events(handle, buf: *mut MwEvent, cap: i32, out_dropped: *mut u32) -> i32
    // 初期構築仕様 §4.6。C → C# のコールバックはしない(M4)——C# 側が毎フレーム
    // これを呼ぶ想定。戻り値は他の FFI 関数と異なり「書き込んだ件数」(0以上)。
    // 失敗時のみ他と同じ MwResult の負の値。buf は呼び出し側確保のバッファへ
    // blittable な MwEvent を直接書き込む(GC アロケーションゼロ)。out_dropped には
    // キュー(固定容量【仮】64、溢れたら古いものから破棄)が今回のポーリングで
    // 溢れさせた件数を書く
```

すべて非ブロッキング(コマンドをキューへ積むだけ)。キューが満杯の場合は
`MwResult::ErrCommandQueueFull` を返す(黙って捨てない。「次のコールバックで必ず発音」の
保証はコマンドが実際にキューへ積まれたことが前提のため)。

### `bus` / `mode` を `i32` で受け取る理由(enum を FFI 引数に直接使わない)

`#[repr(i32)]` の Rust enum を `extern "C"` 関数の引数型に直接使うと、呼び出し側
(C#)が列挙の定義外の整数値を渡した場合に未定義動作になりうる(Rust は enum が
宣言済みの判別子以外の値を取らない前提で最適化する)。これを避けるため、
`mw_bus_*` 系・`mw_sound_load` は `bus`/`mode` を素の `i32` として受け取り、
`MwBus::from_raw` / `MwSoundMode::from_raw`(`src/types.rs`)で検証してから使う。
範囲外の値は `MwResult::ErrInvalidBus` / `MwResult::ErrUnsupportedSoundMode` を返す。
`mw_music_state` の `out_state` も同じ理由で `*mut i32`(`MwMusicState` を直接
書き込ませない)——ただし出力専用引数なので C# 側が不正値を書けるわけではなく、
あくまで既存の規約(「値は常に生の整数で FFI 境界を跨ぐ」)と揃えるための選択。
一方 `MwMusicPosition::state` は `MwMusicState` を直接フィールドに持つ(理由は
`types.rs` のドキュメント参照)。

### 楽曲 ID の空間分離(`mw_sound_load(mode=Music)` / M2-7)

SE の ID は `mw_core::SoundStorage` が採番し `Arc<SoundData>`(デコード済み PCM)を
指す。楽曲は「圧縮のまま保持」なので置き場所が違い(`handle.rs::Instance::
music_bytes`)、同じ ID 空間を共有すると `mw_sound_release` が誤って SE 側の
エントリを消してしまいうる。そこで楽曲 ID は最上位ビット
(`handle.rs::MUSIC_ID_FLAG`)を立てて空間を分離してあり、`mw_sound_load`/
`mw_music_set`/`mw_sound_release` はこのビットで自動的に正しいストレージへ
振り分ける。ID は C# から見て不透明な整数(初期構築仕様『§4.8』)なので、この
ビット演算による分離は呼び出し側に副作用を持たない。

### BGM(`mw_bgm_*` / M4-3)との分担 —— 共有するもの・分けるもの

初期構築仕様『§2』M14「BGM 用の2本目の楽曲ボイス」の実装。楽曲(`mw_music_*`)との
分担は次のとおり:

- **共有するもの**: 圧縮バイト列のストレージ・ID 空間(上記「楽曲 ID の空間分離」
  そのまま——`mw_bgm_set` は `Instance::get_music_bytes` を呼ぶだけで、BGM 専用の
  ストレージや ID フラグを新設していない)、ストリーミングデコードの仕組み一式
  (`SymphoniaDecoder` / `decode_thread::spawn`)、状態機械(`mw_core::MusicVoice`
  をそのまま転用。ループ・フェードの実装も共有)、Bgm バス(`mw_bus_set_volume`/
  `mw_bus_fade` をそのまま使う。BGM 専用バスは追加しない)。
- **分けるもの**: リングバッファとデコードスレッドは BGM 専用にもう1本立てる
  (`handle.rs::Instance::bgm_decoder_tx` 等)。**クロックは発行しない**
  (`mw_music_get_position` 相当の関数が無い。発行元は楽曲ボイスに固定、M14)ため、
  代わりに軽量な `mw_core::BgmStatePublisher` で状態だけを公開する。サンプル精度の
  予約再生・巻き戻し付き再開も BGM には無い(不要なため、対応する FFI 関数も
  `mw_core::Command` のバリアントも意図的に持たせていない)。

## 不変条件

- **全関数を `std::panic::catch_unwind` で包む**(`AssertUnwindSafe` 経由)。
  Rust の panic を FFI 境界の外(C#/IL2CPP)へ絶対に漏らさない。
  捕捉した場合は `MwResult::ErrPanic` を返す(初期構築仕様 §5.4)。
- 生ポインタを受け取る関数は必ず null チェックしてから deref する。null は
  `MwResult::ErrNullPointer` を返し、書き込みを行わない。
- ハンドルは `handle.rs` のレジストリでのみ管理する。他のモジュールが独自にハンドルを
  発行・検証しない(唯一の正を保つ)。
- ここに実装するロジックはゲームスレッドから呼ばれる想定(非ブロッキング)。
  実際の音声コールバックは `mw-backend` が生成するストリームのクロージャ内で走り、
  そこから `mw-core::Renderer::render` だけを呼ぶ(リアルタイム安全性規約は
  `mw-core/CLAUDE.md` を参照。mw-ffi 自体は音声スレッド上のコードをほぼ持たない)。
- シンボルは `mw_` プレフィックスで統一する(csbindgen 生成物の一貫性のため)。

## csbindgen 生成物について

`unity/Runtime/Generated/NativeMethods.g.cs` は **コミットしない**(`.gitignore` 対象)。
`make bindgen`(実体は `cargo build -p mw-ffi`)で毎回再生成する。
関数シグネチャやドキュメントコメントを変更したら、必ず `make bindgen` を実行して
生成物を最新化してからコミットすること(手書きの宣言ズレを構造的に排除するのが
csbindgen 採用の目的、M2)。

DllImport 先のライブラリ名は `csharp_dll_name("mw_ffi")`(macOS: `libmw_ffi.dylib`,
Android: `libmw_ffi.so`)、iOS ビルド(`UNITY_IOS && !UNITY_EDITOR`)のみ `__Internal`
(静的リンクのため)。
