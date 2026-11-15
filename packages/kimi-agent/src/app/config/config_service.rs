use std::collections::{BTreeMap, HashMap};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use indexmap::{IndexMap, IndexSet};
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde_json::{Map, Value, json};

use crate::_base::di::collection::CollectionViewImpl;
use crate::_base::di::descriptors::{Constructor, DynamicValue, SyncDescriptor};
use crate::_base::di::instantiation::{
    DependencyKind, ScopeActivation, ServiceDependency, create_decorator,
    refine_service_decorator,
};
use crate::_base::di::instantiation_service::register_descriptor_dependencies;
use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::_base::di::scope::register_scoped_service;
use crate::_base::errors::errors::BugIndicatingError;
use crate::_base::errors::unexpected_error::on_unexpected_error;
use crate::_base::event::Emitter;
use crate::_base::log::log::{I_LOG_SERVICE, ILogService};
use crate::app::bootstrap::bootstrap::{I_BOOTSTRAP_SERVICE, IBootstrapService};
use crate::app::config::config_overlay_contributions::get_config_overlay_contributions;
use crate::app::config::config_section_contributions::{
    ConfigSectionContribution, get_config_section_contributions,
};
use crate::app::scopes::LifecycleScope;
use crate::human::utils::watch::{
    WatchHandle, WatchSubscription, set_watch_enabled, watch_candidates,
};
use crate::persistence::interface::atomic_document_store::IAtomicTomlDocumentStore;
use crate::wire::record::WireAsyncError;
use kimi_core::primitives::errors::{Error2, Error2Options, domains::core::INTERNAL};

use super::config::*;
use super::config_pure::{deep_equal, deep_merge};
use super::deprecations::collect_key_deprecations;
use super::errors::{CONFIG_INVALID, CONFIG_PERSIST_BLOCKED, register_config_errors};
use super::toml::{
    apply_section_to_toml, camel_to_snake, clone_record, describe_toml_syntax_error,
    parse_toml_document, transform_toml_data,
};
use super::toml_writeback::{DomainUpdate, plan_config_writeback};

const CONFIG_SCOPE: &str = "";
const WATCH_DEBOUNCE_MS: u64 = 150;
const WATCH_SECTION: &str = "watch";

pub struct ConfigRegistry {
    sections: Mutex<IndexMap<String, ConfigSection>>,
    overlays: Mutex<Vec<ConfigEffectiveOverlay>>,
    fold_domains: Mutex<IndexSet<String>>,
    on_register_section: Emitter<ConfigSectionRegisteredEvent>,
    on_unregister_section: Emitter<ConfigSectionRegisteredEvent>,
    on_register_overlay: Emitter<ConfigOverlayRegisteredEvent>,
    view_subscriptions: Mutex<Vec<DisposableRef>>,
}

impl ConfigRegistry {
    pub fn new() -> Arc<Self> {
        register_config_errors();
        let registry = Arc::new(Self {
            sections: Mutex::new(IndexMap::new()),
            overlays: Mutex::new(Vec::new()),
            fold_domains: Mutex::new(IndexSet::new()),
            on_register_section: Emitter::new(),
            on_unregister_section: Emitter::new(),
            on_register_overlay: Emitter::new(),
            view_subscriptions: Mutex::new(Vec::new()),
        });
        for contribution in get_config_section_contributions() {
            registry
                .register_section(&contribution.domain, contribution.schema, contribution.options)
                .unwrap_or_else(|error| panic!("{error}"));
        }
        for overlay in get_config_overlay_contributions() {
            registry.register_effective_overlay(overlay);
        }
        registry
    }

    pub fn with_view(view: Arc<CollectionViewImpl<ConfigSectionContribution>>) -> Arc<Self> {
        let registry = Self::new();
        for contribution in view.items() {
            registry.add_contribution(contribution);
        }
        let weak = Arc::downgrade(&registry);
        let subscription = view.on_did_change_event.subscribe(move |change| {
            let Some(registry) = weak.upgrade() else {
                return;
            };
            for contribution in change.removed {
                registry.remove_contribution(&contribution);
            }
            for contribution in change.added {
                registry.add_contribution(contribution);
            }
        });
        registry.view_subscriptions.lock().push(Arc::new(subscription));
        registry
    }

    pub fn empty() -> Arc<Self> {
        Arc::new(Self {
            sections: Mutex::new(IndexMap::new()),
            overlays: Mutex::new(Vec::new()),
            fold_domains: Mutex::new(IndexSet::new()),
            on_register_section: Emitter::new(),
            on_unregister_section: Emitter::new(),
            on_register_overlay: Emitter::new(),
            view_subscriptions: Mutex::new(Vec::new()),
        })
    }

    pub fn add_contribution(&self, contribution: ConfigSectionContribution) {
        let before = self.get_section(&contribution.domain);
        if let Err(error) = self.register_section(
            &contribution.domain,
            contribution.schema,
            contribution.options,
        ) {
            on_unexpected_error(error.to_string());
            return;
        }
        if before.is_none() && self.get_section(&contribution.domain).is_some() {
            self.fold_domains.lock().insert(contribution.domain);
        }
    }

    pub fn remove_contribution(&self, contribution: &ConfigSectionContribution) {
        if self.fold_domains.lock().shift_remove(&contribution.domain) {
            self.unregister_section(&contribution.domain);
        }
    }
}

impl IDisposable for ConfigRegistry {
    fn dispose(&self) {
        for subscription in self.view_subscriptions.lock().drain(..) {
            subscription.dispose();
        }
        self.on_register_section.dispose();
        self.on_unregister_section.dispose();
        self.on_register_overlay.dispose();
    }
}

fn subscribe<T>(emitter: &Emitter<T>, listener: Box<dyn Fn(T) + Send + Sync>) -> DisposableRef
where
    T: Clone + Send + Sync + 'static,
{
    Arc::new(emitter.event().subscribe_arc(Arc::from(listener)))
}

impl IConfigRegistry for ConfigRegistry {
    fn on_did_register_section(
        &self,
        listener: ConfigSectionRegisteredListener,
    ) -> DisposableRef {
        subscribe(&self.on_register_section, listener)
    }

    fn on_did_unregister_section(
        &self,
        listener: ConfigSectionRegisteredListener,
    ) -> DisposableRef {
        subscribe(&self.on_unregister_section, listener)
    }

    fn on_did_register_overlay(
        &self,
        listener: ConfigOverlayRegisteredListener,
    ) -> DisposableRef {
        subscribe(&self.on_register_overlay, listener)
    }

    fn register_section(
        &self,
        domain: &str,
        schema: Arc<dyn ConfigSchema>,
        options: RegisterSectionOptions,
    ) -> Result<(), BugIndicatingError> {
        let section = ConfigSection {
            domain: domain.to_string(),
            schema,
            default_value: options.default_value.clone(),
            merge: options.merge.clone().unwrap_or_else(default_config_merge),
            scope: options.scope.unwrap_or(ConfigScope::Core),
            env: options.env.clone(),
            strip_env: options.strip_env.clone(),
            from_toml: options.from_toml.clone(),
            to_toml: options.to_toml.clone(),
            deprecations: options.deprecations.clone(),
            collect_diagnostics: options.collect_diagnostics.clone(),
        };
        {
            let mut sections = self.sections.lock();
            if let Some(existing) = sections.get(domain) {
                if is_same_section(existing, &section) {
                    return Ok(());
                }
                return Err(BugIndicatingError::new(
                    format!("ConfigRegistry: section '{domain}' is already registered"),
                ));
            }
            sections.insert(domain.to_string(), section);
        }
        self.on_register_section.fire(ConfigSectionRegisteredEvent {
            domain: domain.to_string(),
        });
        Ok(())
    }

