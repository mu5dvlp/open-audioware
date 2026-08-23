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

pub mod bus;
pub mod clipper;
pub mod clock;
pub mod command;
pub mod config;
pub mod decode;
pub mod format;
pub mod mixer;
pub mod music;
pub mod ramp;
pub mod renderer;
pub mod sound;
pub mod stream;
pub mod voice;
pub mod wav;

pub use bus::{ALL_BUSES, BUS_COUNT, Bus, BusId, BusSet};
pub use clipper::SoftClipper;
pub use clock::RenderedFrameCounter;
pub use command::Command;
pub use config::Config;
pub use decode::{DecodeError, MusicDecoder, SymphoniaDecoder};
pub use format::{AudioFormat, CHANNELS, Sample};
pub use mixer::{CommandSender, Mixer, ReclaimReceiver};
pub use music::{MusicFrameSource, MusicRenderOutcome, MusicState, MusicVoice};
pub use ramp::{Ramp, ms_to_samples};
pub use renderer::Renderer;
pub use sound::{SoundData, SoundId, SoundStorage};
pub use stream::{MusicStreamProducer, PumpOutcome, StreamingMusicSource};
pub use wav::WavError;
