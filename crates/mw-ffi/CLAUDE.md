# mw-ffi

C ABI 境界。`mw-core` / `mw-backend` 両方に依存する唯一のクレート(依存の向きは
`mw-ffi → mw-core / mw-backend`。ワークスペース全体の方針は `../../CLAUDE.md` を参照)。
`crate-type = ["cdylib", "staticlib"]` — cdylib は macOS/Android、staticlib は iOS
(xcframework 経由)で使う。

## ファイル構成

- `src/result.rs` — `MwResult`(`#[repr(i32)]`)。全 FFI 関数の戻り値。`Ok = 0`、
  それ以外は負の整数のエラーコード(初期構築仕様 §4.8)。
- `src/types.rs` — `MwSoundMode` / `MwBus`。FFI 境界の `i32` 引数を検証・復元する内部専用の
  値型(`from_raw(i32) -> Option<Self>`)。**csbindgen の入力には含めない**
  (下記「enum を FFI 引数に直接使わない理由」参照)。
- `src/handle.rs` — init/shutdown のグローバルレジストリ(`OnceLock<Mutex<Option<Instance>>>`)。
  ハンドルは不透明な `u64`(ポインタを C# に渡さない)。二重 init は同一ハンドルを返す
  (冪等)、無効ハンドルの shutdown はエラーコードで検出する。`Instance` は M1 で
  `mw_core::CommandSender` / `mw_core::ReclaimReceiver` / `mw_core::SoundStorage` /
  ボイスシリアル採番器(`AtomicU64`)を持つ。
- `src/ffi.rs` — `#[unsafe(no_mangle)] pub extern "C" fn mw_*` 本体。
  **csbindgen の入力**(`build.rs` がここと `result.rs` を読む)。
- `build.rs` — csbindgen で `unity/Runtime/Generated/NativeMethods.g.cs` を生成する。

## 現状の公開 API(M1: SE 再生 / M2-5: ホスト時刻・予約発音)

```
mw_abi_version() -> u32                                          // 定数 1
mw_host_time_ns() -> u64                                          // ホスト単調時刻(ns)。ハンドル不要
mw_init(out_handle: *mut u64) -> MwResult                        // 冪等。既定出力デバイスにストリームを開く
mw_shutdown(handle: u64) -> MwResult                              // 冪等ではない(無効ハンドルはエラー)。ストリームを閉じる

mw_sound_load(handle, bytes: *const u8, len: usize, mode: i32, out_id: *mut u64) -> MwResult
    // mode=0(SE)のみ実装。mode=1(Music)は ErrUnsupportedSoundMode(楽曲ロード API は未実装)
mw_sound_release(handle, id: u64) -> MwResult
    // 再生中ボイスがあれば既定ランプ経由で即停止させたうえで解放する

mw_se_play(handle, id: u64, bus: i32, volume: f32, out_voice: *mut u64) -> MwResult
    // 次のオーディオコールバックで必ず発音される(初期構築仕様 §4.2)
mw_se_schedule(handle, id: u64, bus: i32, volume: f32, host_time_ns: u64, out_voice: *mut u64) -> MwResult
    // サンプル精度の予約発音(初期構築仕様 §4.5)。バッファ内オフセットへの丸めはしない。
    // 予約時刻が過去ならそのバッファの先頭で即座に発音する(取りこぼさない)
mw_voice_stop(handle, voice: u64) -> MwResult                     // 既定ランプ経由
mw_voice_set_volume(handle, voice: u64, volume: f32) -> MwResult  // 既定ランプ経由
mw_bus_set_volume(handle, bus: i32, volume: f32) -> MwResult      // 既定ランプ経由
mw_bus_fade(handle, bus: i32, target: f32, ms: f32) -> MwResult   // 呼び出し側指定の時間

mw_music_play_scheduled(handle, host_time_ns: u64) -> MwResult
    // 楽曲の予約再生(初期構築仕様 §4.3)。楽曲ロード API がまだ無いため、現状はコマンドが
    // 素通りするだけで実際には鳴らない(楽曲ボイスは Loading のまま繰り下げ続ける)。
    // プリロール未完了時の繰り下げは Rust 内部(`mw_core::Renderer::music_schedule_deferred`)
    // からのみ問い合わせ可能——FFI 公開は後続作業
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