    fn unregister_section(&self, domain: &str) {
        let removed = self.sections.lock().shift_remove(domain).is_some();
        if removed {
            self.on_unregister_section.fire(ConfigSectionRegisteredEvent {
                domain: domain.to_string(),
            });
        }
    }

    fn get_section(&self, domain: &str) -> Option<ConfigSection> {
        self.sections.lock().get(domain).cloned()
    }

    fn list_sections(&self) -> Vec<ConfigSection> {
        self.sections.lock().values().cloned().collect()
    }

    fn register_effective_overlay(&self, overlay: ConfigEffectiveOverlay) {
        self.overlays.lock().push(overlay.clone());
        self.on_register_overlay.fire(ConfigOverlayRegisteredEvent { overlay });
    }

    fn list_effective_overlays(&self) -> Vec<ConfigEffectiveOverlay> {
        self.overlays.lock().clone()
    }

    fn validate(&self, domain: &str, value: Value) -> Result<Value, WireAsyncError> {
        let section = self.sections.lock().get(domain).cloned();
        match section {
            Some(section) => section.schema.parse(value).map_err(|error| Box::new(error) as WireAsyncError),
            None => Ok(value),
        }
    }

    fn merge(&self, domain: &str, base: Option<&Value>, patch: Option<&Value>) -> Option<Value> {
        let section = self.sections.lock().get(domain).cloned();
        match section {
            Some(section) => (section.merge)(base, patch),
            None => deep_merge(base, patch),
        }
    }

    fn default_value(&self, domain: &str) -> Option<Value> {
        self.sections.lock().get(domain).and_then(|section| section.default_value.clone())
    }
}

fn is_same_section(existing: &ConfigSection, next: &ConfigSection) -> bool {
    Arc::ptr_eq(&existing.schema, &next.schema)
        && Arc::ptr_eq(&existing.merge, &next.merge)
        && existing.scope == next.scope
        && match (&existing.env, &next.env) {
            (None, None) => true,
            (Some(a), Some(b)) => env_bindings_equal(a, b),
            _ => false,
        }
        && match (&existing.strip_env, &next.strip_env) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
        && match (&existing.from_toml, &next.from_toml) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
        && match (&existing.to_toml, &next.to_toml) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
        && optional_deep_equal(existing.default_value.as_ref(), next.default_value.as_ref())
        && existing.deprecations == next.deprecations
        && match (&existing.collect_diagnostics, &next.collect_diagnostics) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
}

