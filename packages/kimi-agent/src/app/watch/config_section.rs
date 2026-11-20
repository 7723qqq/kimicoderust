use std::sync::LazyLock;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::_base::utils::env::parse_boolean_env;
use crate::app::config::config::{EnvBindings, StripEnvBoundFields, env_bindings, strip_env_bound_fields};
use crate::app::config::config_section_contributions::register_config_section;
use crate::human::utils::watch::WATCH_ENV;

pub const WATCH_SECTION: &str = "watch";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchConfig {
    pub enabled: Option<bool>,
}

pub static WATCH_CONFIG_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "properties": {
            "enabled": { "type": "boolean" }
        }
    })
});

fn parse_watch_env(raw: &str) -> Option<Value> {
    parse_boolean_env(Some(raw)).map(Value::Bool)
}

pub fn watch_env_bindings() -> EnvBindings {
    env_bindings(&WATCH_CONFIG_SCHEMA, &[("enabled", WATCH_ENV, parse_watch_env)])
}

pub fn strip_watch_env() -> StripEnvBoundFields {
    strip_env_bound_fields(&watch_env_bindings())
}

pub fn register_watch_config() {
    register_config_section(WATCH_SECTION, WATCH_CONFIG_SCHEMA.clone(), Value::Object(Map::new()));
}
