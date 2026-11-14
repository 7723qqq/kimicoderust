use std::collections::HashMap;
use std::time::Duration;

use crate::server::hub::SequencedEvent;
use chrono::{SecondsFormat, Utc};
use serde::Serialize;
use serde_json::Value;

pub const WS_PROTOCOL_VERSION: u32 = 2;

pub const DEFAULT_HEARTBEAT: Duration = Duration::from_secs(10);

pub const WS_AUTH_ERROR_CODE: u32 = 40112;

pub const ACK_OK: u32 = 0;

pub fn timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

#[derive(Serialize)]
struct ControlFrame<T> {
    #[serde(rename = "type")]
    kind: &'static str,
    timestamp: String,
    payload: T,
}

#[derive(Serialize)]
struct Capabilities {
    event_batching: bool,
    compression: bool,
}

#[derive(Serialize)]
struct HelloPayload {
    ws_connection_id: String,
    protocol_version: u32,
    heartbeat_ms: u64,
    capabilities: Capabilities,
}

#[derive(Serialize)]
struct AckFrame<T> {
    #[serde(rename = "type")]
    kind: &'static str,
    id: String,
    code: u32,
    msg: String,
    payload: T,
}

#[derive(Serialize)]
struct PingPayload {
    nonce: String,
}

fn control<T: Serialize>(kind: &'static str, payload: T) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(&ControlFrame {
        kind,
        timestamp: timestamp(),
        payload,
    })
}

pub fn server_hello(
    connection_id: &str,
    heartbeat: Duration,
) -> Result<Vec<u8>, serde_json::Error> {
    control(
        "server_hello",
        HelloPayload {
            ws_connection_id: connection_id.to_string(),
            protocol_version: WS_PROTOCOL_VERSION,
            heartbeat_ms: heartbeat.as_millis() as u64,
            capabilities: Capabilities {
                event_batching: false,
                compression: false,
            },
        },
    )
}

pub fn ping(nonce: &str) -> Result<Vec<u8>, serde_json::Error> {
    control(
        "ping",
        PingPayload {
            nonce: nonce.to_string(),
        },
    )
}

pub fn ack(id: &str, code: u32, msg: &str, payload: Value) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(&AckFrame {
        kind: "ack",
        id: id.to_string(),
        code,
        msg: msg.to_string(),
        payload,
    })
}

#[derive(Serialize)]
struct Envelope<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    seq: u64,
    epoch: &'a str,
    session_id: &'a str,
    timestamp: String,
    payload: Value,
}

fn numeric_turn_id(turn_id: &str) -> u64 {
    let tail = turn_id.rsplit_once('-').map_or(turn_id, |(_, tail)| tail);
    let parsed = tail.parse::<u64>();
    if parsed.is_err() {
        tracing::warn!(
            "ws streaming turn id {turn_id:?} has no numeric tail; encoding turnId 0 \
             (a subagent turn id is `subturn-<n>`)"
        );
    }
    parsed.unwrap_or_default()
}

pub fn encode_envelope(
    session_id: &str,
    epoch: &str,
    seq: u64,
    event: &crate::events::EngineEvent,
) -> Result<Vec<u8>, serde_json::Error> {
    let mut payload = serde_json::to_value(event)?;
    let mut kind = event.event_type().to_string();

    if let crate::events::EngineEvent::Custom(value) = event {
        let inner = value.get("type").and_then(Value::as_str).unwrap_or("");
        match inner {
            "terminal_output" => {
                kind = inner.to_string();
                payload = serde_json::json!({ "data": value.get("data") });
            }
            "terminal_exit" => {
                kind = inner.to_string();
                payload = value
                    .get("payload")
                    .cloned()
                    .unwrap_or(serde_json::json!({}));
            }
            _ => {}
        }
    }

    if let crate::events::EngineEvent::LlmDelta {
        agent_id,
        turn_id,
        part,
        ..
    } = event
    {
        if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
            kind = "assistant.delta".into();
            payload = serde_json::json!({
                "agentId": agent_id,
                "turnId": numeric_turn_id(turn_id),
                "delta": text,
            });
        } else if let Some(thinking) = part
            .get("think")
            .or_else(|| part.get("thinking"))
            .or_else(|| part.get("reasoning"))
            .or_else(|| part.get("reasoning_content"))
            .and_then(|v| v.as_str())
        {
            kind = "thinking.delta".into();
            payload = serde_json::json!({
                "agentId": agent_id,
                "turnId": numeric_turn_id(turn_id),
                "delta": thinking,
            });
        }
    } else if let Value::Object(map) = &mut payload {
        map.remove("type");
    }

    serde_json::to_vec(&Envelope {
        kind: &kind,
        seq,
        epoch,
        session_id,
        timestamp: timestamp(),
        payload,
    })
}