fn env_bindings_equal(a: &EnvBindings, b: &EnvBindings) -> bool {
    match (a, b) {
        (EnvBindings::Name(a), EnvBindings::Name(b)) => a == b,
        (EnvBindings::Binding(a), EnvBindings::Binding(b)) => {
            a.env == b.env
                && a.deprecated_env == b.deprecated_env
                && a.default == b.default
                && match (&a.parse, &b.parse) {
                    (None, None) => true,
                    (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                    _ => false,
                }
        }
        (EnvBindings::Fields(a), EnvBindings::Fields(b)) => {
            a.len() == b.len()
                && a.iter().zip(b.iter()).all(|((a_key, a), (b_key, b))| {
                    a_key == b_key && env_bindings_equal(a, b)
                })
        }
        _ => false,
    }
}

#[derive(Clone)]
struct EnvSectionResolution {
    base: Option<Value>,
    env_values: Vec<Option<String>>,
    value: Option<Value>,
}

#[derive(Default)]
struct ConfigState {
    raw_snake: Map<String, Value>,
    raw: ResolvedConfig,
    validated: ResolvedConfig,
    effective: ResolvedConfig,
    memory: ResolvedConfig,
    delivered: IndexMap<String, Option<Value>>,
    diagnostics: Vec<ConfigDiagnostic>,
    last_diagnostics_snapshot: String,
    read_error: Option<String>,
    env_resolutions: HashMap<String, EnvSectionResolution>,
}

enum CommitEvent {
    Configuration(ConfigChangedEvent),
    Section(ConfigSectionChangedEvent),
}

#[derive(Default)]
struct PendingEvents {
    commits: Vec<CommitEvent>,
    diagnostics: Option<Vec<ConfigDiagnostic>>,
}

pub struct ConfigService {
    registry: Arc<dyn IConfigRegistry>,
    bootstrap: Arc<dyn IBootstrapService>,
    log: Arc<dyn ILogService>,
    document_store: Arc<dyn IAtomicTomlDocumentStore>,
    config_key: String,
    state: Mutex<ConfigState>,
    state_chain: tokio::sync::Mutex<()>,
    ready_cell: tokio::sync::OnceCell<Result<(), String>>,
    on_change_configuration: Emitter<ConfigChangedEvent>,
    on_section_change: Emitter<ConfigSectionChangedEvent>,
    on_change_diagnostics: Emitter<Vec<ConfigDiagnostic>>,
    subscriptions: Mutex<Vec<DisposableRef>>,
    watch_handle: Mutex<Option<Box<WatchHandle>>>,
    watch_subscription: Mutex<Option<Box<WatchSubscription>>>,
    watch_debounce: Mutex<Option<tokio::task::JoinHandle<()>>>,
    self_weak: Weak<ConfigService>,
}

impl ConfigService {
    pub fn new(
        registry: Arc<dyn IConfigRegistry>,
        bootstrap: Arc<dyn IBootstrapService>,
        log: Arc<dyn ILogService>,
        document_store: Arc<dyn IAtomicTomlDocumentStore>,
    ) -> Arc<Self> {
        register_config_errors();
        let config_key = bootstrap.config_key();
        Arc::new_cyclic(|weak| {
            let mut state = ConfigState {
                last_diagnostics_snapshot: "[]".to_string(),
                ..ConfigState::default()
            };
            seed_initial_load(registry.as_ref(), bootstrap.as_ref(), &mut state);
            let service = Self {
                registry,
                bootstrap,
                log,
                document_store,
                config_key,
                state: Mutex::new(state),
                state_chain: tokio::sync::Mutex::new(()),
                ready_cell: tokio::sync::OnceCell::new(),
                on_change_configuration: Emitter::new(),
                on_section_change: Emitter::new(),
                on_change_diagnostics: Emitter::new(),
                subscriptions: Mutex::new(Vec::new()),
                watch_handle: Mutex::new(None),
                watch_subscription: Mutex::new(None),
                watch_debounce: Mutex::new(None),
                self_weak: weak.clone(),
            };
            service.initialize_seed_effective();
            service.apply_watch_enabled();
            service.register_registry_listeners();
            service.start_watcher();
            service
        })
    }

    fn initialize_seed_effective(&self) {
        let mut state = self.state.lock();
        let mut next = state.validated.clone();
        apply_section_env_bindings(
            self.registry.as_ref(),
            self.bootstrap.as_ref(),
            &mut next,
            true,
            &mut state,
        );
        apply_env_overlay(
            self.registry.as_ref(),
            self.bootstrap.as_ref(),
            &mut next,
            true,
            &mut state,
        );
        state.effective = next;
    }

    fn register_registry_listeners(&self) {
        let weak = self.self_weak.clone();
        self.subscriptions.lock().push(self.registry.on_did_register_section(Box::new(move |event| {
            if let Some(this) = weak.upgrade() {
                this.revalidate_domain(&event.domain);
            }
        })));
        let weak = self.self_weak.clone();
        self.subscriptions.lock().push(self.registry.on_did_unregister_section(Box::new(move |event| {
            if let Some(this) = weak.upgrade() {
                this.devalidate_domain(&event.domain);
            }
        })));
        let weak = self.self_weak.clone();
        self.subscriptions.lock().push(self.registry.on_did_register_overlay(Box::new(move |_| {
            if let Some(this) = weak.upgrade() {
                this.reapply_overlays();
            }
        })));
    }

    fn start_watcher(&self) {
        let home_dir = self.bootstrap.home_dir();
        let config_file = crate::app::bootstrap::home::join(&home_dir, &self.config_key);
        let handle = watch_candidates(&home_dir, &[config_file.clone()], None);
        let weak = self.self_weak.clone();
        let subscription = handle.on_did_change(move |change| {
            if normalize_path(&change.path) != normalize_path(&config_file) {
                return;
            }
            let Some(this) = weak.upgrade() else {
                return;
            };
            if let Some(task) = this.watch_debounce.lock().take() {
                task.abort();
            }
            let weak = this.self_weak.clone();
            *this.watch_debounce.lock() = Some(tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(WATCH_DEBOUNCE_MS)).await;
                if let Some(this) = weak.upgrade() {
                    let _ = this.reload().await;
                }
            }));
        });
        *self.watch_handle.lock() = Some(handle);
        *self.watch_subscription.lock() = Some(subscription);
    }

    pub async fn ready(&self) -> Result<(), WireAsyncError> {
        let this = self.self_weak.upgrade().ok_or_else(|| {
            Box::new(Error2::new(INTERNAL, "config service is no longer available")) as WireAsyncError
        })?;
        self.ready_cell
            .get_or_init(|| async move {
                this.load(ConfigChangeSource::Load)
                    .await
                    .map_err(|error| error.to_string())
            })
            .await
            .clone()
            .map_err(|error| Box::new(Error2::new(INTERNAL, error)) as WireAsyncError)
    }

    pub fn get(&self, domain: &str) -> Option<Value> {
        if let Some(value) = self.state.lock().memory.get(domain).cloned() {
            return Some(value);
        }
        self.fresh_effective().get(domain).cloned()
    }

    pub fn inspect(&self, domain: &str) -> ConfigInspectValue {
        let (user_value, memory_value) = {
            let state = self.state.lock();
            (state.raw.get(domain).cloned(), state.memory.get(domain).cloned())
        };
        ConfigInspectValue {
            value: self.get(domain),
            default_value: self.registry.default_value(domain),
            user_value,
            memory_value,
        }
    }

    pub async fn assert_valid(&self, domain: &str) -> Result<(), WireAsyncError> {
        let state = self.state.lock();
        if state.read_error.is_some() {
            return Err(config_error(
                CONFIG_INVALID,
                format!(
                    "Cannot use config section '{domain}' because {} could not be read",
                    self.bootstrap.config_path()
                ),
                None,
            ));
        }
        let Some(value) = state.raw.get(domain).cloned() else {
            return Ok(());
        };
        drop(state);
        self.registry.validate(domain, value).map(|_| ()).map_err(|error| {
            let message = format!("Invalid config section '{domain}': {error}");
            config_error(CONFIG_INVALID, message, Some(error))
        })
    }

    pub fn get_all(&self) -> ResolvedConfig {
        let mut effective = self.fresh_effective();
        for (domain, value) in self.state.lock().memory.iter() {
            effective.insert(domain.clone(), value.clone());
        }
        effective
    }

    fn fresh_effective(&self) -> ResolvedConfig {
        let mut state = self.state.lock();
        let mut effective = state.validated.clone();
        apply_section_env_bindings(
            self.registry.as_ref(),
            self.bootstrap.as_ref(),
            &mut effective,
            false,
            &mut state,
        );
        apply_env_overlay(
            self.registry.as_ref(),
            self.bootstrap.as_ref(),
            &mut effective,
            false,
            &mut state,
        );
        effective
    }

    pub fn diagnostics(&self) -> Vec<ConfigDiagnostic> {
        self.state.lock().diagnostics.clone()
    }

    pub fn on_did_change_configuration(
        &self,
        listener: impl Fn(ConfigChangedEvent) + Send + Sync + 'static,
    ) -> DisposableRef {
        subscribe(&self.on_change_configuration, Box::new(listener))
    }

    pub fn on_did_section_change(
        &self,
        listener: impl Fn(ConfigSectionChangedEvent) + Send + Sync + 'static,
    ) -> DisposableRef {
        subscribe(&self.on_section_change, Box::new(listener))
    }

    pub fn on_did_change_diagnostics(
        &self,
        listener: impl Fn(Vec<ConfigDiagnostic>) + Send + Sync + 'static,
    ) -> DisposableRef {
        subscribe(&self.on_change_diagnostics, Box::new(listener))
    }

    pub async fn set(
        &self,
        domain: &str,
        patch: Option<Value>,
        target: Option<ConfigTarget>,
    ) -> Result<(), WireAsyncError> {
        self.ready().await?;
        if target.unwrap_or(ConfigTarget::User) == ConfigTarget::Memory {
            let base = self.state.lock().memory.get(domain).cloned();
            let next = self.registry.merge(domain, base.as_ref(), patch.as_ref());
            let validated = match next {
                Some(value) => Some(self.registry.validate(domain, value)?),
                None => None,
            };
            let pending = {
                let mut state = self.state.lock();
                match validated {
                    Some(value) => {
                        state.memory.insert(domain.to_string(), value);
                    }
                    None => {
                        state.memory.shift_remove(domain);
                    }
                }
                commit(&mut state, ConfigChangeSource::Set, &[domain.to_string()])
            };
            self.fire_pending(pending);
            return Ok(());
        }
        let _guard = self.state_chain.lock().await;
        self.assert_persistable()?;
        let domains = vec![domain.to_string()];
        let (raw, raw_snake) = self
            .persist_domains(
                &domains,
                |staged_raw, staged_raw_snake| {
                    let next = self.registry.merge(domain, staged_raw.get(domain), patch.as_ref());
                    let validated = match next {
                        Some(value) => Some(self.registry.validate(domain, value)?),
                        None => None,
                    };
                    let stripped = self.strip_env(domain, validated, staged_raw, staged_raw_snake)?;
                    match stripped {
                        Some(stripped) => {
                            self.registry.validate(domain, stripped.clone())?;
                            staged_raw.insert(domain.to_string(), stripped);
                        }
                        None => {
                            staged_raw.shift_remove(domain);
                        }
                    }
                    Ok(())
                },
                true,
                &BTreeMap::new(),
                &BTreeMap::new(),
            )
            .await?;
        let pending = {
            let mut state = self.state.lock();
            state.raw = raw;
            state.raw_snake = raw_snake;
            rebuild_effective(
                self.registry.as_ref(),
                self.bootstrap.as_ref(),
                &mut state,
                ConfigChangeSource::Set,
                Some(&domains),
            )
        };
        self.apply_watch_enabled();
        self.fire_pending(pending);
        Ok(())
    }

    pub async fn replace(
        &self,
        domain: &str,
        value: Option<Value>,
        target: Option<ConfigTarget>,
    ) -> Result<(), WireAsyncError> {
        self.ready().await?;
        let effective_value = value.filter(|value| !value.is_null());
        if target.unwrap_or(ConfigTarget::User) == ConfigTarget::Memory {
            let validated = match effective_value {
                Some(value) => Some(self.registry.validate(domain, value)?),
                None => None,
            };
            let pending = {
                let mut state = self.state.lock();
                match validated {
                    Some(value) => {
                        state.memory.insert(domain.to_string(), value);
                    }
                    None => {
                        state.memory.shift_remove(domain);
                    }
                }
                commit(&mut state, ConfigChangeSource::Set, &[domain.to_string()])
            };
            self.fire_pending(pending);
            return Ok(());
        }
        let _guard = self.state_chain.lock().await;
        self.assert_persistable()?;
        let domains = vec![domain.to_string()];
        let (raw, raw_snake) = self
            .persist_domains(
                &domains,
                |staged_raw, staged_raw_snake| {
                    let stripped = self.strip_env(domain, effective_value.clone(), staged_raw, staged_raw_snake)?;
                    match stripped {
                        Some(stripped) => {
                            let validated = self.registry.validate(domain, stripped)?;
                            staged_raw.insert(domain.to_string(), validated);
                        }
                        None => {
                            staged_raw.shift_remove(domain);
                        }
                    }
                    Ok(())
                },
                true,
                &BTreeMap::new(),
                &BTreeMap::new(),
            )
            .await?;
        let pending = {
            let mut state = self.state.lock();
            state.raw = raw;
            state.raw_snake = raw_snake;
            rebuild_effective(
                self.registry.as_ref(),
                self.bootstrap.as_ref(),
                &mut state,
                ConfigChangeSource::Set,
                Some(&domains),
            )
        };
        self.apply_watch_enabled();
        self.fire_pending(pending);
        Ok(())
    }

    pub async fn replace_sections(
        &self,
        sections: BTreeMap<String, Option<Value>>,
        target: Option<ConfigTarget>,
        options: Option<ConfigReplaceSectionsOptions>,
    ) -> Result<(), WireAsyncError> {
        self.ready().await?;
        let options = options.unwrap_or_default();
        let domains = sections.keys().cloned().collect::<Vec<_>>();
        if domains.is_empty() {
            return Ok(());
        }
        if target.unwrap_or(ConfigTarget::User) == ConfigTarget::Memory {
            let current = self.state.lock().memory.clone();
            self.assert_expected_values(&current, &options.expected_values)?;
            let mut staged = current;
            for domain in &domains {
                let submitted = sections.get(domain).cloned().flatten();
                let Some(submitted) = submitted.filter(|value| !value.is_null()) else {
                    staged.shift_remove(domain);
                    continue;
                };
                let value = match options.exact_keys.get(domain).filter(|_| submitted.is_object()) {
                    Some(exact) => merge_exact_entries(staged.get(domain), submitted.as_object().unwrap(), exact),
                    None => submitted,
                };
                staged.insert(domain.clone(), self.registry.validate(domain, value)?);
            }
            let pending = {
                let mut state = self.state.lock();
                state.memory = staged;
                commit(&mut state, ConfigChangeSource::Set, &domains)
            };
            self.fire_pending(pending);
            return Ok(());
        }
        let _guard = self.state_chain.lock().await;
        self.assert_persistable()?;
        let (raw, raw_snake) = self
            .persist_domains(
                &domains,
                |staged_raw, staged_raw_snake| {
                    for domain in &domains {
                        let submitted = sections.get(domain).cloned().flatten().filter(|value| !value.is_null());
                        let value = match (&submitted, options.exact_keys.get(domain)) {
                            (Some(submitted), Some(exact)) if submitted.is_object() => {
                                Some(merge_exact_entries(staged_raw.get(domain), submitted.as_object().unwrap(), exact))
                            }
                            _ => submitted,
                        };
                        let stripped = self.strip_env(domain, value, staged_raw, staged_raw_snake)?;
                        match stripped {
                            Some(stripped) => {
                                let validated = self.registry.validate(domain, stripped)?;
                                staged_raw.insert(domain.clone(), validated);
                            }
                            None => {
                                staged_raw.shift_remove(domain);
                            }
                        }
                    }
                    Ok(())
                },
                options.preserve_unknown != Some(false),
                &options.exact_keys,
                &options.expected_values,
            )
            .await?;
        let pending = {
            let mut state = self.state.lock();
            state.raw = raw;
            state.raw_snake = raw_snake;
            rebuild_effective(
                self.registry.as_ref(),
                self.bootstrap.as_ref(),
                &mut state,
                ConfigChangeSource::Set,
                Some(&domains),
            )
        };
        self.apply_watch_enabled();
        self.fire_pending(pending);
        Ok(())
    }

    pub async fn reload(&self) -> Result<(), WireAsyncError> {
        self.ready().await?;
        let _guard = self.state_chain.lock().await;
        self.load(ConfigChangeSource::Reload).await
    }

    async fn load(&self, source: ConfigChangeSource) -> Result<(), WireAsyncError> {
        self.state.lock().diagnostics.clear();
        let mut file_data = Map::new();
        let mut read_error = None;
        match self.document_store.get(CONFIG_SCOPE, &self.config_key).await {
            Ok(data) => {
                if let Some(data) = data.filter(|value| value.is_object()) {
                    file_data = data.as_object().cloned().unwrap_or_default();
                }
                self.state.lock().read_error = None;
            }
            Err(error) => {
                read_error = Some(error.to_string());
                self.state.lock().read_error = Some(error.to_string());
                let message = describe_toml_syntax_error(error.as_ref());
                let message = if message == error.to_string() {
                    message
                } else {
                    format!("Failed to parse {}: {message}", self.bootstrap.config_path())
                };
                push_diagnostic(&mut self.state.lock(), ConfigDiagnostic::error("", message));
                self.log.warn("config load failed", Some(json!({"error": error.to_string()})));
                if source != ConfigChangeSource::Load {
                    let diagnostics = diagnostics_if_changed(&mut self.state.lock());
                    self.fire_pending(PendingEvents { commits: Vec::new(), diagnostics });
                    return Ok(());
                }
            }
        }
        if let Some(error) = read_error {
            self.state.lock().read_error = Some(error);
        }
        let next_raw_snake = clone_record(Some(&Value::Object(file_data.clone())));
        for diagnostic in collect_key_deprecations(&next_raw_snake, &self.registry.list_sections()) {
            push_diagnostic(&mut self.state.lock(), diagnostic);
        }
        for section in self.registry.list_sections() {
            let Some(collect) = section.collect_diagnostics else {
                continue;
            };
            let raw_section = next_raw_snake.get(&camel_to_snake(&section.domain));
            for diagnostic in collect(raw_section) {
                push_diagnostic(&mut self.state.lock(), diagnostic);
            }
        }
        if source != ConfigChangeSource::Load
            && serde_json::to_string(&next_raw_snake).ok() == serde_json::to_string(&self.state.lock().raw_snake).ok()
        {
            let pending = {
                let mut state = self.state.lock();
                let mut scratch = state.validated.clone();
                apply_section_env_bindings(
                    self.registry.as_ref(),
                    self.bootstrap.as_ref(),
                    &mut scratch,
                    true,
                    &mut state,
                );
                apply_env_overlay(
                    self.registry.as_ref(),
                    self.bootstrap.as_ref(),
                    &mut scratch,
                    true,
                    &mut state,
                );
                PendingEvents {
                    commits: Vec::new(),
                    diagnostics: diagnostics_if_changed(&mut state),
                }
            };
            self.fire_pending(pending);
            return Ok(());
        }
        let raw = transform_toml_data(&file_data, self.registry.as_ref());
        let pending = {
            let mut state = self.state.lock();
            state.raw_snake = next_raw_snake;
            state.raw = raw;
            rebuild_effective(
                self.registry.as_ref(),
                self.bootstrap.as_ref(),
                &mut state,
                source,
                None,
            )
        };
        self.apply_watch_enabled();
        self.fire_pending(pending);
        Ok(())
    }

    fn reapply_overlays(&self) {
        let pending = {
            let mut state = self.state.lock();
            let before = state.effective.clone();
            state.validated = build_validated(self.registry.as_ref(), &state.raw, &mut state.diagnostics);
            let mut next = state.validated.clone();
            apply_section_env_bindings(
                self.registry.as_ref(),
                self.bootstrap.as_ref(),
                &mut next,
                true,
                &mut state,
            );
            apply_env_overlay(
                self.registry.as_ref(),
                self.bootstrap.as_ref(),
                &mut next,
                true,
                &mut state,
            );
            state.effective = next;
            let domains = before
                .keys()
                .chain(state.effective.keys())
                .cloned()
                .collect::<IndexSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            let mut pending = commit(&mut state, ConfigChangeSource::Reload, &domains);
            pending.diagnostics = diagnostics_if_changed(&mut state);
            pending
        };
        self.fire_pending(pending);
    }

    fn revalidate_domain(&self, domain: &str) {
        let Some(section) = self.registry.get_section(domain) else {
            return;
        };
        {
            let mut state = self.state.lock();
            if let Some(from_toml) = &section.from_toml
                && let Some(raw_snake_value) = state.raw_snake.get(&camel_to_snake(domain))
            {
                state.raw.insert(domain.to_string(), from_toml(raw_snake_value));
            }
            if let Some(raw) = state.raw.get(domain).cloned() {
                match self.registry.validate(domain, raw) {
                    Ok(validated) => {
                        state.validated.insert(domain.to_string(), validated.clone());
                        state.effective.insert(domain.to_string(), validated);
                    }
                    Err(error) => {
                        push_diagnostic(
                            &mut state,
                            ConfigDiagnostic::error(
                                domain,
                                format!("Ignoring invalid config section '{domain}': {error}"),
                            ),
                        );
                        let pending = PendingEvents {
                            commits: Vec::new(),
                            diagnostics: diagnostics_if_changed(&mut state),
                        };
                        drop(state);
                        self.fire_pending(pending);
                        return;
                    }
                }
            } else if let Some(default) = section.default_value.clone().filter(|_| !state.effective.contains_key(domain)) {
                state.validated.insert(domain.to_string(), default.clone());
                state.effective.insert(domain.to_string(), default);
            } else {
                return;
            }
        }
        let pending = {
            let mut state = self.state.lock();
            let mut effective = state.effective.clone();
            apply_env_overlay(
                self.registry.as_ref(),
                self.bootstrap.as_ref(),
                &mut effective,
                true,
                &mut state,
            );
            state.effective = effective;
            if let Some(env) = &section.env {
                let base = state.effective.get(domain).cloned();
                let resolved = {
                    let mut deprecated = |old: &str, new: &str| {
                        push_diagnostic(
                            &mut state,
                            ConfigDiagnostic::warning(
                                domain,
                                format!("Environment variable {old} is deprecated; use {new} instead."),
                            ),
                        );
                    };
                    apply_section_env(base.as_ref(), env, &|name| self.bootstrap.get_env(name), &mut deprecated)
                };
                match resolved {
                    Ok(Some(next)) => match self.registry.validate(domain, next) {
                        Ok(validated) => {
                            state.effective.insert(domain.to_string(), validated);
                        }
                        Err(error) => push_env_warning(&mut state, domain, error),
                    },
                    Ok(None) => {
                        state.effective.shift_remove(domain);
                    }
                    Err(error) => push_env_warning(&mut state, domain, error),
                }
            }
            let mut pending = commit(&mut state, ConfigChangeSource::Reload, &[domain.to_string()]);
            pending.diagnostics = diagnostics_if_changed(&mut state);
            pending
        };
        self.fire_pending(pending);
    }

    fn devalidate_domain(&self, domain: &str) {
        if self.registry.get_section(domain).is_some() {
            return;
        }
        let pending = {
            let mut state = self.state.lock();
            let snake_key = camel_to_snake(domain);
            match state.raw_snake.get(&snake_key).cloned() {
                Some(raw_snake_value) => {
                    let data = Map::from_iter([(snake_key, raw_snake_value)]);
                    let raw = transform_toml_data(&data, self.registry.as_ref()).shift_remove(domain);
                    match raw {
                        Some(raw) => {
                            state.raw.insert(domain.to_string(), raw.clone());
                            state.validated.insert(domain.to_string(), raw.clone());
                            state.effective.insert(domain.to_string(), raw);
                        }
                        None => {
                            state.raw.shift_remove(domain);
                            state.validated.shift_remove(domain);
                            state.effective.shift_remove(domain);
                        }
                    }
                }
                None => {
                    state.raw.shift_remove(domain);
                    state.validated.shift_remove(domain);
                    state.effective.shift_remove(domain);
                }
            }
            let mut effective = state.effective.clone();
            apply_env_overlay(
                self.registry.as_ref(),
                self.bootstrap.as_ref(),
                &mut effective,
                true,
                &mut state,
            );
            state.effective = effective;
            commit(&mut state, ConfigChangeSource::Reload, &[domain.to_string()])
        };
        self.fire_pending(pending);
    }

    fn apply_watch_enabled(&self) {
        let state = self.state.lock();
        let value = state
            .memory
            .get(WATCH_SECTION)
            .or_else(|| state.effective.get(WATCH_SECTION))
            .and_then(|section| section.get("enabled"))
            .and_then(Value::as_bool)
            .unwrap_or(true);
        drop(state);
        set_watch_enabled(value);
    }

    fn fire_pending(&self, pending: PendingEvents) {
        for event in pending.commits {
            match event {
                CommitEvent::Configuration(event) => self.on_change_configuration.fire(event),
                CommitEvent::Section(event) => self.on_section_change.fire(event),
            }
        }
        if let Some(diagnostics) = pending.diagnostics {
            self.on_change_diagnostics.fire(diagnostics);
        }
    }

    fn strip_env(
        &self,
        domain: &str,
        value: Option<Value>,
        raw: &ResolvedConfig,
        raw_snake: &Map<String, Value>,
    ) -> Result<Option<Value>, WireAsyncError> {
        let mut result = value;
        if let Some(strip) = self.registry.get_section(domain).and_then(|section| section.strip_env)
            && let Some(value) = result
        {
            result = strip(value, raw.get(domain), &|name| self.bootstrap.get_env(name));
        }
        let Some(mut value) = result else {
            return Ok(None);
        };
        for overlay in self.registry.list_effective_overlays() {
            let Some(strip) = overlay.strip else {
                continue;
            };
            let Some(next) = strip(domain, value, raw_snake) else {
                return Ok(None);
            };
            value = next;
        }
        Ok(Some(value))
    }

    fn assert_expected_values(
        &self,
        current: &ResolvedConfig,
        expected_values: &BTreeMap<String, Option<Value>>,
    ) -> Result<(), WireAsyncError> {
        if expected_values.is_empty() {
            return Ok(());
        }
        let mut changed = Vec::new();
        for (domain, expected) in expected_values {
            let expected = expected.as_ref().filter(|value| !value.is_null());
            let current_value = current.get(domain);
            let normalized_current = match current_value {
                None if expected.is_some_and(|value| value.is_object()) => {
                    Some(self.registry.validate(domain, Value::Object(Map::new()))?)
                }
                None => None,
                Some(value) => Some(self.registry.validate(domain, value.clone())?),
            };
            let normalized_expected = match expected {
                Some(value) => Some(self.registry.validate(domain, value.clone())?),
                None => None,
            };
            if !optional_deep_equal(normalized_current.as_ref(), normalized_expected.as_ref()) {
                changed.push(domain.clone());
            }
        }
        if changed.is_empty() {
            return Ok(());
        }
        Err(Box::new(Error2::with_options(
            CONFIG_INVALID,
            "Configuration changed while this update was being prepared; retry the operation.",
            Error2Options {
                details: Some(Map::from_iter([("domains".to_string(), json!(changed))])),
                ..Default::default()
            },
        )))
    }

    fn assert_persistable(&self) -> Result<(), WireAsyncError> {
        if self.state.lock().read_error.is_none() {
            return Ok(());
        }
        Err(config_error(
            CONFIG_PERSIST_BLOCKED,
            format!(
                "Refusing to persist config: {} could not be read; fix the file and reload before writing.",
                self.bootstrap.config_path()
            ),
            None,
        ))
    }

    async fn persist_domains(
        &self,
        domains: &[String],
        rebase: impl FnOnce(&mut ResolvedConfig, &mut Map<String, Value>) -> Result<(), WireAsyncError> + Send,
        preserve_unknown: bool,
        exact_keys: &BTreeMap<String, Vec<String>>,
        expected_values: &BTreeMap<String, Option<Value>>,
    ) -> Result<(ResolvedConfig, Map<String, Value>), WireAsyncError> {
        self.assert_persistable()?;
        let on_disk = match self.document_store.get(CONFIG_SCOPE, &self.config_key).await {
            Ok(data) => data
                .filter(|value| value.is_object())
                .and_then(|value| value.as_object().cloned())
                .unwrap_or_default(),
            Err(error) => {
                let syntax = describe_toml_syntax_error(error.as_ref());
                let message = if syntax == error.to_string() {
                    syntax
                } else {
                    format!("Failed to parse {}: {syntax}", self.bootstrap.config_path())
                };
                let diagnostics = {
                    let mut state = self.state.lock();
                    push_diagnostic(&mut state, ConfigDiagnostic::error("", message));
                    state.read_error = Some(error.to_string());
                    diagnostics_if_changed(&mut state)
                };
                self.log.warn(
                    "config persist aborted: re-read failed",
                    Some(json!({"error": error.to_string()})),
                );
                self.fire_pending(PendingEvents { commits: Vec::new(), diagnostics });
                return Err(config_error(
                    CONFIG_PERSIST_BLOCKED,
                    format!(
                        "Refusing to persist config: {} could not be read; fix the file and reload before writing.",
                        self.bootstrap.config_path()
                    ),
                    Some(error),
                ));
            }
        };
        let on_disk_text = self.document_store.get_text(CONFIG_SCOPE, &self.config_key).await.ok().flatten();
        let mut staged_raw_snake = clone_record(Some(&Value::Object(on_disk.clone())));
        let mut staged_raw = transform_toml_data(&on_disk, self.registry.as_ref());
        self.assert_expected_values(&staged_raw, expected_values)?;
        let previous_snake = domains
            .iter()
            .map(|domain| {
                let snake = camel_to_snake(domain);
                (snake.clone(), staged_raw_snake.get(&snake).cloned())
            })
            .collect::<BTreeMap<_, _>>();
        rebase(&mut staged_raw, &mut staged_raw_snake)?;
        if !preserve_unknown {
            for domain in domains {
                let snake_domain = camel_to_snake(domain);
                match exact_keys.get(domain) {
                    Some(keys) if staged_raw_snake.get(&snake_domain).is_some_and(|value| value.is_object()) => {
                        let raw_domain = staged_raw_snake.get_mut(&snake_domain).and_then(Value::as_object_mut).unwrap();
                        for key in keys {
                            raw_domain.remove(key);
                        }
                    }
                    _ => {
                        staged_raw_snake.remove(&snake_domain);
                    }
                }
            }
        }
        for domain in domains {
            apply_section_to_toml(
                &mut staged_raw_snake,
                domain,
                staged_raw.get(domain),
                self.registry.as_ref(),
            );
        }
        let updates = domains
            .iter()
            .map(|domain| {
                let snake_key = camel_to_snake(domain);
                DomainUpdate {
                    previous_value: previous_snake.get(&snake_key).cloned().flatten(),
                    next_value: staged_raw_snake.get(&snake_key).cloned(),
                    snake_key,
                }
            })
            .collect::<Vec<_>>();
        let planned_text = on_disk_text
            .as_deref()
            .and_then(|text| plan_config_writeback(text, &updates, &staged_raw_snake));
        match (&on_disk_text, planned_text) {
            (None, _) => {
                self.document_store
                    .set(CONFIG_SCOPE, &self.config_key, Value::Object(staged_raw_snake.clone()))
                    .await?;
            }
            (Some(_), None) => {
                self.document_store
                    .set(CONFIG_SCOPE, &self.config_key, Value::Object(staged_raw_snake.clone()))
                    .await?;
            }
            (Some(original), Some(planned)) if planned != *original => {
                self.document_store.set_text(CONFIG_SCOPE, &self.config_key, &planned).await?;
            }
            _ => {}
        }
        Ok((staged_raw, staged_raw_snake))
    }
}

