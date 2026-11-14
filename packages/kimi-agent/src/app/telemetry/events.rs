use std::marker::PhantomData;

use serde::{Deserialize, Serialize};

use crate::agent::usage::cacheBreak::CacheBreakCause;
use crate::features::tower::tower::TowerExitReason;
use crate::workspace::sessionLifecycle::sessionLifecycle::SessionCloseReason;

use super::context::{AgentTelemetryMode, TelemetryPrimitive, TelemetryProperties};

macro_rules! telemetry_string_enum {
    ($name:ident { $($variant:ident => $value:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
        pub enum $name {
            $(#[serde(rename = $value)] $variant),+
        }

        impl $name {
            pub fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $value),+
                }
            }
        }
    };
}

#[derive(Debug, Clone, Copy)]
pub struct TelemetryEventMeta {
    pub owner: &'static str,
    pub comment: &'static str,
    pub properties: &'static [(&'static str, &'static str)],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTelemetryEventContext {
    pub agent_id: String,
}

telemetry_string_enum!(WirePlanRecordType { PlanRevision => "plan.revision" });
telemetry_string_enum!(WirePlanLegacyField { Path => "path" });
telemetry_string_enum!(MigrationOutcome { Migrated => "migrated", Skipped => "skipped" });

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WirePlanRevisionMigratedEvent {
    pub record_type: WirePlanRecordType,
    pub legacy_field: WirePlanLegacyField,
    pub migration_outcome: MigrationOutcome,
}

pub const AGENT_TELEMETRY_CONTEXT_PROPERTIES: &[(&str, &str)] =
    &[("agent_id", "Agent id (main or subagent scope id)")];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelemetryEventContext {
    None,
    Agent,
}

