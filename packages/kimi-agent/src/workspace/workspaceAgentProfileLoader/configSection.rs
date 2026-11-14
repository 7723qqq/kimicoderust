use std::sync::LazyLock;

use serde_json::{Value, json};

use crate::app::config::config_section_contributions::register_config_section;

pub const EXTRA_AGENT_DIRS_SECTION: &str = "extraAgentDirs";

pub static EXTRA_AGENT_DIRS_CONFIG_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "array",
        "items": { "type": "string" }
    })
});

pub type ExtraAgentDirsConfig = Option<Vec<String>>;

pub fn register_extra_agent_dirs_config() {
    register_config_section(
        EXTRA_AGENT_DIRS_SECTION,
        EXTRA_AGENT_DIRS_CONFIG_SCHEMA.clone(),
        Value::Array(Vec::new()),
    );
}