impl IDisposable for ConfigService {
    fn dispose(&self) {
        if let Some(task) = self.watch_debounce.lock().take() {
            task.abort();
        }
        if let Some(subscription) = self.watch_subscription.lock().take() {
            let mut subscription = subscription;
            subscription.dispose();
        }
        if let Some(handle) = self.watch_handle.lock().take() {
            handle.dispose();
        }
        for subscription in self.subscriptions.lock().drain(..) {
            subscription.dispose();
        }
        self.on_change_configuration.dispose();
        self.on_section_change.dispose();
        self.on_change_diagnostics.dispose();
    }
}

#[async_trait]
impl IConfigService for ConfigService {
    async fn ready(&self) -> Result<(), WireAsyncError> {
        ConfigService::ready(self).await
    }

    fn on_did_change_configuration(&self, listener: ConfigChangeListener) -> DisposableRef {
        subscribe(&self.on_change_configuration, listener)
    }

    fn on_did_section_change(&self, listener: ConfigSectionChangeListener) -> DisposableRef {
        subscribe(&self.on_section_change, listener)
    }

    fn on_did_change_diagnostics(&self, listener: ConfigDiagnosticsListener) -> DisposableRef {
        subscribe(&self.on_change_diagnostics, listener)
    }

    fn get(&self, domain: &str) -> Option<Value> {
        ConfigService::get(self, domain)
    }

