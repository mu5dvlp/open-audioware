//! mw-backend: 出力デバイス抽象(`Backend` trait)と cpal 実装。
//!
//! 依存の向き(§5.1): `mw-backend → mw-core` のみ。`mw-ffi` からのみ利用される。

#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(target_os = "android")]
pub mod android_context;
pub mod backend;
pub mod cpal_backend;
pub mod host_time;
pub mod ios_session;
pub mod platform_log;

pub use backend::{Backend, BackendError};
pub use cpal_backend::CpalBackend;
pub use host_time::host_time_ns;
