use std::sync::LazyLock;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::mcp_core::config_schema::MAX_MCP_TIMEOUT_MS;

pub const MCP_SECTION: &str = "mcp";
pub const MCP_STARTUP_TIMEOUT_ENV: &str = "KIMI_MCP_STARTUP_TIMEOUT_MS";
pub const MCP_TOOL_TIMEOUT_ENV: &str = "KIMI_MCP_TOOL_TIMEOUT_MS";
pub const MCP_AUTO_DEFER_TOOL_THRESHOLD_ENV: &str = "KIMI_MCP_AUTO_DEFER_TOOL_THRESHOLD";
pub const MCP_RECONNECT_INITIAL_DELAY_ENV: &str = "KIMI_MCP_RECONNECT_INITIAL_DELAY_MS";
pub const MCP_RECONNECT_MAX_DELAY_ENV: &str = "KIMI_MCP_RECONNECT_MAX_DELAY_MS";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpSection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_deferred: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_defer_tool_threshold: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_tool_schemas: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reconnect_initial_delay_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reconnect_max_delay_ms: Option<u64>,
}

pub static MCP_SECTION_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "properties": {
            "startupTimeoutMs": { "type": "integer", "minimum": 1, "maximum": MAX_MCP_TIMEOUT_MS },
            "toolTimeoutMs": { "type": "integer", "minimum": 1, "maximum": MAX_MCP_TIMEOUT_MS },
            "defaultDeferred": { "type": "boolean" },
            "autoDeferToolThreshold": { "type": "integer", "minimum": 0, "maximum": 1_000_000 },
            "compactToolSchemas": { "type": "boolean" },
            "reconnectInitialDelayMs": { "type": "integer", "minimum": 1, "maximum": MAX_MCP_TIMEOUT_MS },
            "reconnectMaxDelayMs": { "type": "integer", "minimum": 1, "maximum": MAX_MCP_TIMEOUT_MS }
        }
    })
});

pub fn validate_mcp_section(value: Value) -> Result<McpSection, String> {
    let section: McpSection = serde_json::from_value(value).map_err(|error| error.to_string())?;
    for (name, timeout) in [
        ("startupTimeoutMs", section.startup_timeout_ms),
        ("toolTimeoutMs", section.tool_timeout_ms),
        ("reconnectInitialDelayMs", section.reconnect_initial_delay_ms),
        ("reconnectMaxDelayMs", section.reconnect_max_delay_ms),
    ] {
        if timeout.is_some_and(|timeout| timeout == 0 || timeout > MAX_MCP_TIMEOUT_MS) {
            return Err(format!("{name} must be an integer in [1, {MAX_MCP_TIMEOUT_MS}]"));
        }
    }
    if section.auto_defer_tool_threshold.is_some_and(|threshold| threshold > 1_000_000) {
        return Err("autoDeferToolThreshold must be an integer in [0, 1000000]".to_string());
    }
    Ok(section)
}

fn parse_timeout_ms_env(raw: &str) -> Option<Value> {
    let parsed = raw.parse::<f64>().ok()?;
    if !parsed.is_finite()
        || parsed.fract() != 0.0
        || parsed < 1.0
        || parsed > MAX_MCP_TIMEOUT_MS as f64
    {
        return None;
    }
    Some(Value::from(parsed as u64))
}

fn parse_auto_defer_tool_threshold_env(raw: &str) -> Option<Value> {
    let parsed = raw.parse::<f64>().ok()?;
    if !parsed.is_finite() || parsed.fract() != 0.0 || parsed < 0.0 || parsed > 1_000_000.0 {
        return None;
    }
    Some(Value::from(parsed as u64))
}

#[derive(Clone, Copy)]
pub struct McpEnvBinding {
    pub field: &'static str,
    pub env: &'static str,
    pub parse: fn(&str) -> Option<Value>,
}

pub static MCP_ENV_BINDINGS: [McpEnvBinding; 5] = [
    McpEnvBinding {
        field: "startupTimeoutMs",
        env: MCP_STARTUP_TIMEOUT_ENV,
        parse: parse_timeout_ms_env,
    },
    McpEnvBinding {
        field: "toolTimeoutMs",
        env: MCP_TOOL_TIMEOUT_ENV,
        parse: parse_timeout_ms_env,
    },
    McpEnvBinding {
        field: "autoDeferToolThreshold",
        env: MCP_AUTO_DEFER_TOOL_THRESHOLD_ENV,
        parse: parse_auto_defer_tool_threshold_env,
    },
    McpEnvBinding {
        field: "reconnectInitialDelayMs",
        env: MCP_RECONNECT_INITIAL_DELAY_ENV,
        parse: parse_timeout_ms_env,
    },
    McpEnvBinding {
        field: "reconnectMaxDelayMs",
        env: MCP_RECONNECT_MAX_DELAY_ENV,
        parse: parse_timeout_ms_env,
    },
];

pub fn mcp_env_bindings() -> &'static [McpEnvBinding] {
    &MCP_ENV_BINDINGS
}

pub fn strip_mcp_env(
    value: Value,
    raw: Option<&Value>,
    get_env: impl Fn(&str) -> Option<String>,
) -> Option<Value> {
    if !value.is_object() {
        return Some(value);
    }
    let base = raw.and_then(Value::as_object).cloned().unwrap_or_default();
    let mut output: Option<Map<String, Value>> = None;
    for binding in mcp_env_bindings() {
        let resolves = get_env(binding.env)
            .as_deref()
            .is_some_and(|raw| (binding.parse)(raw).is_some());
        if !resolves {
            continue;
        }
        let out = output.get_or_insert_with(|| value.as_object().cloned().unwrap_or_default());
        if let Some(raw_value) = base.get(binding.field) {
            out.insert(binding.field.to_string(), raw_value.clone());
        } else {
            out.remove(binding.field);
        }
    }
    let Some(output) = output else {
        return Some(value);
    };
    if !output.is_empty() {
        return Some(Value::Object(output));
    }
    if base.is_empty() {
        None
    } else {
        Some(Value::Object(base))
    }
}

pub struct McpSectionContribution {
    pub domain: &'static str,
    pub schema: &'static Value,
    pub env: &'static [McpEnvBinding],
    pub strip_env: fn(Value, Option<&Value>, &dyn Fn(&str) -> Option<String>) -> Option<Value>,
}

fn strip_mcp_env_contribution(
    value: Value,
    raw: Option<&Value>,
    get_env: &dyn Fn(&str) -> Option<String>,
) -> Option<Value> {
    strip_mcp_env(value, raw, get_env)
}

pub fn register_mcp_config_section(register: impl FnOnce(McpSectionContribution)) {
    register(McpSectionContribution {
        domain: MCP_SECTION,
        schema: &MCP_SECTION_SCHEMA,
        env: mcp_env_bindings(),
        strip_env: strip_mcp_env_contribution,
    });
}