pub fn event_envelope(event: &SequencedEvent) -> Result<Vec<u8>, serde_json::Error> {
    encode_envelope(&event.session_id, &event.epoch, event.seq, &event.event)
}

pub fn client_hello_ack(
    accepted_subscriptions: &[String],
    resync_required: &[String],
    cursors: &HashMap<String, Value>,
) -> Value {
    serde_json::json!({
        "accepted_subscriptions": accepted_subscriptions,
        "resync_required": resync_required,
        "cursors": cursors,
    })
}

pub fn subscribe_ack(
    accepted: &[String],
    not_found: &[String],
    resync_required: &[String],
    cursors: &HashMap<String, Value>,
) -> Value {
    serde_json::json!({
        "accepted": accepted,
        "not_found": not_found,
        "resync_required": resync_required,
        "cursors": cursors,
    })
}

pub fn unsubscribe_ack(
    accepted: &[String],
    not_found: &[String],
    resync_required: &[String],
) -> Value {
    serde_json::json!({
        "accepted": accepted,
        "not_found": not_found,
        "resync_required": resync_required,
    })
}

pub fn subscribe_v2_ack(session_id: &str, accepted: bool, cursor_seq: u64) -> Value {
    serde_json::json!({
        "accepted": if accepted { vec![session_id.to_string()] } else { Vec::<String>::new() },
        "not_found": if accepted { Vec::<String>::new() } else { vec![session_id.to_string()] },
        "resync_required": Vec::<String>::new(),
        "cursors": if accepted {
            serde_json::json!({ session_id: { "seq": cursor_seq } })
        } else {
            serde_json::json!({})
        },
    })
}

pub fn unsubscribe_v2_ack(session_id: &str) -> Value {
    serde_json::json!({
        "accepted": [session_id],
        "not_found": Vec::<String>::new(),
        "resync_required": Vec::<String>::new(),
    })
}

pub fn transcript_reset_frame(
    session_id: &str,
    agent_id: &str,
    snapshot: Value,
) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(&serde_json::json!({
        "type": "transcript.reset",
        "session_id": session_id,
        "timestamp": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        "payload": {
            "agent_id": agent_id,
            "snapshot": snapshot,
            "has_more_older": false,
        },
    }))
}

