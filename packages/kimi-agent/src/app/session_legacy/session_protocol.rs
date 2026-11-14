use std::sync::LazyLock;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::session::sessionActivity::sessionActivity::{SessionPendingInteraction, SessionTurnOutcome};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionWarningSeverity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionWarning {
    pub code: String,
    pub message: String,
    pub severity: SessionWarningSeverity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionWarningsResponse {
    pub warnings: Vec<SessionWarning>,
}

pub type PromptThinking = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptPermissionMode {
    Manual,
    Yolo,
    Auto,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMetadata {
    pub cwd: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SessionMetadataPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GoalControl {
    Pause,
    Resume,
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionAgentConfig {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_servers: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<PromptThinking>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<PromptPermissionMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_mode: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swarm_mode: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hello_world_mode: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tower_mode: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tower_base: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_objective: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_control: Option<GoalControl>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SessionAgentConfigPartial {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_servers: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<PromptThinking>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<PromptPermissionMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_mode: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swarm_mode: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hello_world_mode: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tower_mode: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tower_base: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_objective: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_control: Option<GoalControl>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionRuleMatcherKind {
    #[serde(rename = "command_prefix")]
    CommandPrefix,
    #[serde(rename = "path_glob")]
    PathGlob,
    #[serde(rename = "exact_input")]
    ExactInput,
    #[serde(rename = "always")]
    Always,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRuleMatcher {
    pub kind: PermissionRuleMatcherKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionRuleDecision {
    Approved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionRuleCreatedBy {
    User,
    Agent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRule {
    pub id: String,
    pub tool_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matcher: Option<PermissionRuleMatcher>,
    pub decision: PermissionRuleDecision,
    pub created_at: String,
    pub created_by: PermissionRuleCreatedBy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct UpdateSessionProfileRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<SessionMetadataPatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_config: Option<SessionAgentConfigPartial>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_rules: Option<Vec<PermissionRule>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_cost_usd: Option<f64>,
    pub context_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_limit: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_count: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub workspace_id: String,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
    pub busy: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub main_turn_active: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_interaction: Option<SessionPendingInteraction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_turn_reason: Option<SessionTurnOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_prompt_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_prompt: Option<String>,
    pub metadata: SessionMetadata,
    pub agent_config: SessionAgentConfig,
    pub usage: SessionUsage,
    pub permission_rules: Vec<PermissionRule>,
    pub message_count: u64,
    pub last_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionCreatedEvent {
    #[serde(rename = "type")]
    pub type_: String,
    pub session: Session,
}

impl SessionCreatedEvent {
    pub fn new(session: Session) -> Self {
        Self {
            type_: "event.session.created".to_string(),
            session,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Idle,
    Running,
    AwaitingApproval,
    AwaitingQuestion,
    Aborted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionStatusChangedEvent {
    #[serde(rename = "type")]
    pub type_: String,
    pub status: SessionStatus,
    pub previous_status: SessionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_prompt_id: Option<String>,
}

impl SessionStatusChangedEvent {
    pub fn new(status: SessionStatus, previous_status: SessionStatus, current_prompt_id: Option<String>) -> Self {
        Self {
            type_: "event.session.status_changed".to_string(),
            status,
            previous_status,
            current_prompt_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionWorkChangedEvent {
    #[serde(rename = "type")]
    pub type_: String,
    pub busy: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub main_turn_active: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_interaction: Option<SessionPendingInteraction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_turn_reason: Option<SessionTurnOutcome>,
}

impl SessionWorkChangedEvent {
    pub fn new(busy: bool, main_turn_active: Option<bool>, pending_interaction: Option<SessionPendingInteraction>, last_turn_reason: Option<SessionTurnOutcome>) -> Self {
        Self {
            type_: "event.session.work_changed".to_string(),
            busy,
            main_turn_active,
            pending_interaction,
            last_turn_reason,
        }
    }
}

pub static SESSION_WARNING_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "required": ["code", "message", "severity"],
        "properties": {
            "code": { "type": "string" },
            "message": { "type": "string" },
            "severity": { "enum": ["info", "warning", "error"] }
        }
    })
});

pub static SESSION_WARNINGS_RESPONSE_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "required": ["warnings"],
        "properties": {
            "warnings": { "type": "array", "items": SESSION_WARNING_SCHEMA.clone() }
        }
    })
});

pub static PROMPT_THINKING_SCHEMA: LazyLock<Value> = LazyLock::new(|| json!({ "type": "string", "minLength": 1 }));
pub static PROMPT_PERMISSION_MODE_SCHEMA: LazyLock<Value> = LazyLock::new(|| json!({ "enum": ["manual", "yolo", "auto"] }));

pub static SESSION_METADATA_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "required": ["cwd"],
        "properties": { "cwd": { "type": "string", "minLength": 1 } },
        "additionalProperties": true
    })
});

fn agent_config_properties() -> Value {
    json!({
        "model": { "type": "string" },
        "system_prompt": { "type": "string" },
        "tools": { "type": "array", "items": { "type": "string" } },
        "mcp_servers": { "type": "array", "items": { "type": "string" } },
        "thinking": PROMPT_THINKING_SCHEMA.clone(),
        "permission_mode": PROMPT_PERMISSION_MODE_SCHEMA.clone(),
        "plan_mode": { "type": "boolean" },
        "swarm_mode": { "type": "boolean" },
        "hello_world_mode": { "type": "boolean" },
        "tower_mode": { "type": "boolean" },
        "tower_base": { "type": "string", "minLength": 1 },
        "goal_objective": { "type": "string" },
        "goal_control": { "enum": ["pause", "resume", "cancel"] }
    })
}

pub static SESSION_AGENT_CONFIG_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "required": ["model"],
        "properties": agent_config_properties()
    })
});

pub static SESSION_AGENT_CONFIG_PARTIAL_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "properties": agent_config_properties(),
        "additionalProperties": false
    })
});

pub static PERMISSION_RULE_MATCHER_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "required": ["kind"],
        "properties": {
            "kind": { "enum": ["command_prefix", "path_glob", "exact_input", "always"] },
            "value": { "type": "string" }
        }
    })
});

pub static PERMISSION_RULE_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "required": ["id", "tool_name", "decision", "created_at", "created_by"],
        "properties": {
            "id": { "type": "string", "minLength": 1 },
            "tool_name": { "type": "string", "minLength": 1 },
            "matcher": PERMISSION_RULE_MATCHER_SCHEMA.clone(),
            "decision": { "const": "approved" },
            "created_at": { "type": "string", "format": "date-time" },
            "created_by": { "enum": ["user", "agent"] }
        }
    })
});

pub static UPDATE_SESSION_PROFILE_REQUEST_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "properties": {
            "title": { "type": "string", "minLength": 1 },
            "metadata": SESSION_METADATA_SCHEMA.clone(),
            "agent_config": SESSION_AGENT_CONFIG_PARTIAL_SCHEMA.clone(),
            "permission_rules": { "type": "array", "items": PERMISSION_RULE_SCHEMA.clone() }
        },
        "additionalProperties": false
    })
});
