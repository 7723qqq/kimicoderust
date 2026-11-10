use serde::{Deserialize, Serialize};
use serde_json::Value;

use kimi_ui_core::{SeqSpan, StreamKind};

use crate::rpc::types::TokenUsage;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EngineEvent {
    #[serde(rename = "llm.step.begin")]
    LlmStepBegin {
        #[serde(default = "crate::rpc::types::main_agent_id")]
        agent_id: String,
        turn_id: String,
        step: u32,
    },
    #[serde(rename = "llm.delta")]
    LlmDelta {
        #[serde(default = "crate::rpc::types::main_agent_id")]
        agent_id: String,
        turn_id: String,
        step: u32,
        part: Value,
    },
    #[serde(rename = "llm.step.end")]
    LlmStepEnd {
        #[serde(default = "crate::rpc::types::main_agent_id")]
        agent_id: String,
        turn_id: String,
        step: u32,
        usage: Option<TokenUsage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timing: Option<crate::llm::LlmTiming>,
    },
    #[serde(rename = "tool.native")]
    ToolNative {
        #[serde(default = "crate::rpc::types::main_agent_id")]
        agent_id: String,
        turn_id: String,
        tool_call_id: String,
        tool_name: String,
        arguments: Value,
        content: String,
        is_error: bool,
        note: Option<String>,
    },
    #[serde(rename = "goal.budget.limit_reached")]
    GoalBudgetLimitReached { turn_id: String, goal_id: String },
    #[serde(rename = "cron.fired")]
    CronFired { entry_id: String, prompt: String },
    #[serde(rename = "assistant.delta")]
    AssistantDelta {
        #[serde(default = "crate::rpc::types::main_agent_id")]
        agent_id: String,
        turn_id: String,
        delta: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        block: Option<u32>,
    },
    #[serde(rename = "thinking.delta")]
    ThinkingDelta {
        #[serde(default = "crate::rpc::types::main_agent_id")]
        agent_id: String,
        turn_id: String,
        delta: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        block: Option<u32>,
    },
    #[serde(rename = "tool.call.delta")]
    ToolCallDelta {
        #[serde(default = "crate::rpc::types::main_agent_id")]
        agent_id: String,
        turn_id: String,
        tool_call_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        arguments_part: Option<String>,
    },
    #[serde(rename = "tool.call.started")]
    ToolCallStarted {
        #[serde(default = "crate::rpc::types::main_agent_id")]
        agent_id: String,
        turn_id: String,
        tool_call_id: String,
        name: String,
        args: Value,
    },
    #[serde(rename = "tool.progress")]
    ToolProgress {
        #[serde(default = "crate::rpc::types::main_agent_id")]
        agent_id: String,
        turn_id: String,
        tool_call_id: String,
        update: Value,
    },
    #[serde(rename = "turn.started")]
    TurnStarted {
        #[serde(default = "crate::rpc::types::main_agent_id")]
        agent_id: String,
        turn_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
    },
    #[serde(rename = "turn.ended")]
    TurnEnded {
        #[serde(default = "crate::rpc::types::main_agent_id")]
        agent_id: String,
        turn_id: String,
        reason: String,
    },
    #[serde(rename = "event.session.status_changed")]
    SessionStatusChanged {
        status: String,
        previous_status: String,
    },
    #[serde(rename = "event.session.work_changed")]
    SessionWorkChanged {
        busy: bool,
        main_turn_active: bool,
        pending_interaction: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        last_turn_reason: Option<String>,
    },
    #[serde(rename = "tool.call.completed")]
    ToolCallCompleted {
        #[serde(default = "crate::rpc::types::main_agent_id")]
        agent_id: String,
        turn_id: String,
        tool_call_id: String,
        result: Value,
    },
    #[serde(rename = "tool.call.failed")]
    ToolCallFailed {
        #[serde(default = "crate::rpc::types::main_agent_id")]
        agent_id: String,
        turn_id: String,
        tool_call_id: String,
        error: String,
    },
    #[serde(rename = "session.meta.updated")]
    SessionMetaUpdated { session_id: String, meta: Value },
    #[serde(rename = "event.config.changed")]
    ConfigChanged {
        changed_fields: Vec<String>,
        config: Value,
    },
    #[serde(rename = "subagent.spawned")]
    SubagentSpawned {
        subagent_id: String,
        #[serde(default)]
        subagent_name: Option<String>,
        #[serde(default)]
        parent_tool_call_id: Option<String>,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        run_in_background: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        swarm_index: Option<u32>,
    },
    #[serde(rename = "subagent.completed")]
    SubagentCompleted {
        subagent_id: String,
        #[serde(default)]
        result_summary: Option<String>,
        #[serde(default)]
        usage: Option<TokenUsage>,
    },
    #[serde(rename = "subagent.failed")]
    SubagentFailed { subagent_id: String, error: String },
    #[serde(rename = "stream.segment")]
    StreamSegment {
        #[serde(default = "crate::rpc::types::main_agent_id")]
        agent_id: String,
        turn_id: String,
        stream: StreamKind,
        text: String,
        span: SeqSpan,
        is_final: bool,
    },
    #[serde(untagged)]
    Custom(Value),
}

