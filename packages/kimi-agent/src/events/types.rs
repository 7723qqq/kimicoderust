//! Strongly-typed event definitions for kimi-agent.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use kimi_ui_core::{SeqSpan, StreamKind};

use crate::rpc::types::TokenUsage;

/// Engine lifecycle and streaming events emitted during a turn.
///
/// Every event a turn produces carries the `agent_id` of the agent running it
/// (v2: `turnEvents.ts` gives `agentId` to each `TurnStartedPayload` /
/// `AssistantDeltaPayload` / `ToolCallStartedPayload`, and
/// `loopService.ts:1426-1455` fills it from `this.scopeContext.agentId`).
/// That field is what lets a host keep a subagent's events out of the main
/// transcript — v2 additionally refuses a cross-agent dispatch outright
/// (`eventDispatcherService.ts:502-510`).
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
        /// Per-request LLM timing (v2 `ModelRequestTiming`, upstream #3938).
        /// Absent on the host-proxy path and from transports that do not
        /// measure.
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
    /// A scheduled cron entry fired (v2 `cron.fired`). The host subscribes
    /// and turns the fired prompt into a new turn or surfaces it to the user.
    /// Wire shape carries the entry id and prompt so the host can render or
    /// route without a separate fetch.
    #[serde(rename = "cron.fired")]
    CronFired { entry_id: String, prompt: String },
    #[serde(rename = "assistant.delta")]
    AssistantDelta {
        #[serde(default = "crate::rpc::types::main_agent_id")]
        agent_id: String,
        turn_id: String,
        delta: String,
        /// v2 #4663: the provider's content-block index this delta belongs to.
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
    /// v2 #4663 / `TurnStepStreamReset`: the step's streamed partials were
    /// discarded and a fresh block sequence starts. Observable only.
    #[serde(rename = "turn.step.stream_reset")]
    StepStreamReset {
        #[serde(default = "crate::rpc::types::main_agent_id")]
        agent_id: String,
        turn_id: String,
        step: u32,
        step_id: String,
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
    /// A config mutation the server applied (kap-server `ConfigChangedEvent`,
    /// wire name `event.config.changed`). The payload spells
    /// `changed_fields` — snake_case, what kimi-web's mapper reads; kap-server's
    /// own zod schema says `changedFields`, so the two TS sides disagree and the
    /// live consumer wins.
    #[serde(rename = "event.config.changed")]
    ConfigChanged {
        /// The config sections the mutation touched (kap-server `changedFields`).
        changed_fields: Vec<String>,
        /// The effective config after the mutation (kap-server `ConfigResponse`).
        config: Value,
    },
    #[serde(rename = "subagent.spawned")]
    SubagentSpawned {
        subagent_id: String,
        #[serde(default)]
        subagent_name: Option<String>,
        /// The parent tool call that spawned the member (v2
        /// `SubagentSpawnedPayload.parentToolCallId`, upstream #3970) — the
        /// transcript fold keys the agent↔task association off it.
        #[serde(default)]
        parent_tool_call_id: Option<String>,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        run_in_background: bool,
        /// The member's 1-based position in its batch (v2
        /// `SubagentSpawnedPayload.swarmIndex`).
        ///
        /// Serde drops unknown fields, so leaving this out of the record loses
        /// the swarm identity for everything rebuilt from the journal: the
        /// transcript projector cannot tell a member from a plain child, and a
        /// resumed swarm renders as a stack of ordinary subagent cards. `None`
        /// is an ordinary `Agent` subagent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        swarm_index: Option<u32>,
    },
    #[serde(rename = "subagent.completed")]
    SubagentCompleted {
        subagent_id: String,
        #[serde(default)]
        result_summary: Option<String>,
        /// The member's token usage (upstream #3970): the durable record
        /// carries it and the fold restores it onto the task.
        #[serde(default)]
        usage: Option<TokenUsage>,
    },
    #[serde(rename = "subagent.failed")]
    SubagentFailed { subagent_id: String, error: String },
    /// Batched stream segment — the merged-delta shape of the batching
    /// contract (one event folds a coalesced span of deltas instead of one
    /// event per token; `span` covers the folded engine seqs, `is_final`
    /// marks the stream-closing segment so the tail cannot be lost silently).
    /// Skeleton phase: defined, not yet emitted; the per-delta variants keep
    /// flowing until the behavior slice switches emission over.
    #[serde(rename = "stream.segment")]
    StreamSegment {
        #[serde(default = "crate::rpc::types::main_agent_id")]
        agent_id: String,
        turn_id: String,
        stream: StreamKind,
        /// Merged text of every coalesced delta, in seq order.
        text: String,
        /// Inclusive span of engine seqs folded into this segment.
        span: SeqSpan,
        /// True on the stream-closing segment (tail-loss guard).
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
            EngineEvent::StepStreamReset { .. } => "turn.step.stream_reset",
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

    /// The agent that produced this event, when it names one.
    ///
    /// v2 reads the same field off the payload when filtering
    /// (`sessionEventBroadcaster.ts:1211-1214` takes `payload.agentId` and
    /// passes anything non-string through). A `Custom` event that predates
    /// the stamp has no id, and is treated the same way there.
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
            | EngineEvent::StreamSegment { agent_id, .. }
            | EngineEvent::StepStreamReset { agent_id, .. } => Some(agent_id.as_str()),
            EngineEvent::Custom(value) => value.get("agent_id").and_then(|v| v.as_str()),
            _ => None,
        }
    }
}

