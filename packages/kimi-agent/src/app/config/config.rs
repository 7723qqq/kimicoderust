use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use indexmap::IndexMap;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::_base::di::descriptors::DynamicValue;
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::_base::errors::errors::BugIndicatingError;
use crate::app::config::config_pure::deep_merge;
use crate::app::config::errors::CONFIG_INVALID;
use crate::wire::record::WireAsyncError;
use kimi_core::primitives::errors::Error2;

pub trait ConfigSchema: Send + Sync {
    fn parse(&self, value: Value) -> Result<Value, Error2>;
}

#[derive(Debug, Clone)]
pub struct JsonConfigSchema {
    schema: Arc<Value>,
}

impl JsonConfigSchema {
    pub fn new(schema: Value) -> Self {
        Self {
            schema: Arc::new(schema),
        }
    }

    pub fn schema(&self) -> &Value {
        self.schema.as_ref()
    }
}

impl ConfigSchema for JsonConfigSchema {
    fn parse(&self, value: Value) -> Result<Value, Error2> {
        parse_json_schema(self.schema(), value, "$")
    }
}

pub type ConfigMerge = Arc<dyn Fn(Option<&Value>, Option<&Value>) -> Option<Value> + Send + Sync>;
pub type EnvParser = Arc<dyn Fn(&str) -> Option<Value> + Send + Sync>;
pub type GetEnv<'a> = dyn Fn(&str) -> Option<String> + 'a;
pub type ValidateConfig<'a> = dyn Fn(&str, Value) -> Value + 'a;

#[derive(Clone)]
pub struct EnvBinding {
    pub env: String,
    pub deprecated_env: Option<String>,
    pub parse: Option<EnvParser>,
    pub default: Option<Value>,
}

impl EnvBinding {
    pub fn new(env: impl Into<String>) -> Self {
        Self {
            env: env.into(),
            deprecated_env: None,
            parse: None,
            default: None,
        }
    }
}

#[derive(Clone)]
pub enum EnvBindings {
    Name(String),
    Binding(EnvBinding),
    Fields(IndexMap<String, EnvBindings>),
}

pub type AnyEnvBindings = EnvBindings;

impl EnvBindings {
    pub fn named(name: impl Into<String>) -> Self {
        Self::Name(name.into())
    }

    pub fn binding(binding: EnvBinding) -> Self {
        Self::Binding(binding)
    }

    pub(crate) fn as_binding(&self) -> Option<BindingRef<'_>> {
        match self {
            Self::Name(name) => Some(BindingRef {
                env: name,
                deprecated_env: None,
                parse: None,
                default: None,
            }),
            Self::Binding(binding) => Some(BindingRef {
                env: &binding.env,
                deprecated_env: binding.deprecated_env.as_deref(),
                parse: binding.parse.as_ref(),
                default: binding.default.as_ref(),
            }),
            Self::Fields(_) => None,
        }
    }
}

pub(crate) struct BindingRef<'a> {
    pub(crate) env: &'a str,
    pub(crate) deprecated_env: Option<&'a str>,
    pub(crate) parse: Option<&'a EnvParser>,
    pub(crate) default: Option<&'a Value>,
}

pub fn env_bindings<T, P>(_schema: &Value, bindings: &[(&str, &str, P)]) -> EnvBindings
where
    T: Into<Value>,
    P: Fn(&str) -> Option<T> + Copy + Send + Sync + 'static,
{
    let mut fields = IndexMap::new();
    for (field, env, parse) in bindings {
        let parser: EnvParser = Arc::new(move |raw| parse(raw).map(Into::into));
        fields.insert(
            (*field).to_string(),
            EnvBindings::Binding(EnvBinding {
                env: (*env).to_string(),
                deprecated_env: None,
                parse: Some(parser),
                default: None,
            }),
        );
    }
    EnvBindings::Fields(fields)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigKeyDeprecation {
    pub key: String,
    pub replacement: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigDiagnostic {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub domain: String,
    pub severity: String,
    pub message: String,
}

impl ConfigDiagnostic {
    pub fn warning(domain: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            domain: domain.into(),
            severity: "warning".to_string(),
            message: message.into(),
        }
    }

    pub fn error(domain: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            domain: domain.into(),
            severity: "error".to_string(),
            message: message.into(),
        }
    }
}

