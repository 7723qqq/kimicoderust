use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use indexmap::IndexMap;
use kimi_core::primitives::errors::{Error2, Error2Options, domains::core::INTERNAL};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::_base::utils::fetch_like::FetchLike;
use crate::app::agent_identity::agent_identity::IAgentIdentity;

use super::config_section::{
    DEFAULT_MODEL_SECTION, DEFAULT_PROVIDER_SECTION, MODELS_SECTION, ModelsSection,
    PROVIDERS_SECTION, ProviderConfig, ProvidersSection, THINKING_SECTION, ThinkingConfig,
};
use super::errors::{
    CATALOG_ENTRY_NOT_FOUND, CATALOG_IMPORT_INVALID, PROVIDER_OAUTH_MANAGED,
    REGISTRY_IMPORT_INVALID,
};
use super::kosong_config::{
    ConfigReplaceSectionsOptions, IKosongConfigService, KosongConfigStore,
};
use super::models_dev::{models_dev_provider_models, resolve_models_dev_import, ModelsDevImportResolution};
use super::models_dev_import::{
    IModelsDevImportService, ImportCustomRegistryOptions, ImportCustomRegistryResult,
    ImportModelsDevProviderOptions, ImportModelsDevProviderResult, ModelsDevProviderItem,
    PROVIDER_ID_PATTERN, ProviderCatalogItem,
};
use super::models_dev_upstream::{
    UPSTREAM_FETCH_TIMEOUT_MS, get_models_dev_catalog, models_dev_entry,
    models_dev_model_to_record, to_models_dev_provider_item, upstream_fetch,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomRegistrySource {
    pub kind: CustomRegistrySourceKind,
    pub url: String,
    pub api_key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CustomRegistrySourceKind {
    #[serde(rename = "apiJson")]
    ApiJson,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CustomRegistryProviderEntry {
    pub id: String,
    pub models: IndexMap<String, Value>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Default)]
pub struct CustomRegistryConfigShape {
    pub providers: ProvidersSection,
    pub models: ModelsSection,
    pub default_model: Option<String>,
    pub default_provider: Option<String>,
    pub thinking: Option<ThinkingConfig>,
}

#[derive(Debug, Clone, Default)]
pub struct ProviderCredential {
    pub api_key: Option<String>,
    pub api_key_env: Option<String>,
}

#[derive(Debug)]
pub struct CustomRegistryApiError {
    pub message: String,
    pub status: Option<u16>,
}

impl fmt::Display for CustomRegistryApiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for CustomRegistryApiError {}

#[async_trait]
pub trait CustomRegistryBackend: Send + Sync {
    async fn fetch_custom_registry(
        &self,
        source: &CustomRegistrySource,
        fetch_impl: Arc<dyn FetchLike>,
        user_agent: &str,
        timeout_ms: u64,
    ) -> Result<IndexMap<String, CustomRegistryProviderEntry>, CustomRegistryApiError>;

    fn custom_registry_replacement_keys(
        &self,
        config: &CustomRegistryConfigShape,
        entries: &IndexMap<String, CustomRegistryProviderEntry>,
        source: &CustomRegistrySource,
    ) -> BTreeMap<String, Vec<String>>;

    fn apply_custom_registry_entries(
        &self,
        config: &mut CustomRegistryConfigShape,
        entries: &IndexMap<String, CustomRegistryProviderEntry>,
        source: &CustomRegistrySource,
    ) -> Result<(), String>;

    fn reconcile_provider_credential_update(
        &self,
        existing: &ProviderConfig,
        api_key: Option<String>,
        provider_id: &str,
    ) -> Result<ProviderCredential, String>;

    fn credential_env_hints(
        &self,
        entries: &IndexMap<String, CustomRegistryProviderEntry>,
    ) -> BTreeMap<String, String>;
}

#[async_trait]
pub trait ModelCatalog: Send + Sync {
    async fn get_provider(&self, provider_id: &str) -> Result<ProviderCatalogItem, Error2>;
}

pub struct ModelsDevImportService {
    config: Arc<dyn KosongConfigStore>,
    kosong_config: Arc<dyn IKosongConfigService>,
    model_catalog: Arc<dyn ModelCatalog>,
    identity: Arc<dyn IAgentIdentity>,
    registry: Arc<dyn CustomRegistryBackend>,
    write_chain: tokio::sync::Mutex<()>,
}

impl ModelsDevImportService {
    pub fn new(
        config: Arc<dyn KosongConfigStore>,
        kosong_config: Arc<dyn IKosongConfigService>,
        model_catalog: Arc<dyn ModelCatalog>,
        identity: Arc<dyn IAgentIdentity>,
        registry: Arc<dyn CustomRegistryBackend>,
    ) -> Self {
        Self {
            config,
            kosong_config,
            model_catalog,
            identity,
            registry,
            write_chain: tokio::sync::Mutex::new(()),
        }
    }

    async fn outbound_user_agent(&self) -> Result<String, Error2> {
        self.identity
            .resolved()
            .await
            .map(|identity| identity.outbound_user_agent)
            .map_err(|error| Error2::new(INTERNAL, error.to_string()))
    }

    async fn ready_config(&self) -> Result<(), Error2> {
        self.config.ready().await?;
        self.kosong_config.ready().await
    }

    async fn do_import_models_dev_provider(
        &self,
        options: ImportModelsDevProviderOptions,
    ) -> Result<ImportModelsDevProviderResult, Error2> {
        let catalog_id = options.catalog_id;
        let user_agent = self.outbound_user_agent().await?;
        let catalog = get_models_dev_catalog(&user_agent).await?;
        let entry = models_dev_entry(&catalog, &catalog_id).cloned().ok_or_else(|| {
            Error2::new(
                CATALOG_ENTRY_NOT_FOUND,
                format!("catalog entry {catalog_id} does not exist"),
            )
        })?;
        let resolution = resolve_models_dev_import(&entry, options.base_url.as_deref());
        let (wire, base_url) = match resolution {
            ModelsDevImportResolution::Invalid { reason } => {
                return Err(Error2::new(
                    CATALOG_IMPORT_INVALID,
                    format!(
                        "catalog entry {catalog_id} cannot be imported: {}",
                        reason.as_str()
                    ),
                ));
            }
            ModelsDevImportResolution::NeedsBaseUrl { .. } => {
                return Err(Error2::new(
                    CATALOG_IMPORT_INVALID,
                    format!("catalog entry {catalog_id} requires a base_url"),
                ));
            }
            ModelsDevImportResolution::Ok { wire, base_url, .. } => (wire, base_url),
        };
        let models = models_dev_provider_models(&entry);
        if models.is_empty() {
            return Err(Error2::new(
                CATALOG_IMPORT_INVALID,
                format!("catalog entry {catalog_id} has no importable models"),
            ));
        }
        let target_id = options.id.unwrap_or(catalog_id);
        if !PROVIDER_ID_PATTERN.is_match(&target_id) {
            return Err(Error2::new(
                CATALOG_IMPORT_INVALID,
                format!("catalog entry id {target_id} cannot be used as a provider id"),
            ));
        }

        self.ready_config().await?;
        let providers = decode_providers(self.config.inspect(PROVIDERS_SECTION).user_value);
        let existing = providers.get(&target_id).cloned();
        if existing.as_ref().is_some_and(|provider| provider.oauth.is_some()) {
            return Err(Error2::new(
                PROVIDER_OAUTH_MANAGED,
                format!("provider {target_id} is managed by OAuth login; use POST /oauth/logout instead"),
            ));
        }
        let credential = self
            .registry
            .reconcile_provider_credential_update(
                &existing.unwrap_or_default(),
                options.api_key,
                &target_id,
            )
            .map_err(|message| Error2::new(CATALOG_IMPORT_INVALID, message))?;
        let provider = ProviderConfig {
            type_: Some(wire),
            base_url,
            api_key: credential.api_key,
            api_key_env: credential.api_key_env,
            model_source: None,
            ..Default::default()
        };
        let mut next_providers = providers;
        next_providers.insert(target_id.clone(), provider);
        self.config
            .replace(
                PROVIDERS_SECTION,
                Some(serde_json::to_value(next_providers).unwrap_or_else(|_| json!({}))),
            )
            .await?;

        let records = decode_models(self.config.inspect(MODELS_SECTION).user_value);
        let mut next_models: ModelsSection = records
            .into_iter()
            .filter(|(_, record)| record.provider.as_deref() != Some(target_id.as_str()))
            .collect();
        for model in &models {
            next_models.insert(
                format!("{target_id}/{}", model.id),
                models_dev_model_to_record(&target_id, model),
            );
        }
        self.config
            .replace(
                MODELS_SECTION,
                Some(serde_json::to_value(next_models).unwrap_or_else(|_| json!({}))),
            )
            .await?;
        if let Some(first_model) = models.first() {
            seed_default_model_when_unset(
                &*self.config,
                &format!("{target_id}/{}", first_model.id),
            )
            .await?;
        }
        let imported = self.model_catalog.get_provider(&target_id).await?;
        Ok(ImportModelsDevProviderResult {
            provider: imported,
            models_imported: models.len(),
        })
    }

    async fn do_import_custom_registry(
        &self,
        options: ImportCustomRegistryOptions,
    ) -> Result<ImportCustomRegistryResult, Error2> {
        let url = options.url;
        self.ready_config().await?;
        let initial_providers = decode_providers(self.config.inspect(PROVIDERS_SECTION).user_value);
        let source = CustomRegistrySource {
            kind: CustomRegistrySourceKind::ApiJson,
            api_key: options
                .api_key
                .or_else(|| registry_key_from_existing(&initial_providers, &url))
                .unwrap_or_default(),
            url: url.clone(),
        };
        let entries = self
            .registry
            .fetch_custom_registry(
                &source,
                upstream_fetch(),
                &self.outbound_user_agent().await?,
                UPSTREAM_FETCH_TIMEOUT_MS,
            )
            .await
            .map_err(|error| {
                let mut details = Map::new();
                details.insert("phase".to_string(), Value::String("fetch".to_string()));
                if let Some(status) = error.status {
                    details.insert("status".to_string(), Value::from(status));
                }
                Error2::with_options(
                    REGISTRY_IMPORT_INVALID,
                    format!(
                        "custom registry at {url} cannot be imported: {}",
                        truncate_upstream_message(&error.message, 300)
                    ),
                    Error2Options {
                        details: Some(details),
                        ..Default::default()
                    },
                )
            })?;
        if entries.is_empty() {
            return Err(Error2::with_options(
                REGISTRY_IMPORT_INVALID,
                format!("custom registry at {url} has no importable providers"),
                Error2Options {
                    details: Some(Map::from_iter([(
                        "phase".to_string(),
                        Value::String("empty".to_string()),
                    )])),
                    ..Default::default()
                },
            ));
        }

        self.config.reload().await?;
        let providers = decode_providers(self.config.inspect(PROVIDERS_SECTION).user_value);
        for entry in entries.values() {
            if providers.get(&entry.id).is_some_and(|provider| provider.oauth.is_some()) {
                return Err(Error2::new(
                    PROVIDER_OAUTH_MANAGED,
                    format!("provider {} is managed by OAuth login; use POST /oauth/logout instead", entry.id),
                ));
            }
        }
        let previous_default = config_inspect_string(&*self.config, DEFAULT_MODEL_SECTION);
        let previous_default_provider = config_inspect_string(&*self.config, DEFAULT_PROVIDER_SECTION);
        let previous_thinking = self
            .config
            .inspect(THINKING_SECTION)
            .user_value
            .and_then(|value| serde_json::from_value::<ThinkingConfig>(value).ok());
        let mut next = CustomRegistryConfigShape {
            providers,
            models: decode_models(self.config.inspect(MODELS_SECTION).user_value),
            default_model: previous_default.clone(),
            default_provider: previous_default_provider.clone(),
            thinking: previous_thinking.clone(),
        };
        let replacement_keys = self
            .registry
            .custom_registry_replacement_keys(&next, &entries, &source);
        self.registry
            .apply_custom_registry_entries(&mut next, &entries, &source)
            .map_err(|error| {
                Error2::new(
                    REGISTRY_IMPORT_INVALID,
                    format!(
                        "custom registry at {url} cannot be imported: {}",
                        truncate_upstream_message(&error, 300)
                    ),
                )
            })?;

        let first_entry = entries.values().next();
        let first_model_key = first_entry.and_then(|entry| entry.models.keys().next());
        let had_default = previous_default
            .as_deref()
            .is_some_and(|default| !default.trim().is_empty());
        if options.set_default_when_unset != Some(false)
            && !had_default
            && let (Some(first_entry), Some(first_model_key)) = (first_entry, first_model_key)
        {
            next.default_model = Some(format!("{}/{}", first_entry.id, first_model_key));
        }
        let mut sections = BTreeMap::from_iter([
            (
                PROVIDERS_SECTION.to_string(),
                Some(serde_json::to_value(&next.providers).unwrap_or_else(|_| json!({}))),
            ),
            (
                MODELS_SECTION.to_string(),
                Some(serde_json::to_value(&next.models).unwrap_or_else(|_| json!({}))),
            ),
        ]);
        let mut expected_values = BTreeMap::new();
        if next.default_model != previous_default {
            sections.insert(
                DEFAULT_MODEL_SECTION.to_string(),
                next.default_model.clone().map(Value::String),
            );
            expected_values.insert(
                DEFAULT_MODEL_SECTION.to_string(),
                previous_default.map(Value::String),
            );
        }
        if next.default_provider != previous_default_provider {
            sections.insert(
                DEFAULT_PROVIDER_SECTION.to_string(),
                next.default_provider.clone().map(Value::String),
            );
            expected_values.insert(
                DEFAULT_PROVIDER_SECTION.to_string(),
                previous_default_provider.map(Value::String),
            );
        }
        if thinking_value(&next.thinking) != thinking_value(&previous_thinking) {
            sections.insert(THINKING_SECTION.to_string(), thinking_value(&next.thinking));
            expected_values.insert(
                THINKING_SECTION.to_string(),
                thinking_value(&previous_thinking),
            );
        }
        self.config
            .replace_sections(
                sections,
                Some(ConfigReplaceSectionsOptions {
                    preserve_unknown: Some(false),
                    exact_keys: replacement_keys,
                    expected_values,
                }),
            )
            .await
            .map_err(|error| {
                Error2::new(
                    REGISTRY_IMPORT_INVALID,
                    format!(
                        "custom registry at {url} cannot be imported: {}",
                        truncate_upstream_message(&error.to_string(), 300)
                    ),
                )
            })?;

        let mut imported = Vec::new();
        for entry in entries.values() {
            imported.push(self.model_catalog.get_provider(&entry.id).await?);
        }
        let models_imported = entries.values().map(|entry| entry.models.len()).sum();
        let credential_env = self.registry.credential_env_hints(&entries);
        Ok(ImportCustomRegistryResult {
            providers: imported,
            models_imported,
            credential_env,
        })
    }
}

#[async_trait]
impl IModelsDevImportService for ModelsDevImportService {
    async fn list_models_dev_providers(&self) -> Result<Vec<ModelsDevProviderItem>, Error2> {
        let user_agent = self.outbound_user_agent().await?;
        let catalog = get_models_dev_catalog(&user_agent).await?;
        Ok(catalog
            .iter()
            .map(|(id, entry)| to_models_dev_provider_item(id, entry))
            .collect())
    }

    async fn get_models_dev_provider(&self, catalog_id: &str) -> Result<ModelsDevProviderItem, Error2> {
        let user_agent = self.outbound_user_agent().await?;
        let catalog = get_models_dev_catalog(&user_agent).await?;
        let entry = models_dev_entry(&catalog, catalog_id).ok_or_else(|| {
            Error2::new(
                CATALOG_ENTRY_NOT_FOUND,
                format!("catalog entry {catalog_id} does not exist"),
            )
        })?;
        Ok(to_models_dev_provider_item(catalog_id, entry))
    }

    async fn import_models_dev_provider(
        &self,
        options: ImportModelsDevProviderOptions,
    ) -> Result<ImportModelsDevProviderResult, Error2> {
        let _guard = self.write_chain.lock().await;
        self.do_import_models_dev_provider(options).await
    }

    async fn import_custom_registry(
        &self,
        options: ImportCustomRegistryOptions,
    ) -> Result<ImportCustomRegistryResult, Error2> {
        let _guard = self.write_chain.lock().await;
        self.do_import_custom_registry(options).await
    }
}

async fn seed_default_model_when_unset(
    config: &dyn KosongConfigStore,
    alias: &str,
) -> Result<(), Error2> {
    let current = config_inspect_string(config, DEFAULT_MODEL_SECTION);
    if current.as_deref().is_some_and(|current| !current.trim().is_empty()) {
        return Ok(());
    }
    config
        .replace(DEFAULT_MODEL_SECTION, Some(Value::String(alias.to_string())))
        .await
}

fn registry_key_from_existing(providers: &ProvidersSection, url: &str) -> Option<String> {
    for provider in providers.values() {
        let Some(source) = &provider.source else {
            continue;
        };
        if source.get("kind").and_then(Value::as_str) == Some("apiJson")
            && source.get("url").and_then(Value::as_str) == Some(url)
        {
            if let Some(key) = source
                .get("apiKey")
                .and_then(Value::as_str)
                .filter(|key| !key.is_empty())
            {
                return Some(key.to_string());
            }
        }
    }
    None
}

fn truncate_upstream_message(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    format!("{}…", text.chars().take(limit).collect::<String>())
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

fn thinking_value(value: &Option<ThinkingConfig>) -> Option<Value> {
    value.as_ref().map(|value| serde_json::to_value(value).unwrap_or(Value::Null))
}

pub fn register_models_dev_import_service() {
    crate::_base::di::scope::register_scoped_service(
        crate::app::scopes::LifecycleScope::App,
        crate::_base::di::scope::ScopeActivation::OnScopeCreated,
        "kosongConfig",
    );
}
