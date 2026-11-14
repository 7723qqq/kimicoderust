use serde::{Deserialize, Serialize};

pub use crate::workflow::code_runtime::{
    CodeRunError, CodeRunLimits, CodeRunOutcome, DEFAULT_TIMEOUT_MS, MAX_OUTPUT_CHARS,
    MAX_TIMEOUT_MS, MIN_TIMEOUT_MS,
};

pub const RUN_CODE_TOOL_SERVICE_ID: &str = "runCodeTool";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct RunCodeInput {
    #[serde(deserialize_with = "deserialize_code")]
    pub code: String,
    #[serde(default = "default_timeout_ms", deserialize_with = "deserialize_timeout_ms")]
    pub timeout_ms: u64,
}

fn deserialize_code<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let code = String::deserialize(deserializer)?;
    if code.is_empty() {
        return Err(serde::de::Error::custom("code must not be empty"));
    }
    Ok(code)
}

fn deserialize_timeout_ms<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    let timeout = value
        .as_f64()
        .ok_or_else(|| serde::de::Error::custom("timeout_ms must be an integer"))?;
    if !timeout.is_finite()
        || timeout.fract() != 0.0
        || !(MIN_TIMEOUT_MS as f64..=MAX_TIMEOUT_MS as f64).contains(&timeout)
    {
        return Err(serde::de::Error::custom(format!(
            "timeout_ms must be between {MIN_TIMEOUT_MS} and {MAX_TIMEOUT_MS}"
        )));
    }
    Ok(timeout as u64)
}

const fn default_timeout_ms() -> u64 {
    DEFAULT_TIMEOUT_MS
}

pub fn run_code_input_schema() -> serde_json::Value {
    crate::tool::input_schema::to_input_json_schema(serde_json::json!({
        "type": "object",
        "properties": {
            "code": {
                "type": "string",
                "minLength": 1,
                "description": "The TypeScript/JavaScript program to run. The body executes as an async function, so top-level await and return work. console.log/info/warn/error/debug output is captured."
            },
            "timeout_ms": {
                "type": "integer",
                "minimum": MIN_TIMEOUT_MS,
                "maximum": MAX_TIMEOUT_MS,
                "default": DEFAULT_TIMEOUT_MS,
                "description": "Execution budget in milliseconds before the worker is terminated (default 30000)."
            }
        },
        "required": ["code"]
    }))
}
