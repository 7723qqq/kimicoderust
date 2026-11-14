use std::sync::LazyLock;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::app::config::config::IConfigService;
use crate::app::config::config_section_contributions::register_config_section;

pub const WORKSPACE_TRUST_SECTION: &str = "workspace";

pub static WORKSPACE_TRUST_CONFIG_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "properties": {
            "requireTrust": { "type": "boolean" }
        }
    })
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceTrustConfig {
    pub require_trust: Option<bool>,
}

pub const DEFAULT_WORKSPACE_TRUST_CONFIG: WorkspaceTrustConfig = WorkspaceTrustConfig {
    require_trust: Some(true),
};

pub fn workspace_trust_required(config: &dyn IConfigService) -> bool {
    config
        .get(WORKSPACE_TRUST_SECTION)
        .as_ref()
        .and_then(|section| section.get("requireTrust"))
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

pub fn register_workspace_trust_config() {
    register_config_section(
        WORKSPACE_TRUST_SECTION,
        WORKSPACE_TRUST_CONFIG_SCHEMA.clone(),
        serde_json::to_value(DEFAULT_WORKSPACE_TRUST_CONFIG).unwrap(),
    );
}
