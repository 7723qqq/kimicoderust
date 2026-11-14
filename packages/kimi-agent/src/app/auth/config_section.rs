use std::collections::HashMap;
use std::sync::LazyLock;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::app::config::config_overlay_contributions::register_config_overlay;
use crate::app::config::config_section_contributions::register_config_section;
use crate::app::kosong_config::config_section::OAuthRef;

pub const SERVICES_SECTION: &str = "services";
pub const WEB_SEARCH_BASE_URL_ENV: &str = "KIMI_WEB_SEARCH_BASE_URL";
pub const WEB_SEARCH_API_KEY_ENV: &str = "KIMI_WEB_SEARCH_API_KEY";
pub const WEB_FETCH_BASE_URL_ENV: &str = "KIMI_WEB_FETCH_BASE_URL";
pub const WEB_FETCH_API_KEY_ENV: &str = "KIMI_WEB_FETCH_API_KEY";

static STRING_RECORD_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({"type": "object", "additionalProperties": {"type": "string"}})
});

static OAUTH_REF_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "required": ["storage", "key"],
        "properties": {
            "storage": {"enum": ["file", "keyring"]},
            "key": {"type": "string", "minLength": 1},
            "oauthHost": {"type": "string", "minLength": 1}
        }
    })
});

pub static MOONSHOT_SERVICE_CONFIG_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "properties": {
            "baseUrl": {"type": "string"},
            "apiKey": {"type": "string"},
            "oauth": OAUTH_REF_SCHEMA.clone(),
            "customHeaders": STRING_RECORD_SCHEMA.clone()
        }
    })
});

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MoonshotServiceConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<OAuthRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_headers: Option<HashMap<String, String>>,
}

pub static SERVICES_CONFIG_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "properties": {
            "moonshotSearch": MOONSHOT_SERVICE_CONFIG_SCHEMA.clone(),
            "moonshotFetch": MOONSHOT_SERVICE_CONFIG_SCHEMA.clone()
        },
        "additionalProperties": true
    })
});

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServicesConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moonshot_search: Option<MoonshotServiceConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moonshot_fetch: Option<MoonshotServiceConfig>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceEnvBinding {
    pub service: &'static str,
    pub field: &'static str,
    pub env: &'static str,
}

pub static SERVICES_ENV_BINDINGS: [ServiceEnvBinding; 4] = [
    ServiceEnvBinding {
        service: "moonshotSearch",
        field: "baseUrl",
        env: WEB_SEARCH_BASE_URL_ENV,
    },
    ServiceEnvBinding {
        service: "moonshotSearch",
        field: "apiKey",
        env: WEB_SEARCH_API_KEY_ENV,
    },
    ServiceEnvBinding {
        service: "moonshotFetch",
        field: "baseUrl",
        env: WEB_FETCH_BASE_URL_ENV,
    },
    ServiceEnvBinding {
        service: "moonshotFetch",
        field: "apiKey",
        env: WEB_FETCH_API_KEY_ENV,
    },
];

pub fn services_env_bindings() -> &'static [ServiceEnvBinding] {
    &SERVICES_ENV_BINDINGS
}

fn non_blank_env(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

pub type GetEnv = dyn Fn(&str) -> Option<String>;
pub type ValidateConfig = dyn Fn(&str, Value) -> Value;

pub fn apply_services_credential_env_overlay(
    effective: &mut Value,
    get_env: &GetEnv,
    validate: &ValidateConfig,
) -> Vec<&'static str> {
    let Some(services) = effective.get(SERVICES_SECTION).and_then(Value::as_object) else {
        return Vec::new();
    };
    let (moonshot_search, search_changed) = isolate_env_service_credentials(
        services.get("moonshotSearch"),
        get_env,
        WEB_SEARCH_BASE_URL_ENV,
        WEB_SEARCH_API_KEY_ENV,
    );
    let (moonshot_fetch, fetch_changed) = isolate_env_service_credentials(
        services.get("moonshotFetch"),
        get_env,
        WEB_FETCH_BASE_URL_ENV,
        WEB_FETCH_API_KEY_ENV,
    );
    if !search_changed && !fetch_changed {
        return Vec::new();
    }
    let mut next = services.clone();
    match moonshot_search {
        Some(value) => {
            next.insert("moonshotSearch".to_string(), value);
        }
        None => {
            next.remove("moonshotSearch");
        }
    }
    match moonshot_fetch {
        Some(value) => {
            next.insert("moonshotFetch".to_string(), value);
        }
        None => {
            next.remove("moonshotFetch");
        }
    }
    effective[SERVICES_SECTION] = validate(SERVICES_SECTION, Value::Object(next));
    vec![SERVICES_SECTION]
}

fn isolate_env_service_credentials(
    service: Option<&Value>,
    get_env: &GetEnv,
    base_url_env: &str,
    api_key_env: &str,
) -> (Option<Value>, bool) {
    let base_url = get_env(base_url_env).as_deref().and_then(non_blank_env);
    let api_key = get_env(api_key_env).as_deref().and_then(non_blank_env);
    if let Some(base_url) = base_url {
        let mut output = Map::new();
        output.insert("baseUrl".to_string(), Value::String(base_url));
        if let Some(api_key) = api_key {
            output.insert("apiKey".to_string(), Value::String(api_key));
        }
        return (Some(Value::Object(output)), true);
    }
    let Some(api_key) = api_key else {
        return (service.cloned(), false);
    };
    let Some(service) = service.and_then(Value::as_object) else {
        let mut output = Map::new();
        output.insert("apiKey".to_string(), Value::String(api_key));
        return (Some(Value::Object(output)), true);
    };
    let mut output = service.clone();
    output.remove("apiKey");
    output.remove("oauth");
    output.insert("apiKey".to_string(), Value::String(api_key));
    (Some(Value::Object(output)), true)
}

