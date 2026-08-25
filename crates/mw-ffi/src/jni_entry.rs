//! Android の JNI エントリポイント。
//!
//! # このファイルが `ffi.rs` と別になっている理由
//!
//! **csbindgen の入力に含めてはならないため**(`build.rs` 参照)。
//!
//! csbindgen は `cfg` を評価せずソースを走査して `#[no_mangle] pub extern` を拾うので、
//! [`JNI_OnLoad`] を `ffi.rs` に置くと **Android 専用の関数なのに全プラットフォーム向けの
//! `DllImport` 宣言が生成される**。iOS はネイティブライブラリを静的リンクする
//! (`__DllName = "__Internal"`)ため、生成された宣言がマネージドコードに残ったまま
//! IL2CPP ビルドすると **`Undefined symbol: _JNI_OnLoad` でリンクに失敗する**
//! (実際にテンプレート側の iOS ビルドで発生した)。
//!
//! そもそも [`JNI_OnLoad`] は **Android ランタイムが `System.loadLibrary` の中から呼ぶ**
//! ものであり、C# から呼ぶ API ではない。バインディングを生成しないのが正しい。
//!
//! 同じ理由で、**プラットフォーム条件付きのエクスポートをここへ追加するときは
//! `build.rs` の入力に加えないこと**。

/// Android のライブラリロード時に JavaVM を受け取る。
///
/// cpal(AAudio)が Java 側を参照するため、`ndk_context` の初期化に JavaVM が要る
/// (`mw_backend::android_context` のモジュール doc 参照)。ここで控えておき、実際の初期化は
/// バックエンドを開く直前に行う。
///
/// **`System.loadLibrary` 経由でロードされた場合にのみ呼ばれる。** `dlopen` で直接開かれた
/// 場合は呼ばれないため、そのときは `ndk_context` を初期化できない旨がログに出る。
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "system" fn JNI_OnLoad(
    vm: *mut std::ffi::c_void,
    _reserved: *mut std::ffi::c_void,
) -> i32 {
    mw_backend::android_context::set_java_vm(vm);
    mw_backend::mw_log!("[mw-ffi] JNI_OnLoad: JavaVM を受け取った");

    // JNI_VERSION_1_6
    0x0001_0006
}
