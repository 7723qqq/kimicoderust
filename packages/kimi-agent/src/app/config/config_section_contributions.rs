use std::sync::{Arc, LazyLock};

use parking_lot::Mutex;
use serde_json::Value;

use crate::_base::di::collection::{CollectionOptions, CollectionToken, collection};

use super::config::{ConfigSchema, JsonConfigSchema, RegisterSectionOptions};

#[derive(Clone)]
pub struct ConfigSectionContribution {
    pub domain: String,
    pub schema: Arc<dyn ConfigSchema>,
    pub options: RegisterSectionOptions,
}

pub static CONFIG_SECTION_CONTRIBUTION: LazyLock<CollectionToken<ConfigSectionContribution>> =
    LazyLock::new(|| collection("config-section", CollectionOptions::default()));

static CONTRIBUTIONS: LazyLock<Mutex<Vec<ConfigSectionContribution>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

pub fn register_config_section(
    domain: &str,
    schema: Value,
    options: impl Into<RegisterSectionOptions>,
) {
    register_config_schema_section(domain, Arc::new(JsonConfigSchema::new(schema)), options);
}

pub fn register_config_schema_section(
    domain: &str,
    schema: Arc<dyn ConfigSchema>,
    options: impl Into<RegisterSectionOptions>,
) {
    CONTRIBUTIONS.lock().push(ConfigSectionContribution {
        domain: domain.to_string(),
        schema,
        options: options.into(),
    });
}

pub fn register_config_section_without_default(domain: &str, schema: Value) {
    register_config_section(domain, schema, RegisterSectionOptions::default());
}

pub fn get_config_section_contributions() -> Vec<ConfigSectionContribution> {
    CONTRIBUTIONS.lock().clone()
}

pub fn _clear_config_section_contributions_for_tests() {
    CONTRIBUTIONS.lock().clear();
}
