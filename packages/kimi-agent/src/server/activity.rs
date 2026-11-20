use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::Serialize;
use serde_json::Value;

use crate::events::EngineEvent;
use crate::server::hub::EventHub;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn same_series(a: &AgentPhase, b: &AgentPhase) -> bool {
    match (a, b) {
        (AgentPhase::Idle, AgentPhase::Idle) => true,
        (
            AgentPhase::Running {
                turn_id: at,
                step: as_,
                step_id: asi,
                ..
            },
            AgentPhase::Running {
                turn_id: bt,
                step: bs,
                step_id: bsi,
                ..
            },
        ) => at == bt && as_ == bs && asi == bsi,
        (
            AgentPhase::Streaming {
                turn_id: at,
                step: as_,
                step_id: asi,
                stream: ast,
                ..
            },
            AgentPhase::Streaming {
                turn_id: bt,
                step: bs,
                step_id: bsi,
                stream: bst,
                ..
            },
        ) => at == bt && as_ == bs && asi == bsi && ast == bst,
        (
            AgentPhase::ToolCall {
                turn_id: at,
                step: as_,
                step_id: asi,
                tool_call_id: acid,
                name: an,
                ..
            },
            AgentPhase::ToolCall {
                turn_id: bt,
                step: bs,
                step_id: bsi,
                tool_call_id: bcid,
                name: bn,
                ..
            },
        ) => at == bt && as_ == bs && asi == bsi && acid == bcid && an == bn,
        (
            AgentPhase::Retrying {
                turn_id: at,
                step: as_,
                step_id: asi,
                failed_attempt: afa,
                next_attempt: ana,
                max_attempts: ama,
                delay_ms: adm,
                error_name: aen,
                status_code: asc,
                ..
            },
            AgentPhase::Retrying {
                turn_id: bt,
                step: bs,
                step_id: bsi,
                failed_attempt: bfa,
                next_attempt: bna,
                max_attempts: bma,
                delay_ms: bdm,
                error_name: ben,
                status_code: bsc,
                ..
            },
        ) => {
            at == bt
                && as_ == bs
                && asi == bsi
                && afa == bfa
                && ana == bna
                && ama == bma
                && adm == bdm
                && aen == ben
                && asc == bsc
        }
        (
            AgentPhase::AwaitingApproval {
                turn_id: at,
                step: as_,
                approval: aa,
                ..
            },
            AgentPhase::AwaitingApproval {
                turn_id: bt,
                step: bs,
                approval: ba,
                ..
            },
        ) => at == bt && as_ == bs && aa == ba,
        (
            AgentPhase::Interrupted {
                turn_id: at,
                step: as_,
                reason: ar,
                message: am,
                ..
            },
            AgentPhase::Interrupted {
                turn_id: bt,
                step: bs,
                reason: br,
                message: bm,
                ..
            },
        ) => at == bt && as_ == bs && ar == br && am == bm,
        (
            AgentPhase::Ended {
                turn_id: at,
                reason: ar,
                duration_ms: ad,
                ..
            },
            AgentPhase::Ended {
                turn_id: bt,
                reason: br,
                duration_ms: bd,
                ..
            },
        ) => at == bt && ar == br && ad == bd,
        _ => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamKind {
    Assistant,
    Thinking,
    ToolCall,
}

impl StreamKind {
    fn from_part_kind(part_kind: &str) -> Option<Self> {
        match part_kind {
            "text" => Some(Self::Assistant),
            "think" => Some(Self::Thinking),
            "tool_call" => Some(Self::ToolCall),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InterruptReason {
    Aborted,
    MaxSteps,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalRef {
    pub approval_id: String,
    pub tool_call_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentPhase {
    Idle,
    Running {
        #[serde(rename = "turnId")]
        turn_id: u64,
        step: u32,
        #[serde(rename = "stepId")]
        step_id: String,
        since: u64,
    },
    Streaming {
        #[serde(rename = "turnId")]
        turn_id: u64,
        step: u32,
        #[serde(rename = "stepId")]
        step_id: String,
        stream: StreamKind,
        since: u64,
    },
    ToolCall {
        #[serde(rename = "turnId")]
        turn_id: u64,
        step: u32,
        #[serde(rename = "stepId")]
        step_id: String,
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        name: String,
        since: u64,
    },
    Retrying {
        #[serde(rename = "turnId")]
        turn_id: u64,
        step: u32,
        #[serde(rename = "stepId")]
        step_id: String,
        #[serde(rename = "failedAttempt")]
        failed_attempt: u32,
        #[serde(rename = "nextAttempt")]
        next_attempt: u32,
        #[serde(rename = "maxAttempts")]
        max_attempts: u32,
        #[serde(rename = "delayMs")]
        delay_ms: u64,
        #[serde(rename = "errorName", skip_serializing_if = "Option::is_none")]
        error_name: Option<String>,
        #[serde(rename = "statusCode", skip_serializing_if = "Option::is_none")]
        status_code: Option<u32>,
        since: u64,
    },
    AwaitingApproval {
        #[serde(rename = "turnId")]
        turn_id: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        step: Option<u32>,
        approval: ApprovalRef,
        since: u64,
    },
    Interrupted {
        #[serde(rename = "turnId")]
        turn_id: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        step: Option<u32>,
        reason: InterruptReason,
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
        at: u64,
    },
    Ended {
        #[serde(rename = "turnId")]
        turn_id: u64,
        reason: String,
        #[serde(rename = "durationMs", skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        at: u64,
    },
}

pub fn phase_turn_id(phase: &AgentPhase) -> Option<u64> {
    match phase {
        AgentPhase::Idle => None,
        AgentPhase::Running { turn_id, .. }
        | AgentPhase::Streaming { turn_id, .. }
        | AgentPhase::ToolCall { turn_id, .. }
        | AgentPhase::Retrying { turn_id, .. }
        | AgentPhase::AwaitingApproval { turn_id, .. }
        | AgentPhase::Interrupted { turn_id, .. }
        | AgentPhase::Ended { turn_id, .. } => Some(*turn_id),
    }
}

pub fn phase_since(phase: &AgentPhase) -> Option<u64> {
    match phase {
        AgentPhase::Idle => None,
        AgentPhase::Running { since, .. }
        | AgentPhase::Streaming { since, .. }
        | AgentPhase::ToolCall { since, .. }
        | AgentPhase::Retrying { since, .. }
        | AgentPhase::AwaitingApproval { since, .. } => Some(*since),
        AgentPhase::Interrupted { at, .. } | AgentPhase::Ended { at, .. } => Some(*at),
    }
}

pub fn phase_has_pending_requests(phase: &AgentPhase) -> bool {
    matches!(phase, AgentPhase::AwaitingApproval { .. })
}

pub struct ActivityTracker {
    session_id: String,
    hub: Arc<EventHub>,
    state: Mutex<AgentPhase>,

    step: AtomicU32,
    turn_id: AtomicU32,
    turn_started_at: Mutex<Option<Instant>>,
}

impl ActivityTracker {
    fn new(session_id: String, hub: Arc<EventHub>) -> Self {
        Self {
            session_id,
            hub,
            state: Mutex::new(AgentPhase::Idle),
            step: AtomicU32::new(0),
            turn_id: AtomicU32::new(0),
            turn_started_at: Mutex::new(None),
        }
    }

    pub fn snapshot(&self) -> AgentPhase {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn turn_step(&self) -> u32 {
        self.step.load(Ordering::Relaxed)
    }

    pub fn turn_started_ms(&self) -> Option<u64> {
        let started = (*self
            .turn_started_at
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()))?;
        Some(now_ms().saturating_sub(started.elapsed().as_millis() as u64))
    }

    fn transition(&self, next: AgentPhase) {
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if same_series(&state, &next) {
                return;
            }
            *state = next;
        }
        self.publish();
    }

    fn publish(&self) {
        let phase = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        self.hub
            .bus_for(&self.session_id)
            .publish(&EngineEvent::Custom(serde_json::json!({
                "type": "agent.status.updated",
                "phase": phase,
            })));
    }

    pub fn publish_turn_event(&self, payload: serde_json::Value) {
        self.hub
            .bus_for(&self.session_id)
            .publish(&EngineEvent::Custom(payload));
    }

    pub fn turn_started(&self, turn_number: u32) {
        self.turn_id.store(turn_number, Ordering::Relaxed);
        self.step.store(0, Ordering::Relaxed);
        *self
            .turn_started_at
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Instant::now());
        self.transition(AgentPhase::Running {
            turn_id: u64::from(turn_number),
            step: 0,
            step_id: String::new(),
            since: now_ms(),
        });
    }

    pub fn llm_delta(&self, part: &Value) {
        let Some(part_kind) = part.get("type").and_then(|v| v.as_str()) else {
            return;
        };
        let Some(stream) = StreamKind::from_part_kind(part_kind) else {
            return;
        };
        self.transition(AgentPhase::Streaming {
            turn_id: u64::from(self.turn_id.load(Ordering::Relaxed)),
            step: self.step.load(Ordering::Relaxed),
            step_id: String::new(),
            stream,
            since: now_ms(),
        });
    }

    pub fn step_begin(&self) {
        self.transition(AgentPhase::Running {
            turn_id: u64::from(self.turn_id.load(Ordering::Relaxed)),
            step: self.step.load(Ordering::Relaxed),
            step_id: String::new(),
            since: now_ms(),
        });
    }

    pub fn tool_started(&self, tool_call_id: &str, name: &str) {
        let step = self.step.fetch_add(1, Ordering::Relaxed) + 1;
        self.transition(AgentPhase::ToolCall {
            turn_id: u64::from(self.turn_id.load(Ordering::Relaxed)),
            step,
            step_id: String::new(),
            tool_call_id: tool_call_id.to_string(),
            name: name.to_string(),
            since: now_ms(),
        });
    }

    pub fn retrying(
        &self,
        failed_attempt: u32,
        next_attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
    ) {
        self.transition(AgentPhase::Retrying {
            turn_id: u64::from(self.turn_id.load(Ordering::Relaxed)),
            step: self.step.load(Ordering::Relaxed),
            step_id: String::new(),
            failed_attempt,
            next_attempt,
            max_attempts,
            delay_ms,
            error_name: None,
            status_code: None,
            since: now_ms(),
        });
    }

    pub fn awaiting(&self, approval_id: &str, tool_call_id: &str) {
        self.transition(AgentPhase::AwaitingApproval {
            turn_id: u64::from(self.turn_id.load(Ordering::Relaxed)),
            step: Some(self.step.load(Ordering::Relaxed)),
            approval: ApprovalRef {
                approval_id: approval_id.to_string(),
                tool_call_id: tool_call_id.to_string(),
            },
            since: now_ms(),
        });
    }

    pub fn interaction_resolved(&self) {
        let matches = matches!(
            *self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            AgentPhase::AwaitingApproval { .. }
        );
        if matches {
            self.transition(AgentPhase::Running {
                turn_id: u64::from(self.turn_id.load(Ordering::Relaxed)),
                step: self.step.load(Ordering::Relaxed),
                step_id: String::new(),
                since: now_ms(),
            });
        }
    }

    pub fn turn_ended(&self, reason: &str) {
        let duration_ms = self
            .turn_started_at
            .lock()
            .unwrap()
            .map(|started| started.elapsed().as_millis() as u64);
        self.transition(AgentPhase::Ended {
            turn_id: u64::from(self.turn_id.load(Ordering::Relaxed)),
            reason: reason.to_string(),
            duration_ms,
            at: now_ms(),
        });
    }

    pub fn interrupted(&self, reason: InterruptReason, message: Option<String>) {
        self.transition(AgentPhase::Interrupted {
            turn_id: u64::from(self.turn_id.load(Ordering::Relaxed)),
            step: Some(self.step.load(Ordering::Relaxed)),
            reason,
            message,
            at: now_ms(),
        });
    }
}

#[derive(Debug, Clone)]
pub enum ActivitySignal {
    Pending {
        approval_id: String,
        tool_call_id: String,
    },

    Resolved,
}

pub type ActivityNotifier = Arc<dyn Fn(&str, ActivitySignal) + Send + Sync>;

pub struct ActivityRegistry {
    hub: Arc<EventHub>,
    sessions: Mutex<HashMap<String, std::sync::Arc<ActivityTracker>>>,
}

impl ActivityRegistry {
    pub fn new(hub: Arc<EventHub>) -> Self {
        Self {
            hub,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    pub fn tracker(
        self: &std::sync::Arc<Self>,
        session_id: &str,
    ) -> std::sync::Arc<ActivityTracker> {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        sessions
            .entry(session_id.to_string())
            .or_insert_with(|| {
                std::sync::Arc::new(ActivityTracker::new(
                    session_id.to_string(),
                    self.hub.clone(),
                ))
            })
            .clone()
    }
}

use crate::callbacks::HostCallbacks;
use crate::rpc::types::{
    AskQuestionRequest, AskQuestionResponse, CheckpointRequest, ListToolsResponse, LlmChatRequest,
    LlmChatResponse, PermissionCheckRequest, PermissionDecision, StateReadRequest,
    StateReadResponse, StateWriteRequest, StateWriteResponse, ToolExecuteRequest,
    ToolExecuteResponse,
};
use crate::turn_loop::types::{GoalContext, LLMMessage};

pub struct ActivityCallbacks {
    pub inner: Arc<dyn HostCallbacks>,
    pub tracker: Arc<ActivityTracker>,
}

impl HostCallbacks for ActivityCallbacks {
    fn hook_guard(&self) -> Option<Arc<crate::tools::external_hooks::HookGuard>> {
        self.inner.hook_guard()
    }

    fn llm_chat(
        &self,
        request: LlmChatRequest,
    ) -> crate::rpc::types::BoxFuture<'static, Result<LlmChatResponse, String>> {
        self.inner.llm_chat(request)
    }

    fn execute_tool(
        &self,
        request: ToolExecuteRequest,
    ) -> crate::rpc::types::BoxFuture<'static, Result<ToolExecuteResponse, String>> {
        self.tracker
            .tool_started(&request.tool_call_id, &request.tool_name);
        self.inner.execute_tool(request)
    }

    fn check_permission(
        &self,
        request: PermissionCheckRequest,
    ) -> crate::rpc::types::BoxFuture<'static, Result<PermissionDecision, String>> {
        self.inner.check_permission(request)
    }

    fn ask_question(
        &self,
        request: AskQuestionRequest,
    ) -> crate::rpc::types::BoxFuture<'static, Result<AskQuestionResponse, String>> {
        self.inner.ask_question(request)
    }

    fn cancel_question(&self, question_id: &str, reason: &str) {
        self.inner.cancel_question(question_id, reason);
    }

    fn state_read(
        &self,
        request: StateReadRequest,
    ) -> crate::rpc::types::BoxFuture<'static, Result<StateReadResponse, String>> {
        self.inner.state_read(request)
    }

    fn state_write(
        &self,
        request: StateWriteRequest,
    ) -> crate::rpc::types::BoxFuture<'static, Result<StateWriteResponse, String>> {
        self.inner.state_write(request)
    }

    fn checkpoint(
        &self,
        request: CheckpointRequest,
    ) -> crate::rpc::types::BoxFuture<'static, Result<(), String>> {
        self.inner.checkpoint(request)
    }

    fn checkpoint_state(&self) -> crate::rpc::types::BoxFuture<'static, Result<(), String>> {
        self.inner.checkpoint_state()
    }

    fn list_tools(
        &self,
    ) -> crate::rpc::types::BoxFuture<'static, Result<ListToolsResponse, String>> {
        self.inner.list_tools()
    }

    fn goal(&self) -> crate::rpc::types::BoxFuture<'static, Result<Option<GoalContext>, String>> {
        self.inner.goal()
    }

    fn parse_bash(
        &self,
        request: crate::rpc::types::ParseBashRequest,
    ) -> crate::rpc::types::BoxFuture<'static, Result<Option<Vec<String>>, String>> {
        self.inner.parse_bash(request)
    }

    fn auth_token(
        &self,
        provider: String,
        force: bool,
    ) -> crate::rpc::types::BoxFuture<'static, Result<String, String>> {
        self.inner.auth_token(provider, force)
    }

    fn drain_steers(
        &self,
    ) -> crate::rpc::types::BoxFuture<'static, Result<Vec<LLMMessage>, String>> {
        self.inner.drain_steers()
    }

    fn set_turn_goal(&self, turn_id: &str, goal_id: Option<&str>) {
        self.inner.set_turn_goal(turn_id, goal_id);
    }

    fn emit_event(&self, event: serde_json::Value) {
        match event.get("type").and_then(|v| v.as_str()) {
            Some("llm.delta") => {
                if let Some(part) = event.get("part") {
                    self.tracker.llm_delta(part);
                }
            }
            Some("llm.step.begin") => self.tracker.step_begin(),
            _ => {}
        }
        self.inner.emit_event(event);
    }

    fn turn_event(&self, event: crate::turn_events::TurnEvent) {
        if let Ok(payload) = serde_json::to_value(&event) {
            self.tracker.publish_turn_event(payload);
        }
        self.inner.turn_event(event);
    }

    fn telemetry(&self, event: serde_json::Value) {
        if event.get("event").and_then(|v| v.as_str()) == Some("TurnStepRetrying") {
            self.tracker.retrying(
                event
                    .get("failed_attempt")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32,
                event
                    .get("next_attempt")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32,
                event
                    .get("max_attempts")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32,
                event.get("delay_ms").and_then(|v| v.as_u64()).unwrap_or(0),
            );
        }
        self.inner.telemetry(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn next_event(sub: &mut crate::server::hub::WsSubscription) -> Value {
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), sub.recv())
            .await
            .expect("event within 5s")
            .expect("hub open");
        serde_json::to_value(&event.event).unwrap()
    }

    #[derive(Default)]
    struct RecordingCallbacks {
        turn_events: std::sync::Mutex<Vec<crate::turn_events::TurnEvent>>,
    }

    impl HostCallbacks for RecordingCallbacks {
        fn llm_chat(
            &self,
            _request: LlmChatRequest,
        ) -> crate::rpc::types::BoxFuture<'static, Result<LlmChatResponse, String>> {
            Box::pin(async { Err("not used".into()) })
        }

        fn execute_tool(
            &self,
            _request: ToolExecuteRequest,
        ) -> crate::rpc::types::BoxFuture<'static, Result<ToolExecuteResponse, String>> {
            Box::pin(async { Err("not used".into()) })
        }

        fn check_permission(
            &self,
            _request: PermissionCheckRequest,
        ) -> crate::rpc::types::BoxFuture<'static, Result<PermissionDecision, String>> {
            Box::pin(async { Ok(PermissionDecision::allow()) })
        }

        fn turn_event(&self, event: crate::turn_events::TurnEvent) {
            self.turn_events
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(event);
        }
    }

    #[tokio::test]
    async fn turn_lifecycle_records_reach_the_wire() {
        let hub = Arc::new(EventHub::new());
        let tracker = Arc::new(ActivityTracker::new("sess-turn".into(), hub.clone()));
        let mut sub = hub.attach();

        let inner = Arc::new(RecordingCallbacks::default());
        let callbacks = ActivityCallbacks {
            inner: inner.clone(),
            tracker,
        };

        callbacks.turn_event(crate::turn_events::TurnEvent::Started {
            turn_id: 3,
            prompt_id: Some("prompt-3-test".to_string()),
            origin: serde_json::json!({ "kind": "user" }),
            prompt: Some("hello".to_string()),
            prompt_attachments: None,
        });
        let started = next_event(&mut sub).await;
        assert_eq!(started["type"], "turn.started");
        assert_eq!(started["turnId"], 3);

        callbacks.turn_event(crate::turn_events::TurnEvent::Ended {
            turn_id: 3,
            reason: crate::turn_events::TurnEndReason::Completed,
            error: None,
            duration_ms: Some(12),
            interrupt_reason: None,
            trace_id: None,
            stop_reason: None,
        });
        let ended = next_event(&mut sub).await;
        assert_eq!(ended["type"], "turn.ended");
        assert_eq!(ended["reason"], "completed");

        assert_eq!(
            inner
                .turn_events
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn the_fold_walks_the_whole_lifecycle() {
        let hub = Arc::new(EventHub::new());
        let tracker = ActivityTracker::new("sess-fold".into(), hub.clone());
        let mut sub = hub.attach();

        tracker.turn_started(7);
        let running = next_event(&mut sub).await;
        assert_eq!(running["type"], "agent.status.updated");
        assert_eq!(running["phase"]["kind"], "running");
        assert_eq!(running["phase"]["turnId"], 7);

        tracker.llm_delta(&serde_json::json!({ "type": "think", "think": "hmm" }));
        let streaming = next_event(&mut sub).await;
        assert_eq!(streaming["phase"]["kind"], "streaming");
        assert_eq!(streaming["phase"]["stream"], "thinking");

        tracker.tool_started("call_1", "Bash");
        let tool = next_event(&mut sub).await;
        assert_eq!(tool["phase"]["kind"], "tool_call");
        assert_eq!(tool["phase"]["toolCallId"], "call_1");
        assert_eq!(tool["phase"]["name"], "Bash");

        tracker.retrying(1, 2, 10, 500);
        let retrying = next_event(&mut sub).await;
        assert_eq!(retrying["phase"]["kind"], "retrying");
        assert_eq!(retrying["phase"]["failedAttempt"], 1);
        assert_eq!(retrying["phase"]["nextAttempt"], 2);
        assert_eq!(retrying["phase"]["delayMs"], 500);

        tracker.awaiting("appr_9", "call_2");
        let awaiting = next_event(&mut sub).await;
        assert_eq!(awaiting["phase"]["kind"], "awaiting_approval");
        assert_eq!(awaiting["phase"]["approval"]["approvalId"], "appr_9");
        assert_eq!(awaiting["phase"]["approval"]["toolCallId"], "call_2");

        tracker.interaction_resolved();
        let resumed = next_event(&mut sub).await;
        assert_eq!(resumed["phase"]["kind"], "running");

        tracker.turn_ended("completed");
        let ended = next_event(&mut sub).await;
        assert_eq!(ended["phase"]["kind"], "ended");
        assert_eq!(ended["phase"]["reason"], "completed");
        assert!(ended["phase"]["durationMs"].is_u64());
    }

    #[tokio::test]
    async fn identical_phases_are_not_republished() {
        let hub = Arc::new(EventHub::new());
        let tracker = ActivityTracker::new("sess-dedup".into(), hub.clone());
        let mut sub = hub.attach();

        tracker.llm_delta(&serde_json::json!({ "type": "text", "text": "a" }));

        let first = next_event(&mut sub).await;
        assert_eq!(first["phase"]["kind"], "streaming");

        tracker.llm_delta(&serde_json::json!({ "type": "text", "text": "b" }));
        tracker.llm_delta(&serde_json::json!({ "type": "text", "text": "c" }));
        let quiet = tokio::time::timeout(std::time::Duration::from_millis(50), sub.recv()).await;
        assert!(quiet.is_err(), "identical phases must stay silent");
    }

    #[tokio::test]
    async fn non_content_parts_and_stray_resolves_do_not_move_the_phase() {
        let hub = Arc::new(EventHub::new());
        let tracker = ActivityTracker::new("sess-still".into(), hub.clone());
        let mut sub = hub.attach();

        tracker.llm_delta(&serde_json::json!({ "type": "image", "mediaType": "image/png" }));
        tracker.interaction_resolved();
        let quiet = tokio::time::timeout(std::time::Duration::from_millis(50), sub.recv()).await;
        assert!(quiet.is_err(), "the tracker must stay idle");

        tracker.interrupted(
            InterruptReason::MaxSteps,
            Some("max_steps budget exhausted".into()),
        );
        let event = next_event(&mut sub).await;
        assert_eq!(event["phase"]["kind"], "interrupted");
        assert_eq!(event["phase"]["reason"], "max_steps");
        assert_eq!(event["phase"]["message"], "max_steps budget exhausted");
    }

    #[test]
    fn the_registry_returns_the_same_tracker_per_session() {
        let registry = Arc::new(ActivityRegistry::new(Arc::new(EventHub::new())));
        let a = registry.tracker("sess-1");
        let b = registry.tracker("sess-1");
        let c = registry.tracker("sess-2");
        assert!(Arc::ptr_eq(&a, &b), "same session → same tracker");
        assert!(
            !Arc::ptr_eq(&a, &c),
            "different session → different tracker"
        );
    }
}
