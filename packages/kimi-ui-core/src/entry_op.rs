//! UI projection contract (design `reports/design/RUST-UI-DECOUPLING.md`
//! §1.1/§1.2): the `EntryOp` vocabulary every UI consumes, plus the in-memory
//! projection semantics (`EntryStore`) used to test them.
//!
//! ## Merged-delta rule (batching contract)
//!
//! One op never carries one token. The engine-side coalescer (tunables in
//! [`crate::coalesce`]) merges pending deltas and emits one
//! [`EntryOp::Append`] per merged segment; an [`AppendDelta`] covers the
//! inclusive [`SeqSpan`] of engine event seqs it folds. Ordering is by span,
//! not by arrival:
//!
//! - inside one entry stream, spans must continue exactly
//!   (`span.first == prev.last + 1`); a gap or overlap is a contract violation;
//! - the stream-closing segment carries `is_final = true`. Boundary flushing
//!   guarantees the final segment is emitted before the sealing snapshot
//!   event, so a [`EntryOp::Seal`] that follows a non-final segment means the
//!   tail was lost upstream — recorded as
//!   [`ContractViolation::SealWithoutFinal`], never a panic.
//!
//! All violations are recorded and the offending op dropped or absorbed; the
//! projection never panics, because the ops cross a process boundary.

use crate::entry_id::{EntryId, EntryKind};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Inclusive span `[first, last]` of the per-stream delta ordinals one merged
/// segment folds. Ordinals are local to each entry's stream (that stream's
/// first delta is ordinal 0), so interleaved streams never false-positive the
/// continuity check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeqSpan {
    pub first: u64,
    pub last: u64,
}

impl SeqSpan {
    pub const fn new(first: u64, last: u64) -> Self {
        Self { first, last }
    }

    /// `first <= last`. Construction is unchecked; validity is asserted here.
    pub const fn is_valid(&self) -> bool {
        self.first <= self.last
    }

    /// True when `self` follows `prev` with neither a gap nor an overlap.
    pub const fn continues(&self, prev: &SeqSpan) -> bool {
        self.first == prev.last + 1
    }
}

/// One merged stream segment: the text of every engine delta in `span`,
/// concatenated in seq order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppendDelta {
    pub text: String,
    pub span: SeqSpan,
    /// True on the stream-closing segment. See the module-level batching rule.
    pub is_final: bool,
}

/// Entry payload.
///
/// `kind` is the entry's own identity kind, carried here as well because the
/// projection is what a renderer receives: reading it off the id forces every
/// consumer to re-parse `EntryId`, and a renderer that ignores the kind draws
/// a tool call like a paragraph. The two are kept in step by
/// [`EntryContent::new`] and [`EntryStore::apply`], which rejects a mismatch
/// as a contract violation rather than letting a renderer see a contradiction.
///
/// Structured bodies (markdown AST, tool-card payloads) arrive with the
/// behavior slices; this struct is the seam they grow through.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryContent {
    pub text: String,
    #[serde(default = "EntryContent::default_kind")]
    pub kind: EntryKind,
    /// Set when the entry came from a subagent rather than the main agent.
    /// The UI keeps them apart (v2 announces the agent on every event), and a
    /// renderer that cannot see the owner cannot group them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
}

impl EntryContent {
    fn default_kind() -> EntryKind {
        EntryKind::AssistantMsg
    }

    /// Plain text of the given kind, main agent.
    pub fn new(kind: EntryKind, text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind,
            agent_id: None,
        }
    }

    /// The same content, attributed to `agent_id`.
    pub fn by_agent(mut self, agent_id: impl Into<String>) -> Self {
        self.agent_id = Some(agent_id.into());
        self
    }

    /// Whether this entry belongs to a subagent rather than the main agent.
    pub fn is_subagent(&self) -> bool {
        self.agent_id.as_deref().is_some_and(|id| id != "main")
    }
}

/// One projection op against the UI's entry table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EntryOp {
    /// Create or wholesale-replace an entry (idempotent upsert; folds a
    /// delta stream's final snapshot into the same id).
    Upsert { id: EntryId, content: EntryContent },
    /// Grow an entry by one merged segment.
    Append { id: EntryId, delta: AppendDelta },
    /// Finalize an entry; any later content mutation is a contract violation.
    Seal { id: EntryId },
    /// Remove an entry (undo range pruning).
    Remove { id: EntryId },
    /// Ordering fence for debugging and recorded-stream tests. No-op on apply.
    Barrier,
}

