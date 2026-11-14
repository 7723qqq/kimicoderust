use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::workspace_trust::WorkspaceTrustAsyncError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrustGatedMcpTransport {
    Stdio,
    Http,
    Sse,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrustGatedMcpServer {
    pub name: String,
    pub transport: TrustGatedMcpTransport,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub origin: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrustGatedInstructionSources {
    pub agents_md_paths: Vec<String>,
    pub skills: Vec<String>,
    pub agent_profiles: Vec<String>,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrustGatedActivation {
    pub mcp_servers: Vec<TrustGatedMcpServer>,
    pub additional_dirs: Vec<String>,
    pub additional_dir_sources: Vec<String>,
    pub warnings: Vec<String>,
    pub instruction_sources: TrustGatedInstructionSources,
}

#[async_trait]
pub trait IWorkspaceTrustDisclosure: Send + Sync {
    async fn describe_gated_activation(
        &self,
        pending_additional_dirs: Option<&[String]>,
        base_dir: Option<&str>,
    ) -> Result<TrustGatedActivation, WorkspaceTrustAsyncError>;
}