pub type ConfigCollectDiagnostics =
    Arc<dyn Fn(Option<&Value>) -> Vec<ConfigDiagnostic> + Send + Sync>;
pub type StripEnvBoundFields =
    Arc<dyn Fn(Value, Option<&Value>, &GetEnv) -> Option<Value> + Send + Sync>;
pub type ConfigStripEnv = StripEnvBoundFields;
pub type ConfigFromToml = Arc<dyn Fn(&Value) -> Value + Send + Sync>;
pub type ConfigToToml = Arc<dyn Fn(&Value, &Value) -> Option<Value> + Send + Sync>;

pub fn strip_env_bound_fields(bindings: &EnvBindings) -> StripEnvBoundFields {
    let bindings = bindings.clone();
    Arc::new(move |value, raw, get_env| {
        if value.is_null() || !value.is_object() {
            return Some(value);
        }
        let EnvBindings::Fields(fields) = &bindings else {
            return Some(value);
        };
        let base = raw
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let mut out: Option<Map<String, Value>> = None;
        for (field, binding) in fields {
            let Some(binding) = binding.as_binding() else {
                continue;
            };
            if !resolves_from_env(&binding, get_env) {
                continue;
            }
            let out = out.get_or_insert_with(|| value.as_object().cloned().unwrap_or_default());
            if let Some(raw_value) = base.get(field) {
                out.insert(field.clone(), raw_value.clone());
            } else {
                out.remove(field);
            }
        }
        let Some(out) = out else {
            return Some(value);
        };
        if !out.is_empty() {
            return Some(Value::Object(out));
        }
        if base.is_empty() {
            None
        } else {
            Some(Value::Object(base))
        }
    })
}

fn resolves_from_env(binding: &BindingRef<'_>, get_env: &GetEnv) -> bool {
    std::iter::once(binding.env)
        .chain(binding.deprecated_env)
        .any(|name| {
            get_env(name).is_some_and(|raw| {
                binding
                    .parse
                    .is_none_or(|parse| parse(&raw).is_some())
            })
        })
}

pub type ApplyConfigOverlay = Arc<
    dyn Fn(&mut Value, &GetEnv, &ValidateConfig) -> Vec<&'static str> + Send + Sync,
>;
pub type StripConfigOverlay =
    Arc<dyn Fn(&str, Value, &Map<String, Value>) -> Option<Value> + Send + Sync>;

#[derive(Clone)]
pub struct ConfigEffectiveOverlay {
    pub apply: ApplyConfigOverlay,
    pub strip: Option<StripConfigOverlay>,
}

impl ConfigEffectiveOverlay {
    pub fn new(apply: ApplyConfigOverlay) -> Self {
        Self { apply, strip: None }
    }

    pub fn with_strip(mut self, strip: StripConfigOverlay) -> Self {
        self.strip = Some(strip);
        self
    }
}

#[derive(Clone)]
pub struct ConfigSection {
    pub domain: String,
    pub schema: Arc<dyn ConfigSchema>,
    pub default_value: Option<Value>,
    pub merge: ConfigMerge,
    pub scope: ConfigScope,
    pub env: Option<EnvBindings>,
    pub strip_env: Option<ConfigStripEnv>,
    pub from_toml: Option<ConfigFromToml>,
    pub to_toml: Option<ConfigToToml>,
    pub deprecations: Vec<ConfigKeyDeprecation>,
    pub collect_diagnostics: Option<ConfigCollectDiagnostics>,
}

#[derive(Clone, Default)]
pub struct RegisterSectionOptions {
    pub default_value: Option<Value>,
    pub merge: Option<ConfigMerge>,
    pub scope: Option<ConfigScope>,
    pub env: Option<EnvBindings>,
    pub strip_env: Option<ConfigStripEnv>,
    pub from_toml: Option<ConfigFromToml>,
    pub to_toml: Option<ConfigToToml>,
    pub deprecations: Vec<ConfigKeyDeprecation>,
    pub collect_diagnostics: Option<ConfigCollectDiagnostics>,
}

