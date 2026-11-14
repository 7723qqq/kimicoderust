use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Active,
    Paused,
    Blocked,
    Complete,
    BudgetLimited,
    UsageLimited,
}

impl GoalStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Blocked => "blocked",
            Self::Complete => "complete",
            Self::BudgetLimited => "budget_limited",
            Self::UsageLimited => "usage_limited",
        }
    }
}

impl From<&str> for GoalStatus {
    fn from(value: &str) -> Self {
        match value {
            "paused" => Self::Paused,
            "blocked" => Self::Blocked,
            "complete" => Self::Complete,
            "budget_limited" => Self::BudgetLimited,
            "usage_limited" => Self::UsageLimited,
            _ => Self::Active,
        }
    }
}

impl std::fmt::Display for GoalStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalActor {
    User,
    Model,
    Runtime,
    System,
}

impl GoalActor {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Model => "model",
            Self::Runtime => "runtime",
            Self::System => "system",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalAccountingMode {
    ActiveStatusOnly,
    ActiveOnly,
    ActiveOrComplete,
    ActiveOrStopped,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalBudgetLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_budget: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_clock_budget_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalBudgetReport {
    pub token_budget: Option<u64>,
    pub turn_budget: Option<u64>,
    pub wall_clock_budget_ms: Option<i64>,
    pub remaining_tokens: Option<u64>,
    pub remaining_turns: Option<u64>,
    pub remaining_wall_clock_ms: Option<i64>,
    pub token_budget_reached: bool,
    pub turn_budget_reached: bool,
    pub wall_clock_budget_reached: bool,
    pub over_budget: bool,
    pub input_tokens_used: u64,
    pub output_tokens_used: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalState {
    pub goal_id: String,
    pub objective: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_criterion: Option<String>,
    pub status: GoalStatus,
    pub turns_used: u64,
    pub tokens_used: u64,
    pub input_tokens_used: u64,
    pub output_tokens_used: u64,
    pub wall_clock_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_clock_resumed_at: Option<i64>,
    pub budget_limits: GoalBudgetLimits,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_reason: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

pub type GoalModelState = Option<GoalState>;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalForkNoticeState {
    pub goal_present: bool,
    pub reminder_pending: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalRuntimeState {
    pub goal: GoalModelState,
    pub fork_notice: GoalForkNoticeState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalSnapshot {
    pub goal_id: String,
    pub objective: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_criterion: Option<String>,
    pub status: GoalStatus,
    pub turns_used: u64,
    pub tokens_used: u64,
    pub input_tokens_used: u64,
    pub output_tokens_used: u64,
    pub wall_clock_ms: i64,
    pub budget: GoalBudgetReport,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_streak: Option<u64>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalToolResult {
    pub goal: Option<GoalSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalChangeStats {
    pub turns_used: u64,
    pub tokens_used: u64,
    pub wall_clock_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalChangeKind {
    Lifecycle,
    Completion,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalChange {
    pub kind: GoalChangeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stats: Option<GoalChangeStats>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<GoalActor>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateGoalInput {
    pub objective: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_criterion: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replace: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditGoalObjectiveInput {
    pub objective: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GoalReasonInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumeGoalInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continue_if_paused: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continue_if_blocked: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct PendingContinuation {
    pub prompt_id: String,
    pub goal_id: String,
    pub turn_id: Option<u64>,
    pub turn: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct ResumeContinuation {
    pub turn_id: Option<u64>,
    pub goal_id: String,
}

#[derive(Debug, Clone, Default)]
pub struct GoalEffectState {
    pub goal_driven_turns: HashMap<u64, String>,
    pub counted_goal_turns: HashSet<u64>,
    pub goal_starter_turns: HashSet<u64>,
    pub goal_outcome_tool_result_turns: HashMap<u64, String>,
    pub goal_outcome_continuation_turns: HashSet<u64>,
    pub budget_grace_turns: HashSet<u64>,
    pub pending_continuation_goals: HashMap<u64, String>,
    pub goal_turn_targets: HashMap<u64, String>,
    pub turn_objective_revisions: HashMap<u64, u64>,
    pub objective_revision: u64,
    pub exhausted_turn_budget_goals: HashMap<u64, String>,
    pub live_turn_id: Option<u64>,
    pub live_wall_clock_started_at: Option<i64>,
    pub pending_continuation: Option<PendingContinuation>,
    pub resume_continuation: Option<ResumeContinuation>,
}

#[derive(Debug, Clone, Default)]
pub struct LifecycleOptions {
    pub preserve_live_continuation: bool,
    pub cancellation_reason: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct ClearOptions {
    pub emit: bool,
    pub track: bool,
    pub preserve_live_continuation: bool,
}

impl Default for ClearOptions {
    fn default() -> Self {
        Self {
            emit: true,
            track: true,
            preserve_live_continuation: false,
        }
    }
}
