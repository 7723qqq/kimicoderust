use std::collections::HashMap;
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use futures_util::{FutureExt, StreamExt};
use indexmap::IndexMap;
use kimi_core::primitives::errors::domains::config::CONFIG_PERSIST_BLOCKED;
use kimi_core::primitives::errors::{Error2, Error2Options};
use parking_lot::Mutex;
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::app::config::config::IConfigService;
use crate::app::kosong_config::config_section::DEFAULT_MODEL_SECTION;
use crate::human::credentials::credentials::{
    create_oauth_credential_provider, create_static_credential_provider,
};
use crate::human::llm::finish_reason::FinishReason;
use crate::human::llm::provider_catalog::{CatalogModel, CatalogProviderInfo};
use crate::human::llm::requester::requester::{
    LlmCredential, LlmCredentialProvider,
};
use crate::human::llm::usage::TokenUsage;
use crate::llm_adapter::contract::capability::{
    ModelCapability, is_unknown_capability, mark_unknown_capability,
};
use crate::llm_adapter::contract::errors::CONFIG_INVALID_ERROR_CODE;
use crate::llm_adapter::model::catalog::{
    IModelCatalog, Model, ModelCatalogItem, ModelPingResult, ProviderCatalogItem,
    ProviderCredentialState, SetDefaultModelResponse, to_protocol_model,
    to_protocol_model_fallback, to_protocol_provider,
};
use crate::llm_adapter::model::catalog_runtime::{IProviderCatalogRuntime, raw_record_of};
use crate::llm_adapter::model::credential_recovery::{
    run_with_credential_recovery, stream_with_credential_recovery,
};
use crate::llm_adapter::model::errors::{MODEL_NOT_FOUND, PROVIDER_NOT_FOUND};
use crate::llm_adapter::model::host_request_headers::IHostRequestHeaders;
use crate::llm_adapter::model::model::{IModelService, ModelRecord};
use crate::llm_adapter::model::model_auth::{
    DeclaredProviderCredential, ResolveModelAuthMaterialArgs, api_key_env_missing_message,
    declared_provider_credential, derive_provider_id, non_empty, resolve_endpoint_base_url,
    resolve_model_auth_material, resolve_model_protocol, with_anthropic_profile,
};
use crate::llm_adapter::model::model_oauth::{AccessTokenOptions, IModelOAuthTokens};
use crate::llm_adapter::model::model_requester::{
    ModelRequestEvent, ModelRequestInput, ModelRequestParams, ModelRequestStream, ModelRequester,
};
use crate::llm_adapter::model::model_requester_impl::{ModelRequesterImpl, RegistryGateway};
use crate::llm_adapter::model::model_types::ResolvedModelAuthMaterial;
use crate::llm_adapter::protocol::protocol::{
    IProtocolAdapterRegistry, Protocol, ProtocolProviderOptions,
};
use crate::llm_adapter::provider::provider::{IProviderService, OAuthStorage, ProviderConfig};
use crate::llm_adapter::provider::provider_definition::{
    HostHeaders, get_provider_definition, resolve_provider_endpoint,
};
use crate::wire::record::WireAsyncError;

#[derive(Clone)]
struct CatalogEntry {
    model: Model,
    requester: Arc<dyn ModelRequester>,
}

pub struct ModelCatalog {
    runtime: Arc<dyn IProviderCatalogRuntime>,
    providers: Arc<dyn IProviderService>,
    models: Arc<dyn IModelService>,
    oauth: Arc<dyn IModelOAuthTokens>,
    protocol_registry: Arc<dyn IProtocolAdapterRegistry>,
    host_request_headers: Arc<dyn IHostRequestHeaders>,
    config: Arc<dyn IConfigService>,
    cache: Mutex<HashMap<String, CatalogEntry>>,
    runtime_subscription: Mutex<Option<DisposableRef>>,
}

