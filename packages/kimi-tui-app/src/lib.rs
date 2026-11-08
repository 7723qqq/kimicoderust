//! `kimi-tui-app` — the terminal UI's application layer.
//!
//! Layer position: `app`, above `shared`. Two edges: `kimi-ui-core` for the
//! entry contract and the line policy, and `textual-rs` for the widget tree,
//! the layout engine and the terminal itself. The crate never links the
//! engine: the link to a live session is the [`UiTransport`] trait.
//!
//! Scope, matching the first vertical slice in
//! `reports/design/RUST-UI-DECOUPLING.md` §4: startup, one turn's stream,
//! typing and submitting a prompt, and stopping a turn with Esc.

pub mod app;
pub mod input;
pub mod prompt;
pub mod screen;
pub mod status;
pub mod transcript;

pub use app::{
    ApprovalResponse, PendingApproval, PendingQuestion, QuestionResponse, RecordingTransport,
    UiCommand, UiState, UiTransport, run_terminal,
};
pub use input::to_input_event;
pub use screen::AppScreen;
pub use status::StatusLine;