impl TelemetryEventContext {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Agent => "agent",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct TelemetryEventDefinition<P = TelemetryProperties> {
    pub context: TelemetryEventContext,
    pub meta: TelemetryEventMeta,
    pub _properties: PhantomData<fn() -> P>,
}

pub const fn define_telemetry_event<P>(
    meta: TelemetryEventMeta,
) -> TelemetryEventDefinition<P> {
    TelemetryEventDefinition {
        context: TelemetryEventContext::None,
        meta,
        _properties: PhantomData,
    }
}

pub const fn define_agent_telemetry_event<P>(
    meta: TelemetryEventMeta,
) -> TelemetryEventDefinition<P> {
    TelemetryEventDefinition {
        context: TelemetryEventContext::Agent,
        meta,
        _properties: PhantomData,
    }
}

pub trait StrictPropertyCheck<T> {}
impl<T> StrictPropertyCheck<T> for T {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnStartedEvent {
    pub turn_id: u64,
    pub mode: AgentTelemetryMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_plugins: Option<String>,
}

telemetry_string_enum!(TurnInterruptReason {
    UserCancelled => "user_cancelled",
    Aborted => "aborted",
    MaxSteps => "max_steps",
    Error => "error",
    Filtered => "filtered",
    Blocked => "blocked"
});

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnInterruptedEvent {
    pub turn_id: u64,
    pub at_step: u64,
    pub mode: AgentTelemetryMode,
    pub interrupt_reason: TurnInterruptReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

telemetry_string_enum!(TurnEndedReason {
    Completed => "completed",
    Cancelled => "cancelled",
    Failed => "failed"
});

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnEndedEvent {
    pub turn_id: u64,
    pub reason: TurnEndedReason,
    pub duration_ms: u64,
    pub mode: AgentTelemetryMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_plugins: Option<String>,
}

telemetry_string_enum!(PromptCacheProbeSource { Fork => "fork", Compaction => "compaction" });

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptCacheProbeEvent {
    pub source: PromptCacheProbeSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    pub input_tokens: u64,
    pub input_cache_read: u64,
    pub input_cache_creation: u64,
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_match: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools_match: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_match: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_prefix_match: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_request_gap_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheBreakDetectedEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_no: Option<u64>,
    pub cause: CacheBreakCause,
    pub prev_model: String,
    pub curr_model: String,
    pub prev_effort: String,
    pub curr_effort: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev_projection: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub curr_projection: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev_message_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub curr_message_count: Option<u64>,
    pub prev_input_cache_read: u64,
    pub curr_input_cache_read: u64,
    pub prev_input_other: u64,
    pub curr_input_other: u64,
    pub prev_output: u64,
    pub curr_output: u64,
    pub prev_input_cache_creation: u64,
    pub curr_input_cache_creation: u64,
    pub cache_read_drop_ratio: f64,
    pub interval_ms: u64,
}

telemetry_string_enum!(ToolCallOutcome { Success => "success", Error => "error", Cancelled => "cancelled" });
telemetry_string_enum!(ToolCallDupType { Normal => "normal", SameStep => "same_step", CrossStep => "cross_step" });
telemetry_string_enum!(ToolCallErrorType { Cancelled => "cancelled", Error => "error" });

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallEvent {
    pub turn_id: u64,
    pub tool_call_id: String,
    pub tool_name: String,
    pub outcome: ToolCallOutcome,
    pub duration_ms: u64,
    pub dup_type: ToolCallDupType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_type: Option<ToolCallErrorType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiErrorEvent {
    pub error_type: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    pub retryable: bool,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_no: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_request_bytes: Option<u64>,
}

telemetry_string_enum!(SkillInvocationTrigger {
    UserSlash => "user-slash",
    ModelTool => "model-tool",
    NestedSkill => "nested-skill"
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillInvokedEvent {
    pub skill_name: String,
    pub trigger: SkillInvocationTrigger,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlowInvokedEvent {
    pub flow_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputSteerEvent {
    pub parts: u64,
}

telemetry_string_enum!(CancelFrom { Streaming => "streaming", Compacting => "compacting" });

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelEvent {
    pub from: CancelFrom,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationUndoEvent {
    pub count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct YoloToggleEvent {
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AfkToggleEvent {
    pub enabled: bool,
}

telemetry_string_enum!(TelemetryPermissionMode { Manual => "manual", Yolo => "yolo", Auto => "auto" });
telemetry_string_enum!(PermissionPolicyDecision { Approve => "approve", Deny => "deny", Ask => "ask" });

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionPolicyDecisionEvent {
    pub turn_id: u64,
    pub tool_call_id: String,
    pub policy_name: String,
    pub tool_name: String,
    pub permission_mode: TelemetryPermissionMode,
    pub decision: PermissionPolicyDecision,
    #[serde(flatten)]
    pub additional_properties: std::collections::HashMap<String, TelemetryPrimitive>,
}

telemetry_string_enum!(PermissionApprovalResult {
    Error => "error",
    ApprovedForSession => "approved_for_session",
    Approved => "approved",
    Rejected => "rejected",
    Cancelled => "cancelled"
});

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionApprovalResultEvent {
    pub turn_id: u64,
    pub tool_call_id: String,
    pub policy_name: Option<String>,
    pub tool_name: String,
    pub permission_mode: TelemetryPermissionMode,
    pub result: PermissionApprovalResult,
    pub approval_surface: String,
    pub duration_ms: u64,
    pub session_cache_written: bool,
    pub has_feedback: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanSubmittedEvent {
    pub has_options: bool,
}

telemetry_string_enum!(PlanResolvedOutcome {
    Approved => "approved",
    Dismissed => "dismissed",
    RejectedAndExited => "rejected_and_exited",
    Revise => "revise",
    Rejected => "rejected"
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanResolvedEvent {
    pub outcome: PlanResolvedOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chosen_option: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_feedback: Option<bool>,
}

telemetry_string_enum!(PlanEnterResolvedOutcome { AutoApproved => "auto_approved" });

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanEnterResolvedEvent {
    pub outcome: PlanEnterResolvedOutcome,
}

telemetry_string_enum!(SpecEnterResolvedOutcome { Accepted => "accepted" });

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecEnterResolvedEvent {
    pub outcome: SpecEnterResolvedOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecSubmittedEvent {
    pub file_count: u64,
}

telemetry_string_enum!(SpecResolvedOutcome {
    Approved => "approved",
    Dismissed => "dismissed",
    Rejected => "rejected",
    Revise => "revise",
    RejectedAndExited => "rejected_and_exited",
    ApprovedWithoutReview => "approved_without_review"
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecResolvedEvent {
    pub outcome: SpecResolvedOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_feedback: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanToTodoConvertedEvent {
    pub item_count: u64,
}

telemetry_string_enum!(TowerModeEnterOutcome { Entered => "entered", Rejected => "rejected" });
telemetry_string_enum!(TowerModeEnterReason {
    NotMainAgent => "not-main-agent",
    ExperimentOff => "experiment-off",
    FeatureNotAssembled => "feature-not-assembled",
    OwnedByLiveSession => "owned-by-live-session"
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TowerModeEnterEvent {
    pub outcome: TowerModeEnterOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<TowerModeEnterReason>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TowerModeExitEvent {
    pub reason: TowerExitReason,
}

telemetry_string_enum!(ModeTransitionTrigger { Manual => "manual", Task => "task", Tool => "tool" });

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmModeTransitionEvent {
    pub trigger: ModeTransitionTrigger,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloWorldModeTransitionEvent {
    pub trigger: ModeTransitionTrigger,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloWorldStageChangedEvent {
    pub stage: String,
    pub previous: String,
}

telemetry_string_enum!(ExternalHookAction { Allow => "allow", Block => "block" });

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalHookResolvedEvent {
    pub event: String,
    pub action: ExternalHookAction,
    pub matched_count: u64,
    pub failed_count: u64,
}

telemetry_string_enum!(RemoteControlToggleOutcome {
    Ok => "ok",
    AlreadyRunning => "already_running",
    Rejected => "rejected",
    Error => "error"
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteControlToggleEvent {
    pub enabled: bool,
    pub outcome: RemoteControlToggleOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginToggleEvent {
    pub plugin_id: String,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_plugins: Option<String>,
}

telemetry_string_enum!(CompactionSource { Manual => "manual", Auto => "auto" });

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompactionFinishedEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u64>,
    pub source: CompactionSource,
    pub tokens_before: u64,
    pub tokens_after: u64,
    pub duration_ms: u64,
    pub compacted_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dropped_count: Option<u64>,
    pub retry_count: u64,
    pub round: u64,
    pub thinking_effort: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_cache_read: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_cache_creation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MicroCompactionFinishedEvent {
    pub keep_recent_messages: u64,
    pub min_content_tokens: u64,
    pub cache_missed_threshold_ms: u64,
    pub truncated_marker: String,
    pub min_context_usage_ratio: f64,
    pub truncated_tool_result_count: u64,
    pub truncated_tool_result_tokens_before: u64,
    pub truncated_tool_result_tokens_after: u64,
    pub tokens_before: u64,
    pub tokens_after: u64,
    pub previous_cutoff: u64,
    pub cutoff: u64,
    pub message_count: u64,
    pub cache_age_ms: Option<u64>,
    pub thinking_effort: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompactionFailedEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u64>,
    pub source: CompactionSource,
    pub tokens_before: u64,
    pub duration_ms: u64,
    pub round: u64,
    pub retry_count: u64,
    pub thinking_effort: String,
    pub error_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextProjectionRepairedEvent {
    pub reordered: u64,
    pub synthesized: u64,
    pub dropped_orphan: u64,
    pub duplicate_calls_dropped: u64,
    pub duplicate_results_dropped: u64,
    pub leading_dropped: u64,
    pub assistants_merged: u64,
    pub whitespace_dropped: u64,
    pub vacuous_dropped: u64,
}

telemetry_string_enum!(BackgroundTaskCreatedKind { Bash => "bash", Agent => "agent", Question => "question" });
telemetry_string_enum!(BackgroundTaskCompletedKind { Agent => "agent", Process => "process", Question => "question" });
telemetry_string_enum!(BackgroundTaskStatus {
    Running => "running",
    Completed => "completed",
    Failed => "failed",
    TimedOut => "timed_out",
    Killed => "killed",
    Lost => "lost"
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundTaskCreatedEvent {
    pub task_id: String,
    pub kind: BackgroundTaskCreatedKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundTaskCompletedEvent {
    pub task_id: String,
    pub kind: BackgroundTaskCompletedKind,
    pub duration_ms: Option<u64>,
    pub status: BackgroundTaskStatus,
}

telemetry_string_enum!(WaitForCompletedOutcome {
    Completed => "completed",
    TimedOut => "timed_out",
    TaskNotFound => "task_not_found",
    Aborted => "aborted",
    Interrupted => "interrupted"
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitForCompletedEvent {
    pub outcome: WaitForCompletedOutcome,
    pub timeout_ms: u64,
    pub waited_ms: u64,
    pub has_task_id: bool,
    pub extra_completed_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSwitchEvent {
    pub model: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThinkingToggleEvent {
    pub enabled: bool,
    pub effort: String,
    pub from: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionDismissedEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

telemetry_string_enum!(QuestionAnsweredMethod { Enter => "enter", Space => "space", NumberKey => "number_key" });

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionAnsweredEvent {
    pub answered: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<QuestionAnsweredMethod>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

telemetry_string_enum!(TelemetryGoalActor { User => "user", Model => "model", Runtime => "runtime", System => "system" });

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalBudgetProperties {
    pub has_token_budget: bool,
    pub has_turn_budget: bool,
    pub has_wall_clock_budget: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalCreatedEvent {
    pub actor: TelemetryGoalActor,
    pub replace: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalBudgetSetEvent {
    #[serde(flatten)]
    pub budget: GoalBudgetProperties,
    pub actor: TelemetryGoalActor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalContinuedEvent {
    pub turns_used: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalClearedEvent {
    pub actor: TelemetryGoalActor,
}

telemetry_string_enum!(GoalStatus {
    Active => "active",
    Paused => "paused",
    Blocked => "blocked",
    Complete => "complete",
    BudgetLimited => "budget_limited",
    UsageLimited => "usage_limited"
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalStatusChangedEvent {
    #[serde(flatten)]
    pub budget: GoalBudgetProperties,
    pub actor: TelemetryGoalActor,
    pub status: GoalStatus,
    pub turns_used: u64,
    pub tokens_used: u64,
    pub wall_clock_ms: u64,
}

telemetry_string_enum!(ToolCallStepDupType { SameStep => "same_step", CrossStep => "cross_step" });

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallDedupDetectedEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u64>,
    pub step_no: u64,
    pub tool_call_id: String,
    pub tool_name: String,
    pub dup_type: ToolCallStepDupType,
    pub args_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

telemetry_string_enum!(ToolCallRepeatAction {
    None => "none",
    R1 => "r1",
    R2 => "r2",
    R3 => "r3",
    Stop => "stop"
});

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallRepeatEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u64>,
    pub tool_name: String,
    pub repeat_count: u64,
    pub action: ToolCallRepeatAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallTurnRepeatEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u64>,
    pub step_no: u64,
    pub tool_call_id: String,
    pub tool_name: String,
    pub turn_repeat_count: u64,
    pub args_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

telemetry_string_enum!(ToolCallRepeatHandoffOutcome { Text => "text", Vetoed => "vetoed" });

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallRepeatHandoffEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u64>,
    pub outcome: ToolCallRepeatHandoffOutcome,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentsMdReminderShownEvent {
    pub turn_id: u64,
    pub tool_name: String,
    pub reminded_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotifyUserNudgeShownEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u64>,
    pub rounds_since_notify: u64,
    pub nudge_index: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotifyUserSentEvent {
    pub turn_id: u64,
    pub rounds_since_notify: u64,
    pub after_nudge: bool,
    pub title_chars: u64,
    pub message_chars: u64,
    pub displayed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoListWrittenEvent {
    pub turn_id: u64,
    pub item_count: u64,
    pub open_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steps_since_write: Option<u64>,
    pub after_reminder: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoReminderShownEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u64>,
    pub steps_since_write: u64,
    pub item_count: u64,
    pub open_count: u64,
}

telemetry_string_enum!(TodoTurnEndedReason {
    Completed => "completed",
    Cancelled => "cancelled",
    Failed => "failed",
    Blocked => "blocked"
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoTurnEndedEvent {
    pub turn_id: u64,
    pub reason: TodoTurnEndedReason,
    pub item_count: u64,
    pub open_count: u64,
    pub updated_in_turn: bool,
}

telemetry_string_enum!(RgFallbackSource {
    ShareBinCached => "share-bin-cached",
    Vendor => "vendor",
    ShareBinDownloaded => "share-bin-downloaded"
});
telemetry_string_enum!(FallbackOutcome { Resolved => "resolved", Failed => "failed" });

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrepToolRgFallbackEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<RgFallbackSource>,
    pub outcome: FallbackOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobToolRgFallbackEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<RgFallbackSource>,
    pub outcome: FallbackOutcome,
}

telemetry_string_enum!(FsGrepNodeFallbackReason { RgMissing => "rg_missing" });
telemetry_string_enum!(FsSuggestNodeFallbackReason { RgMissing => "rg_missing", RgError => "rg_error" });

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsGrepNodeFallbackEvent {
    pub reason: FsGrepNodeFallbackReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsSuggestNodeFallbackEvent {
    pub reason: FsSuggestNodeFallbackReason,
}

telemetry_string_enum!(SubagentModelSource { Inherited => "inherited", SecondaryPool => "secondary_pool" });

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentCreatedEvent {
    pub subagent_name: String,
    pub run_in_background: bool,
    pub fork: bool,
    pub agent_id: String,
    pub parent_agent_id: String,
    pub parent_tool_call_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_source: Option<SubagentModelSource>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpConnectedEvent {
    pub server_count: u64,
    pub total_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpFailedEvent {
    pub failed_count: u64,
    pub total_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpInitialLoadWaitEvent {
    pub budget_ms: u64,
    pub pending_count: u64,
    pub total_count: u64,
}

telemetry_string_enum!(ToolRetrievalMode { Announce => "announce", Search => "search" });

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRetrievalEvent {
    pub mode: ToolRetrievalMode,
    pub catalog_size: u64,
    pub candidate_count: u64,
    pub selected_count: u64,
    pub query_chars: u64,
    pub matched: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpSchemaCompactedEvent {
    pub tool_count: u64,
    pub original_chars: u64,
    pub compacted_chars: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CronMissedEvent {
    pub count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CronScheduledEvent {
    pub recurring: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CronDeletedEvent {
    pub task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CronFiredEvent {
    pub recurring: bool,
    pub coalesced_count: u64,
    pub stale: bool,
    pub buffered: bool,
}

telemetry_string_enum!(ImageCompressOutcome {
    Compressed => "compressed",
    PassthroughFast => "passthrough_fast",
    PassthroughGuard => "passthrough_guard",
    PassthroughUnsupported => "passthrough_unsupported",
    PassthroughUnhelpful => "passthrough_unhelpful",
    PassthroughError => "passthrough_error"
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageCompressEvent {
    pub source: String,
    pub outcome: ImageCompressOutcome,
    pub input_mime: String,
    pub output_mime: String,
    pub original_bytes: u64,
    pub final_bytes: u64,
    pub original_width: u64,
    pub original_height: u64,
    pub final_width: u64,
    pub final_height: u64,
    pub exif_transposed: bool,
    pub duration_ms: u64,
}

telemetry_string_enum!(ImageCropErrorKind {
    Empty => "empty",
    UnsupportedFormat => "unsupported_format",
    RegionInvalid => "region_invalid",
    TooLarge => "too_large",
    OutOfBounds => "out_of_bounds",
    Budget => "budget",
    DecodeFailed => "decode_failed"
});

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageCropEvent {
    pub source: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<ImageCropErrorKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resized: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_width: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_height: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region_area_ratio: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_bytes: Option<u64>,
    pub duration_ms: u64,
}

telemetry_string_enum!(VideoUploadOutcome { Success => "success", Error => "error" });

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoUploadEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    pub mime_type: String,
    pub size_bytes: u64,
    pub outcome: VideoUploadOutcome,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStartedEvent {
    pub resumed: bool,
    pub experimental_flags: String,
}

telemetry_string_enum!(SessionTitleGenerationTrigger { Auto => "auto", Manual => "manual" });
telemetry_string_enum!(SessionTitleGenerationSource {
    UserPrompts => "user_prompts",
    FirstTurn => "first_turn",
    Digest => "digest"
});
telemetry_string_enum!(SessionTitleGenerationOutcome {
    Success => "success",
    Unavailable => "unavailable",
    Error => "error"
});
telemetry_string_enum!(SessionTitleGenerationReason {
    CustomTitle => "custom_title",
    NoAgent => "no_agent",
    NoInput => "no_input",
    ProviderUnavailable => "provider_unavailable",
    TokenUnavailable => "token_unavailable",
    OauthUnavailable => "oauth_unavailable",
    RequestFailed => "request_failed",
    Unexpected => "unexpected"
});
telemetry_string_enum!(SessionTitleOutcome { Ok => "ok", Truncated => "truncated" });

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTitleGenerationFinishedEvent {
    pub trigger: SessionTitleGenerationTrigger,
    pub source: SessionTitleGenerationSource,
    pub outcome: SessionTitleGenerationOutcome,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<SessionTitleGenerationReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_chars: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_outcome: Option<SessionTitleOutcome>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionLoadFailedEvent {
    pub reason: String,
}

telemetry_string_enum!(WireRepairKind { Corrupted => "corrupted", Truncated => "truncated" });
telemetry_string_enum!(WireRepairOutcome { Repaired => "repaired", Failed => "failed" });

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireRepairEvent {
    pub kind: WireRepairKind,
    pub outcome: WireRepairOutcome,
    pub dropped_count: u64,
    pub backup_created: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FirstLaunchEvent {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitEvent {
    pub duration_ms: u64,
}

telemetry_string_enum!(OauthLoginStatus {
    Authenticated => "authenticated",
    Cancelled => "cancelled",
    Expired => "expired",
    Denied => "denied",
    Failed => "failed"
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OauthLoginFinishedEvent {
    pub provider: String,
    pub status: OauthLoginStatus,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OauthModelsRefreshFinishedEvent {
    pub changed_count: u64,
    pub unchanged_count: u64,
    pub failed_count: u64,
}

telemetry_string_enum!(AuthEnsureReadyFailedReason {
    ProvisioningRequired => "provisioning_required",
    ModelNotResolved => "model_not_resolved",
    TokenMissing => "token_missing",
    Unexpected => "unexpected"
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthEnsureReadyFailedEvent {
    pub reason: AuthEnsureReadyFailedReason,
    pub has_model_override: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellCommandFinishedEvent {
    pub duration_ms: u64,
    pub is_error: bool,
    pub backgrounded: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCreateFailedEvent {
    pub agent_id: String,
    pub stage: String,
    pub error_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEndedEvent {
    pub reason: SessionCloseReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebFetchFallbackEvent {
    pub error_type: String,
    pub used_api_key: bool,
}

telemetry_string_enum!(MediaResolveKind { Image => "image", Video => "video" });
telemetry_string_enum!(MediaResolveFallbackReason {
    Unsupported => "unsupported",
    ReadFailed => "read_failed",
    UploadFailed => "upload_failed",
    Invalid => "invalid"
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaResolveFallbackEvent {
    pub kind: MediaResolveKind,
    pub reason: MediaResolveFallbackReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

telemetry_string_enum!(LlmRequestProjectionFallback {
    MediaDegraded => "media-degraded",
    MediaStripped => "media-stripped",
    Strict => "strict"
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmRequestProjectionFallbackEvent {
    pub projection: LlmRequestProjectionFallback,
    pub error_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionIndexDegradedEvent {
    pub reason: String,
    pub degraded_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionIndexProjectedEvent {
    pub duration_ms: u64,
    pub session_count: u64,
    pub generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionIndexMirrorGiveUpEvent {
    pub pending_count: u64,
    pub consecutive_failures: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceTrustChangedEvent {
    pub trusted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceTrustReadFailedEvent {
    pub error_type: String,
}

telemetry_string_enum!(EnvironmentType { Ssh => "ssh", Docker => "docker", Command => "command" });
telemetry_string_enum!(EnvironmentConnectedOutcome { Ok => "ok", Error => "error", Cancelled => "cancelled" });
telemetry_string_enum!(EnvironmentConnectedErrorType {
    Missing => "missing",
    Timeout => "timeout",
    Incompatible => "incompatible",
    Other => "other"
});
telemetry_string_enum!(EnvironmentDisconnectedReason {
    Eof => "eof",
    PipeError => "pipe_error",
    ProtocolError => "protocol_error",
    ClientClosed => "client_closed",
    Liveness => "liveness",
    Other => "other"
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentConnectedEvent {
    pub environment_type: EnvironmentType,
    pub duration_ms: u64,
    pub outcome: EnvironmentConnectedOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_type: Option<EnvironmentConnectedErrorType>,
    pub is_reconnect: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentDisconnectedEvent {
    pub environment_type: EnvironmentType,
    pub uptime_ms: u64,
    pub reason: EnvironmentDisconnectedReason,
}

const fn event_meta(
    comment: &'static str,
    properties: &'static [(&'static str, &'static str)],
) -> TelemetryEventMeta {
    TelemetryEventMeta {
        owner: "kimi-code",
        comment,
        properties,
    }
}

pub const TELEMETRY_EVENT_DEFINITIONS: &[(&str, TelemetryEventDefinition)] = &[
    ("environment_connected", define_telemetry_event(event_meta(
        "A physical remote environment connection attempt settles.",
        &[
            ("environment_type", "Launcher type"),
            ("duration_ms", "Connection attempt duration in milliseconds"),
            ("outcome", "Connection attempt outcome"),
            ("error_type", "Bounded handshake failure category"),
            ("is_reconnect", "Whether this launcher previously connected successfully"),
        ],
    ))),
    ("environment_disconnected", define_telemetry_event(event_meta(
        "An established remote environment connection unexpectedly closes.",
        &[
            ("environment_type", "Launcher type"),
            ("uptime_ms", "Established connection duration in milliseconds"),
            ("reason", "Bounded connection close category"),
        ],
    ))),
    ("wire_plan_revision_migrated", define_agent_telemetry_event(event_meta(
        "A legacy plan revision wire record is normalized during restore.",
        &[
            ("record_type", "Wire record type"),
            ("legacy_field", "Legacy field name"),
            ("migration_outcome", "Migration outcome"),
        ],
    ))),
    ("turn_started", define_agent_telemetry_event(event_meta(
        "A turn starts running.",
        &[
            ("turn_id", "Per-agent turn index (main or subagent); pair with agent_id to locate a turn within a session"),
            ("mode", "Agent mode the turn runs in"),
            ("provider_type", "Provider protocol type"),
            ("protocol", "Request protocol"),
            ("thinking_effort", "Effective thinking effort the turn runs with"),
            ("enabled_plugins", "Comma-separated sorted ids of enabled, loaded plugins when the turn starts; empty string for a known empty set, absent when no plugin snapshot is available"),
        ],
    ))),
    ("turn_interrupted", define_agent_telemetry_event(event_meta(
        "A running turn is interrupted.",
        &[
            ("turn_id", "Per-agent turn index (main or subagent); pair with agent_id to locate a turn within a session"),
            ("at_step", "Step index the turn reached before interruption"),
            ("mode", "Agent mode the turn ran in"),
            ("interrupt_reason", "Why the turn was interrupted"),
            ("provider_type", "Provider protocol type"),
            ("protocol", "Request protocol"),
            ("thinking_effort", "Effective thinking effort the turn ran with"),
            ("trace_id", "Trace id of the most recent LLM request in this turn (the failed request when the turn errored); absent for non-Kimi protocols"),
        ],
    ))),
    ("turn_ended", define_agent_telemetry_event(event_meta(
        "A turn ends, unconditionally.",
        &[
            ("turn_id", "Per-agent turn index (main or subagent); pair with agent_id to locate a turn within a session"),
            ("reason", "How the turn ended"),
            ("duration_ms", "Turn wall-clock time in milliseconds"),
            ("mode", "Agent mode the turn ran in"),
            ("error_type", "Classified error category when reason is failed"),
            ("provider_type", "Provider protocol type"),
            ("protocol", "Request protocol"),
            ("thinking_effort", "Effective thinking effort the turn ran with"),
            ("trace_id", "Trace id of the most recent LLM request in this turn; absent for non-Kimi protocols"),
            ("enabled_plugins", "Comma-separated sorted ids of enabled, loaded plugins when the turn ends; empty string for a known empty set, absent when no plugin snapshot is available"),
        ],
    ))),
    ("prompt_cache_probe", define_agent_telemetry_event(event_meta(
        "A request expected to reuse an earlier prompt cache reports its cache usage and how its provider-visible prefix compares with the baseline turn request.",
        &[
            ("source", "Why a cache hit was expected for this request"),
            ("turn_id", "Per-agent turn index of the probed request; absent for manual compaction between turns"),
            ("provider_type", "Provider protocol type"),
            ("protocol", "Request protocol"),
            ("input_tokens", "Total input tokens of the probed request (other + cache read + cache creation)"),
            ("input_cache_read", "Cache-read input tokens of the probed request"),
            ("input_cache_creation", "Cache-creation input tokens of the probed request"),
            ("output_tokens", "Output tokens of the probed request"),
            ("model_match", "Whether the baseline and probed requests use the same model; absent without a baseline"),
            ("tools_match", "Whether the provider-visible tool table hash equals the baseline request; absent without a usable baseline or when the model or context revision differs"),
            ("system_prompt_match", "Whether the system prompt hash equals the baseline request; absent without a usable baseline or when the model or context revision differs"),
            ("history_prefix_match", "For compaction, whether the projected messages start with the exact baseline history; absent for fork or without a usable baseline"),
            ("parent_input_tokens", "Total input tokens of the baseline turn request (the parent before fork, or the previous turn before compaction); absent without a usable baseline on the same model"),
            ("parent_request_gap_ms", "Milliseconds from the baseline turn request finishing to the probed request starting; absent without a usable baseline or when the model or context revision differs"),
        ],
    ))),
    ("cache_break_detected", define_agent_telemetry_event(event_meta(
        "A completed step read substantially less of the prompt cache than the previous step, indicating a likely cache invalidation.",
        &[
            ("turn_id", "Per-agent turn index of the step that read less cache"),
            ("step_no", "One-based step number within that turn"),
            ("cause", "Classified cause of the cache-read drop"),
            ("prev_model", "Model alias used by the previous step"),
            ("curr_model", "Model alias used by the current step"),
            ("prev_effort", "Thinking effort used by the previous step"),
            ("curr_effort", "Thinking effort used by the current step"),
            ("prev_projection", "Media projection applied to the previous step, absent when none was"),
            ("curr_projection", "Media projection applied to the current step, absent when none was"),
            ("prev_message_count", "Message count sent by the previous step"),
            ("curr_message_count", "Message count sent by the current step"),
            ("prev_input_cache_read", "Cache-read input tokens of the previous step"),
            ("curr_input_cache_read", "Cache-read input tokens of the current step"),
            ("prev_input_other", "Uncached input tokens of the previous step"),
            ("curr_input_other", "Uncached input tokens of the current step"),
            ("prev_output", "Output tokens of the previous step"),
            ("curr_output", "Output tokens of the current step"),
            ("prev_input_cache_creation", "Cache-creation input tokens of the previous step"),
            ("curr_input_cache_creation", "Cache-creation input tokens of the current step"),
            ("cache_read_drop_ratio", "Fraction of the previous cache-read tokens that disappeared"),
            ("interval_ms", "Milliseconds from the previous step completion to the current one"),
        ],
    ))),
    ("tool_call", define_agent_telemetry_event(event_meta(
        "A tool call finishes execution.",
        &[
            ("turn_id", "Per-agent turn index (main or subagent); pair with agent_id to locate a turn within a session"),
            ("tool_call_id", "Provider-assigned tool call id"),
            ("tool_name", "Registered tool name"),
            ("outcome", "Execution outcome"),
            ("duration_ms", "Wall-clock execution time in milliseconds"),
            ("dup_type", "Whether the call was a duplicate within the same step or across steps"),
            ("error_type", "Error category when the call failed"),
            ("trace_id", "Trace id of the LLM request that produced this tool call; absent for non-Kimi protocols"),
        ],
    ))),
    ("api_error", define_agent_telemetry_event(event_meta(
        "An LLM API request fails.",
        &[
            ("error_type", "Classified error category"),
            ("model", "Model id the request targeted"),
            ("alias", "Model alias the request targeted"),
            ("retryable", "Whether the error is retryable"),
            ("duration_ms", "Request wall-clock time in milliseconds"),
            ("status_code", "HTTP status code when available"),
            ("provider_type", "Provider protocol type"),
            ("protocol", "Request protocol"),
            ("input_tokens", "Current turn's accumulated total input tokens"),
            ("turn_id", "Per-agent turn index when the request belongs to a turn; omitted for out-of-turn operations"),
            ("request_kind", "Request source vocabulary: 'turn' for turn requests, the operation's requestKind (e.g. 'full_compaction') otherwise"),
            ("step_no", "Step index within the turn, when the request belongs to a turn step"),
            ("trace_id", "Trace id of the failed request, from its response headers or its error response; absent when the failure happened before any response headers arrived (network errors, local aborts), and for non-Kimi protocols"),
            ("estimated_request_bytes", "Estimated outgoing request body size in bytes; absent when the failure happened before a request body was assembled"),
        ],
    ))),
    ("skill_invoked", define_agent_telemetry_event(event_meta(
        "A skill is invoked.",
        &[("skill_name", "Skill name"), ("trigger", "How the skill was triggered")],
    ))),
    ("flow_invoked", define_agent_telemetry_event(event_meta(
        "A flow-type skill is invoked.",
        &[("flow_name", "Flow name")],
    ))),
    ("input_steer", define_agent_telemetry_event(event_meta(
        "The user steers input while a turn is running.",
        &[("parts", "Number of input parts")],
    ))),
    ("cancel", define_agent_telemetry_event(event_meta(
        "The user cancels ongoing work.",
        &[
            ("from", "What was running when cancelled"),
            ("trace_id", "Trace id of the in-flight request, or of the most recent request between steps; absent for non-Kimi protocols"),
        ],
    ))),
    ("conversation_undo", define_agent_telemetry_event(event_meta(
        "The user undoes conversation entries.",
        &[("count", "Number of entries undone")],
    ))),
    ("yolo_toggle", define_agent_telemetry_event(event_meta(
        "Yolo permission mode is toggled.",
        &[("enabled", "Whether yolo mode is now enabled")],
    ))),
    ("afk_toggle", define_agent_telemetry_event(event_meta(
        "AFK (auto) permission mode is toggled.",
        &[("enabled", "Whether auto mode is now enabled")],
    ))),
    ("permission_policy_decision", define_agent_telemetry_event(event_meta(
        "A permission policy evaluates a tool call.",
        &[
            ("turn_id", "Per-agent turn index (main or subagent); pair with agent_id to locate a turn within a session"),
            ("tool_call_id", "Provider-assigned tool call id"),
            ("policy_name", "Name of the deciding policy"),
            ("tool_name", "Tool being gated"),
            ("permission_mode", "Active permission mode"),
            ("decision", "Policy decision"),
        ],
    ))),
    ("permission_approval_result", define_agent_telemetry_event(event_meta(
        "A permission approval prompt resolves.",
        &[
            ("turn_id", "Per-agent turn index (main or subagent); pair with agent_id to locate a turn within a session"),
            ("tool_call_id", "Provider-assigned tool call id"),
            ("policy_name", "Name of the asking policy, null when unknown"),
            ("tool_name", "Tool being approved"),
            ("permission_mode", "Active permission mode"),
            ("result", "How the approval resolved"),
            ("approval_surface", "UI surface that presented the approval"),
            ("duration_ms", "Time the approval took in milliseconds"),
            ("session_cache_written", "Whether a session approval rule was cached"),
            ("has_feedback", "Whether the user attached feedback"),
            ("trace_id", "Trace id of the LLM request that produced the gated tool call; absent for non-Kimi protocols"),
        ],
    ))),
    ("plan_submitted", define_agent_telemetry_event(event_meta(
        "A plan is submitted for review.",
        &[("has_options", "Whether the plan offered selectable options")],
    ))),
    ("plan_resolved", define_agent_telemetry_event(event_meta(
        "A submitted plan is resolved.",
        &[
            ("outcome", "How the plan was resolved"),
            ("chosen_option", "Label of the option the user chose"),
            ("has_feedback", "Whether the user attached revision feedback"),
        ],
    ))),
    ("plan_enter_resolved", define_agent_telemetry_event(event_meta(
        "A request to enter plan mode is resolved.",
        &[("outcome", "How the request was resolved")],
    ))),
    ("spec_enter_resolved", define_agent_telemetry_event(event_meta(
        "A request to enter spec mode is resolved.",
        &[("outcome", "How the request was resolved")],
    ))),
    ("spec_submitted", define_agent_telemetry_event(event_meta(
        "A spec is submitted for review.",
        &[("file_count", "Number of spec documents presented")],
    ))),
    ("spec_resolved", define_agent_telemetry_event(event_meta(
        "A submitted spec is resolved by the user.",
        &[("outcome", "How the spec was resolved"), ("has_feedback", "Whether the user attached revision feedback")],
    ))),
    ("plan_to_todo_converted", define_agent_telemetry_event(event_meta(
        "Plan-mode exit seeded the todo list from the approved plan.",
        &[("item_count", "Number of todo items written (milestones + leaves).")],
    ))),
    ("tower_mode_enter", define_agent_telemetry_event(event_meta(
        "A request to enter tower mode resolves.",
        &[("outcome", "Whether tower mode was entered or the request was rejected"), ("reason", "Why the request was rejected; omitted when tower mode was entered")],
    ))),
    ("tower_mode_exit", define_agent_telemetry_event(event_meta(
        "Tower mode is exited.",
        &[("reason", "Why tower mode was exited: the user turned it off, another session took the tower over, or a foreign tower was reconciled away")],
    ))),
    ("swarm_mode_entered", define_agent_telemetry_event(event_meta(
        "Swarm mode is entered.",
        &[("trigger", "What triggered swarm mode")],
    ))),
    ("swarm_mode_exited", define_agent_telemetry_event(event_meta(
        "Swarm mode is exited.",
        &[("trigger", "What originally triggered the swarm mode being exited")],
    ))),
    ("hello_world_mode_entered", define_agent_telemetry_event(event_meta(
        "Hello-world mode is entered.",
        &[("trigger", "What triggered hello-world mode")],
    ))),
    ("hello_world_mode_exited", define_agent_telemetry_event(event_meta(
        "Hello-world mode is exited.",
        &[("trigger", "What originally triggered the hello-world mode being exited")],
    ))),
    ("hello_world_stage_changed", define_agent_telemetry_event(event_meta(
        "The hello-world run advances to a new stage.",
        &[("stage", "The stage the run advanced to"), ("previous", "The stage the run was in before the change")],
    ))),
    ("external_hook_resolved", define_telemetry_event(event_meta(
        "An external hook trigger finishes running its matched hooks.",
        &[
            ("event", "Hook event type (e.g. PreToolUse, UserPromptSubmit, Stop)"),
            ("action", "Whether the trigger resolved to allow or block"),
            ("matched_count", "Number of hooks that ran for the trigger"),
            ("failed_count", "Number of hooks that failed (timeout, spawn error, or a non-zero exit code other than 2)"),
        ],
    ))),
    ("remote_control_toggle", define_telemetry_event(event_meta(
        "A request to toggle the Remote Control tunnel resolves.",
        &[("enabled", "Whether the request was to enable or disable the tunnel"), ("outcome", "How the request resolved")],
    ))),
    ("plugin_toggle", define_telemetry_event(event_meta(
        "An installed plugin is enabled or disabled.",
        &[("plugin_id", "Id of the toggled plugin"), ("enabled", "Whether the plugin is enabled after the toggle"), ("enabled_plugins", "Comma-separated sorted ids of enabled, loaded plugins after the toggle commits; empty string for a known empty set")],
    ))),
    ("compaction_finished", define_agent_telemetry_event(event_meta(
        "Context compaction completes.",
        &[
            ("turn_id", "Per-agent turn index when compaction ran inside a turn; omitted for manual compaction between turns"),
            ("source", "Whether compaction was triggered manually or automatically"),
            ("tokens_before", "Token count before compaction"),
            ("tokens_after", "Token count after compaction"),
            ("duration_ms", "Compaction wall-clock time in milliseconds"),
            ("compacted_count", "Number of entries compacted"),
            ("dropped_count", "Number of entries dropped"),
            ("retry_count", "Number of retries attempted"),
            ("round", "Compaction round index"),
            ("thinking_effort", "Thinking effort level in effect"),
            ("input_tokens", "Total input tokens (other + cache read + cache creation)"),
            ("output_tokens", "Output tokens"),
            ("input_cache_read", "Cache-read input tokens"),
            ("input_cache_creation", "Cache-creation input tokens"),
            ("trace_id", "Trace id of the final compaction request round; absent for non-Kimi protocols"),
        ],
    ))),
    ("micro_compaction_finished", define_agent_telemetry_event(event_meta(
        "Cache-miss micro compaction advances the tool-result truncation cutoff.",
        &[
            ("keep_recent_messages", "Number of trailing messages exempt from truncation"),
            ("min_content_tokens", "Minimum content tokens for a tool result to be truncated"),
            ("cache_missed_threshold_ms", "Idle time after the last assistant output that counts as a cache miss"),
            ("truncated_marker", "Text marker replacing a truncated tool result"),
            ("min_context_usage_ratio", "Minimum context-window usage ratio for truncation to apply"),
            ("truncated_tool_result_count", "Number of tool results truncated by the new cutoff"),
            ("truncated_tool_result_tokens_before", "Tokens of the truncated tool results before truncation"),
            ("truncated_tool_result_tokens_after", "Tokens of the truncated tool results after truncation"),
            ("tokens_before", "Whole-context tokens before this cutoff change"),
            ("tokens_after", "Whole-context tokens after this cutoff change"),
            ("previous_cutoff", "Truncation cutoff before this change"),
            ("cutoff", "Truncation cutoff after this change"),
            ("message_count", "Context message count when the cutoff advanced"),
            ("cache_age_ms", "Cache age at detection in milliseconds, null when the last assistant output is unknown"),
            ("thinking_effort", "Thinking effort level in effect"),
        ],
    ))),
    ("compaction_failed", define_agent_telemetry_event(event_meta(
        "Context compaction fails.",
        &[
            ("turn_id", "Per-agent turn index when compaction ran inside a turn; omitted for manual compaction between turns"),
            ("source", "Whether compaction was triggered manually or automatically"),
            ("tokens_before", "Token count before compaction"),
            ("duration_ms", "Wall-clock time until failure in milliseconds"),
            ("round", "Compaction round index"),
            ("retry_count", "Number of retries attempted"),
            ("thinking_effort", "Thinking effort level in effect"),
            ("error_type", "Error class name"),
            ("trace_id", "Trace id of the failed compaction request, from its response headers or its error response; absent when the failure happened before any request or before response headers arrived (network errors), and for non-Kimi protocols"),
        ],
    ))),
    ("context_projection_repaired", define_agent_telemetry_event(event_meta(
        "The context projector repairs the outgoing request to keep it wire-valid.",
        &[
            ("reordered", "Tool results moved back next to their call"),
            ("synthesized", "Placeholder results invented for lost ones"),
            ("dropped_orphan", "Results with no matching call dropped"),
            ("duplicate_calls_dropped", "Tool calls with an already-seen id dropped"),
            ("duplicate_results_dropped", "Second results for an already-answered id dropped"),
            ("leading_dropped", "Leading non-user messages dropped"),
            ("assistants_merged", "Consecutive assistant messages merged"),
            ("whitespace_dropped", "Whitespace-only text blocks dropped"),
            ("vacuous_dropped", "Messages dropped because every recorded part serialized to nothing"),
        ],
    ))),
    ("background_task_created", define_agent_telemetry_event(event_meta(
        "A background task is created.",
        &[("task_id", "Background task id; joins background_task_created with background_task_completed"), ("kind", "Task kind; process tasks retain the legacy bash value")],
    ))),
    ("background_task_completed", define_agent_telemetry_event(event_meta(
        "A background task reaches a terminal state.",
        &[("task_id", "Background task id; joins background_task_created with background_task_completed"), ("kind", "Task kind"), ("duration_ms", "Task wall-clock time in milliseconds, null when unknown"), ("status", "Terminal task status")],
    ))),
    ("wait_for_completed", define_agent_telemetry_event(event_meta(
        "A WaitFor tool call returns.",
        &[
            ("outcome", "How the wait ended: the waited task finished, the wait timed out, the task id was unknown, or the wait was aborted"),
            ("timeout_ms", "Timeout argument in milliseconds"),
            ("waited_ms", "Actual wall-clock wait time in milliseconds"),
            ("has_task_id", "Whether a specific task id was given"),
            ("extra_completed_count", "Number of additional tasks that finished within the wait window"),
        ],
    ))),
    ("model_switch", define_agent_telemetry_event(event_meta(
        "The active model is bound or switched.",
        &[("model", "Model alias")],
    ))),
    ("thinking_toggle", define_agent_telemetry_event(event_meta(
        "Thinking effort is toggled.",
        &[("enabled", "Whether thinking is now enabled"), ("effort", "New thinking effort level"), ("from", "Previous thinking effort level")],
    ))),
    ("question_dismissed", define_agent_telemetry_event(event_meta(
        "A user question prompt is dismissed.",
        &[("trace_id", "Trace id of the LLM request that produced the questioning tool call; absent for non-Kimi protocols")],
    ))),
    ("question_answered", define_agent_telemetry_event(event_meta(
        "A user question prompt is answered.",
        &[("answered", "Number of questions answered"), ("method", "Input method used to answer"), ("trace_id", "Trace id of the LLM request that produced the questioning tool call; absent for non-Kimi protocols")],
    ))),
    ("goal_created", define_agent_telemetry_event(event_meta(
        "A goal is created.",
        &[("actor", "Who created the goal"), ("replace", "Whether the goal replaces an existing one")],
    ))),
    ("goal_budget_set", define_agent_telemetry_event(event_meta(
        "A goal budget is set.",
        &[("actor", "Who set the budget"), ("has_token_budget", "Whether a token budget was set"), ("has_turn_budget", "Whether a turn budget was set"), ("has_wall_clock_budget", "Whether a wall-clock budget was set")],
    ))),
    ("goal_continued", define_agent_telemetry_event(event_meta(
        "A goal continues into another turn.",
        &[("turns_used", "Turns consumed so far")],
    ))),
    ("goal_cleared", define_agent_telemetry_event(event_meta(
        "A goal is cleared.",
        &[("actor", "Who cleared the goal")],
    ))),
    ("goal_status_changed", define_agent_telemetry_event(event_meta(
        "A goal changes status.",
        &[
            ("actor", "Who changed the status"),
            ("status", "New goal status"),
            ("turns_used", "Turns consumed so far"),
            ("tokens_used", "Tokens consumed so far"),
            ("wall_clock_ms", "Wall-clock time consumed so far in milliseconds"),
            ("has_token_budget", "Whether a token budget was set"),
            ("has_turn_budget", "Whether a turn budget was set"),
            ("has_wall_clock_budget", "Whether a wall-clock budget was set"),
        ],
    ))),
    ("tool_call_dedup_detected", define_agent_telemetry_event(event_meta(
        "A duplicate tool call is detected.",
        &[
            ("turn_id", "Per-agent turn index (main or subagent); pair with agent_id to locate a turn within a session; omitted when no turn is active"),
            ("step_no", "Step index within the turn"),
            ("tool_call_id", "Provider-assigned tool call id"),
            ("tool_name", "Registered tool name"),
            ("dup_type", "Whether the duplicate is within the same step or across steps"),
            ("args_hash", "Hash of the tool call arguments"),
            ("trace_id", "Trace id of the LLM request that produced the duplicate tool call; absent for non-Kimi protocols"),
        ],
    ))),
    ("tool_call_repeat", define_agent_telemetry_event(event_meta(
        "A repeated tool call streak is detected.",
        &[
            ("turn_id", "Per-agent turn index (main or subagent); pair with agent_id to locate a turn within a session; omitted when no turn is active"),
            ("tool_name", "Registered tool name"),
            ("repeat_count", "Length of the repeat streak"),
            ("action", "Intervention action taken"),
            ("trace_id", "Trace id of the LLM request that produced the repeated tool call; absent for non-Kimi protocols"),
        ],
    ))),
    ("tool_call_turn_repeat", define_agent_telemetry_event(event_meta(
        "A tool call reappears within the same turn.",
        &[
            ("turn_id", "Per-agent turn index (main or subagent); pair with agent_id to locate a turn within a session; omitted when no turn is active"),
            ("step_no", "Step index within the turn"),
            ("tool_call_id", "Provider-assigned tool call id"),
            ("tool_name", "Registered tool name"),
            ("turn_repeat_count", "Number of prior-step tool-call reappearances counted in the turn"),
            ("args_hash", "Hash of the tool call arguments"),
            ("trace_id", "Trace id of the LLM request that produced the repeated tool call; absent for non-Kimi protocols"),
        ],
    ))),
    ("tool_call_repeat_handoff", define_agent_telemetry_event(event_meta(
        "The text-only handoff step that follows a repeat-breaker force stop finished.",
        &[("turn_id", "Per-agent turn index (main or subagent); pair with agent_id to locate a turn within a session; omitted when no turn is active"), ("outcome", "Whether the model answered in text or its tool calls were vetoed")],
    ))),
    ("agents_md_reminder_shown", define_agent_telemetry_event(event_meta(
        "An AGENTS.md discovery reminder is queued for context injection after a tool call.",
        &[
            ("turn_id", "Per-agent turn index (main or subagent); pair with agent_id to locate a turn within a session"),
            ("tool_name", "Registered tool name whose execution discovered the file"),
            ("reminded_count", "Number of AGENTS.md paths listed in the reminder"),
            ("trace_id", "Trace id of the LLM request that produced the tool call; absent for non-Kimi protocols"),
        ],
    ))),
    ("notify_user_nudge_shown", define_agent_telemetry_event(event_meta(
        "A reminder asking the agent to post a NotifyUser update is queued for context injection.",
        &[
            ("turn_id", "Per-agent turn index (main or subagent); pair with agent_id to locate a turn within a session; omitted when no turn is active"),
            ("rounds_since_notify", "Tool-call rounds (assistant steps with tool calls) since the turn started or the last NotifyUser call"),
            ("nudge_index", "Position of this reminder within the current silent stretch; 1 for the first, higher when earlier reminders went unanswered"),
        ],
    ))),
    ("notify_user_sent", define_agent_telemetry_event(event_meta(
        "The agent posted a NotifyUser progress update.",
        &[
            ("turn_id", "Per-agent turn index (main or subagent); pair with agent_id to locate a turn within a session"),
            ("rounds_since_notify", "Tool-call rounds (assistant steps with tool calls) before this update since the turn started or the previous NotifyUser call"),
            ("after_nudge", "Whether a NotifyUser reminder was injected since the turn started or the previous NotifyUser call"),
            ("title_chars", "Length of the update title in UTF-16 code units; the title text itself is not recorded"),
            ("message_chars", "Length of the update message in UTF-16 code units; the message text itself is not recorded"),
            ("displayed", "Whether the update was shown to the user; false when the feature was turned off mid-session or the host cannot display updates"),
        ],
    ))),
    ("todo_list_written", define_agent_telemetry_event(event_meta(
        "The agent replaced or cleared its todo list through the TodoList tool.",
        &[
            ("turn_id", "Per-agent turn index (main or subagent); pair with agent_id to locate a turn within a session"),
            ("item_count", "Number of items in the list after the write; 0 when the list was cleared"),
            ("open_count", "Number of items still pending or in_progress after the write"),
            ("steps_since_write", "Assistant steps between the previous TodoList write in context and this one; omitted when no earlier write is in context"),
            ("after_reminder", "Whether a stale todo list reminder was injected since the previous TodoList write"),
        ],
    ))),
    ("todo_reminder_shown", define_agent_telemetry_event(event_meta(
        "A reminder that the todo list still has open items and has not been updated recently is queued for context injection.",
        &[("turn_id", "Per-agent turn index (main or subagent); pair with agent_id to locate a turn within a session; omitted when no turn is active"), ("steps_since_write", "Assistant steps since the last TodoList write in context"), ("item_count", "Number of items in the list"), ("open_count", "Number of items still pending or in_progress")],
    ))),
    ("todo_turn_ended", define_agent_telemetry_event(event_meta(
        "A main-agent turn ended while the todo list was non-empty or had changed during the turn.",
        &[
            ("turn_id", "Per-agent turn index (main or subagent); pair with agent_id to locate a turn within a session"),
            ("reason", "How the turn ended"),
            ("item_count", "Number of items in the list when the turn ended; 0 when the list was cleared during the turn"),
            ("open_count", "Number of items still pending or in_progress when the turn ended"),
            ("updated_in_turn", "Whether the list changed during this turn"),
        ],
    ))),
    ("grep_tool_rg_fallback", define_agent_telemetry_event(event_meta(
        "The grep tool falls back when resolving ripgrep.",
        &[("source", "Where ripgrep was resolved from"), ("outcome", "Whether the fallback resolved or failed")],
    ))),
    ("glob_tool_rg_fallback", define_agent_telemetry_event(event_meta(
        "The glob tool falls back when resolving ripgrep.",
        &[("source", "Where ripgrep was resolved from"), ("outcome", "Whether the fallback resolved or failed")],
    ))),
    ("fs_grep_node_fallback", define_telemetry_event(event_meta(
        "The fs grep path falls back to the node implementation.",
        &[("reason", "Why the fallback was taken")],
    ))),
    ("fs_suggest_node_fallback", define_telemetry_event(event_meta(
        "The fs suggest path falls back to the node implementation.",
        &[("reason", "Why the fallback was taken")],
    ))),
    ("subagent_created", define_telemetry_event(event_meta(
        "A subagent run is created.",
        &[
            ("subagent_name", "Profile name of the subagent"),
            ("run_in_background", "Whether the subagent runs in the background"),
            ("fork", "Whether the subagent was forked with a snapshot of the parent conversation history"),
            ("agent_id", "Child agent id"),
            ("parent_agent_id", "Parent (caller) agent id"),
            ("parent_tool_call_id", "Tool call id of the launching call in the parent agent; '' when not launched from a tool call"),
            ("model", "Model alias the subagent binds to (secondary-model choice or inherited caller model); omitted when no binding was resolved"),
            ("model_source", "How the bound model was chosen: 'inherited' = caller's own model (no pool or fork), 'secondary_pool' = [secondary_model.models] pool pick; omitted when no binding resolution happened (e.g. resume)"),
        ],
    ))),
    ("mcp_connected", define_telemetry_event(event_meta(
        "MCP servers connect at session start.",
        &[("server_count", "Number of servers connected"), ("total_count", "Total number of configured servers")],
    ))),
    ("mcp_failed", define_telemetry_event(event_meta(
        "MCP servers fail to connect at session start.",
        &[("failed_count", "Number of servers that failed"), ("total_count", "Total number of configured servers")],
    ))),
    ("mcp_initial_load_wait", define_agent_telemetry_event(event_meta(
        "A turn proceeded before all MCP servers finished their initial connection.",
        &[("budget_ms", "Maximum time the turn waited for MCP initial load"), ("pending_count", "MCP servers still connecting when the wait budget elapsed"), ("total_count", "Total configured MCP servers")],
    ))),
    ("tool_retrieval", define_agent_telemetry_event(event_meta(
        "A deferred tool catalog is ranked for a turn or searched by capability.",
        &[("mode", "Whether the catalog was ranked for an announcement or searched by query"), ("catalog_size", "Number of loadable tools in the catalog"), ("candidate_count", "Number of tools considered for ranking or search"), ("selected_count", "Number of tools returned or announced"), ("query_chars", "Character length of the retrieval query, not its content"), ("matched", "Whether at least one positive retrieval match was found")],
    ))),
    ("mcp_schema_compacted", define_telemetry_event(event_meta(
        "An MCP server connected with compacted tool schemas enabled.",
        &[("tool_count", "Number of tools discovered from the server"), ("original_chars", "Serialized character length of the original parameter schemas"), ("compacted_chars", "Serialized character length after compaction")],
    ))),
    ("cron_missed", define_telemetry_event(event_meta(
        "Cron tasks fire late after being slept through.",
        &[("count", "Number of tasks that missed their fire time")],
    ))),
    ("cron_scheduled", define_telemetry_event(event_meta(
        "A cron task is scheduled.",
        &[("recurring", "Whether the task repeats"), ("agent_id", "Agent that scheduled the task; omitted for session-level scheduling")],
    ))),
    ("cron_deleted", define_telemetry_event(event_meta(
        "A cron task is deleted.",
        &[("task_id", "Cron task id"), ("agent_id", "Agent that deleted the task; omitted for session-level deletion (e.g. stale auto-removal)")],
    ))),
    ("cron_fired", define_telemetry_event(event_meta(
        "A cron task fires.",
        &[("recurring", "Whether the task repeats"), ("coalesced_count", "How many ideal fires collapsed into this delivery"), ("stale", "Whether the task fired past its staleness threshold"), ("buffered", "Whether the fire was buffered while a turn was running")],
    ))),
    ("image_compress", define_telemetry_event(event_meta(
        "An image is compressed before being sent to the model.",
        &[
            ("source", "Where the image came from"),
            ("outcome", "Compression outcome"),
            ("input_mime", "Input MIME type"),
            ("output_mime", "Output MIME type"),
            ("original_bytes", "Input size in bytes"),
            ("final_bytes", "Output size in bytes"),
            ("original_width", "Input width in pixels"),
            ("original_height", "Input height in pixels"),
            ("final_width", "Output width in pixels"),
            ("final_height", "Output height in pixels"),
            ("exif_transposed", "Whether EXIF orientation was applied"),
            ("duration_ms", "Compression wall-clock time in milliseconds"),
        ],
    ))),
    ("image_crop", define_telemetry_event(event_meta(
        "An image is cropped to a region before being sent to the model.",
        &[("source", "Where the image came from"), ("ok", "Whether the crop succeeded"), ("error_kind", "Failure category when the crop failed"), ("resized", "Whether the crop was resized"), ("original_width", "Input width in pixels"), ("original_height", "Input height in pixels"), ("region_area_ratio", "Cropped region area relative to the original"), ("final_bytes", "Output size in bytes"), ("duration_ms", "Crop wall-clock time in milliseconds")],
    ))),
    ("video_upload", define_agent_telemetry_event(event_meta(
        "A video is uploaded for the model.",
        &[("model", "Model the video is uploaded for"), ("provider_type", "Provider protocol type"), ("protocol", "Upload protocol"), ("mime_type", "Video MIME type"), ("size_bytes", "Video size in bytes"), ("outcome", "Upload outcome"), ("duration_ms", "Upload wall-clock time in milliseconds"), ("error_type", "Error class name when the upload failed")],
    ))),
    ("session_started", define_telemetry_event(event_meta(
        "A session becomes active (created, forked, or resumed).",
        &[("resumed", "Whether the session was resumed from disk"), ("experimental_flags", "Sorted comma-separated ids of enabled experimental flags, empty when none are enabled")],
    ))),
    ("session_title_generation_finished", define_telemetry_event(event_meta(
        "An actual session title generation attempt reaches a terminal result.",
        &[
            ("trigger", "Automatic or explicitly forced manual generation"),
            ("source", "Conversation excerpt used for generation"),
            ("outcome", "Generation result; skipped calls for existing titles are not recorded"),
            ("duration_ms", "Wall-clock time from the generation attempt to its terminal result"),
            ("reason", "Bounded unavailable or error category"),
            ("error_type", "Error class name for an unexpected failure"),
            ("title_chars", "Applied title length in UTF-16 code units; the title text is not recorded"),
            ("title_outcome", "Whether the generated title was truncated before being applied"),
        ],
    ))),
    ("session_load_failed", define_telemetry_event(event_meta(
        "A session resume fails.",
        &[("reason", "Error code, error name, or unknown")],
    ))),
    ("wire_repair", define_telemetry_event(event_meta(
        "A corrupted wire journal is truncated to its valid prefix and healed on disk.",
        &[("kind", "Corruption kind: unparseable middle line or torn final line"), ("outcome", "Whether the on-disk repair succeeded"), ("dropped_count", "Journal lines dropped from the corrupted tail"), ("backup_created", "Whether a first-time .bak backup of the corrupted file was created")],
    ))),
    ("first_launch", define_telemetry_event(event_meta(
        "The CLI runs for the first time on this device.",
        &[],
    ))),
    ("exit", define_telemetry_event(event_meta(
        "A CLI run exits.",
        &[("duration_ms", "Run wall-clock time in milliseconds")],
    ))),
    ("oauth_login_finished", define_telemetry_event(event_meta(
        "An OAuth login flow reaches a terminal status.",
        &[("provider", "OAuth provider name"), ("status", "Terminal status of the login flow"), ("duration_ms", "Login flow wall-clock time in milliseconds")],
    ))),
    ("oauth_models_refresh_finished", define_telemetry_event(event_meta(
        "A refresh of the managed OAuth provider model catalog finishes.",
        &[("changed_count", "Number of models added or updated by the refresh"), ("unchanged_count", "Number of models left unchanged"), ("failed_count", "Number of models that failed to refresh")],
    ))),
    ("auth_ensure_ready_failed", define_telemetry_event(event_meta(
        "Auth readiness check fails before a turn can start.",
        &[("reason", "Why auth is not ready"), ("has_model_override", "Whether a model override is configured")],
    ))),
    ("shell_command_finished", define_agent_telemetry_event(event_meta(
        "A shell command execution finishes; this path bypasses the tool executor.",
        &[("duration_ms", "Execution wall-clock time in milliseconds"), ("is_error", "Whether the execution ended with an error"), ("backgrounded", "Whether the command was sent to the background")],
    ))),
    ("agent_create_failed", define_telemetry_event(event_meta(
        "Agent scope creation fails partway through.",
        &[("agent_id", "Id of the agent whose creation failed"), ("stage", "Creation stage the failure occurred in"), ("error_type", "Classified error category")],
    ))),
    ("session_ended", define_telemetry_event(event_meta(
        "A session is closed or archived.",
        &[("reason", "How the session ended")],
    ))),
    ("web_fetch_fallback", define_telemetry_event(event_meta(
        "The managed fetch-url provider fails and the call silently falls back to the local fetcher.",
        &[("error_type", "Classified error category of the managed fetch failure"), ("used_api_key", "Whether a managed access token was obtained before the failure")],
    ))),
    ("media_resolve_fallback", define_agent_telemetry_event(event_meta(
        "A media part is silently degraded or replaced while resolving model input.",
        &[("kind", "Media kind being resolved"), ("reason", "Why the media could not be resolved as-is"), ("model", "Model the media was resolved for")],
    ))),
    ("llm_request_projection_fallback", define_agent_telemetry_event(event_meta(
        "A rejected LLM request is retried with a degraded context projection.",
        &[("projection", "Projection policy the request is degraded to"), ("error_type", "Classified error category of the rejection"), ("model", "Model that rejected the request"), ("turn_id", "Per-agent turn index; pair with agent_id to locate a turn within a session")],
    ))),
    ("session_index_degraded", define_telemetry_event(event_meta(
        "The session index read model degrades to the authoritative directory scan.",
        &[("reason", "Why the read model degraded"), ("degraded_count", "How many times the read model has degraded so far"), ("error_type", "Classified error category when degradation was caused by an error")],
    ))),
    ("session_index_projected", define_telemetry_event(event_meta(
        "The session index finishes projecting the sessions directory into the read model.",
        &[("duration_ms", "Projection wall-clock time in milliseconds"), ("session_count", "Number of sessions projected"), ("generation", "Read model generation after this projection")],
    ))),
    ("session_index_mirror_give_up", define_telemetry_event(event_meta(
        "The session index mirror stops retrying after consecutive write failures.",
        &[("pending_count", "Number of queued mirror writes left pending"), ("consecutive_failures", "Number of consecutive write failures that triggered the give-up")],
    ))),
    ("workspace_trust_changed", define_telemetry_event(event_meta(
        "A workspace is trusted or untrusted.",
        &[("trusted", "Whether the workspace is now trusted")],
    ))),
    ("workspace_trust_read_failed", define_telemetry_event(event_meta(
        "Reading the workspace trust record fails and the error is propagated to the caller.",
        &[("error_type", "Classified error category")],
    ))),
];

pub type TelemetryEventRegistry = &'static [(&'static str, TelemetryEventDefinition)];
pub type TelemetryEventName = &'static str;
pub type TelemetryEventPayload = TelemetryProperties;
pub type TelemetryEventProperties = TelemetryProperties;
