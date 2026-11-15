use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

use indexmap::IndexMap;
use parking_lot::Mutex;
use serde_json::{Map, Value};

use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::human::llm::capability::ModelCapability;
use crate::human::llm::model::{LlmConnection, LlmModel};
use crate::human::llm::protocol::base::ProtocolName;
use crate::human::llm::provider_catalog::{
    CatalogModel, CatalogModelDefinition, CatalogModelOverrides, CatalogOAuthRef,
    CatalogProviderInfo, ProviderCatalog, create_provider_catalog_sync,
};
use crate::llm_adapter::model::model_auth::{derive_provider_id, non_empty};
use crate::llm_adapter::model::model::{IModelService, ModelOverride, ModelRecord};
use crate::llm_adapter::protocol::protocol::Protocol;
use crate::llm_adapter::provider::provider::{IProviderService, OAuthRef, ProviderConfig};

pub trait IProviderCatalogRuntime: Send + Sync {
    fn sync(&self);
    fn resync(&self);
    fn aliases(&self) -> Vec<String>;
    fn provider_ids(&self) -> Vec<String>;
    fn provider_info(&self, provider_id: &str) -> Option<CatalogProviderInfo>;
    fn lookup(&self, alias: &str) -> Option<CatalogModel>;
    fn on_changed(
        &self,
        listener: Arc<dyn Fn(Vec<String>) + Send + Sync>,
    ) -> DisposableRef;
}

pub fn raw_record_of(definition: &CatalogModelDefinition) -> ModelRecord {
    let value = definition
        .extras
        .as_ref()
        .and_then(|extras| extras.get("record"))
        .cloned()
        .expect("catalog model extras contain the source record");
    serde_json::from_value(value).expect("catalog model source record is valid")
}

pub fn bucket_of_record(record: &ModelRecord, default_provider: Option<&str>) -> String {
    if let Some(referenced) = record
        .provider_id
        .as_deref()
        .or(record.provider.as_deref())
        .or(default_provider)
    {
        return referenced.to_string();
    }
    derive_provider_id(&non_empty(record.base_url.as_deref()).unwrap_or_default())
}

fn oauth_to_catalog(oauth: Option<&OAuthRef>) -> Option<CatalogOAuthRef> {
    oauth.map(|oauth| CatalogOAuthRef {
        storage: oauth.storage.clone(),
        key: oauth.key.clone(),
        oauth_host: oauth.oauth_host.clone(),
    })
}

pub fn to_catalog_provider_info(config: &ProviderConfig) -> CatalogProviderInfo {
    CatalogProviderInfo {
        type_: config.type_.clone(),
        api_key: config.api_key.clone(),
        api_key_env: config.api_key_env.clone(),
        base_url: config.base_url.clone(),
        custom_headers: config.custom_headers.as_ref().map(|headers| {
            headers
                .iter()
                .map(|(key, value)| (key.clone(), Value::String(value.clone())))
                .collect()
        }),
        default_model: config.default_model.clone(),
        oauth: oauth_to_catalog(config.oauth.as_ref()),
        env: config.env.as_ref().map(|env| {
            env.iter()
                .map(|(key, value)| (key.clone(), Value::String(value.clone())))
                .collect()
        }),
        model_source: config.model_source.clone(),
        source: config.source.clone(),
    }
}

fn protocol_name(protocol: Protocol) -> ProtocolName {
    match protocol {
        Protocol::Anthropic => ProtocolName::Anthropic,
        Protocol::OpenAI => ProtocolName::OpenAI,
        Protocol::OpenAIResponses => ProtocolName::OpenAIResponses,
        Protocol::GoogleGenAI => ProtocolName::GoogleGenAI,
        Protocol::Antigravity => ProtocolName::Antigravity,
    }
}

