use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use indexmap::IndexMap;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::human::llm::capability::ModelCapability as HumanModelCapability;
use crate::human::llm::model_family::resolve_model_family;
use crate::human::llm::protocol::base::ProtocolName;
use crate::human::llm::protocol::connection::{ProtocolEndpoint, ProviderConnection};
use crate::human::llm::provider::definition::ProviderCapability;
use crate::human::llm::requester::bases::anthropic::requester::{
    AnthropicRequesterOptions, create_anthropic_base,
};
use crate::human::llm::requester::bases::openai::requester::OpenAIBase;
use crate::human::llm::requester::bases::openai_responses::requester::OpenAIResponsesBase;
use crate::human::llm::requester::requester::LlmErrorClassifier;
use crate::human::llm_kimi::errors::classify_kimi_quota_error;
use crate::human::llm_kimi::r#trait::{
    KIMI_ANTHROPIC_TRAIT, KIMI_CONNECTION, KIMI_DEFAULT_BASE_URL, KIMI_OPEN_AI_TRAIT,
    KIMI_RESPONSES_TRAIT,
};
use crate::llm_adapter::protocol::protocol::Protocol;
use crate::llm_adapter::protocol::protocol_base::{ProtocolTrait, ProtocolTraitMetadata};
use crate::llm_adapter::provider::provider::ModelSource;

fn endpoint(api_key_env: &str, base_url_env: &str, default_base_url: Option<&str>) -> ProtocolEndpoint {
    ProtocolEndpoint {
        api_key_env: Some(api_key_env.to_string()),
        base_url_env: Some(base_url_env.to_string()),
        default_base_url: default_base_url.map(ToString::to_string),
    }
}

pub static OPEN_AI_CONNECTION: LazyLock<ProviderConnection> = LazyLock::new(|| {
    ProviderConnection::new(|_| Some(endpoint("OPENAI_API_KEY", "OPENAI_BASE_URL", None)))
});
pub static ANTHROPIC_CONNECTION: LazyLock<ProviderConnection> = LazyLock::new(|| {
    ProviderConnection::new(|_| Some(endpoint("ANTHROPIC_API_KEY", "ANTHROPIC_BASE_URL", None)))
});
pub static GEMINI_ENDPOINT: LazyLock<ProtocolEndpoint> =
    LazyLock::new(|| endpoint("GOOGLE_API_KEY", "GOOGLE_GEMINI_BASE_URL", None));
pub static VERTEX_ENDPOINT: LazyLock<ProtocolEndpoint> =
    LazyLock::new(|| endpoint("VERTEXAI_API_KEY", "GOOGLE_VERTEX_BASE_URL", None));
pub static GEMINI_CONNECTION: LazyLock<ProviderConnection> =
    LazyLock::new(|| ProviderConnection::new(|_| Some(GEMINI_ENDPOINT.clone())));
pub static VERTEX_CONNECTION: LazyLock<ProviderConnection> =
    LazyLock::new(|| ProviderConnection::new(|_| Some(VERTEX_ENDPOINT.clone())));
pub static KIMI_ENDPOINT: LazyLock<ProtocolEndpoint> = LazyLock::new(|| {
    endpoint(
        "KIMI_API_KEY",
        "KIMI_BASE_URL",
        Some(KIMI_DEFAULT_BASE_URL),
    )
});
pub static DEEPSEEK_ENDPOINT: LazyLock<ProtocolEndpoint> = LazyLock::new(|| {
    endpoint(
        "DEEPSEEK_API_KEY",
        "DEEPSEEK_BASE_URL",
        Some("https://api.deepseek.com"),
    )
});
pub static DEEPSEEK_CONNECTION: LazyLock<ProviderConnection> =
    LazyLock::new(|| ProviderConnection::new(|_| Some(DEEPSEEK_ENDPOINT.clone())));

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HostHeaders {
    Full,
    UserAgent,
}

