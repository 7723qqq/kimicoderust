use serde_json::{Map, Value};

use super::config_section::GITHUB_SECTION;

pub const GITHUB_TOKEN_ENV_VARS: [&str; 2] = ["GITHUB_TOKEN", "GH_TOKEN"];
pub const GITHUB_BASE_URL_ENV_VAR: &str = "GITHUB_API_URL";

fn trimmed(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let text = value.trim();
        (!text.is_empty()).then(|| text.to_string())
    })
}

fn section_of(value: Option<&Value>) -> Map<String, Value> {
    value.and_then(Value::as_object).cloned().unwrap_or_default()
}

fn current_string(section: &Map<String, Value>, key: &str) -> Option<String> {
    trimmed(section.get(key).and_then(Value::as_str).map(str::to_string))
}

#[derive(Debug, Clone, Copy, Default)]
pub struct GithubEnvOverlay;

impl GithubEnvOverlay {
    pub fn apply(
        &self,
        effective: &mut Map<String, Value>,
        get_env: impl Fn(&str) -> Option<String>,
    ) -> Vec<&'static str> {
        let mut section = section_of(effective.get(GITHUB_SECTION));
        let mut changed = false;
        if current_string(&section, "token").is_none() {
            for name in GITHUB_TOKEN_ENV_VARS {
                if let Some(value) = trimmed(get_env(name)) {
                    section.insert("token".to_string(), Value::String(value));
                    changed = true;
                    break;
                }
            }
        }
        if current_string(&section, "baseUrl").is_none()
            && let Some(value) = trimmed(get_env(GITHUB_BASE_URL_ENV_VAR))
        {
            section.insert("baseUrl".to_string(), Value::String(value));
            changed = true;
        }
        if !changed {
            return Vec::new();
        }
        effective.insert(GITHUB_SECTION.to_string(), Value::Object(section));
        vec![GITHUB_SECTION]
    }

    pub fn strip(&self, domain: &str, value: Value, raw_snake: &Map<String, Value>) -> Value {
        if domain != GITHUB_SECTION {
            return value;
        }
        let Some(value) = value.as_object() else {
            return value;
        };
        let raw = section_of(raw_snake.get(GITHUB_SECTION));
        let mut next = value.clone();
        if !raw.contains_key("token") {
            next.remove("token");
        }
        if !raw.contains_key("base_url") {
            next.remove("baseUrl");
        }
        Value::Object(next)
    }
}

pub const GITHUB_ENV_OVERLAY: GithubEnvOverlay = GithubEnvOverlay;
