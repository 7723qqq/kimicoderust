use std::sync::{Arc, LazyLock};

use serde_json::{Value, json};

use crate::_base::utils::env::parse_boolean_env;
use crate::app::config::config::{
    ConfigStripEnv, EnvBinding, EnvBindings, IConfigService, RegisterSectionOptions,
};
use crate::app::config::config_section_contributions::register_config_section;

pub const EXTRA_SKILL_DIRS_SECTION: &str = "extraSkillDirs";

pub static EXTRA_SKILL_DIRS_CONFIG_SCHEMA: LazyLock<Value> =
    LazyLock::new(|| json!({"type": "array", "items": {"type": "string"}}));

pub const MERGE_ALL_AVAILABLE_SKILLS_SECTION: &str = "mergeAllAvailableSkills";

pub static MERGE_ALL_AVAILABLE_SKILLS_CONFIG_SCHEMA: LazyLock<Value> =
    LazyLock::new(|| json!({"type": "boolean"}));

pub const BUILTIN_PRODUCT_SKILLS_SECTION: &str = "builtinProductSkills";

pub static BUILTIN_PRODUCT_SKILLS_CONFIG_SCHEMA: LazyLock<Value> =
    LazyLock::new(|| json!({"type": "boolean"}));

pub const BUILTIN_PRODUCT_SKILLS_ENV: &str = "KIMI_CODE_BUILTIN_PRODUCT_SKILLS";

pub fn builtin_product_skills_env_bindings() -> EnvBindings {
    EnvBindings::Binding(EnvBinding {
        env: BUILTIN_PRODUCT_SKILLS_ENV.to_string(),
        deprecated_env: None,
        parse: Some(Arc::new(|raw: &str| {
            parse_boolean_env(Some(raw)).map(Value::Bool)
        })),
        default: None,
    })
}

pub fn strip_builtin_product_skills_env() -> ConfigStripEnv {
    Arc::new(|value, raw, get_env| {
        if parse_boolean_env(get_env(BUILTIN_PRODUCT_SKILLS_ENV).as_deref()).is_none() {
            return Some(value);
        }
        raw.filter(|raw| raw.is_boolean()).cloned()
    })
}

pub fn register_skill_catalog_config() {
    register_config_section(
        EXTRA_SKILL_DIRS_SECTION,
        EXTRA_SKILL_DIRS_CONFIG_SCHEMA.clone(),
        Value::Array(Vec::new()),
    );
    register_config_section(
        MERGE_ALL_AVAILABLE_SKILLS_SECTION,
        MERGE_ALL_AVAILABLE_SKILLS_CONFIG_SCHEMA.clone(),
        Value::Bool(true),
    );
    register_config_section(
        BUILTIN_PRODUCT_SKILLS_SECTION,
        BUILTIN_PRODUCT_SKILLS_CONFIG_SCHEMA.clone(),
        RegisterSectionOptions {
            default_value: Some(Value::Bool(true)),
            env: Some(builtin_product_skills_env_bindings()),
            strip_env: Some(strip_builtin_product_skills_env()),
            ..Default::default()
        },
    );
}

pub fn builtin_product_skills_enabled(config: &dyn IConfigService) -> bool {
    config
        .get(BUILTIN_PRODUCT_SKILLS_SECTION)
        .and_then(|value| value.as_bool())
        != Some(false)
}
