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

mod event;
mod ffi;
mod handle;
mod result;
mod types;
