//! Event classification and direction constraints (design
//! `reports/design/RUST-UI-DECOUPLING.md` §1.1): the minimal vocabulary the
//! engine↔UI event contract is stated in.

use serde::{Deserialize, Serialize};

/// Contract category of an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventCategory {
    /// Fire-and-forget (`assistant.delta`, `turn.ended`, `cron.fired`). The UI
    /// never replies; loss is fatal to the stream (the transport's
    /// `ErrorStrategy::Fatal` stance is part of the contract).
    Notify,
    /// Idempotent upsert semantics (`turn.ended`, `tool.native`); re-delivery
    /// must be harmless.
    Snapshot,
    /// Engine→UI request with an exactly-once UI→engine reply
    /// (`check_permission`, `ask_question`, `checkpoint`).
    Request,
    /// UI→engine command (`prompt`, `steer`, `cancel`, `undo`); the outcome
    /// flows back as notify/snapshot events, never as a synchronous return.
    Control,
}

/// Direction an event is allowed to travel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventDirection {
    EngineToUi,
    UiToEngine,
    /// Request events: engine→UI request, UI→engine reply.
    Bidirectional,
}

impl EventCategory {
    /// The direction constraint the contract grants each category.
    pub const fn direction(self) -> EventDirection {
        match self {
            EventCategory::Notify | EventCategory::Snapshot => EventDirection::EngineToUi,
            EventCategory::Request => EventDirection::Bidirectional,
            EventCategory::Control => EventDirection::UiToEngine,
        }
    }
}

/// Whether an engine event becomes a UI entry at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiVisibility {
    /// Projects to `EntryOp`s; visible to every UI.
    Projected,
    /// Crosses the transport for SDK/host accounting but never projects to an
    /// entry.
    ///
    /// Canonical case: `llm.step.end`. The TS host parses it for usage/timing
    /// and discards it — nothing renders. In the Rust UI contract it is
    /// transport-only by declaration: the projection layer must not turn it
    /// into an `EntryOp`.
    TransportOnly,
}

/// Wire names of transport-only events (see [`UiVisibility::TransportOnly`]).
///
/// Every event the engine emits is in exactly one of three places: projected to
/// an entry, listed here as transport-only, or **unaccounted** — and the third
/// case is the one that hides bugs, because an unhandled event falls through a
/// `_ => Vec::new()` arm and looks exactly like an event with nothing to say.
///
/// `llm.step.begin` / `llm.step.end` carry step and usage accounting for the
/// host and render nothing. `tool.progress` updates a card the host may or may
/// not draw; it never creates one. `event.config.changed` /
/// `event.session.status_changed` / `event.session.work_changed` are status
/// feeds for chrome the transcript does not own. `goal.budget.limit_reached`
/// and `cron.fired` are announcements a host may surface; neither is a
/// transcript entry. `session.meta.updated` feeds the title bar and the model
/// badge, which are chrome rather than transcript.
///
/// Deliberately **not** listed: `subagent.*`. Those are entries (a member has
/// its own card), and the transcript projector folds them; only the `EntryOp`
/// projection does not yet, which is a gap rather than a classification.
pub const TRANSPORT_ONLY_EVENTS: &[&str] = &[
    "llm.step.begin",
    "llm.step.end",
    "tool.progress",
    "event.config.changed",
    "event.session.status_changed",
    "event.session.work_changed",
    "goal.budget.limit_reached",
    "cron.fired",
    "session.meta.updated",
];

/// How an engine event relates to the projected entry stream.
///
/// One function, so "what does the UI show for this event" has one answer
/// instead of an implicit one spread across two projectors' match arms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Projection {
    /// Becomes or updates an entry.
    Entry,
    /// Crosses the transport, renders nothing (see [`TRANSPORT_ONLY_EVENTS`]).
    TransportOnly,
    /// Not classified yet. Callers should treat it as transport-only *and*
    /// report it: silence here is the failure mode this enum exists to stop.
    Unaccounted,
}

