use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures_util::future::{BoxFuture, Shared};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::_base::utils::abort::AbortController;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowMeta {
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when_to_use: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phases: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
pub struct WorkflowEntry {
    pub meta: WorkflowMeta,
    pub script: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentOpts {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkflowStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl WorkflowStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowRunResult {
    pub run_id: String,
    pub status: WorkflowStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_phase: Option<String>,
    pub agent_count: usize,
    pub started_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<i64>,
}

pub type WorkflowRunPromise = Shared<BoxFuture<'static, ()>>;

pub struct WorkflowRunEntry {
    pub run_id: String,
    pub status: Mutex<WorkflowStatus>,
    pub result: Mutex<Option<Value>>,
    pub error: Mutex<Option<String>>,
    pub current_phase: Mutex<Option<String>>,
    pub agent_count: AtomicUsize,
    pub started_at: i64,
    pub finished_at: Mutex<Option<i64>>,
    pub abort_controller: AbortController,
    pub run_promise: Mutex<Option<WorkflowRunPromise>>,
}

impl WorkflowRunEntry {
    pub fn new(run_id: String, started_at: i64) -> Self {
        Self {
            run_id,
            status: Mutex::new(WorkflowStatus::Running),
            result: Mutex::new(None),
            error: Mutex::new(None),
            current_phase: Mutex::new(None),
            agent_count: AtomicUsize::new(0),
            started_at,
            finished_at: Mutex::new(None),
            abort_controller: AbortController::new(),
            run_promise: Mutex::new(None),
        }
    }

    pub fn status(&self) -> WorkflowStatus {
        *self.status.lock()
    }

    pub fn is_running(&self) -> bool {
        self.status() == WorkflowStatus::Running
    }

    pub fn record_agent(&self) {
        self.agent_count.fetch_add(1, Ordering::SeqCst);
    }

    pub fn agent_count(&self) -> usize {
        self.agent_count.load(Ordering::SeqCst)
    }

    pub fn set_phase(&self, phase: String) {
        *self.current_phase.lock() = Some(phase);
    }
}

pub type SharedWorkflowRunEntry = Arc<WorkflowRunEntry>;