impl ModelCatalog {
    pub fn new(
        runtime: Arc<dyn IProviderCatalogRuntime>,
        providers: Arc<dyn IProviderService>,
        models: Arc<dyn IModelService>,
        oauth: Arc<dyn IModelOAuthTokens>,
        protocol_registry: Arc<dyn IProtocolAdapterRegistry>,
        host_request_headers: Arc<dyn IHostRequestHeaders>,
        config: Arc<dyn IConfigService>,
    ) -> Arc<Self> {
        let this = Arc::new(Self {
            runtime,
            providers,
            models,
            oauth,
            protocol_registry,
            host_request_headers,
            config,
            cache: Mutex::new(HashMap::new()),
            runtime_subscription: Mutex::new(None),
        });
        let subscription = this.runtime.on_changed({
            let weak = Arc::downgrade(&this);
            Arc::new(move |providers| {
                if let Some(this) = weak.upgrade() {
                    this.invalidate(&providers);
                }
            })
        });
        *this.runtime_subscription.lock() = Some(subscription);
        this
    }

    pub fn notify_config_changed(&self) {
        self.runtime.resync();
    }

    fn invalidate(&self, provider_ids: &[String]) {
        if provider_ids.is_empty() {
            return;
        }
        self.cache.lock().retain(|_, entry| {
            !provider_ids.contains(&entry.model.provider_name)
        });
    }

    fn entry(&self, id: &str) -> Result<CatalogEntry, WireAsyncError> {
        self.runtime.sync();
        if let Some(entry) = self.cache.lock().get(id).cloned() {
            return Ok(entry);
        }
        let model = self.build_model(id)?;
        let requester: Arc<dyn ModelRequester> = Arc::new(ModelRequesterImpl::new(
            model.clone(),
            Arc::new(RegistryGateway::new(self.protocol_registry.clone())),
        ));
        let entry = CatalogEntry { model, requester };
        self.cache.lock().insert(id.to_string(), entry.clone());
        Ok(entry)
    }

    fn all_records(&self) -> IndexMap<String, ModelRecord> {
        self.runtime
            .aliases()
            .into_iter()
            .filter_map(|alias| {
                self.runtime
                    .lookup(&alias)
                    .map(|definition| (alias, raw_record_of(&definition)))
            })
            .collect()
    }

    fn provider_type_of(&self, record: &ModelRecord) -> Option<String> {
        let default_provider = self.providers.get_default_provider();
        let provider_id = record
            .provider_id
            .as_deref()
            .or(record.provider.as_deref())
            .or(default_provider.as_deref());
        self.runtime
            .provider_info(provider_id.unwrap_or_default())
            .and_then(|provider| provider.type_)
            .or_else(|| record.protocol.map(|protocol| protocol.as_str().to_string()))
    }

    async fn to_catalog_provider(
        &self,
        provider_id: &str,
        provider: CatalogProviderInfo,
        models: &IndexMap<String, ModelRecord>,
        global_default_model: Option<&str>,
    ) -> ProviderCatalogItem {
        let provider = provider_config_from_catalog(&provider);
        let credential = self.resolve_credential(provider_id, &provider).await;
        to_protocol_provider(
            provider_id,
            &provider,
            models,
            global_default_model,
            &credential,
        )
    }

    async fn resolve_credential(
        &self,
        provider_id: &str,
        provider: &ProviderConfig,
    ) -> ProviderCredentialState {
        let declared = declared_provider_credential(provider, provider_id);
        let has_oauth_token = match provider.oauth.as_ref() {
            Some(oauth) => self.oauth.has_cached_access_token(provider_id, oauth).await,
            None => false,
        };
        ProviderCredentialState {
            has_api_key: has_configured_api_key(provider, Some(provider_id)),
            has_oauth_token,
            has_credential_conflict: matches!(declared, DeclaredProviderCredential::Conflict { .. }),
        }
    }

