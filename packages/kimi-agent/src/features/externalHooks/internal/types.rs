use serde::{Deserialize, Serialize};

use crate::rpc::types::ContentBlock;

pub const HOOK_EVENT_TYPES: &[&str] = &[
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "PermissionRequest",
    "PermissionResult",
    "UserPromptSubmit",
    "UserPromptQueued",
    "TurnStarted",
    "Stop",
    "StopFailure",
    "Interrupt",
    "SessionStart",
    "SessionEnd",
    "SessionHeartbeat",
    "SubagentStart",
    "SubagentStop",
    "TaskStarted",
    "PreCompact",
    "PostCompact",
    "Notification",
];

pub type HookEventType = &'static str;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookDef {
    #[serde(deserialize_with = "crate::features::externalHooks::configSection::deserialize_hook_event")]
    pub event: String,
    #[serde(default)]
    pub matcher: String,
    #[serde(deserialize_with = "crate::features::externalHooks::configSection::deserialize_hook_command")]
    pub command: String,
    #[serde(default, deserialize_with = "crate::features::externalHooks::configSection::deserialize_hook_timeout")]
    pub timeout: Option<u64>,
    #[serde(default, deserialize_with = "crate::features::externalHooks::configSection::deserialize_hook_cwd")]
    pub cwd: Option<String>,
    #[serde(default, deserialize_with = "crate::features::externalHooks::configSection::deserialize_hook_env")]
    pub env: Option<std::collections::HashMap<String, String>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HookAction {
    #[default]
    Allow,
    Block,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookResult {
    pub action: HookAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timed_out: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub errored: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_output: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_truncated: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct HookBlockDecision {
    pub block: bool,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum HookMatcherValue {
    Text(String),
    Parts(Vec<ContentBlock>),
}

impl HookMatcherValue {
    pub fn text(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Parts(parts) => parts
                .iter()
                .filter_map(|part| match part {
                    ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" "),
        }
    }
}
