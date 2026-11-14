use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub use crate::session::subagent::spawn::DEFAULT_PROFILE_NAME;

pub const RESUMED_LABEL: &str = "subagent";
pub const BACKGROUND_AGENT_UNAVAILABLE: &str = "Background agent execution is not available for this agent because TaskList, TaskOutput, and TaskStop are not enabled.";
pub const RESUME_WITH_TYPE_UNAVAILABLE: &str =
    "Cannot set subagent_type when resuming an existing agent. Resume by agent id only.";
pub const USER_INTERRUPTED_SUBAGENT_MESSAGE: &str =
    "The subagent was stopped before it finished by user.";
pub const SUBAGENT_STOPPED_MESSAGE: &str = "The subagent was stopped before it finished.";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubagentToolInput {
    pub prompt: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_in_background: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
}

pub fn normalize_subagent_tool_input(input: Value) -> Value {
    let Some(mut record) = input.as_object().cloned() else {
        return input;
    };
    let has_resume_id = record
        .get("resume")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.trim().is_empty());
    let has_subagent_type = record
        .get("subagent_type")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty());
    let has_fork = record.get("fork").and_then(Value::as_bool) == Some(true);
    let has_backend = record
        .get("backend")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.trim().is_empty());
    if !has_subagent_type && !has_resume_id && !has_fork && !has_backend {
        record.insert(
            "subagent_type".to_string(),
            Value::String(DEFAULT_PROFILE_NAME.to_string()),
        );
    } else if !has_subagent_type {
        record.remove("subagent_type");
    }
    Value::Object(record)
}

pub fn subagent_tool_input_schema() -> Value {
    json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": {
            "prompt": {
                "type": "string",
                "description": "Full task prompt for the subagent"
            },
            "description": {
                "type": "string",
                "description": "Short task description (3-5 words) for UI display"
            },
            "subagent_type": {
                "type": "string",
                "description": "One of the available agent types (see \"Available agent types\" in this tool description). Defaults to \"coder\" when omitted."
            },
            "resume": {
                "type": "string",
                "description": "Optional agent ID to resume instead of creating a new instance. When set, do not also pass subagent_type — the resumed agent keeps its own type, and supplying both is rejected."
            },
            "run_in_background": {
                "type": "boolean",
                "description": "If true, return immediately without waiting for completion. Prefer false unless the task can run independently and there is a clear benefit to not waiting."
            },
            "fork": {
                "type": "boolean",
                "description": "Start with a snapshot of your completed conversation history, inheriting your current agent type, model, and tool set."
            },
            "model": {
                "type": "string",
                "description": "Which model to run the subagent on: one of the aliases listed under \"Available models\" in this tool description. When omitted, the configured default model is used. Ignored when resuming — resumed subagents keep their own model."
            },
            "backend": {
                "type": "string",
                "description": "Run the task on an external agent CLI backend (see \"Available backends\" in this tool description) instead of an in-process subagent. Cannot be combined with subagent_type, resume, fork, model, or run_in_background."
            }
        },
        "required": ["prompt", "description"],
        "additionalProperties": false
    })
}

pub fn subagent_tool_output_schema() -> Value {
    json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": {
            "result": {
                "type": "string",
                "description": "Aggregated text output from the subagent"
            },
            "usage": {
                "type": "object",
                "properties": {
                    "input": { "type": "integer", "minimum": 0 },
                    "output": { "type": "integer", "minimum": 0 },
                    "cache_read": { "type": "integer", "minimum": 0 },
                    "cache_write": { "type": "integer", "minimum": 0 }
                },
                "required": ["input", "output"],
                "additionalProperties": false,
                "description": "Cumulative token usage"
            }
        },
        "required": ["result", "usage"],
        "additionalProperties": false
    })
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubagentToolOutput {
    pub result: String,
    pub usage: SubagentToolUsage,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubagentToolUsage {
    pub input: u64,
    pub output: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<u64>,
}

pub trait ISubagentTool: Send + Sync {
    /// Drop the cached catalog-profile descriptions so the next listing
    /// re-reads the freshly refreshed profile catalog (v2
    /// `invalidateProfileDescriptions`, `agentTool.ts:30-32`).
    fn invalidate_profile_descriptions(&self);
}
