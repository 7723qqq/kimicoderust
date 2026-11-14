use kimi_core::primitives::errors::Error2;
use serde_json::{Map, Number, Value, json};

use crate::llm_adapter::contract::errors::CONFIG_INVALID_ERROR_CODE;

use super::config_section::ENV_MODEL_PROVIDER_KEY;

pub const ENV_MODEL_ALIAS_KEY: &str = "__kimi_env_model__";
const DEFAULT_MAX_CONTEXT_SIZE: u64 = 262144;
const DEFAULT_CAPABILITIES: [&str; 2] = ["image_in", "thinking"];

#[derive(Debug, Clone, Copy, Default)]
pub struct KimiModelEnvOverlay;

fn trimmed(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

fn fail<T>(message: impl Into<String>) -> Result<T, Error2> {
    Err(Error2::new(CONFIG_INVALID_ERROR_CODE, message))
}

fn parse_positive_int(raw: &str, var_name: &str) -> Result<u64, Error2> {
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return fail(format!("{var_name} must be a positive integer, got \"{raw}\"."));
    }
    let parsed = raw.parse::<u64>().ok().filter(|parsed| *parsed > 0);
    match parsed {
        Some(parsed) => Ok(parsed),
        None => fail(format!("{var_name} must be a positive integer, got \"{raw}\".")),
    }
}

fn parse_float_env(raw: Option<String>, var_name: &str) -> Result<Option<f64>, Error2> {
    let Some(value) = trimmed(raw.clone()) else {
        return Ok(None);
    };
    match value.parse::<f64>().ok().filter(|value| value.is_finite()) {
        Some(parsed) => Ok(Some(parsed)),
        None => fail(format!("{var_name} must be a number, got \"{}\".", raw.unwrap_or_default())),
    }
}

fn parse_completion_tokens(raw: Option<String>) -> Option<i64> {
    let value = trimmed(raw)?;
    let parsed = value.parse::<f64>().ok()?;
    if !parsed.is_finite() || parsed.fract() != 0.0 {
        return None;
    }
    if parsed < i64::MIN as f64 || parsed > i64::MAX as f64 {
        return None;
    }
    Some(parsed as i64)
}

fn parse_capabilities(raw: Option<String>) -> Option<Vec<String>> {
    let capabilities: Vec<String> = raw?
        .split(',')
        .map(|capability| capability.trim().to_lowercase())
        .filter(|capability| !capability.is_empty())
        .collect();
    (!capabilities.is_empty()).then_some(capabilities)
}

fn parse_boolean_env(value: &str) -> Option<bool> {
    match value.trim().to_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn parse_boolean_var(raw: Option<String>, var_name: &str) -> Result<Option<bool>, Error2> {
    let Some(value) = trimmed(raw.clone()) else {
        return Ok(None);
    };
    match parse_boolean_env(&value) {
        Some(parsed) => Ok(Some(parsed)),
        None => fail(format!(
            "{var_name} must be a boolean (true/false/1/0/yes/no/on/off), got \"{}\".",
            raw.unwrap_or_default()
        )),
    }
}

fn as_record(value: Option<&Value>) -> Map<String, Value> {
    value.and_then(Value::as_object).cloned().unwrap_or_default()
}

fn without_key(value: Value, key: &str) -> Value {
    let Some(mut object) = value.as_object().cloned() else {
        return value;
    };
    if !object.contains_key(key) {
        return value;
    }
    object.remove(key);
    Value::Object(object)
}

fn number(value: f64) -> Value {
    Number::from_f64(value).map(Value::Number).unwrap_or(Value::Null)
}

#[derive(Default)]
struct ModelOverrideInput {
    temperature: Option<f64>,
    top_p: Option<f64>,
    thinking_keep: Option<String>,
    max_completion_tokens: Option<i64>,
}

fn collect_model_overrides(input: ModelOverrideInput) -> Option<Value> {
    let mut output = Map::new();
    if let Some(temperature) = input.temperature {
        output.insert("temperature".to_string(), number(temperature));
    }
    if let Some(top_p) = input.top_p {
        output.insert("topP".to_string(), number(top_p));
    }
    if let Some(thinking_keep) = input.thinking_keep {
        output.insert("thinkingKeep".to_string(), Value::String(thinking_keep));
    }
    if let Some(max_completion_tokens) = input.max_completion_tokens {
        output.insert("maxCompletionTokens".to_string(), Value::from(max_completion_tokens));
    }
    (!output.is_empty()).then_some(Value::Object(output))
}

fn first_env(get_env: &impl Fn(&str) -> Option<String>, names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| get_env(name).filter(|value| !value.is_empty()))
}

