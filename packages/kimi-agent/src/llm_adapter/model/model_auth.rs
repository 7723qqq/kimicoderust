use std::collections::HashMap;

use indexmap::IndexMap;
use kimi_core::primitives::errors::Error2;
use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::human::llm::requester::bases::anthropic::profile::{
    BUDGET_THINKING_EFFORTS, match_known_anthropic_model_profile, match_unknown_claude_profile,
};
use crate::llm_adapter::contract::errors::CONFIG_INVALID_ERROR_CODE;
use crate::llm_adapter::model::model_types::ResolvedModelAuthMaterial;
use crate::llm_adapter::model::thinking::drives_thinking_through_traits;
use crate::llm_adapter::protocol::protocol::Protocol;
use crate::llm_adapter::provider::provider::ProviderConfig;
use crate::llm_adapter::provider::provider_definition::{
    explain_provider_endpoint, get_provider_definition,
};
use crate::wire::record::WireAsyncError;

use super::model::ModelRecord;

const LEGACY_VERTEX_PROVIDER_TYPE: &str = "vertexai";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeclaredProviderCredential {
    Inline { api_key: String },
    Env { api_key_env: String },
    None,
    Conflict { message: String },
}

pub fn credential_conflict_message(
    kind: &str,
    name: &str,
    first: &str,
    second: &str,
) -> String {
    format!(
        "{kind} \"{name}\" has both {first} and {second} set in config.toml - they are mutually exclusive. Remove one."
    )
}

pub fn api_key_env_missing_message(provider_name: &str, env_name: &str) -> String {
    format!(
        "Provider \"{provider_name}\" declares api_key_env = \"{env_name}\" in config.toml, but the environment variable is not set or is empty."
    )
}

pub fn declared_provider_credential(
    provider: &ProviderConfig,
    provider_name: &str,
) -> DeclaredProviderCredential {
    let inline_api_key = non_empty(provider.api_key.as_deref());
    let api_key_env = non_empty(provider.api_key_env.as_deref());
    let has_oauth = provider.oauth.is_some();
    if inline_api_key.is_some() && api_key_env.is_some() {
        return DeclaredProviderCredential::Conflict {
            message: credential_conflict_message(
                "Provider",
                provider_name,
                "apiKey",
                "apiKeyEnv",
            ),
        };
    }
    if api_key_env.is_some() && has_oauth {
        return DeclaredProviderCredential::Conflict {
            message: credential_conflict_message(
                "Provider",
                provider_name,
                "apiKeyEnv",
                "oauth",
            ),
        };
    }
    if inline_api_key.is_some() && has_oauth {
        return DeclaredProviderCredential::Conflict {
            message: credential_conflict_message(
                "Provider",
                provider_name,
                "apiKey",
                "oauth",
            ),
        };
    }
    if let Some(api_key) = inline_api_key {
        return DeclaredProviderCredential::Inline { api_key };
    }
    if let Some(api_key_env) = api_key_env {
        return DeclaredProviderCredential::Env { api_key_env };
    }
    DeclaredProviderCredential::None
}

pub struct ResolveModelAuthMaterialArgs<'a> {
    pub model_id: &'a str,
    pub model: &'a ModelRecord,
    pub provider: Option<&'a ProviderConfig>,
    pub provider_name: &'a str,
}

pub fn resolve_model_auth_material(
    args: ResolveModelAuthMaterialArgs<'_>,
) -> Result<ResolvedModelAuthMaterial, WireAsyncError> {
    let model_api_key = non_empty(args.model.api_key.as_deref());
    if model_api_key.is_some() && args.model.oauth.is_some() {
        return Err(Box::new(Error2::new(
            CONFIG_INVALID_ERROR_CODE,
            credential_conflict_message("Model", args.model_id, "apiKey", "oauth"),
        )));
    }
    if let Some(api_key) = model_api_key {
        return Ok(ResolvedModelAuthMaterial {
            api_key: Some(api_key),
            ..Default::default()
        });
    }
    if let Some(oauth) = &args.model.oauth {
        return Ok(ResolvedModelAuthMaterial {
            oauth: Some(oauth.clone()),
            oauth_provider_key: args
                .model
                .provider_id
                .clone()
                .or_else(|| args.model.provider.clone()),
            ..Default::default()
        });
    }

    let raw_auth_type = args
        .provider
        .and_then(|provider| provider.type_.as_deref())
        .or_else(|| args.model.protocol.map(|protocol| protocol.as_str()));
    let provider_auth_type = raw_auth_type.map(|type_| {
        if type_ == LEGACY_VERTEX_PROVIDER_TYPE {
            "google-genai"
        } else {
            type_
        }
    });
    let empty_env = HashMap::new();
    let env = args
        .provider
        .and_then(|provider| provider.env.as_ref())
        .unwrap_or(&empty_env);
    let provider_endpoint = provider_auth_type.map(|type_| {
        explain_provider_endpoint(type_, env, Some(args.provider_name))
    });
    match declared_provider_credential(
        args.provider.unwrap_or(&ProviderConfig::default()),
        args.provider_name,
    ) {
        DeclaredProviderCredential::Conflict { message } => {
            return Err(Box::new(Error2::new(CONFIG_INVALID_ERROR_CODE, message)));
        }
        DeclaredProviderCredential::Inline { api_key } => {
            return Ok(ResolvedModelAuthMaterial {
                api_key: Some(api_key),
                ..Default::default()
            });
        }
        DeclaredProviderCredential::Env { api_key_env } => {
            return Ok(ResolvedModelAuthMaterial {
                api_key_env: Some(api_key_env),
                ..Default::default()
            });
        }
        DeclaredProviderCredential::None => {}
    }
    let endpoint_api_key = provider_endpoint
        .as_ref()
        .and_then(|endpoint| non_empty(endpoint.api_key.as_deref()));
    if endpoint_api_key.is_some()
        && args.provider.is_some_and(|provider| provider.oauth.is_some())
    {
        return Err(Box::new(Error2::new(
            CONFIG_INVALID_ERROR_CODE,
            credential_conflict_message("Provider", args.provider_name, "apiKey", "oauth"),
        )));
    }
    if let Some(api_key) = endpoint_api_key {
        return Ok(ResolvedModelAuthMaterial {
            api_key: Some(api_key),
            ..Default::default()
        });
    }
    if let Some(oauth) = args.provider.and_then(|provider| provider.oauth.as_ref()) {
        return Ok(ResolvedModelAuthMaterial {
            oauth: Some(oauth.clone()),
            oauth_provider_key: args
                .model
                .provider_id
                .clone()
                .or_else(|| args.model.provider.clone()),
            ..Default::default()
        });
    }
    Ok(ResolvedModelAuthMaterial::default())
}

