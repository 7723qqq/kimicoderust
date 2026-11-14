use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::llm_adapter::contract::capability::ModelCapability;

use super::config_section::ProviderType;

pub const KNOWN_WIRE_TYPES: [&str; 6] = [
    "anthropic",
    "openai",
    "kimi",
    "google-genai",
    "openai_responses",
    "antigravity",
];

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelsDevModelEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<ModelsDevModelLimit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_options: Option<Vec<ModelsDevReasoningOption>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ModelsDevModelProviderOverride>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dynamically_loaded_tools: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interleaved: Option<ModelsDevInterleaved>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modalities: Option<ModelsDevModalities>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelsDevModelLimit {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelsDevReasoningOption {
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub type_: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub values: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelsDevModelProviderOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub npm: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ModelsDevInterleaved {
    Enabled(bool),
    Options(ModelsDevInterleavedOptions),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelsDevInterleavedOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelsDevModalities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelsDevProviderEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub npm: Option<String>,
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub type_: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<IndexMap<String, ModelsDevModelEntry>>,
}

pub type ModelsDevCatalog = IndexMap<String, ModelsDevProviderEntry>;

#[derive(Debug, Clone, PartialEq)]
pub struct ModelsDevModel {
    pub id: String,
    pub name: Option<String>,
    pub max_output_size: Option<u32>,
    pub reasoning_key: Option<String>,
    pub support_efforts: Option<Vec<String>>,
    pub off_effort: Option<String>,
    pub always_thinking: Option<bool>,
    pub protocol: Option<String>,
    pub base_url: Option<String>,
    pub capability: ModelCapability,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelsDevImportInvalidReason {
    UnknownExplicitType,
    ProprietarySdk,
    EmptyBaseUrl,
    PlaceholderBaseUrl,
}

impl ModelsDevImportInvalidReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UnknownExplicitType => "unknown-explicit-type",
            Self::ProprietarySdk => "proprietary-sdk",
            Self::EmptyBaseUrl => "empty-base-url",
            Self::PlaceholderBaseUrl => "placeholder-base-url",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelsDevImportResolution {
    Ok {
        wire: ProviderType,
        guessed: bool,
        base_url: Option<String>,
    },
    NeedsBaseUrl {
        wire: ProviderType,
        guessed: bool,
    },
    Invalid {
        reason: ModelsDevImportInvalidReason,
    },
}

fn is_wire_type(value: &str) -> bool {
    KNOWN_WIRE_TYPES.contains(&value)
}

fn has_embedding_marker(value: Option<&str>) -> bool {
    let Some(value) = value else {
        return false;
    };
    let lower = value.to_lowercase();
    lower.contains("embedding")
        || lower
            .split(['-', '_', '/'])
            .any(|segment| segment == "embed")
}

fn is_usable_chat_model(model: &ModelsDevModelEntry) -> bool {
    if model
        .modalities
        .as_ref()
        .and_then(|modalities| modalities.output.as_ref())
        .is_some_and(|outputs| !outputs.iter().any(|output| output == "text"))
    {
        return false;
    }
    if matches!(model.status.as_deref(), Some("deprecated" | "alpha")) {
        return false;
    }
    !has_embedding_marker(model.family.as_deref())
        && !has_embedding_marker(model.id.as_deref())
        && !has_embedding_marker(model.name.as_deref())
}

pub fn resolve_models_dev_import(
    entry: &ModelsDevProviderEntry,
    user_base_url: Option<&str>,
) -> ModelsDevImportResolution {
    let Some(wire) = resolve_models_dev_wire(entry) else {
        return ModelsDevImportResolution::Invalid {
            reason: if entry.type_.as_deref().is_some_and(|value| !value.is_empty()) {
                ModelsDevImportInvalidReason::UnknownExplicitType
            } else {
                ModelsDevImportInvalidReason::ProprietarySdk
            },
        };
    };
    let guessed = infer_declared_wire_type(entry).is_none();
    if let Some(user_base_url) = user_base_url {
        let trimmed = user_base_url.trim();
        if trimmed.is_empty() {
            return ModelsDevImportResolution::Invalid {
                reason: ModelsDevImportInvalidReason::EmptyBaseUrl,
            };
        }
        if trimmed.contains("${") {
            return ModelsDevImportResolution::Invalid {
                reason: ModelsDevImportInvalidReason::PlaceholderBaseUrl,
            };
        }
        return ModelsDevImportResolution::Ok {
            base_url: Some(adapt_base_url_for_wire(trimmed, &wire)),
            wire,
            guessed,
        };
    }
    if let Some(base_url) = models_dev_base_url(entry, &wire) {
        return ModelsDevImportResolution::Ok {
            base_url: Some(base_url),
            wire,
            guessed,
        };
    }
    if models_dev_endpoint_required(entry, &wire) {
        return ModelsDevImportResolution::NeedsBaseUrl { wire, guessed };
    }
    ModelsDevImportResolution::Ok {
        wire,
        guessed,
        base_url: None,
    }
}

fn resolve_models_dev_wire(entry: &ModelsDevProviderEntry) -> Option<ProviderType> {
    if let Some(explicit) = entry.type_.as_deref() {
        if is_wire_type(explicit) {
            return Some(explicit.to_string());
        }
        if !explicit.is_empty() {
            return None;
        }
    }
    if let Some(declared) = infer_declared_wire_type(entry) {
        return Some(declared.to_string());
    }
    let npm = entry.npm.as_deref().unwrap_or_default().to_lowercase();
    if npm.contains("amazon-bedrock") || npm.contains("cohere") {
        return None;
    }
    Some("openai".to_string())
}

fn infer_declared_wire_type(entry: &ModelsDevProviderEntry) -> Option<&'static str> {
    if let Some(explicit) = entry.type_.as_deref()
        && is_wire_type(explicit)
    {
        return KNOWN_WIRE_TYPES.iter().find(|wire| **wire == explicit).copied();
    }
    let npm = entry.npm.as_deref().unwrap_or_default().to_lowercase();
    let id = entry.id.as_deref().unwrap_or_default().to_lowercase();
    if npm.contains("anthropic") || id.contains("anthropic") || id.contains("claude") {
        return Some("anthropic");
    }
    if id.contains("vertex") {
        return Some("google-genai");
    }
    if npm.contains("google") || id.contains("google") || id.contains("gemini") {
        return Some("google-genai");
    }
    if npm.contains("openai") || id.contains("openai") {
        return Some("openai");
    }
    None
}

pub fn models_dev_base_url(entry: &ModelsDevProviderEntry, wire: &str) -> Option<String> {
    let api = entry.api.as_deref()?;
    if api.is_empty() || api.contains("${") {
        return None;
    }
    Some(adapt_base_url_for_wire(api, wire))
}

pub fn adapt_base_url_for_wire(base_url: &str, wire: &str) -> String {
    if wire != "anthropic" {
        return base_url.to_string();
    }
    if let Some(base) = base_url.strip_suffix("/v1/") {
        return base.to_string();
    }
    if let Some(base) = base_url.strip_suffix("/v1") {
        return base.to_string();
    }
    base_url.to_string()
}

fn models_dev_endpoint_required(entry: &ModelsDevProviderEntry, wire: &str) -> bool {
    if entry.api.as_deref().is_some_and(|api| !api.is_empty()) {
        return true;
    }
    let npm = entry.npm.as_deref().unwrap_or_default().to_lowercase();
    match wire {
        "openai" | "openai_responses" => npm != "@ai-sdk/openai",
        "anthropic" => npm != "@ai-sdk/anthropic",
        _ => false,
    }
}

#[derive(Default)]
struct ModelsDevThinkingOptions {
    efforts: Option<Vec<String>>,
    off_effort: Option<String>,
    has_toggle: bool,
    always_thinking: Option<bool>,
}

fn models_dev_thinking_options(options: Option<&[ModelsDevReasoningOption]>) -> ModelsDevThinkingOptions {
    let Some(options) = options else {
        return ModelsDevThinkingOptions::default();
    };
    let mut result = ModelsDevThinkingOptions::default();
    for option in options {
        if option.type_.as_deref() == Some("toggle") {
            result.has_toggle = true;
            continue;
        }
        if option.type_.as_deref() != Some("effort") {
            continue;
        }
        let Some(values) = option.values.as_ref() else {
            continue;
        };
        let has_null_tier = values.iter().any(|value| value.is_null());
        let levels: Vec<String> = values
            .iter()
            .filter_map(|value| value.as_str())
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect();
        if let Some(off) = levels.iter().find(|value| value.eq_ignore_ascii_case("none")) {
            result.off_effort = Some(off.clone());
        } else if has_null_tier {
            result.off_effort = Some("none".to_string());
        }
        let selectable: Vec<String> = levels
            .into_iter()
            .filter(|value| !value.eq_ignore_ascii_case("none"))
            .collect();
        if !selectable.is_empty() {
            result.efforts = Some(selectable);
        }
    }
    result.always_thinking =
        (result.efforts.is_some() && result.off_effort.is_none() && !result.has_toggle).then_some(true);
    result
}

fn models_dev_reasoning_key(interleaved: Option<&ModelsDevInterleaved>) -> Option<String> {
    let ModelsDevInterleaved::Options(options) = interleaved? else {
        return None;
    };
    options
        .field
        .as_deref()
        .map(str::trim)
        .filter(|field| !field.is_empty())
        .map(str::to_string)
}

pub fn models_dev_model_to_capability(model: &ModelsDevModelEntry) -> Option<ModelsDevModel> {
    let id = model.id.as_deref().filter(|id| !id.is_empty())?;
    let context = model.limit.as_ref().and_then(|limit| limit.context)?;
    if context == 0 || !is_usable_chat_model(model) {
        return None;
    }
    let inputs = model
        .modalities
        .as_ref()
        .and_then(|modalities| modalities.input.as_deref())
        .unwrap_or_default();
    let output = model.limit.as_ref().and_then(|limit| limit.output);
    let thinking = models_dev_thinking_options(model.reasoning_options.as_deref());
    let input = model.limit.as_ref().and_then(|limit| limit.input);
    let max_input_tokens = input.filter(|input| *input > 0).map(|input| input.min(context));
    Some(ModelsDevModel {
        id: id.to_string(),
        name: model
            .name
            .as_deref()
            .filter(|name| !name.is_empty())
            .map(str::to_string),
        max_output_size: output.filter(|output| *output > 0),
        reasoning_key: models_dev_reasoning_key(model.interleaved.as_ref()),
        support_efforts: thinking.efforts.clone(),
        off_effort: thinking.off_effort.clone(),
        always_thinking: thinking.always_thinking,
        protocol: None,
        base_url: None,
        capability: ModelCapability {
            image_in: inputs.iter().any(|input| input == "image"),
            video_in: inputs.iter().any(|input| input == "video"),
            audio_in: inputs.iter().any(|input| input == "audio"),
            pdf_in: Some(inputs.iter().any(|input| input == "pdf")),
            thinking: model.reasoning == Some(true)
                || thinking.efforts.is_some()
                || thinking.has_toggle,
            tool_use: model.tool_call.unwrap_or(true),
            max_context_tokens: context,
            max_input_tokens,
            dynamically_loaded_tools: Some(model.dynamically_loaded_tools == Some(true)),
            unknown_capability: false,
        },
    })
}

fn wire_has_protocol_thinking_disable(protocol: Option<&str>) -> bool {
    matches!(protocol, Some("anthropic" | "kimi"))
}

pub fn models_dev_provider_models(entry: &ModelsDevProviderEntry) -> Vec<ModelsDevModel> {
    let provider_wire = resolve_models_dev_wire(entry);
    entry
        .models
        .as_ref()
        .map(|models| models.values().collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|raw| {
            apply_model_provider_override(
                models_dev_model_to_capability(raw),
                raw,
                entry,
                provider_wire.as_deref(),
            )
        })
        .map(|mut model| {
            let protocol = model.protocol.as_deref().or(provider_wire.as_deref());
            if model.always_thinking == Some(true) && wire_has_protocol_thinking_disable(protocol) {
                model.always_thinking = None;
            }
            model
        })
        .collect()
}

fn apply_model_provider_override(
    model: Option<ModelsDevModel>,
    raw: &ModelsDevModelEntry,
    entry: &ModelsDevProviderEntry,
    provider_wire: Option<&str>,
) -> Option<ModelsDevModel> {
    let model = model?;
    let Some(override_) = raw.provider.as_ref() else {
        return Some(model);
    };
    let override_npm = override_.npm.as_deref().map(str::to_lowercase);
    if override_npm.as_deref().is_some_and(|npm| {
        npm.contains("amazon-bedrock") || npm.contains("cohere")
    }) {
        return None;
    }
    let override_wire = match override_npm.as_deref() {
        Some(npm) => Some(infer_override_wire(npm).unwrap_or("openai")),
        None => provider_wire,
    };
    let Some(override_wire) = override_wire else {
        return Some(model);
    };
    let raw_api = override_.api.as_deref();
    let api = raw_api.or(entry.api.as_deref());
    let usable_api = api.filter(|api| !api.is_empty() && !api.contains("${"));
    if Some(override_wire) == provider_wire {
        if raw_api.is_some_and(|api| api.contains("${")) {
            return None;
        }
        if let Some(usable_api) = usable_api
            && Some(usable_api) != entry.api.as_deref()
        {
            return Some(ModelsDevModel {
                base_url: Some(adapt_base_url_for_wire(usable_api, override_wire)),
                ..model
            });
        }
        return Some(model);
    }
    if override_wire == "anthropic"
        && let Some(usable_api) = usable_api
    {
        return Some(ModelsDevModel {
            protocol: Some("anthropic".to_string()),
            base_url: Some(adapt_base_url_for_wire(usable_api, "anthropic")),
            ..model
        });
    }
    None
}

fn infer_override_wire(npm: &str) -> Option<&'static str> {
    let normalized = npm.to_lowercase();
    if normalized.contains("anthropic") {
        return Some("anthropic");
    }
    if normalized.contains("vertex") || normalized.contains("google") {
        return Some("google-genai");
    }
    if normalized.contains("openai") {
        return Some("openai");
    }
    None
}
