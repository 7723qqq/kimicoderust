use serde_json::{Map, Value};

use super::config::{ConfigDiagnostic, ConfigSection};
use super::config_pure::is_plain_object;
use super::toml::camel_to_snake;

pub fn collect_key_deprecations(
    raw_snake: &Map<String, Value>,
    sections: &[ConfigSection],
) -> Vec<ConfigDiagnostic> {
    let mut diagnostics = Vec::new();
    for section in sections {
        if section.deprecations.is_empty() {
            continue;
        }
        let snake_domain = camel_to_snake(&section.domain);
        let Some(raw_section) = raw_snake.get(&snake_domain).filter(|value| is_plain_object(value)) else {
            continue;
        };
        for deprecation in &section.deprecations {
            if raw_section.get(&deprecation.key).is_none() {
                continue;
            }
            let mut message = format!(
                "[{snake_domain}] '{}' is deprecated and no longer used; rename it to '{}'.",
                deprecation.key, deprecation.replacement
            );
            if let Some(extra) = &deprecation.message {
                message.push(' ');
                message.push_str(extra);
            }
            message.push_str(" Run /update-config to fix it.");
            diagnostics.push(ConfigDiagnostic {
                domain: section.domain.clone(),
                severity: "warning".to_string(),
                message,
            });
        }
    }
    diagnostics
}
