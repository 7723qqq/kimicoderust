use async_trait::async_trait;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};

use crate::_base::di::descriptors::DynamicValue;
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
use crate::_base::utils::abort::AbortSignal;
use crate::wire::record::WireAsyncError;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellEnvironment {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub term: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub term_program: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub term_program_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multiplexer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportSessionPayload {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_global_log: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_desktop_log: Option<bool>,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desktop_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell_env: Option<ShellEnvironment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportSessionManifest {
    pub session_id: String,
    pub exported_at: String,
    pub kimi_code_version: String,
    pub wire_protocol_version: String,
    pub os: String,
    pub nodejs_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_first_activity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_last_activity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_log_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub global_log_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desktop_log_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_log_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desktop_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell_env: Option<ShellEnvironment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportSessionResult {
    pub zip_path: String,
    pub entries: Vec<String>,
    pub session_dir: String,
    pub manifest: ExportSessionManifest,
}

#[derive(Clone, Default)]
pub struct ExportSessionOptions {
    pub web_log: Option<String>,
    pub signal: Option<AbortSignal>,
}

#[async_trait]
pub trait ISessionExportService: Send + Sync {
    async fn export(
        &self,
        input: ExportSessionPayload,
        options: Option<ExportSessionOptions>,
    ) -> Result<ExportSessionResult, WireAsyncError>;
}

pub static I_SESSION_EXPORT_SERVICE: Lazy<ServiceIdentifier<DynamicValue>> =
    Lazy::new(|| create_decorator("sessionExportService"));