    fn resolve_provider_context(
        &self,
        id: &str,
        model: &ModelRecord,
    ) -> Result<(Option<ProviderConfig>, String, Option<String>), WireAsyncError> {
        let default_provider = self.providers.get_default_provider();
        let provider_id = model
            .provider_id
            .as_deref()
            .or(model.provider.as_deref())
            .or(default_provider.as_deref());
        if let Some(provider_id) = provider_id {
            let provider = self.runtime.provider_info(provider_id).ok_or_else(|| {
                Box::new(Error2::new(
                    CONFIG_INVALID_ERROR_CODE,
                    format!("Provider \"{provider_id}\" referenced by model \"{id}\" is not configured."),
                )) as WireAsyncError
            })?;
            let provider = provider_config_from_catalog(&provider);
            let base_url = resolve_endpoint_base_url(model, &provider, Some(provider_id));
            return Ok((Some(provider), provider_id.to_string(), base_url));
        }
        let base_url = non_empty(model.base_url.as_deref()).ok_or_else(|| {
            Box::new(Error2::new(
                CONFIG_INVALID_ERROR_CODE,
                format!("Model \"{id}\" must set either providerId or baseUrl in config.toml."),
            )) as WireAsyncError
        })?;
        Ok((None, derive_provider_id(&base_url), Some(base_url)))
    }

    fn build_credential_provider(
        &self,
        provider_name: &str,
        auth: ResolvedModelAuthMaterial,
    ) -> Arc<dyn LlmCredentialProvider> {
        if let Some(api_key) = auth.api_key {
            return create_static_credential_provider(Some(api_key));
        }
        if let Some(env_name) = auth.api_key_env {
            return Arc::new(EnvCredentialProvider {
                provider_name: provider_name.to_string(),
                env_name,
            });
        }
        if let Some(oauth_ref) = auth.oauth {
            let provider_key = auth
                .oauth_provider_key
                .unwrap_or_else(|| provider_name.to_string());
            let tokens = self.oauth.clone();
            return create_oauth_credential_provider(Arc::new(move |force| {
                let tokens = tokens.clone();
                let provider_key = provider_key.clone();
                let oauth_ref = oauth_ref.clone();
                async move {
                    tokens
                        .get_access_token(
                            &provider_key,
                            &oauth_ref,
                            Some(AccessTokenOptions { force }),
                        )
                        .await
                        .map(Some)
                        .map_err(|error| Box::new(error) as WireAsyncError)
                }
                .boxed()
            }));
        }
        create_static_credential_provider(None)
    }

