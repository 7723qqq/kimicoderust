use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::wire::record::WireAsyncError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptLaunchResult {
    pub turn_id: u64,
}

#[async_trait]
pub trait IRestGateway: Send + Sync {
    async fn prompt(
        &self,
        session_id: &str,
        agent_id: &str,
        input: &str,
    ) -> Result<Option<PromptLaunchResult>, WireAsyncError>;

    async fn steer(
        &self,
        session_id: &str,
        agent_id: &str,
        content: &str,
    ) -> Result<Option<PromptLaunchResult>, WireAsyncError>;

    async fn cancel(
        &self,
        session_id: &str,
        agent_id: &str,
        reason: Option<String>,
    ) -> Result<(), WireAsyncError>;

    async fn get_status(&self, session_id: &str) -> Value;

    async fn flush_logs(&self, session_id: &str) -> Result<(), WireAsyncError>;

    async fn flush_global_logs(&self) -> Result<(), WireAsyncError>;
}

pub trait IWSGateway: Send + Sync {
    fn connect(&self, connection_id: &str);
    fn broadcast(&self, session_id: &str, event: Value);
}
