use std::collections::BTreeMap;

use indexmap::IndexMap;
use kimi_core::primitives::errors::Error2;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Map, Value, json};

use crate::llm_adapter::contract::errors::CONFIG_INVALID_ERROR_CODE;

pub const PROVIDERS_SECTION: &str = "providers";
pub const DEFAULT_PROVIDER_SECTION: &str = "defaultProvider";
pub const ENV_MODEL_PROVIDER_KEY: &str = "__kimi_env__";
pub const MODELS_SECTION: &str = "models";
pub const DEFAULT_MODEL_SECTION: &str = "defaultModel";
pub const MODEL_OVERRIDES_SECTION: &str = "modelOverrides";
pub const THINKING_SECTION: &str = "thinking";
pub const MODEL_CATALOG_SECTION: &str = "modelCatalog";

const PROVIDER_CREDENTIAL_FIELDS: [&str; 3] = ["apiKey", "oauth", "apiKeyEnv"];
const MODEL_OBJECT_FIELDS: [&str; 3] = ["oauth", "overrides", "upgrade"];

pub type ProviderType = String;
pub type ProvidersSection = IndexMap<String, ProviderConfig>;
pub type ModelsSection = IndexMap<String, ModelRecord>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OAuthStorage {
    File,
    Keyring,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthRef {
    pub storage: OAuthStorage,
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_host: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelSource {
    #[serde(rename = "static")]
    Static,
    #[serde(rename = "discover")]
    Discover,
    #[serde(rename = "oauth-catalog")]
    OauthCatalog,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_source: Option<ModelSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_headers: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub type_: Option<ProviderType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<OAuthRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<BTreeMap<String, Value>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Protocol {
    #[serde(rename = "anthropic")]
    Anthropic,
    #[serde(rename = "openai")]
    Openai,
    #[serde(rename = "openai_responses")]
    OpenaiResponses,
    #[serde(rename = "google-genai")]
    GoogleGenai,
    #[serde(rename = "antigravity")]
    Antigravity,
}

impl Protocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::Openai => "openai",
            Self::OpenaiResponses => "openai_responses",
            Self::GoogleGenai => "google-genai",
            Self::Antigravity => "antigravity",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelUpgrade {
    pub model: String,
    pub migration_markdown: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retirement_at: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_context_size: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_size: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_size: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_thinking: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub support_efforts: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub off_effort: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<OAuthRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<Protocol>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aliases: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_context_size: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_size: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_size: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_thinking: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub beta_api: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub support_efforts: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub off_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upgrade: Option<ModelUpgrade>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overrides: Option<ModelOverride>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_keep: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forced_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCatalogConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_interval_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_on_start: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigDiagnostic {
    pub domain: Option<String>,
    pub severity: ConfigDiagnosticSeverity,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigDiagnosticSeverity {
    Warning,
    Error,
}

pub trait ConfigSchema: Send + Sync {
    fn parse(&self, value: Value) -> Result<Value, Error2>;
}

fn schema_error(message: impl Into<String>) -> Error2 {
    Error2::new(CONFIG_INVALID_ERROR_CODE, message)
}

fn decode<T: DeserializeOwned>(value: Value, name: &str) -> Result<T, Error2> {
    serde_json::from_value(value).map_err(|error| schema_error(format!("invalid {name}: {error}")))
}

fn encode<T: Serialize>(value: &T, name: &str) -> Result<Value, Error2> {
    serde_json::to_value(value).map_err(|error| schema_error(format!("invalid {name}: {error}")))
}

fn validate_oauth_ref(value: &OAuthRef) -> Result<(), Error2> {
    if value.key.is_empty() {
        return Err(schema_error("OAuth key must not be empty"));
    }
    if value.oauth_host.as_ref().is_some_and(String::is_empty) {
        return Err(schema_error("OAuth host must not be empty"));
    }
    Ok(())
}

fn validate_provider(value: &ProviderConfig) -> Result<(), Error2> {
    if let Some(oauth) = &value.oauth {
        validate_oauth_ref(oauth)?;
    }
    Ok(())
}

fn validate_model_override(value: &ModelOverride) -> Result<(), Error2> {
    if value.max_context_size == Some(0)
        || value.max_input_size == Some(0)
        || value.max_output_size == Some(0)
    {
        return Err(schema_error("model sizes must be positive integers"));
    }
    Ok(())
}

fn validate_model_record(value: &ModelRecord) -> Result<(), Error2> {
    if let Some(oauth) = &value.oauth {
        validate_oauth_ref(oauth)?;
    }
    if value.max_context_size == Some(0)
        || value.max_input_size == Some(0)
        || value.max_output_size == Some(0)
    {
        return Err(schema_error("model sizes must be positive integers"));
    }
    if let Some(upgrade) = &value.upgrade
        && upgrade.model.is_empty()
    {
        return Err(schema_error("model upgrade target must not be empty"));
    }
    if let Some(overrides) = &value.overrides {
        validate_model_override(overrides)?;
    }
    Ok(())
}

pub struct ProviderTypeSchema;
pub struct OAuthRefSchema;
pub struct ModelSourceSchema;
pub struct ProviderConfigSchema;
pub struct ProvidersSectionSchema;
pub struct ModelOverrideSchema;
pub struct ModelRecordSchema;
pub struct ModelsSectionSchema;
pub struct ModelOverridesSchema;
pub struct ThinkingConfigSchema;
pub struct ModelCatalogConfigSchema;

pub static PROVIDER_TYPE_SCHEMA: ProviderTypeSchema = ProviderTypeSchema;
pub static OAUTH_REF_SCHEMA: OAuthRefSchema = OAuthRefSchema;
pub static MODEL_SOURCE_SCHEMA: ModelSourceSchema = ModelSourceSchema;
pub static PROVIDER_CONFIG_SCHEMA: ProviderConfigSchema = ProviderConfigSchema;
pub static PROVIDERS_SECTION_SCHEMA: ProvidersSectionSchema = ProvidersSectionSchema;
pub static MODEL_OVERRIDE_SCHEMA: ModelOverrideSchema = ModelOverrideSchema;
pub static MODEL_RECORD_SCHEMA: ModelRecordSchema = ModelRecordSchema;
pub static MODELS_SECTION_SCHEMA: ModelsSectionSchema = ModelsSectionSchema;
pub static MODEL_OVERRIDES_SCHEMA: ModelOverridesSchema = ModelOverridesSchema;
pub static THINKING_CONFIG_SCHEMA: ThinkingConfigSchema = ThinkingConfigSchema;
pub static MODEL_CATALOG_CONFIG_SCHEMA: ModelCatalogConfigSchema = ModelCatalogConfigSchema;

impl ConfigSchema for ProviderTypeSchema {
    fn parse(&self, value: Value) -> Result<Value, Error2> {
        decode::<String>(value, "provider type").map(Value::String)
    }
}

impl ConfigSchema for OAuthRefSchema {
    fn parse(&self, value: Value) -> Result<Value, Error2> {
        let decoded: OAuthRef = decode(value, "OAuth reference")?;
        validate_oauth_ref(&decoded)?;
        encode(&decoded, "OAuth reference")
    }
}

impl ConfigSchema for ModelSourceSchema {
    fn parse(&self, value: Value) -> Result<Value, Error2> {
        let decoded: ModelSource = decode(value, "model source")?;
        encode(&decoded, "model source")
    }
}

impl ConfigSchema for ProviderConfigSchema {
    fn parse(&self, value: Value) -> Result<Value, Error2> {
        let decoded: ProviderConfig = decode(value, "provider config")?;
        validate_provider(&decoded)?;
        encode(&decoded, "provider config")
    }
}

impl ConfigSchema for ProvidersSectionSchema {
    fn parse(&self, value: Value) -> Result<Value, Error2> {
        let decoded: ProvidersSection = decode(value, PROVIDERS_SECTION)?;
        for provider in decoded.values() {
            validate_provider(provider)?;
        }
        encode(&decoded, PROVIDERS_SECTION)
    }
}

impl ConfigSchema for ModelOverrideSchema {
    fn parse(&self, value: Value) -> Result<Value, Error2> {
        let decoded: ModelOverride = decode(value, "model override")?;
        validate_model_override(&decoded)?;
        encode(&decoded, "model override")
    }
}

impl ConfigSchema for ModelRecordSchema {
    fn parse(&self, value: Value) -> Result<Value, Error2> {
        let decoded: ModelRecord = decode(value, "model record")?;
        validate_model_record(&decoded)?;
        encode(&decoded, "model record")
    }
}

impl ConfigSchema for ModelsSectionSchema {
    fn parse(&self, value: Value) -> Result<Value, Error2> {
        let decoded: ModelsSection = decode(value, MODELS_SECTION)?;
        for model in decoded.values() {
            validate_model_record(model)?;
        }
        encode(&decoded, MODELS_SECTION)
    }
}

impl ConfigSchema for ModelOverridesSchema {
    fn parse(&self, value: Value) -> Result<Value, Error2> {
        let decoded: ModelOverrides = decode(value, MODEL_OVERRIDES_SECTION)?;
        if decoded.max_completion_tokens == Some(0) {
            return Err(schema_error("maxCompletionTokens must be a positive integer"));
        }
        encode(&decoded, MODEL_OVERRIDES_SECTION)
    }
}

impl ConfigSchema for ThinkingConfigSchema {
    fn parse(&self, value: Value) -> Result<Value, Error2> {
        let decoded: ThinkingConfig = decode(value, THINKING_SECTION)?;
        encode(&decoded, THINKING_SECTION)
    }
}

impl ConfigSchema for ModelCatalogConfigSchema {
    fn parse(&self, value: Value) -> Result<Value, Error2> {
        let decoded: ModelCatalogConfig = decode(value, MODEL_CATALOG_SECTION)?;
        encode(&decoded, MODEL_CATALOG_SECTION)
    }
}

pub fn providers_env_bindings() -> Value {
    json!({
        ENV_MODEL_PROVIDER_KEY: {
            "apiKey": "KIMI_MODEL_API_KEY",
            "type": "KIMI_MODEL_PROVIDER_TYPE",
            "baseUrl": "KIMI_MODEL_BASE_URL"
        }
    })
}

pub fn thinking_env_bindings() -> Value {
    json!({ "forcedEffort": "KIMI_MODEL_THINKING_EFFORT" })
}

pub fn strip_providers_env(value: Value) -> Value {
    let Some(mut object) = value.as_object().cloned() else {
        return value;
    };
    if !object.contains_key(ENV_MODEL_PROVIDER_KEY) {
        return value;
    }
    object.remove(ENV_MODEL_PROVIDER_KEY);
    Value::Object(object)
}

pub fn strip_thinking_env(value: Value) -> Value {
    let Ok(mut decoded) = serde_json::from_value::<ThinkingConfig>(value.clone()) else {
        return value;
    };
    decoded.forced_effort = None;
    serde_json::to_value(decoded).unwrap_or(value)
}

pub fn snake_to_camel(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '_'
            && chars.peek().is_some_and(|next| next.is_ascii_lowercase())
        {
            if let Some(next) = chars.next() {
                output.push(next.to_ascii_uppercase());
            }
        } else {
            output.push(ch);
        }
    }
    output
}

pub fn camel_to_snake(value: &str) -> String {
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

fn plain_object_to_toml(value: &Map<String, Value>, raw: Option<&Value>) -> Map<String, Value> {
    let mut output = clone_record(raw);
    for (key, value) in value {
        output.insert(camel_to_snake(key), value.clone());
    }
    output
}

fn clone_record(value: Option<&Value>) -> Map<String, Value> {
    value.and_then(Value::as_object).cloned().unwrap_or_default()
}

fn provider_entry_from_toml(data: &Map<String, Value>) -> Map<String, Value> {
    let mut output = Map::new();
    for (key, value) in data {
        let target_key = snake_to_camel(key);
        let converted = match target_key.as_str() {
            "oauth" => value
                .as_object()
                .map(|value| Value::Object(transform_plain_object(value)))
                .unwrap_or_else(|| value.clone()),
            "env" | "customHeaders" => value.clone(),
            _ => value.clone(),
        };
        output.insert(target_key, converted);
    }
    output
}

pub fn providers_from_toml(raw_snake: &Value) -> Value {
    let Some(raw) = raw_snake.as_object() else {
        return raw_snake.clone();
    };
    Value::Object(
        raw.iter()
            .map(|(name, entry)| {
                let converted = entry
                    .as_object()
                    .map(|entry| Value::Object(provider_entry_from_toml(entry)))
                    .unwrap_or_else(|| entry.clone());
                (name.clone(), converted)
            })
            .collect(),
    )
}

fn provider_entry_to_toml(provider: &Map<String, Value>, raw_provider: Option<&Value>) -> Map<String, Value> {
    let mut output = clone_record(raw_provider);
    for key in PROVIDER_CREDENTIAL_FIELDS {
        if !provider.contains_key(key) {
            output.remove(&camel_to_snake(key));
        }
    }
    for (key, value) in provider {
        let target = camel_to_snake(key);
        let converted = match key.as_str() {
            "oauth" => value
                .as_object()
                .map(|value| Value::Object(plain_object_to_toml(value, None)))
                .unwrap_or_else(|| value.clone()),
            "env" | "customHeaders" => value.clone(),
            _ => value.clone(),
        };
        output.insert(target, converted);
    }
    output
}

pub fn providers_to_toml(value: &Value, raw_snake: &Value) -> Value {
    let Some(value) = value.as_object() else {
        return value.clone();
    };
    let raw = clone_record(Some(raw_snake));
    Value::Object(
        value
            .iter()
            .map(|(name, entry)| {
                let converted = entry
                    .as_object()
                    .map(|entry| Value::Object(provider_entry_to_toml(entry, raw.get(name))))
                    .unwrap_or_else(|| entry.clone());
                (name.clone(), converted)
            })
            .collect(),
    )
}

fn dotted_alias_suffix(alias: &str, entry: &Map<String, Value>) -> Option<String> {
    for (key, value) in entry {
        if MODEL_OBJECT_FIELDS.contains(&key.as_str()) {
            continue;
        }
        let Some(value) = value.as_object() else {
            continue;
        };
        let nested = format!("{alias}.{key}");
        return Some(dotted_alias_suffix(&nested, value).unwrap_or(nested));
    }
    None
}

fn malformed_model_message(alias: &str, entry: &Map<String, Value>) -> String {
    let base = format!("[models] entry '{alias}' is missing the 'model' field and cannot be used as a model");
    match dotted_alias_suffix(alias, entry) {
        Some(dotted) => format!("{base}; if the alias contains dots, quote the table name (e.g. [models.\"{dotted}\"])."),
        None => format!("{base}."),
    }
}

pub fn collect_malformed_model_entries(raw_models: &Value) -> Vec<ConfigDiagnostic> {
    let Some(models) = raw_models.as_object() else {
        return Vec::new();
    };
    let mut diagnostics = Vec::new();
    for (alias, entry) in models {
        let Some(entry) = entry.as_object() else {
            continue;
        };
        if entry.contains_key("model") || entry.contains_key("name") {
            continue;
        }
        diagnostics.push(ConfigDiagnostic {
            domain: Some(MODELS_SECTION.to_string()),
            severity: ConfigDiagnosticSeverity::Warning,
            message: malformed_model_message(alias, entry),
        });
    }
    diagnostics
}

pub fn models_from_toml(raw_snake: &Value) -> Value {
    let Some(raw) = raw_snake.as_object() else {
        return raw_snake.clone();
    };
    let mut output = Map::new();
    for (id, entry) in raw {
        let Some(entry) = entry.as_object() else {
            output.insert(id.clone(), entry.clone());
            continue;
        };
        let mut converted = transform_plain_object(entry);
        for key in ["overrides", "upgrade"] {
            if let Some(nested) = converted.get(key).and_then(Value::as_object) {
                converted.insert(key.to_string(), Value::Object(transform_plain_object(nested)));
            }
        }
        output.insert(id.clone(), Value::Object(converted));
    }
    Value::Object(output)
}

fn normalized_capabilities(field: &[Value]) -> Vec<Value> {
    let mut replaced: Vec<String> = Vec::new();
    for capability in field {
        let Some(capability) = capability.as_str() else {
            continue;
        };
        let capability = capability.trim();
        if !capability.is_empty() && !replaced.iter().any(|current| current == capability) {
            replaced.push(capability.to_string());
        }
    }
    replaced.into_iter().map(Value::String).collect()
}

fn model_overrides_to_toml(overrides: &Map<String, Value>, raw_snake: Option<&Value>) -> Map<String, Value> {
    let mut output = clone_record(raw_snake);
    for (key, value) in overrides {
        output.insert(camel_to_snake(key), value.clone());
    }
    output
}

pub fn models_to_toml(value: &Value, raw_snake: &Value) -> Value {
    let Some(value) = value.as_object() else {
        return value.clone();
    };
    let raw = clone_record(Some(raw_snake));
    let mut output = Map::new();
    for (id, entry) in value {
        let Some(entry) = entry.as_object() else {
            output.insert(id.clone(), entry.clone());
            continue;
        };
        let mut merged = clone_record(raw.get(id));
        if !entry.contains_key("upgrade") {
            merged.remove("upgrade");
        }
        for (key, field) in entry {
            match key.as_str() {
                "capabilities" => {
                    if let Some(field) = field.as_array() {
                        merged.insert(camel_to_snake(key), Value::Array(normalized_capabilities(field)));
                    } else {
                        merged.insert(camel_to_snake(key), field.clone());
                    }
                }
                "overrides" => {
                    if let Some(field) = field.as_object() {
                        merged.insert(
                            "overrides".to_string(),
                            Value::Object(model_overrides_to_toml(field, merged.get("overrides"))),
                        );
                    } else {
                        merged.insert("overrides".to_string(), field.clone());
                    }
                }
                "upgrade" => {
                    if let Some(field) = field.as_object() {
                        merged.insert(
                            "upgrade".to_string(),
                            Value::Object(plain_object_to_toml(field, None)),
                        );
                    } else {
                        merged.remove("upgrade");
                    }
                }
                _ => {
                    merged.insert(camel_to_snake(key), field.clone());
                }
            }
        }
        output.insert(id.clone(), Value::Object(merged));
    }
    Value::Object(output)
}

pub type ConfigFromToml = fn(&Value) -> Value;
pub type ConfigToToml = fn(&Value, &Value) -> Value;
pub type ConfigStripEnv = fn(Value) -> Value;
pub type ConfigCollectDiagnostics = fn(&Value) -> Vec<ConfigDiagnostic>;

#[derive(Clone)]
pub struct ConfigSectionContribution {
    pub domain: &'static str,
    pub schema: &'static dyn ConfigSchema,
    pub default_value: Option<Value>,
    pub env: Option<Value>,
    pub strip_env: Option<ConfigStripEnv>,
    pub from_toml: Option<ConfigFromToml>,
    pub to_toml: Option<ConfigToToml>,
    pub collect_diagnostics: Option<ConfigCollectDiagnostics>,
}

impl ConfigSectionContribution {
    fn new(domain: &'static str, schema: &'static dyn ConfigSchema) -> Self {
        Self {
            domain,
            schema,
            default_value: None,
            env: None,
            strip_env: None,
            from_toml: None,
            to_toml: None,
            collect_diagnostics: None,
        }
    }
}

pub fn config_section_contributions() -> Vec<ConfigSectionContribution> {
    let mut providers = ConfigSectionContribution::new(PROVIDERS_SECTION, &PROVIDERS_SECTION_SCHEMA);
    providers.default_value = Some(json!({}));
    providers.env = Some(providers_env_bindings());
    providers.strip_env = Some(strip_providers_env);
    providers.from_toml = Some(providers_from_toml);
    providers.to_toml = Some(providers_to_toml);

    let mut models = ConfigSectionContribution::new(MODELS_SECTION, &MODELS_SECTION_SCHEMA);
    models.default_value = Some(json!({}));
    models.from_toml = Some(models_from_toml);
    models.to_toml = Some(models_to_toml);
    models.collect_diagnostics = Some(collect_malformed_model_entries);

    let mut overrides = ConfigSectionContribution::new(MODEL_OVERRIDES_SECTION, &MODEL_OVERRIDES_SCHEMA);
    overrides.default_value = Some(json!({}));

    let mut thinking = ConfigSectionContribution::new(THINKING_SECTION, &THINKING_CONFIG_SCHEMA);
    thinking.env = Some(thinking_env_bindings());
    thinking.strip_env = Some(strip_thinking_env);

    let catalog = ConfigSectionContribution::new(MODEL_CATALOG_SECTION, &MODEL_CATALOG_CONFIG_SCHEMA);
    vec![providers, models, overrides, thinking, catalog]
}

pub fn register_config_sections(mut register: impl FnMut(ConfigSectionContribution)) {
    for contribution in config_section_contributions() {
        register(contribution);
    }
}