/// In-process sequencing envelope for [`EngineEvent`].
///
/// Carries the optional per-`(agent_id, turn_id)` monotonic `seq` the event
/// contract reserves (design `reports/design/RUST-UI-DECOUPLING.md` §1.1).
/// Scope: engine internals and the napi bindings layer only. The wire is
/// untouched — `EngineEvent` itself gains no field, this wrapper is not
/// serialized onto either channel (TSFN / WS), and even an explicit envelope
/// serialization omits `seq` while unset.
///
/// Why an envelope rather than a field on each variant: a new variant field
/// must be named at every literal construction site, including `acp/` test
/// code that is outside this phase's editable scope. The envelope carries the
/// same information with zero churn; collapsing it into per-variant fields is
/// a mechanical follow-up once that scope opens.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SequencedEngineEvent {
    /// Monotonic within the event's `(agent_id, turn_id)` scope; `None` until
    /// the emission layer stamps it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    pub event: EngineEvent,
}

impl SequencedEngineEvent {
    /// An unstamped envelope.
    pub fn new(event: EngineEvent) -> Self {
        Self { seq: None, event }
    }

    /// An envelope stamped with `seq`.
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

    /// The swarm discriminator has to survive the journal round-trip. Serde
    /// drops unknown fields, so a record that omits `swarm_index` silently
    /// degrades every swarm member into an ordinary child — which is exactly
    /// how a restored swarm ended up rendered as a pile of plain subagent cards.
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
        // …and it is still there when the record is written back out.
        let round_tripped = event.to_json();
        assert_eq!(round_tripped["swarm_index"], 1, "{round_tripped}");
    }

    /// An ordinary `Agent` subagent has no index, and the field must stay
    /// absent rather than become `null` — the client's discriminator is
    /// `typeof swarm_index === 'number'`.
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

    /// The envelope defaults to unstamped and stamps explicitly.
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

    /// `seq` stays off the wire while unset, and serializes only when stamped.
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

    /// v2 #4663: assistant/thinking deltas carry the provider's content-block
    /// index, and a step whose stream is discarded emits a stream_reset event.
    #[test]
    fn block_indexed_deltas_and_stream_reset_round_trip() {
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

        let reset = EngineEvent::StepStreamReset {
            agent_id: "main".into(),
            turn_id: "t".into(),
            step: 3,
            step_id: "step-uuid".into(),
        };
        let value = serde_json::to_value(&reset).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "type": "turn.step.stream_reset",
                "agent_id": "main",
                "turn_id": "t",
                "step": 3,
                "step_id": "step-uuid",
            })
        );
        assert_eq!(reset.event_type(), "turn.step.stream_reset");
        let back: EngineEvent = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(&back).unwrap(), value);
    }

    /// The batched-segment variant round-trips with its span and stream tag.
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

    /// Wire regression pin: no event variant may ever serialize a `seq` key.
    /// Sequencing is in-process only (the envelope); the WS contract and the
    /// TSFN payload shape stay unchanged.
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
