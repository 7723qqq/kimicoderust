use std::collections::HashMap;
use std::sync::LazyLock;

use serde_json::{Map, Value, json};

use crate::app::config::config::{ConfigDiagnostic, EnvBindings, IConfigService, env_bindings, strip_env_bound_fields};
use crate::app::config::config_section_contributions::register_config_section;
use crate::app::config::toml::is_plain_object;
use crate::app::kosong_config::config_section::THINKING_SECTION;
use crate::llm_adapter::model::catalog::{IModelCatalog, Model};
use crate::llm_adapter::model::thinking::{
    declared_default_effort_for_model, model_supports_thinking, model_supports_thinking_effort,
    normalize_requested_thinking_effort,
};
use crate::mcp_core::errors::MCP_STARTUP_FAILED;
use crate::wire::record::WireAsyncError;
use kimi_core::primitives::errors::{Error2, Error2Options};
use kimi_core::primitives::errors::domains::config::CONFIG_INVALID;

pub const SUBAGENT_SECTION: &str = "subagent";
pub const SECONDARY_MODEL_SECTION: &str = "secondaryModel";
pub const DEFAULT_SUBAGENT_TIMEOUT_MS: u64 = 2 * 60 * 60 * 1000;
pub const SUBAGENT_TIMEOUT_ENV: &str = "KIMI_SUBAGENT_TIMEOUT_MS";

pub static SUBAGENT_CONFIG_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({"type": "object", "properties": {"timeoutMs": {"type": "integer", "minimum": 0}}})
});
pub static SECONDARY_MODEL_CONFIG_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "properties": {
            "defaultModel": {"type": "string", "minLength": 1},
            "models": {"type": "object"},
            "model": {"type": "string", "minLength": 1},
            "defaultEffort": {"type": "string"}
        }
    })
});

pub fn parse_timeout_ms_env(raw: &str) -> Option<u64> {
    let parsed = raw.parse::<f64>().ok()?;
    (parsed.is_finite() && parsed.fract() == 0.0 && parsed >= 1.0).then_some(parsed as u64)
}

pub fn subagent_env_bindings() -> EnvBindings {
    env_bindings(&SUBAGENT_CONFIG_SCHEMA, &[("timeoutMs", SUBAGENT_TIMEOUT_ENV, parse_timeout_ms_env)])
}

pub fn strip_subagent_env() -> crate::app::config::config::StripEnvBoundFields {
    strip_env_bound_fields(&subagent_env_bindings())
}

pub fn register_subagent_config() {
    register_config_section(
        SUBAGENT_SECTION,
        SUBAGENT_CONFIG_SCHEMA.clone(),
        json!({"timeoutMs": DEFAULT_SUBAGENT_TIMEOUT_MS}),
    );
}

pub fn register_secondary_model_config() {
    register_config_section(
        SECONDARY_MODEL_SECTION,
        SECONDARY_MODEL_CONFIG_SCHEMA.clone(),
        Value::Object(Map::new()),
    );
}

pub const SECONDARY_MODEL_FORCE_REMOVED_MESSAGE: &str = "[secondary_model] 'force' is no longer supported and is ignored: a lone default_model already pins every Agent and AgentSwarm subagent to that model, and a [secondary_model.models] table only offers its own entries. Remove the key; run /update-config to fix it.";

const REMOVED_SECONDARY_MODEL_TUNING_KEYS: &[&str] = &[
    "max_context_size",
    "max_input_size",
    "max_output_size",
    "capabilities",
    "display_name",
    "reasoning_key",
    "adaptive_thinking",
    "support_efforts",
    "off_effort",
];

pub fn secondary_model_removed_keys_message(keys: &[String]) -> String {
    format!(
        "[secondary_model] {} {} ignored: subagents bind the [models] entry as configured. Set model tuning on the [models.<alias>] entry instead and remove {} from [secondary_model].",
        keys.join(", "),
        if keys.len() == 1 { "is" } else { "are" },
        if keys.len() == 1 { "this key" } else { "these keys" }
    )
}

