use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::rpc::types::BoxFuture;

pub use crate::tools::spec_mode::{SPEC_DIR_NAME, SPEC_REQUIRED_FILES, spec_dir};

pub const SPEC_REQUIREMENTS_FILE: &str = "requirements.md";
pub const SPEC_DESIGN_FILE: &str = "design.md";
pub const SPEC_TASKS_FILE: &str = "tasks.md";
pub const SPEC_PROGRESS_FILE: &str = "progress.md";
pub const SPEC_ALL_FILES: &[&str] = &[
    SPEC_REQUIREMENTS_FILE,
    SPEC_DESIGN_FILE,
    SPEC_TASKS_FILE,
    SPEC_PROGRESS_FILE,
];
pub const AGENT_SPEC_SERVICE_ID: &str = "agentSpecService";

pub trait IAgentSpecService: Send + Sync {
    fn status(&self) -> BoxFuture<'static, Result<Option<SpecData>, String>>;

    fn record_revision(&self, data: SpecData) -> BoxFuture<'static, Result<(), String>>;

    fn exit(&self) -> BoxFuture<'static, Result<(), String>>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SpecStage {
    Specify,
    Plan,
    Tasks,
    Implement,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SpecData {
    pub id: String,
    pub dir: String,
    pub files: BTreeMap<String, String>,
    pub progress: String,
    pub missing: Vec<String>,
    pub complete: bool,
    pub stage: SpecStage,
}

pub fn stage_for(missing: &[String]) -> SpecStage {
    if missing.iter().any(|name| name == SPEC_REQUIREMENTS_FILE) {
        SpecStage::Specify
    } else if missing.iter().any(|name| name == SPEC_DESIGN_FILE) {
        SpecStage::Plan
    } else if missing.iter().any(|name| name == SPEC_TASKS_FILE) {
        SpecStage::Tasks
    } else {
        SpecStage::Implement
    }
}
