//! `kimi-ui-core` — shared contract types for the Rust UI migration.
//!
//! Layer position: `shared`, the bottom of `app > sdk > engine > shared`.
//! This crate must never depend on engine, sdk, or app crates; the engine
//! (`kimi-agent`) and the UI crates both depend on *it*, so engine events and
//! UI projections speak one vocabulary with no mirrored definitions.
//!
//! Layer contents (design: `reports/design/RUST-UI-DECOUPLING.md`):
//!
//! - [`entry_id`] — structured entry identity (`EntryId`) replacing
//!   `instanceof` addressing.
//! - [`entry_op`] — the UI projection contract (`EntryOp`) including merged
//!   stream segments (`AppendDelta`), so one op carries a coalesced span of
//!   deltas instead of one op per token.
//! - [`event_class`] — event category / direction / UI-visibility constraints.
//! - [`coalesce`] — engine-side delta coalescing policy knobs (defaults only;
//!   tuning is deferred to measurement).
//! - [`input`] — the neutral keyboard / mouse / paste vocabulary the app
//!   decodes a terminal event into.
//! - [`layout`] — the text primitives the row policy is built from: cell width,
//!   tab expansion, wrapping, and the row/block shapes.
//! - [`render`] — the row policy itself: what an entry looks like (gutter,
//!   wrap, per-kind dimming).
//!
//! What this crate deliberately does not contain is a renderer. The widget
//! tree, the layout engine and the terminal belong to `textual-rs`, and
//! `kimi-tui-app` paints [`render::entry_lines`] into the buffer that library
//! hands it. Keeping the row policy here — and only the policy — is what lets
//! the renderer change without changing what a reader sees.

pub mod coalesce;
pub mod entry_id;
pub mod entry_op;
pub mod event_class;
pub mod input;
pub mod layout;
pub mod render;
pub mod steer;

pub use coalesce::CoalescingPolicy;
pub use entry_id::{EntryId, EntryIdAllocator, EntryKind, EntryNs, ParseEntryIdError};
pub use entry_op::{AppendDelta, ContractViolation, EntryContent, EntryOp, EntryStore, SeqSpan};
pub use event_class::{
    DeltaPart, EventCategory, EventDirection, Projection, StreamKind, TRANSPORT_ONLY_EVENTS,
    UiVisibility, classify_delta_part, delta_part_text,
};
pub use input::{InputEvent, Key, KeyEvent, Modifiers, MouseEvent, MouseKind};
pub use layout::{Attributes, Block, Color, Row, Style, char_width, expand_tabs, width, wrap};
pub use steer::{SteerDelivery, SteerJournal, SteerStage, SteerTrace, SubmissionState};
