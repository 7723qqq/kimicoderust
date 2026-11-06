//! `kimi-ui-core` — shared contract types for the Rust UI migration.
//!
//! Layer position: `shared`, the bottom of `app > sdk > engine > shared`.
//! This crate must never depend on engine, sdk, or app crates; the engine
//! (`kimi-agent`) and the future UI crates both depend on *it*, so engine
//! events and UI projections speak one vocabulary with no mirrored
//! definitions.
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
//! - [`render_backend`] — the `RenderBackend` trait, the shared `NodeArena`
//!   behind every backend (parent links, depth-first paint order, cascade
//!   unmount, focus validation, queued input), and the three backends:
//!   scrollback passthrough, fullscreen alt-screen, and the plain-text
//!   [`headless`] one that drives projection tests and recorded-stream replay.
//!
//! The backend layer is complete on its own side of the seam: mounting,
//! unmounting, focus, input queuing, render scheduling and the two product
//! render strategies all behave the same way here, and `headless` proves it
//! without a terminal. What it deliberately does not contain yet is the
//! component model (measurement, styles, per-node input handlers) — that is
//! the next crate up, and [`render_backend::ComponentNode`] is the seam it
//! grows through.

pub mod coalesce;
pub mod entry_id;
pub mod entry_op;
pub mod event_class;
pub mod headless;
pub mod layout;
pub mod render_backend;

pub use coalesce::CoalescingPolicy;
pub use entry_id::{EntryId, EntryIdAllocator, EntryKind, EntryNs, ParseEntryIdError};
pub use entry_op::{AppendDelta, ContractViolation, EntryContent, EntryOp, EntryStore, SeqSpan};
pub use event_class::{
    DeltaPart, EventCategory, EventDirection, Projection, StreamKind, TRANSPORT_ONLY_EVENTS,
    UiVisibility, classify_delta_part, delta_part_text,
};
pub use headless::HeadlessBackend;
pub use layout::{Attributes, Block, Color, Row, Style, char_width, expand_tabs, width, wrap};
pub use render_backend::{
    AltScreenBackend, CapabilitySet, ComponentNode, InputEvent, Key, KeyEvent, Modifiers,
    MouseEvent, MouseKind, NodeId, RenderBackend, ScrollbackBackend, StopOptions,
};
