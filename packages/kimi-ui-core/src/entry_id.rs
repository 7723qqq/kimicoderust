//! Structured entry identity (design `reports/design/RUST-UI-DECOUPLING.md`
//! §1.2).
//!
//! `EntryId` replaces `instanceof` addressing on the UI side: an entry is
//! named by the namespace of the side that produced it, the owning turn (for
//! engine entries), a per-namespace monotonic sequence, and its semantic
//! kind. Ids cross the engine↔UI boundary inside `EntryOp`s, so both sides
//! share one definition — this module is that definition.

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::error::Error;
use std::fmt;
use std::str::FromStr;

/// Namespace an [`EntryId`] is allocated from.
///
/// Engine entries are allocated by the engine and visible to every UI; UI-local
/// entries (status lines, banners, error toasts) are allocated by one UI and
/// never cross the engine boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryNs {
    Engine,
    UiLocal,
}

impl EntryNs {
    /// Sort rank: engine entries order before UI-local ones.
    const fn rank(self) -> u8 {
        match self {
            EntryNs::Engine => 0,
            EntryNs::UiLocal => 1,
        }
    }

    /// Token used in the string form of [`EntryId`].
    const fn token(self) -> &'static str {
        match self {
            EntryNs::Engine => "engine",
            EntryNs::UiLocal => "ui_local",
        }
    }

    fn from_token(token: &str) -> Option<Self> {
        match token {
            "engine" => Some(EntryNs::Engine),
            "ui_local" => Some(EntryNs::UiLocal),
            _ => None,
        }
    }
}

/// Semantic kind of a transcript entry.
///
/// Descriptive, not part of the identity: allocation guarantees
/// `(ns, turn_id, seq)` is unique, so two live entries never differ only in
/// `kind`. It participates in [`Ord`] solely as a deterministic tiebreaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    UserMsg,
    AssistantMsg,
    Thinking,
    ToolCall,
    Status,
    Banner,
    CompactionBoundary,
}

impl EntryKind {
    const fn token(self) -> &'static str {
        match self {
            EntryKind::UserMsg => "user_msg",
            EntryKind::AssistantMsg => "assistant_msg",
            EntryKind::Thinking => "thinking",
            EntryKind::ToolCall => "tool_call",
            EntryKind::Status => "status",
            EntryKind::Banner => "banner",
            EntryKind::CompactionBoundary => "compaction_boundary",
        }
    }

    fn from_token(token: &str) -> Option<Self> {
        Some(match token {
            "user_msg" => EntryKind::UserMsg,
            "assistant_msg" => EntryKind::AssistantMsg,
            "thinking" => EntryKind::Thinking,
            "tool_call" => EntryKind::ToolCall,
            "status" => EntryKind::Status,
            "banner" => EntryKind::Banner,
            "compaction_boundary" => EntryKind::CompactionBoundary,
            _ => return None,
        })
    }
}

/// Structured identity of one transcript entry.
///
/// Invariants, enforced by the constructors and the parser:
///
/// - `ns == Engine` ⇒ `turn_id` is `Some`.
/// - `ns == UiLocal` ⇒ `turn_id` is `None`.
/// - `seq` is monotonic within `(ns, turn_id)` when allocated through
///   [`EntryIdAllocator`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EntryId {
    pub ns: EntryNs,
    pub turn_id: Option<String>,
    pub seq: u64,
    pub kind: EntryKind,
}

impl EntryId {
    /// An engine-side entry owned by `turn_id`.
    pub fn engine(turn_id: impl Into<String>, seq: u64, kind: EntryKind) -> Self {
        Self {
            ns: EntryNs::Engine,
            turn_id: Some(turn_id.into()),
            seq,
            kind,
        }
    }

    /// A UI-local entry the engine never sees.
    pub fn ui_local(seq: u64, kind: EntryKind) -> Self {
        Self {
            ns: EntryNs::UiLocal,
            turn_id: None,
            seq,
            kind,
        }
    }
}

/// Total order: `(ns, turn_id, seq, kind)`.
///
/// Exists for deterministic storage and for range pruning (`undo.applied`
/// removes every engine entry of a turn with `seq > upto.seq`). It is *not* a
/// render-order guarantee across turns: the event contract deliberately gives
/// no ordering across `(agent_id, turn_id)` scopes. Comparing every field keeps
/// the order consistent with `Eq`.
impl Ord for EntryId {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.ns.rank(), self.turn_id.as_deref(), self.seq, self.kind).cmp(&(
            other.ns.rank(),
            other.turn_id.as_deref(),
            other.seq,
            other.kind,
        ))
    }
}

impl PartialOrd for EntryId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// String form: `<ns>:<turn_id|->:<seq>:<kind>`.
///
/// `-` stands in for the absent turn of a UI-local entry. `turn_id` values
/// containing `:` are outside the contract (engine turn ids are ULIDs/uuids).
impl fmt::Display for EntryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let turn = self.turn_id.as_deref().unwrap_or("-");
        write!(
            f,
            "{}:{}:{}:{}",
            self.ns.token(),
            turn,
            self.seq,
            self.kind.token()
        )
    }
}

/// [`EntryId::from_str`] failure. Unit type on purpose: the input that failed
/// is already in the caller's hands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseEntryIdError;

impl fmt::Display for ParseEntryIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid entry id")
    }
}

impl Error for ParseEntryIdError {}

