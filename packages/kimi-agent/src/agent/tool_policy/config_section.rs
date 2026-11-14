use std::sync::LazyLock;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::app::config::config::RegisterSectionOptions;
use crate::app::config::config_section_contributions::register_config_section;

pub const TOOLS_SECTION: &str = "tools";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolsConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled: Option<Vec<String>>,
}

pub static TOOLS_CONFIG_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "properties": {
            "enabled": { "type": "array", "items": { "type": "string" } },
            "disabled": { "type": "array", "items": { "type": "string" } }
        }
    })
});

pub fn register_tools_config() {
    register_config_section(
        TOOLS_SECTION,
        TOOLS_CONFIG_SCHEMA.clone(),
        RegisterSectionOptions::default(),
    );
}