/// A breach of the projection contract. Recorded by [`EntryStore`]; the
/// violating op is dropped unless noted otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContractViolation {
    /// `Append` for an id that was never upserted.
    AppendToUnknown { id: EntryId },
    /// `Append` after `Seal` — sealed entries are immutable.
    AppendAfterSeal { id: EntryId },
    /// The payload's kind disagrees with the id's. Applied (the id is the
    /// identity) but recorded: a renderer drawing one kind under another's
    /// identity is a projection bug that would otherwise look like styling.
    KindMismatch {
        id: EntryId,
        id_kind: EntryKind,
        content_kind: EntryKind,
    },
    /// `Append` after a segment that already carried `is_final`.
    AppendAfterFinal { id: EntryId },
    /// `span.first > prev.last + 1`: bytes were lost upstream. The segment is
    /// *applied* (dropping it would compound the loss) and tracking resyncs to
    /// it; the violation stays on record.
    SeqGap {
        id: EntryId,
        expected: u64,
        got: u64,
    },
    /// `span.first <= prev.last`: duplicate or overlapping bytes. Dropped.
    SeqOverlap {
        id: EntryId,
        expected: u64,
        got: u64,
    },
    /// `Seal` over a streamed entry whose last segment was not final — the
    /// tail-loss detector of the batching contract. The seal still applies.
    SealWithoutFinal { id: EntryId },
    /// `Seal` for an id that was never upserted.
    SealUnknown { id: EntryId },
    /// `Upsert` after `Seal` — sealed content is authoritative; dropped.
    UpsertAfterSeal { id: EntryId },
    /// `Remove` for an id that does not exist.
    RemoveUnknown { id: EntryId },
}

#[derive(Debug)]
struct EntryRecord {
    content: EntryContent,
    sealed: bool,
    last_span: Option<SeqSpan>,
    saw_final: bool,
}

impl EntryRecord {
    fn new(content: EntryContent) -> Self {
        Self {
            content,
            sealed: false,
            last_span: None,
            saw_final: false,
        }
    }
}

/// In-memory entry table the ops project onto. Pure and deterministic: the
/// projection contract is testable without a terminal.
#[derive(Debug, Default)]
pub struct EntryStore {
    entries: BTreeMap<EntryId, EntryRecord>,
    violations: Vec<ContractViolation>,
}

