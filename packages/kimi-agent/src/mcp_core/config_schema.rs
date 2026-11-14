use std::collections::HashMap;
use std::sync::LazyLock;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

pub const MAX_MCP_TIMEOUT_MS: u64 = 2_147_483_647;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    Stdio,
    Http,
    Sse,
}

impl McpTransport {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::Http => "http",
            Self::Sse => "sse",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpExecutor {
    Local,
    Kaos,
}

impl McpExecutor {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Kaos => "kaos",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpAuth {
    Oauth,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerConfig {
    pub transport: McpTransport,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor: Option<McpExecutor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<McpAuth>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_token_env_var: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_tools: Option<Vec<String>>,
}

pub type McpServerStdioConfig = McpServerConfig;
pub type McpServerHttpConfig = McpServerConfig;
pub type McpServerSseConfig = McpServerConfig;
pub type McpRemoteServerConfig = McpServerConfig;

pub static MCP_TIMEOUT_MS_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "number",
        "multipleOf": 1,
        "minimum": 1,
        "maximum": MAX_MCP_TIMEOUT_MS,
    })
});

fn common_schema_fields() -> Map<String, Value> {
    let mut fields = Map::new();
    fields.insert("enabled".to_string(), json!({"type": "boolean"}));
    fields.insert("deferred".to_string(), json!({"type": "boolean"}));
    fields.insert("startupTimeoutMs".to_string(), MCP_TIMEOUT_MS_SCHEMA.clone());
    fields.insert("toolTimeoutMs".to_string(), MCP_TIMEOUT_MS_SCHEMA.clone());
    fields.insert("enabledTools".to_string(), json!({"type": "array", "items": {"type": "string"}}));
    fields.insert("disabledTools".to_string(), json!({"type": "array", "items": {"type": "string"}}));
    fields
}

fn remote_schema_fields() -> Map<String, Value> {
    let mut fields = common_schema_fields();
    fields.insert("url".to_string(), json!({"type": "string", "format": "uri"}));
    fields.insert("headers".to_string(), json!({"type": "object", "additionalProperties": {"type": "string"}}));
    fields.insert("auth".to_string(), json!({"const": "oauth"}));
    fields.insert("bearerTokenEnvVar".to_string(), json!({"type": "string", "minLength": 1}));
    fields
}

pub static MCP_SERVER_STDIO_CONFIG_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    let mut fields = common_schema_fields();
    fields.insert("transport".to_string(), json!({"const": "stdio"}));
    fields.insert("command".to_string(), json!({"type": "string", "minLength": 1}));
    fields.insert("args".to_string(), json!({"type": "array", "items": {"type": "string"}}));
    fields.insert("env".to_string(), json!({"type": "object", "additionalProperties": {"type": "string"}}));
    fields.insert("cwd".to_string(), json!({"type": "string"}));
    fields.insert("executor".to_string(), json!({"enum": ["local", "kaos"]}));
    fields.insert("environment_id".to_string(), json!({"type": "string", "minLength": 1}));
    json!({"type": "object", "required": ["transport", "command"], "properties": fields})
});

pub static MCP_SERVER_HTTP_CONFIG_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    let mut fields = remote_schema_fields();
    fields.insert("transport".to_string(), json!({"const": "http"}));
    json!({"type": "object", "required": ["transport", "url"], "properties": fields})
});

pub static MCP_SERVER_SSE_CONFIG_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    let mut fields = remote_schema_fields();
    fields.insert("transport".to_string(), json!({"const": "sse"}));
    json!({"type": "object", "required": ["transport", "url"], "properties": fields})
});

pub static MCP_SERVER_CONFIG_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "anyOf": [
            MCP_SERVER_STDIO_CONFIG_SCHEMA.clone(),
            MCP_SERVER_HTTP_CONFIG_SCHEMA.clone(),
            MCP_SERVER_SSE_CONFIG_SCHEMA.clone(),
        ]
    })
});

pub fn normalize_mcp_server_config(value: Value) -> Value {
    let Some(mut object) = value.as_object().cloned() else {
        return value;
    };
    if object.contains_key("transport") {
        return Value::Object(object);
    }
    if object.get("command").is_some_and(Value::is_string) {
        object.insert("transport".to_string(), "stdio".into());
    } else if object.get("url").is_some_and(Value::is_string) {
        object.insert("transport".to_string(), "http".into());
    }
    Value::Object(object)
}

pub fn validate_mcp_server_config(value: Value) -> Result<McpServerConfig, String> {
    let mut value = normalize_mcp_server_config(value);
    let transport = value
        .get("transport")
        .and_then(Value::as_str)
        .ok_or_else(|| "transport is required".to_string())?
        .to_string();
    match transport.as_str() {
        "stdio" => {
            if value.get("command").and_then(Value::as_str).is_none_or(|command| command.is_empty()) {
                return Err("command must be a non-empty string".to_string());
            }
            if value.get("environment_id").and_then(Value::as_str).is_some_and(|id| id.is_empty()) {
                return Err("environment_id must be a non-empty string".to_string());
            }
            let object = value.as_object_mut().unwrap();
            for key in ["url", "headers", "auth", "bearerTokenEnvVar"] {
                object.remove(key);
            }
        }
        "http" | "sse" => {
            if value.get("url").and_then(Value::as_str).is_none_or(|url| url::Url::parse(url).is_err()) {
                return Err("url must be a valid URL".to_string());
            }
            if value.get("bearerTokenEnvVar").and_then(Value::as_str).is_some_and(|name| name.is_empty()) {
                return Err("bearerTokenEnvVar must be a non-empty string".to_string());
            }
            let object = value.as_object_mut().unwrap();
            for key in ["command", "args", "env", "cwd", "executor", "environment_id"] {
                object.remove(key);
            }
        }
        _ => return Err(format!("unknown transport {transport}")),
    }
    for key in ["startupTimeoutMs", "toolTimeoutMs"] {
        let normalized = match value.get(key) {
            Some(timeout) => {
                let valid = timeout.as_f64().is_some_and(|timeout| {
                    timeout > 0.0 && timeout <= MAX_MCP_TIMEOUT_MS as f64 && timeout.fract() == 0.0
                });
                if !valid {
                    return Err(format!("{key} must be an integer in [1, {MAX_MCP_TIMEOUT_MS}]"));
                }
                Some(timeout.as_f64().unwrap() as u64)
            }
            None => None,
        };
        if let Some(timeout) = normalized {
            value.as_object_mut().unwrap().insert(key.to_string(), timeout.into());
        }
    }
    serde_json::from_value(value).map_err(|error| error.to_string())
}