    fn build_model(&self, id: &str) -> Result<Model, WireAsyncError> {
        let definition = self.runtime.lookup(id).ok_or_else(|| {
            Box::new(Error2::with_options(
                CONFIG_INVALID_ERROR_CODE,
                format!("Model \"{id}\" is not configured in config.toml."),
                Error2Options {
                    details: Some(Map::from_iter([("model".to_string(), id.into())])),
                    ..Default::default()
                },
            )) as WireAsyncError
        })?;
        let configured_model = raw_record_of(&definition);
        let (provider_config, provider_name, raw_base_url) =
            self.resolve_provider_context(id, &configured_model)?;
        let protocol = resolve_model_protocol(&configured_model, provider_config.as_ref())
            .ok_or_else(|| {
                Box::new(Error2::new(
                    CONFIG_INVALID_ERROR_CODE,
                    format!("Model \"{id}\" must declare a wire protocol (config: models.<id>.protocol)."),
                )) as WireAsyncError
            })?;
        let provider_type_for_profile = provider_config
            .as_ref()
            .and_then(|provider| provider.type_.as_deref())
            .or_else(|| configured_model.protocol.map(|protocol| protocol.as_str()));
        let model_record = with_anthropic_profile(
            &effective_record_of(&definition),
            provider_type_for_profile,
        );
        let wire_name = model_record
            .name
            .clone()
            .or_else(|| model_record.model.clone());
        let auth = resolve_model_auth_material(ResolveModelAuthMaterialArgs {
            model_id: id,
            model: &model_record,
            provider: provider_config.as_ref(),
            provider_name: &provider_name,
        })?;
        let credential_provider = self.build_credential_provider(&provider_name, auth);
        let provider_type = provider_config
            .as_ref()
            .and_then(|provider| provider.type_.clone())
            .unwrap_or_else(|| protocol.as_str().to_string());
        let resolved_base_url = if protocol == Protocol::Anthropic {
            raw_base_url.map(|base_url| strip_trailing_v1(&base_url))
        } else {
            raw_base_url
        };
        let wire_name = wire_name.ok_or_else(|| {
            Box::new(Error2::new(
                CONFIG_INVALID_ERROR_CODE,
                format!("Model \"{id}\" must define a wire-facing name in config.toml."),
            )) as WireAsyncError
        })?;
        if model_record.max_context_size.is_none() {
            return Err(Box::new(Error2::new(
                CONFIG_INVALID_ERROR_CODE,
                format!("Model \"{id}\" must define a positive max_context_size in config.toml."),
            )));
        }
        let detected = self.protocol_registry.resolve_capability(
            protocol,
            &wire_name,
            Some(&provider_type),
            Some(&provider_name),
        );
        let capabilities = resolve_model_capabilities(
            model_record.capabilities.as_deref(),
            &detected,
            model_record.max_context_size.unwrap(),
            model_record.max_input_size,
        );
        let provider_options = build_protocol_provider_options(
            &model_record,
            protocol,
            provider_config.as_ref(),
            resolved_base_url.as_deref(),
        );
        let declared = model_record
            .capabilities
            .unwrap_or_default()
            .iter()
            .map(|capability| js_trim(capability).to_lowercase())
            .collect::<Vec<_>>();
        let headers = resolve_outbound_headers(
            provider_config.as_ref().and_then(|provider| provider.type_.as_deref()),
            provider_config
                .as_ref()
                .and_then(|provider| provider.custom_headers.as_ref()),
            self.host_request_headers.as_ref(),
        );
        Ok(Model {
            id: id.to_string(),
            name: wire_name,
            aliases: model_record.aliases.unwrap_or_default(),
            protocol,
            base_url: resolved_base_url,
            headers,
            capabilities,
            max_context_size: model_record.max_context_size.unwrap(),
            max_input_size: model_record.max_input_size,
            max_output_size: model_record.max_output_size,
            display_name: model_record.display_name,
            reasoning_key: model_record.reasoning_key,
            support_efforts: model_record.support_efforts,
            default_effort: model_record.default_effort,
            always_thinking: Some(declared.iter().any(|capability| capability == "always_thinking")),
            adaptive_thinking: model_record.adaptive_thinking,
            provider_type: Some(provider_type),
            provider_name,
            credential_provider: Some(credential_provider),
            provider_options,
        })
    }
}

struct EnvCredentialProvider {
    provider_name: String,
    env_name: String,
}

#[async_trait]
impl LlmCredentialProvider for EnvCredentialProvider {
    async fn resolve(&self) -> Result<Option<LlmCredential>, WireAsyncError> {
        let api_key = std::env::var(&self.env_name)
            .ok()
            .and_then(|value| non_empty(Some(&value)))
            .ok_or_else(|| {
                Box::new(Error2::new(
                    CONFIG_INVALID_ERROR_CODE,
                    api_key_env_missing_message(&self.provider_name, &self.env_name),
                )) as WireAsyncError
            })?;
        Ok(Some(LlmCredential {
            api_key: Some(api_key),
            headers: None,
        }))
    }
}

