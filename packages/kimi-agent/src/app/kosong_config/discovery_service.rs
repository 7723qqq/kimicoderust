use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use kimi_core::primitives::errors::{
    Error2, Error2Options, domains::auth::AUTH_TOKEN_MISSING, domains::core::INTERNAL,
    domains::model_catalog::PROVIDER_NOT_FOUND,
};
use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::app::agent_identity::agent_identity::IAgentIdentity;

use super::config_section::{
    DEFAULT_MODEL_SECTION, MODELS_SECTION, ModelRecord, ModelSource, ModelsSection,
    PROVIDERS_SECTION, ProviderConfig, ProvidersSection, THINKING_SECTION, ThinkingConfig,
};
use super::discovery::{
    Dispose, IProviderDiscoveryService, ModelCatalogChanged, ProviderRefreshChange,
    ProviderRefreshFailure, RefreshProviderModelsOptions, RefreshProviderModelsResponse,
    RetiredDefault, RetiredModel, RetiredModelListener,
};
use super::kosong_config::KosongConfigStore;
use super::kosong_config_service::KosongProviderService;
use super::oauth_token_adapter::OAuthAccessTokenService;

#[derive(Debug, Clone, Default)]
pub struct ManagedKimiConfigShape {
    pub providers: ProvidersSection,
    pub models: ModelsSection,
    pub default_model: Option<String>,
    pub thinking: Option<ThinkingConfig>,
}

#[derive(Debug, Clone, Default)]
pub enum PatchField<T> {
    #[default]
    Missing,
    Value(Option<T>),
}

impl<T> PatchField<T> {
    pub fn is_present(&self) -> bool {
        matches!(self, Self::Value(_))
    }
}

#[derive(Debug, Clone, Default)]
pub struct ManagedKimiConfigPatch {
    pub providers: Option<ProvidersSection>,
    pub models: Option<ModelsSection>,
    pub default_model: PatchField<String>,
    pub thinking: PatchField<ThinkingConfig>,
}

#[derive(Debug, Clone)]
pub struct RefreshProviderChange {
    pub provider_id: String,
    pub provider_name: String,
    pub added: u64,
    pub removed: u64,
}

#[derive(Debug, Clone)]
pub struct RefreshProviderFailure {
    pub provider: String,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct RefreshRetiredDefault {
    pub from: String,
    pub to: String,
    pub from_model: ModelRecord,
}

#[derive(Debug, Clone, Default)]
pub struct RefreshResult {
    pub changed: Vec<RefreshProviderChange>,
    pub unchanged: Vec<String>,
    pub failed: Vec<RefreshProviderFailure>,
    pub retired_default: Option<RefreshRetiredDefault>,
}

#[async_trait]
pub trait RefreshProviderHost: Send + Sync {
    async fn get_config(&self) -> Result<ManagedKimiConfigShape, Error2>;
    async fn remove_provider(&self, provider_id: &str) -> Result<ManagedKimiConfigShape, Error2>;
    async fn set_config(&self, patch: ManagedKimiConfigPatch) -> Result<ManagedKimiConfigShape, Error2>;
    async fn resolve_oauth_token(
        &self,
        provider_name: &str,
        oauth_ref: Option<&super::config_section::OAuthRef>,
    ) -> Result<String, Error2>;
    fn user_agent(&self) -> String;
}

#[async_trait]
pub trait ProviderModelsRefreshBackend: Send + Sync {
    async fn refresh_provider_models(
        &self,
        host: Arc<dyn RefreshProviderHost>,
        options: RefreshProviderModelsOptions,
    ) -> Result<RefreshResult, Error2>;
}

pub trait ModelCatalogEventPublisher: Send + Sync {
    fn publish(&self, event: ModelCatalogChanged);
}

#[derive(Clone, Default)]
struct StaticExclusion {
    providers: ProvidersSection,
    models: ModelsSection,
    default_model: Option<String>,
    thinking: Option<ThinkingConfig>,
}

pub struct ProviderDiscoveryService {
    provider_service: Arc<dyn KosongProviderService>,
    config: Arc<dyn KosongConfigStore>,
    oauth: Arc<dyn OAuthAccessTokenService>,
    events: Arc<dyn ModelCatalogEventPublisher>,
    identity: Arc<dyn IAgentIdentity>,
    backend: Arc<dyn ProviderModelsRefreshBackend>,
    refresh_chain: tokio::sync::Mutex<()>,
    retired_models: Mutex<HashMap<String, String>>,
    retired_listeners: Mutex<BTreeMap<u64, RetiredModelListener>>,
    next_listener_id: AtomicU64,
    self_weak: Weak<ProviderDiscoveryService>,
}

impl ProviderDiscoveryService {
    pub fn new(
        provider_service: Arc<dyn KosongProviderService>,
        config: Arc<dyn KosongConfigStore>,
        oauth: Arc<dyn OAuthAccessTokenService>,
        events: Arc<dyn ModelCatalogEventPublisher>,
        identity: Arc<dyn IAgentIdentity>,
        backend: Arc<dyn ProviderModelsRefreshBackend>,
    ) -> Arc<Self> {
        Arc::new_cyclic(|weak| Self {
            provider_service,
            config,
            oauth,
            events,
            identity,
            backend,
            refresh_chain: tokio::sync::Mutex::new(()),
            retired_models: Mutex::new(HashMap::new()),
            retired_listeners: Mutex::new(BTreeMap::new()),
            next_listener_id: AtomicU64::new(0),
            self_weak: weak.clone(),
        })
    }

