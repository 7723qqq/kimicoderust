use std::sync::Arc;

use async_trait::async_trait;
use futures_util::future::BoxFuture;

use crate::human::llm::capability::{ModelCapability, UNKNOWN_CAPABILITY};
use crate::human::llm::media::upload::ProviderMediaContribution;
use crate::human::llm::model::{LlmConnection, LlmModel};
use crate::human::llm::protocol::base::{ProtocolBase, ProtocolName, ProtocolRequesterOptions};
use crate::human::llm::protocol::connection::ProviderConnection;
use crate::human::llm::requester::requester::{
    LlmErrorClassifier, LlmRequester,
};
use crate::wire::record::WireAsyncError;

pub type ProviderCapability =
    Arc<dyn Fn(&str) -> Option<ModelCapability> + Send + Sync>;
type RequesterFactory =
    Arc<dyn Fn() -> Result<Arc<dyn LlmRequester>, WireAsyncError> + Send + Sync>;

#[derive(Clone)]
pub struct ProtocolBinding {
    protocol: ProtocolName,
    capability: Option<ProviderCapability>,
    base_capability: ProviderCapability,
    create_requester: RequesterFactory,
}

impl ProtocolBinding {
    pub fn new<TTrait>(
        protocol: ProtocolName,
        base: Arc<dyn ProtocolBase<TTrait>>,
        trait_: Option<TTrait>,
        connection: Option<ProviderConnection>,
        classify_error: Option<LlmErrorClassifier>,
        capability: Option<ProviderCapability>,
    ) -> Self
    where
        TTrait: Clone + Send + Sync + 'static,
    {
        let base_capability = {
            let base = base.clone();
            Arc::new(move |model_name: &str| base.capability(model_name)) as ProviderCapability
        };
        let create_requester = Arc::new(move || {
            base.create_requester(ProtocolRequesterOptions {
                connection: connection.clone(),
                trait_: trait_.clone(),
                classify_error: classify_error.clone(),
                log: None,
            })
        }) as RequesterFactory;
        Self {
            protocol,
            capability,
            base_capability,
            create_requester,
        }
    }

    pub fn protocol(&self) -> ProtocolName {
        self.protocol
    }

    fn detect_capability(&self, model_name: &str) -> ModelCapability {
        self.capability
            .as_ref()
            .and_then(|capability| capability(model_name))
            .or_else(|| (self.base_capability)(model_name))
            .unwrap_or(UNKNOWN_CAPABILITY)
    }

    fn seed_capability(&self, seed: &LlmModelSeed) -> ModelCapability {
        self.capability
            .as_ref()
            .and_then(|capability| capability(&seed.model))
            .or_else(|| seed.capability.clone())
            .or_else(|| (self.base_capability)(&seed.model))
            .unwrap_or(UNKNOWN_CAPABILITY)
    }

    fn create_requester(&self) -> Result<Arc<dyn LlmRequester>, WireAsyncError> {
        (self.create_requester)()
    }
}

#[derive(Debug, Clone, Default)]
pub struct LlmModelSeed {
    pub model: String,
    pub capability: Option<ModelCapability>,
    pub max_context_size: Option<u64>,
    pub max_input_size: Option<u64>,
    pub base_url: Option<String>,
}

pub type ProviderModelSource = Arc<
    dyn Fn() -> BoxFuture<'static, Result<Vec<LlmModelSeed>, WireAsyncError>> + Send + Sync,
>;

#[derive(Clone)]
pub struct ProviderDefinition {
    pub id: String,
    pub protocols: Vec<ProtocolBinding>,
    pub media: Option<ProviderMediaContribution>,
    pub models: Option<ProviderModelSource>,
}

#[derive(Debug, Clone, Default)]
pub struct LlmResolveModelOptions {
    pub connection: LlmConnection,
    pub protocol: Option<ProtocolName>,
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> &str;
    fn protocols(&self) -> &[ProtocolName];
    fn media(&self) -> Option<&ProviderMediaContribution>;
    async fn list_models(&self) -> Result<Vec<LlmModel>, WireAsyncError>;
    fn resolve_model(
        &self,
        model: &str,
        options: &LlmResolveModelOptions,
    ) -> Result<LlmModel, WireAsyncError>;
    fn create_requester(
        &self,
        protocol: Option<ProtocolName>,
    ) -> Result<Arc<dyn LlmRequester>, WireAsyncError>;
}

struct ConfiguredProvider {
    id: String,
    entries: Vec<ProtocolBinding>,
    protocols: Vec<ProtocolName>,
    media: Option<ProviderMediaContribution>,
    models: Option<ProviderModelSource>,
}

fn protocol_name(name: ProtocolName) -> &'static str {
    match name {
        ProtocolName::OpenAI => "openai",
        ProtocolName::OpenAIResponses => "openai_responses",
        ProtocolName::Anthropic => "anthropic",
        ProtocolName::GoogleGenAI => "google-genai",
        ProtocolName::Antigravity => "antigravity",
    }
}

impl ConfiguredProvider {
    fn binding_for(&self, name: Option<ProtocolName>) -> Result<&ProtocolBinding, WireAsyncError> {
        match name {
            None => Ok(&self.entries[0]),
            Some(name) => self
                .entries
                .iter()
                .find(|binding| binding.protocol == name)
                .ok_or_else(|| {
                    format!(
                        "provider '{}' has no protocol '{}' (available: {})",
                        self.id,
                        protocol_name(name),
                        self.protocols
                            .iter()
                            .copied()
                            .map(protocol_name)
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                    .into()
                }),
        }
    }
}

#[async_trait]
impl Provider for ConfiguredProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn protocols(&self) -> &[ProtocolName] {
        &self.protocols
    }

    fn media(&self) -> Option<&ProviderMediaContribution> {
        self.media.as_ref()
    }

    async fn list_models(&self) -> Result<Vec<LlmModel>, WireAsyncError> {
        let Some(source) = &self.models else {
            return Ok(Vec::new());
        };
        let seeds = source().await?;
        let default_binding = &self.entries[0];
        Ok(seeds
            .into_iter()
            .map(|seed| LlmModel {
                provider: self.id.clone(),
                model: seed.model.clone(),
                capability: default_binding.seed_capability(&seed),
                max_context_size: seed.max_context_size,
                max_input_size: seed.max_input_size,
                connection: LlmConnection {
                    base_url: seed.base_url,
                    ..Default::default()
                },
                ..Default::default()
            })
            .collect())
    }

    fn resolve_model(
        &self,
        model: &str,
        options: &LlmResolveModelOptions,
    ) -> Result<LlmModel, WireAsyncError> {
        let binding = self.binding_for(options.protocol)?;
        Ok(LlmModel {
            provider: self.id.clone(),
            model: model.to_string(),
            capability: binding.detect_capability(model),
            connection: options.connection.clone(),
            ..Default::default()
        })
    }

    fn create_requester(
        &self,
        protocol: Option<ProtocolName>,
    ) -> Result<Arc<dyn LlmRequester>, WireAsyncError> {
        self.binding_for(protocol)?.create_requester()
    }
}

pub fn create_provider(
    definition: ProviderDefinition,
) -> Result<Arc<dyn Provider>, WireAsyncError> {
    if definition.protocols.is_empty() {
        return Err(format!("provider '{}' declares no protocols", definition.id).into());
    }
    let protocols = definition
        .protocols
        .iter()
        .map(|binding| binding.protocol)
        .collect();
    Ok(Arc::new(ConfiguredProvider {
        id: definition.id,
        entries: definition.protocols,
        protocols,
        media: definition.media,
        models: definition.models,
    }))
}
