use std::future::Future;
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::future::BoxFuture;
use kimi_core::primitives::errors::{Error2, domains::core::INTERNAL};
use parking_lot::Mutex;
use serde_json::{Value, json};

use super::config_section::{
    DEFAULT_MODEL_SECTION, DEFAULT_PROVIDER_SECTION, ModelRecord, MODELS_SECTION,
    ModelsSection, PROVIDERS_SECTION, ProviderConfig, ProvidersSection,
};
use super::kosong_config::{
    ConfigSectionChangedEvent, Dispose, IKosongConfigService, KosongConfigChangeStore,
    KosongConfigStore,
};

const PERSIST_MAX_ATTEMPTS: usize = 3;

#[async_trait]
pub trait KosongProviderService: Send + Sync {
    fn get(&self, name: &str) -> Option<ProviderConfig>;
    fn list(&self) -> ProvidersSection;
    fn get_default_provider(&self) -> Option<String>;
    fn load_all(&self, providers: ProvidersSection, default_provider: Option<String>);
    async fn set_default_provider(&self, id: Option<String>) -> Result<(), Error2>;

    fn on_did_change_providers(&self, listener: ProvidersChangeListener) -> Dispose;
    fn on_did_change_default_provider(&self, listener: DefaultProviderChangeListener) -> Dispose;
}

#[async_trait]
pub trait KosongModelService: Send + Sync {
    fn get(&self, id: &str) -> Option<ModelRecord>;
    fn list(&self) -> ModelsSection;
    fn get_default_model(&self) -> Option<String>;
    fn load_all(&self, models: ModelsSection, default_model: Option<String>);
    async fn set_default_model(&self, id: Option<String>) -> Result<(), Error2>;

    fn on_did_change_models(&self, listener: ModelsChangeListener) -> Dispose;
    fn on_did_change_default_model(&self, listener: DefaultModelChangeListener) -> Dispose;
}

pub type ProvidersChangeListener = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;
pub type DefaultProviderChangeListener =
    Arc<dyn Fn(Option<String>) -> BoxFuture<'static, ()> + Send + Sync>;
pub type ModelsChangeListener = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;
pub type DefaultModelChangeListener =
    Arc<dyn Fn(Option<String>) -> BoxFuture<'static, ()> + Send + Sync>;

pub trait KosongConfigLog: Send + Sync {
    fn warn(&self, message: &str, error: &str);
}

pub type RetrySleep = Arc<dyn Fn(u64) -> BoxFuture<'static, ()> + Send + Sync>;

pub struct KosongConfigService {
    config: Arc<dyn KosongConfigChangeStore>,
    providers: Arc<dyn KosongProviderService>,
    models: Arc<dyn KosongModelService>,
    log: Arc<dyn KosongConfigLog>,
    retry_sleep: RetrySleep,
    ready: tokio::sync::OnceCell<Result<(), String>>,
    persist_chain: tokio::sync::Mutex<()>,
    disposables: Mutex<Vec<Dispose>>,
    self_weak: Weak<KosongConfigService>,
}

impl KosongConfigService {
    pub fn new(
        config: Arc<dyn KosongConfigChangeStore>,
        providers: Arc<dyn KosongProviderService>,
        models: Arc<dyn KosongModelService>,
        log: Arc<dyn KosongConfigLog>,
        retry_sleep: Option<RetrySleep>,
    ) -> Arc<Self> {
        Arc::new_cyclic(|weak| Self {
            config,
            providers,
            models,
            log,
            retry_sleep: retry_sleep.unwrap_or_else(|| {
                Arc::new(|delay_ms| {
                    Box::pin(async move {
                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    })
                })
            }),
            ready: tokio::sync::OnceCell::new(),
            persist_chain: tokio::sync::Mutex::new(()),
            disposables: Mutex::new(Vec::new()),
            self_weak: weak.clone(),
        })
    }