impl EntryStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one op, recording (never panicking on) contract violations.
    pub fn apply(&mut self, op: EntryOp) {
        match op {
            EntryOp::Upsert { id, content } => {
                // The id names the kind; a payload that disagrees would make
                // the renderer draw one thing under another thing's identity.
                if content.kind != id.kind {
                    self.violations.push(ContractViolation::KindMismatch {
                        id: id.clone(),
                        id_kind: id.kind,
                        content_kind: content.kind,
                    });
                }
                match self.entries.get_mut(&id) {
                    Some(record) if record.sealed => {
                        self.violations
                            .push(ContractViolation::UpsertAfterSeal { id });
                    }
                    Some(record) => {
                        // Wholesale replace starts a new content epoch: stream
                        // bookkeeping resets so a post-fold segment starts fresh.
                        record.content = content;
                        record.last_span = None;
                        record.saw_final = false;
                    }
                    None => {
                        self.entries.insert(id, EntryRecord::new(content));
                    }
                }
            }
            EntryOp::Append { id, delta } => match self.entries.get_mut(&id) {
                None => {
                    self.violations
                        .push(ContractViolation::AppendToUnknown { id });
                }
                Some(record) => {
                    if record.sealed {
                        self.violations
                            .push(ContractViolation::AppendAfterSeal { id });
                        return;
                    }
                    if record.saw_final {
                        self.violations
                            .push(ContractViolation::AppendAfterFinal { id });
                        return;
                    }
                    if let Some(prev) = record.last_span {
                        let expected = prev.last + 1;
                        if delta.span.first <= prev.last {
                            self.violations.push(ContractViolation::SeqOverlap {
                                id,
                                expected,
                                got: delta.span.first,
                            });
                            return;
                        }
                        if !delta.span.continues(&prev) {
                            self.violations.push(ContractViolation::SeqGap {
                                id,
                                expected,
                                got: delta.span.first,
                            });
                            // Fall through: apply the segment anyway (see the
                            // SeqGap doc) and resync tracking to it.
                        }
                    }
                    record.content.text.push_str(&delta.text);
                    record.last_span = Some(delta.span);
                    record.saw_final = delta.is_final;
                }
            },
            EntryOp::Seal { id } => match self.entries.get_mut(&id) {
                None => {
                    self.violations.push(ContractViolation::SealUnknown { id });
                }
                Some(record) => {
                    // A streamed entry (at least one Append) must end on a
                    // final segment; an upsert-only entry seals cleanly.
                    if record.last_span.is_some() && !record.saw_final {
                        self.violations
                            .push(ContractViolation::SealWithoutFinal { id });
                    }
                    record.sealed = true;
                }
            },
            EntryOp::Remove { id } => {
                if self.entries.remove(&id).is_none() {
                    self.violations
                        .push(ContractViolation::RemoveUnknown { id });
                }
            }
            EntryOp::Barrier => {}
        }
    }

    pub fn get(&self, id: &EntryId) -> Option<&EntryContent> {
        self.entries.get(id).map(|record| &record.content)
    }

    pub fn contains(&self, id: &EntryId) -> bool {
        self.entries.contains_key(id)
    }

    pub fn is_sealed(&self, id: &EntryId) -> bool {
        self.entries.get(id).is_some_and(|record| record.sealed)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterate entries in deterministic (sorted) id order, for rendering.
    pub fn entries(&self) -> impl Iterator<Item = (&EntryId, &EntryContent)> {
        self.entries
            .iter()
            .map(|(id, record)| (id, &record.content))
    }

    /// Violations recorded so far, in arrival order.
    pub fn violations(&self) -> &[ContractViolation] {
        &self.violations
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry_id::{EntryKind, EntryNs};

    fn eid(seq: u64) -> EntryId {
        EntryId::engine("turn-1", seq, EntryKind::AssistantMsg)
    }

    fn content(text: &str) -> EntryContent {
        EntryContent::new(EntryKind::AssistantMsg, text)
    }

    fn segment(text: &str, first: u64, last: u64, is_final: bool) -> AppendDelta {
        AppendDelta {
            text: text.into(),
            span: SeqSpan::new(first, last),
            is_final,
        }
    }

    #[test]
    fn upsert_append_seal_is_the_happy_path() {
        let mut store = EntryStore::new();
        let id = eid(1);
        store.apply(EntryOp::Upsert {
            id: id.clone(),
            content: content(""),
        });
        store.apply(EntryOp::Append {
            id: id.clone(),
            delta: segment("hello ", 0, 4, false),
        });
        store.apply(EntryOp::Append {
            id: id.clone(),
            delta: segment("world", 5, 9, true),
        });
        store.apply(EntryOp::Seal { id: id.clone() });
        assert_eq!(store.get(&id).unwrap().text, "hello world");
        assert!(store.is_sealed(&id));
        assert_eq!(store.violations(), &[]);
    }

    #[test]
    fn one_append_carries_a_merged_segment_not_a_token() {
        let mut store = EntryStore::new();
        let id = eid(1);
        store.apply(EntryOp::Upsert {
            id: id.clone(),
            content: content(""),
        });
        // One op folds seqs 0..=49 — the shape the coalescer emits instead of
        // fifty single-token ops.
        store.apply(EntryOp::Append {
            id: id.clone(),
            delta: segment("merged fifty tokens", 0, 49, true),
        });
        store.apply(EntryOp::Seal { id: id.clone() });
        assert_eq!(store.get(&id).unwrap().text, "merged fifty tokens");
        assert_eq!(store.violations(), &[]);
    }

    #[test]
    fn append_after_seal_is_recorded_and_dropped() {
        let mut store = EntryStore::new();
        let id = eid(1);
        store.apply(EntryOp::Upsert {
            id: id.clone(),
            content: content("done"),
        });
        store.apply(EntryOp::Seal { id: id.clone() });
        store.apply(EntryOp::Append {
            id: id.clone(),
            delta: segment("late", 0, 3, false),
        });
        assert_eq!(store.get(&id).unwrap().text, "done");
        assert_eq!(
            store.violations(),
            &[ContractViolation::AppendAfterSeal { id: id.clone() }]
        );
    }

    #[test]
    fn upsert_after_seal_is_recorded_and_dropped() {
        let mut store = EntryStore::new();
        let id = eid(1);
        store.apply(EntryOp::Upsert {
            id: id.clone(),
            content: content("done"),
        });
        store.apply(EntryOp::Seal { id: id.clone() });
        store.apply(EntryOp::Upsert {
            id: id.clone(),
            content: content("overwrite"),
        });
        assert_eq!(store.get(&id).unwrap().text, "done");
        assert_eq!(
            store.violations(),
            &[ContractViolation::UpsertAfterSeal { id: id.clone() }]
        );
    }

    #[test]
    fn seal_without_final_flags_a_lost_tail() {
        let mut store = EntryStore::new();
        let id = eid(1);
        store.apply(EntryOp::Upsert {
            id: id.clone(),
            content: content(""),
        });
        store.apply(EntryOp::Append {
            id: id.clone(),
            delta: segment("truncated", 0, 8, false),
        });
        store.apply(EntryOp::Seal { id: id.clone() });
        assert!(store.is_sealed(&id));
        assert_eq!(
            store.violations(),
            &[ContractViolation::SealWithoutFinal { id: id.clone() }]
        );
    }

    #[test]
    fn seal_after_upsert_only_is_clean() {
        // Snapshot-shaped entries never stream; sealing them is not a
        // tail-loss signal.
        let mut store = EntryStore::new();
        let id = eid(1);
        store.apply(EntryOp::Upsert {
            id: id.clone(),
            content: content("complete"),
        });
        store.apply(EntryOp::Seal { id: id.clone() });
        assert_eq!(store.violations(), &[]);
    }

    #[test]
    fn seq_gap_is_recorded_but_the_segment_is_applied() {
        let mut store = EntryStore::new();
        let id = eid(1);
        store.apply(EntryOp::Upsert {
            id: id.clone(),
            content: content(""),
        });
        store.apply(EntryOp::Append {
            id: id.clone(),
            delta: segment("ab", 0, 1, false),
        });
        store.apply(EntryOp::Append {
            id: id.clone(),
            delta: segment("ef", 4, 5, true),
        });
        assert_eq!(store.get(&id).unwrap().text, "abef");
        assert_eq!(
            store.violations(),
            &[ContractViolation::SeqGap {
                id: id.clone(),
                expected: 2,
                got: 4,
            }]
        );
    }

    #[test]
    fn seq_overlap_is_recorded_and_dropped() {
        let mut store = EntryStore::new();
        let id = eid(1);
        store.apply(EntryOp::Upsert {
            id: id.clone(),
            content: content(""),
        });
        store.apply(EntryOp::Append {
            id: id.clone(),
            delta: segment("ab", 0, 1, false),
        });
        store.apply(EntryOp::Append {
            id: id.clone(),
            delta: segment("bc", 1, 2, false),
        });
        assert_eq!(store.get(&id).unwrap().text, "ab");
        assert_eq!(
            store.violations(),
            &[ContractViolation::SeqOverlap {
                id: id.clone(),
                expected: 2,
                got: 1,
            }]
        );
    }

    #[test]
    fn append_after_final_is_recorded_and_dropped() {
        let mut store = EntryStore::new();
        let id = eid(1);
        store.apply(EntryOp::Upsert {
            id: id.clone(),
            content: content(""),
        });
        store.apply(EntryOp::Append {
            id: id.clone(),
            delta: segment("end", 0, 2, true),
        });
        store.apply(EntryOp::Append {
            id: id.clone(),
            delta: segment("zombie", 3, 8, false),
        });
        assert_eq!(store.get(&id).unwrap().text, "end");
        assert_eq!(
            store.violations(),
            &[ContractViolation::AppendAfterFinal { id: id.clone() }]
        );
    }

    #[test]
    fn upsert_replaces_content_and_resets_stream_tracking() {
        // The delta→snapshot fold: `tool.call.delta`* then `tool.native`
        // projects to Append* + a final Upsert on the same id (design §1.2).
        let mut store = EntryStore::new();
        let id = eid(1);
        store.apply(EntryOp::Upsert {
            id: id.clone(),
            content: content(""),
        });
        store.apply(EntryOp::Append {
            id: id.clone(),
            delta: segment("partial", 0, 6, false),
        });
        store.apply(EntryOp::Upsert {
            id: id.clone(),
            content: content("full result"),
        });
        store.apply(EntryOp::Seal { id: id.clone() });
        assert_eq!(store.get(&id).unwrap().text, "full result");
        assert_eq!(store.violations(), &[]);
    }

    #[test]
    fn remove_prunes_and_unknown_ops_are_recorded() {
        let mut store = EntryStore::new();
        let id = eid(1);
        let ghost = eid(2);
        store.apply(EntryOp::Remove { id: ghost.clone() });
        store.apply(EntryOp::Append {
            id: ghost.clone(),
            delta: segment("x", 0, 0, false),
        });
        store.apply(EntryOp::Seal { id: ghost.clone() });
        store.apply(EntryOp::Upsert {
            id: id.clone(),
            content: content("here"),
        });
        store.apply(EntryOp::Remove { id: id.clone() });
        assert!(!store.contains(&id));
        assert_eq!(
            store.violations(),
            &[
                ContractViolation::RemoveUnknown { id: ghost.clone() },
                ContractViolation::AppendToUnknown { id: ghost.clone() },
                ContractViolation::SealUnknown { id: ghost },
            ]
        );
    }

    #[test]
    fn barrier_is_a_no_op() {
        let mut store = EntryStore::new();
        store.apply(EntryOp::Barrier);
        assert!(store.is_empty());
        assert_eq!(store.violations(), &[]);
    }

    #[test]
    fn seq_span_geometry() {
        let prev = SeqSpan::new(0, 4);
        assert!(SeqSpan::new(5, 9).continues(&prev));
        assert!(!SeqSpan::new(6, 9).continues(&prev));
        assert!(!SeqSpan::new(4, 9).continues(&prev));
        assert!(prev.is_valid());
        assert!(!SeqSpan::new(9, 4).is_valid());
    }

    #[test]
    fn namespace_split_keeps_ui_entries_out_of_engine_ordering() {
        let mut store = EntryStore::new();
        let engine = EntryId::engine("turn-1", 0, EntryKind::AssistantMsg);
        let local = EntryId::ui_local(0, EntryKind::Status);
        store.apply(EntryOp::Upsert {
            id: engine.clone(),
            content: content("engine"),
        });
        store.apply(EntryOp::Upsert {
            id: local.clone(),
            content: content("local"),
        });
        assert!(engine < local);
        assert_eq!(store.len(), 2);
        assert_eq!(engine.ns, EntryNs::Engine);
    }

    /// A renderer reads the kind off the content, not off the id: re-parsing
    /// an `EntryId` in every consumer is how one of them ends up drawing a
    /// tool call like a paragraph.
    #[test]
    fn content_carries_the_kind_and_the_agent() {
        let mut store = EntryStore::new();
        let id = EntryId::engine("turn-1", 0, EntryKind::ToolCall);
        store.apply(EntryOp::Upsert {
            id: id.clone(),
            content: EntryContent::new(EntryKind::ToolCall, "Read(x)").by_agent("sub-1"),
        });
        let got = store.get(&id).expect("upserted");
        assert_eq!(got.kind, EntryKind::ToolCall);
        assert_eq!(got.agent_id.as_deref(), Some("sub-1"));
        assert!(got.is_subagent(), "sub-1 is not the main agent");
        assert!(store.violations().is_empty(), "{:?}", store.violations());
    }

    /// A payload whose kind disagrees with its id is a projection bug: the id
    /// is the identity, so the content applies, but the mismatch is recorded
    /// instead of letting a renderer show one kind under another's identity.
    #[test]
    fn a_kind_mismatch_is_recorded_not_silently_applied() {
        let mut store = EntryStore::new();
        let id = EntryId::engine("turn-1", 0, EntryKind::ToolCall);
        store.apply(EntryOp::Upsert {
            id: id.clone(),
            content: EntryContent::new(EntryKind::AssistantMsg, "looks like prose"),
        });
        assert!(matches!(
            store.violations().first(),
            Some(ContractViolation::KindMismatch {
                id_kind: EntryKind::ToolCall,
                content_kind: EntryKind::AssistantMsg,
                ..
            })
        ));
        assert_eq!(
            store.get(&id).map(|c| c.kind),
            Some(EntryKind::AssistantMsg)
        );
    }

    /// A main-agent entry is not a subagent entry, and an unattributed one is
    /// main by default.
    #[test]
    fn only_a_non_main_agent_id_counts_as_a_subagent_entry() {
        assert!(!EntryContent::new(EntryKind::AssistantMsg, "x").is_subagent());
        assert!(
            !EntryContent::new(EntryKind::AssistantMsg, "x")
                .by_agent("main")
                .is_subagent()
        );
        assert!(
            EntryContent::new(EntryKind::AssistantMsg, "x")
                .by_agent("sub-9")
                .is_subagent()
        );
    }
}