    async fn do_refresh_provider_models(
        self: &Arc<Self>,
        options: RefreshProviderModelsOptions,
    ) -> Result<RefreshProviderModelsResponse, Error2> {
        self.config.reload().await?;
        if let Some(provider_id) = options.provider_id.as_deref() {
            let provider = self.provider_service.get(provider_id).ok_or_else(|| {
                Error2::new(
                    PROVIDER_NOT_FOUND,
                    format!("provider {provider_id} does not exist"),
                )
            })?;
            if effective_model_source(&provider) == Some(ModelSource::Static) {
                return Ok(RefreshProviderModelsResponse {
                    changed: Vec::new(),
                    unchanged: vec![provider_id.to_string()],
                    failed: Vec::new(),
                    retired_default: None,
                });
            }
        }

        let exclusion = self.compute_static_exclusion();
        let identity = self
            .identity
            .resolved()
            .await
            .map_err(|error| Error2::new(INTERNAL, error.to_string()))?;
        let host = Arc::new(DiscoveryRefreshHost {
            service: self.clone(),
            exclusion,
            user_agent: identity.outbound_user_agent,
        });
        let result = self.backend.refresh_provider_models(host, options).await?;
        if let Some(retired) = &result.retired_default {
            let mut retired_models = self.retired_models.lock();
            let aliases: Vec<String> = retired_models
                .iter()
                .filter(|(_, replacement)| *replacement == &retired.from)
                .map(|(alias, _)| alias.clone())
                .collect();
            for alias in aliases {
                retired_models.insert(alias, retired.to.clone());
            }
            retired_models.insert(retired.from.clone(), retired.to.clone());
            drop(retired_models);
            let event = RetiredModel {
                from: retired.from.clone(),
                to: retired.to.clone(),
            };
            for listener in self.retired_listeners.lock().values() {
                listener(event.clone());
            }
        }
        let response = map_refresh_result(result);
        if !response.changed.is_empty() {
            self.events
                .publish(ModelCatalogChanged::new(response.clone()));
        }
        Ok(response)
    }