impl EngineEvent {
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    pub fn from_json(value: Value) -> Self {
        serde_json::from_value(value.clone()).unwrap_or(EngineEvent::Custom(value))
    }

    pub fn event_type(&self) -> &str {
        match self {
            EngineEvent::LlmStepBegin { .. } => "llm.step.begin",
            EngineEvent::LlmDelta { .. } => "llm.delta",
            EngineEvent::LlmStepEnd { .. } => "llm.step.end",
            EngineEvent::ToolNative { .. } => "tool.native",
            EngineEvent::GoalBudgetLimitReached { .. } => "goal.budget.limit_reached",
            EngineEvent::AssistantDelta { .. } => "assistant.delta",
            EngineEvent::ThinkingDelta { .. } => "thinking.delta",
            EngineEvent::ToolCallDelta { .. } => "tool.call.delta",
            EngineEvent::ToolCallStarted { .. } => "tool.call.started",
            EngineEvent::ToolProgress { .. } => "tool.progress",
            EngineEvent::TurnStarted { .. } => "turn.started",
            EngineEvent::TurnEnded { .. } => "turn.ended",
            EngineEvent::SessionStatusChanged { .. } => "event.session.status_changed",
            EngineEvent::SessionWorkChanged { .. } => "event.session.work_changed",
            EngineEvent::ToolCallCompleted { .. } => "tool.call.completed",
            EngineEvent::ToolCallFailed { .. } => "tool.call.failed",
            EngineEvent::SessionMetaUpdated { .. } => "session.meta.updated",
            EngineEvent::ConfigChanged { .. } => "event.config.changed",
            EngineEvent::SubagentSpawned { .. } => "subagent.spawned",
            EngineEvent::SubagentCompleted { .. } => "subagent.completed",
            EngineEvent::SubagentFailed { .. } => "subagent.failed",
            EngineEvent::CronFired { .. } => "cron.fired",
            EngineEvent::StreamSegment { .. } => "stream.segment",
            EngineEvent::Custom(v) => v.get("type").and_then(|t| t.as_str()).unwrap_or("custom"),
        }
    }

