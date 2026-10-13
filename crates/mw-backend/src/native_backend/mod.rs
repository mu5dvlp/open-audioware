//! 自前(非 cpal)バックエンドの置き場(AUDIOWARE-DEPS-PLAN.md ステップ4-1〜4-4)。
//!
//! OS ごとに実装を分ける。4-1(macOS)は [`apple`] に AudioUnit(AUHAL)実装を置き、
//! 4-2 で iOS/tvOS(RemoteIO)を同じファイルへ追加した——差分はサブタイプと
//! `ios_session::configure`(セッション設定)・補正項の更新(ルート変化監視)だけに
//! 絞れた(見込みどおり)。4-3(Android/AAudio)は [`android`] に別ファイルとして
//! 置いた——macOS/iOS/tvOS とタイムスタンプの取り方(`AudioTimeStamp.mHostTime` vs
//! `AAudioStream_getTimestamp`)が根本的に異なるため、共通化はしていない。
//!
//! Linux は [`linux`] に ALSA(libasound)の C API を直接叩く実装を置いた。専用スレッドが
//! `snd_pcm_writei` を回す方式で、他の OS のようなコールバック駆動ではない。
//!
//! Windows は [`windows`] に WASAPI(共有モード・イベント駆動)を COM の自前宣言で直接叩く実装を置いた。
//! 専用スレッドがイベントを待って `IAudioRenderClient` のバッファへ直接描画する。
//!
//! `android` / `linux` / `windows` モジュール自体は `target_os` を問わずコンパイルする(`host_time` と同じ
//! 流儀)——純粋ロジック(タイムスタンプの外挿計算・バッファ検証・エラー分類)を
//! ホスト(macOS)の `cargo test` から固定化するため。実際にハードウェアを叩く
//! `Backend` 実装([`android::AndroidBackend`] / [`linux::LinuxBackend`])自体はそれぞれ
//! Android / Linux 専用(`target_os = "android"` と `"linux"` は排他なので
//! `NativeBackend` の別名は重ならない)。
//!
//! `mw-ffi` の切替口(`handle.rs::make_backend`)は `backend-native` Cargo feature で
//! ここの実装を選ぶ。選べる OS が無いターゲットでは `backend-native` を有効にすると
//! `mw-ffi` 側でコンパイルエラーにする(`backend-cpal` へ戻せることを保証するため)。

pub mod android;
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
pub mod apple;
pub mod linux;
pub mod windows;

#[cfg(target_os = "android")]
pub use android::AndroidBackend as NativeBackend;
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
pub use apple::AppleBackend as NativeBackend;
#[cfg(target_os = "linux")]
pub use linux::LinuxBackend as NativeBackend;
#[cfg(target_os = "windows")]
pub use windows::WindowsBackend as NativeBackend;
