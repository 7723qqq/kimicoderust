use std::sync::{Arc, LazyLock};

use futures_util::future::FutureExt;

use crate::human::llm::capability::ModelCapability as HumanModelCapability;
use crate::human::log::log::LlmLogger;
use crate::human::llm::media::upload::ProviderMediaContribution;
use crate::human::llm::model::LlmModel;
use crate::human::llm::protocol::base::{ProtocolBase, ProtocolRequesterOptions};
use crate::human::llm::requester::bases::anthropic::requester::{
    AnthropicRequesterOptions, create_anthropic_base,
};
use crate::human::llm::requester::bases::antigravity::requester::create_antigravity_base;
use crate::human::llm::requester::bases::google_genai::requester::create_google_gen_ai_base;
use crate::human::llm::requester::bases::openai::openai_trait::OpenAITrait;
use crate::human::llm::requester::bases::openai::requester::OpenAIBase;
use crate::human::llm::requester::bases::openai_responses::requester::OpenAIResponsesBase;
use crate::human::llm_kimi::files::{
    KimiFiles, KimiFilesFetch, KimiFilesOptions, KimiUploadOptions, kimi_files_base_url,
};
use crate::llm_adapter::contract::capability::{
    ModelCapability, UNKNOWN_CAPABILITY, to_llm_capability,
};
use crate::llm_adapter::model::catalog::Model;
use crate::llm_adapter::model::model_requester_impl::ResolvedLlmModel;
use crate::llm_adapter::protocol::protocol::{IProtocolAdapterRegistry, Protocol};
use crate::llm_adapter::protocol::protocol_base::{
    ProtocolBaseId, ProtocolTrait, ResolvedAdapterIdentity, get_protocol_base, list_protocol_bases,
};
use crate::llm_adapter::provider::provider::ModelSource;
use crate::llm_adapter::provider::provider_definition::{
    ANTHROPIC_CONNECTION, GEMINI_CONNECTION, OPEN_AI_CONNECTION, VERTEX_CONNECTION,
    ProviderDefinition, get_provider_definition, resolve_provider_definition,
};
use crate::wire::record::WireAsyncError;

static KIMI_MEDIA: LazyLock<ProviderMediaContribution> =
    LazyLock::new(|| kimi_media_contribution(None));

pub struct ProtocolAdapterRegistry {
    log: Option<LlmLogger>,
    media_fetch: Option<KimiFilesFetch>,
}

impl ProtocolAdapterRegistry {
    pub fn new(log: Option<LlmLogger>, media_fetch: Option<KimiFilesFetch>) -> Self {
        Self { log, media_fetch }
    }

    fn route_media(&self, definition: Option<&ProviderDefinition>) -> Option<ProviderMediaContribution> {
        (definition.is_some_and(|definition| {
            definition.model_source == Some(ModelSource::OauthCatalog)
        }))
        .then(|| match &self.media_fetch {
            Some(fetch) => kimi_media_contribution(Some(fetch.clone())),
            None => KIMI_MEDIA.clone(),
        })
    }