    async fn assert_valid(&self, domain: &str) -> Result<(), WireAsyncError> {
        ConfigService::assert_valid(self, domain).await
    }

    fn inspect(&self, domain: &str) -> ConfigInspectValue {
        ConfigService::inspect(self, domain)
    }

    fn get_all(&self) -> ResolvedConfig {
        ConfigService::get_all(self)
    }

    async fn set(
        &self,
        domain: &str,
        patch: Option<Value>,
        target: Option<ConfigTarget>,
    ) -> Result<(), WireAsyncError> {
        ConfigService::set(self, domain, patch, target).await
    }

    async fn replace(
        &self,
        domain: &str,
        value: Option<Value>,
        target: Option<ConfigTarget>,
    ) -> Result<(), WireAsyncError> {
        ConfigService::replace(self, domain, value, target).await
    }

    async fn replace_sections(
        &self,
        sections: BTreeMap<String, Option<Value>>,
        target: Option<ConfigTarget>,
        options: Option<ConfigReplaceSectionsOptions>,
    ) -> Result<(), WireAsyncError> {
        ConfigService::replace_sections(self, sections, target, options).await
    }

    async fn reload(&self) -> Result<(), WireAsyncError> {
        ConfigService::reload(self).await
    }

    fn diagnostics(&self) -> Vec<ConfigDiagnostic> {
        ConfigService::diagnostics(self)
    }
}

