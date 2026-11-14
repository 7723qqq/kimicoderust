use std::sync::LazyLock;

use serde_json::{Value, json};

use crate::app::config::config::{
    EnvBindings, IConfigService, RegisterSectionOptions, StripEnvBoundFields, env_bindings,
    strip_env_bound_fields,
};
use crate::app::config::config_section_contributions::register_config_section;

pub const LLM_REQUESTER_SECTION: &str = "llmRequester";

pub const LLM_REQUEST_BYTE_BUDGET_ENV: &str = "KIMI_LLM_REQUEST_BYTE_BUDGET";

pub const DEFAULT_REQUEST_BYTE_BUDGET_BYTES: u64 = 32 * 1024 * 1024;

pub static LLM_REQUESTER_CONFIG_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "properties": {
            "requestByteBudget": {"type": "integer", "minimum": 1},
        }
    })
});

fn parse_positive_int(raw: &str) -> Option<u64> {
    let value = raw.trim();
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse::<u64>().ok().filter(|value| *value > 0)
}

pub fn llm_requester_env_bindings() -> EnvBindings {
    env_bindings(
        &LLM_REQUESTER_CONFIG_SCHEMA,
        &[(
            "requestByteBudget",
            LLM_REQUEST_BYTE_BUDGET_ENV,
            parse_positive_int,
        )],
    )
}

pub fn strip_llm_requester_env() -> StripEnvBoundFields {
    strip_env_bound_fields(&llm_requester_env_bindings())
}

pub fn register_llm_requester_config() {
    register_config_section(
        LLM_REQUESTER_SECTION,
        LLM_REQUESTER_CONFIG_SCHEMA.clone(),
        RegisterSectionOptions {
            default_value: Some(json!({})),
            env: Some(llm_requester_env_bindings()),
            strip_env: Some(strip_llm_requester_env()),
            ..Default::default()
        },
    );
}

pub fn resolve_request_byte_budget(config: &dyn IConfigService) -> u64 {
    config
        .get(LLM_REQUESTER_SECTION)
        .and_then(|value| value.get("requestByteBudget").and_then(Value::as_u64))
        .unwrap_or(DEFAULT_REQUEST_BYTE_BUDGET_BYTES)
}