    fn resolve_route(&self, model: &Model) -> Result<ResolvedLlmModel, WireAsyncError> {
        let definition = resolve_provider_definition(
            model.provider_type.as_deref(),
            Some(&model.provider_name),
            Some(model.protocol),
        );
        let custom = definition.as_ref().filter(|definition| {
            definition.adapter_trait.is_some()
                || definition.connection.is_some()
                || definition.classify_error.is_some()
        });
        let media = self.route_media(definition.as_ref());
        let route_provider;
        let requester = match model.protocol {
            Protocol::OpenAI => {
                route_provider = "openai";
                let trait_ = match custom.and_then(|definition| definition.adapter_trait.as_ref()) {
                    Some(ProtocolTrait::OpenAI(trait_)) => Some(trait_.clone()),
                    _ => openai_reasoning_trait_for(model),
                };
                let connection = custom
                    .and_then(|definition| definition.connection.clone())
                    .or_else(|| custom.is_none().then(|| OPEN_AI_CONNECTION.clone()));
                let classify_error = custom.and_then(|definition| definition.classify_error.clone());
                OpenAIBase.create_requester(ProtocolRequesterOptions {
                    connection,
                    trait_,
                    classify_error,
                    log: self.log.clone(),
                })?
            }
            Protocol::OpenAIResponses => {
                route_provider = "openai-responses";
                let trait_ = match custom.and_then(|definition| definition.adapter_trait.as_ref()) {
                    Some(ProtocolTrait::OpenAIResponses(trait_)) => Some(trait_.clone()),
                    _ => None,
                };
                let connection = custom
                    .and_then(|definition| definition.connection.clone())
                    .or_else(|| custom.is_none().then(|| OPEN_AI_CONNECTION.clone()));
                let classify_error = custom.and_then(|definition| definition.classify_error.clone());
                OpenAIResponsesBase.create_requester(ProtocolRequesterOptions {
                    connection,
                    trait_,
                    classify_error,
                    log: self.log.clone(),
                })?
            }
            Protocol::Anthropic => {
                route_provider = "anthropic";
                let beta_api = model
                    .provider_options
                    .as_ref()
                    .and_then(|options| options.beta_api)
                    == Some(true);
                let base = create_anthropic_base(AnthropicRequesterOptions {
                    beta_api: Some(beta_api),
                    ..Default::default()
                });
                let trait_ = match custom.and_then(|definition| definition.adapter_trait.as_ref()) {
                    Some(ProtocolTrait::Anthropic(trait_)) => Some(trait_.clone()),
                    _ => None,
                };
                let connection = custom
                    .and_then(|definition| definition.connection.clone())
                    .or_else(|| custom.is_none().then(|| ANTHROPIC_CONNECTION.clone()));
                let classify_error = custom.and_then(|definition| definition.classify_error.clone());
                base.create_requester(ProtocolRequesterOptions {
                    connection,
                    trait_,
                    classify_error,
                    log: self.log.clone(),
                })?
            }
            Protocol::GoogleGenAI => {
                route_provider = "google_genai";
                let vertexai = model
                    .provider_options
                    .as_ref()
                    .and_then(|options| options.vertexai)
                    == Some(true);
                let base = create_google_gen_ai_base(None, vertexai);
                base.create_requester(ProtocolRequesterOptions {
                    connection: if vertexai {
                        Some(VERTEX_CONNECTION.clone())
                    } else {
                        Some(GEMINI_CONNECTION.clone())
                    },
                    trait_: None,
                    classify_error: None,
                    log: self.log.clone(),
                })?
            }
            Protocol::Antigravity => {
                route_provider = "antigravity";
                create_antigravity_base().create_requester(ProtocolRequesterOptions {
                    connection: None,
                    trait_: None,
                    classify_error: None,
                    log: self.log.clone(),
                })?
            }
        };
        let llm_model = LlmModel {
            provider: route_provider.to_string(),
            model: model.name.clone(),
            capability: to_llm_capability(&model.capabilities),
            max_context_size: (model.max_context_size > 0)
                .then(|| u64::from(model.max_context_size)),
            max_input_size: model.max_input_size.map(u64::from),
            support_efforts: model.support_efforts.clone(),
            default_effort: model.default_effort.clone(),
            off_effort: model
                .provider_options
                .as_ref()
                .and_then(|options| options.off_effort.clone()),
            always_thinking: model.always_thinking,
            adaptive_thinking: model
                .provider_options
                .as_ref()
                .and_then(|options| options.adaptive_thinking),
            connection: crate::human::llm::model::LlmConnection {
                base_url: model.base_url.clone(),
                api_key: None,
                default_headers: (!model.headers.is_empty()).then(|| model.headers.clone()),
                beta_api: None,
                vertexai: None,
            },
        };
        Ok(ResolvedLlmModel {
            requester,
            protocol: model.protocol,
            model: llm_model,
            media: if custom.is_some() { media } else { None },
        })
    }
}

fn openai_reasoning_trait_for(model: &Model) -> Option<OpenAITrait> {
    let reasoning_key = model
        .provider_options
        .as_ref()
        .and_then(|options| options.reasoning_key.clone())
        .or_else(|| model.reasoning_key.clone())?;
    Some(OpenAITrait {
        reasoning_key: Some(reasoning_key),
        ..Default::default()
    })
}

fn kimi_files(model: &LlmModel, media_fetch: Option<KimiFilesFetch>) -> KimiFiles {
    KimiFiles::new(KimiFilesOptions {
        api_key: model.connection.api_key.clone(),
        base_url: kimi_files_base_url(model),
        default_headers: model.connection.default_headers.clone(),
        fetch: media_fetch,
    })
}