fn resolve_provider_base_url(
    provider_type: &str,
    get_env: &impl Fn(&str) -> Option<String>,
) -> Option<String> {
    match provider_type {
        "anthropic" => first_env(get_env, &["ANTHROPIC_BASE_URL"]),
        "openai" | "openai_responses" => first_env(get_env, &["OPENAI_BASE_URL"]),
        "vertexai" => first_env(get_env, &["GOOGLE_VERTEX_BASE_URL"]),
        "google-genai" => first_env(
            get_env,
            &["GOOGLE_VERTEX_BASE_URL", "GOOGLE_GEMINI_BASE_URL"],
        ),
        "kimi" => Some(
            first_env(get_env, &["KIMI_BASE_URL"])
                .unwrap_or_else(|| "https://api.moonshot.ai/v1".to_string()),
        ),
        "deepseek" => Some(
            first_env(get_env, &["DEEPSEEK_BASE_URL"])
                .unwrap_or_else(|| "https://api.deepseek.com".to_string()),
        ),
        _ => None,
    }
}

impl KimiModelEnvOverlay {
    pub fn apply(
        &self,
        effective: &mut Map<String, Value>,
        get_env: impl Fn(&str) -> Option<String>,
        validate: impl Fn(&str, Value) -> Result<Value, Error2>,
    ) -> Result<Vec<String>, Error2> {
        let model = trimmed(get_env("KIMI_MODEL_NAME"));
        let temperature = parse_float_env(get_env("KIMI_MODEL_TEMPERATURE"), "KIMI_MODEL_TEMPERATURE")?;
        let top_p = parse_float_env(get_env("KIMI_MODEL_TOP_P"), "KIMI_MODEL_TOP_P")?;
        let thinking_keep = trimmed(get_env("KIMI_MODEL_THINKING_KEEP"));
        let max_completion_tokens = parse_completion_tokens(get_env("KIMI_MODEL_MAX_COMPLETION_TOKENS"))
            .or_else(|| parse_completion_tokens(get_env("KIMI_MODEL_MAX_TOKENS")));
        let overrides = || {
            collect_model_overrides(ModelOverrideInput {
                temperature,
                top_p,
                thinking_keep: thinking_keep.clone(),
                max_completion_tokens,
            })
        };
        let mut changed = Vec::new();
        let Some(model) = model else {
            if let Some(model_overrides) = overrides() {
                effective.insert("modelOverrides".to_string(), model_overrides);
                changed.push("modelOverrides".to_string());
            }
            return Ok(changed);
        };

        let max_context_raw = trimmed(get_env("KIMI_MODEL_MAX_CONTEXT_SIZE"));
        let max_context_size = match max_context_raw {
            Some(raw) => parse_positive_int(&raw, "KIMI_MODEL_MAX_CONTEXT_SIZE")?,
            None => DEFAULT_MAX_CONTEXT_SIZE,
        };
        let max_output_raw = trimmed(get_env("KIMI_MODEL_MAX_OUTPUT_SIZE"));
        let max_output_size = match max_output_raw {
            Some(raw) => Some(parse_positive_int(&raw, "KIMI_MODEL_MAX_OUTPUT_SIZE")?),
            None => None,
        };
        let capabilities = parse_capabilities(get_env("KIMI_MODEL_CAPABILITIES"))
            .unwrap_or_else(|| DEFAULT_CAPABILITIES.iter().map(|value| value.to_string()).collect());
        let display_name = trimmed(get_env("KIMI_MODEL_DISPLAY_NAME"));
        let reasoning_key = trimmed(get_env("KIMI_MODEL_REASONING_KEY"));
        let adaptive_thinking = parse_boolean_var(
            get_env("KIMI_MODEL_ADAPTIVE_THINKING"),
            "KIMI_MODEL_ADAPTIVE_THINKING",
        )?;

        let mut alias = Map::from_iter([
            ("provider".to_string(), Value::String(ENV_MODEL_PROVIDER_KEY.to_string())),
            ("model".to_string(), Value::String(model)),
            ("maxContextSize".to_string(), Value::from(max_context_size)),
            ("capabilities".to_string(), json!(capabilities)),
        ]);
        if let Some(display_name) = display_name {
            alias.insert("displayName".to_string(), Value::String(display_name));
        }
        if let Some(max_output_size) = max_output_size {
            alias.insert("maxOutputSize".to_string(), Value::from(max_output_size));
        }
        if let Some(reasoning_key) = reasoning_key {
            alias.insert("reasoningKey".to_string(), Value::String(reasoning_key));
        }
        if let Some(adaptive_thinking) = adaptive_thinking {
            alias.insert("adaptiveThinking".to_string(), Value::Bool(adaptive_thinking));
        }

        let mut models = as_record(effective.get("models"));
        models.insert(ENV_MODEL_ALIAS_KEY.to_string(), Value::Object(alias));
        let next_models = validate("models", Value::Object(models))?;
        effective.insert("models".to_string(), next_models);
        changed.push("models".to_string());

        let providers = as_record(effective.get("providers"));
        let env_provider = as_record(providers.get(ENV_MODEL_PROVIDER_KEY));
        let provider_type = env_provider
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("kimi");
        let provider_base_url = match env_provider.get("baseUrl").and_then(Value::as_str) {
            Some(base_url) if !base_url.is_empty() => Some(base_url.to_string()),
            _ => resolve_provider_base_url(provider_type, &get_env),
        };
        let mut provider_patch = Map::new();
        if !env_provider.contains_key("type") {
            provider_patch.insert("type".to_string(), Value::String("kimi".to_string()));
        }
        if let Some(provider_base_url) = provider_base_url
            && !env_provider.contains_key("baseUrl")
        {
            provider_patch.insert("baseUrl".to_string(), Value::String(provider_base_url));
        }
        if !provider_patch.is_empty() {
            let mut next_providers = providers;
            let mut next_env_provider = env_provider;
            next_env_provider.extend(provider_patch);
            next_providers.insert(
                ENV_MODEL_PROVIDER_KEY.to_string(),
                Value::Object(next_env_provider),
            );
            let next_providers = validate("providers", Value::Object(next_providers))?;
            effective.insert("providers".to_string(), next_providers);
            changed.push("providers".to_string());
        }

        effective.insert(
            "defaultModel".to_string(),
            Value::String(ENV_MODEL_ALIAS_KEY.to_string()),
        );
        changed.push("defaultModel".to_string());
        if let Some(model_overrides) = overrides() {
            effective.insert("modelOverrides".to_string(), model_overrides);
            changed.push("modelOverrides".to_string());
        }
        Ok(changed)
    }

    pub fn strip(&self, domain: &str, value: Value, raw_snake: &Map<String, Value>) -> Option<Value> {
        match domain {
            "models" => Some(without_key(value, ENV_MODEL_ALIAS_KEY)),
            "defaultModel" => {
                if value.as_str() != Some(ENV_MODEL_ALIAS_KEY) {
                    return Some(value);
                }
                raw_snake
                    .get("default_model")
                    .and_then(Value::as_str)
                    .map(|value| Value::String(value.to_string()))
            }
            "modelOverrides" => None,
            _ => Some(value),
        }
    }
}

pub const KIMI_MODEL_ENV_OVERLAY: KimiModelEnvOverlay = KimiModelEnvOverlay;

pub fn register_kimi_model_env_overlay(register: impl FnOnce(KimiModelEnvOverlay)) {
    register(KIMI_MODEL_ENV_OVERLAY);
}