pub fn collect_removed_secondary_model_keys(raw_section: &Value) -> Vec<ConfigDiagnostic> {
    if !is_plain_object(raw_section) {
        return Vec::new();
    }
    let mut messages = Vec::new();
    if raw_section.get("force").is_some() {
        messages.push(SECONDARY_MODEL_FORCE_REMOVED_MESSAGE.to_string());
    }
    let tuning_keys = REMOVED_SECONDARY_MODEL_TUNING_KEYS
        .iter()
        .filter(|key| raw_section.get(**key).is_some())
        .map(|key| key.to_string())
        .collect::<Vec<_>>();
    if !tuning_keys.is_empty() {
        messages.push(secondary_model_removed_keys_message(&tuning_keys));
    }
    messages
        .into_iter()
        .map(|message| ConfigDiagnostic {
            domain: SECONDARY_MODEL_SECTION.to_string(),
            severity: "warning".to_string(),
            message,
        })
        .collect()
}

pub fn resolve_subagent_timeout_ms(config: &dyn IConfigService) -> u64 {
    config
        .get(SUBAGENT_SECTION)
        .as_ref()
        .and_then(|section| section.get("timeoutMs"))
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_SUBAGENT_TIMEOUT_MS)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentModelPool {
    pub default_model: Option<String>,
    pub models: HashMap<String, String>,
}

pub fn resolve_subagent_model_pool(config: &dyn IConfigService) -> Option<SubagentModelPool> {
    let section = config.get(SECONDARY_MODEL_SECTION)?;
    if let Some(models) = section.get("models").and_then(Value::as_object) {
        return Some(SubagentModelPool {
            default_model: section.get("defaultModel").and_then(Value::as_str).map(str::to_string),
            models: models
                .iter()
                .filter_map(|(key, value)| value.as_str().map(|value| (key.clone(), value.to_string())))
                .collect(),
        });
    }
    if let Some(default_model) = section.get("defaultModel").and_then(Value::as_str) {
        return Some(SubagentModelPool {
            default_model: Some(default_model.to_string()),
            models: HashMap::from([(default_model.to_string(), String::new())]),
        });
    }
    if let Some(model) = section.get("model").and_then(Value::as_str) {
        return Some(SubagentModelPool {
            default_model: Some(model.to_string()),
            models: HashMap::from([(model.to_string(), String::new())]),
        });
    }
    None
}

pub fn exposes_subagent_model_choice(config: &dyn IConfigService) -> bool {
    resolve_subagent_model_pool(config).is_some_and(|pool| pool.models.len() > 1)
}

pub const SECONDARY_MODEL_DEFAULT_MODEL_REQUIRED_MESSAGE: &str =
    "[secondary_model].default_model is required when [secondary_model.models] is configured";

pub fn assert_valid_subagent_model_pool(
    pool: &SubagentModelPool,
    model_catalog: &dyn IModelCatalog,
) -> Result<(), WireAsyncError> {
    let aliases = pool.models.keys().cloned().collect::<Vec<_>>();
    let Some(default_model) = &pool.default_model else {
        return Err(Box::new(Error2::new(
            CONFIG_INVALID,
            SECONDARY_MODEL_DEFAULT_MODEL_REQUIRED_MESSAGE,
        )));
    };
    if !pool.models.contains_key(default_model) {
        return Err(Box::new(Error2::with_options(
            CONFIG_INVALID,
            format!("[secondary_model].default_model \"{default_model}\" is not a [secondary_model.models] key. Available models: {}.", aliases.join(", ")),
            Error2Options {
                details: Some(serde_json::Map::from_iter([
                    ("model".to_string(), default_model.clone().into()),
                    ("availableModels".to_string(), aliases.into()),
                ])),
                ..Default::default()
            },
        )));
    }
    for alias in aliases {
        if let Err(error) = model_catalog.get(&alias) {
            return Err(Box::new(Error2::with_options(
                CONFIG_INVALID,
                format!("[secondary_model.models] entry \"{alias}\" could not be resolved: {error}"),
                Error2Options {
                    cause: Some(error),
                    details: Some(serde_json::Map::from_iter([("model".to_string(), alias.into())])),
                    ..Default::default()
                },
            )));
        }
    }
    Ok(())
}