fn provider_config_from_catalog(provider: &CatalogProviderInfo) -> ProviderConfig {
    ProviderConfig {
        model_source: provider.model_source.as_deref().and_then(|source| {
            use crate::app::kosong_config::config_section::ModelSource;
            match source {
                "static" => Some(ModelSource::Static),
                "discover" => Some(ModelSource::Discover),
                "oauth-catalog" => Some(ModelSource::OauthCatalog),
                _ => None,
            }
        }),
        base_url: provider.base_url.clone(),
        custom_headers: provider.custom_headers.as_ref().map(|headers| {
            headers
                .iter()
                .filter_map(|(key, value)| {
                    value.as_str().map(|value| (key.clone(), value.to_string()))
                })
                .collect()
        }),
        default_model: provider.default_model.clone(),
        type_: provider.type_.clone(),
        api_key: provider.api_key.clone(),
        api_key_env: provider.api_key_env.clone(),
        oauth: provider.oauth.as_ref().map(|oauth| {
            crate::llm_adapter::provider::provider::OAuthRef {
                storage: if oauth.storage == "keyring" {
                    OAuthStorage::Keyring
                } else {
                    OAuthStorage::File
                },
                key: oauth.key.clone(),
                oauth_host: oauth.oauth_host.clone(),
            }
        }),
        env: provider.env.as_ref().map(|env| {
            env.iter()
                .filter_map(|(key, value)| {
                    value.as_str().map(|value| (key.clone(), value.to_string()))
                })
                .collect()
        }),
        source: provider.source.clone(),
    }
}

#[async_trait]
impl IModelCatalog for ModelCatalog {
    fn get(&self, id: &str) -> Result<Model, WireAsyncError> {
        Ok(self.entry(id)?.model)
    }

    fn get_requester(&self, id: &str) -> Result<Arc<dyn ModelRequester>, WireAsyncError> {
        Ok(self.entry(id)?.requester)
    }

    fn generate(
        &self,
        id: &str,
        input: ModelRequestInput,
        signal: Option<CancellationToken>,
        params: Option<ModelRequestParams>,
    ) -> Result<ModelRequestStream, WireAsyncError> {
        let entry = self.entry(id)?;
        let requester = entry.requester.clone();
        let recovery_signal = signal.clone();
        Ok(stream_with_credential_recovery(
            entry.model.credential_provider.clone(),
            move || {
                requester.request(input.clone(), signal.clone(), params.clone())
            },
            recovery_signal,
        )
        .boxed())
    }

    async fn ping(&self, id: &str) -> ModelPingResult {
        let started_at = std::time::Instant::now();
        let result = match self.entry(id) {
            Ok(entry) => {
                let requester = entry.requester.clone();
                run_with_credential_recovery(
                    entry.model.credential_provider.as_ref(),
                    || {
                        let requester = requester.clone();
                        async move { consume_ping(requester).await }
                    },
                    None,
                )
                .await
            }
            Err(error) => Err(error),
        };
        let duration_ms = started_at.elapsed().as_millis() as u64;
        match result {
            Ok(result) => ModelPingResult {
                ok: true,
                duration_ms,
                text: Some(js_trim(&result.text).to_string()),
                finish_reason: result.finish_reason,
                usage: result.usage,
                error: None,
            },
            Err(error) => ModelPingResult {
                ok: false,
                duration_ms,
                text: None,
                finish_reason: None,
                usage: None,
                error: Some(error.to_string()),
            },
        }
    }

    fn find_by_name(&self, name: &str) -> Vec<String> {
        self.runtime
            .aliases()
            .into_iter()
            .filter(|alias| {
                let Some(definition) = self.runtime.lookup(alias) else {
                    return false;
                };
                let record = raw_record_of(&definition);
                record.name.as_deref() == Some(name)
                    || record.model.as_deref() == Some(name)
                    || record
                        .aliases
                        .as_ref()
                        .is_some_and(|aliases| aliases.iter().any(|alias| alias == name))
            })
            .collect()
    }