impl Projection {
    /// Classify a wire event name.
    pub fn of(event_name: &str) -> Self {
        if TRANSPORT_ONLY_EVENTS.contains(&event_name) {
            return Self::TransportOnly;
        }
        if PROJECTED_EVENTS.contains(&event_name) {
            return Self::Entry;
        }
        Self::Unaccounted
    }
}

/// Wire names that become entries. Kept beside [`TRANSPORT_ONLY_EVENTS`] so the
/// two lists can be checked against the engine's own vocabulary rather than
/// against each other's omissions.
pub const PROJECTED_EVENTS: &[&str] = &[
    "turn.started",
    "turn.ended",
    "assistant.delta",
    "thinking.delta",
    "stream.segment",
    "llm.delta",
    "tool.call.started",
    "tool.call.delta",
    "tool.call.completed",
    "tool.call.failed",
    "tool.native",
    "subagent.spawned",
    "subagent.completed",
    "subagent.failed",
];

/// Which logical stream a batched segment extends.
///
/// Shared vocabulary for the batched-delta event shape (the reserved
/// `stream.segment` wire variant): the engine stamps it, the projection maps
/// it to an [`crate::entry_op::EntryOp::Append`] on the matching
/// [`crate::entry_id::EntryId`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "stream", rename_all = "snake_case")]
pub enum StreamKind {
    /// Assistant body text.
    Assistant,
    /// Thinking stream.
    Thinking,
    /// One tool call's argument/result stream.
    ToolCall { tool_call_id: String },
}

/// The logical stream one raw LLM delta part belongs to.
///
/// Providers spell "reasoning" four different ways on the wire; the raw
/// `llm.delta` part is the transport's shape, not a semantic one, and both
/// projections need the same answer for what a part *is* before they can
/// decide what to do with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaPart {
    /// Visible assistant body text.
    Assistant,
    /// Reasoning / thinking text.
    Thinking,
    /// Nothing this layer renders (tool arguments and provider bookkeeping
    /// travel other events).
    Ignored,
}

/// Classify one raw `llm.delta` part (the transport's `{ "type": ..., ... }`
/// shape, deserialized to a JSON value).
///
/// One function so "is this text or thinking" has one answer: the two
/// projectors previously disagreed by omission — the entry projector expanded
/// four provider aliases, the transcript projector never saw the event at all,
/// and nothing recorded which of those was a decision.
pub fn classify_delta_part(part: &serde_json::Value) -> DeltaPart {
    let ty = part.get("type").and_then(serde_json::Value::as_str);
    if let Some(text) = part.get("text").and_then(serde_json::Value::as_str) {
        if ty == Some("think") || ty == Some("thinking") {
            // A thinking part that spells its payload `text` (some providers
            // do): the type decides, not the field name.
            return DeltaPart::Thinking;
        }
        let _ = text;
        return DeltaPart::Assistant;
    }
    // The reasoning aliases, in the order the entry projector happened to
    // check them. `type: "think"` with a matching field is the same answer.
    let reasoning = ["think", "thinking", "reasoning", "reasoning_content"]
        .iter()
        .find_map(|key| part.get(*key).and_then(serde_json::Value::as_str));
    if reasoning.is_some() {
        return DeltaPart::Thinking;
    }
    DeltaPart::Ignored
}