pub fn effective_model_config(model: &ModelRecord, provider_type: Option<&str>) -> ModelRecord {
    let mut effective = model.clone();
    if let Some(overrides) = model.overrides.clone() {
        if overrides.max_context_size.is_some() {
            effective.max_context_size = overrides.max_context_size;
        }
        if overrides.max_input_size.is_some() {
            effective.max_input_size = overrides.max_input_size;
        }
        if overrides.max_output_size.is_some() {
            effective.max_output_size = overrides.max_output_size;
        }
        if overrides.capabilities.is_some() {
            effective.capabilities = overrides.capabilities.clone();
        }
        if overrides.display_name.is_some() {
            effective.display_name = overrides.display_name.clone();
        }
        if overrides.reasoning_key.is_some() {
            effective.reasoning_key = overrides.reasoning_key.clone();
        }
        if overrides.adaptive_thinking.is_some() {
            effective.adaptive_thinking = overrides.adaptive_thinking;
        }
        if overrides.support_efforts.is_some() {
            effective.support_efforts = overrides.support_efforts.clone();
        }
        if overrides.default_effort.is_some() {
            effective.default_effort = overrides.default_effort.clone();
        }
        if overrides.off_effort.is_some() {
            effective.off_effort = overrides.off_effort.clone();
        }
        if overrides.support_efforts.is_some()
            && overrides.default_effort.is_none()
            && effective.default_effort.as_ref().is_some_and(|default| {
                !overrides
                    .support_efforts
                    .as_ref()
                    .is_some_and(|efforts| efforts.contains(default))
            })
        {
            effective.default_effort = None;
        }
    }
    effective.overrides = None;
    if effective.max_input_size.is_some()
        && effective.max_context_size.is_some()
        && effective.max_input_size > effective.max_context_size
    {
        effective.max_input_size = effective.max_context_size;
    }
    with_anthropic_profile(&effective, provider_type)
}

pub fn with_anthropic_profile(model: &ModelRecord, provider_type: Option<&str>) -> ModelRecord {
    let Some(wire_name) = model.name.as_ref().or(model.model.as_ref()) else {
        return model.clone();
    };
    let protocol = model
        .protocol
        .map(|protocol| protocol.as_str())
        .or(provider_type);
    let profile = if provider_type.is_some_and(|provider_type| {
        !drives_thinking_through_traits(provider_type) && protocol == Some("anthropic")
    }) {
        match_known_anthropic_model_profile(wire_name)
            .or_else(|| match_unknown_claude_profile(wire_name))
    } else {
        match_known_anthropic_model_profile(wire_name)
    };
    let Some(profile) = profile else {
        return model.clone();
    };
    let capability = if profile.can_disable_thinking {
        "thinking"
    } else {
        "always_thinking"
    };
    let mut capabilities = model.capabilities.clone().unwrap_or_default();
    if !capabilities
        .iter()
        .any(|candidate| js_trim(candidate).to_lowercase() == capability)
    {
        capabilities.push(capability.to_string());
    }
    let support_efforts = model.support_efforts.clone().unwrap_or_else(|| {
        if model.adaptive_thinking == Some(false) {
            BUDGET_THINKING_EFFORTS
                .iter()
                .map(|effort| effort.to_string())
                .collect()
        } else {
            profile.efforts.clone()
        }
    });
    let default_effort = model.default_effort.clone().or_else(|| {
        support_efforts
            .iter()
            .any(|effort| effort == "high")
            .then(|| "high".to_string())
    });
    ModelRecord {
        capabilities: Some(capabilities),
        support_efforts: Some(support_efforts),
        default_effort,
        ..model.clone()
    }
}

