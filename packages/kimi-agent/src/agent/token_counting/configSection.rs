pub use crate::config::*;

use std::sync::LazyLock;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::app::config::config::{
    EnvBindings, RegisterSectionOptions, StripEnvBoundFields, env_bindings, strip_env_bound_fields,
};
use crate::app::config::config_section_contributions::register_config_section;

use super::token_counting::TokenCountingStrategy;

pub const TOKEN_COUNTING_SECTION: &str = "tokenCounting";
pub const TOKEN_COUNTING_STRATEGY_ENV: &str = "KIMI_TOKEN_COUNTING_STRATEGY";
pub const TOKEN_COUNTING_STRATEGIES: [&str; 3] = ["measured+estimated", "measured", "estimated"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenCountingConfig {
    pub strategy: TokenCountingStrategy,
}

impl Default for TokenCountingConfig {
    fn default() -> Self {
        Self {
            strategy: TokenCountingStrategy::MeasuredAndEstimated,
        }
    }
}

pub const DEFAULT_TOKEN_COUNTING_CONFIG: TokenCountingConfig = TokenCountingConfig {
    strategy: TokenCountingStrategy::MeasuredAndEstimated,
};

pub static TOKEN_COUNTING_CONFIG_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "required": ["strategy"],
        "properties": {
            "strategy": {"enum": TOKEN_COUNTING_STRATEGIES},
        }
    })
});

fn parse_strategy_value(raw: &str) -> Option<Value> {
    let value = raw
        .trim_matches(|character| {
            matches!(
                character,
                '\u{9}'..='\u{D}'
                    | '\u{20}'
                    | '\u{A0}'
                    | '\u{1680}'
                    | '\u{2000}'..='\u{200A}'
                    | '\u{2028}'
                    | '\u{2029}'
                    | '\u{202F}'
                    | '\u{205F}'
                    | '\u{3000}'
                    | '\u{FEFF}'
            )
        })
        .to_lowercase();
    TOKEN_COUNTING_STRATEGIES
        .contains(&value.as_str())
        .then_some(Value::String(value))
}

pub fn token_counting_env_bindings() -> EnvBindings {
    env_bindings(
        &TOKEN_COUNTING_CONFIG_SCHEMA,
        &[("strategy", TOKEN_COUNTING_STRATEGY_ENV, parse_strategy_value)],
    )
}

pub fn strip_token_counting_env() -> StripEnvBoundFields {
    strip_env_bound_fields(&token_counting_env_bindings())
}

pub fn register_token_counting_config() {
    register_config_section(
        TOKEN_COUNTING_SECTION,
        TOKEN_COUNTING_CONFIG_SCHEMA.clone(),
        RegisterSectionOptions {
            default_value: Some(json!({"strategy": "measured+estimated"})),
            env: Some(token_counting_env_bindings()),
            strip_env: Some(strip_token_counting_env()),
            ..Default::default()
        },
    );
}
