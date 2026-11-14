use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::human::llm::capability::is_unknown_capability;
use crate::human::llm::errors::{LlmErrorKind, LlmErrorMessage, LlmSyntaxErrorCode};
use crate::human::llm::model::LlmModel;
use crate::human::llm::syntax_errors::SyntaxRequestFormatError;
use crate::wire::record::WireAsyncError;

pub type ThinkingEffort = String;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThinkingRequestOptions {
    pub effort: ThinkingEffort,
    pub keep: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelThinkingMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub support_efforts: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub off_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub always_thinking: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_thinking: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThinkingDefaults {
    pub enabled: Option<bool>,
    pub effort: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThinkingConfigErrorCode {
    EffortNotSupported,
    ThinkingUnsupported,
    ThinkingCannotDisable,
    OffNeedsOffeffort,
    BudgetExceedsCap,
}

#[derive(Debug)]
pub struct ThinkingConfigError {
    pub code: ThinkingConfigErrorCode,
    message: String,
}

impl ThinkingConfigError {
    pub fn new(code: ThinkingConfigErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn to_llm_error_message(&self) -> LlmErrorMessage {
        LlmErrorMessage {
            kind: LlmErrorKind::Syntax,
            code: Some(LlmSyntaxErrorCode::ThinkingConfig),
            message: self.message.clone(),
            status_code: None,
            request_id: None,
            retry_after_ms: None,
            headers: None,
            retryable: None,
            finish_reason: None,
            raw_finish_reason: None,
        }
    }
}

impl fmt::Display for ThinkingConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ThinkingConfigError {}

#[derive(Debug)]
pub enum ThinkingResolution {
    Silent,
    Effort(String),
    Error(ThinkingConfigError),
}

pub fn thinking_metadata_of(model: &LlmModel) -> Option<ModelThinkingMetadata> {
    let metadata = ModelThinkingMetadata {
        support_efforts: model.support_efforts.clone(),
        default_effort: model.default_effort.clone(),
        off_effort: model.off_effort.clone(),
        always_thinking: model.always_thinking,
        adaptive_thinking: model.adaptive_thinking,
    };
    if metadata.support_efforts.is_none()
        && metadata.default_effort.is_none()
        && metadata.off_effort.is_none()
        && metadata.always_thinking.is_none()
        && metadata.adaptive_thinking.is_none()
    {
        return None;
    }
    Some(metadata)
}

fn capability_thinking(model: &LlmModel) -> Option<bool> {
    if is_unknown_capability(&model.capability) {
        None
    } else {
        Some(model.capability.thinking)
    }
}

fn effort_list(efforts: Option<&[String]>) -> Option<String> {
    efforts.filter(|efforts| !efforts.is_empty()).map(|efforts| efforts.join(", "))
}

pub fn resolve_thinking_effort(
    options: &ThinkingRequestOptions,
    model: &LlmModel,
    strict_validation: bool,
) -> ThinkingResolution {
    let effort = &options.effort;
    let meta = thinking_metadata_of(model);
    if effort == "on" {
        return ThinkingResolution::Silent;
    }
    if effort == "off" {
        if let Some(off_effort) = meta.as_ref().and_then(|meta| meta.off_effort.clone()) {
            return ThinkingResolution::Effort(off_effort);
        }
        if meta.as_ref().and_then(|meta| meta.always_thinking) == Some(true) {
            let list = effort_list(meta.as_ref().and_then(|meta| meta.support_efforts.as_deref()));
            return ThinkingResolution::Error(ThinkingConfigError::new(
                ThinkingConfigErrorCode::ThinkingCannotDisable,
                match list {
                    Some(list) => format!(
                        "Model '{}' always reasons and thinking cannot be turned off. Choose a concrete thinking effort ({list}) instead of 'off'.",
                        model.model
                    ),
                    None => format!(
                        "Model '{}' always reasons and thinking cannot be turned off. Choose a concrete thinking effort instead of 'off'.",
                        model.model
                    ),
                },
            ));
        }
        if meta.as_ref().and_then(|meta| meta.support_efforts.as_ref()).is_some() {
            return ThinkingResolution::Error(ThinkingConfigError::new(
                ThinkingConfigErrorCode::OffNeedsOffeffort,
                format!(
                    "Model '{}' reasons by default but declares no off effort, so thinking cannot be turned off. Declare offEffort (for example 'none') for this model in the model catalog configuration.",
                    model.model
                ),
            ));
        }
        return ThinkingResolution::Silent;
    }
    if strict_validation
        && let Some(supported) = meta.as_ref().and_then(|meta| meta.support_efforts.as_ref())
        && !supported.contains(effort)
    {
        return ThinkingResolution::Error(ThinkingConfigError::new(
            ThinkingConfigErrorCode::EffortNotSupported,
            format!(
                "Model '{}' does not support thinking effort '{effort}'. Supported efforts: {}. Set the thinking effort to one of the supported values.",
                model.model,
                supported.join(", ")
            ),
        ));
    }
    if meta.is_none() && capability_thinking(model) == Some(false) {
        return ThinkingResolution::Error(ThinkingConfigError::new(
            ThinkingConfigErrorCode::ThinkingUnsupported,
            format!(
                "Model '{}' does not support thinking, but thinking effort '{effort}' was requested. Remove the thinking effort setting or choose a thinking-capable model.",
                model.model
            ),
        ));
    }
    ThinkingResolution::Effort(effort.clone())
}

pub fn encode_reasoning_effort_fallback(
    thinking: &ThinkingRequestOptions,
    model: &LlmModel,
    strict_validation: bool,
) -> Result<Option<Map<String, Value>>, WireAsyncError> {
    match resolve_thinking_effort(thinking, model, strict_validation) {
        ThinkingResolution::Silent => Ok(None),
        ThinkingResolution::Effort(value) => Ok(Some(Map::from_iter([(
            "reasoning_effort".to_string(),
            Value::String(value),
        )]))),
        ThinkingResolution::Error(error) => Err(Box::new(error)),
    }
}

fn js_trim(value: &str) -> &str {
    value.trim_matches(|character| {
        matches!(
            character,
            '\u{0009}'..='\u{000d}'
                | ' '
                | '\u{00a0}'
                | '\u{1680}'
                | '\u{2000}'..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
        )
    })
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(js_trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn middle_of(values: &[String]) -> String {
    values[values.len() / 2].clone()
}

fn efforts_for(meta: Option<&ModelThinkingMetadata>) -> Vec<String> {
    meta.and_then(|meta| meta.support_efforts.as_ref())
        .map(|efforts| {
            efforts
                .iter()
                .filter_map(|effort| non_empty(Some(effort)))
                .collect()
        })
        .unwrap_or_default()
}

pub fn normalize_requested_thinking_effort(requested: Option<&str>) -> Option<ThinkingEffort> {
    non_empty(requested).map(|effort| effort.to_lowercase())
}

pub fn model_supports_thinking(model: &LlmModel) -> bool {
    let meta = thinking_metadata_of(model);
    meta.as_ref().and_then(|meta| meta.always_thinking) == Some(true)
        || meta.as_ref().and_then(|meta| meta.adaptive_thinking) == Some(true)
        || capability_thinking(model) == Some(true)
}

pub fn default_thinking_effort_for_model(model: &LlmModel) -> ThinkingEffort {
    let meta = thinking_metadata_of(model);
    if !model_supports_thinking(model) {
        return "off".to_string();
    }
    let efforts = efforts_for(meta.as_ref());
    if !efforts.is_empty() {
        let declared = non_empty(meta.as_ref().and_then(|meta| meta.default_effort.as_deref()));
        return declared
            .filter(|declared| efforts.contains(declared))
            .unwrap_or_else(|| middle_of(&efforts));
    }
    "on".to_string()
}

fn normalize_thinking_effort_for_model(
    effort: ThinkingEffort,
    model: &LlmModel,
    strict_validation: bool,
) -> ThinkingEffort {
    let meta = thinking_metadata_of(model);
    if effort == "off" && meta.as_ref().and_then(|meta| meta.always_thinking) != Some(true) {
        return "off".to_string();
    }
    let efforts = efforts_for(meta.as_ref());
    if !strict_validation {
        return if effort == "on" && !efforts.is_empty() {
            default_thinking_effort_for_model(model)
        } else {
            effort
        };
    }
    if !model_supports_thinking(model) {
        return "off".to_string();
    }
    if efforts.is_empty() {
        return "on".to_string();
    }
    if effort == "on" || !efforts.contains(&effort) {
        return default_thinking_effort_for_model(model);
    }
    effort
}

pub fn resolve_thinking_effort_for_model(
    requested: Option<&str>,
    defaults: Option<&ThinkingDefaults>,
    model: &LlmModel,
    strict_validation: bool,
) -> ThinkingEffort {
    let configured = normalize_requested_thinking_effort(defaults.and_then(|defaults| defaults.effort.as_deref()));
    let normalized = normalize_requested_thinking_effort(requested);
    let mut effort = if let Some(normalized) = normalized {
        normalized
    } else if defaults.and_then(|defaults| defaults.enabled) == Some(false) {
        "off".to_string()
    } else {
        configured
            .clone()
            .unwrap_or_else(|| default_thinking_effort_for_model(model))
    };
    if effort == "off" && thinking_metadata_of(model).and_then(|meta| meta.always_thinking) == Some(true) {
        effort = configured
            .filter(|configured| configured != "off")
            .unwrap_or_else(|| default_thinking_effort_for_model(model));
    }
    normalize_thinking_effort_for_model(effort, model, strict_validation)
}

fn parse_keep_value(raw: Option<&str>) -> (bool, Option<String>) {
    let Some(trimmed) = raw.map(js_trim).filter(|value| !value.is_empty()) else {
        return (false, None);
    };
    if ["0", "false", "no", "off", "none", "null"]
        .contains(&trimmed.to_lowercase().as_str())
    {
        return (true, None);
    }
    (true, Some(trimmed.to_string()))
}

pub fn resolve_thinking_keep(
    env_keep: Option<&str>,
    config_keep: Option<&str>,
    thinking_effort: &str,
) -> Option<String> {
    if thinking_effort == "off" {
        return None;
    }
    let from_env = parse_keep_value(env_keep);
    if from_env.0 {
        return from_env.1;
    }
    let from_config = parse_keep_value(config_keep);
    if from_config.0 {
        return from_config.1;
    }
    Some("all".to_string())
}

impl From<ThinkingConfigError> for SyntaxRequestFormatError {
    fn from(value: ThinkingConfigError) -> Self {
        SyntaxRequestFormatError::with_code(LlmSyntaxErrorCode::ThinkingConfig, value.message)
    }
}