    fn compute_static_exclusion(&self) -> StaticExclusion {
        let providers = decode_providers(
            self.config.inspect(PROVIDERS_SECTION).user_value,
        );
        let static_ids: Vec<String> = providers
            .iter()
            .filter(|(_, provider)| effective_model_source(provider) == Some(ModelSource::Static))
            .map(|(id, _)| id.clone())
            .collect();
        if static_ids.is_empty() {
            return StaticExclusion::default();
        }
        let excluded_providers = providers
            .iter()
            .filter(|(id, _)| static_ids.contains(id))
            .map(|(id, provider)| (id.clone(), provider.clone()))
            .collect();
        let models = decode_models(self.config.inspect(MODELS_SECTION).user_value);
        let excluded_models: ModelsSection = models
            .iter()
            .filter(|(_, record)| {
                record.provider.as_ref().is_some_and(|provider| {
                    excluded_providers.contains_key(provider)
                })
            })
            .map(|(id, record)| (id.clone(), record.clone()))
            .collect();
        let default_model = config_inspect_string(&*self.config, DEFAULT_MODEL_SECTION);
        let thinking = self
            .config
            .inspect(THINKING_SECTION)
            .user_value
            .and_then(|value| serde_json::from_value::<ThinkingConfig>(value).ok());
        let default_is_excluded = default_model
            .as_ref()
            .is_some_and(|default| excluded_models.contains_key(default));
        StaticExclusion {
            providers: excluded_providers,
            models: excluded_models,
            default_model: default_is_excluded.then(|| default_model.clone()).flatten(),
            thinking: default_is_excluded.then_some(thinking).flatten(),
        }
    }

    fn read_user_config_shape(&self, exclusion: &StaticExclusion) -> ManagedKimiConfigShape {
        let providers = decode_providers(self.config.inspect(PROVIDERS_SECTION).user_value);
        let models = decode_models(self.config.inspect(MODELS_SECTION).user_value);
        let default_model = config_inspect_string(&*self.config, DEFAULT_MODEL_SECTION);
        let thinking = self
            .config
            .inspect(THINKING_SECTION)
            .user_value
            .and_then(|value| serde_json::from_value::<ThinkingConfig>(value).ok());
        let mut visible_models = without_keys(&models, &exclusion.models);
        if let Some(excluded_default) = &exclusion.default_model
            && let Some(record) = models.get(excluded_default)
        {
            visible_models.insert(excluded_default.clone(), record.clone());
        }
        ManagedKimiConfigShape {
            providers: without_keys(&providers, &exclusion.providers),
            models: visible_models,
            default_model,
            thinking,
        }
    }

    fn shape_without_provider(&self, provider_id: &str) -> ManagedKimiConfigShape {
        let ManagedKimiConfigShape {
            providers,
            models,
            default_model,
            thinking,
        } = self.read_user_config_shape(&StaticExclusion::default());
        let providers = providers
            .into_iter()
            .filter(|(id, _)| id != provider_id)
            .collect();
        let models = models
            .into_iter()
            .filter(|(_, record)| record.provider.as_deref() != Some(provider_id))
            .collect();
        ManagedKimiConfigShape {
            providers,
            models,
            default_model,
            thinking,
        }
    }

