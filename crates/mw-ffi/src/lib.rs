//! mw-ffi: C ABI 境界。
//!
//! 初期構築仕様 §5.1(【仮】): 依存の向きは `mw-ffi → mw-core / mw-backend` のみ。
//! ハンドル管理・コマンド/イベントキュー(M0 以降で拡張)・csbindgen の入力を置く場所。
//!
//! `crate-type = ["cdylib", "staticlib"]`。エクスポートされる `mw_` プレフィックスの
//! `extern "C"` 関数は `src/ffi.rs` にあり、`build.rs` がそこから
//! `unity/Runtime/Generated/NativeMethods.g.cs` を自動生成する。
//!
//! 設計・不変条件の詳細は `crates/mw-ffi/CLAUDE.md` を参照。

#![deny(unsafe_op_in_unsafe_fn)]

// C# 側の手書きラッパ(`unity/Runtime/MwNative.cs`)との判別子同期を `cargo test` で
// 検証するテスト専用モジュール。詳細・設計根拠はモジュール doc を参照。
#[cfg(test)]
mod csharp_abi_sync;
mod decode_thread;
mod event;
mod ffi;
mod handle;
// テスト専用のバックエンド差し替え口。実デバイスが無い環境でも内部再オープンの
// 段2・段3と、段2中の shutdown 割り込みを実際に通すための fake と直列化ヘルパを置く。
#[cfg(test)]
mod test_backend;
// Android の JNI エントリポイント。**csbindgen の入力に含めない**
// (理由はモジュール doc と build.rs のコメントを参照)。
mod jni_entry;
mod reopen;
mod result;
mod types;
