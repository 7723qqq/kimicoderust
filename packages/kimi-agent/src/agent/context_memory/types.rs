use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::agent::task::types::AgentTaskStatus;
use crate::human::llm::message::{ContentPart, ToolCall, ToolDescription};
pub use crate::human::llm::message::Role;
use crate::human::llm::tokens::EstimableMessage;
use crate::human::llm::usage::TokenUsage;
use crate::llm_adapter::contract::message::Message;
use crate::tool::tool_input_display::ToolInputDisplay;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillSource {
    Project,
    User,
    Extra,
    Builtin,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptFileAttachment {
    pub name: String,
    pub media_type: String,
    pub size: u64,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BundledSkillActivation {
    pub activation_id: String,
    pub skill_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_args: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_source: Option<SkillSource>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserPromptOrigin {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_turn: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_metadata: Option<Vec<Map<String, Value>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_activations: Option<Vec<BundledSkillActivation>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Vec<PromptFileAttachment>>,
}

pub fn user_prompt_origin() -> UserPromptOrigin {
    UserPromptOrigin {
        kind: "user".to_string(),
        in_turn: None,
        client_metadata: None,
        skill_activations: None,
        attachments: None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SkillActivationTrigger {
    UserSlash,
    ModelTool,
    NestedSkill,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillActivationOrigin {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_turn: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_metadata: Option<Vec<Map<String, Value>>>,
    pub activation_id: String,
    pub skill_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_args: Option<String>,
    pub trigger: SkillActivationTrigger,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_source: Option<SkillSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Vec<PromptFileAttachment>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PluginCommandTrigger {
    UserSlash,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginCommandOrigin {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_turn: Option<bool>,
    pub activation_id: String,
    pub plugin_id: String,
    pub command_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_args: Option<String>,
    pub trigger: PluginCommandTrigger,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InjectionOrigin {
    pub kind: String,
    pub variant: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_prompt_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disclosure: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShellCommandPhase {
    Input,
    Output,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellCommandOrigin {
    pub kind: String,
    pub phase: ShellCommandPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionSummaryOrigin {
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemTriggerOrigin {
    pub kind: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background_helper: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskOrigin {
    pub kind: String,
    pub task_id: String,
    pub status: AgentTaskStatus,
    pub notification_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CronJobOrigin {
    pub kind: String,
    pub job_id: String,
    pub cron: String,
    pub recurring: bool,
    pub coalesced_count: u32,
    pub stale: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CronMissedOrigin {
    pub kind: String,
    pub count: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HookResultOrigin {
    pub kind: String,
    pub event: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetryOrigin {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum PromptOrigin {
    User(UserPromptOrigin),
    SkillActivation(SkillActivationOrigin),
    PluginCommand(PluginCommandOrigin),
    Injection(InjectionOrigin),
    ShellCommand(ShellCommandOrigin),
    CompactionSummary(CompactionSummaryOrigin),
    SystemTrigger(SystemTriggerOrigin),
    Task(TaskOrigin),
    CronJob(CronJobOrigin),
    CronMissed(CronMissedOrigin),
    HookResult(HookResultOrigin),
    Retry(RetryOrigin),
}

impl<'de> Deserialize<'de> for PromptOrigin {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let kind = value
            .get("kind")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| serde::de::Error::missing_field("kind"))?;
        let origin = match kind.as_str() {
            "user" => serde_json::from_value(value).map(Self::User),
            "skill_activation" => serde_json::from_value(value).map(Self::SkillActivation),
            "plugin_command" => serde_json::from_value(value).map(Self::PluginCommand),
            "injection" => serde_json::from_value(value).map(Self::Injection),
            "shell_command" => serde_json::from_value(value).map(Self::ShellCommand),
            "compaction_summary" => serde_json::from_value(value).map(Self::CompactionSummary),
            "system_trigger" => serde_json::from_value(value).map(Self::SystemTrigger),
            "task" => serde_json::from_value(value).map(Self::Task),
            "cron_job" => serde_json::from_value(value).map(Self::CronJob),
            "cron_missed" => serde_json::from_value(value).map(Self::CronMissed),
            "hook_result" => serde_json::from_value(value).map(Self::HookResult),
            "retry" => serde_json::from_value(value).map(Self::Retry),
            _ => {
                return Err(serde::de::Error::unknown_variant(
                    &kind,
                    &[
                        "user",
                        "skill_activation",
                        "plugin_command",
                        "injection",
                        "shell_command",
                        "compaction_summary",
                        "system_trigger",
                        "task",
                        "cron_job",
                        "cron_missed",
                        "hook_result",
                        "retry",
                    ],
                ));
            }
        };
        origin.map_err(serde::de::Error::custom)
    }
}

impl PromptOrigin {
    pub fn injection_variant(&self) -> Option<&str> {
        match self {
            Self::Injection(origin) if origin.kind == "injection" => {
                Some(origin.variant.as_str())
            }
            _ => None,
        }
    }

    pub fn injection_disclosure(&self) -> Option<&Value> {
        match self {
            Self::Injection(origin) if origin.kind == "injection" => origin.disclosure.as_ref(),
            _ => None,
        }
    }

    pub fn kind(&self) -> &str {
        match self {
            Self::User(origin) => &origin.kind,
            Self::SkillActivation(origin) => &origin.kind,
            Self::PluginCommand(origin) => &origin.kind,
            Self::Injection(origin) => &origin.kind,
            Self::ShellCommand(origin) => &origin.kind,
            Self::CompactionSummary(origin) => &origin.kind,
            Self::SystemTrigger(origin) => &origin.kind,
            Self::Task(origin) => &origin.kind,
            Self::CronJob(origin) => &origin.kind,
            Self::CronMissed(origin) => &origin.kind,
            Self::HookResult(origin) => &origin.kind,
            Self::Retry(origin) => &origin.kind,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextMessageTiming {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_first_token_latency_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_stream_duration_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextMessage {
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub content: Vec<ContentPart>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDescription>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<PromptOrigin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_displays: Option<HashMap<String, ToolInputDisplay>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_timing: Option<ContextMessageTiming>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

impl ContextMessage {
    pub fn new(role: Role, content: Vec<ContentPart>) -> Self {
        Self {
            role,
            name: None,
            content,
            tool_calls: Vec::new(),
            tool_call_id: None,
            partial: None,
            tools: None,
            id: None,
            provider_message_id: None,
            origin: None,
            is_error: None,
            tool_call_displays: None,
            note: None,
            usage: None,
            llm_timing: None,
            duration_ms: None,
        }
    }

    pub fn estimable(&self) -> EstimableMessage {
        EstimableMessage {
            role: match self.role {
                Role::System => "system",
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "tool",
            }
            .to_string(),
            content: self.content.clone(),
            tool_calls: Some(self.tool_calls.clone()),
        }
    }
}

impl From<Message> for ContextMessage {
    fn from(message: Message) -> Self {
        let base = Self::new(message.role, message.content.clone());
        Self {
            role: message.role,
            name: message.name,
            content: message.content,
            tool_calls: message.tool_calls,
            tool_call_id: message.tool_call_id,
            partial: message.partial,
            tools: message.tools,
            ..base
        }
    }
}

impl From<ContextMessage> for Message {
    fn from(message: ContextMessage) -> Self {
        Self {
            role: message.role,
            name: message.name,
            content: message.content,
            tool_calls: message.tool_calls,
            tool_call_id: message.tool_call_id,
            partial: message.partial,
            tools: message.tools,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserMessageRecord {
    pub content: Vec<ContentPart>,
    pub origin: PromptOrigin,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemReminderRecord {
    pub content: String,
    pub origin: PromptOrigin,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentContextData {
    pub history: Vec<ContextMessage>,
    pub token_count: u64,
}
