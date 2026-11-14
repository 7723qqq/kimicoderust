use serde::{Deserialize, Deserializer, Serialize};

pub use super::promptOptimizer::{
    KNOWN_EFFORTS, PROMPT_OPTIMIZER_SECTION, PromptOptimizerConfigDiagnostic,
    collect_invalid_effort,
};

use super::promptOptimizer::PROMPT_OPTIMIZER_TIMEOUT_MS;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct PromptOptimizerEngineConfig {
    #[serde(
        rename = "timeoutMs",
        default = "default_timeout_ms",
        deserialize_with = "deserialize_positive_timeout_ms"
    )]
    pub timeout_ms: u64,
    #[serde(default, deserialize_with = "deserialize_effort")]
    pub effort: Option<String>,
}

impl Default for PromptOptimizerEngineConfig {
    fn default() -> Self {
        Self {
            timeout_ms: default_timeout_ms(),
            effort: None,
        }
    }
}

pub const fn default_timeout_ms() -> u64 {
    PROMPT_OPTIMIZER_TIMEOUT_MS
}

fn deserialize_positive_timeout_ms<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    let timeout = value
        .as_f64()
        .ok_or_else(|| serde::de::Error::custom("expected a positive integer"))?;
    if !timeout.is_finite()
        || timeout.fract() != 0.0
        || timeout < 1.0
        || timeout > u64::MAX as f64
    {
        return Err(serde::de::Error::custom("expected a positive integer"));
    }
    Ok(timeout as u64)
}

fn deserialize_effort<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    String::deserialize(deserializer).map(Some)
}
