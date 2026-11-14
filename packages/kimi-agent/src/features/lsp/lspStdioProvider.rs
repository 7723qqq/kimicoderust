use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use futures_util::future::join_all;
use indexmap::IndexMap;
use kimi_core::primitives::errors::Error2;
use kimi_core::primitives::errors::domains::lsp::{LSP_MALFORMED_RESPONSE, LSP_UNAVAILABLE};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::_base::errors::unexpected_error::on_unexpected_error;
use crate::app::config::config_service::ConfigService;

use super::configSection::{LSP_SECTION, LspConfig, LspServerConfig};
use super::lsp::{ILspService, LspProvider, LspProviderQuery, LspQueryResult};
use super::lspInstance::LspInstance;
use super::lspService::LspProviderRegistration;

pub const LSP_STDIO_PROVIDER_SERVICE_ID: &str = "lspStdioProviderService";

pub struct LspStdioProvider {
    pub id: String,
    config: LspServerConfig,
    instances: Mutex<HashMap<String, Arc<LspInstance>>>,
    disposed: AtomicBool,
}

impl LspStdioProvider {
    pub fn new(id: String, config: LspServerConfig) -> Self {
        Self {
            id,
            config,
            instances: Mutex::new(HashMap::new()),
            disposed: AtomicBool::new(false),
        }
    }

    pub async fn dispose(&self) {
        if self.disposed.swap(true, Ordering::AcqRel) {
            return;
        }
        let instances: Vec<_> = std::mem::take(&mut *self.instances.lock().await)
            .into_values()
            .collect();
        join_all(instances.into_iter().map(|instance| async move { instance.dispose().await })).await;
    }

    async fn create_instance(
        &self,
        workspace_root: String,
        signal: Option<CancellationToken>,
    ) -> Result<Arc<LspInstance>, Error2> {
        let id = self.id.clone();
        LspInstance::create(
            self.config.clone(),
            workspace_root,
            signal,
            Some(Arc::new(move |detail| {
                on_unexpected_error(Error2::new(
                    LSP_MALFORMED_RESPONSE,
                    format!("LSP server \"{id}\" sent a malformed frame: {detail}"),
                ));
            })),
        )
        .await
        .map(Arc::new)
    }
}

#[async_trait]
impl LspProvider for LspStdioProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn extension_to_language(&self) -> &IndexMap<String, String> {
        &self.config.extension_to_language
    }

    async fn query(
        &self,
        request: LspProviderQuery,
        signal: Option<CancellationToken>,
    ) -> Result<LspQueryResult, Error2> {
        if self.disposed.load(Ordering::Acquire) {
            return Err(Error2::new(
                LSP_UNAVAILABLE,
                format!("LSP provider \"{}\" is disposed", self.id),
            ));
        }
        let key = normalize_workspace_key(&request.workspace_root);
        let mut instance = match self.instances.lock().await.get(&key).cloned() {
            Some(instance) if !instance.is_closed() => instance,
            _ => {
                let created = self.create_instance(key.clone(), signal.clone()).await?;
                self.instances.lock().await.insert(key.clone(), created.clone());
                created
            }
        };
        match instance.query(request.clone(), signal.clone()).await {
            Ok(result) => Ok(result),
            Err(error) if is_recoverable_transport_error(&error.to_string()) => {
                instance.dispose().await;
                self.instances.lock().await.remove(&key);
                let fresh = self.create_instance(key.clone(), signal.clone()).await?;
                self.instances.lock().await.insert(key, fresh.clone());
                instance = fresh;
                instance.query(request, signal).await
            }
            Err(error) => Err(error),
        }
    }
}

pub struct LspStdioProviderService {
    providers: Vec<Arc<LspStdioProvider>>,
    registrations: Mutex<Vec<LspProviderRegistration>>,
}

impl LspStdioProviderService {
    pub fn new(config: &ConfigService, lsp: Arc<dyn ILspService>) -> Result<Self, Error2> {
        let Some(config) = config.get(LSP_SECTION) else {
            return Ok(Self {
                providers: Vec::new(),
                registrations: Mutex::new(Vec::new()),
            });
        };
        let config: LspConfig = serde_json::from_value(config)
            .map_err(|error| Error2::new(LSP_UNAVAILABLE, error.to_string()))?;
        let mut providers = Vec::new();
        let mut registrations = Vec::new();
        for (id, server_config) in config.servers.unwrap_or_default() {
            let provider = Arc::new(LspStdioProvider::new(id, server_config));
            registrations.push(lsp.register_provider(provider.clone())?);
            providers.push(provider);
        }
        Ok(Self {
            providers,
            registrations: Mutex::new(registrations),
        })
    }

    pub async fn dispose(&self) {
        self.registrations.lock().await.clear();
        join_all(self.providers.iter().map(|provider| provider.dispose())).await;
    }
}

pub fn normalize_workspace_key(workspace_root: &str) -> String {
    let raw = workspace_root.replace('\\', "/");
    let trailing_separator = raw.len() > 1 && raw.ends_with('/');
    let mut normalized = std::path::PathBuf::new();
    for component in std::path::Path::new(&raw).components() {
        match component {
            std::path::Component::ParentDir => match normalized.components().next_back() {
                Some(std::path::Component::Normal(_)) => {
                    normalized.pop();
                }
                Some(std::path::Component::ParentDir) | None => normalized.push(".."),
                Some(_) => {}
            },
            std::path::Component::CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }
    let mut output = if normalized.as_os_str().is_empty() {
        ".".to_string()
    } else {
        normalized.to_string_lossy().replace('\\', "/")
    };
    if trailing_separator && output != "/" {
        output.push('/');
    }
    output
}

pub fn is_recoverable_transport_error(error: &str) -> bool {
    error == "LSP server transport closed"
}