pub fn to_catalog_model_definition(
    alias: &str,
    record: &ModelRecord,
    bucket: &str,
) -> CatalogModelDefinition {
    let model = LlmModel {
        connection: LlmConnection {
            base_url: record.base_url.clone(),
            api_key: record.api_key.clone(),
            default_headers: None,
            beta_api: record.beta_api,
            vertexai: None,
        },
        provider: bucket.to_string(),
        model: alias.to_string(),
        capability: capability_from_declared(record.capabilities.as_deref()),
        max_context_size: record.max_context_size.map(u64::from),
        max_input_size: record.max_input_size.map(u64::from),
        support_efforts: record.support_efforts.clone(),
        default_effort: record.default_effort.clone(),
        off_effort: record.off_effort.clone(),
        always_thinking: Some(declares_always_thinking(record.capabilities.as_deref())),
        adaptive_thinking: record.adaptive_thinking,
    };
    let extras = Map::from_iter([(
        "record".to_string(),
        serde_json::to_value(record).expect("model record is serializable"),
    )]);
    CatalogModelDefinition {
        model,
        display_name: record.display_name.clone(),
        max_output_size: record.max_output_size.map(u64::from),
        reasoning_key: record.reasoning_key.clone(),
        support_efforts: record.support_efforts.clone(),
        off_effort: record.off_effort.clone(),
        always_thinking: Some(declares_always_thinking(record.capabilities.as_deref())),
        protocol: record.protocol.map(protocol_name),
        default_effort: record.default_effort.clone(),
        adaptive_thinking: record.adaptive_thinking,
        name: record.name.clone(),
        aliases: record.aliases.clone(),
        oauth: oauth_to_catalog(record.oauth.as_ref()),
        overrides: to_catalog_overrides(record.overrides.as_ref()),
        extras: Some(extras),
    }
}

fn to_catalog_overrides(overrides: Option<&ModelOverride>) -> Option<CatalogModelOverrides> {
    let overrides = overrides?;
    Some(CatalogModelOverrides {
        max_context_size: overrides.max_context_size.map(u64::from),
        max_input_size: overrides.max_input_size.map(u64::from),
        max_output_size: overrides.max_output_size.map(u64::from),
        capability: overrides
            .capabilities
            .as_deref()
            .map(|capabilities| capability_from_declared(Some(capabilities))),
        display_name: overrides.display_name.clone(),
        reasoning_key: overrides.reasoning_key.clone(),
        adaptive_thinking: overrides.adaptive_thinking,
        support_efforts: overrides.support_efforts.clone(),
        default_effort: overrides.default_effort.clone(),
        off_effort: overrides.off_effort.clone(),
        always_thinking: overrides
            .capabilities
            .as_deref()
            .map(|capabilities| declares_always_thinking(Some(capabilities))),
    })
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

fn declared_set(capabilities: Option<&[String]>) -> Vec<String> {
    capabilities
        .unwrap_or_default()
        .iter()
        .map(|capability| js_trim(capability).to_lowercase())
        .collect()
}

fn declares_always_thinking(capabilities: Option<&[String]>) -> bool {
    declared_set(capabilities)
        .iter()
        .any(|capability| capability == "always_thinking")
}

fn capability_from_declared(capabilities: Option<&[String]>) -> ModelCapability {
    let declared = declared_set(capabilities);
    let has = |capability: &str| declared.iter().any(|candidate| candidate == capability);
    ModelCapability {
        image_in: has("image_in"),
        video_in: has("video_in"),
        audio_in: has("audio_in"),
        thinking: has("thinking") || has("always_thinking"),
        tool_use: has("tool_use"),
        pdf_in: Some(has("pdf_in")),
        dynamically_loaded_tools: Some(has("dynamically_loaded_tools")),
        unknown_capability: false,
    }
}

#[derive(Debug, Clone, PartialEq)]
struct DesiredBucket {
    info: Option<CatalogProviderInfo>,
    models: Vec<CatalogModelDefinition>,
}

struct CatalogSubscription {
    unsubscribe: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl IDisposable for CatalogSubscription {
    fn dispose(&self) {
        if let Some(unsubscribe) = self.unsubscribe.lock().take() {
            unsubscribe();
        }
    }
}

pub struct ProviderCatalogRuntimeService {
    model_service: Arc<dyn IModelService>,
    provider_service: Arc<dyn IProviderService>,
    catalog: ProviderCatalog,
    synced: Mutex<IndexMap<String, DesiredBucket>>,
    routing: Mutex<HashMap<String, String>>,
    alias_order: Mutex<Vec<String>>,
    provider_order: Mutex<Vec<String>>,
    dirty: AtomicBool,
    subscriptions: Mutex<Vec<DisposableRef>>,
}

impl ProviderCatalogRuntimeService {
    pub fn new(
        model_service: Arc<dyn IModelService>,
        provider_service: Arc<dyn IProviderService>,
    ) -> Arc<Self> {
        let this = Arc::new(Self {
            model_service,
            provider_service,
            catalog: create_provider_catalog_sync(None),
            synced: Mutex::new(IndexMap::new()),
            routing: Mutex::new(HashMap::new()),
            alias_order: Mutex::new(Vec::new()),
            provider_order: Mutex::new(Vec::new()),
            dirty: AtomicBool::new(true),
            subscriptions: Mutex::new(Vec::new()),
        });
        let models_subscription = this.model_service.on_did_change_models({
            let weak = Arc::downgrade(&this);
            Arc::new(move |_| mark_dirty(&weak))
        });
        let providers_subscription = this.provider_service.on_did_change_providers({
            let weak = Arc::downgrade(&this);
            Arc::new(move |_| mark_dirty(&weak))
        });
        let default_subscription = this.provider_service.on_did_change_default_provider({
            let weak = Arc::downgrade(&this);
            Arc::new(move |_| mark_dirty(&weak))
        });
        this.subscriptions.lock().extend([
            models_subscription,
            providers_subscription,
            default_subscription,
        ]);
        this
    }

    fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Relaxed);
    }

    fn sync_all(&self) {
        let providers = self.provider_service.list();
        let models = self.model_service.list();
        let default_provider = self.provider_service.get_default_provider();
        let mut desired = IndexMap::<String, DesiredBucket>::new();
        for (provider_id, config) in &providers {
            desired.insert(
                provider_id.clone(),
                DesiredBucket {
                    info: Some(to_catalog_provider_info(config)),
                    models: Vec::new(),
                },
            );
        }
        let mut routing = HashMap::new();
        for (alias, record) in &models {
            let bucket = bucket_of_record(record, default_provider.as_deref());
            routing.insert(alias.clone(), bucket.clone());
            let definition = to_catalog_model_definition(alias, record, &bucket);
            desired
                .entry(bucket)
                .or_insert_with(|| DesiredBucket {
                    info: None,
                    models: Vec::new(),
                })
                .models
                .push(definition);
        }
        *self.routing.lock() = routing;
        *self.alias_order.lock() = models.keys().cloned().collect();
        *self.provider_order.lock() = providers.keys().cloned().collect();

        for (provider_id, bucket) in &desired {
            let unchanged = self
                .synced
                .lock()
                .get(provider_id)
                .is_some_and(|previous| previous == bucket);
            if unchanged {
                continue;
            }
            self.synced
                .lock()
                .insert(provider_id.clone(), bucket.clone());
            self.catalog.upsert_entry(
                provider_id,
                bucket.info.clone(),
                Some(bucket.models.clone()),
            );
        }
        let removed = self
            .synced
            .lock()
            .keys()
            .filter(|provider_id| !desired.contains_key(*provider_id))
            .cloned()
            .collect::<Vec<_>>();
        for provider_id in removed {
            self.synced.lock().shift_remove(&provider_id);
            self.catalog.remove(&provider_id);
        }
    }
}