pub fn assert_valid_subagent_model_config(
    config: &dyn IConfigService,
    model_catalog: &dyn IModelCatalog,
) -> Result<(), WireAsyncError> {
    let section = config.get(SECONDARY_MODEL_SECTION);
    let pool = resolve_subagent_model_pool(config);
    if let Some(pool) = &pool {
        assert_valid_subagent_model_pool(pool, model_catalog)?;
    }
    assert_valid_subagent_default_effort(section.as_ref(), pool.as_ref(), model_catalog)
}

fn assert_valid_subagent_default_effort(
    section: Option<&Value>,
    pool: Option<&SubagentModelPool>,
    model_catalog: &dyn IModelCatalog,
) -> Result<(), WireAsyncError> {
    let Some(effort) = section
        .and_then(|section| section.get("defaultEffort"))
        .and_then(Value::as_str)
        .map(normalize_requested_thinking_effort)
    else {
        return Ok(());
    };
    let Some(pool) = pool else {
        return Ok(());
    };
    for alias in pool.models.keys() {
        let model = model_catalog.get(alias)?;
        if effort == "off" && model.always_thinking == Some(true) {
            return Err(Box::new(Error2::with_options(
                CONFIG_INVALID,
                format!("[secondary_model].default_effort \"off\" cannot disable thinking for model \"{alias}\", which always reasons. Choose a concrete thinking effort instead of \"off\"."),
                Error2Options {
                    details: Some(serde_json::Map::from_iter([
                        ("section".to_string(), SECONDARY_MODEL_SECTION.into()),
                        ("field".to_string(), "defaultEffort".into()),
                        ("model".to_string(), alias.clone().into()),
                        ("effort".to_string(), effort.into()),
                    ])),
                    ..Default::default()
                },
            )));
        }
        if model_supports_thinking_effort(&effort, &model, true) {
            continue;
        }
        if !model_supports_thinking(&model) {
            return Err(Box::new(Error2::with_options(
                CONFIG_INVALID,
                format!("[secondary_model].default_effort \"{effort}\" is set but model \"{alias}\" does not support thinking."),
                Error2Options {
                    details: Some(serde_json::Map::from_iter([
                        ("section".to_string(), SECONDARY_MODEL_SECTION.into()),
                        ("field".to_string(), "defaultEffort".into()),
                        ("model".to_string(), alias.clone().into()),
                        ("effort".to_string(), effort.into()),
                    ])),
                    ..Default::default()
                },
            )));
        }
        return Err(Box::new(Error2::with_options(
            CONFIG_INVALID,
            format!("[secondary_model].default_effort \"{effort}\" is not supported by model \"{alias}\". Supported efforts: {}.", model.support_efforts.as_ref().map(|efforts| efforts.join(", ")).unwrap_or_default()),
            Error2Options {
                details: Some(serde_json::Map::from_iter([
                    ("section".to_string(), SECONDARY_MODEL_SECTION.into()),
                    ("field".to_string(), "defaultEffort".into()),
                    ("model".to_string(), alias.clone().into()),
                    ("effort".to_string(), effort.into()),
                ])),
                ..Default::default()
            },
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentModelSource {
    Inherited,
    SecondaryPool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentBinding {
    pub model: String,
    pub thinking: Option<String>,
    pub model_source: SubagentModelSource,
}

pub fn resolve_subagent_binding(
    config: &dyn IConfigService,
    own: (&str, &str),
    requested: Option<&str>,
) -> Result<SubagentBinding, WireAsyncError> {
    let pool = resolve_subagent_model_pool(config);
    let Some(pool) = pool else {
        if let Some(requested) = requested {
            return Err(Box::new(Error2::with_options(
                CONFIG_INVALID,
                format!("Invalid model \"{requested}\": no [secondary_model] pool is configured, so subagents inherit the caller's model (omit the model parameter)."),
                Error2Options {
                    details: Some(serde_json::Map::from_iter([("model".to_string(), requested.into())])),
                    ..Default::default()
                },
            )));
        }
        return Ok(SubagentBinding {
            model: own.0.to_string(),
            thinking: Some(own.1.to_string()),
            model_source: SubagentModelSource::Inherited,
        });
    };
    let choice = requested.map(str::to_string).or(pool.default_model.clone());
    let Some(choice) = choice else {
        return Err(Box::new(Error2::with_options(
            CONFIG_INVALID,
            SECONDARY_MODEL_DEFAULT_MODEL_REQUIRED_MESSAGE,
            Error2Options {
                details: Some(serde_json::Map::from_iter([
                    ("section".to_string(), SECONDARY_MODEL_SECTION.into()),
                    ("field".to_string(), "defaultModel".into()),
                ])),
                ..Default::default()
            },
        )));
    };
    if !pool.models.contains_key(&choice) {
        let available = pool.models.keys().cloned().collect::<Vec<_>>();
        return Err(Box::new(Error2::with_options(
            CONFIG_INVALID,
            format!("Invalid model \"{choice}\". Available models: {}.", available.join(", ")),
            Error2Options {
                details: Some(serde_json::Map::from_iter([
                    ("model".to_string(), choice.into()),
                    ("availableModels".to_string(), available.into()),
                ])),
                ..Default::default()
            },
        )));
    }
    Ok(SubagentBinding {
        model: choice,
        thinking: config
            .get(SECONDARY_MODEL_SECTION)
            .as_ref()
            .and_then(|section| section.get("defaultEffort"))
            .and_then(Value::as_str)
            .map(str::to_string),
        model_source: SubagentModelSource::SecondaryPool,
    })
}

pub fn resolve_subagent_thinking(
    config: &dyn IConfigService,
    model: Option<&Model>,
    explicit: Option<&str>,
) -> Option<String> {
    if let Some(explicit) = explicit {
        return Some(explicit.to_string());
    }
    if config
        .get(THINKING_SECTION)
        .as_ref()
        .and_then(|section| section.get("enabled"))
        .and_then(Value::as_bool)
        == Some(false)
    {
        return None;
    }
    declared_default_effort_for_model(model)
}

pub fn build_subagent_model_descriptions(config: &dyn IConfigService) -> Option<String> {
    if !exposes_subagent_model_choice(config) {
        return None;
    }
    let pool = resolve_subagent_model_pool(config).unwrap();
    Some(pool_description_lines(&pool).join("\n"))
}

pub fn build_subagent_model_summary(config: &dyn IConfigService) -> Option<String> {
    if !exposes_subagent_model_choice(config) {
        return None;
    }
    let pool = resolve_subagent_model_pool(config).unwrap();
    let labels = ordered_pool_aliases(&pool)
        .into_iter()
        .map(|alias| {
            if Some(&alias) == pool.default_model.as_ref() {
                format!("{alias} [default]")
            } else {
                alias
            }
        })
        .collect::<Vec<_>>();
    Some(format!("Available models (pass via model): {}.", labels.join(", ")))
}

fn pool_description_lines(pool: &SubagentModelPool) -> Vec<String> {
    let mut lines = vec!["Available models (pass via model):".to_string()];
    for alias in ordered_pool_aliases(pool) {
        let marker = if Some(&alias) == pool.default_model.as_ref() { " [default]" } else { "" };
        lines.push(format_pool_line(&format!("{alias}{marker}"), pool.models.get(&alias).unwrap()));
    }
    lines
}

fn ordered_pool_aliases(pool: &SubagentModelPool) -> Vec<String> {
    let aliases = pool.models.keys().cloned().collect::<Vec<_>>();
    let Some(default_model) = &pool.default_model else {
        return aliases;
    };
    if !pool.models.contains_key(default_model) {
        return aliases;
    }
    std::iter::once(default_model.clone())
        .chain(aliases.into_iter().filter(|alias| alias != default_model))
        .collect()
}

fn format_pool_line(label: &str, description: &str) -> String {
    if description.is_empty() {
        format!("- {label}")
    } else {
        format!("- {label}: {description}")
    }
}

pub fn strip_subagent_model_parameter(parameters: &Map<String, Value>) -> Map<String, Value> {
    let Some(properties) = parameters.get("properties").and_then(Value::as_object) else {
        return parameters.clone();
    };
    if !properties.contains_key("model") {
        return parameters.clone();
    }
    let mut next_properties = properties.clone();
    next_properties.remove("model");
    let mut next = parameters.clone();
    next.insert("properties".to_string(), Value::Object(next_properties));
    if let Some(required) = parameters.get("required").and_then(Value::as_array) {
        if required.iter().any(|entry| entry.as_str() == Some("model")) {
            next.insert(
                "required".to_string(),
                Value::Array(
                    required
                        .iter()
                        .filter(|entry| entry.as_str() != Some("model"))
                        .cloned()
                        .collect(),
                ),
            );
        }
    }
    next
}

pub fn strip_subagent_fork_parameter(parameters: &Map<String, Value>) -> Map<String, Value> {
    let Some(properties) = parameters.get("properties").and_then(Value::as_object) else {
        return parameters.clone();
    };
    if !properties.contains_key("fork") {
        return parameters.clone();
    }
    let mut next_properties = properties.clone();
    next_properties.remove("fork");
    let mut next = parameters.clone();
    next.insert("properties".to_string(), Value::Object(next_properties));
    if let Some(required) = parameters.get("required").and_then(Value::as_array) {
        if required.iter().any(|entry| entry.as_str() == Some("fork")) {
            next.insert(
                "required".to_string(),
                Value::Array(
                    required
                        .iter()
                        .filter(|entry| entry.as_str() != Some("fork"))
                        .cloned()
                        .collect(),
                ),
            );
        }
    }
    next
}

pub fn wrap_subagent_model_error(
    error: WireAsyncError,
    bound_model: &str,
    caller_model_alias: Option<&str>,
) -> WireAsyncError {
    if Some(bound_model) == caller_model_alias {
        return error;
    }
    let Some(coded) = error.downcast_ref::<Error2>() else {
        return error;
    };
    if coded.code().as_str() != CONFIG_INVALID || coded.details().and_then(|details| details.get("model")).and_then(Value::as_str) != Some(bound_model) {
        return error;
    }
    let mut details = coded.details().cloned().unwrap_or_default();
    details.insert("subagentModel".to_string(), bound_model.into());
    details.insert(
        "subagentModelConfig".to_string(),
        json!({"section": "secondary_model.models"}),
    );
    Box::new(Error2::with_options(
        coded.code().clone(),
        format!("{} (subagent model \"{bound_model}\" comes from [secondary_model.models] — check that it names a valid [models] entry)", coded.to_string()),
        Error2Options {
            cause: Some(error),
            name: Some(coded.name().to_string()),
            details: Some(details),
            ..Default::default()
        },
    ))
}

pub fn format_subagent_timeout_description(ms: u64) -> String {
    if ms % (60 * 60 * 1000) == 0 {
        let hours = ms / (60 * 60 * 1000);
        return format!("{hours} hour{}", if hours == 1 { "" } else { "s" });
    }
    if ms % (60 * 1000) == 0 {
        let minutes = ms / (60 * 1000);
        return format!("{minutes} minute{}", if minutes == 1 { "" } else { "s" });
    }
    if ms % 1000 == 0 {
        let seconds = ms / 1000;
        return format!("{seconds} second{}", if seconds == 1 { "" } else { "s" });
    }
    format!("{ms} ms")
}

#[allow(dead_code)]
fn _error_marker(_: &Error2) {}