#[derive(Clone)]
pub struct ProviderDefinition {
    pub id: String,
    pub base_protocol: Protocol,
    pub trait_: Option<ProtocolTraitMetadata>,
    pub adapter_trait: Option<ProtocolTrait>,
    pub connection: Option<ProviderConnection>,
    pub classify_error: Option<LlmErrorClassifier>,
    pub capability: Option<ProviderCapability>,
    pub endpoint: Option<ProtocolEndpoint>,
    pub endpoints: Option<Vec<ProtocolEndpoint>>,
    pub host_headers: Option<HostHeaders>,
    pub model_source: Option<ModelSource>,
    pub binding: Option<crate::human::llm::provider::definition::ProtocolBinding>,
}

impl ProviderDefinition {
    pub fn new(id: &str, base_protocol: Protocol) -> Self {
        Self {
            id: id.to_string(),
            base_protocol,
            trait_: None,
            adapter_trait: None,
            connection: None,
            classify_error: None,
            capability: None,
            endpoint: None,
            endpoints: None,
            host_headers: None,
            model_source: None,
            binding: None,
        }
    }

    pub fn with_trait(mut self, trait_: ProtocolTrait) -> Self {
        self.trait_ = Some(trait_.metadata());
        self.adapter_trait = Some(trait_);
        self
    }
}

static PROVIDER_DEFINITIONS: LazyLock<Mutex<IndexMap<String, IndexMap<Protocol, ProviderDefinition>>>> =
    LazyLock::new(|| {
        let mut registry = IndexMap::new();
        for definition in builtin_definitions() {
            register_in(&mut registry, definition);
        }
        Mutex::new(registry)
    });

fn register_in(
    registry: &mut IndexMap<String, IndexMap<Protocol, ProviderDefinition>>,
    definition: ProviderDefinition,
) {
    let by_protocol = registry.entry(definition.id.clone()).or_default();
    if by_protocol.contains_key(&definition.base_protocol) {
        panic!(
            "provider definition '{}' is already registered for protocol '{}'",
            definition.id, definition.base_protocol
        );
    }
    by_protocol.insert(definition.base_protocol, definition);
}

pub fn register_provider_definition(definition: ProviderDefinition) {
    register_in(&mut PROVIDER_DEFINITIONS.lock(), definition);
}

pub fn get_provider_definition(
    id: &str,
    protocol: Option<Protocol>,
) -> Option<ProviderDefinition> {
    let registry = PROVIDER_DEFINITIONS.lock();
    let by_protocol = registry.get(id)?;
    match protocol {
        Some(protocol) => by_protocol.get(&protocol).cloned(),
        None => by_protocol.first().map(|(_, definition)| definition.clone()),
    }
}

pub fn get_provider_definitions(id: &str) -> Vec<ProviderDefinition> {
    PROVIDER_DEFINITIONS
        .lock()
        .get(id)
        .map(|definitions| definitions.values().cloned().collect())
        .unwrap_or_default()
}

pub fn has_provider_definition(id: &str) -> bool {
    PROVIDER_DEFINITIONS.lock().contains_key(id)
}

pub fn resolve_provider_definition(
    provider_type: Option<&str>,
    provider_id: Option<&str>,
    protocol: Option<Protocol>,
) -> Option<ProviderDefinition> {
    let by_id = provider_id
        .filter(|provider_id| Some(*provider_id) != provider_type)
        .and_then(|provider_id| get_provider_definition(provider_id, protocol));
    if by_id.is_some() {
        return by_id;
    }
    get_provider_definition(provider_type?, protocol)
}

pub fn register_provider_definition_if_absent(definition: ProviderDefinition) {
    if get_provider_definition(&definition.id, Some(definition.base_protocol)).is_some() {
        return;
    }
    register_provider_definition(definition);
}

pub fn is_oauth_catalog_vendor(id: Option<&str>) -> bool {
    id.is_some_and(|id| {
        get_provider_definitions(id)
            .iter()
            .any(|definition| definition.model_source == Some(ModelSource::OauthCatalog))
    })
}

