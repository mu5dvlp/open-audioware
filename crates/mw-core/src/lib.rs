//! mw-core: OS 非依存のミキサ・ボイス管理・クロック・デコード・リサンプルコア。
//!
//! 初期構築仕様 §5.1(【仮】): 依存の向きは `mw-ffi → mw-core / mw-backend`、
//! `mw-backend → mw-core` のみ。mw-core は他クレートに依存しない。
//! オフラインレンダリング(デバイス無しでバッファに書き出す)はこのクレートだけで完結し、
//! テストの主戦場になる(§8)。
//!
//! リアルタイム安全性規約(§5.3)はこのクレート全体の不変条件。
//! 詳細は `crates/mw-core/CLAUDE.md` を参照。

#![deny(unsafe_op_in_unsafe_fn)]

pub mod clock;
pub mod format;
pub mod renderer;

pub use clock::RenderedFrameCounter;
pub use format::{AudioFormat, CHANNELS, Sample};
pub use renderer::Renderer;
