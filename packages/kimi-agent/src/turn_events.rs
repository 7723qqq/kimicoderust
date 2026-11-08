//! Engine → host turn lifecycle events (M1b: `host/turn_event`).
//!
//! Each variant serializes to exactly the payload of the matching v2 `Event2`
//! class, minus `agentId` — the host owns agent identity and stamps it in.
//! `turn.prompt` / `turn.cancel` / `turn.ended` are durable (the host appends
//! them to the log and folds them into `turnKey`); `turn.started` is
//! observable-only.
//!
//! Display fields (`prompt`, `promptAttachments`) are deliberately absent from
//! `turn.started`: v2 derives them from the prompt's `input` + `origin` via
//! `turnPromptText` / `turnPromptAttachments`, and the engine has no view of
//! skill-activation block packing or daemon file ids. The host derives them
//! from the same `input` it received, so the two engines cannot disagree.

use serde::{Deserialize, Serialize};

/// Why a turn ended. Mirrors v2 `TurnEndReason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnEndReason {
    Completed,
    Cancelled,
    Failed,
    Blocked,
}

/// Which side of the queue a cancellation hit. Mirrors v2 `TurnCancel.target`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnCancelTarget {
    Active,
    Queued,
}

/// Why a turn was cancelled. Mirrors v2 `TurnCancel.reason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnCancelReason {
    UserCancelled,
    Aborted,
}