    async fn list_models(&self) -> Result<Vec<ModelCatalogItem>, WireAsyncError> {
        let mut output = Vec::new();
        for model_id in self.runtime.aliases() {
            let Some(definition) = self.runtime.lookup(&model_id) else {
                continue;
            };
            let record = raw_record_of(&definition);
            let provider_type = self.provider_type_of(&record);
            match self.get(&model_id) {
                Ok(model) => output.push(to_protocol_model(
                    &model,
                    &record,
                    provider_type.as_deref(),
                )),
                Err(_) => output.push(to_protocol_model_fallback(
                    &model_id,
                    &record,
                    provider_type.as_deref(),
                )),
            }
        }
        Ok(output)
    }

    async fn list_providers(&self) -> Result<Vec<ProviderCatalogItem>, WireAsyncError> {
        let records = self.all_records();
        let default_model = self.models.get_default_model();
        let mut output = Vec::new();
        for provider_id in self.runtime.provider_ids() {
            let Some(provider) = self.runtime.provider_info(&provider_id) else {
                continue;
            };
            output.push(
                self.to_catalog_provider(
                    &provider_id,
                    provider,
                    &records,
                    default_model.as_deref(),
                )
                .await,
            );
        }
        Ok(output)
    }

    async fn get_provider(&self, provider_id: &str) -> Result<ProviderCatalogItem, WireAsyncError> {
        let provider = self.runtime.provider_info(provider_id).ok_or_else(|| {
            Box::new(Error2::new(
                PROVIDER_NOT_FOUND,
                format!("provider {provider_id} does not exist"),
            )) as WireAsyncError
        })?;
        let records = self.all_records();
        let default_model = self.models.get_default_model();
        Ok(self
            .to_catalog_provider(provider_id, provider, &records, default_model.as_deref())
            .await)
    }

    async fn set_default_model(
        &self,
        model_id: &str,
    ) -> Result<SetDefaultModelResponse, WireAsyncError> {
        let definition = self.runtime.lookup(model_id).ok_or_else(|| {
            Box::new(Error2::new(
                MODEL_NOT_FOUND,
                format!("model {model_id} does not exist"),
            )) as WireAsyncError
        })?;
        let record = raw_record_of(&definition);
        let model = self.get(model_id)?;
        self.models
            .set_default_model(Some(model_id.to_string()))
            .await?;
        let persisted = self
            .config
            .inspect(DEFAULT_MODEL_SECTION)
            .user_value
            .and_then(|value| value.as_str().map(ToString::to_string));
        if persisted.as_deref() != Some(model_id) {
            return Err(Box::new(Error2::new(
                CONFIG_PERSIST_BLOCKED,
                format!(
                    "Refusing to report success: default_model was not persisted (config still says {}).",
                    persisted.as_deref().unwrap_or("nothing")
                ),
            )));
        }
        let provider_type = self.provider_type_of(&record);
        Ok(SetDefaultModelResponse {
            default_model: model_id.to_string(),
            model: to_protocol_model(&model, &record, provider_type.as_deref()),
        })
    }
}

struct PingConsumption {
    text: String,
    usage: Option<TokenUsage>,
    finish_reason: Option<String>,
}

async fn consume_ping(
    requester: Arc<dyn ModelRequester>,
) -> Result<PingConsumption, WireAsyncError> {
    let input = ModelRequestInput {
        system_prompt: "You are a connectivity probe. Answer with the single word \"pong\"."
            .to_string(),
        tools: Vec::new(),
        messages: vec![crate::llm_adapter::contract::message::create_user_message("ping")],
        response_format: None,
    };
    let params = ModelRequestParams {
        max_completion_tokens: Some(512),
        ..Default::default()
    };
    let mut stream = requester.request(input, None, Some(params));
    let mut text = String::new();
    let mut usage = None;
    let mut finish_reason = None;
    while let Some(event) = stream.next().await {
        match event? {
            ModelRequestEvent::Part {
                part: crate::llm_adapter::contract::message::StreamedMessagePart::Content(
                    crate::llm_adapter::contract::message::ContentPart::Text { text: part, .. },
                ),
            } => text.push_str(&part),
            ModelRequestEvent::Usage { usage: next, .. } => usage = Some(next),
            ModelRequestEvent::Finish {
                provider_finish_reason,
                raw_finish_reason,
                ..
            } => {
                finish_reason = provider_finish_reason
                    .map(finish_reason_name)
                    .or(raw_finish_reason);
            }
            _ => {}
        }
    }
    Ok(PingConsumption {
        text,
        usage,
        finish_reason,
    })
}

