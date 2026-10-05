//! 自前(非 cpal)バックエンドの置き場(AUDIOWARE-DEPS-PLAN.md ステップ4-1〜4-3)。
//!
//! OS ごとに実装を分ける。4-1(macOS)は [`apple`] に AudioUnit(AUHAL)実装を置いた。
//! 4-2 で iOS(RemoteIO)を同じファイルへ追加する見込み(サブタイプと
//! `ios_session`/`ios_interruption` との接続だけが差分になる想定)。4-3(Android/AAudio)は
//! 別ファイルになる見込み。
//!
//! `mw-ffi` の切替口(`handle.rs::make_backend`)は `backend-native` Cargo feature で
//! ここの実装を選ぶ。選べる OS が無いターゲットでは `backend-native` を有効にすると
//! `mw-ffi` 側でコンパイルエラーにする(`backend-cpal` へ戻せることを保証するため)。

#[cfg(target_os = "macos")]
pub mod apple;

#[cfg(target_os = "macos")]
pub use apple::AppleBackend as NativeBackend;
