pub use crate::config::*;

use std::sync::LazyLock;

use serde_json::{Value, json};

use crate::app::config::config::{
    EnvBindings, RegisterSectionOptions, StripEnvBoundFields, env_bindings, strip_env_bound_fields,
};
use crate::app::config::config_section_contributions::register_config_section;

pub const IMAGE_SECTION: &str = "image";
pub const IMAGE_MAX_EDGE_ENV: &str = "KIMI_IMAGE_MAX_EDGE_PX";
pub const IMAGE_READ_BYTE_BUDGET_ENV: &str = "KIMI_IMAGE_READ_BYTE_BUDGET";

pub type ImageConfig = crate::config::ImageConfig;

pub static IMAGE_CONFIG_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "properties": {
            "maxEdgePx": {"type": "integer", "minimum": 1},
            "readByteBudget": {"type": "integer", "minimum": 1},
        }
    })
});

fn parse_positive_int(raw: &str) -> Option<u64> {
    let value = raw.trim();
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse::<u64>().ok().filter(|value| *value > 0)
}

pub fn image_env_bindings() -> EnvBindings {
    env_bindings(
        &IMAGE_CONFIG_SCHEMA,
        &[
            ("maxEdgePx", IMAGE_MAX_EDGE_ENV, parse_positive_int),
            ("readByteBudget", IMAGE_READ_BYTE_BUDGET_ENV, parse_positive_int),
        ],
    )
}

pub fn strip_image_env() -> StripEnvBoundFields {
    strip_env_bound_fields(&image_env_bindings())
}

pub fn register_image_config() {
    register_config_section(
        IMAGE_SECTION,
        IMAGE_CONFIG_SCHEMA.clone(),
        RegisterSectionOptions {
            default_value: Some(json!({})),
            env: Some(image_env_bindings()),
            strip_env: Some(strip_image_env()),
            ..Default::default()
        },
    );
}
