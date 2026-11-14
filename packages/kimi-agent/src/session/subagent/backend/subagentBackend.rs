use std::sync::Arc;

use async_trait::async_trait;
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};

use crate::_base::utils::abort::AbortSignal;
use crate::wire::record::WireAsyncError;

pub const SUBAGENT_BACKEND_NAMES: &[&str] = &["claude-code", "codex", "acp"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SubagentBackendName {
    #[serde(rename = "claude-code")]
    ClaudeCode,
    #[serde(rename = "codex")]
    Codex,
    #[serde(rename = "acp")]
    Acp,
}

impl SubagentBackendName {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::Acp => "acp",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "claude-code" => Some(Self::ClaudeCode),
            "codex" => Some(Self::Codex),
            "acp" => Some(Self::Acp),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubagentBackendStopReason {
    #[serde(rename = "completed")]
    Completed,
    #[serde(rename = "aborted")]
    Aborted,
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "max-tokens")]
    MaxTokens,
    #[serde(rename = "refusal")]
    Refusal,
}

#[derive(Clone)]
pub struct SubagentBackendStartRequest {
    pub prompt: String,
    pub cwd: String,
    pub signal: AbortSignal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentBackendResult {
    pub output: String,
    pub stop_reason: SubagentBackendStopReason,
}

pub type SubagentBackendResultFuture =
    BoxFuture<'static, Result<SubagentBackendResult, WireAsyncError>>;
pub type SubagentBackendDispose =
    Arc<dyn Fn() -> BoxFuture<'static, Result<(), WireAsyncError>> + Send + Sync>;

pub struct SubagentBackendRun {
    pub id: String,
    pub result: SubagentBackendResultFuture,
    pub dispose: SubagentBackendDispose,
}

#[async_trait]
pub trait ISubagentBackend: Send + Sync {
    fn name(&self) -> SubagentBackendName;
    async fn start(
        &self,
        request: SubagentBackendStartRequest,
    ) -> Result<SubagentBackendRun, WireAsyncError>;
}

#[async_trait]
pub trait ISubagentBackendService: Send + Sync {
    fn get_backend(&self, name: SubagentBackendName) -> Option<Arc<dyn ISubagentBackend>>;
    fn list(&self) -> Vec<Arc<dyn ISubagentBackend>>;
    async fn start(
        &self,
        name: SubagentBackendName,
        request: SubagentBackendStartRequest,
    ) -> Result<SubagentBackendRun, WireAsyncError>;
}
