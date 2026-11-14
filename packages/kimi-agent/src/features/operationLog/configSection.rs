use std::sync::LazyLock;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::operationLogStore::PartialRetention;
use crate::app::config::config::{
    EnvBindings, IConfigService, RegisterSectionOptions, StripEnvBoundFields, env_bindings,
    strip_env_bound_fields,
};
use crate::app::config::config_section_contributions::register_config_section;

pub const OPERATION_LOG_SECTION: &str = "operationLog";
pub const OPERATION_LOG_MAX_OPERATIONS_ENV: &str = "KIMI_CODE_OPERATION_LOG_MAX_OPERATIONS";
pub const OPERATION_LOG_MAX_AGE_MS_ENV: &str = "KIMI_CODE_OPERATION_LOG_MAX_AGE_MS";
pub const OPERATION_LOG_MAX_TOTAL_BYTES_ENV: &str = "KIMI_CODE_OPERATION_LOG_MAX_TOTAL_BYTES";

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

pub static OPERATION_LOG_CONFIG_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "properties": {
            "maxOperations": { "type": "integer", "minimum": 1 },
            "maxAgeMs": { "type": "integer", "minimum": 0 },
            "maxTotalBytes": { "type": "integer", "minimum": 1 }
        }
    })
});

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OperationLogConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_operations: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_age_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_total_bytes: Option<u64>,
}

fn parse_positive_int(raw: &str) -> Option<u64> {
    let value = raw.trim();
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value
        .parse::<u64>()
        .ok()
        .filter(|parsed| *parsed > 0 && *parsed <= MAX_SAFE_INTEGER)
}

fn parse_non_negative_int(raw: &str) -> Option<u64> {
    let value = raw.trim();
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value
        .parse::<u64>()
        .ok()
        .filter(|parsed| *parsed <= MAX_SAFE_INTEGER)
}

pub fn operation_log_env_bindings() -> EnvBindings {
    env_bindings(
        &OPERATION_LOG_CONFIG_SCHEMA,
        &[
            (
                "maxOperations",
                OPERATION_LOG_MAX_OPERATIONS_ENV,
                parse_positive_int,
            ),
            (
                "maxAgeMs",
                OPERATION_LOG_MAX_AGE_MS_ENV,
                parse_non_negative_int,
            ),
            (
                "maxTotalBytes",
                OPERATION_LOG_MAX_TOTAL_BYTES_ENV,
                parse_positive_int,
            ),
        ],
    )
}

pub fn strip_operation_log_env() -> StripEnvBoundFields {
    strip_env_bound_fields(&operation_log_env_bindings())
}

pub fn register_operation_log_config() {
    register_config_section(
        OPERATION_LOG_SECTION,
        OPERATION_LOG_CONFIG_SCHEMA.clone(),
        RegisterSectionOptions {
            env: Some(operation_log_env_bindings()),
            strip_env: Some(strip_operation_log_env()),
            ..Default::default()
        },
    );
}

pub trait OperationLogRetentionSource {
    fn operation_log_config_value(&self) -> Option<Value>;
}

impl OperationLogRetentionSource for crate::config::KimiConfig {
    fn operation_log_config_value(&self) -> Option<Value> {
        Some(json!({
            "maxOperations": self.operation_log.max_operations,
            "maxAgeMs": self.operation_log.max_age_ms,
            "maxTotalBytes": self.operation_log.max_total_bytes,
        }))
    }
}

impl OperationLogRetentionSource for dyn IConfigService {
    fn operation_log_config_value(&self) -> Option<Value> {
        self.get(OPERATION_LOG_SECTION)
    }
}

pub fn resolve_operation_log_retention(
    source: &dyn OperationLogRetentionSource,
) -> PartialRetention {
    source
        .operation_log_config_value()
        .and_then(|value| serde_json::from_value::<OperationLogConfig>(value).ok())
        .map(|config| PartialRetention {
            max_operations: config.max_operations,
            max_age_ms: config.max_age_ms,
            max_total_bytes: config.max_total_bytes,
        })
        .unwrap_or_default()
}
