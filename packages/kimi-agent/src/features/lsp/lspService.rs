use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex, Weak};

use async_trait::async_trait;
use kimi_core::primitives::errors::Error2;
use kimi_core::primitives::errors::domains::lsp::{LSP_CONFLICT, LSP_UNAVAILABLE};
use tokio_util::sync::CancellationToken;

use super::lsp::{ILspService, LspProvider, LspProviderQuery, LspQueryRequest, LspQueryResult};

#[derive(Default)]
struct ProviderRegistry {
    providers: BTreeMap<String, Arc<dyn LspProvider>>,
    extension_to_provider: BTreeMap<String, String>,
}

#[derive(Clone, Default)]
pub struct LspService {
    registry: Arc<Mutex<ProviderRegistry>>,
}

pub struct LspProviderRegistration {
    registry: Weak<Mutex<ProviderRegistry>>,
    provider_id: String,
    disposed: bool,
}

impl LspProviderRegistration {
    pub fn dispose(mut self) {
        self.remove();
    }

    fn remove(&mut self) {
        if self.disposed {
            return;
        }
        self.disposed = true;
        let Some(registry) = self.registry.upgrade() else {
            return;
        };
        let mut registry = registry.lock().unwrap();
        registry.providers.remove(&self.provider_id);
        registry
            .extension_to_provider
            .retain(|_, owner| owner != &self.provider_id);
    }
}

impl Drop for LspProviderRegistration {
    fn drop(&mut self) {
        self.remove();
    }
}

impl LspService {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_provider(
        &self,
        provider: Arc<dyn LspProvider>,
    ) -> Result<LspProviderRegistration, Error2> {
        let provider_id = provider.id().to_string();
        let mut registry = self.registry.lock().unwrap();
        if registry.providers.contains_key(&provider_id) {
            return Err(Error2::new(
                LSP_CONFLICT,
                format!("an LSP provider with id \"{provider_id}\" is already registered"),
            ));
        }
        for extension in provider.extension_to_language().keys() {
            if let Some(owner) = registry.extension_to_provider.get(extension)
                && owner != &provider_id
            {
                return Err(Error2::new(
                    LSP_CONFLICT,
                    format!("LSP provider \"{owner}\" already covers extension \"{extension}\""),
                ));
            }
        }
        registry.providers.insert(provider_id.clone(), provider.clone());
        for extension in provider.extension_to_language().keys() {
            registry
                .extension_to_provider
                .insert(extension.clone(), provider_id.clone());
        }
        drop(registry);
        Ok(LspProviderRegistration {
            registry: Arc::downgrade(&self.registry),
            provider_id,
            disposed: false,
        })
    }

    pub async fn query(
        &self,
        request: LspQueryRequest,
        signal: Option<CancellationToken>,
    ) -> Result<LspQueryResult, Error2> {
        let extension = final_extension(&request.file_path);
        let (provider, language_id) = {
            let registry = self.registry.lock().unwrap();
            let Some(provider_id) = registry.extension_to_provider.get(&extension) else {
                return Err(Error2::new(
                    LSP_UNAVAILABLE,
                    format!(
                        "no LSP provider is configured for extension \"{extension}\" (file: {}). Configure one in the [lsp] config section, e.g. `servers.typescript = {{ command = \"typescript-language-server\", args = [\"--stdio\"], extensionToLanguage = {{ ts = \"typescript\" }} }}`.",
                        request.file_path
                    ),
                ));
            };
            let Some(provider) = registry.providers.get(provider_id) else {
                return Err(Error2::new(
                    LSP_UNAVAILABLE,
                    format!("LSP provider \"{provider_id}\" is registered for extension \"{extension}\" but missing from the provider table"),
                ));
            };
            let Some(language_id) = provider.extension_to_language().get(&extension) else {
                return Err(Error2::new(
                    LSP_UNAVAILABLE,
                    format!("LSP provider \"{provider_id}\" covers extension \"{extension}\" but does not map it to a language id"),
                ));
            };
            (provider.clone(), language_id.clone())
        };
        provider
            .query(
                LspProviderQuery {
                    operation: request.operation,
                    file_path: request.file_path,
                    position: request.position,
                    workspace_root: request.workspace_root,
                    language_id,
                },
                signal,
            )
            .await
    }
}

#[async_trait]
impl ILspService for LspService {
    fn register_provider(
        &self,
        provider: Arc<dyn LspProvider>,
    ) -> Result<LspProviderRegistration, Error2> {
        Self::register_provider(self, provider)
    }

    async fn query(
        &self,
        request: LspQueryRequest,
        signal: Option<CancellationToken>,
    ) -> Result<LspQueryResult, Error2> {
        Self::query(self, request, signal).await
    }
}

pub fn final_extension(file_path: &str) -> String {
    Path::new(file_path)
        .extension()
        .map(|extension| extension.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}
