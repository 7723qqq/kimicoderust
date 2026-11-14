pub use crate::config::*;

use std::sync::LazyLock;

use serde_json::{Map, Value, json};

use crate::app::config::config::{
    ConfigKeyDeprecation, EnvBindings, RegisterSectionOptions, StripEnvBoundFields, env_bindings,
    strip_env_bound_fields,
};
use crate::app::config::config_section_contributions::register_config_section;

pub const LOOP_CONTROL_SECTION: &str = "loopControl";
pub const LOOP_MAX_STEPS_PER_TURN_ENV: &str = "KIMI_LOOP_MAX_STEPS_PER_TURN";
pub const LOOP_MAX_ATTEMPTS_PER_STEP_ENV: &str = "KIMI_LOOP_MAX_ATTEMPTS_PER_STEP";
pub const LOOP_MAX_RETRIES_PER_STEP_ENV: &str = "KIMI_LOOP_MAX_RETRIES_PER_STEP";

pub type LoopControl = LoopControlConfig;

pub static LOOP_CONTROL_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "properties": {
            "maxStepsPerTurn": { "type": "integer", "minimum": 0 },
            "maxAttemptsPerStep": { "type": "integer", "minimum": 0 },
            "maxRalphIterations": { "type": "integer", "minimum": -1 },
            "reservedContextSize": { "type": "integer", "minimum": 0 },
            "compactionTriggerRatio": { "type": "number", "minimum": 0.5, "maximum": 0.99 },
            "compactionMaxAttempts": { "type": "integer", "minimum": 1 }
        }
    })
});

pub const LOOP_CONTROL_DEPRECATIONS: &[(&str, &str)] = &[
    ("max_retries_per_step", "max_attempts_per_step"),
    ("max_steps_per_run", "max_steps_per_turn"),
];

pub fn parse_non_negative_int(raw: &str) -> Option<u32> {
    let value = raw.trim();
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse::<u32>().ok()
}

pub fn loop_control_env_bindings() -> EnvBindings {
    let mut bindings = env_bindings(
        &LOOP_CONTROL_SCHEMA,
        &[
            ("maxStepsPerTurn", LOOP_MAX_STEPS_PER_TURN_ENV, parse_non_negative_int),
            ("maxAttemptsPerStep", LOOP_MAX_ATTEMPTS_PER_STEP_ENV, parse_non_negative_int),
        ],
    );
    if let EnvBindings::Fields(fields) = &mut bindings
        && let Some(EnvBindings::Binding(binding)) = fields.get_mut("maxAttemptsPerStep")
    {
        binding.deprecated_env = Some(LOOP_MAX_RETRIES_PER_STEP_ENV.to_string());
    }
    bindings
}

pub fn strip_loop_control_env() -> StripEnvBoundFields {
    strip_env_bound_fields(&loop_control_env_bindings())
}

fn camel_to_snake(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_ascii_uppercase() {
            output.push('_');
            output.push(character.to_ascii_lowercase());
        } else {
            output.push(character);
        }
    }
    output
}

pub fn loop_control_to_toml(value: &Value, raw_snake: &Value) -> Value {
    let Some(object) = value.as_object() else {
        return value.clone();
    };
    let mut output = raw_snake.as_object().cloned().unwrap_or_else(Map::new);
    for (key, entry) in object {
        output.insert(camel_to_snake(key), entry.clone());
    }
    Value::Object(output)
}

pub fn register_loop_control_config() {
    register_config_section(
        LOOP_CONTROL_SECTION,
        LOOP_CONTROL_SCHEMA.clone(),
        RegisterSectionOptions {
            env: Some(loop_control_env_bindings()),
            strip_env: Some(strip_loop_control_env()),
            to_toml: Some(std::sync::Arc::new(|value, raw| {
                Some(loop_control_to_toml(value, raw))
            })),
            deprecations: LOOP_CONTROL_DEPRECATIONS
                .iter()
                .map(|(key, replacement)| ConfigKeyDeprecation {
                    key: (*key).to_string(),
                    replacement: (*replacement).to_string(),
                    message: None,
                })
                .collect(),
            ..Default::default()
        },
    );
}
