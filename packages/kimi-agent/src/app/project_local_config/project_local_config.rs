use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::wire::record::WireAsyncError;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectAdditionalDirsLoadResult {
    pub project_root: String,
    pub config_path: String,
    pub additional_dirs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectPermissionRulesLoadResult {
    pub project_root: String,
    pub config_path: String,
    pub rules: Vec<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AdditionalDirSource {
    Session,
    Config,
}

#[async_trait]
pub trait IProjectLocalConfigService: Send + Sync {
    async fn read_additional_dirs(&self, work_dir: &str) -> Result<ProjectAdditionalDirsLoadResult, WireAsyncError>;
    async fn resolve_additional_dirs(&self, base_dir: &str, additional_dirs: &[String], source: Option<AdditionalDirSource>) -> Result<Vec<String>, WireAsyncError>;
    async fn append_additional_dir(&self, work_dir: &str, input_path: &str) -> Result<ProjectAdditionalDirsLoadResult, WireAsyncError>;
    async fn read_permission_rules(&self, work_dir: &str) -> Result<ProjectPermissionRulesLoadResult, WireAsyncError>;
    async fn append_permission_rules(&self, work_dir: &str, rules: &[Value]) -> Result<ProjectPermissionRulesLoadResult, WireAsyncError>;
}
