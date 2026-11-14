use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::wire::record::WireAsyncError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ManagedProviderStatus {
    Authenticated,
    Expired,
    Revoked,
    Unauthenticated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedProviderSummary {
    pub name: String,
    pub status: ManagedProviderStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthSummary {
    pub models_ready: bool,
    pub providers_count: usize,
    pub managed_provider: Option<ManagedProviderSummary>,
}

#[async_trait]
pub trait IAuthLegacyService: Send + Sync {
    async fn get(&self) -> Result<AuthSummary, WireAsyncError>;
}
