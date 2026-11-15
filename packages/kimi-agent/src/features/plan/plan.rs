use std::path::PathBuf;
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
use crate::environment::environment::EnvironmentPath;
use crate::os::interface::host_environment::PathClass;
use crate::os::interface::host_file_system::IHostFileSystem;

pub const AGENT_PLAN_SERVICE_ID: &str = "agentPlanService";

#[derive(Clone)]
pub struct PlanFileTarget {
    pub fs: Arc<dyn IHostFileSystem>,
    pub path: PathBuf,
    pub environment_path: EnvironmentPath,
    pub path_class: PathClass,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PlanData {
    pub id: String,
    pub content: String,
    pub path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanFileOperation {
    Read,
    Write,
}

pub type PlanFilePath = Option<String>;

#[async_trait]
pub trait IAgentPlanService: Send + Sync {
    async fn enter(&self, id: Option<String>, create_file: bool) -> Result<(), String>;
    fn cancel(&self, id: Option<String>);
    async fn clear(&self) -> Result<(), String>;
    fn exit(&self, id: Option<String>);
    async fn record_revision(&self) -> Result<(), String>;
    async fn resolve_file(
        &self,
        path: &str,
        operation: Option<PlanFileOperation>,
    ) -> Option<PlanFileTarget>;
    async fn status(&self) -> Result<Option<PlanData>, String>;
}


pub static I_AGENT_PLAN_SERVICE: LazyLock<ServiceIdentifier<Arc<dyn IAgentPlanService>>> =
    LazyLock::new(|| create_decorator("agentPlanService"));