impl From<Value> for RegisterSectionOptions {
    fn from(default_value: Value) -> Self {
        Self {
            default_value: Some(default_value),
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigSectionRegisteredEvent {
    pub domain: String,
}

#[derive(Clone)]
pub struct ConfigOverlayRegisteredEvent {
    pub overlay: ConfigEffectiveOverlay,
}

pub type ConfigSectionRegisteredListener = Box<dyn Fn(ConfigSectionRegisteredEvent) + Send + Sync>;
pub type ConfigOverlayRegisteredListener = Box<dyn Fn(ConfigOverlayRegisteredEvent) + Send + Sync>;

pub trait IConfigRegistry: Send + Sync {
    fn on_did_register_section(
        &self,
        listener: ConfigSectionRegisteredListener,
    ) -> DisposableRef;
    fn on_did_unregister_section(
        &self,
        listener: ConfigSectionRegisteredListener,
    ) -> DisposableRef;
    fn on_did_register_overlay(
        &self,
        listener: ConfigOverlayRegisteredListener,
    ) -> DisposableRef;
    fn register_section(
        &self,
        domain: &str,
        schema: Arc<dyn ConfigSchema>,
        options: RegisterSectionOptions,
    ) -> Result<(), BugIndicatingError>;
    fn unregister_section(&self, domain: &str);
    fn get_section(&self, domain: &str) -> Option<ConfigSection>;
    fn list_sections(&self) -> Vec<ConfigSection>;
    fn register_effective_overlay(&self, overlay: ConfigEffectiveOverlay);
    fn list_effective_overlays(&self) -> Vec<ConfigEffectiveOverlay>;
    fn validate(&self, domain: &str, value: Value) -> Result<Value, WireAsyncError>;
    fn merge(&self, domain: &str, base: Option<&Value>, patch: Option<&Value>) -> Option<Value>;
    fn default_value(&self, domain: &str) -> Option<Value>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigChangeSource {
    Load,
    Reload,
    Set,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConfigChangedEvent {
    pub domain: String,
    pub source: ConfigChangeSource,
    pub value: Option<Value>,
    pub previous_value: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConfigSectionChangedEvent {
    pub domain: String,
    pub source: ConfigChangeSource,
    pub value: Option<Value>,
    pub previous_value: Option<Value>,
}

pub type ResolvedConfig = IndexMap<String, Value>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigScope {
    Core,
    Session,
    Project,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigTarget {
    User,
    Memory,
}

#[derive(Debug, Clone, Default)]
pub struct ConfigInspectValue {
    pub value: Option<Value>,
    pub default_value: Option<Value>,
    pub user_value: Option<Value>,
    pub memory_value: Option<Value>,
}

#[derive(Debug, Clone, Default)]
pub struct ConfigReplaceSectionsOptions {
    pub preserve_unknown: Option<bool>,
    pub exact_keys: BTreeMap<String, Vec<String>>,
    pub expected_values: BTreeMap<String, Option<Value>>,
}

pub type ConfigChangeListener = Box<dyn Fn(ConfigChangedEvent) + Send + Sync>;
pub type ConfigSectionChangeListener = Box<dyn Fn(ConfigSectionChangedEvent) + Send + Sync>;
pub type ConfigDiagnosticsListener = Box<dyn Fn(Vec<ConfigDiagnostic>) + Send + Sync>;

#[async_trait]
pub trait IConfigService: Send + Sync {
    async fn ready(&self) -> Result<(), WireAsyncError>;
    fn on_did_change_configuration(
        &self,
        listener: ConfigChangeListener,
    ) -> DisposableRef;
    fn on_did_section_change(
        &self,
        listener: ConfigSectionChangeListener,
    ) -> DisposableRef;
    fn on_did_change_diagnostics(
        &self,
        listener: ConfigDiagnosticsListener,
    ) -> DisposableRef;
    fn get(&self, domain: &str) -> Option<Value>;
    async fn assert_valid(&self, domain: &str) -> Result<(), WireAsyncError>;
    fn inspect(&self, domain: &str) -> ConfigInspectValue;
    fn get_all(&self) -> ResolvedConfig;
    async fn set(
        &self,
        domain: &str,
        patch: Option<Value>,
        target: Option<ConfigTarget>,
    ) -> Result<(), WireAsyncError>;
    async fn replace(
        &self,
        domain: &str,
        value: Option<Value>,
        target: Option<ConfigTarget>,
    ) -> Result<(), WireAsyncError>;
    async fn replace_sections(
        &self,
        sections: BTreeMap<String, Option<Value>>,
        target: Option<ConfigTarget>,
        options: Option<ConfigReplaceSectionsOptions>,
    ) -> Result<(), WireAsyncError>;
    async fn reload(&self) -> Result<(), WireAsyncError>;
    fn diagnostics(&self) -> Vec<ConfigDiagnostic>;
}

pub static I_CONFIG_REGISTRY: Lazy<ServiceIdentifier<DynamicValue>> =
    Lazy::new(|| create_decorator("configRegistry"));
pub static I_CONFIG_SERVICE: Lazy<ServiceIdentifier<DynamicValue>> =
    Lazy::new(|| create_decorator("configService"));

static DEFAULT_CONFIG_MERGE: std::sync::LazyLock<ConfigMerge> =
    std::sync::LazyLock::new(|| Arc::new(deep_merge));

pub fn default_config_merge() -> ConfigMerge {
    DEFAULT_CONFIG_MERGE.clone()
}

fn parse_json_schema(schema: &Value, value: Value, path: &str) -> Result<Value, Error2> {
    if let Some(expected) = schema.get("const") {
        if &value != expected {
            return Err(schema_error(path, "value does not match const"));
        }
    }
    if let Some(expected) = schema.get("enum").and_then(Value::as_array) {
        if !expected.contains(&value) {
            return Err(schema_error(path, "value is not in enum"));
        }
    }
    if let Some(all) = schema.get("allOf").and_then(Value::as_array) {
        let mut value = value;
        for entry in all {
            value = parse_json_schema(entry, value, path)?;
        }
        return Ok(value);
    }
    if let Some(any) = schema.get("anyOf").and_then(Value::as_array) {
        let mut last_error = None;
        for entry in any {
            match parse_json_schema(entry, value.clone(), path) {
                Ok(value) => return Ok(value),
                Err(error) => last_error = Some(error),
            }
        }
        return Err(last_error.unwrap_or_else(|| schema_error(path, "value does not match anyOf")));
    }
    if let Some(one) = schema.get("oneOf").and_then(Value::as_array) {
        let mut matches = Vec::new();
        for entry in one {
            if let Ok(value) = parse_json_schema(entry, value.clone(), path) {
                matches.push(value);
            }
        }
        if matches.len() != 1 {
            return Err(schema_error(path, "value must match exactly one oneOf schema"));
        }
        return Ok(matches.remove(0));
    }
    let types = schema.get("type");
    let type_name = types.and_then(Value::as_str);
    let type_matches = match types {
        Some(Value::Array(types)) => types.iter().filter_map(Value::as_str).any(|kind| value_has_type(&value, kind)),
        Some(Value::String(kind)) => value_has_type(&value, kind),
        _ => true,
    };
    if !type_matches {
        return Err(schema_error(path, format!("expected {}", types.and_then(Value::as_str).unwrap_or("declared type"))));
    }
    match type_name.or_else(|| infer_schema_type(schema)) {
        Some("object") => parse_object_schema(schema, value, path),
        Some("array") => parse_array_schema(schema, value, path),
        Some("string") => parse_string_schema(schema, value, path),
        Some("number") | Some("integer") => parse_number_schema(schema, value, path, type_name == Some("integer")),
        _ => Ok(value),
    }
}

fn infer_schema_type(schema: &Value) -> Option<&'static str> {
    if schema.get("properties").is_some() || schema.get("additionalProperties").is_some() {
        Some("object")
    } else if schema.get("items").is_some() {
        Some("array")
    } else {
        None
    }
}

fn value_has_type(value: &Value, kind: &str) -> bool {
    match kind {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        "number" => value.is_number(),
        "integer" => value.as_f64().is_some_and(|number| number.is_finite() && number.fract() == 0.0),
        _ => true,
    }
}

fn parse_object_schema(schema: &Value, value: Value, path: &str) -> Result<Value, Error2> {
    let Value::Object(mut object) = value else {
        return Err(schema_error(path, "expected object"));
    };
    let properties = schema.get("properties").and_then(Value::as_object);
    if let Some(properties) = properties {
        for (key, property_schema) in properties {
            if let Some(property) = object.remove(key) {
                object.insert(key.clone(), parse_json_schema(property_schema, property, &format!("{path}.{key}"))?);
            } else if let Some(default) = property_schema.get("default") {
                object.insert(key.clone(), parse_json_schema(property_schema, default.clone(), &format!("{path}.{key}"))?);
            }
        }
    }
    for required in schema.get("required").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
        if !object.contains_key(required) {
            return Err(schema_error(path, format!("missing required property '{required}'")));
        }
    }
    let additional = schema.get("additionalProperties");
    let keys = object.keys().cloned().collect::<Vec<_>>();
    for key in keys {
        if properties.is_some_and(|properties| properties.contains_key(&key)) {
            continue;
        }
        match additional {
            Some(Value::Bool(false)) => return Err(schema_error(path, format!("unknown property '{key}'"))),
            Some(Value::Bool(true)) => {}
            Some(additional_schema) if additional_schema.is_object() => {
                let property = object.remove(&key).unwrap_or(Value::Null);
                object.insert(key.clone(), parse_json_schema(additional_schema, property, &format!("{path}.{key}"))?);
            }
            None if properties.is_some() => {
                object.remove(&key);
            }
            _ => {}
        }
    }
    Ok(Value::Object(object))
}

fn parse_array_schema(schema: &Value, value: Value, path: &str) -> Result<Value, Error2> {
    let Value::Array(items) = value else {
        return Err(schema_error(path, "expected array"));
    };
    let mut output = Vec::with_capacity(items.len());
    for (index, item) in items.into_iter().enumerate() {
        output.push(match schema.get("items") {
            Some(item_schema) => parse_json_schema(item_schema, item, &format!("{path}[{index}]"))?,
            None => item,
        });
    }
    if let Some(min) = schema.get("minItems").and_then(Value::as_u64)
        && output.len() < min as usize
    {
        return Err(schema_error(path, format!("expected at least {min} items")));
    }
    if let Some(max) = schema.get("maxItems").and_then(Value::as_u64)
        && output.len() > max as usize
    {
        return Err(schema_error(path, format!("expected at most {max} items")));
    }
    Ok(Value::Array(output))
}

fn parse_string_schema(schema: &Value, value: Value, path: &str) -> Result<Value, Error2> {
    let Value::String(text) = value else {
        return Err(schema_error(path, "expected string"));
    };
    let length = text.encode_utf16().count();
    if let Some(min) = schema.get("minLength").and_then(Value::as_u64)
        && length < min as usize
    {
        return Err(schema_error(path, format!("expected at least {min} characters")));
    }
    if let Some(max) = schema.get("maxLength").and_then(Value::as_u64)
        && length > max as usize
    {
        return Err(schema_error(path, format!("expected at most {max} characters")));
    }
    if let Some(pattern) = schema.get("pattern").and_then(Value::as_str) {
        let regex = Regex::new(pattern).map_err(|error| schema_error(path, error.to_string()))?;
        if !regex.is_match(&text) {
            return Err(schema_error(path, "string does not match pattern"));
        }
    }
    Ok(Value::String(text))
}

fn parse_number_schema(schema: &Value, value: Value, path: &str, integer: bool) -> Result<Value, Error2> {
    let Some(number) = value.as_f64() else {
        return Err(schema_error(path, "expected number"));
    };
    if !number.is_finite() || integer && number.fract() != 0.0 {
        return Err(schema_error(path, if integer { "expected integer" } else { "expected finite number" }));
    }
    if schema.get("minimum").and_then(Value::as_f64).is_some_and(|minimum| number < minimum)
        || schema.get("exclusiveMinimum").and_then(Value::as_f64).is_some_and(|minimum| number <= minimum)
        || schema.get("maximum").and_then(Value::as_f64).is_some_and(|maximum| number > maximum)
        || schema.get("exclusiveMaximum").and_then(Value::as_f64).is_some_and(|maximum| number >= maximum)
    {
        return Err(schema_error(path, "number is outside its allowed range"));
    }
    Ok(value)
}

fn schema_error(path: &str, message: impl Into<String>) -> Error2 {
    Error2::new(CONFIG_INVALID, format!("Invalid value at {path}: {}", message.into()))
}

