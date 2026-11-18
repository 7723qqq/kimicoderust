use std::sync::LazyLock;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::_base::utils::env::{parse_boolean_env, parse_number_env};
use crate::app::config::config::{
    EnvBindings, IConfigService, RegisterSectionOptions, StripEnvBoundFields, env_bindings,
    strip_env_bound_fields,
};
use crate::app::config::config_section_contributions::register_config_section;

pub const DATABASE_SECTION: &str = "database";
pub const PERSISTENCE_MINIDB_READMODEL_ENV: &str = "KIMI_CODE_PERSISTENCE_MINIDB_READMODEL";
pub const GLOBAL_SEARCH_ENV: &str = "KIMI_CODE_GLOBAL_SEARCH";
pub const SEARCH_WORKER_ENV: &str = "KIMI_CODE_SEARCH_WORKER";
pub const SEARCH_SYNC_SESSION_CAP_ENV: &str = "KIMI_CODE_SEARCH_SYNC_SESSION_CAP";
pub const SEARCH_SYNC_DEBOUNCE_MS_ENV: &str = "KIMI_CODE_SEARCH_SYNC_DEBOUNCE_MS";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DatabaseConfig {
    pub base: Option<bool>,
    pub global_search: Option<bool>,
    pub search: Option<bool>,
    pub search_sync_session_cap: Option<u64>,
    pub search_sync_debounce_ms: Option<u64>,
}

pub static DATABASE_CONFIG_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "properties": {
            "base": { "type": "boolean" },
            "globalSearch": { "type": "boolean" },
            "search": { "type": "boolean" },
            "searchSyncSessionCap": { "type": "integer", "minimum": 1 },
            "searchSyncDebounceMs": { "type": "integer", "minimum": 0 }
        }
    })
});

fn parse_bool_value(raw: &str) -> Option<Value> {
    parse_boolean_env(Some(raw)).map(Value::Bool)
}

fn parse_number_value(raw: &str) -> Option<Value> {
    parse_number_env(Some(raw)).map(|value| json!(value))
}

pub fn database_env_bindings() -> EnvBindings {
    env_bindings(
        &DATABASE_CONFIG_SCHEMA,
        &[
            ("base", PERSISTENCE_MINIDB_READMODEL_ENV, parse_bool_value as fn(&str) -> Option<Value>),
            ("globalSearch", GLOBAL_SEARCH_ENV, parse_bool_value as fn(&str) -> Option<Value>),
            ("search", SEARCH_WORKER_ENV, parse_bool_value as fn(&str) -> Option<Value>),
            ("searchSyncSessionCap", SEARCH_SYNC_SESSION_CAP_ENV, parse_number_value as fn(&str) -> Option<Value>),
            ("searchSyncDebounceMs", SEARCH_SYNC_DEBOUNCE_MS_ENV, parse_number_value as fn(&str) -> Option<Value>),
        ],
    )
}

pub fn strip_database_env() -> StripEnvBoundFields {
    strip_env_bound_fields(&database_env_bindings())
}

pub fn register_database_config() {
    let env = database_env_bindings();
    let strip_env = strip_env_bound_fields(&env);
    register_config_section(
        DATABASE_SECTION,
        DATABASE_CONFIG_SCHEMA.clone(),
        RegisterSectionOptions {
            env: Some(env),
            strip_env: Some(strip_env),
            ..Default::default()
        },
    );
}

pub fn database_base_enabled(config: &dyn IConfigService) -> bool {
    config
        .get(DATABASE_SECTION)
        .as_ref()
        .and_then(|section| section.get("base"))
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

pub fn database_global_search_enabled(config: &dyn IConfigService) -> bool {
    config
        .get(DATABASE_SECTION)
        .as_ref()
        .and_then(|section| section.get("globalSearch"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

pub fn database_search_enabled(config: &dyn IConfigService) -> bool {
    config
        .get(DATABASE_SECTION)
        .as_ref()
        .and_then(|section| section.get("search"))
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DatabaseSearchSyncTuning {
    pub session_cap: Option<u64>,
    pub debounce_ms: Option<u64>,
}

pub fn database_search_sync_tuning(config: &dyn IConfigService) -> DatabaseSearchSyncTuning {
    let section = config.get(DATABASE_SECTION);
    DatabaseSearchSyncTuning {
        session_cap: section
            .as_ref()
            .and_then(|section| section.get("searchSyncSessionCap"))
            .and_then(Value::as_u64),
        debounce_ms: section
            .as_ref()
            .and_then(|section| section.get("searchSyncDebounceMs"))
            .and_then(Value::as_u64),
    }
}
