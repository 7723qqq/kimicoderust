
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnEndReason {
    Completed,
    Cancelled,
    Failed,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnCancelTarget {
    Active,
    Queued,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnCancelReason {
    UserCancelled,
    Aborted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnInterruptReason {
    UserCancelled,
    Aborted,
    MaxSteps,
    Error,
    Filtered,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum TurnEvent {
    #[serde(rename = "turn.prompt")]
    Prompt {
        #[serde(rename = "turnId")]
        turn_id: u64,
        #[serde(rename = "promptId", skip_serializing_if = "Option::is_none")]
        prompt_id: Option<String>,
        input: serde_json::Value,
        origin: serde_json::Value,
    },
    #[serde(rename = "turn.started")]
    Started {
        #[serde(rename = "turnId")]
        turn_id: u64,
        #[serde(rename = "promptId", skip_serializing_if = "Option::is_none")]
        prompt_id: Option<String>,
        origin: serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
        #[serde(rename = "promptAttachments", skip_serializing_if = "Option::is_none")]
        prompt_attachments: Option<Vec<serde_json::Value>>,
    },
    #[serde(rename = "turn.steer")]
    Steer {
        #[serde(rename = "turnId", skip_serializing_if = "Option::is_none")]
        turn_id: Option<u64>,
        input: serde_json::Value,
        origin: serde_json::Value,
        #[serde(rename = "messageId", skip_serializing_if = "Option::is_none")]
        message_id: Option<String>,
        #[serde(rename = "promptIds", default, skip_serializing_if = "Vec::is_empty")]
        prompt_ids: Vec<String>,
    },
    #[serde(rename = "turn.cancel")]
    Cancel {
        #[serde(rename = "turnId", skip_serializing_if = "Option::is_none")]
        turn_id: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        target: Option<TurnCancelTarget>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<TurnCancelReason>,
    },
    #[serde(rename = "turn.ended")]
    Ended {
        #[serde(rename = "turnId")]
        turn_id: u64,
        reason: TurnEndReason,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<serde_json::Value>,
        #[serde(rename = "durationMs", skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        #[serde(rename = "interruptReason", skip_serializing_if = "Option::is_none")]
        interrupt_reason: Option<TurnInterruptReason>,
        #[serde(rename = "traceId", skip_serializing_if = "Option::is_none")]
        trace_id: Option<String>,
        #[serde(rename = "stopReason", skip_serializing_if = "Option::is_none")]
        stop_reason: Option<String>,
    },
}

fn display_prompt(input: &serde_json::Value, origin: &serde_json::Value) -> Option<String> {
    if !is_displayable_prompt_origin(origin) {
        return None;
    }
    let parts = input.as_array()?;
    let bundled = if origin["kind"].as_str() == Some("user") {
        origin["skillActivations"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(0)
    } else {
        0
    };
    let already_annotated = parts.iter().any(|part| is_skill_activation_part(part));
    let mut annotated = 0usize;
    let text = parts
        .iter()
        .filter(|part| part["type"] == "text")
        .filter(|part| {
            let source = &part["meta"]["source"];
            // v2 filters hook injections out before `turnPromptText`, so they
            // neither show up nor consume a bundled-activation slot.
            if source == "user prompt submit hook" {
                return false;
            }
            let bundled_part = !already_annotated && source.is_null() && annotated < bundled && {
                annotated += 1;
                true
            };
            !bundled_part && !is_skill_activation_part(part)
        })
        .filter_map(|part| part["text"].as_str())
        .collect::<String>();
    (!text.is_empty()).then_some(text)
}

fn is_skill_activation_part(part: &serde_json::Value) -> bool {
    part["type"] == "text" && part["meta"]["source"] == "skill activation"
}

pub fn strip_bundled_skill_blocks(
    input: &serde_json::Value,
    origin: &serde_json::Value,
) -> serde_json::Value {
    let Some(parts) = input.as_array() else {
        return input.clone();
    };
    let bundled = if origin["kind"].as_str() == Some("user") {
        origin["skillActivations"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(0)
    } else {
        0
    };
    let already_annotated = parts.iter().any(is_skill_activation_part);
    let mut annotated = 0usize;
    let kept: Vec<serde_json::Value> = parts
        .iter()
        .filter(|part| {
            if !already_annotated
                && part["type"] == "text"
                && part["meta"]["source"].is_null()
                && annotated < bundled
            {
                annotated += 1;
                return false;
            }
            !is_skill_activation_part(part)
        })
        .cloned()
        .collect();
    serde_json::Value::Array(kept)
}

fn prompt_attachments(
    input: &serde_json::Value,
    origin: &serde_json::Value,
) -> Option<Vec<serde_json::Value>> {
    let file_id = |url: &str, id: Option<&str>| -> Option<String> {
        let rest = url.strip_prefix("kimi-file://")?;
        let parsed = rest.split('?').next().unwrap_or(rest);
        if parsed.is_empty() {
            return None;
        }
        match id {
            None => Some(parsed.to_string()),
            Some(id) => (parsed == id).then(|| parsed.to_string()),
        }
    };
    let mut attachments = Vec::new();
    for part in input.as_array().into_iter().flatten() {
        let (kind, media) = match part["type"].as_str() {
            Some("image_url") => ("image", &part["imageUrl"]),
            Some("video_url") => ("video", &part["videoUrl"]),
            Some("audio_url") => ("audio", &part["audioUrl"]),
            _ => continue,
        };
        let Some(url) = media["url"].as_str() else {
            continue;
        };
        let Some(file_id) = file_id(url, media["id"].as_str()) else {
            continue;
        };
        let mut attachment = serde_json::json!({ "kind": kind, "fileId": file_id });
        if let Some(name) = media["name"].as_str() {
            attachment["name"] = serde_json::Value::String(name.to_string());
        }
        attachments.push(attachment);
    }
    let kind = origin["kind"].as_str();
    if kind == Some("user") || kind == Some("skill_activation") {
        for attachment in origin["attachments"].as_array().into_iter().flatten() {
            let mut file = attachment.clone();
            file["kind"] = serde_json::Value::String("file".to_string());
            attachments.push(file);
        }
    }
    (!attachments.is_empty()).then_some(attachments)
}

fn is_displayable_prompt_origin(origin: &serde_json::Value) -> bool {
    match origin["kind"].as_str() {
        Some("user") => true,
        Some("system_trigger") => origin["name"].as_str() == Some("subagent"),
        Some("skill_activation") | Some("plugin_command") => {
            origin["trigger"].as_str() == Some("user-slash")
        }
        _ => false,
    }
}

pub fn started_display(
    input: &serde_json::Value,
    origin: &serde_json::Value,
) -> (Option<String>, Option<Vec<serde_json::Value>>) {
    (
        display_prompt(input, origin),
        prompt_attachments(input, origin),
    )
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
    fn bundled_skill_blocks_are_hidden_from_the_display_prompt() {
        // node-sdk prepends one rendered block per bundled activation and
        // lists them in the origin without marking the parts; the first N
        // unannotated text parts are those blocks (v2 `annotateBundledSkillParts`).
        let input = json!([
            {"type": "text", "text": "<skill body>"},
            {"type": "text", "text": "actual user text"},
            {"type": "image_url", "imageUrl": {"url": "kimi-file://img-1"}},
        ]);
        let origin = json!({
            "kind": "user",
            "skillActivations": [{"activationId": "act-1", "skillName": "demo"}],
        });
        let (prompt, attachments) = started_display(&input, &origin);
        assert_eq!(prompt.as_deref(), Some("actual user text"));
        assert_eq!(
            attachments.unwrap(),
            vec![json!({"kind": "image", "fileId": "img-1"})]
        );
    }

    #[test]
    fn an_annotated_part_disables_positional_bundling() {
        // v2 returns the content unchanged as soon as any part already says it
        // is a skill activation; the positional guess would double-hide a
        // caller's own text otherwise.
        let input = json!([
            {"type": "text", "text": "visible", "meta": {"source": "skill activation"}},
            {"type": "text", "text": "typed"},
        ]);
        let origin = json!({"kind": "user", "skillActivations": [{"activationId": "act-1"}]});
        let (prompt, _) = started_display(&input, &origin);
        assert_eq!(prompt.as_deref(), Some("typed"));
    }

    #[test]
    fn hook_parts_neither_display_nor_consume_a_bundled_slot() {
        let input = json!([
            {"type": "text", "text": "hook text", "meta": {"source": "user prompt submit hook"}},
            {"type": "text", "text": "<skill body>"},
            {"type": "text", "text": "typed"},
        ]);
        let origin = json!({"kind": "user", "skillActivations": [{"activationId": "act-1"}]});
        let (prompt, _) = started_display(&input, &origin);
        assert_eq!(prompt.as_deref(), Some("typed"));
    }

    #[test]
    fn only_displayable_origins_carry_a_prompt_text() {
        let input = json!([{"type": "text", "text": "typed"}]);
        for origin in [
            json!({"kind": "system_trigger", "name": "subagent"}),
            json!({"kind": "skill_activation", "trigger": "user-slash"}),
            json!({"kind": "plugin_command", "trigger": "user-slash"}),
        ] {
            let (prompt, _) = started_display(&input, &origin);
            assert_eq!(prompt.as_deref(), Some("typed"), "origin {origin}");
        }
        for origin in [
            json!({"kind": "system_trigger", "name": "cron"}),
            json!({"kind": "skill_activation", "trigger": "model"}),
            json!({"kind": "task"}),
        ] {
            let (prompt, _) = started_display(&input, &origin);
            assert_eq!(prompt, None, "origin {origin}");
        }
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
                prompt: None,
                prompt_attachments: None,
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
                interrupt_reason: None,
                trace_id: Some("turn-7".to_string()),
                stop_reason: None,
            },
            json!({
                "type": "turn.ended",
                "turnId": 7,
                "reason": "completed",
                "durationMs": 1234,
                "traceId": "turn-7",
            }),
        );
        // v2's only forced stop reason: the tool-dedupe repeat breaker
        // (`REPEAT_BREAKER_STOP_REASON`).
        round_trip(
            &TurnEvent::Ended {
                turn_id: 8,
                reason: TurnEndReason::Completed,
                error: None,
                duration_ms: Some(1),
                interrupt_reason: None,
                trace_id: None,
                stop_reason: Some("repeat_breaker".to_string()),
            },
            json!({
                "type": "turn.ended",
                "turnId": 8,
                "reason": "completed",
                "durationMs": 1,
                "stopReason": "repeat_breaker",
            }),
        );
        round_trip(
            &TurnEvent::Ended {
                turn_id: 7,
                reason: TurnEndReason::Failed,
                error: Some(json!({"code": "provider_error"})),
                duration_ms: None,
                interrupt_reason: Some(TurnInterruptReason::Error),
                trace_id: Some("turn-7".to_string()),
                stop_reason: None,
            },
            json!({
                "type": "turn.ended",
                "turnId": 7,
                "reason": "failed",
                "error": {"code": "provider_error"},
                "interruptReason": "error",
                "traceId": "turn-7",
            }),
        );
        // v2 `interruptReasonFor` distinguishes cancellation, the step budget
        // and provider filtering; the field stays absent on a clean stop and
        // on the engine's blocked stop (v2 has no blocked turn outcome).
        round_trip(
            &TurnEvent::Ended {
                turn_id: 7,
                reason: TurnEndReason::Cancelled,
                error: None,
                duration_ms: Some(9),
                interrupt_reason: Some(TurnInterruptReason::Aborted),
                trace_id: None,
                stop_reason: None,
            },
            json!({
                "type": "turn.ended",
                "turnId": 7,
                "reason": "cancelled",
                "durationMs": 9,
                "interruptReason": "aborted",
            }),
        );
        round_trip(
            &TurnEvent::Ended {
                turn_id: 7,
                reason: TurnEndReason::Blocked,
                error: None,
                duration_ms: None,
                interrupt_reason: None,
                trace_id: None,
                stop_reason: None,
            },
            json!({
                "type": "turn.ended",
                "turnId": 7,
                "reason": "blocked",
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
                prompt: None,
                prompt_attachments: None,
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
                interrupt_reason: None,
                trace_id: None,
                stop_reason: None,
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
