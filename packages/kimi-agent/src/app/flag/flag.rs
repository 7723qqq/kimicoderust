use std::sync::{Arc, LazyLock};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::app::config::config_section_contributions::register_config_section;

use super::flag_registry::{FlagDefinitionInput, FlagId, FlagSurface, IFlagRegistry};

pub type ExperimentalFlagMap = IndexMap<String, bool>;
pub type ExperimentalFlagConfig = IndexMap<FlagId, bool>;
pub type ExperimentalConfig = IndexMap<String, bool>;

pub const EXPERIMENTAL_SECTION: &str = "experimental";

pub static EXPERIMENTAL_CONFIG_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "additionalProperties": {"type": "boolean"}
    })
});

pub fn experimental_from_toml(raw_snake: &Value) -> Value {
    match raw_snake.as_object() {
        Some(record) => Value::Object(record.clone()),
        None => raw_snake.clone(),
    }
}

pub fn experimental_to_toml(value: &Value, _raw_snake: &Value) -> Value {
    match value.as_object() {
        Some(record) => {
            let mut output = Map::new();
            for (key, entry) in record {
                output.insert(key.clone(), entry.clone());
            }
            Value::Object(output)
        }
        None => value.clone(),
    }
}

pub fn register_experimental_config() {
    register_config_section(
        EXPERIMENTAL_SECTION,
        EXPERIMENTAL_CONFIG_SCHEMA.clone(),
        Value::Object(Map::new()),
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExperimentalFlagSource {
    MasterEnv,
    Env,
    Config,
    Default,
}

#[derive(Clone)]
pub struct ExperimentalFeatureState {
    pub id: FlagId,
    pub title: String,
    pub description: String,
    pub surface: FlagSurface,
    pub env: String,
    pub default_enabled: bool,
    pub enabled: bool,
    pub source: ExperimentalFlagSource,
    pub config_value: Option<bool>,
}

pub trait IFlagService: Send + Sync {
    fn registry(&self) -> Arc<dyn IFlagRegistry>;
    fn enabled(&self, id: &str) -> bool;
    fn snapshot(&self) -> ExperimentalFlagMap;
    fn enabled_ids(&self) -> Vec<FlagId>;
    fn exposed_ids(&self) -> Vec<FlagId>;
    fn explain(&self, id: &str) -> Option<ExperimentalFeatureState>;
    fn explain_all(&self) -> Vec<ExperimentalFeatureState>;
    fn set_config_overrides(&self, overrides: Option<ExperimentalFlagConfig>);
}

pub fn state_for_definition(
    definition: &FlagDefinitionInput,
    enabled: bool,
    source: ExperimentalFlagSource,
    config_value: Option<bool>,
) -> ExperimentalFeatureState {
    ExperimentalFeatureState {
        id: definition.id.clone(),
        title: definition.title.clone(),
        description: definition.description.clone(),
        surface: definition.surface,
        env: definition.env.clone(),
        default_enabled: definition.default,
        enabled,
        source,
        config_value,
    }
}
