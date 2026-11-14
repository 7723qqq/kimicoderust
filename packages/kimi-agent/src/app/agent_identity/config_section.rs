use std::sync::LazyLock;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::app::config::config::{
    EnvBindings, StripEnvBoundFields, env_bindings, strip_env_bound_fields,
};
use crate::app::config::config_section_contributions::register_config_section;

pub const IDENTITY_SECTION: &str = "identity";
pub const IDENTITY_NAME_ENV: &str = "KIMI_CODE_IDENTITY_NAME";
pub const IDENTITY_SLUG_ENV: &str = "KIMI_CODE_IDENTITY_SLUG";

pub static IDENTITY_CONFIG_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" },
            "slug": { "type": "string" }
        }
    })
});

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
}

fn parse_identity_env(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn parse_identity_env_value(raw: &str) -> Option<Value> {
    parse_identity_env(raw).map(Value::String)
}

pub fn identity_env_bindings() -> EnvBindings {
    env_bindings(
        &IDENTITY_CONFIG_SCHEMA,
        &[
            ("name", IDENTITY_NAME_ENV, parse_identity_env_value),
            ("slug", IDENTITY_SLUG_ENV, parse_identity_env_value),
        ],
    )
}

pub fn strip_identity_env() -> StripEnvBoundFields {
    strip_env_bound_fields(&identity_env_bindings())
}

pub fn register_identity_config() {
    register_config_section(
        IDENTITY_SECTION,
        IDENTITY_CONFIG_SCHEMA.clone(),
        Value::Object(Map::new()),
    );
}
