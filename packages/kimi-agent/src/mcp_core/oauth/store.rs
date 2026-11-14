use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::_base::utils::paths::basename;
use crate::wire::record::WireAsyncError;
use kimi_core::primitives::errors::Error2;
use kimi_core::primitives::errors::domains::config::CONFIG_INVALID;

pub fn sanitize_store_key(name: &str) -> Result<String, WireAsyncError> {
    let mut safe = String::new();
    let mut underscore = false;
    for ch in basename(name).chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' {
            safe.push(ch);
            underscore = false;
        } else if !underscore {
            safe.push('_');
            underscore = true;
        }
    }
    if safe.is_empty() || safe.starts_with('.') {
        return Err(Box::new(Error2::new(
            CONFIG_INVALID,
            format!("Invalid MCP OAuth store key: \"{name}\""),
        )));
    }
    Ok(safe)
}

pub fn canonical_mcp_oauth_resource(server_url: &str) -> Result<String, WireAsyncError> {
    let mut url = url::Url::parse(server_url)?;
    url.set_fragment(None);
    Ok(url.to_string())
}

pub fn mcp_oauth_store_key(server_name: &str, server_url: &str) -> Result<String, WireAsyncError> {
    let safe_name = sanitize_store_key(server_name)?;
    let resource = canonical_mcp_oauth_resource(server_url)?;
    let mut hasher = Sha256::new();
    hasher.update(server_name.as_bytes());
    hasher.update([0]);
    hasher.update(resource.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    Ok(format!("{safe_name}-{}", &digest[..24]))
}

pub const META_SUFFIX: &str = "-meta.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpOAuthStoreMeta {
    pub server_name: String,
    pub server_url: String,
}

#[async_trait]
pub trait McpOAuthStore: Send + Sync {
    async fn read(&self, key: &str) -> Result<Option<serde_json::Value>, WireAsyncError>;
    async fn write(&self, key: &str, data: serde_json::Value) -> Result<(), WireAsyncError>;
    async fn remove(&self, key: &str) -> Result<(), WireAsyncError>;
    async fn list(&self, prefix: Option<&str>) -> Result<Vec<String>, WireAsyncError>;
}