    async fn apply_refresh_patch(
        &self,
        patch: ManagedKimiConfigPatch,
        exclusion: &StaticExclusion,
    ) -> Result<ManagedKimiConfigShape, Error2> {
        let user_providers = decode_providers(self.config.inspect(PROVIDERS_SECTION).user_value);
        let user_models = decode_models(self.config.inspect(MODELS_SECTION).user_value);
        let mut sections = BTreeMap::new();
        let merged_providers = patch.providers.as_ref().map(|providers| {
            merge_sections(&exclusion.providers, providers)
        });
        let merged_models = patch.models.as_ref().map(|models| merge_sections(&exclusion.models, models));
        if let Some(providers) = &merged_providers {
            sections.insert(
                PROVIDERS_SECTION.to_string(),
                Some(serde_json::to_value(providers).unwrap_or_else(|_| json!({}))),
            );
        }
        if let Some(models) = &merged_models {
            sections.insert(
                MODELS_SECTION.to_string(),
                Some(serde_json::to_value(models).unwrap_or_else(|_| json!({}))),
            );
        }
        let restore_default = exclusion.default_model.is_some();
        let next_default = match &patch.default_model {
            PatchField::Missing => None,
            PatchField::Value(value) => Some(if restore_default {
                exclusion.default_model.clone()
            } else {
                value.clone()
            }),
        };
        if let Some(default_model) = &next_default {
            sections.insert(
                DEFAULT_MODEL_SECTION.to_string(),
                default_model.clone().map(Value::String),
            );
        }
        let next_thinking = match &patch.thinking {
            PatchField::Missing => None,
            PatchField::Value(value) => Some(if restore_default {
                exclusion.thinking.clone()
            } else {
                value.clone()
            }),
        };
        if let Some(thinking) = &next_thinking {
            sections.insert(
                THINKING_SECTION.to_string(),
                thinking
                    .as_ref()
                    .map(|thinking| serde_json::to_value(thinking).unwrap_or(Value::Null)),
            );
        }
        self.config.replace_sections(sections, None).await?;
        Ok(ManagedKimiConfigShape {
            providers: merged_providers.unwrap_or(user_providers),
            models: merged_models.unwrap_or(user_models),
            default_model: match next_default {
                Some(value) => value,
                None => config_inspect_string(&*self.config, DEFAULT_MODEL_SECTION),
            },
            thinking: match next_thinking {
                Some(value) => value,
                None => self
                    .config
                    .inspect(THINKING_SECTION)
                    .user_value
                    .and_then(|value| serde_json::from_value(value).ok()),
            },
        })
    }

    async fn resolve_oauth_token(
        &self,
        provider_name: &str,
        oauth_ref: Option<&super::config_section::OAuthRef>,
    ) -> Result<String, Error2> {
        let token_provider = self
            .oauth
            .resolve_token_provider(provider_name, oauth_ref)
            .ok_or_else(|| {
                let mut details = serde_json::Map::new();
                details.insert("provider_id".to_string(), Value::String(provider_name.to_string()));
                Error2::with_options(
                    AUTH_TOKEN_MISSING,
                    "OAuth token provider is not configured.",
                    Error2Options {
                        details: Some(details),
                        ..Default::default()
                    },
                )
            })?;
        token_provider.get_access_token(None).await
    }
}

#[async_trait]
impl IProviderDiscoveryService for ProviderDiscoveryService {
    fn on_did_retire_model(&self, listener: RetiredModelListener) -> Dispose {
        let id = self.next_listener_id.fetch_add(1, Ordering::Relaxed);
        self.retired_listeners.lock().insert(id, listener);
        let service = self.self_weak.clone();
        Box::new(move || {
            if let Some(service) = service.upgrade() {
                service.retired_listeners.lock().remove(&id);
            }
        })
    }

    async fn refresh_provider_models(
        &self,
        options: RefreshProviderModelsOptions,
    ) -> Result<RefreshProviderModelsResponse, Error2> {
        let _guard = self.refresh_chain.lock().await;
        let service = self
            .self_weak
            .upgrade()
            .ok_or_else(|| Error2::new(INTERNAL, "provider discovery service is no longer available"))?;
        service.do_refresh_provider_models(options).await
    }

    fn retired_model_replacement(&self, alias: &str) -> Option<String> {
        let models = decode_models(self.config.get(MODELS_SECTION));
        if models.contains_key(alias) {
            return None;
        }
        self.retired_models
            .lock()
            .get(alias)
            .filter(|replacement| models.contains_key(*replacement))
            .cloned()
    }
}

struct DiscoveryRefreshHost {
    service: Arc<ProviderDiscoveryService>,
    exclusion: StaticExclusion,
    user_agent: String,
}

#[async_trait]
impl RefreshProviderHost for DiscoveryRefreshHost {
    async fn get_config(&self) -> Result<ManagedKimiConfigShape, Error2> {
        Ok(self.service.read_user_config_shape(&self.exclusion))
    }

    async fn remove_provider(&self, provider_id: &str) -> Result<ManagedKimiConfigShape, Error2> {
        Ok(self.service.shape_without_provider(provider_id))
    }

    async fn set_config(&self, patch: ManagedKimiConfigPatch) -> Result<ManagedKimiConfigShape, Error2> {
        self.service.apply_refresh_patch(patch, &self.exclusion).await
    }