fn mark_dirty(service: &Weak<ProviderCatalogRuntimeService>) {
    if let Some(service) = service.upgrade() {
        service.mark_dirty();
    }
}

impl IProviderCatalogRuntime for ProviderCatalogRuntimeService {
    fn sync(&self) {
        if !self.dirty.swap(false, Ordering::Relaxed) {
            return;
        }
        self.sync_all();
    }

    fn resync(&self) {
        self.dirty.store(false, Ordering::Relaxed);
        self.sync_all();
    }

    fn aliases(&self) -> Vec<String> {
        self.sync();
        self.alias_order.lock().clone()
    }

    fn provider_ids(&self) -> Vec<String> {
        self.sync();
        self.provider_order.lock().clone()
    }

    fn provider_info(&self, provider_id: &str) -> Option<CatalogProviderInfo> {
        self.sync();
        self.catalog.provider_info(provider_id)
    }

    fn lookup(&self, alias: &str) -> Option<CatalogModel> {
        self.sync();
        let bucket = self.routing.lock().get(alias).cloned()?;
        self.catalog
            .models(&bucket)
            .into_iter()
            .find(|model| model.model == alias)
    }

    fn on_changed(
        &self,
        listener: Arc<dyn Fn(Vec<String>) + Send + Sync>,
    ) -> DisposableRef {
        Arc::new(CatalogSubscription {
            unsubscribe: Mutex::new(Some(self.catalog.on_changed(listener))),
        })
    }
}

impl IDisposable for ProviderCatalogRuntimeService {
    fn dispose(&self) {
        self.catalog.stop();
        for subscription in self.subscriptions.lock().iter() {
            subscription.dispose();
        }
    }
}