fn seed_initial_load(registry: &dyn IConfigRegistry, bootstrap: &dyn IBootstrapService, state: &mut ConfigState) {
    let Ok(text) = std::fs::read_to_string(bootstrap.config_path()) else {
        return;
    };
    let Ok(data) = parse_toml_document(&text) else {
        return;
    };
    if !data.is_object() {
        return;
    }
    let file_data = clone_record(Some(&data));
    state.raw_snake = file_data.clone();
    state.raw = transform_toml_data(&file_data, registry);
    state.validated = build_validated(registry, &state.raw, &mut state.diagnostics);
}

fn build_validated(
    registry: &dyn IConfigRegistry,
    raw: &ResolvedConfig,
    diagnostics: &mut Vec<ConfigDiagnostic>,
) -> ResolvedConfig {
    let mut validated = ResolvedConfig::new();
    for (domain, value) in raw {
        match registry.validate(domain, value.clone()) {
            Ok(value) => {
                validated.insert(domain.clone(), value);
            }
            Err(error) => push_diagnostic_vec(
                diagnostics,
                ConfigDiagnostic::warning(
                    domain,
                    format!("Ignored invalid config section '{domain}': {error}"),
                ),
            ),
        }
    }
    for section in registry.list_sections() {
        if !validated.contains_key(&section.domain)
            && let Some(default) = section.default_value
        {
            validated.insert(section.domain, default);
        }
    }
    validated
}

