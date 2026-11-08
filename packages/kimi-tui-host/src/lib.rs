//! `kimi-tui-host` — the composition root that puts an engine session behind
//! the Rust terminal UI.
//!
//! Two crates meet here and nowhere else: `kimi-agent` (the session, the
//! projector, the native toolset) and `kimi-tui-app` (the widget stack and the
//! `UiTransport` seam). Neither links the other, which is why this crate exists
//! instead of the UI depending on the engine.
//!
//! What is wired:
//!
//! - [`build::build_engine`] assembles a session from the user's config: the
//!   native transport, the in-process permission engine, the native toolset and
//!   the tool table every standalone host advertises.
//! - [`build::BuiltEngine`] also carries the engine's event bus, already
//!   subscribed: every `EngineEvent` is folded by the projector into `EntryOp`s
//!   and applied to the UI state, so a streamed token reaches the transcript the
//!   way the engine emitted it.
//! - [`transport::EngineTransport`] is the other direction: a `UiCommand` becomes
//!   an enqueued turn or a cancellation.

pub mod build;
pub mod host;
pub mod transport;

pub use build::{BuiltEngine, build_engine, build_session};
pub use host::TuiHostCallbacks;
pub use transport::{EngineTransport, drive_commands};
