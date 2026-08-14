# mw-ffi

C ABI 境界。`mw-core` / `mw-backend` 両方に依存する唯一のクレート(依存の向きは
`mw-ffi → mw-core / mw-backend`。ワークスペース全体の方針は `../../CLAUDE.md` を参照)。
`crate-type = ["cdylib", "staticlib"]` — cdylib は macOS/Android、staticlib は iOS
(xcframework 経由)で使う。

## ファイル構成

- `src/result.rs` — `MwResult`(`#[repr(i32)]`)。全 FFI 関数の戻り値。`Ok = 0`、
  それ以外は負の整数のエラーコード(初期構築仕様 §4.8)。
- `src/handle.rs` — init/shutdown のグローバルレジストリ(`OnceLock<Mutex<Option<Instance>>>`)。
  ハンドルは不透明な `u64`(ポインタを C# に渡さない)。二重 init は同一ハンドルを返す
  (冪等)、無効ハンドルの shutdown はエラーコードで検出する。
- `src/ffi.rs` — `#[unsafe(no_mangle)] pub extern "C" fn mw_*` 本体。
  **csbindgen の入力**(`build.rs` がここと `result.rs` を読む)。
- `build.rs` — csbindgen で `unity/Runtime/Generated/NativeMethods.g.cs` を生成する。

## 現状の公開 API(M0)

```
mw_abi_version() -> u32                  // 定数 1
mw_init(out_handle: *mut u64) -> MwResult   // 冪等。既定出力デバイスにストリームを開く
mw_shutdown(handle: u64) -> MwResult        // 冪等ではない(無効ハンドルはエラー)。ストリームを閉じる
```

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
