use serde::{Deserialize, Serialize};

use crate::llm_adapter::model::catalog::Model;
use crate::llm_adapter::model::model_types::{
    ModelThinkingCapabilities, ModelThinkingMetadata, ThinkingDefaults,
};
use crate::llm_adapter::protocol::protocol::{IProtocolAdapterRegistry, Protocol};
use crate::llm_adapter::provider::provider_definition::get_provider_definitions;

pub use crate::human::llm::thinking::resolve_thinking_keep;

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

pub fn drives_thinking_through_traits(provider_type: Option<&str>) -> bool {
    let Some(provider_type) = provider_type else {
        return false;
    };
    get_provider_definitions(provider_type)
        .iter()
        .any(|definition| {
            definition
                .trait_
                .as_ref()
                .is_some_and(|trait_| trait_.thinking.is_some())
        })
}

pub fn uses_trait_driven_thinking(
    registry: &dyn IProtocolAdapterRegistry,
    protocol: Protocol,
    provider_type: Option<&str>,
) -> bool {
    registry
        .resolve_adapter_identity(protocol, provider_type)
        .trait_
        .is_some_and(|trait_| trait_.thinking.is_some())
}

pub fn requires_strict_thinking_validation(
    registry: &dyn IProtocolAdapterRegistry,
    protocol: Protocol,
    provider_type: Option<&str>,
) -> bool {
    let Some(provider_type) = provider_type else {
        return false;
    };
    let identity = registry.resolve_adapter_identity(protocol, Some(provider_type));
    identity
        .trait_
        .is_some_and(|trait_| {
            trait_.thinking.is_some() && trait_.strict_thinking_validation == Some(true)
        })
}