pub fn strip_services_env(value: Value, raw: Option<&Value>, get_env: &GetEnv) -> Value {
    let Some(value_object) = value.as_object() else {
        return value;
    };
    let mut output: Option<Map<String, Value>> = None;
    for (key, base_url_env, api_key_env) in [
        ("moonshotSearch", WEB_SEARCH_BASE_URL_ENV, WEB_SEARCH_API_KEY_ENV),
        ("moonshotFetch", WEB_FETCH_BASE_URL_ENV, WEB_FETCH_API_KEY_ENV),
    ] {
        let Some(entry) = value_object.get(key) else {
            continue;
        };
        let raw_entry = raw.and_then(Value::as_object).and_then(|raw| raw.get(key));
        let (stripped, changed) =
            strip_service_env(entry, raw_entry, get_env, base_url_env, api_key_env);
        if !changed {
            continue;
        }
        let out = output.get_or_insert_with(|| value_object.clone());
        if stripped.is_null() {
            out.remove(key);
        } else {
            out.insert(key.to_string(), stripped);
        }
    }
    output.map(Value::Object).unwrap_or(value)
}

fn strip_service_env(
    service: &Value,
    raw_service: Option<&Value>,
    get_env: &GetEnv,
    base_url_env: &str,
    api_key_env: &str,
) -> (Value, bool) {
    let Some(service_object) = service.as_object() else {
        return (service.clone(), false);
    };
    let raw_object = raw_service.and_then(Value::as_object).cloned().unwrap_or_default();
    let mut output: Option<Map<String, Value>> = None;
    for (field, env) in [("baseUrl", base_url_env), ("apiKey", api_key_env)] {
        if get_env(env).as_deref().and_then(non_blank_env).is_none() {
            continue;
        }
        let out = output.get_or_insert_with(|| service_object.clone());
        if let Some(raw_value) = raw_object.get(field) {
            out.insert(field.to_string(), raw_value.clone());
        } else {
            out.remove(field);
        }
    }
    let Some(output) = output else {
        return (service.clone(), false);
    };
    if !output.is_empty() {
        return (Value::Object(output), true);
    }
    if raw_object.is_empty() {
        (Value::Null, true)
    } else {
        (Value::Object(raw_object), true)
    }
}

pub fn services_from_toml(raw_snake: &Value) -> Value {
    let Some(raw) = raw_snake.as_object() else {
        return raw_snake.clone();
    };
    Value::Object(
        raw.iter()
            .map(|(name, entry)| {
                let converted = entry
                    .as_object()
                    .map(|entry| Value::Object(service_entry_from_toml(entry)))
                    .unwrap_or_else(|| entry.clone());
                (snake_to_camel(name), converted)
            })
            .collect(),
    )
}

fn service_entry_from_toml(data: &Map<String, Value>) -> Map<String, Value> {
    let mut output = Map::new();
    for (key, value) in data {
        let target_key = snake_to_camel(key);
        let converted = match target_key.as_str() {
            "oauth" => value
                .as_object()
                .map(|value| Value::Object(transform_plain_object(value)))
                .unwrap_or_else(|| value.clone()),
            "customHeaders" => value.clone(),
            _ => value.clone(),
        };
        output.insert(target_key, converted);
    }
    output
}

pub fn services_to_toml(value: &Value, raw_snake: &Value) -> Value {
    let Some(value) = value.as_object() else {
        return value.clone();
    };
    let mut output = clone_record(raw_snake);
    write_service(&mut output, "moonshot_search", value.get("moonshotSearch"));
    write_service(&mut output, "moonshot_fetch", value.get("moonshotFetch"));
    Value::Object(output)
}

fn write_service(output: &mut Map<String, Value>, snake_key: &str, service: Option<&Value>) {
    if let Some(service) = service.and_then(Value::as_object) {
        output.insert(snake_key.to_string(), Value::Object(service_entry_to_toml(service)));
    } else {
        output.remove(snake_key);
    }
}

fn service_entry_to_toml(service: &Map<String, Value>) -> Map<String, Value> {
    let mut output = Map::new();
    for (key, value) in service {
        let target = camel_to_snake(key);
        match key.as_str() {
            "oauth" if value.is_object() => {
                output.insert(target, Value::Object(plain_object_to_toml(value)));
            }
            "customHeaders" => {
                output.insert(target, Value::Object(clone_record(value)));
            }
            _ => {
                output.insert(target, value.clone());
            }
        }
    }
    output
}

fn snake_to_camel(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '_' && chars.peek().is_some_and(|next| next.is_ascii_lowercase()) {
            if let Some(next) = chars.next() {
                output.push(next.to_ascii_uppercase());
            }
        } else {
            output.push(ch);
        }
    }
    output
}

fn camel_to_snake(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_uppercase() {
            output.push('_');
            output.push(ch.to_ascii_lowercase());
        } else {
            output.push(ch);
        }
    }
    output
}

fn transform_plain_object(value: &Map<String, Value>) -> Map<String, Value> {
    value
        .iter()
        .map(|(key, value)| (snake_to_camel(key), value.clone()))
        .collect()
}

fn plain_object_to_toml(value: &Value) -> Map<String, Value> {
    value
        .as_object()
        .map(|value| {
            value
                .iter()
                .map(|(key, value)| (camel_to_snake(key), value.clone()))
                .collect()
        })
        .unwrap_or_default()
}

fn clone_record(value: &Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

pub fn register_services_config() {
    register_config_section(
        SERVICES_SECTION,
        SERVICES_CONFIG_SCHEMA.clone(),
        Value::Object(Map::new()),
    );
    register_config_overlay(apply_services_credential_env_overlay);
}
