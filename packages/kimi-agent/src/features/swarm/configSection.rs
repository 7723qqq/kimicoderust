use std::sync::LazyLock;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::app::config::config::{
    EnvBindings, IConfigService, RegisterSectionOptions, StripEnvBoundFields, env_bindings,
    strip_env_bound_fields,
};
use crate::app::config::config_section_contributions::register_config_section;

pub const SWARM_SECTION: &str = "swarm";
pub const DEFAULT_SWARM_TIMEOUT_MS: u64 = 2 * 60 * 60 * 1000;
pub const SWARM_TIMEOUT_ENV: &str = "KIMI_CODE_SWARM_TIMEOUT_MS";

pub static SWARM_CONFIG_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "properties": {
            "timeoutMs": { "type": "integer", "minimum": 0 }
        }
    })
});

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SwarmConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

fn parse_timeout_ms_env(raw: &str) -> Option<u64> {
    let parsed = raw.trim().parse::<f64>().ok()?;
    if !parsed.is_finite() || parsed.fract() != 0.0 || parsed < 1.0 {
        return None;
    }
    Some(parsed as u64)
}

pub fn swarm_env_bindings() -> EnvBindings {
    env_bindings(
        &SWARM_CONFIG_SCHEMA,
        &[("timeoutMs", SWARM_TIMEOUT_ENV, parse_timeout_ms_env)],
    )
}

pub fn strip_swarm_env() -> StripEnvBoundFields {
    strip_env_bound_fields(&swarm_env_bindings())
}

pub fn register_swarm_config() {
    register_config_section(
        SWARM_SECTION,
        SWARM_CONFIG_SCHEMA.clone(),
        RegisterSectionOptions {
            default_value: Some(json!({ "timeoutMs": DEFAULT_SWARM_TIMEOUT_MS })),
            env: Some(swarm_env_bindings()),
            strip_env: Some(strip_swarm_env()),
            ..Default::default()
        },
    );
}

pub fn resolve_swarm_timeout_ms(config: &dyn IConfigService) -> u64 {
    config
        .get(SWARM_SECTION)
        .and_then(|value| serde_json::from_value::<SwarmConfig>(value).ok())
        .and_then(|config| config.timeout_ms)
        .unwrap_or(DEFAULT_SWARM_TIMEOUT_MS)
}
