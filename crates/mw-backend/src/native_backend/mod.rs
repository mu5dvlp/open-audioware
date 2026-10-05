//! 自前(非 cpal)バックエンドの置き場(AUDIOWARE-DEPS-PLAN.md ステップ4-1〜4-3)。
//!
//! OS ごとに実装を分ける。4-1(macOS)は [`apple`] に AudioUnit(AUHAL)実装を置き、
//! 4-2 で iOS/tvOS(RemoteIO)を同じファイルへ追加した——差分はサブタイプと
//! `ios_session::configure`(セッション設定)・補正項の更新(ルート変化監視)だけに
//! 絞れた(見込みどおり)。4-3(Android/AAudio)は別ファイルになる見込み。
//!
//! `mw-ffi` の切替口(`handle.rs::make_backend`)は `backend-native` Cargo feature で
//! ここの実装を選ぶ。選べる OS が無いターゲットでは `backend-native` を有効にすると
//! `mw-ffi` 側でコンパイルエラーにする(`backend-cpal` へ戻せることを保証するため)。

#[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
pub mod apple;

#[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
pub use apple::AppleBackend as NativeBackend;