fn finish_reason_name(reason: FinishReason) -> String {
    match reason {
        FinishReason::Completed => "completed",
        FinishReason::ToolCalls => "tool_calls",
        FinishReason::Truncated => "truncated",
        FinishReason::Filtered => "filtered",
        FinishReason::Paused => "paused",
        FinishReason::Error => "error",
        FinishReason::Other => "other",
    }
    .to_string()
}

pub fn resolve_outbound_headers(
    provider_type: Option<&str>,
    custom_headers: Option<&HashMap<String, String>>,
    host: &dyn IHostRequestHeaders,
) -> HashMap<String, String> {
    let forwards_all = provider_type
        .and_then(|provider_type| get_provider_definition(provider_type, None))
        .is_some_and(|definition| definition.host_headers == Some(HostHeaders::Full));
    let host_layer = if forwards_all {
        host.headers()
    } else {
        host.third_party_headers()
    };
    let mut headers = parse_kimi_code_custom_headers();
    headers.extend(host_layer);
    if let Some(custom_headers) = custom_headers {
        headers.extend(custom_headers.clone());
    }
    headers
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

fn parse_kimi_code_custom_headers() -> HashMap<String, String> {
    let Ok(raw) = std::env::var("KIMI_CODE_CUSTOM_HEADERS") else {
        return HashMap::new();
    };
    let raw = js_trim(&raw);
    if raw.is_empty() {
        return HashMap::new();
    }
    raw.lines()
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            let name = js_trim(name);
            (!name.is_empty()).then(|| (name.to_string(), js_trim(value).to_string()))
        })
        .collect()
}

fn resolve_model_capabilities(
    declared_capabilities: Option<&[String]>,
    detected: &ModelCapability,
    max_context_size: u32,
    max_input_size: Option<u32>,
) -> ModelCapability {
    let declared = declared_capabilities
        .unwrap_or_default()
        .iter()
        .map(|capability| js_trim(capability).to_lowercase())
        .collect::<Vec<_>>();
    let has = |capability: &str| declared.iter().any(|candidate| candidate == capability);
    let capabilities = ModelCapability {
        image_in: has("image_in") || detected.image_in,
        video_in: has("video_in") || detected.video_in,
        audio_in: has("audio_in") || detected.audio_in,
        thinking: has("thinking") || has("always_thinking") || detected.thinking,
        tool_use: has("tool_use") || detected.tool_use,
        pdf_in: Some(has("pdf_in") || detected.pdf_in == Some(true)),
        max_context_tokens: max_context_size,
        max_input_tokens: max_input_size,
        dynamically_loaded_tools: Some(
            has("dynamically_loaded_tools") || detected.dynamically_loaded_tools == Some(true),
        ),
        unknown_capability: false,
    };
    if is_unknown_capability(detected)
        && !has("image_in")
        && !has("video_in")
        && !has("audio_in")
        && !has("pdf_in")
    {
        return mark_unknown_capability(&capabilities);
    }
    capabilities
}

fn strip_trailing_v1(base_url: &str) -> String {
    base_url
        .strip_suffix("/v1/")
        .or_else(|| base_url.strip_suffix("/v1"))
        .unwrap_or(base_url)
        .to_string()
}