    async fn resolve_oauth_token(
        &self,
        provider_name: &str,
        oauth_ref: Option<&super::config_section::OAuthRef>,
    ) -> Result<String, Error2> {
        self.service.resolve_oauth_token(provider_name, oauth_ref).await
    }

    fn user_agent(&self) -> String {
        self.user_agent.clone()
    }
}

fn effective_model_source(provider: &ProviderConfig) -> Option<ModelSource> {
    provider.model_source.or_else(|| {
        if provider.type_.as_deref() == Some("kimi") {
            Some(ModelSource::OauthCatalog)
        } else {
            None
        }
    })
}

fn decode_providers(value: Option<Value>) -> ProvidersSection {
    value
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default()
}

fn decode_models(value: Option<Value>) -> ModelsSection {
    value
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default()
}

fn config_inspect_string(config: &dyn KosongConfigStore, domain: &str) -> Option<String> {
    config
        .inspect(domain)
        .user_value
        .and_then(|value| value.as_str().map(str::to_string))
}

fn without_keys<T: Clone>(
    record: &indexmap::IndexMap<String, T>,
    excluded: &indexmap::IndexMap<String, T>,
) -> indexmap::IndexMap<String, T> {
    if excluded.is_empty() {
        return record.clone();
    }
    record
        .iter()
        .filter(|(key, _)| !excluded.contains_key(*key))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn merge_sections<T: Clone>(
    excluded: &indexmap::IndexMap<String, T>,
    patch: &indexmap::IndexMap<String, T>,
) -> indexmap::IndexMap<String, T> {
    let mut merged = excluded.clone();
    for (key, value) in patch {
        merged.insert(key.clone(), value.clone());
    }
    merged
}

fn effective_model_config(model: &ModelRecord) -> ModelRecord {
    let Some(overrides) = &model.overrides else {
        return model.clone();
    };
    let mut effective = model.clone();
    effective.overrides = None;
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
    if let Some(support_efforts) = &overrides.support_efforts {
        effective.support_efforts = Some(support_efforts.clone());
        if overrides.default_effort.is_none()
            && effective
                .default_effort
                .as_ref()
                .is_some_and(|default| !support_efforts.contains(default))
        {
            effective.default_effort = None;
        }
    }
    if overrides.default_effort.is_some() {
        effective.default_effort = overrides.default_effort.clone();
    }
    if overrides.off_effort.is_some() {
        effective.off_effort = overrides.off_effort.clone();
    }
    if let (Some(input), Some(context)) = (effective.max_input_size, effective.max_context_size)
        && input > context
    {
        effective.max_input_size = Some(context);
    }
    effective
}

fn map_refresh_result(result: RefreshResult) -> RefreshProviderModelsResponse {
    let retired_model = result
        .retired_default
        .as_ref()
        .map(|retired| effective_model_config(&retired.from_model));
    RefreshProviderModelsResponse {
        changed: result
            .changed
            .into_iter()
            .map(|change| ProviderRefreshChange {
                provider_id: change.provider_id,
                provider_name: change.provider_name,
                added: change.added,
                removed: change.removed,
            })
            .collect(),
        unchanged: result.unchanged,
        failed: result
            .failed
            .into_iter()
            .map(|failure| ProviderRefreshFailure {
                provider: failure.provider,
                reason: failure.reason,
            })
            .collect(),
        retired_default: result.retired_default.map(|retired| RetiredDefault {
            from_display_name: retired_model
                .as_ref()
                .and_then(|model| model.display_name.clone().or(model.model.clone()))
                .unwrap_or_else(|| retired.from.clone()),
            from: retired.from,
            to: retired.to,
        }),
    }
}

pub fn register_provider_discovery_service() {
    crate::_base::di::scope::register_scoped_service(
        crate::app::scopes::LifecycleScope::App,
        crate::_base::di::scope::ScopeActivation::OnScopeCreated,
        "kosongConfig",
    );
}