/// Extract the text of a part classified as [`DeltaPart::Assistant`] or
/// [`DeltaPart::Thinking`]; `None` for [`DeltaPart::Ignored`].
pub fn delta_part_text(part: &serde_json::Value) -> Option<(&'static str, &str)> {
    match classify_delta_part(part) {
        DeltaPart::Assistant => Some(("assistant", part.get("text")?.as_str()?)),
        DeltaPart::Thinking => {
            let text = ["think", "thinking", "reasoning", "reasoning_content"]
                .iter()
                .find_map(|key| part.get(*key).and_then(serde_json::Value::as_str))?;
            Some(("thinking", text))
        }
        DeltaPart::Ignored => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn categories_have_their_contract_direction() {
        assert_eq!(
            EventCategory::Notify.direction(),
            EventDirection::EngineToUi
        );
        assert_eq!(
            EventCategory::Snapshot.direction(),
            EventDirection::EngineToUi
        );
        assert_eq!(
            EventCategory::Request.direction(),
            EventDirection::Bidirectional
        );
        assert_eq!(
            EventCategory::Control.direction(),
            EventDirection::UiToEngine
        );
    }

    /// Every event the engine emits is classified as either an entry or
    /// transport-only. An unclassified event is the failure this table exists
    /// to prevent: it falls through a projector's catch-all arm and is
    /// indistinguishable from an event that genuinely has nothing to render.
    #[test]
    fn every_engine_event_is_classified() {
        // The engine's own vocabulary. Kept here rather than derived because
        // deriving it would mean this crate depends on the engine, which the
        // layer order forbids; the gate in `scripts/check-event-coverage.mjs`
        // is what keeps this list honest against `events/types.rs`.
        const ENGINE_EVENTS: &[&str] = &[
            "turn.started",
            "turn.ended",
            "assistant.delta",
            "thinking.delta",
            "stream.segment",
            "llm.delta",
            "llm.step.begin",
            "llm.step.end",
            "tool.call.started",
            "tool.call.delta",
            "tool.call.completed",
            "tool.call.failed",
            "tool.native",
            "tool.progress",
            "subagent.spawned",
            "subagent.completed",
            "subagent.failed",
            "goal.budget.limit_reached",
            "cron.fired",
            "event.config.changed",
            "event.session.status_changed",
            "event.session.work_changed",
            "session.meta.updated",
        ];
        let mut unaccounted = Vec::new();
        for name in ENGINE_EVENTS {
            if Projection::of(name) == Projection::Unaccounted {
                unaccounted.push(*name);
            }
        }
        assert!(
            unaccounted.is_empty(),
            "these engine events have no projection classification: {unaccounted:?}"
        );
    }

    /// The two lists are disjoint: an event cannot both create an entry and be
    /// transport-only.
    #[test]
    fn entry_and_transport_only_lists_do_not_overlap() {
        for name in TRANSPORT_ONLY_EVENTS {
            assert!(!PROJECTED_EVENTS.contains(name), "{name} is in both lists");
        }
    }

    /// The raw transport delta shapes providers actually send. The four
    /// reasoning aliases are the ones the entry projector happened to spell
    /// out; they are pinned here so a shared answer cannot quietly narrow.
    #[test]
    fn raw_llm_delta_parts_are_classified_the_same_everywhere() {
        use serde_json::json;

        assert_eq!(
            classify_delta_part(&json!({"type":"text","text":"hi"})),
            DeltaPart::Assistant
        );
        assert_eq!(
            classify_delta_part(&json!({"type":"think","think":"hmm"})),
            DeltaPart::Thinking
        );
        // A thinking part whose payload field is `text`: type wins.
        assert_eq!(
            classify_delta_part(&json!({"type":"think","text":"hmm"})),
            DeltaPart::Thinking
        );
        for alias in ["think", "thinking", "reasoning", "reasoning_content"] {
            let part = json!({ alias: "hmm" });
            assert_eq!(classify_delta_part(&part), DeltaPart::Thinking, "{alias}");
        }
        // Tool arguments and bookkeeping are not transcript text.
        assert_eq!(
            classify_delta_part(&json!({"type":"tool_use","id":"x"})),
            DeltaPart::Ignored
        );
        assert_eq!(classify_delta_part(&json!({})), DeltaPart::Ignored);

        // The text extractor agrees with the classifier, including which
        // field the text came from.
        assert_eq!(
            delta_part_text(&json!({"type":"text","text":"hi"})),
            Some(("assistant", "hi"))
        );
        assert_eq!(
            delta_part_text(&json!({"reasoning_content":"hmm"})),
            Some(("thinking", "hmm"))
        );
        assert_eq!(delta_part_text(&json!({"type":"tool_use"})), None);
    }

    /// A name neither list mentions is reported as unaccounted rather than
    /// silently defaulting to one of them.
    #[test]
    fn an_unknown_event_is_unaccounted_not_silently_filtered() {
        assert_eq!(Projection::of("some.future.event"), Projection::Unaccounted);
        assert_eq!(Projection::of("assistant.delta"), Projection::Entry);
        assert_eq!(Projection::of("llm.step.end"), Projection::TransportOnly);
    }
}