    pub async fn ready(&self) -> Result<(), Error2> {
        let this = self
            .self_weak
            .upgrade()
            .ok_or_else(|| Error2::new(INTERNAL, "kosong config service is no longer available"))?;
        let result = self
            .ready
            .get_or_init(|| async move { this.initialize().await })
            .await
            .clone();
        if let Err(error) = &result {
            self.log
                .warn("kosong config bridge initialization failed", error);
        }
        result.map_err(|error| Error2::new(INTERNAL, error))
    }

    async fn initialize(self: Arc<Self>) -> Result<(), String> {
        self.config.ready().await.map_err(|error| error.to_string())?;
        self.providers.load_all(
            decode_providers(self.config.get(PROVIDERS_SECTION)),
            config_string(&*self.config, DEFAULT_PROVIDER_SECTION),
        );
        self.models.load_all(
            decode_models(self.config.get(MODELS_SECTION)),
            config_string(&*self.config, DEFAULT_MODEL_SECTION),
        );

        let this = self.clone();
        self.disposables.lock().push(self.config.on_did_section_change(
            Arc::new(move |event| this.on_config_section_changed(event)),
        ));

        let this = self.clone();
        self.disposables.lock().push(self.providers.on_did_change_providers(
            Arc::new(move || {
                let this = this.clone();
                Box::pin(async move {
                    let current = this.config.get(PROVIDERS_SECTION).unwrap_or_else(|| json!({}));
                    if current == providers_value(this.providers.list()) {
                        return;
                    }
                    this.enqueue_persist_providers().await;
                })
            }),
        ));

        let this = self.clone();
        self.disposables.lock().push(self.providers.on_did_change_default_provider(
            Arc::new(move |id| {
                let this = this.clone();
                Box::pin(async move {
                    if config_string(&*this.config, DEFAULT_PROVIDER_SECTION) == id {
                        return;
                    }
                    this.enqueue_persist_default_pointer(DEFAULT_PROVIDER_SECTION, id).await;
                })
            }),
        ));

        let this = self.clone();
        self.disposables.lock().push(self.models.on_did_change_models(
            Arc::new(move || {
                let this = this.clone();
                Box::pin(async move {
                    let current = this.config.get(MODELS_SECTION).unwrap_or_else(|| json!({}));
                    if current == models_value(this.models.list()) {
                        return;
                    }
                    this.enqueue_persist_models().await;
                })
            }),
        ));

        let this = self.clone();
        self.disposables.lock().push(self.models.on_did_change_default_model(
            Arc::new(move |id| {
                let this = this.clone();
                Box::pin(async move {
                    if config_string(&*this.config, DEFAULT_MODEL_SECTION) == id {
                        return;
                    }
                    this.enqueue_persist_default_pointer(DEFAULT_MODEL_SECTION, id).await;
                })
            }),
        ));
        Ok(())
    }

    fn on_config_section_changed(&self, event: ConfigSectionChangedEvent) {
        match event.domain.as_str() {
            PROVIDERS_SECTION => self.providers.load_all(
                decode_providers(event.value),
                self.providers.get_default_provider(),
            ),
            MODELS_SECTION => self.models.load_all(
                decode_models(event.value),
                self.models.get_default_model(),
            ),
            DEFAULT_PROVIDER_SECTION => {
                let value = event.value.and_then(|value| value.as_str().map(str::to_string));
                let this = self.self_weak.upgrade();
                if let Some(this) = this {
                    tokio::spawn(async move {
                        if let Err(error) = this.providers.set_default_provider(value).await {
                            this.log_persist_failure(&error);
                        }
                    });
                }
            }
            DEFAULT_MODEL_SECTION => {
                let value = event.value.and_then(|value| value.as_str().map(str::to_string));
                let this = self.self_weak.upgrade();
                if let Some(this) = this {
                    tokio::spawn(async move {
                        if let Err(error) = this.models.set_default_model(value).await {
                            this.log_persist_failure(&error);
                        }
                    });
                }
            }
            _ => {}
        }
    }

