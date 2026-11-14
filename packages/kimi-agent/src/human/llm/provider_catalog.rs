use std::collections::{BTreeMap, HashMap};
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use futures_util::future::BoxFuture;
use indexmap::IndexMap;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::human::llm::capability::{ModelCapability, UNKNOWN_CAPABILITY};
use crate::human::llm::model::LlmModel;
use crate::human::llm::protocol::base::ProtocolName;
use crate::human::llm::provider::definition::Provider;
use crate::human::llm::requester::requester::{
    LlmEvent, LlmRequestConfig, LlmRequestContent, LlmRequestControl, LlmRequester,
};
use crate::human::llm::message::{Message, create_user_message};
use crate::wire::record::WireAsyncError;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogOAuthRef {
    pub storage: String,
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_host: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogModelOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_context_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability: Option<ModelCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_thinking: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub support_efforts: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub off_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub always_thinking: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogModelDefinition {
    #[serde(flatten)]
    pub model: LlmModel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub support_efforts: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub off_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub always_thinking: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<ProtocolName>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_thinking: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aliases: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<CatalogOAuthRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overrides: Option<CatalogModelOverrides>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extras: Option<Map<String, Value>>,
}

impl Deref for CatalogModelDefinition {
    type Target = LlmModel;

    fn deref(&self) -> &Self::Target {
        &self.model
    }
}

impl DerefMut for CatalogModelDefinition {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.model
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogProviderInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub type_: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_headers: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<CatalogOAuthRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogProviderEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub info: Option<CatalogProviderInfo>,
    pub discovered: BTreeMap<String, LlmModel>,
    pub override_: BTreeMap<String, CatalogModelDefinition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ping_errors: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogModel {
    #[serde(flatten)]
    pub definition: CatalogModelDefinition,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ping_error: Option<String>,
}

impl Deref for CatalogModel {
    type Target = CatalogModelDefinition;

    fn deref(&self) -> &Self::Target {
        &self.definition
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CatalogSnapshot {
    pub providers: BTreeMap<String, CatalogProviderEntry>,
}

#[async_trait]
pub trait ProviderCatalogStore: Send + Sync {
    async fn load(&self) -> Result<Option<CatalogSnapshot>, WireAsyncError>;
    async fn save(&self, snapshot: CatalogSnapshot) -> Result<(), WireAsyncError>;
}

#[derive(Clone, Default)]
pub struct MemoryProviderCatalogStore {
    snapshot: Arc<Mutex<Option<CatalogSnapshot>>>,
}

#[async_trait]
impl ProviderCatalogStore for MemoryProviderCatalogStore {
    async fn load(&self) -> Result<Option<CatalogSnapshot>, WireAsyncError> {
        Ok(self.snapshot.lock().clone())
    }

    async fn save(&self, snapshot: CatalogSnapshot) -> Result<(), WireAsyncError> {
        *self.snapshot.lock() = Some(snapshot);
        Ok(())
    }
}

pub fn create_memory_provider_catalog_store() -> MemoryProviderCatalogStore {
    MemoryProviderCatalogStore::default()
}

#[derive(Clone)]
pub enum ProviderCatalogEvent {
    Upsert {
        provider_id: String,
        info: Option<CatalogProviderInfo>,
        models: Option<Vec<CatalogModelDefinition>>,
    },
    Remove {
        provider_id: String,
    },
    Refresh {
        providers: Vec<Arc<dyn Provider>>,
    },
    Ping {
        provider: Arc<dyn Provider>,
        model: String,
    },
    Stop,
}

#[derive(Clone)]
pub enum ProviderCatalogEmitted {
    Changed { providers: Vec<String> },
    RefreshFailed { provider_id: String, error: String },
}

#[derive(Clone)]
struct PingRequest {
    provider: Arc<dyn Provider>,
    model: CatalogModelDefinition,
}

#[derive(Clone)]
struct PingOutcome {
    provider_id: String,
    model: String,
    error: Option<String>,
}

#[derive(Clone)]
struct PullResult {
    provider_id: String,
    models: Option<Vec<LlmModel>>,
    error: Option<String>,
}

#[derive(Clone, Default)]
struct ProviderCatalogContext {
    snapshot: CatalogSnapshot,
    batch: Vec<Arc<dyn Provider>>,
    queue: Vec<Arc<dyn Provider>>,
    dirty: Vec<String>,
    failures: Vec<(String, String)>,
    ping: Option<PingRequest>,
    ping_queue: Vec<PingRequest>,
}

pub struct ProviderCatalogMachine {
    context: Arc<Mutex<ProviderCatalogContext>>,
    sender: tokio::sync::mpsc::UnboundedSender<ProviderCatalogEvent>,
    receiver: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<ProviderCatalogEvent>>>>,
    listeners: Arc<Mutex<IndexMap<u64, Arc<dyn Fn(ProviderCatalogEmitted) + Send + Sync>>>>,
    next_listener_id: AtomicU64,
    started: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl ProviderCatalogMachine {
    pub fn start(self: &Arc<Self>) {
        let mut started = self.started.lock();
        if started.is_some() {
            return;
        }
        let this = self.clone();
        let receiver = self.receiver.lock().take();
        *started = Some(tokio::spawn(async move {
            this.run(receiver.unwrap()).await;
        }));
    }

    pub fn send(&self, event: ProviderCatalogEvent) {
        match event {
            ProviderCatalogEvent::Upsert { .. } | ProviderCatalogEvent::Remove { .. } => {
                self.apply_upsert_or_remove(event);
                return;
            }
            _ => {}
        }
        let _ = self.sender.send(event);
    }

    pub fn snapshot(&self) -> CatalogSnapshot {
        self.context.lock().snapshot.clone()
    }

    pub fn on(
        &self,
        listener: Arc<dyn Fn(ProviderCatalogEmitted) + Send + Sync>,
    ) -> Box<dyn FnOnce() + Send> {
        let id = self.next_listener_id.fetch_add(1, Ordering::Relaxed);
        self.listeners.lock().insert(id, listener);
        let listeners = self.listeners.clone();
        Box::new(move || {
            listeners.lock().remove(&id);
        })
    }

    pub fn stop(&self) {
        let _ = self.sender.send(ProviderCatalogEvent::Stop);
        if let Some(handle) = self.started.lock().take() {
            handle.abort();
        }
    }

    fn apply_upsert_or_remove(&self, event: ProviderCatalogEvent) {
        match event {
            ProviderCatalogEvent::Upsert {
                provider_id,
                info,
                models,
            } => {
                self.context.lock().snapshot.providers.insert(
                    provider_id.clone(),
                    CatalogProviderEntry {
                        info,
                        discovered: BTreeMap::new(),
                        override_: models
                            .unwrap_or_default()
                            .into_iter()
                            .map(|model| (model.model.clone(), model))
                            .collect(),
                        ping_errors: None,
                    },
                );
                self.emit(ProviderCatalogEmitted::Changed {
                    providers: vec![provider_id],
                });
            }
            ProviderCatalogEvent::Remove { provider_id } => {
                self.context.lock().snapshot.providers.remove(&provider_id);
                self.emit(ProviderCatalogEmitted::Changed {
                    providers: vec![provider_id],
                });
            }
            _ => {}
        }
    }

    fn emit(&self, event: ProviderCatalogEmitted) {
        for listener in self.listeners.lock().values() {
            listener(event.clone());
        }
    }

    async fn run(self: Arc<Self>, mut receiver: tokio::sync::mpsc::UnboundedReceiver<ProviderCatalogEvent>) {
        let mut mode = CatalogMode::Idle;
        loop {
            match mode {
                CatalogMode::Idle => {
                    let Some(event) = receiver.recv().await else {
                        return;
                    };
                    match event {
                        ProviderCatalogEvent::Stop => return,
                        ProviderCatalogEvent::Refresh { providers } => {
                            let mut context = self.context.lock();
                            context.batch = providers;
                            context.queue.clear();
                            context.dirty.clear();
                            context.failures.clear();
                            mode = CatalogMode::Refreshing;
                        }
                        ProviderCatalogEvent::Ping { provider, model } => {
                            let resolved = resolve_catalog_model(
                                self.context.lock().snapshot.providers.get(provider.id()),
                                &model,
                            );
                            if let Some(model) = resolved {
                                self.context.lock().ping = Some(PingRequest { provider, model });
                                mode = CatalogMode::Pinging;
                            }
                        }
                        _ => {}
                    }
                }
                CatalogMode::Refreshing => {
                    let providers = self.context.lock().batch.clone();
                    let pull = pull_batch(providers);
                    tokio::pin!(pull);
                    let results = loop {
                        tokio::select! {
                            results = &mut pull => break results,
                            event = receiver.recv() => {
                                match event {
                                    Some(ProviderCatalogEvent::Stop) => return,
                                    Some(event) => self.enqueue_background_event(event, true),
                                    None => return,
                                }
                            }
                        }
                    };
                    self.finish_refresh(results);
                    let mut context = self.context.lock();
                    if !context.queue.is_empty() {
                        context.batch = std::mem::take(&mut context.queue);
                        mode = CatalogMode::Refreshing;
                    } else if !context.ping_queue.is_empty() {
                        context.ping = Some(context.ping_queue.remove(0));
                        mode = CatalogMode::Pinging;
                    } else {
                        mode = CatalogMode::Idle;
                    }
                }
                CatalogMode::Pinging => {
                    let request = self.context.lock().ping.clone().unwrap();
                    let probe = run_ping_probe(request.provider.clone(), request.model.clone());
                    tokio::pin!(probe);
                    let error = loop {
                        tokio::select! {
                            error = &mut probe => break error,
                            event = receiver.recv() => {
                                match event {
                                    Some(ProviderCatalogEvent::Stop) => return,
                                    Some(event) => self.enqueue_background_event(event, false),
                                    None => return,
                                }
                            }
                        }
                    };
                    let outcome = PingOutcome {
                        provider_id: request.provider.id().to_string(),
                        model: request.model.model.clone(),
                        error,
                    };
                    let changed = apply_ping_outcome(&mut self.context.lock().snapshot, &outcome);
                    if changed {
                        self.emit(ProviderCatalogEmitted::Changed {
                            providers: vec![outcome.provider_id.clone()],
                        });
                    }
                    let mut context = self.context.lock();
                    if !context.ping_queue.is_empty() {
                        context.ping = Some(context.ping_queue.remove(0));
                        mode = CatalogMode::Pinging;
                    } else if !context.queue.is_empty() {
                        context.batch = std::mem::take(&mut context.queue);
                        context.ping = None;
                        context.dirty.clear();
                        context.failures.clear();
                        mode = CatalogMode::Refreshing;
                    } else {
                        context.ping = None;
                        mode = CatalogMode::Idle;
                    }
                }
            }
        }
    }

    fn enqueue_background_event(&self, event: ProviderCatalogEvent, refreshing: bool) {
        match event {
            ProviderCatalogEvent::Refresh { providers } => {
                let mut context = self.context.lock();
                context.queue = if refreshing {
                    enqueue_providers(&context.batch, &context.queue, providers)
                } else {
                    enqueue_providers(&[], &context.queue, providers)
                };
            }
            ProviderCatalogEvent::Ping { provider, model } => {
                let resolved = resolve_catalog_model(
                    self.context.lock().snapshot.providers.get(provider.id()),
                    &model,
                );
                if let Some(model) = resolved {
                    let mut context = self.context.lock();
                    context.ping_queue = enqueue_ping(
                        context.ping.clone(),
                        &context.ping_queue,
                        PingRequest { provider, model },
                    );
                }
            }
            _ => {}
        }
    }

    fn finish_refresh(&self, results: Vec<PullResult>) {
        let mut context = self.context.lock();
        apply_pull_results(&mut context.snapshot, &results);
        let dirty = merge_dirty(&context.dirty, &results);
        let failures = pull_failures(&results);
        context.dirty = dirty;
        context.failures.extend(failures);
        if !context.queue.is_empty() {
            return;
        }
        let dirty = context.dirty.clone();
        let failures = context.failures.clone();
        drop(context);
        if !dirty.is_empty() {
            self.emit(ProviderCatalogEmitted::Changed { providers: dirty });
        }
        for (provider_id, error) in failures {
            self.emit(ProviderCatalogEmitted::RefreshFailed {
                provider_id,
                error,
            });
        }
    }
}

#[derive(Clone, Copy)]
enum CatalogMode {
    Idle,
    Refreshing,
    Pinging,
}

pub fn create_provider_catalog_machine(input: Option<CatalogSnapshot>) -> Arc<ProviderCatalogMachine> {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    Arc::new(ProviderCatalogMachine {
        context: Arc::new(Mutex::new(ProviderCatalogContext {
            snapshot: input.unwrap_or_default(),
            ..Default::default()
        })),
        sender,
        receiver: Arc::new(Mutex::new(Some(receiver))),
        listeners: Arc::new(Mutex::new(IndexMap::new())),
        next_listener_id: AtomicU64::new(0),
        started: Arc::new(Mutex::new(None)),
    })
}

pub struct ProviderCatalog {
    actor: Arc<ProviderCatalogMachine>,
    live: Arc<Mutex<HashMap<String, Arc<dyn Provider>>>>,
}

impl ProviderCatalog {
    pub fn providers(&self) -> Vec<String> {
        self.actor.snapshot().providers.keys().cloned().collect()
    }

    pub fn provider_info(&self, provider_id: &str) -> Option<CatalogProviderInfo> {
        self.actor
            .snapshot()
            .providers
            .get(provider_id)
            .and_then(|entry| entry.info.clone())
    }

    pub fn models(&self, provider_id: &str) -> Vec<CatalogModel> {
        self.actor
            .snapshot()
            .providers
            .get(provider_id)
            .map(merge_entry_models)
            .unwrap_or_default()
    }

    pub fn upsert(
        &self,
        provider: Arc<dyn Provider>,
        info: Option<CatalogProviderInfo>,
        models: Option<Vec<CatalogModelDefinition>>,
    ) {
        self.live.lock().insert(provider.id().to_string(), provider.clone());
        self.actor.send(ProviderCatalogEvent::Upsert {
            provider_id: provider.id().to_string(),
            info,
            models,
        });
        self.actor.send(ProviderCatalogEvent::Refresh {
            providers: vec![provider],
        });
    }

    pub fn upsert_entry(
        &self,
        provider_id: &str,
        info: Option<CatalogProviderInfo>,
        models: Option<Vec<CatalogModelDefinition>>,
    ) {
        self.actor.send(ProviderCatalogEvent::Upsert {
            provider_id: provider_id.to_string(),
            info,
            models,
        });
    }

    pub fn remove(&self, provider_id: &str) {
        self.live.lock().remove(provider_id);
        self.actor.send(ProviderCatalogEvent::Remove {
            provider_id: provider_id.to_string(),
        });
    }

    pub fn refresh(&self, provider: Arc<dyn Provider>) {
        self.live.lock().insert(provider.id().to_string(), provider.clone());
        self.actor.send(ProviderCatalogEvent::Refresh {
            providers: vec![provider],
        });
    }

    pub fn ping(&self, provider_id: &str, model: &str) {
        let provider = self.live.lock().get(provider_id).cloned();
        if let Some(provider) = provider {
            self.actor.send(ProviderCatalogEvent::Ping {
                provider,
                model: model.to_string(),
            });
        }
    }

    pub fn on_changed(
        &self,
        listener: Arc<dyn Fn(Vec<String>) + Send + Sync>,
    ) -> Box<dyn FnOnce() + Send> {
        self.actor.on(Arc::new(move |event| {
            if let ProviderCatalogEmitted::Changed { providers } = event {
                listener(providers);
            }
        }))
    }

    pub fn on_refresh_failed(
        &self,
        listener: Arc<dyn Fn(String, String) + Send + Sync>,
    ) -> Box<dyn FnOnce() + Send> {
        self.actor.on(Arc::new(move |event| {
            if let ProviderCatalogEmitted::RefreshFailed { provider_id, error } = event {
                listener(provider_id, error);
            }
        }))
    }

    pub fn stop(&self) {
        self.live.lock().clear();
        self.actor.stop();
    }
}

pub async fn create_provider_catalog(
    store: Option<Arc<dyn ProviderCatalogStore>>,
    snapshot: Option<CatalogSnapshot>,
) -> Result<ProviderCatalog, WireAsyncError> {
    let loaded = match snapshot {
        Some(snapshot) => Some(snapshot),
        None => match &store {
            Some(store) => store.load().await?,
            None => None,
        },
    };
    let catalog = create_provider_catalog_sync(loaded);
    if let Some(store) = store {
        let actor = catalog.actor.clone();
        catalog.actor.on(Arc::new(move |event| {
            if !matches!(&event, ProviderCatalogEmitted::Changed { .. }) {
                return;
            }
            let snapshot = actor.snapshot();
            let store = store.clone();
            tokio::spawn(async move {
                let _ = store.save(snapshot).await;
            });
        }));
    }
    catalog.actor.start();
    Ok(catalog)
}

pub fn create_provider_catalog_sync(snapshot: Option<CatalogSnapshot>) -> ProviderCatalog {
    let catalog = ProviderCatalog {
        actor: create_provider_catalog_machine(snapshot),
        live: Arc::new(Mutex::new(HashMap::new())),
    };
    catalog.actor.start();
    catalog
}

fn apply_pull_results(snapshot: &mut CatalogSnapshot, results: &[PullResult]) {
    for result in results {
        let Some(models) = &result.models else {
            continue;
        };
        let entry = snapshot.providers.get(&result.provider_id).cloned();
        snapshot.providers.insert(
            result.provider_id.clone(),
            CatalogProviderEntry {
                info: entry.as_ref().and_then(|entry| entry.info.clone()),
                override_: entry
                    .as_ref()
                    .map(|entry| entry.override_.clone())
                    .unwrap_or_default(),
                discovered: models
                    .iter()
                    .map(|model| (model.model.clone(), model.clone()))
                    .collect(),
                ping_errors: entry.and_then(|entry| entry.ping_errors),
            },
        );
    }
}

fn pull_failures(results: &[PullResult]) -> Vec<(String, String)> {
    results
        .iter()
        .filter(|result| result.models.is_none())
        .map(|result| {
            (
                result.provider_id.clone(),
                result.error.clone().unwrap_or_default(),
            )
        })
        .collect()
}

fn merge_dirty(dirty: &[String], results: &[PullResult]) -> Vec<String> {
    let mut output = dirty.to_vec();
    output.extend(
        results
            .iter()
            .filter(|result| result.models.is_some())
            .map(|result| result.provider_id.clone()),
    );
    output.sort();
    output.dedup();
    output
}

fn enqueue_providers(
    batch: &[Arc<dyn Provider>],
    queue: &[Arc<dyn Provider>],
    incoming: Vec<Arc<dyn Provider>>,
) -> Vec<Arc<dyn Provider>> {
    let active = batch
        .iter()
        .chain(queue)
        .map(|provider| provider.id().to_string())
        .collect::<std::collections::HashSet<_>>();
    let mut output = queue.to_vec();
    output.extend(
        incoming
            .into_iter()
            .filter(|provider| !active.contains(provider.id())),
    );
    output
}

fn apply_ping_outcome(snapshot: &mut CatalogSnapshot, outcome: &PingOutcome) -> bool {
    let Some(entry) = snapshot.providers.get_mut(&outcome.provider_id) else {
        return false;
    };
    if entry.ping_errors.as_ref().and_then(|errors| errors.get(&outcome.model)) == outcome.error.as_ref() {
        return false;
    }
    let mut ping_errors = entry.ping_errors.clone().unwrap_or_default();
    if let Some(error) = &outcome.error {
        ping_errors.insert(outcome.model.clone(), error.clone());
    } else {
        ping_errors.remove(&outcome.model);
    }
    entry.ping_errors = Some(ping_errors);
    true
}

fn resolve_catalog_model(
    entry: Option<&CatalogProviderEntry>,
    model_id: &str,
) -> Option<CatalogModelDefinition> {
    let entry = entry?;
    if let Some(override_) = entry.override_.get(model_id) {
        return Some(merge_model(
            override_.clone(),
            entry.discovered.get(model_id),
        ));
    }
    entry
        .discovered
        .get(model_id)
        .map(|discovered| merge_model(discovered.clone().into(), None))
}

impl From<LlmModel> for CatalogModelDefinition {
    fn from(model: LlmModel) -> Self {
        Self {
            model,
            ..Default::default()
        }
    }
}

fn merge_entry_models(entry: &CatalogProviderEntry) -> Vec<CatalogModel> {
    let attach = |model: CatalogModelDefinition| {
        let ping_error = entry
            .ping_errors
            .as_ref()
            .and_then(|errors| errors.get(&model.model).cloned());
        CatalogModel {
            definition: model,
            ping_error,
        }
    };
    let mut merged = entry
        .override_
        .values()
        .map(|record| attach(merge_model(record.clone(), entry.discovered.get(&record.model))))
        .collect::<Vec<_>>();
    merged.extend(
        entry
            .discovered
            .values()
            .filter(|model| !entry.override_.contains_key(&model.model))
            .map(|model| attach(merge_model(model.clone().into(), None))),
    );
    merged.sort_by(|left, right| left.model.cmp(&right.model));
    merged
}

fn enqueue_ping(
    active: Option<PingRequest>,
    queue: &[PingRequest],
    request: PingRequest,
) -> Vec<PingRequest> {
    let key = |ping: &PingRequest| format!("{}#{}", ping.provider.id(), ping.model.model);
    if active.as_ref().is_some_and(|active| key(active) == key(&request))
        || queue.iter().any(|ping| key(ping) == key(&request))
    {
        return queue.to_vec();
    }
    let mut output = queue.to_vec();
    output.push(request);
    output
}

async fn pull_batch(providers: Vec<Arc<dyn Provider>>) -> Vec<PullResult> {
    let mut pending = Vec::new();
    for provider in providers {
        pending.push(async move {
            match provider.list_models().await {
                Ok(models) => PullResult {
                    provider_id: provider.id().to_string(),
                    models: Some(models),
                    error: None,
                },
                Err(error) => PullResult {
                    provider_id: provider.id().to_string(),
                    models: None,
                    error: Some(error.to_string()),
                },
            }
        });
    }
    futures_util::future::join_all(pending).await
}

async fn run_ping_probe(
    provider: Arc<dyn Provider>,
    model: CatalogModelDefinition,
) -> Option<String> {
    let requester = match provider.create_requester(model.protocol) {
        Ok(requester) => requester,
        Err(error) => return Some(error.to_string()),
    };
    let failure = Arc::new(Mutex::new(None));
    let failure_sink = failure.clone();
    let result = requester
        .generate(
            LlmRequestConfig {
                model: model.clone(),
                system_prompt: Some("You are a connectivity probe. Answer with the single word \"pong\".".to_string()),
                tools: Some(Vec::new()),
                max_completion_tokens: Some(512),
                ..Default::default()
            },
            LlmRequestContent {
                messages: vec![Message::User(create_user_message("ping"))],
                ..Default::default()
            },
            LlmRequestControl {
                signal: tokio_util::sync::CancellationToken::new(),
                on_event: Arc::new(move |event| {
                    if let LlmEvent::FailedSyntax { error } | LlmEvent::FailedRemote { error } = event {
                        *failure_sink.lock() = Some(error.message);
                    }
                }),
            },
        )
        .await;
    if let Err(error) = result {
        return Some(error.to_string());
    }
    failure.lock().clone()
}

fn merge_capability(
    discovered: Option<&ModelCapability>,
    override_: Option<&ModelCapability>,
) -> ModelCapability {
    let Some(discovered) = discovered else {
        return override_.cloned().unwrap_or(UNKNOWN_CAPABILITY);
    };
    let Some(override_) = override_ else {
        return discovered.clone();
    };
    ModelCapability {
        image_in: discovered.image_in || override_.image_in,
        video_in: discovered.video_in || override_.video_in,
        audio_in: discovered.audio_in || override_.audio_in,
        thinking: discovered.thinking || override_.thinking,
        tool_use: discovered.tool_use || override_.tool_use,
        pdf_in: Some(discovered.pdf_in == Some(true) || override_.pdf_in == Some(true)),
        dynamically_loaded_tools: Some(
            discovered.dynamically_loaded_tools == Some(true)
                || override_.dynamically_loaded_tools == Some(true),
        ),
        unknown_capability: false,
    }
}

fn clamp_max_input_size(mut model: CatalogModelDefinition) -> CatalogModelDefinition {
    if let (Some(input), Some(context)) = (model.max_input_size, model.max_context_size)
        && input > context
    {
        model.max_input_size = Some(context);
    }
    model
}

fn apply_model_overrides(
    mut model: CatalogModelDefinition,
    overrides: Option<&CatalogModelOverrides>,
) -> CatalogModelDefinition {
    let Some(overrides) = overrides else {
        return model;
    };
    if overrides.max_context_size.is_some() {
        model.max_context_size = overrides.max_context_size;
    }
    if overrides.max_input_size.is_some() {
        model.max_input_size = overrides.max_input_size;
    }
    if overrides.max_output_size.is_some() {
        model.max_output_size = overrides.max_output_size;
    }
    if let Some(capability) = &overrides.capability {
        model.capability = capability.clone();
    }
    if overrides.display_name.is_some() {
        model.display_name = overrides.display_name.clone();
    }
    if overrides.reasoning_key.is_some() {
        model.reasoning_key = overrides.reasoning_key.clone();
    }
    if overrides.adaptive_thinking.is_some() {
        model.adaptive_thinking = overrides.adaptive_thinking;
    }
    if let Some(support) = &overrides.support_efforts {
        model.support_efforts = Some(support.clone());
        if overrides.default_effort.is_none()
            && model
                .default_effort
                .as_ref()
                .is_some_and(|default| !support.contains(default))
        {
            model.default_effort = None;
        }
    }
    if overrides.default_effort.is_some() {
        model.default_effort = overrides.default_effort.clone();
    }
    if overrides.off_effort.is_some() {
        model.off_effort = overrides.off_effort.clone();
    }
    if overrides.always_thinking.is_some() {
        model.always_thinking = overrides.always_thinking;
    }
    clamp_max_input_size(model)
}

fn merge_model(
    mut merged: CatalogModelDefinition,
    discovered: Option<&LlmModel>,
) -> CatalogModelDefinition {
    merged.capability = merge_capability(
        discovered.map(|model| &model.capability),
        Some(&merged.capability),
    );
    merged.max_context_size = merged.max_context_size.or_else(|| discovered.and_then(|model| model.max_context_size));
    merged.max_input_size = merged.max_input_size.or_else(|| discovered.and_then(|model| model.max_input_size));
    merged.connection.base_url = merged
        .connection
        .base_url
        .take()
        .or_else(|| discovered.and_then(|model| model.connection.base_url.clone()));
    merged.connection.api_key = merged
        .connection
        .api_key
        .take()
        .or_else(|| discovered.and_then(|model| model.connection.api_key.clone()));
    merged.connection.default_headers = merged
        .connection
        .default_headers
        .take()
        .or_else(|| discovered.and_then(|model| model.connection.default_headers.clone()));
    let overrides = merged.overrides.clone();
    merged.overrides = None;
    apply_model_overrides(merged, overrides.as_ref())
}
