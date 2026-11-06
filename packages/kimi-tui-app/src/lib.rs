//! `kimi-tui-app` — the terminal UI's application layer.
//!
//! Layer position: `app`, above `shared`. Three edges: `kimi-ui-core` for the
//! entry contract and layout, `kimi-ui-terminal` for painting, and `crossterm`
//! for decoding terminal events and for the device itself.
//!
//! Scope, matching the first vertical slice in
//! `reports/design/RUST-UI-DECOUPLING.md` §4: startup, one turn's stream,
//! typing and submitting a prompt, and stopping a turn with Esc. The link to a
//! live session is the [`UiTransport`] trait, so this crate never links the
//! engine — the host that owns an engine handle implements it.

pub mod app;
pub mod input;

pub use app::{App, RecordingTransport, UiCommand, UiTransport};
pub use input::to_input_event;