    pub fn agent_id(&self) -> Option<&str> {
        match self {
            EngineEvent::LlmStepBegin { agent_id, .. }
            | EngineEvent::LlmDelta { agent_id, .. }
            | EngineEvent::LlmStepEnd { agent_id, .. }
            | EngineEvent::ToolNative { agent_id, .. }
            | EngineEvent::AssistantDelta { agent_id, .. }
            | EngineEvent::ThinkingDelta { agent_id, .. }
            | EngineEvent::ToolCallDelta { agent_id, .. }
            | EngineEvent::ToolCallStarted { agent_id, .. }
            | EngineEvent::ToolProgress { agent_id, .. }
            | EngineEvent::TurnStarted { agent_id, .. }
            | EngineEvent::TurnEnded { agent_id, .. }
            | EngineEvent::ToolCallCompleted { agent_id, .. }
            | EngineEvent::ToolCallFailed { agent_id, .. }
            | EngineEvent::StreamSegment { agent_id, .. } => Some(agent_id.as_str()),
            EngineEvent::Custom(value) => value.get("agent_id").and_then(|v| v.as_str()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SequencedEngineEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    pub event: EngineEvent,
}

impl SequencedEngineEvent {
    pub fn new(event: EngineEvent) -> Self {
        Self { seq: None, event }
    }

    pub fn with_seq(seq: u64, event: EngineEvent) -> Self {
        Self {
            seq: Some(seq),
            event,
        }
    }

    pub fn seq(&self) -> Option<u64> {
        self.seq
    }

    pub fn event(&self) -> &EngineEvent {
        &self.event
    }

    pub fn into_event(self) -> EngineEvent {
        self.event
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_swarm_members_index_survives_the_journal() {
        let raw = serde_json::json!({
            "type": "subagent.spawned",
            "subagent_id": "subagent-1",
            "subagent_name": "coder",
            "parent_tool_call_id": "call_swarm",
            "description": "work on a",
            "run_in_background": false,
            "swarm_index": 1,
        });
        let event = EngineEvent::from_json(raw.clone());
        assert!(
            matches!(
                event,
                EngineEvent::SubagentSpawned {
                    swarm_index: Some(1),
                    ..
                }
            ),
            "the index is parsed, not dropped: {event:?}"
        );
        let round_tripped = event.to_json();
        assert_eq!(round_tripped["swarm_index"], 1, "{round_tripped}");
    }

    #[test]
    fn a_plain_subagent_records_no_index() {
        let raw = serde_json::json!({
            "type": "subagent.spawned",
            "subagent_id": "subagent-9",
            "run_in_background": false,
        });
        let event = EngineEvent::from_json(raw);
        assert!(matches!(
            event,
            EngineEvent::SubagentSpawned {
                swarm_index: None,
                ..
            }
        ));
        let written = event.to_json();
        assert!(
            written.get("swarm_index").is_none(),
            "omitted, not null: {written}"
        );
    }

    #[test]
    fn sequenced_envelope_defaults_to_none() {
        let event = EngineEvent::TurnEnded {
            agent_id: "main".into(),
            turn_id: "t".into(),
            reason: "done".into(),
        };
        let envelope = SequencedEngineEvent::new(event.clone());
        assert_eq!(envelope.seq(), None);
        assert_eq!(envelope.event().event_type(), "turn.ended");
        let stamped = SequencedEngineEvent::with_seq(41, event);
        assert_eq!(stamped.seq(), Some(41));
        assert_eq!(stamped.into_event().event_type(), "turn.ended");
    }

    #[test]
    fn sequenced_envelope_omits_seq_until_stamped() {
        let event = EngineEvent::AssistantDelta {
            agent_id: "main".into(),
            turn_id: "t".into(),
            delta: "d".into(),
            block: None,
        };
        let plain = serde_json::to_value(SequencedEngineEvent::new(event.clone())).unwrap();
        assert!(plain.get("seq").is_none(), "unset seq leaks: {plain}");
        let stamped = serde_json::to_value(SequencedEngineEvent::with_seq(7, event)).unwrap();
        assert_eq!(stamped["seq"], 7);
        assert_eq!(stamped["event"]["type"], "assistant.delta");
        let back: SequencedEngineEvent = serde_json::from_value(stamped).unwrap();
        assert_eq!(back.seq(), Some(7));
    }

    #[test]
    fn block_indexed_deltas_round_trip() {
        let assistant = EngineEvent::AssistantDelta {
            agent_id: "main".into(),
            turn_id: "t".into(),
            delta: "hi".into(),
            block: Some(2),
        };
        let value = serde_json::to_value(&assistant).unwrap();
        assert_eq!(value["type"], "assistant.delta");
        assert_eq!(value["block"], 2);
        assert_eq!(assistant.event_type(), "assistant.delta");
        assert_eq!(assistant.agent_id(), Some("main"));
        let back: EngineEvent = serde_json::from_value(value).unwrap();
        assert_eq!(
            serde_json::to_value(&back).unwrap(),
            serde_json::to_value(&assistant).unwrap()
        );

        let without_block = EngineEvent::ThinkingDelta {
            agent_id: "main".into(),
            turn_id: "t".into(),
            delta: "think".into(),
            block: None,
        };
        let value = serde_json::to_value(&without_block).unwrap();
        assert!(value.get("block").is_none(), "absent block must not leak");
    }

    #[test]
    fn stream_segment_round_trips() {
        let event = EngineEvent::StreamSegment {
            agent_id: "main".into(),
            turn_id: "t".into(),
            stream: StreamKind::ToolCall {
                tool_call_id: "c".into(),
            },
            text: "merged".into(),
            span: SeqSpan::new(3, 9),
            is_final: true,
        };
        assert_eq!(event.event_type(), "stream.segment");
        assert_eq!(event.agent_id(), Some("main"));
        let json = event.to_json();
        assert_eq!(json["type"], "stream.segment");
        assert_eq!(json["stream"]["stream"], "tool_call");
        assert_eq!(json["stream"]["tool_call_id"], "c");
        assert_eq!(json["span"], serde_json::json!({ "first": 3, "last": 9 }));
        assert_eq!(json["is_final"], true);
        assert!(matches!(
            EngineEvent::from_json(json),
            EngineEvent::StreamSegment {
                is_final: true,
                span: SeqSpan { first: 3, last: 9 },
                ..
            }
        ));
    }

    #[test]
    fn no_variant_serializes_a_seq_field() {
        let events = vec![
            EngineEvent::LlmStepBegin {
                agent_id: "main".into(),
                turn_id: "t".into(),
                step: 0,
            },
            EngineEvent::LlmDelta {
                agent_id: "main".into(),
                turn_id: "t".into(),
                step: 0,
                part: Value::Null,
            },
            EngineEvent::LlmStepEnd {
                agent_id: "main".into(),
                turn_id: "t".into(),
                step: 0,
                usage: None,
                timing: None,
            },
            EngineEvent::ToolNative {
                agent_id: "main".into(),
                turn_id: "t".into(),
                tool_call_id: "c".into(),
                tool_name: "Read".into(),
                arguments: Value::Null,
                content: String::new(),
                is_error: false,
                note: None,
            },
            EngineEvent::GoalBudgetLimitReached {
                turn_id: "t".into(),
                goal_id: "g".into(),
            },
            EngineEvent::CronFired {
                entry_id: "e".into(),
                prompt: "p".into(),
            },
            EngineEvent::AssistantDelta {
                agent_id: "main".into(),
                turn_id: "t".into(),
                delta: "d".into(),
                block: None,
            },
            EngineEvent::ThinkingDelta {
                agent_id: "main".into(),
                turn_id: "t".into(),
                delta: "d".into(),
                block: None,
            },
            EngineEvent::ToolCallDelta {
                agent_id: "main".into(),
                turn_id: "t".into(),
                tool_call_id: "c".into(),
                name: None,
                arguments_part: None,
            },
            EngineEvent::ToolCallStarted {
                agent_id: "main".into(),
                turn_id: "t".into(),
                tool_call_id: "c".into(),
                name: "Read".into(),
                args: Value::Null,
            },
            EngineEvent::ToolProgress {
                agent_id: "main".into(),
                turn_id: "t".into(),
                tool_call_id: "c".into(),
                update: Value::Null,
            },
            EngineEvent::TurnStarted {
                agent_id: "main".into(),
                turn_id: "t".into(),
                prompt: None,
            },
            EngineEvent::TurnEnded {
                agent_id: "main".into(),
                turn_id: "t".into(),
                reason: "done".into(),
            },
            EngineEvent::SessionStatusChanged {
                status: "s".into(),
                previous_status: "p".into(),
            },
            EngineEvent::SessionWorkChanged {
                busy: false,
                main_turn_active: false,
                pending_interaction: String::new(),
                last_turn_reason: None,
            },
            EngineEvent::ToolCallCompleted {
                agent_id: "main".into(),
                turn_id: "t".into(),
                tool_call_id: "c".into(),
                result: Value::Null,
            },
            EngineEvent::ToolCallFailed {
                agent_id: "main".into(),
                turn_id: "t".into(),
                tool_call_id: "c".into(),
                error: "e".into(),
            },
            EngineEvent::SessionMetaUpdated {
                session_id: "s".into(),
                meta: Value::Null,
            },
            EngineEvent::ConfigChanged {
                changed_fields: vec![],
                config: Value::Null,
            },
            EngineEvent::SubagentSpawned {
                subagent_id: "s".into(),
                subagent_name: None,
                parent_tool_call_id: None,
                description: None,
                run_in_background: false,
                swarm_index: None,
            },
            EngineEvent::SubagentCompleted {
                subagent_id: "s".into(),
                result_summary: None,
                usage: None,
            },
            EngineEvent::SubagentFailed {
                subagent_id: "s".into(),
                error: "e".into(),
            },
            EngineEvent::StreamSegment {
                agent_id: "main".into(),
                turn_id: "t".into(),
                stream: StreamKind::Assistant,
                text: "x".into(),
                span: SeqSpan::new(0, 1),
                is_final: true,
            },
            EngineEvent::Custom(serde_json::json!({ "type": "anything" })),
        ];
        assert_eq!(events.len(), 24, "every variant is pinned");
        for event in events {
            let json = event.to_json();
            assert!(
                json.get("seq").is_none(),
                "seq leaked onto the wire: {json}"
            );
        }
    }
}