pub fn wire_has_protocol_thinking_disable(protocol: Option<&str>) -> bool {
    matches!(protocol, Some("anthropic" | "kimi"))
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
    let trimmed = js_trim(value?);
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

pub fn normalize_requested_thinking_effort(requested: Option<&str>) -> Option<String> {
    normalize_optional_effort(requested)
}

fn normalize_optional_effort(requested: Option<&str>) -> Option<String> {
    non_empty(requested).map(|effort| effort.to_lowercase())
}

pub fn resolve_forced_thinking_effort(
    forced: Option<&str>,
    effective: &str,
    trait_driven: bool,
) -> Option<String> {
    if !trait_driven || effective == "off" {
        return None;
    }
    normalize_optional_effort(forced)
}

pub trait ThinkingModel {
    fn supports_capability(&self, capability: &str) -> bool;
    fn adaptive_thinking(&self) -> Option<bool>;
    fn always_thinking(&self) -> Option<bool>;
    fn support_efforts(&self) -> Option<&[String]>;
    fn default_effort(&self) -> Option<&str>;
}

impl ThinkingModel for Model {
    fn supports_capability(&self, capability: &str) -> bool {
        match capability {
            "thinking" => self.capabilities.thinking,
            "always_thinking" => false,
            _ => false,
        }
    }

    fn adaptive_thinking(&self) -> Option<bool> {
        self.adaptive_thinking
    }

    fn always_thinking(&self) -> Option<bool> {
        self.always_thinking
    }

    fn support_efforts(&self) -> Option<&[String]> {
        self.support_efforts.as_deref()
    }

    fn default_effort(&self) -> Option<&str> {
        self.default_effort.as_deref()
    }
}

impl ThinkingModel for ModelThinkingMetadata {
    fn supports_capability(&self, capability: &str) -> bool {
        match self.capabilities.as_ref() {
            Some(ModelThinkingCapabilities::Declared(capabilities)) => capabilities
                .iter()
                .any(|candidate| js_trim(candidate).to_lowercase() == capability),
            Some(ModelThinkingCapabilities::Resolved(capabilities)) => match capability {
                "thinking" => capabilities.thinking,
                _ => false,
            },
            None => false,
        }
    }

    fn adaptive_thinking(&self) -> Option<bool> {
        self.adaptive_thinking
    }

    fn always_thinking(&self) -> Option<bool> {
        self.always_thinking
    }

    fn support_efforts(&self) -> Option<&[String]> {
        self.support_efforts.as_deref()
    }

    fn default_effort(&self) -> Option<&str> {
        self.default_effort.as_deref()
    }
}

fn efforts_for<M: ThinkingModel + ?Sized>(model: Option<&M>) -> Vec<String> {
    model
        .and_then(|model| model.support_efforts())
        .unwrap_or_default()
        .iter()
        .filter_map(|effort| non_empty(Some(effort)))
        .collect()
}

pub fn model_supports_thinking<M: ThinkingModel + ?Sized>(model: &M) -> bool {
    model.always_thinking() == Some(true)
        || model.adaptive_thinking() == Some(true)
        || model.supports_capability("thinking")
        || model.supports_capability("always_thinking")
}

pub fn default_thinking_effort_for_model<M: ThinkingModel + ?Sized>(
    model: Option<&M>,
) -> String {
    if !model.is_some_and(model_supports_thinking) {
        return "off".to_string();
    }
    let efforts = efforts_for(model);
    if !efforts.is_empty() {
        let declared_default = model.and_then(|model| non_empty(model.default_effort()));
        return declared_default
            .filter(|default| efforts.contains(default))
            .unwrap_or_else(|| efforts[efforts.len() / 2].clone());
    }
    "on".to_string()
}

pub fn declared_default_effort_for_model<M: ThinkingModel + ?Sized>(
    model: Option<&M>,
) -> Option<String> {
    if !model.is_some_and(model_supports_thinking) {
        return None;
    }
    let declared = model.and_then(|model| non_empty(model.default_effort()))?;
    efforts_for(model).contains(&declared).then_some(declared)
}

pub fn model_supports_thinking_effort<M: ThinkingModel + ?Sized>(
    effort: &str,
    model: &M,
    strict_validation: bool,
) -> bool {
    if !strict_validation || effort == "off" {
        return true;
    }
    if !model_supports_thinking(model) {
        return false;
    }
    let efforts = efforts_for(Some(model));
    efforts.is_empty() || effort == "on" || efforts.iter().any(|candidate| candidate == effort)
}

fn normalize_thinking_effort_for_model<M: ThinkingModel + ?Sized>(
    effort: String,
    model: Option<&M>,
    strict_validation: bool,
) -> String {
    if effort == "off" && model.and_then(|model| model.always_thinking()) != Some(true) {
        return "off".to_string();
    }
    let efforts = efforts_for(model);
    if !strict_validation {
        return if effort == "on" && !efforts.is_empty() {
            default_thinking_effort_for_model(model)
        } else {
            effort
        };
    }
    if !model.is_some_and(model_supports_thinking) {
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

pub fn resolve_thinking_effort_for_model<M: ThinkingModel + ?Sized>(
    requested: Option<&str>,
    defaults: Option<&ThinkingDefaults>,
    model: Option<&M>,
    strict_validation: bool,
) -> String {
    let configured = normalize_optional_effort(defaults.and_then(|defaults| defaults.effort.as_deref()));
    let normalized = normalize_optional_effort(requested);
    let mut effort = if let Some(normalized) = normalized {
        normalized
    } else if defaults.and_then(|defaults| defaults.enabled) == Some(false) {
        "off".to_string()
    } else {
        configured
            .clone()
            .unwrap_or_else(|| default_thinking_effort_for_model(model))
    };
    if effort == "off" && model.and_then(|model| model.always_thinking()) == Some(true) {
        effort = configured
            .filter(|configured| configured != "off")
            .unwrap_or_else(|| default_thinking_effort_for_model(model));
    }
    normalize_thinking_effort_for_model(effort, model, strict_validation)
}
