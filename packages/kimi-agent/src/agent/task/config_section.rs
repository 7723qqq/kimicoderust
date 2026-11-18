use std::sync::LazyLock;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::_base::utils::env::parse_boolean_env;
use crate::app::config::config::{
    EnvBindings, IConfigService, RegisterSectionOptions, StripEnvBoundFields, env_bindings,
    strip_env_bound_fields,
};
use crate::app::config::config_section_contributions::register_config_section;

pub const TASK_SECTION: &str = "task";
pub const LEGACY_BACKGROUND_SECTION: &str = "background";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PrintBackgroundMode {
    Exit,
    Drain,
    Steer,
}

impl PrintBackgroundMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exit => "exit",
            Self::Drain => "drain",
            Self::Steer => "steer",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentTaskConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_running_tasks: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_settled_tasks: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_alive_on_exit: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bash_auto_background_on_timeout: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bash_task_timeout_s: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kill_grace_period_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub print_wait_ceiling_s: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub print_background_mode: Option<PrintBackgroundMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub print_max_turns: Option<u32>,
}

pub static AGENT_TASK_CONFIG_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "properties": {
            "maxRunningTasks": {"type": "integer", "minimum": 1},
            "maxSettledTasks": {"type": "integer", "minimum": 1},
            "keepAliveOnExit": {"type": "boolean"},
            "bashAutoBackgroundOnTimeout": {"type": "boolean"},
            "bashTaskTimeoutS": {"type": "integer", "minimum": 0},
            "killGracePeriodMs": {"type": "integer", "minimum": 0},
            "printWaitCeilingS": {"type": "integer", "minimum": 1},
            "printBackgroundMode": {"enum": ["exit", "drain", "steer"]},
            "printMaxTurns": {"type": "integer", "minimum": 1}
        }
    })
});

pub static PRINT_BACKGROUND_MODE_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({"enum": ["exit", "drain", "steer"]})
});

pub fn resolve_agent_task_config(config: &dyn IConfigService) -> Option<AgentTaskConfig> {
    let legacy = config.get(LEGACY_BACKGROUND_SECTION);
    let current = config.get(TASK_SECTION);
    match (legacy, current) {
        (None, current) => current.and_then(|value| serde_json::from_value(value).ok()),
        (legacy, None) => legacy.and_then(|value| serde_json::from_value(value).ok()),
        (Some(legacy), Some(current)) => {
            let mut merged = legacy.as_object().cloned().unwrap_or_else(Map::new);
            for (key, value) in current.as_object().into_iter().flatten() {
                merged.insert(key.clone(), value.clone());
            }
            serde_json::from_value(Value::Object(merged)).ok()
        }
    }
}

pub fn resolve_print_background_mode(config: &dyn IConfigService) -> PrintBackgroundMode {
    let section = resolve_agent_task_config(config);
    if let Some(mode) = section.as_ref().and_then(|section| section.print_background_mode) {
        return mode;
    }
    if section.and_then(|section| section.keep_alive_on_exit) == Some(true) {
        PrintBackgroundMode::Drain
    } else {
        PrintBackgroundMode::Steer
    }
}

pub const KEEP_ALIVE_ON_EXIT_ENV: &str = "KIMI_CODE_BACKGROUND_KEEP_ALIVE_ON_EXIT";
pub const MAX_RUNNING_TASKS_ENV: &str = "KIMI_CODE_BACKGROUND_MAX_RUNNING_TASKS";
pub const BASH_TASK_TIMEOUT_S_ENV: &str = "KIMI_CODE_BACKGROUND_BASH_TASK_TIMEOUT_S";
pub const PRINT_WAIT_CEILING_S_ENV: &str = "KIMI_CODE_BACKGROUND_PRINT_WAIT_CEILING_S";
pub const PRINT_BACKGROUND_MODE_ENV: &str = "KIMI_CODE_BACKGROUND_PRINT_BACKGROUND_MODE";
pub const PRINT_MAX_TURNS_ENV: &str = "KIMI_CODE_BACKGROUND_PRINT_MAX_TURNS";

pub fn parse_positive_int(raw: &str) -> Option<u64> {
    let value = raw.trim();
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0 && *value <= 9_007_199_254_740_991)
}

pub fn parse_non_negative_int(raw: &str) -> Option<u64> {
    let value = raw.trim();
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value
        .parse::<u64>()
        .ok()
        .filter(|value| *value <= 9_007_199_254_740_991)
}

pub fn parse_print_background_mode(raw: &str) -> Option<String> {
    match raw.trim() {
        "exit" => Some("exit".to_string()),
        "drain" => Some("drain".to_string()),
        "steer" => Some("steer".to_string()),
        _ => None,
    }
}

fn parse_bool_value(raw: &str) -> Option<Value> {
    parse_boolean_env(Some(raw)).map(Value::Bool)
}

fn parse_positive_int_value(raw: &str) -> Option<Value> {
    parse_positive_int(raw).map(Value::from)
}

fn parse_non_negative_int_value(raw: &str) -> Option<Value> {
    parse_non_negative_int(raw).map(Value::from)
}

fn parse_print_background_mode_value(raw: &str) -> Option<Value> {
    parse_print_background_mode(raw).map(Value::String)
}

pub fn task_env_bindings() -> EnvBindings {
    env_bindings(
        &AGENT_TASK_CONFIG_SCHEMA,
        &[
            ("keepAliveOnExit", KEEP_ALIVE_ON_EXIT_ENV, parse_bool_value as fn(&str) -> Option<Value>),
            ("maxRunningTasks", MAX_RUNNING_TASKS_ENV, parse_positive_int_value as fn(&str) -> Option<Value>),
            ("bashTaskTimeoutS", BASH_TASK_TIMEOUT_S_ENV, parse_non_negative_int_value as fn(&str) -> Option<Value>),
            ("printWaitCeilingS", PRINT_WAIT_CEILING_S_ENV, parse_positive_int_value as fn(&str) -> Option<Value>),
            ("printBackgroundMode", PRINT_BACKGROUND_MODE_ENV, parse_print_background_mode_value as fn(&str) -> Option<Value>),
            ("printMaxTurns", PRINT_MAX_TURNS_ENV, parse_positive_int_value as fn(&str) -> Option<Value>),
        ],
    )
}

pub fn strip_task_env() -> StripEnvBoundFields {
    strip_env_bound_fields(&task_env_bindings())
}

pub fn register_task_config() {
    for section in [TASK_SECTION, LEGACY_BACKGROUND_SECTION] {
        register_config_section(
            section,
            AGENT_TASK_CONFIG_SCHEMA.clone(),
            RegisterSectionOptions {
                env: Some(task_env_bindings()),
                strip_env: Some(strip_task_env()),
                ..Default::default()
            },
        );
    }
}