/// One engine → host turn lifecycle event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum TurnEvent {
    /// A turn has been claimed from the queue and is about to run. Durable:
    /// the host appends `turn.prompt` and advances its turn clock.
    #[serde(rename = "turn.prompt")]
    Prompt {
        #[serde(rename = "turnId")]
        turn_id: u64,
        /// The prompt id the engine minted at admission (v2 #4627
        /// `TurnPrompt.promptId`). A host that submitted a prompt as a
        /// potential steer pairs its optimistic message with this echo.
        #[serde(rename = "promptId", skip_serializing_if = "Option::is_none")]
        prompt_id: Option<String>,
        /// The prompt as a `ContentPart[]` JSON array, echoed back verbatim
        /// from what the host handed the engine.
        input: serde_json::Value,
        /// `PromptOrigin` JSON, echoed back verbatim. The host maps it to its
        /// own origin type — the engine does not model origin variants.
        origin: serde_json::Value,
    },
    /// A turn's first step has begun. Observable only.
    #[serde(rename = "turn.started")]
    Started {
        #[serde(rename = "turnId")]
        turn_id: u64,
        /// See `Prompt::prompt_id`: a steer that was queued behind another
        /// turn keeps its id until its own turn starts.
        #[serde(rename = "promptId", skip_serializing_if = "Option::is_none")]
        prompt_id: Option<String>,
        origin: serde_json::Value,
    },
    /// A steered message has been consumed into the running turn's context
    /// (v2 #4627 `TurnSteer`, emitted when a steer nudge is consumed rather
    /// than when it is submitted). Durable.
    #[serde(rename = "turn.steer")]
    Steer {
        /// The turn the message joined. Absent when the drain site cannot
        /// name it (v2 declares `turnId` optional).
        #[serde(rename = "turnId", skip_serializing_if = "Option::is_none")]
        turn_id: Option<u64>,
        /// The steered message as the model receives it, projected to
        /// `ContentPart[]`.
        input: serde_json::Value,
        origin: serde_json::Value,
        /// The steered message's id (v2 `TurnSteer.messageId`).
        #[serde(rename = "messageId", skip_serializing_if = "Option::is_none")]
        message_id: Option<String>,
        /// The host prompt ids this steer carries (v2 `TurnSteer.promptIds`).
        /// Always non-empty on an engine-emitted steer: the drain only reports
        /// messages that carry a prompt id, so a host never needs a text-based
        /// fallback to match a delivered steer.
        #[serde(rename = "promptIds", default, skip_serializing_if = "Vec::is_empty")]
        prompt_ids: Vec<String>,
    },
    /// An active or queued turn was cancelled. Durable. `turn_id = None`
    /// means "cancel whatever is active".
    #[serde(rename = "turn.cancel")]
    Cancel {
        #[serde(rename = "turnId", skip_serializing_if = "Option::is_none")]
        turn_id: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        target: Option<TurnCancelTarget>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<TurnCancelReason>,
    },
    /// A turn has finished. Durable.
    #[serde(rename = "turn.ended")]
    Ended {
        #[serde(rename = "turnId")]
        turn_id: u64,
        reason: TurnEndReason,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<serde_json::Value>,
        #[serde(rename = "durationMs", skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn round_trip(event: &TurnEvent, expected: serde_json::Value) {
        let serialized = serde_json::to_value(event).unwrap();
        assert_eq!(serialized, expected, "wire shape drifted");
        let parsed: TurnEvent = serde_json::from_value(serialized).unwrap();
        assert_eq!(&parsed, event, "round trip lost information");
    }

    #[test]
    fn turn_prompt_wire_shape() {
        round_trip(
            &TurnEvent::Prompt {
                turn_id: 7,
                prompt_id: Some("prompt-7-abc".to_string()),
                input: json!([{"type": "text", "text": "hello"}]),
                origin: json!({"kind": "user"}),
            },
            json!({
                "type": "turn.prompt",
                "turnId": 7,
                "promptId": "prompt-7-abc",
                "input": [{"type": "text", "text": "hello"}],
                "origin": {"kind": "user"},
            }),
        );
        // A turn the engine minted for itself (background notification) has no
        // host prompt id; the field is omitted rather than sent empty.
        round_trip(
            &TurnEvent::Prompt {
                turn_id: 8,
                prompt_id: None,
                input: json!([]),
                origin: json!({"kind": "task"}),
            },
            json!({
                "type": "turn.prompt",
                "turnId": 8,
                "input": [],
                "origin": {"kind": "task"},
            }),
        );
    }

    #[test]
    fn turn_started_wire_shape() {
        round_trip(
            &TurnEvent::Started {
                turn_id: 7,
                prompt_id: Some("prompt-7-abc".to_string()),
                origin: json!({"kind": "user"}),
            },
            json!({
                "type": "turn.started",
                "turnId": 7,
                "promptId": "prompt-7-abc",
                "origin": {"kind": "user"},
            }),
        );
    }

    #[test]
    fn turn_steer_wire_shape() {
        round_trip(
            &TurnEvent::Steer {
                turn_id: Some(7),
                input: json!([{"type": "text", "text": "steer me"}]),
                origin: json!({"kind": "user"}),
                message_id: Some("prompt-8-def".to_string()),
                prompt_ids: vec!["prompt-8-def".to_string()],
            },
            json!({
                "type": "turn.steer",
                "turnId": 7,
                "input": [{"type": "text", "text": "steer me"}],
                "origin": {"kind": "user"},
                "messageId": "prompt-8-def",
                "promptIds": ["prompt-8-def"],
            }),
        );
        // The drain site cannot name the turn; the optional fields disappear.
        round_trip(
            &TurnEvent::Steer {
                turn_id: None,
                input: json!([{"type": "text", "text": "steer"}]),
                origin: json!({"kind": "user"}),
                message_id: None,
                prompt_ids: Vec::new(),
            },
            json!({
                "type": "turn.steer",
                "input": [{"type": "text", "text": "steer"}],
                "origin": {"kind": "user"},
            }),
        );
    }

    #[test]
    fn turn_cancel_wire_shapes() {
        round_trip(
            &TurnEvent::Cancel {
                turn_id: Some(3),
                target: Some(TurnCancelTarget::Queued),
                reason: Some(TurnCancelReason::UserCancelled),
            },
            json!({
                "type": "turn.cancel",
                "turnId": 3,
                "target": "queued",
                "reason": "user_cancelled",
            }),
        );
        round_trip(
            &TurnEvent::Cancel {
                turn_id: None,
                target: Some(TurnCancelTarget::Active),
                reason: Some(TurnCancelReason::Aborted),
            },
            json!({
                "type": "turn.cancel",
                "target": "active",
                "reason": "aborted",
            }),
        );
    }

    #[test]
    fn turn_ended_wire_shapes() {
        round_trip(
            &TurnEvent::Ended {
                turn_id: 7,
                reason: TurnEndReason::Completed,
                error: None,
                duration_ms: Some(1234),
            },
            json!({
                "type": "turn.ended",
                "turnId": 7,
                "reason": "completed",
                "durationMs": 1234,
            }),
        );
        round_trip(
            &TurnEvent::Ended {
                turn_id: 7,
                reason: TurnEndReason::Failed,
                error: Some(json!({"code": "provider_error"})),
                duration_ms: None,
            },
            json!({
                "type": "turn.ended",
                "turnId": 7,
                "reason": "failed",
                "error": {"code": "provider_error"},
            }),
        );
    }

    #[test]
    fn every_variant_carries_a_dotted_type_tag() {
        let events = [
            TurnEvent::Prompt {
                turn_id: 0,
                prompt_id: None,
                input: json!([]),
                origin: json!(null),
            },
            TurnEvent::Started {
                turn_id: 0,
                prompt_id: None,
                origin: json!(null),
            },
            TurnEvent::Steer {
                turn_id: None,
                input: json!([]),
                origin: json!(null),
                message_id: None,
                prompt_ids: Vec::new(),
            },
            TurnEvent::Cancel {
                turn_id: None,
                target: None,
                reason: None,
            },
            TurnEvent::Ended {
                turn_id: 0,
                reason: TurnEndReason::Blocked,
                error: None,
                duration_ms: None,
            },
        ];
        let types: Vec<String> = events
            .iter()
            .map(|e| {
                serde_json::to_value(e).unwrap()["type"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(
            types,
            vec![
                "turn.prompt",
                "turn.started",
                "turn.steer",
                "turn.cancel",
                "turn.ended",
            ]
        );
    }
}