fn rebuild_effective(
    registry: &dyn IConfigRegistry,
    bootstrap: &dyn IBootstrapService,
    state: &mut ConfigState,
    source: ConfigChangeSource,
    domains: Option<&[String]>,
) -> PendingEvents {
    let previous = state.effective.clone();
    state.validated = build_validated(registry, &state.raw, &mut state.diagnostics);
    let mut next = state.validated.clone();
    apply_section_env_bindings(registry, bootstrap, &mut next, true, state);
    apply_env_overlay(registry, bootstrap, &mut next, true, state);
    state.effective = next;
    let mut candidates = domains
        .map(|domains| domains.iter().cloned().collect::<IndexSet<_>>())
        .unwrap_or_else(|| previous.keys().chain(state.effective.keys()).cloned().collect());
    for domain in previous.keys().chain(state.effective.keys()).cloned().collect::<IndexSet<_>>() {
        if !optional_deep_equal(previous.get(&domain), state.effective.get(&domain)) {
            candidates.insert(domain);
        }
    }
    let candidates = candidates.into_iter().collect::<Vec<_>>();
    let mut pending = commit(state, source, &candidates);
    pending.diagnostics = diagnostics_if_changed(state);
    pending
}

fn commit(state: &mut ConfigState, source: ConfigChangeSource, domains: &[String]) -> PendingEvents {
    let mut pending = PendingEvents::default();
    for domain in domains {
        let previous_value = state.delivered.get(domain).cloned().flatten();
        let value = state
            .memory
            .get(domain)
            .cloned()
            .or_else(|| state.effective.get(domain).cloned());
        pending.commits.push(CommitEvent::Configuration(ConfigChangedEvent {
            domain: domain.clone(),
            source,
            value: value.clone(),
            previous_value: previous_value.clone(),
        }));
        if !optional_deep_equal(value.as_ref(), previous_value.as_ref()) {
            pending.commits.push(CommitEvent::Section(ConfigSectionChangedEvent {
                domain: domain.clone(),
                source,
                value: value.clone(),
                previous_value,
            }));
        }
        state.delivered.insert(domain.clone(), value);
    }
    pending
}

fn diagnostics_if_changed(state: &mut ConfigState) -> Option<Vec<ConfigDiagnostic>> {
    let snapshot = serde_json::to_string(&state.diagnostics).unwrap_or_else(|_| "[]".to_string());
    if snapshot == state.last_diagnostics_snapshot {
        return None;
    }
    state.last_diagnostics_snapshot = snapshot;
    Some(state.diagnostics.clone())
}

fn push_diagnostic(state: &mut ConfigState, diagnostic: ConfigDiagnostic) {
    push_diagnostic_vec(&mut state.diagnostics, diagnostic);
}

fn push_diagnostic_vec(diagnostics: &mut Vec<ConfigDiagnostic>, diagnostic: ConfigDiagnostic) {
    if !diagnostics.contains(&diagnostic) {
        diagnostics.push(diagnostic);
    }
}

fn merge_exact_entries(
    current: Option<&Value>,
    submitted: &Map<String, Value>,
    exact: &[String],
) -> Value {
    let mut merged = current.and_then(Value::as_object).cloned().unwrap_or_default();
    for key in exact {
        merged.remove(key);
        if let Some(value) = submitted.get(key) {
            merged.insert(key.clone(), value.clone());
        }
    }
    Value::Object(merged)
}

fn optional_deep_equal(a: Option<&Value>, b: Option<&Value>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => deep_equal(a, b),
        _ => false,
    }
}

fn resolve_binding(
    binding: &EnvBindings,
    get_env: &GetEnv,
    existing: Option<&Value>,
    on_deprecated_env: &mut dyn FnMut(&str, &str),
) -> Option<Value> {
    let binding = binding.as_binding()?;
    if let Some(raw) = get_env(binding.env) {
        let parsed = match binding.parse {
            Some(parse) => parse(&raw),
            None => Some(Value::String(raw)),
        };
        if parsed.is_some() {
            return parsed;
        }
    }
    if let Some(deprecated) = binding.deprecated_env
        && let Some(raw) = get_env(deprecated)
    {
        let parsed = match binding.parse {
            Some(parse) => parse(&raw),
            None => Some(Value::String(raw)),
        };
        if let Some(parsed) = parsed {
            on_deprecated_env(deprecated, binding.env);
            return Some(parsed);
        }
    }
    if existing.is_none()
        && let Some(default) = binding.default
    {
        return Some(default.clone());
    }
    existing.cloned()
}

fn apply_env_bindings(
    target: &mut Map<String, Value>,
    bindings: &IndexMap<String, EnvBindings>,
    get_env: &GetEnv,
    on_deprecated_env: &mut dyn FnMut(&str, &str),
) {
    for (key, binding) in bindings {
        if binding.as_binding().is_some() {
            let resolved = resolve_binding(binding, get_env, target.get(key), on_deprecated_env);
            if let Some(resolved) = resolved {
                target.insert(key.clone(), resolved);
            }
        } else if let EnvBindings::Fields(fields) = binding {
            let mut child = target.get(key).and_then(Value::as_object).cloned().unwrap_or_default();
            apply_env_bindings(&mut child, fields, get_env, on_deprecated_env);
            if child.is_empty() {
                target.remove(key);
            } else {
                target.insert(key.clone(), Value::Object(child));
            }
        }
    }
}

pub fn apply_section_env(
    base: Option<&Value>,
    env: &EnvBindings,
    get_env: &GetEnv,
    on_deprecated_env: &mut dyn FnMut(&str, &str),
) -> Result<Option<Value>, WireAsyncError> {
    if env.as_binding().is_some() {
        return Ok(resolve_binding(env, get_env, base, on_deprecated_env));
    }
    let EnvBindings::Fields(fields) = env else {
        return Ok(base.cloned());
    };
    let mut target = base.and_then(Value::as_object).cloned().unwrap_or_default();
    apply_env_bindings(&mut target, fields, get_env, on_deprecated_env);
    Ok(Some(Value::Object(target)))
}