impl FromStr for EntryId {
    type Err = ParseEntryIdError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let mut parts = text.split(':');
        let (Some(ns), Some(turn), Some(seq), Some(kind), None) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        ) else {
            return Err(ParseEntryIdError);
        };
        let ns = EntryNs::from_token(ns).ok_or(ParseEntryIdError)?;
        let seq: u64 = seq.parse().map_err(|_| ParseEntryIdError)?;
        let kind = EntryKind::from_token(kind).ok_or(ParseEntryIdError)?;
        match ns {
            EntryNs::Engine => {
                if turn.is_empty() || turn == "-" {
                    return Err(ParseEntryIdError);
                }
                Ok(EntryId::engine(turn, seq, kind))
            }
            EntryNs::UiLocal => {
                if turn != "-" {
                    return Err(ParseEntryIdError);
                }
                Ok(EntryId::ui_local(seq, kind))
            }
        }
    }
}

/// Monotonic id allocator for one namespace (and one turn, for engine
/// entries).
///
/// One allocator per scope: the engine keeps one per turn; a UI keeps one for
/// its local entries. Sequences start at 0 and never reuse values.
#[derive(Debug, Clone)]
pub struct EntryIdAllocator {
    ns: EntryNs,
    turn_id: Option<String>,
    next: u64,
}

impl EntryIdAllocator {
    /// Allocator for engine entries of one turn.
    pub fn engine(turn_id: impl Into<String>) -> Self {
        Self {
            ns: EntryNs::Engine,
            turn_id: Some(turn_id.into()),
            next: 0,
        }
    }

    /// Allocator for UI-local entries.
    pub fn ui_local() -> Self {
        Self {
            ns: EntryNs::UiLocal,
            turn_id: None,
            next: 0,
        }
    }

    /// The next id in this scope.
    pub fn alloc(&mut self, kind: EntryKind) -> EntryId {
        let seq = self.next;
        self.next += 1;
        EntryId {
            ns: self.ns,
            turn_id: self.turn_id.clone(),
            seq,
            kind,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_allocator_is_monotonic_and_stamps_the_turn() {
        let mut alloc = EntryIdAllocator::engine("turn-1");
        let a = alloc.alloc(EntryKind::UserMsg);
        let b = alloc.alloc(EntryKind::AssistantMsg);
        assert_eq!(a, EntryId::engine("turn-1", 0, EntryKind::UserMsg));
        assert_eq!(b, EntryId::engine("turn-1", 1, EntryKind::AssistantMsg));
        assert!(a < b);
    }

    #[test]
    fn ui_local_allocator_has_no_turn() {
        let mut alloc = EntryIdAllocator::ui_local();
        let a = alloc.alloc(EntryKind::Status);
        assert_eq!(a.ns, EntryNs::UiLocal);
        assert_eq!(a.turn_id, None);
        assert_eq!(a.seq, 0);
    }

    #[test]
    fn string_form_round_trips() {
        let cases = [
            EntryId::engine("turn-1", 7, EntryKind::AssistantMsg),
            EntryId::ui_local(3, EntryKind::Status),
            EntryId::engine("t", 0, EntryKind::CompactionBoundary),
        ];
        for id in cases {
            let parsed: EntryId = id.to_string().parse().unwrap();
            assert_eq!(parsed, id, "{}", id);
        }
    }

    #[test]
    fn string_form_is_stable() {
        assert_eq!(
            EntryId::engine("turn-1", 7, EntryKind::AssistantMsg).to_string(),
            "engine:turn-1:7:assistant_msg"
        );
        assert_eq!(
            EntryId::ui_local(3, EntryKind::Status).to_string(),
            "ui_local:-:3:status"
        );
    }

    #[test]
    fn parser_rejects_malformed_ids() {
        for bad in [
            "",
            "engine:turn-1:7",
            "engine:turn-1:7:assistant_msg:extra",
            "nope:turn-1:7:assistant_msg",
            "engine:-:7:assistant_msg",
            "engine::7:assistant_msg",
            "ui_local:turn-1:3:status",
            "engine:turn-1:x:assistant_msg",
            "engine:turn-1:7:nope",
        ] {
            assert!(bad.parse::<EntryId>().is_err(), "{bad}");
        }
    }

    #[test]
    fn ordering_is_ns_then_turn_then_seq() {
        let e1 = EntryId::engine("turn-1", 1, EntryKind::Status);
        let e2 = EntryId::engine("turn-1", 2, EntryKind::UserMsg);
        let e3 = EntryId::engine("turn-2", 0, EntryKind::UserMsg);
        let u1 = EntryId::ui_local(0, EntryKind::UserMsg);
        assert!(e1 < e2, "seq decides inside one turn");
        assert!(e2 < e3, "turn decides across turns");
        assert!(e3 < u1, "engine sorts before ui_local");
        // Consistency with Eq: equal ids compare Equal.
        assert_eq!(e1.cmp(&e1.clone()), Ordering::Equal);
    }

    #[test]
    fn serde_round_trip() {
        let id = EntryId::engine("turn-1", 7, EntryKind::ToolCall);
        let json = serde_json::to_value(&id).unwrap();
        assert_eq!(json["ns"], "engine");
        assert_eq!(json["turn_id"], "turn-1");
        assert_eq!(json["seq"], 7);
        assert_eq!(json["kind"], "tool_call");
        let back: EntryId = serde_json::from_value(json).unwrap();
        assert_eq!(back, id);
    }
}