fn kimi_media_contribution(media_fetch: Option<KimiFilesFetch>) -> ProviderMediaContribution {
    ProviderMediaContribution {
        inline_video: None,
        upload_video: Some(Arc::new({
            let media_fetch = media_fetch.clone();
            move |video, options| {
                let files = kimi_files(&options.model, media_fetch.clone());
                async move {
                    files
                        .upload_video(video, Some(KimiUploadOptions {
                            signal: options.signal,
                        }))
                        .await
                }
                .boxed()
            }
        })),
        upload_image: Some(Arc::new(move |image, options| {
            let files = kimi_files(&options.model, media_fetch.clone());
            async move {
                files
                    .upload_image(image, Some(KimiUploadOptions {
                        signal: options.signal,
                    }))
                    .await
            }
            .boxed()
        })),
    }
}

fn to_v2_capability(capability: &HumanModelCapability) -> ModelCapability {
    ModelCapability {
        image_in: capability.image_in,
        video_in: capability.video_in,
        audio_in: capability.audio_in,
        thinking: capability.thinking,
        tool_use: capability.tool_use,
        pdf_in: capability.pdf_in,
        max_context_tokens: 0,
        max_input_tokens: None,
        dynamically_loaded_tools: capability.dynamically_loaded_tools,
        unknown_capability: false,
    }
}

fn base_capability(base_id: ProtocolBaseId, model_name: &str) -> Option<HumanModelCapability> {
    match base_id {
        Protocol::OpenAI => OpenAIBase.capability(model_name),
        Protocol::OpenAIResponses => OpenAIResponsesBase.capability(model_name),
        Protocol::Anthropic => create_anthropic_base(Default::default()).capability(model_name),
        Protocol::GoogleGenAI => create_google_gen_ai_base(None, false).capability(model_name),
        Protocol::Antigravity => create_antigravity_base().capability(model_name),
    }
}

impl IProtocolAdapterRegistry for ProtocolAdapterRegistry {
    fn supported_protocols(&self) -> Vec<Protocol> {
        list_protocol_bases()
            .iter()
            .map(|definition| definition.id)
            .collect()
    }

    fn resolve_adapter_identity(
        &self,
        protocol: Protocol,
        provider_type: Option<&str>,
    ) -> ResolvedAdapterIdentity {
        let definition = provider_type.and_then(|provider_type| {
            get_provider_definition(provider_type, Some(protocol))
        });
        let base_id = definition
            .as_ref()
            .map(|definition| definition.base_protocol)
            .unwrap_or(protocol);
        let binding = definition
            .as_ref()
            .and_then(|definition| definition.binding.clone())
            .or_else(|| get_protocol_base(base_id).map(|base| base.binding.clone()))
            .expect("registered protocol base");
        ResolvedAdapterIdentity {
            base_id,
            trait_: definition.and_then(|definition| definition.trait_),
            binding,
        }
    }

    fn resolve_provider_base_id(
        &self,
        protocol: Protocol,
        provider_type: Option<&str>,
    ) -> ProtocolBaseId {
        provider_type
            .and_then(|provider_type| get_provider_definition(provider_type, Some(protocol)))
            .map(|definition| definition.base_protocol)
            .unwrap_or(protocol)
    }

    fn resolve_capability(
        &self,
        protocol: Protocol,
        model_name: &str,
        provider_type: Option<&str>,
        provider_id: Option<&str>,
    ) -> ModelCapability {
        let definition = resolve_provider_definition(provider_type, provider_id, Some(protocol));
        if let Some(capability) = definition
            .as_ref()
            .and_then(|definition| definition.capability.as_ref())
            .and_then(|capability| capability(model_name))
        {
            return to_v2_capability(&capability);
        }
        let base_id = definition
            .as_ref()
            .map(|definition| definition.base_protocol)
            .unwrap_or(protocol);
        base_capability(base_id, model_name)
            .map(|capability| to_v2_capability(&capability))
            .unwrap_or(UNKNOWN_CAPABILITY)
    }

    fn resolve(&self, model: &Model) -> Result<ResolvedLlmModel, WireAsyncError> {
        self.resolve_route(model)
    }
}

pub fn register_protocol_adapter_registry() {
    crate::_base::di::scope::register_scoped_service(
        crate::app::scopes::LifecycleScope::App,
        crate::_base::di::scope::ScopeActivation::OnScopeCreated,
        "provider",
    );
}