    async fn enqueue_persist_providers(&self) {
        self.enqueue(async {
            let next = self.providers.list();
            let current = self.config.get(PROVIDERS_SECTION).unwrap_or_else(|| json!({}));
            if current == providers_value(next.clone()) {
                return Ok(());
            }
            self.replace_with_retry(PROVIDERS_SECTION, Some(providers_value(next))).await
        })
        .await;
    }

    async fn enqueue_persist_models(&self) {
        self.enqueue(async {
            let next = self.models.list();
            let current = self.config.get(MODELS_SECTION).unwrap_or_else(|| json!({}));
            if current == models_value(next.clone()) {
                return Ok(());
            }
            self.replace_with_retry(MODELS_SECTION, Some(models_value(next))).await
        })
        .await;
    }

    async fn enqueue_persist_default_pointer(&self, domain: &'static str, value: Option<String>) {
        self.enqueue(async {
            if config_string(&*self.config, domain) == value {
                return Ok(());
            }
            self.replace_with_retry(domain, value.clone().map(Value::String)).await?;
            let effective = config_string(&*self.config, domain);
            if effective == value {
                return Ok(());
            }
            match domain {
                DEFAULT_PROVIDER_SECTION => {
                    if let Err(error) = self.providers.set_default_provider(effective).await {
                        self.log_persist_failure(&error);
                    }
                }
                DEFAULT_MODEL_SECTION => {
                    if let Err(error) = self.models.set_default_model(effective).await {
                        self.log_persist_failure(&error);
                    }
                }
                _ => {}
            }
            Ok(())
        })
        .await;
    }

    async fn replace_with_retry(&self, domain: &str, value: Option<Value>) -> Result<(), Error2> {
        let delays = retry_backoff_delays(PERSIST_MAX_ATTEMPTS);
        let mut attempt = 0;
        loop {
            match self.config.replace(domain, value.clone()).await {
                Ok(()) => return Ok(()),
                Err(error) => {
                    let Some(delay) = delays.get(attempt).copied() else {
                        return Err(error);
                    };
                    (self.retry_sleep)(delay).await;
                    attempt += 1;
                }
            }
        }
    }

    async fn enqueue(&self, task: impl Future<Output = Result<(), Error2>>) {
        let _guard = self.persist_chain.lock().await;
        if let Err(error) = task.await {
            self.log_persist_failure(&error);
        }
    }

    fn log_persist_failure(&self, error: &Error2) {
        self.log.warn("kosong config persist failed", &error.to_string());
    }
}

#[async_trait]
impl IKosongConfigService for KosongConfigService {
    async fn ready(&self) -> Result<(), Error2> {
        KosongConfigService::ready(self).await
    }
}

fn config_string(config: &dyn KosongConfigStore, domain: &str) -> Option<String> {
    config
        .get(domain)
        .and_then(|value| value.as_str().map(str::to_string))
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

fn providers_value(value: ProvidersSection) -> Value {
    serde_json::to_value(value).unwrap_or_else(|_| json!({}))
}

fn models_value(value: ModelsSection) -> Value {
    serde_json::to_value(value).unwrap_or_else(|_| json!({}))
}

fn retry_backoff_delays(max_attempts: usize) -> Vec<u64> {
    (0..max_attempts.saturating_sub(1))
        .map(|attempt| {
            let exponent = u32::try_from(attempt).unwrap_or(u32::MAX);
            let base = 500u64.saturating_mul(2u64.saturating_pow(exponent)).min(32_000);
            base + fastrand::u64(0..=base / 4)
        })
        .collect()
}

pub fn register_kosong_config_service() {
    crate::_base::di::scope::register_scoped_service(
        crate::app::scopes::LifecycleScope::App,
        crate::_base::di::scope::ScopeActivation::OnScopeCreated,
        "kosongConfig",
    );
}