fn collect_env_names(bindings: &EnvBindings, into: &mut Vec<String>) {
    if let Some(binding) = bindings.as_binding() {
        into.push(binding.env.to_string());
        if let Some(deprecated) = binding.deprecated_env {
            into.push(deprecated.to_string());
        }
        return;
    }
    if let EnvBindings::Fields(fields) = bindings {
        for child in fields.values() {
            collect_env_names(child, into);
        }
    }
}

fn apply_section_env_bindings(
    registry: &dyn IConfigRegistry,
    bootstrap: &dyn IBootstrapService,
    effective: &mut ResolvedConfig,
    report_errors: bool,
    state: &mut ConfigState,
) {
    for section in registry.list_sections() {
        let Some(env) = &section.env else {
            continue;
        };
        let base = effective.get(&section.domain).cloned();
        let mut names = Vec::new();
        collect_env_names(env, &mut names);
        let env_values = names.iter().map(|name| bootstrap.get_env(name)).collect::<Vec<_>>();
        if !report_errors
            && let Some(resolved) = state.env_resolutions.get(&section.domain)
            && optional_deep_equal(resolved.base.as_ref(), base.as_ref())
            && resolved.env_values == env_values
        {
            match &resolved.value {
                Some(value) => {
                    effective.insert(section.domain.clone(), value.clone());
                }
                None => {
                    effective.shift_remove(&section.domain);
                }
            }
            continue;
        }
        let mut deprecated = |old: &str, new: &str| {
            if report_errors {
                push_diagnostic(
                    state,
                    ConfigDiagnostic::warning(
                        &section.domain,
                        format!("Environment variable {old} is deprecated; use {new} instead."),
                    ),
                );
            }
        };
        let resolved = apply_section_env(base.as_ref(), env, &|name| bootstrap.get_env(name), &mut deprecated)
            .and_then(|next| match next {
                Some(next) => registry.validate(&section.domain, next).map(Some),
                None => Ok(None),
            });
        match resolved {
            Ok(value) => {
                match &value {
                    Some(value) => {
                        effective.insert(section.domain.clone(), value.clone());
                    }
                    None => {
                        effective.shift_remove(&section.domain);
                    }
                }
                state.env_resolutions.insert(
                    section.domain.clone(),
                    EnvSectionResolution { base, env_values, value },
                );
            }
            Err(error) => {
                state.env_resolutions.remove(&section.domain);
                if report_errors {
                    push_env_warning(state, &section.domain, error);
                }
            }
        }
    }
}

fn push_env_warning(state: &mut ConfigState, domain: &str, error: WireAsyncError) {
    push_diagnostic(
        state,
        ConfigDiagnostic::warning(domain, format!("Ignoring env overlay for '{domain}': {error}")),
    );
}

fn apply_env_overlay(
    registry: &dyn IConfigRegistry,
    bootstrap: &dyn IBootstrapService,
    effective: &mut ResolvedConfig,
    report_errors: bool,
    state: &mut ConfigState,
) {
    for overlay in registry.list_effective_overlays() {
        let mut root = resolved_to_value(effective);
        let get_env = |name: &str| bootstrap.get_env(name);
        let validate = |domain: &str, value: Value| {
            registry
                .validate(domain, value)
                .unwrap_or_else(|error| std::panic::panic_any(error.to_string()))
        };
        let result = catch_unwind(AssertUnwindSafe(|| {
            (overlay.apply)(&mut root, &get_env, &validate)
        }));
        if let Value::Object(root_value) = &root {
            *effective = root_value.clone().into_iter().collect();
        }
        match result {
            Ok(_) => {}
            Err(error) => {
                if report_errors {
                    let message = error
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| error.downcast_ref::<&str>().map(|value| (*value).to_string()))
                        .unwrap_or_else(|| "unknown overlay error".to_string());
                    push_diagnostic(
                        state,
                        ConfigDiagnostic::warning("", format!("Ignoring config environment overlay: {message}")),
                    );
                }
            }
        }
    }
}

fn resolved_to_value(value: &ResolvedConfig) -> Value {
    Value::Object(value.iter().map(|(key, value)| (key.clone(), value.clone())).collect())
}

fn normalize_path(path: &str) -> String {
    path.replace('\\', "/").trim_end_matches('/').to_string()
}

fn config_error(code: &str, message: impl Into<String>, cause: Option<WireAsyncError>) -> WireAsyncError {
    Box::new(Error2::with_options(
        code,
        message,
        Error2Options {
            cause,
            ..Default::default()
        },
    ))
}

fn dependency<T>(args: &[DynamicValue], index: usize) -> Arc<T>
where
    T: Send + Sync + 'static + ?Sized,
{
    Arc::downcast::<Arc<T>>(args[index].clone())
        .unwrap_or_else(|_| panic!("config dependency {index} has an incompatible type"))
        .as_ref()
        .clone()
}

static CONFIG_REGISTRY_REGISTRATION: Lazy<Result<(), String>> = Lazy::new(|| {
    let ctor: Constructor<DynamicValue> = Arc::new(|_| {
        let service: Arc<dyn IConfigRegistry> = ConfigRegistry::new();
        Arc::new(service) as DynamicValue
    });
    let descriptor = SyncDescriptor::new(ctor, Vec::new());
    register_scoped_service(
        LifecycleScope::App.as_str().to_string(),
        I_CONFIG_REGISTRY.clone(),
        descriptor,
        ScopeActivation::OnScopeCreated,
        "config",
    )
    .map_err(|error| error.to_string())
});

static CONFIG_SERVICE_REGISTRATION: Lazy<Result<(), String>> = Lazy::new(|| {
    let _ = Lazy::force(&CONFIG_REGISTRY_REGISTRATION);
    let ctor: Constructor<DynamicValue> = Arc::new(|args| {
        let service: Arc<dyn IConfigService> = ConfigService::new(
            dependency::<dyn IConfigRegistry>(&args, 0),
            dependency::<dyn IBootstrapService>(&args, 1),
            dependency::<dyn ILogService>(&args, 2),
            dependency::<dyn IAtomicTomlDocumentStore>(&args, 3),
        );
        Arc::new(service) as DynamicValue
    });
    let descriptor = SyncDescriptor::new(ctor, Vec::new());
    register_descriptor_dependencies(
        &descriptor,
        vec![
            ServiceDependency {
                id: I_CONFIG_REGISTRY.clone(),
                index: 0,
                kind: DependencyKind::Instance,
            },
            ServiceDependency {
                id: refine_service_decorator(&I_BOOTSTRAP_SERVICE),
                index: 1,
                kind: DependencyKind::Instance,
            },
            ServiceDependency {
                id: refine_service_decorator(&I_LOG_SERVICE),
                index: 2,
                kind: DependencyKind::Instance,
            },
            ServiceDependency {
                id: create_decorator("atomicTomlDocumentStore"),
                index: 3,
                kind: DependencyKind::Instance,
            },
        ],
    );
    register_scoped_service(
        LifecycleScope::App.as_str().to_string(),
        I_CONFIG_SERVICE.clone(),
        descriptor,
        ScopeActivation::OnScopeCreated,
        "config",
    )
    .map_err(|error| error.to_string())
});

pub fn ensure_config_registry_registered() -> Result<(), String> {
    Lazy::force(&CONFIG_REGISTRY_REGISTRATION).clone()
}

pub fn ensure_config_service_registered() -> Result<(), String> {
    Lazy::force(&CONFIG_SERVICE_REGISTRATION).clone()
}

