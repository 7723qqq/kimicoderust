//! `kimi-ui-terminal` — the terminal backend for the Rust UI migration.
//!
//! Layer position: `app`, above `shared`. It depends on `kimi-ui-core` for the
//! component tree, the render contract and the style vocabulary, and on nothing
//! else: no crossterm, no ratatui, no libc. The bytes it writes are ANSI, which
//! every terminal this product targets understands.
//!
//! What is here: the two render strategies (`Mode::Scrollback`,
//! `Mode::AltScreen`), their capability sets, and SGR encoding.
//!
//! What is deliberately not here: raw mode, key decoding, mouse capture and
//! inline images. Raw mode and key decoding need the terminal handle an
//! application owns, and `kimi-ui-core`'s `InputEvent` is the neutral shape the
//! app decodes into; a decoder down here would have to re-derive the same
//! platform quirks. The app crate that lands next is where that belongs.
//!
//! Design: `reports/design/RUST-UI-DECOUPLING.md` §1.3 (render backend
//! contract) and §3 (crate boundaries).

pub mod ansi;
pub mod backend;

pub use ansi::{RESET, open, push_styled, sgr_params};
pub use backend::{Mode, TerminalBackend};
