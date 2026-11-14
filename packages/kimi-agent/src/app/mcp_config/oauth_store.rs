use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::mcp::oauth::crypto::{EncryptedBlob, decrypt, encrypt};
use crate::mcp_core::oauth::store::McpOAuthStore;
use crate::persistence::interface::atomic_document_store::IAtomicDocumentStore;
use crate::wire::record::WireAsyncError;

const CREDENTIALS_SCOPE: &str = "credentials/mcp";

pub trait IMcpOAuthStore: McpOAuthStore {}

pub struct McpOAuthStoreAdapter {
    docs: Arc<dyn IAtomicDocumentStore>,
}

impl McpOAuthStoreAdapter {
    pub fn new(docs: Arc<dyn IAtomicDocumentStore>) -> Self {
        Self { docs }
    }
}

pub fn create_mcp_oauth_store(docs: Arc<dyn IAtomicDocumentStore>) -> McpOAuthStoreAdapter {
    McpOAuthStoreAdapter::new(docs)
}

#[async_trait]
impl McpOAuthStore for McpOAuthStoreAdapter {
    async fn read(&self, key: &str) -> Result<Option<Value>, WireAsyncError> {
        let result = async {
            let raw = self.docs.get(CREDENTIALS_SCOPE, key).await.ok().flatten()?;
            if raw.is_object() && raw.get("iv").is_some() && raw.get("tag").is_some() && raw.get("data").is_some() {
                let blob: EncryptedBlob = serde_json::from_value(raw).ok()?;
                let decrypted = decrypt(&blob).ok()?;
                return serde_json::from_str(&decrypted).ok();
            }
            Some(raw)
        };
        Ok(result.await)
    }

    async fn write(&self, key: &str, data: Value) -> Result<(), WireAsyncError> {
        let text = serde_json::to_string(&data)?;
        let blob = encrypt(&text).map_err(|error| -> WireAsyncError { error.into() })?;
        self.docs
            .set(CREDENTIALS_SCOPE, key, serde_json::to_value(blob)?)
            .await
    }

    async fn remove(&self, key: &str) -> Result<(), WireAsyncError> {
        self.docs.delete(CREDENTIALS_SCOPE, key).await
    }

    async fn list(&self, prefix: Option<&str>) -> Result<Vec<String>, WireAsyncError> {
        self.docs.list(CREDENTIALS_SCOPE, prefix).await
    }
}

impl IMcpOAuthStore for McpOAuthStoreAdapter {}

pub fn register_mcp_oauth_store() {
    crate::_base::di::scope::register_scoped_service(
        crate::app::scopes::LifecycleScope::App,
        crate::_base::di::scope::ScopeActivation::OnDemand,
        "mcpConfig",
    );
}