fn effective_record_of(definition: &CatalogModel) -> ModelRecord {
    let raw = raw_record_of(definition);
    let mut record = raw.clone();
    record.capabilities = raw
        .overrides
        .as_ref()
        .and_then(|overrides| overrides.capabilities.clone())
        .or(raw.capabilities);
    record.max_context_size = definition.max_context_size.and_then(|value| u32::try_from(value).ok());
    record.max_input_size = definition.max_input_size.and_then(|value| u32::try_from(value).ok());
    record.max_output_size = definition.max_output_size.and_then(|value| u32::try_from(value).ok());
    record.display_name = definition.display_name.clone();
    record.reasoning_key = definition.reasoning_key.clone();
    record.adaptive_thinking = definition.adaptive_thinking;
    record.support_efforts = definition.support_efforts.clone();
    record.default_effort = definition.default_effort.clone();
    record.off_effort = definition.off_effort.clone();
    record.overrides = None;
    record
}

fn build_protocol_provider_options(
    model: &ModelRecord,
    protocol: Protocol,
    provider: Option<&ProviderConfig>,
    base_url: Option<&str>,
) -> Option<ProtocolProviderOptions> {
    let mut options = ProtocolProviderOptions::default();
    match protocol {
        Protocol::Anthropic => {
            options.default_max_tokens = model.max_output_size;
            options.support_efforts = model.support_efforts.clone();
            options.adaptive_thinking = model.adaptive_thinking;
            options.beta_api = model.beta_api;
        }
        Protocol::OpenAI => {
            options.reasoning_key = non_empty(model.reasoning_key.as_deref());
            options.off_effort = model.off_effort.clone();
        }
        Protocol::GoogleGenAI => {
            let project = vertex_ai_project(provider);
            let location = vertex_ai_location(provider, base_url);
            if let (Some(project), Some(location)) = (project.clone(), location.clone()) {
                options.vertexai = Some(true);
                options.project = Some(project);
                options.location = Some(location);
            } else if provider.and_then(|provider| provider.type_.as_deref()) == Some("vertexai") {
                options.vertexai = Some(true);
                options.project = project;
                options.location = location;
            }
        }
        Protocol::Antigravity => {
            options.adaptive_thinking = model.adaptive_thinking;
        }
        Protocol::OpenAIResponses => {
            options.off_effort = model.off_effort.clone();
        }
    }
    (!options.is_empty()).then_some(options)
}

fn vertex_ai_project(provider: Option<&ProviderConfig>) -> Option<String> {
    env_value(provider.and_then(|provider| provider.env.as_ref()), "GOOGLE_CLOUD_PROJECT")
}

fn vertex_ai_location(provider: Option<&ProviderConfig>, base_url: Option<&str>) -> Option<String> {
    env_value(provider.and_then(|provider| provider.env.as_ref()), "GOOGLE_CLOUD_LOCATION")
        .or_else(|| location_from_vertex_ai_base_url(base_url))
}

fn env_value(env: Option<&HashMap<String, String>>, key: &str) -> Option<String> {
    non_empty(env.and_then(|env| env.get(key)).map(String::as_str))
}

fn location_from_vertex_ai_base_url(base_url: Option<&str>) -> Option<String> {
    let url = non_empty(base_url)?;
    let host = url::Url::parse(&url).ok()?.host_str()?.to_string();
    let suffix = "-aiplatform.googleapis.com";
    host.strip_suffix(suffix)
        .and_then(|location| non_empty(Some(location)))
}

fn has_configured_api_key(provider: &ProviderConfig, provider_id: Option<&str>) -> bool {
    if non_empty(provider.api_key.as_deref()).is_some() {
        return true;
    }
    if let Some(api_key_env) = non_empty(provider.api_key_env.as_deref()) {
        return std::env::var(api_key_env)
            .ok()
            .and_then(|value| non_empty(Some(&value)))
            .is_some();
    }
    let Some(provider_type) = provider.type_.as_deref() else {
        return false;
    };
    let endpoint_type = if provider_type == "vertexai" {
        "google-genai"
    } else {
        provider_type
    };
    let empty = HashMap::new();
    resolve_provider_endpoint(
        endpoint_type,
        provider.env.as_ref().unwrap_or(&empty),
        provider_id,
    )
    .api_key
    .is_some()
}