pub fn transcript_ops_frame(
    session_id: &str,
    agent_id: &str,
    ops: Value,
    seq: u64,
) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(&serde_json::json!({
        "type": "transcript.ops",
        "session_id": session_id,
        "timestamp": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        "payload": {
            "agent_id": agent_id,
            "ops": ops,
            "seq": seq,
        },
    }))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorSpec {
    pub seq: u64,
    pub epoch: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Inbound {
    ClientHello {
        id: String,
        token: Option<String>,
        subscriptions: Vec<String>,
        cursors: HashMap<String, CursorSpec>,
    },

    Subscribe {
        id: String,
        session_ids: Vec<String>,
        cursors: HashMap<String, CursorSpec>,
    },

    Unsubscribe {
        id: String,
        session_ids: Vec<String>,
    },

    SubscribeV2 {
        id: String,
        session_id: String,
        transcript: HashMap<String, String>,
        transcript_since: HashMap<String, u64>,
    },

    UnsubscribeV2 {
        id: String,
        session_id: String,
        agent_ids: Vec<String>,
    },

    Abort {
        id: String,
        session_id: String,

        prompt_id: String,
    },

    TerminalAttach {
        id: String,
        session_id: String,
        terminal_id: String,
        since_seq: Option<u64>,
    },

    TerminalDetach {
        id: String,
        session_id: String,
        terminal_id: String,
    },

    TerminalInput {
        id: String,
        session_id: String,
        terminal_id: String,
        data: String,
    },

    TerminalResize {
        id: String,
        session_id: String,
        terminal_id: String,
        cols: u32,
        rows: u32,
    },

    TerminalClose {
        id: String,
        session_id: String,
        terminal_id: String,
    },

    Pong,

    Unknown,
}

fn parse_string_array(payload: Option<&serde_json::Map<String, Value>>, key: &str) -> Vec<String> {
    payload
        .and_then(|p| p.get(key))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn parse_cursors(payload: Option<&serde_json::Map<String, Value>>) -> HashMap<String, CursorSpec> {
    let mut cursors = HashMap::new();
    if let Some(map) = payload
        .and_then(|p| p.get("cursors"))
        .and_then(Value::as_object)
    {
        for (k, v) in map {
            if let Some(obj) = v.as_object() {
                let seq = obj.get("seq").and_then(Value::as_u64).unwrap_or(0);
                let epoch = obj.get("epoch").and_then(Value::as_str).map(str::to_string);
                cursors.insert(k.clone(), CursorSpec { seq, epoch });
            } else if let Some(seq) = v.as_u64() {
                cursors.insert(k.clone(), CursorSpec { seq, epoch: None });
            }
        }
    }
    cursors
}

pub fn parse_inbound(raw: &[u8]) -> Inbound {
    let Ok(Value::Object(frame)) = serde_json::from_slice::<Value>(raw) else {
        return Inbound::Unknown;
    };
    match frame.get("type").and_then(Value::as_str) {
        Some("pong") => Inbound::Pong,
        Some("client_hello") => {
            let id = request_id(&frame);
            let payload = frame.get("payload").and_then(Value::as_object);
            let token = payload
                .and_then(|p| p.get("token"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let subscriptions = parse_string_array(payload, "subscriptions");
            let cursors = parse_cursors(payload);
            Inbound::ClientHello {
                id,
                token,
                subscriptions,
                cursors,
            }
        }
        Some("subscribe") => {
            let id = request_id(&frame);
            let payload = frame.get("payload").and_then(Value::as_object);
            let session_ids = parse_string_array(payload, "session_ids");
            let cursors = parse_cursors(payload);
            Inbound::Subscribe {
                id,
                session_ids,
                cursors,
            }
        }
        Some("unsubscribe") => {
            let id = request_id(&frame);
            let payload = frame.get("payload").and_then(Value::as_object);
            let session_ids = parse_string_array(payload, "session_ids");
            Inbound::Unsubscribe { id, session_ids }
        }
        Some("subscribe_v2") => {
            let id = request_id(&frame);
            let payload = frame.get("payload").and_then(Value::as_object);
            let session_id = payload
                .and_then(|p| p.get("session_id").or_else(|| p.get("sessionId")))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let transcript = payload
                .and_then(|p| p.get("transcript"))
                .and_then(Value::as_object)
                .map(|map| {
                    map.iter()
                        .filter_map(|(key, value)| {
                            value.as_str().map(|grade| (key.clone(), grade.to_string()))
                        })
                        .collect()
                })
                .unwrap_or_default();
            let transcript_since = payload
                .and_then(|p| p.get("transcript_since"))
                .and_then(Value::as_object)
                .map(|map| {
                    map.iter()
                        .filter_map(|(key, value)| value.as_u64().map(|seq| (key.clone(), seq)))
                        .collect()
                })
                .unwrap_or_default();
            Inbound::SubscribeV2 {
                id,
                session_id,
                transcript,
                transcript_since,
            }
        }
        Some("unsubscribe_v2") => {
            let id = request_id(&frame);
            let payload = frame.get("payload").and_then(Value::as_object);
            let session_id = payload
                .and_then(|p| p.get("session_id").or_else(|| p.get("sessionId")))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let agent_ids = parse_string_array(payload, "agent_ids");
            Inbound::UnsubscribeV2 {
                id,
                session_id,
                agent_ids,
            }
        }
        Some("abort") => {
            let id = request_id(&frame);
            let payload = frame.get("payload").and_then(Value::as_object);
            let session_id = payload
                .and_then(|p| p.get("session_id").or_else(|| p.get("sessionId")))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let prompt_id = payload
                .and_then(|p| p.get("prompt_id").or_else(|| p.get("promptId")))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            Inbound::Abort {
                id,
                session_id,
                prompt_id,
            }
        }
        Some("terminal_attach") => {
            let id = request_id(&frame);
            let payload = frame.get("payload").and_then(Value::as_object);
            let session_id = payload
                .and_then(|p| p.get("session_id").or_else(|| p.get("sessionId")))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let terminal_id = payload
                .and_then(|p| p.get("terminal_id").or_else(|| p.get("terminalId")))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let since_seq = payload
                .and_then(|p| p.get("since_seq").or_else(|| p.get("sinceSeq")))
                .and_then(Value::as_u64);
            Inbound::TerminalAttach {
                id,
                session_id,
                terminal_id,
                since_seq,
            }
        }
        Some("terminal_detach") => {
            let id = request_id(&frame);
            let payload = frame.get("payload").and_then(Value::as_object);
            let session_id = payload
                .and_then(|p| p.get("session_id").or_else(|| p.get("sessionId")))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let terminal_id = payload
                .and_then(|p| p.get("terminal_id").or_else(|| p.get("terminalId")))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            Inbound::TerminalDetach {
                id,
                session_id,
                terminal_id,
            }
        }
        Some("terminal_input") => {
            let id = request_id(&frame);
            let payload = frame.get("payload").and_then(Value::as_object);
            let session_id = payload
                .and_then(|p| p.get("session_id").or_else(|| p.get("sessionId")))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let terminal_id = payload
                .and_then(|p| p.get("terminal_id").or_else(|| p.get("terminalId")))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let data = payload
                .and_then(|p| p.get("data"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            Inbound::TerminalInput {
                id,
                session_id,
                terminal_id,
                data,
            }
        }
        Some("terminal_resize") => {
            let id = request_id(&frame);
            let payload = frame.get("payload").and_then(Value::as_object);
            let session_id = payload
                .and_then(|p| p.get("session_id").or_else(|| p.get("sessionId")))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let terminal_id = payload
                .and_then(|p| p.get("terminal_id").or_else(|| p.get("terminalId")))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let cols = payload
                .and_then(|p| p.get("cols"))
                .and_then(Value::as_u64)
                .unwrap_or(80) as u32;
            let rows = payload
                .and_then(|p| p.get("rows"))
                .and_then(Value::as_u64)
                .unwrap_or(24) as u32;
            Inbound::TerminalResize {
                id,
                session_id,
                terminal_id,
                cols,
                rows,
            }
        }
        Some("terminal_close") => {
            let id = request_id(&frame);
            let payload = frame.get("payload").and_then(Value::as_object);
            let session_id = payload
                .and_then(|p| p.get("session_id").or_else(|| p.get("sessionId")))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let terminal_id = payload
                .and_then(|p| p.get("terminal_id").or_else(|| p.get("terminalId")))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            Inbound::TerminalClose {
                id,
                session_id,
                terminal_id,
            }
        }
        Some(_) => Inbound::Unknown,
        None => Inbound::Unknown,
    }
}

pub fn error_frame(code: i64, msg: &str, fatal: bool, request_id: Option<&str>) -> Value {
    serde_json::json!({
        "type": "error",
        "timestamp": timestamp(),
        "payload": {
            "code": code,
            "msg": msg,
            "fatal": fatal,
            "request_id": request_id,
        }
    })
}

fn request_id(frame: &serde_json::Map<String, Value>) -> String {
    frame
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn server_hello_carries_kap_server_field_names_and_honest_values() {
        let frame: Value =
            serde_json::from_slice(&server_hello("ws-1", DEFAULT_HEARTBEAT).unwrap()).unwrap();

        assert_eq!(frame["type"], "server_hello");
        assert_eq!(frame["payload"]["ws_connection_id"], "ws-1");
        assert_eq!(frame["payload"]["protocol_version"], 2);
        assert_eq!(frame["payload"]["heartbeat_ms"], 10_000);
        assert_eq!(frame["payload"]["capabilities"]["event_batching"], false);
        assert_eq!(frame["payload"]["capabilities"]["compression"], false);

        assert!(frame["payload"].get("max_event_buffer_size").is_none());
    }

    #[test]
    fn an_ack_has_the_same_field_order_as_kap_server_builds_it() {
        let bytes = ack("req-9", WS_AUTH_ERROR_CODE, "unauthorized", Value::Null).unwrap();

        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            r#"{"type":"ack","id":"req-9","code":40112,"msg":"unauthorized","payload":null}"#
        );
    }

    #[test]
    fn timestamps_match_the_shape_of_date_toisostring() {
        let stamp = timestamp();
        assert_eq!(stamp.len(), 24, "{stamp}");
        assert!(stamp.ends_with('Z'), "{stamp}");
        assert_eq!(stamp.matches(':').count(), 2, "{stamp}");
        assert_eq!(&stamp[19..20], ".", "{stamp}");
    }

    #[test]
    fn a_ping_carries_a_nonce() {
        let frame: Value = serde_json::from_slice(&ping("n-1").unwrap()).unwrap();
        assert_eq!(frame["type"], "ping");
        assert_eq!(frame["payload"]["nonce"], "n-1");
    }

    #[test]
    fn a_lane_event_becomes_kap_servers_envelope() {
        use crate::events::EngineEvent;

        let event = SequencedEvent::new(
            std::sync::Arc::from("sess-1"),
            std::sync::Arc::from("epoch-2"),
            7,
            EngineEvent::LlmStepBegin {
                agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
                turn_id: "turn-9".into(),
                step: 3,
            },
        );
        let frame: Value = serde_json::from_slice(&event_envelope(&event).unwrap()).unwrap();

        assert_eq!(frame["type"], "llm.step.begin");
        assert_eq!(frame["seq"], 7);
        assert_eq!(frame["epoch"], "epoch-2");
        assert_eq!(frame["session_id"], "sess-1");
        assert_eq!(frame["payload"]["turn_id"], "turn-9");
        assert_eq!(frame["payload"]["step"], 3);
        assert!(
            frame["payload"].get("type").is_none(),
            "the event name must not appear twice: {frame}"
        );
    }

    #[test]
    fn streaming_delta_events_map_to_standard_frontend_formats() {
        use crate::events::EngineEvent;

        let event_native = SequencedEvent::new(
            std::sync::Arc::from("sess-delta"),
            std::sync::Arc::from("epoch-1"),
            1,
            EngineEvent::AssistantDelta {
                agent_id: "main".into(),
                turn_id: "turn-2".into(),
                delta: "Hello world".into(),
                block: None,
            },
        );
        let frame1: Value =
            serde_json::from_slice(&event_envelope(&event_native).unwrap()).unwrap();
        assert_eq!(frame1["type"], "assistant.delta");
        assert_eq!(frame1["payload"]["delta"], "Hello world");
        assert_eq!(frame1["payload"]["agent_id"], "main");

        let event_transformed = SequencedEvent::new(
            std::sync::Arc::from("sess-delta"),
            std::sync::Arc::from("epoch-1"),
            2,
            EngineEvent::LlmDelta {
                agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
                turn_id: "2".into(),
                step: 1,
                part: serde_json::json!({ "text": " Streaming chunk" }),
            },
        );
        let frame2: Value =
            serde_json::from_slice(&event_envelope(&event_transformed).unwrap()).unwrap();
        assert_eq!(frame2["type"], "assistant.delta");
        assert_eq!(frame2["payload"]["delta"], " Streaming chunk");
        assert_eq!(frame2["payload"]["turnId"], 2);

        let event_thinking = SequencedEvent::new(
            std::sync::Arc::from("sess-delta"),
            std::sync::Arc::from("epoch-1"),
            3,
            EngineEvent::LlmDelta {
                agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
                turn_id: "2".into(),
                step: 1,
                part: serde_json::json!({ "thinking": "Let me consider..." }),
            },
        );
        let frame3: Value =
            serde_json::from_slice(&event_envelope(&event_thinking).unwrap()).unwrap();
        assert_eq!(frame3["type"], "thinking.delta");
        assert_eq!(frame3["payload"]["delta"], "Let me consider...");
    }

    #[test]
    fn a_subagents_streaming_event_keeps_its_agent_and_its_turn_number() {
        use crate::events::EngineEvent;

        let event = SequencedEvent::new(
            std::sync::Arc::from("sess-delta"),
            std::sync::Arc::from("epoch-1"),
            4,
            EngineEvent::LlmDelta {
                agent_id: "subagent-42".into(),
                turn_id: "subturn-7".into(),
                step: 1,
                part: serde_json::json!({ "text": " Subagent chunk" }),
            },
        );
        let frame: Value = serde_json::from_slice(&event_envelope(&event).unwrap()).unwrap();
        assert_eq!(frame["type"], "assistant.delta");
        assert_eq!(frame["payload"]["agentId"], "subagent-42");
        assert_eq!(frame["payload"]["turnId"], 7);
        assert_eq!(frame["payload"]["delta"], " Subagent chunk");
    }

    #[test]
    fn a_turn_id_without_a_numeric_tail_encodes_as_zero() {
        assert_eq!(numeric_turn_id("2"), 2);
        assert_eq!(numeric_turn_id("subturn-7"), 7);
        assert_eq!(numeric_turn_id("subturn-"), 0);
        assert_eq!(numeric_turn_id("subturn"), 0);
    }

    #[test]
    fn transcript_subscription_acks_use_the_upstream_shape() {
        let accepted = subscribe_v2_ack("sess-a", true, 42);
        assert_eq!(accepted["accepted"], json!(["sess-a"]));
        assert_eq!(accepted["not_found"], json!([]));
        assert_eq!(accepted["resync_required"], json!([]));
        assert_eq!(accepted["cursors"]["sess-a"]["seq"], 42);

        let missing = subscribe_v2_ack("sess-a", false, 0);
        assert_eq!(missing["accepted"], json!([]));
        assert_eq!(missing["not_found"], json!(["sess-a"]));
        assert_eq!(missing["cursors"], json!({}));

        let detached = unsubscribe_v2_ack("sess-a");
        assert_eq!(detached["accepted"], json!(["sess-a"]));
        assert_eq!(detached["not_found"], json!([]));
        assert_eq!(detached["resync_required"], json!([]));
    }

    #[test]
    fn inbound_frames_are_properly_discriminated() {
        assert_eq!(parse_inbound(br#"{"type":"pong"}"#), Inbound::Pong);
        assert_eq!(
            parse_inbound(
                br#"{"type":"subscribe","id":"s1","payload":{"session_ids":["sess-a"]}}"#
            ),
            Inbound::Subscribe {
                id: "s1".into(),
                session_ids: vec!["sess-a".into()],
                cursors: HashMap::new(),
            }
        );
        assert_eq!(
            parse_inbound(
                br#"{"type":"unsubscribe","id":"u1","payload":{"session_ids":["sess-a"]}}"#
            ),
            Inbound::Unsubscribe {
                id: "u1".into(),
                session_ids: vec!["sess-a".into()],
            }
        );
        assert_eq!(
            parse_inbound(
                br#"{"type":"subscribe_v2","id":"v1","payload":{"session_id":"sess-a","transcript":{"*":"block"},"transcript_since":{"main":3}}}"#
            ),
            Inbound::SubscribeV2 {
                id: "v1".into(),
                session_id: "sess-a".into(),
                transcript: HashMap::from([("*".to_string(), "block".to_string())]),
                transcript_since: HashMap::from([("main".to_string(), 3)]),
            }
        );
        assert_eq!(
            parse_inbound(
                br#"{"type":"unsubscribe_v2","id":"v2","payload":{"session_id":"sess-a","agent_ids":["main"]}}"#
            ),
            Inbound::UnsubscribeV2 {
                id: "v2".into(),
                session_id: "sess-a".into(),
                agent_ids: vec!["main".into()],
            }
        );
        assert_eq!(
            parse_inbound(
                br#"{"type":"abort","id":"a1","payload":{"session_id":"sess-1","prompt_id":"p1"}}"#
            ),
            Inbound::Abort {
                id: "a1".into(),
                session_id: "sess-1".into(),
                prompt_id: "p1".into(),
            }
        );

        assert_eq!(
            parse_inbound(
                br#"{"type":"prompt","id":"p1","payload":{"session_id":"sess-1","prompt":"hello"}}"#
            ),
            Inbound::Unknown
        );
        assert_eq!(
            parse_inbound(br#"{"type":"cancel","id":"c1","payload":{"session_id":"sess-1"}}"#),
            Inbound::Unknown
        );
        assert_eq!(
            parse_inbound(br#"{"type":"terminal_attach","id":"ta1","payload":{"session_id":"sess-1","terminal_id":"term-1","since_seq":10}}"#),
            Inbound::TerminalAttach {
                id: "ta1".into(),
                session_id: "sess-1".into(),
                terminal_id: "term-1".into(),
                since_seq: Some(10),
            }
        );
        assert_eq!(
            parse_inbound(br#"{"type":"terminal_detach","id":"td1","payload":{"session_id":"sess-1","terminal_id":"term-1"}}"#),
            Inbound::TerminalDetach {
                id: "td1".into(),
                session_id: "sess-1".into(),
                terminal_id: "term-1".into(),
            }
        );
        assert_eq!(
            parse_inbound(br#"{"type":"terminal_input","id":"ti1","payload":{"session_id":"sess-1","terminal_id":"term-1","data":"echo 1\n"}}"#),
            Inbound::TerminalInput {
                id: "ti1".into(),
                session_id: "sess-1".into(),
                terminal_id: "term-1".into(),
                data: "echo 1\n".into(),
            }
        );
        assert_eq!(
            parse_inbound(br#"{"type":"terminal_resize","id":"tr1","payload":{"session_id":"sess-1","terminal_id":"term-1","cols":120,"rows":30}}"#),
            Inbound::TerminalResize {
                id: "tr1".into(),
                session_id: "sess-1".into(),
                terminal_id: "term-1".into(),
                cols: 120,
                rows: 30,
            }
        );
        assert_eq!(
            parse_inbound(br#"{"type":"terminal_close","id":"tc1","payload":{"session_id":"sess-1","terminal_id":"term-1"}}"#),
            Inbound::TerminalClose {
                id: "tc1".into(),
                session_id: "sess-1".into(),
                terminal_id: "term-1".into(),
            }
        );
        assert_eq!(
            parse_inbound(br#"{"type":"unknown_type"}"#),
            Inbound::Unknown
        );
        assert_eq!(parse_inbound(b"not json at all"), Inbound::Unknown);
        assert_eq!(parse_inbound(br#"{"payload":{}}"#), Inbound::Unknown);
        assert_eq!(parse_inbound(b"[1,2,3]"), Inbound::Unknown);
    }

    #[test]
    fn client_hello_reads_the_credential_where_kap_server_does() {
        let hello = br#"{"type":"client_hello","id":"c1","payload":{"token":"tok3n","subscriptions":["s1"],"cursors":{"s1":{"seq":5,"epoch":"ep1"}}}}"#;
        let mut expected_cursors = HashMap::new();
        expected_cursors.insert(
            "s1".into(),
            CursorSpec {
                seq: 5,
                epoch: Some("ep1".into()),
            },
        );
        assert_eq!(
            parse_inbound(hello),
            Inbound::ClientHello {
                id: "c1".into(),
                token: Some("tok3n".into()),
                subscriptions: vec!["s1".into()],
                cursors: expected_cursors,
            }
        );

        let empty = br#"{"type":"client_hello","id":"c2","payload":{}}"#;
        assert_eq!(
            parse_inbound(empty),
            Inbound::ClientHello {
                id: "c2".into(),
                token: None,
                subscriptions: Vec::new(),
                cursors: HashMap::new(),
            }
        );
        let wrong_type = br#"{"type":"client_hello","payload":{"token":42}}"#;
        assert_eq!(
            parse_inbound(wrong_type),
            Inbound::ClientHello {
                id: String::new(),
                token: None,
                subscriptions: Vec::new(),
                cursors: HashMap::new(),
            }
        );
    }

    #[test]
    fn test_ack_payload_constructors() {
        let mut cursors = HashMap::new();
        cursors.insert(
            "s1".into(),
            serde_json::json!({ "seq": 10, "epoch": "ep1" }),
        );
        let val_hello = client_hello_ack(&["s1".into()], &["s2".into()], &cursors);
        assert_eq!(val_hello["accepted_subscriptions"], json!(["s1"]));
        assert_eq!(val_hello["resync_required"], json!(["s2"]));
        assert_eq!(val_hello["cursors"]["s1"]["seq"], 10);

        let val_sub = subscribe_ack(&["s1".into()], &["missing".into()], &[], &cursors);
        assert_eq!(val_sub["accepted"], json!(["s1"]));
        assert_eq!(val_sub["not_found"], json!(["missing"]));
        assert_eq!(val_sub["resync_required"], json!([]));
        assert_eq!(val_sub["cursors"]["s1"]["seq"], 10);

        let val_unsub = unsubscribe_ack(&["s1".into()], &["s_err".into()], &[]);
        assert_eq!(val_unsub["accepted"], json!(["s1"]));
        assert_eq!(val_unsub["not_found"], json!(["s_err"]));
        assert_eq!(val_unsub["resync_required"], json!([]));
    }

    #[test]
    fn error_frame_carries_code_msg_fatal() {
        let frame = error_frame(40414, "terminal not found", false, Some("req-9"));
        assert_eq!(frame["type"], "error");
        assert_eq!(frame["payload"]["code"], 40414);
        assert_eq!(frame["payload"]["msg"], "terminal not found");
        assert_eq!(frame["payload"]["fatal"], false);
        assert_eq!(frame["payload"]["request_id"], "req-9");
    }

    #[test]
    fn terminal_custom_events_map_to_contract_frames() {
        let out = SequencedEvent::new(
            "s1".into(),
            "e".into(),
            7,
            crate::events::EngineEvent::Custom(json!({
                "type": "terminal_output",
                "session_id": "s1",
                "terminal_id": "t1",
                "seq": 3,
                "data": "hello"
            })),
        );
        let frame: Value = serde_json::from_slice(&event_envelope(&out).unwrap()).unwrap();
        assert_eq!(frame["type"], "terminal_output");
        assert_eq!(frame["seq"], 7);
        assert_eq!(frame["payload"]["data"], "hello");
        assert!(
            frame["payload"].get("type").is_none(),
            "payload narrows to the contract field"
        );

        let exit = SequencedEvent::new(
            "s1".into(),
            "e".into(),
            8,
            crate::events::EngineEvent::Custom(json!({
                "type": "terminal_exit",
                "session_id": "s1",
                "terminal_id": "t1",
                "payload": { "exit_code": 0 }
            })),
        );
        let frame: Value = serde_json::from_slice(&event_envelope(&exit).unwrap()).unwrap();
        assert_eq!(frame["type"], "terminal_exit");
        assert_eq!(frame["payload"]["exit_code"], 0);
    }

    #[test]
    fn transcript_frames_carry_the_agent_and_snapshot() {
        let reset: Value = serde_json::from_slice(
            &transcript_reset_frame("sess-a", "main", serde_json::json!({ "items": [] })).unwrap(),
        )
        .unwrap();
        assert_eq!(reset["type"], "transcript.reset");
        assert_eq!(reset["session_id"], "sess-a");
        assert_eq!(reset["payload"]["agent_id"], "main");
        assert_eq!(reset["payload"]["has_more_older"], false);
        assert_eq!(reset["payload"]["snapshot"]["items"], serde_json::json!([]));

        let ops: Value = serde_json::from_slice(
            &transcript_ops_frame("sess-a", "main", serde_json::json!([]), 7).unwrap(),
        )
        .unwrap();
        assert_eq!(ops["type"], "transcript.ops");
        assert_eq!(ops["payload"]["agent_id"], "main");
        assert_eq!(ops["payload"]["seq"], 7);
    }
}
