pub use crate::config::*;

use std::sync::LazyLock;

use serde_json::{Value, json};

use crate::app::config::config::RegisterSectionOptions;
use crate::app::config::config_section_contributions::register_config_section;

pub const DEFAULT_PERMISSION_MODE_SECTION: &str = "defaultPermissionMode";

pub static DEFAULT_PERMISSION_MODE_SCHEMA: LazyLock<Value> =
    LazyLock::new(|| json!({"enum": ["manual", "auto", "yolo"]}));

pub fn register_default_permission_mode_config() {
    register_config_section(
        DEFAULT_PERMISSION_MODE_SECTION,
        DEFAULT_PERMISSION_MODE_SCHEMA.clone(),
        RegisterSectionOptions::default(),
    );
}