pub fn derive_provider_id(base_url: &str) -> String {
    let Ok(url) = url::Url::parse(base_url) else {
        return base_url.to_string();
    };
    let Some(host) = url.host_str() else {
        return String::new();
    };
    match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    }
}

pub fn provider_name_from_flat_model(model: &ModelRecord) -> Option<String> {
    non_empty(model.base_url.as_deref()).map(|base_url| derive_provider_id(&base_url))
}

pub fn resolve_model_protocol(
    model: &ModelRecord,
    provider: Option<&ProviderConfig>,
) -> Option<Protocol> {
    if let Some(protocol) = model.protocol {
        return Some(protocol);
    }
    let provider_type = provider?.type_.as_deref()?;
    if provider_type == LEGACY_VERTEX_PROVIDER_TYPE {
        return Some(Protocol::GoogleGenAI);
    }
    if let Ok(protocol) = provider_type.parse() {
        return Some(protocol);
    }
    get_provider_definition(provider_type).map(|definition| definition.base_protocol)
}

pub fn resolve_endpoint_base_url(
    model: &ModelRecord,
    provider: &ProviderConfig,
    provider_id: Option<&str>,
) -> Option<String> {
    if let Some(base_url) = non_empty(model.base_url.as_deref()) {
        return Some(base_url);
    }
    if let Some(base_url) = non_empty(provider.base_url.as_deref()) {
        return Some(base_url);
    }
    let raw_endpoint_type = provider
        .type_
        .as_deref()
        .or_else(|| model.protocol.map(|protocol| protocol.as_str()))?;
    let endpoint_type = if raw_endpoint_type == LEGACY_VERTEX_PROVIDER_TYPE {
        "google-genai"
    } else {
        raw_endpoint_type
    };
    let empty_env = HashMap::new();
    let endpoint = explain_provider_endpoint(
        endpoint_type,
        provider.env.as_ref().unwrap_or(&empty_env),
        provider_id,
    );
    non_empty(endpoint.base_url.as_deref())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModelReadyFailureReason {
    NoDefault,
    DanglingAlias,
    ProviderMissing,
    Unresolvable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelReadyResolution {
    Resolved,
    Failed(ModelReadyFailureReason),
}

impl Serialize for ModelReadyResolution {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Resolved => {
                let mut state = serializer.serialize_struct("ModelReadyResolution", 1)?;
                state.serialize_field("resolved", &true)?;
                state.end()
            }
            Self::Failed(reason) => {
                let mut state = serializer.serialize_struct("ModelReadyResolution", 2)?;
                state.serialize_field("resolved", &false)?;
                state.serialize_field("reason", reason)?;
                state.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for ModelReadyResolution {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            resolved: bool,
            reason: Option<ModelReadyFailureReason>,
        }

        let wire = Wire::deserialize(deserializer)?;
        if wire.resolved {
            return Ok(Self::Resolved);
        }
        wire.reason
            .map(Self::Failed)
            .ok_or_else(|| de::Error::missing_field("reason"))
    }
}

pub fn resolve_model_for_ready(
    model_id: Option<&str>,
    models: &IndexMap<String, ModelRecord>,
    providers: &HashMap<String, ProviderConfig>,
    default_provider: Option<&str>,
) -> ModelReadyResolution {
    let Some(model_id) = model_id.filter(|id| !js_trim(id).is_empty()) else {
        return ModelReadyResolution::Failed(ModelReadyFailureReason::NoDefault);
    };
    let Some(configured) = models.get(model_id) else {
        return ModelReadyResolution::Failed(ModelReadyFailureReason::DanglingAlias);
    };
    let model = effective_model_config(configured, None);
    let fallback_provider = default_provider.filter(|provider| !js_trim(provider).is_empty());
    let provider_id = model
        .provider_id
        .as_deref()
        .or(model.provider.as_deref())
        .or(fallback_provider);
    let provider = provider_id.and_then(|provider_id| providers.get(provider_id));
    if provider_id.is_some() && provider.is_none() {
        return ModelReadyResolution::Failed(ModelReadyFailureReason::ProviderMissing);
    }
    let provider_name = provider_id
        .map(ToString::to_string)
        .or_else(|| provider_name_from_flat_model(&model));
    if provider_name.is_none()
        || non_empty(model.name.as_deref().or(model.model.as_deref())).is_none()
        || model.max_context_size.unwrap_or(0) == 0
        || resolve_model_protocol(&model, provider).is_none()
    {
        return ModelReadyResolution::Failed(ModelReadyFailureReason::Unresolvable);
    }
    ModelReadyResolution::Resolved
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

pub fn non_empty(value: Option<&str>) -> Option<String> {
    let trimmed = js_trim(value?);
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}
