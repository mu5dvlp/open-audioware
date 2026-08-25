//! csbindgen で C ABI(`src/ffi.rs` / `src/result.rs`)から C# バインディングを生成する。
//!
//! 出力先は `unity/Runtime/Plugins/*` ではなく `unity/Runtime/Generated/NativeMethods.g.cs`
//! (初期構築仕様: UPM パッケージ骨格)。手書きの薄いラッパ `Mw.Native.MwNative` が
//! この生成コードを呼び出す。生成物自体はビルド成果物として .gitignore 対象。

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/ffi.rs");
    println!("cargo:rerun-if-changed=src/result.rs");
    println!("cargo:rerun-if-changed=src/event.rs");
    println!("cargo:rerun-if-changed=src/types.rs");
    // 注意: `src/jni_entry.rs` は**意図的に入力へ含めない**。csbindgen は `cfg` を評価せず
    // ソースを走査して `#[no_mangle] pub extern` を拾うため、Android 専用の `JNI_OnLoad` に
    // 対しても全プラットフォーム向けの `DllImport` 宣言を生成してしまう。iOS は静的リンク
    // (`__Internal`)なので、その宣言がマネージドコードに残ったまま IL2CPP ビルドすると
    // `Undefined symbol: _JNI_OnLoad` でリンクに失敗する(テンプレート側の iOS ビルドで実際に発生)。
    // プラットフォーム条件付きのエクスポートを足すときは同じ扱いにすること。

    let out_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../unity/Runtime/Generated/NativeMethods.g.cs");

    csbindgen::Builder::default()
        .input_extern_file("src/ffi.rs")
        .input_extern_file("src/result.rs")
        .input_extern_file("src/event.rs")
        // M2-7 で追加: `MwMusicPosition`(`mw_music_get_position` の out 引数の実型)
        // が extern 関数シグネチャに現れるようになったため、`event.rs` と同じ理由で
        // 入力に含める(`types.rs` モジュール doc 参照)。
        .input_extern_file("src/types.rs")
        // DllImport 先のバイナリ名(拡張子・lib プレフィックスは .NET のライブラリ解決規約に従う)。
        // macOS: libmw_ffi.dylib / Android: libmw_ffi.so
        .csharp_dll_name("mw_ffi")
        // iOS は静的リンク(xcframework)のため実行バイナリに直接埋め込まれる。
        .csharp_dll_name_if("UNITY_IOS && !UNITY_EDITOR", "__Internal")
        .csharp_class_name("NativeMethods")
        .csharp_namespace("Mw.Native.Generated")
        // Unity(IL2CPP/Mono)は C# 9 の `delegate*` 関数ポインタに非対応な構成があるため無効化。
        // M4 の決定(C→C# コールバックをしない)により現状は影響しないが既定を安全側に倒す。
        .csharp_use_function_pointer(false)
        .generate_csharp_file(out_dir)
        .expect("failed to generate C# bindings via csbindgen");
}