pub fn list_provider_definitions() -> Vec<ProviderDefinition> {
    PROVIDER_DEFINITIONS
        .lock()
        .values()
        .flat_map(|definitions| definitions.values().cloned().collect::<Vec<_>>())
        .collect()
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedProviderEndpoint {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExplainedProviderEndpoint {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url_env_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url_is_default: Option<bool>,
}

#[derive(Default)]
struct AggregatedEndpointDeclaration {
    api_key_env: Vec<String>,
    base_url_env: Vec<String>,
    default_base_url: Option<String>,
}

fn normalize_endpoint_declaration(
    endpoint: Option<&ProtocolEndpoint>,
) -> Option<AggregatedEndpointDeclaration> {
    endpoint.map(|endpoint| AggregatedEndpointDeclaration {
        api_key_env: endpoint.api_key_env.iter().cloned().collect(),
        base_url_env: endpoint.base_url_env.iter().cloned().collect(),
        default_base_url: endpoint.default_base_url.clone(),
    })
}

fn aggregate_endpoints(
    endpoints: Option<&[ProtocolEndpoint]>,
) -> Option<AggregatedEndpointDeclaration> {
    let endpoints = endpoints.filter(|endpoints| !endpoints.is_empty())?;
    let mut aggregated = AggregatedEndpointDeclaration::default();
    for endpoint in endpoints {
        if let Some(api_key_env) = &endpoint.api_key_env {
            aggregated.api_key_env.push(api_key_env.clone());
        }
        if let Some(base_url_env) = &endpoint.base_url_env {
            aggregated.base_url_env.push(base_url_env.clone());
        }
        if endpoint.default_base_url.is_some() {
            aggregated.default_base_url = endpoint.default_base_url.clone();
        }
    }
    Some(aggregated)
}

fn first_env_hit(
    names: &[String],
    env: &HashMap<String, String>,
) -> Option<(String, String)> {
    names.iter().find_map(|name| {
        env.get(name)
            .filter(|value| !value.is_empty())
            .map(|value| (name.clone(), value.clone()))
    })
}

pub fn explain_provider_endpoint(
    provider_type: &str,
    env: &HashMap<String, String>,
    provider_id: Option<&str>,
) -> ExplainedProviderEndpoint {
    let Some(definition) = resolve_provider_definition(Some(provider_type), provider_id, None)
    else {
        return ExplainedProviderEndpoint::default();
    };
    let endpoint = normalize_endpoint_declaration(definition.endpoint.as_ref())
        .or_else(|| aggregate_endpoints(definition.endpoints.as_deref()));
    let Some(endpoint) = endpoint else {
        return ExplainedProviderEndpoint::default();
    };
    let api_key_hit = first_env_hit(&endpoint.api_key_env, env);
    let base_url_hit = first_env_hit(&endpoint.base_url_env, env);
    ExplainedProviderEndpoint {
        api_key: api_key_hit.as_ref().map(|(_, value)| value.clone()),
        api_key_env_name: api_key_hit.as_ref().map(|(name, _)| name.clone()),
        base_url: base_url_hit
            .as_ref()
            .map(|(_, value)| value.clone())
            .or(endpoint.default_base_url.clone()),
        base_url_env_name: base_url_hit.as_ref().map(|(name, _)| name.clone()),
        base_url_is_default: (base_url_hit.is_none() && endpoint.default_base_url.is_some())
            .then_some(true),
    }
}

pub fn resolve_provider_endpoint(
    provider_type: &str,
    env: &HashMap<String, String>,
    provider_id: Option<&str>,
) -> ResolvedProviderEndpoint {
    let endpoint = explain_provider_endpoint(provider_type, env, provider_id);
    ResolvedProviderEndpoint {
        api_key: endpoint.api_key,
        base_url: endpoint.base_url,
    }
}

fn deepseek_capability(model_name: &str) -> Option<HumanModelCapability> {
    if resolve_model_family(model_name)?.id != "deepseek" {
        return None;
    }
    Some(HumanModelCapability {
        image_in: true,
        video_in: false,
        audio_in: false,
        thinking: true,
        tool_use: true,
        pdf_in: Some(false),
        dynamically_loaded_tools: None,
        unknown_capability: false,
    })
}

fn builtin_definitions() -> Vec<ProviderDefinition> {
    let mut anthropic = ProviderDefinition::new("anthropic", Protocol::Anthropic);
    anthropic.endpoint = Some(endpoint("ANTHROPIC_API_KEY", "ANTHROPIC_BASE_URL", None));
    let mut openai = ProviderDefinition::new("openai", Protocol::OpenAI);
    openai.endpoint = Some(endpoint("OPENAI_API_KEY", "OPENAI_BASE_URL", None));
    let mut responses = ProviderDefinition::new("openai_responses", Protocol::OpenAIResponses);
    responses.endpoint = Some(endpoint("OPENAI_API_KEY", "OPENAI_BASE_URL", None));
    let mut vertex = ProviderDefinition::new("vertexai", Protocol::GoogleGenAI);
    vertex.endpoints = Some(vec![VERTEX_ENDPOINT.clone()]);
    let mut google = ProviderDefinition::new("google-genai", Protocol::GoogleGenAI);
    google.endpoints = Some(vec![VERTEX_ENDPOINT.clone(), GEMINI_ENDPOINT.clone()]);

    let kimi_openai = ProviderDefinition::new("kimi", Protocol::OpenAI)
        .with_trait(ProtocolTrait::OpenAI(KIMI_OPEN_AI_TRAIT.clone()));
    let kimi_anthropic = ProviderDefinition::new("kimi", Protocol::Anthropic)
        .with_trait(ProtocolTrait::Anthropic(KIMI_ANTHROPIC_TRAIT.clone()));
    let kimi_responses = ProviderDefinition::new("kimi", Protocol::OpenAIResponses)
        .with_trait(ProtocolTrait::OpenAIResponses(
            KIMI_RESPONSES_TRAIT.clone(),
        ));
    let kimi_openai_binding = crate::human::llm::provider::definition::ProtocolBinding::new(
        ProtocolName::OpenAI,
        Arc::new(OpenAIBase),
        Some(KIMI_OPEN_AI_TRAIT.clone()),
        Some(KIMI_CONNECTION.clone()),
        Some(Arc::new(classify_kimi_quota_error)),
        None,
    );
    let kimi_anthropic_binding = crate::human::llm::provider::definition::ProtocolBinding::new(
        ProtocolName::Anthropic,
        Arc::new(create_anthropic_base(AnthropicRequesterOptions {
            beta_api: Some(true),
            ..Default::default()
        })),
        Some(KIMI_ANTHROPIC_TRAIT.clone()),
        Some(KIMI_CONNECTION.clone()),
        Some(Arc::new(classify_kimi_quota_error)),
        None,
    );
    let kimi_responses_binding = crate::human::llm::provider::definition::ProtocolBinding::new(
        ProtocolName::OpenAIResponses,
        Arc::new(OpenAIResponsesBase),
        Some(KIMI_RESPONSES_TRAIT.clone()),
        Some(KIMI_CONNECTION.clone()),
        Some(Arc::new(classify_kimi_quota_error)),
        None,
    );
    let kimi = |mut definition: ProviderDefinition, binding| {
        definition.connection = Some(KIMI_CONNECTION.clone());
        definition.classify_error = Some(Arc::new(classify_kimi_quota_error));
        definition.endpoint = Some(KIMI_ENDPOINT.clone());
        definition.host_headers = Some(HostHeaders::Full);
        definition.model_source = Some(ModelSource::OauthCatalog);
        definition.binding = Some(binding);
        definition
    };

    let mut deepseek = ProviderDefinition::new("deepseek", Protocol::OpenAI);
    deepseek.connection = Some(DEEPSEEK_CONNECTION.clone());
    deepseek.endpoint = Some(DEEPSEEK_ENDPOINT.clone());
    deepseek.capability = Some(Arc::new(deepseek_capability));
    deepseek.binding = Some(crate::human::llm::provider::definition::ProtocolBinding::new(
        ProtocolName::OpenAI,
        Arc::new(OpenAIBase),
        None,
        Some(DEEPSEEK_CONNECTION.clone()),
        None,
        Some(Arc::new(deepseek_capability)),
    ));

    vec![
        anthropic,
        openai,
        responses,
        vertex,
        google,
        kimi(kimi_openai, kimi_openai_binding),
        kimi(kimi_anthropic, kimi_anthropic_binding),
        kimi(kimi_responses, kimi_responses_binding),
        deepseek,
    ]
}
