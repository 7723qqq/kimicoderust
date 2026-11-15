use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::FutureExt;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::_base::di::descriptors::DynamicValue;
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
use crate::_base::di::lifecycle::IDisposable;
use crate::_base::log::log::ILogService;
use crate::_base::utils::abort::AbortError;
use crate::app::auth::web_search::web_search::WebSearchProviderService;
use crate::app::bootstrap::bootstrap::IBootstrapService;
use crate::session::agent_lifecycle::agent_lifecycle::IAgentLifecycleService;
use crate::session::session_context::session_context::ISessionContext;
use crate::session::subagent::subagent::ISessionSubagentService;
use crate::wire::record::WireAsyncError;

use super::workflow_registry::list_builtins;
use super::workflow_runtime::{WORKFLOW_CANCELLED_ERROR_MESSAGE, WORKFLOW_DEADLINE_ERROR_MESSAGE, WorkflowRuntimeDeps, execute_workflow, workflow_now_ms};
use super::workflow_types::{SharedWorkflowRunEntry, WorkflowMeta, WorkflowRunEntry, WorkflowRunResult, WorkflowStatus};

const CANCEL_SETTLE_GRACE_MS: u64 = 5_000;

pub struct WorkflowStartInput {
    pub script: String,
    pub args: Option<Value>,
    pub caller_agent_id: String,
    pub session_context: Arc<dyn ISessionContext>,
    pub lifecycle: Arc<dyn IAgentLifecycleService>,
    pub subagents: Arc<dyn ISessionSubagentService>,
    pub deadline_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowStartResult {
    pub run_id: String,
}

#[async_trait]
pub trait IWorkflowService: Send + Sync {
    async fn start(self: Arc<Self>, input: WorkflowStartInput) -> Result<WorkflowStartResult, WireAsyncError>;
    fn status(&self, run_id: &str) -> Option<WorkflowRunResult>;
    async fn wait(&self, run_id: &str, timeout_ms: Option<u64>) -> WorkflowRunResult;
    async fn cancel(&self, run_id: &str);
    fn list_builtins(&self) -> Vec<WorkflowMeta>;
}

pub static I_WORKFLOW_SERVICE: Lazy<ServiceIdentifier<DynamicValue>> = Lazy::new(|| create_decorator("workflowService"));

pub struct WorkflowService {
    _bootstrap: Arc<dyn IBootstrapService>,
    log: Arc<dyn ILogService>,
    web_search_provider: Arc<dyn WebSearchProviderService>,
    runs: Mutex<HashMap<String, SharedWorkflowRunEntry>>,
    counter: AtomicU64,
}

impl WorkflowService {
    pub fn new(
        bootstrap: Arc<dyn IBootstrapService>,
        log: Arc<dyn ILogService>,
        web_search_provider: Arc<dyn WebSearchProviderService>,
    ) -> Self {
        Self {
            _bootstrap: bootstrap,
            log,
            web_search_provider,
            runs: Mutex::new(HashMap::new()),
            counter: AtomicU64::new(0),
        }
    }

    async fn run_workflow(
        &self,
        run_id: String,
        input: WorkflowStartInput,
        entry: SharedWorkflowRunEntry,
    ) -> Result<(), WireAsyncError> {
        let workspace_root = input.session_context.cwd();
        let deps = WorkflowRuntimeDeps {
            lifecycle: input.lifecycle.clone(),
            subagents: input.subagents.clone(),
            session_context: input.session_context.clone(),
            log: self.log.clone(),
            caller_agent_id: input.caller_agent_id.clone(),
            workspace_root,
            web_search_provider: Some(self.web_search_provider.clone()),
        };
        match execute_workflow(super::workflow_runtime::WorkflowRunOptions {
            script: input.script,
            args: input.args,
            deps,
            entry: entry.clone(),
            deadline_ms: input.deadline_ms,
            sync_prefix_limit_ms: None,
            timers: None,
            on_deadline_aborted: None,
        }).await {
            Ok(result) => {
                *entry.status.lock() = WorkflowStatus::Completed;
                *entry.result.lock() = result;
                *entry.finished_at.lock() = Some(workflow_now_ms());
                self.log.info("workflow.run.completed", Some(json!({ "runId": run_id, "agentCount": entry.agent_count() })));
            }
            Err(error) => {
                let reason = entry.abort_controller.signal().reason();
                let deadline_exceeded = reason.as_ref().is_some_and(|reason| reason.to_string() == WORKFLOW_DEADLINE_ERROR_MESSAGE);
                if entry.abort_controller.signal().aborted() && !deadline_exceeded {
                    *entry.status.lock() = WorkflowStatus::Cancelled;
                } else {
                    *entry.status.lock() = WorkflowStatus::Failed;
                    *entry.error.lock() = Some(error.to_string());
                }
                *entry.finished_at.lock() = Some(workflow_now_ms());
                self.log.warn("workflow.run.failed", Some(json!({ "runId": run_id, "error": entry.error.lock().clone() })));
            }
        }
        Ok(())
    }

    fn abort_running(&self) {
        for entry in self.runs.lock().values() {
            if entry.is_running() {
                entry.abort_controller.abort(None);
            }
        }
    }
}

#[async_trait]
impl IWorkflowService for WorkflowService {
    async fn start(self: Arc<Self>, input: WorkflowStartInput) -> Result<WorkflowStartResult, WireAsyncError> {
        let run_id = format!("wf_{}", self.counter.fetch_add(1, Ordering::SeqCst) + 1);
        let entry = Arc::new(WorkflowRunEntry::new(run_id.clone(), workflow_now_ms()));
        self.runs.lock().insert(run_id.clone(), entry.clone());

        let service = self.clone();
        let run_entry = entry.clone();
        let running_id = run_id.clone();
        let run_promise = async move {
            if let Err(error) = service.run_workflow(running_id.clone(), input, run_entry.clone()).await {
                *run_entry.status.lock() = WorkflowStatus::Failed;
                *run_entry.error.lock() = Some(error.to_string());
                *run_entry.finished_at.lock() = Some(workflow_now_ms());
                service.log.error("workflow.run.uncaught", Some(json!({ "runId": running_id, "error": run_entry.error.lock().clone() })));
            }
        }.boxed().shared();
        *entry.run_promise.lock() = Some(run_promise.clone());
        tokio::spawn(run_promise);

        Ok(WorkflowStartResult { run_id })
    }

    fn status(&self, run_id: &str) -> Option<WorkflowRunResult> {
        self.runs.lock().get(run_id).map(|entry| to_result(entry))
    }

    async fn wait(&self, run_id: &str, timeout_ms: Option<u64>) -> WorkflowRunResult {
        let entry = self.runs.lock().get(run_id).cloned();
        let Some(entry) = entry else {
            return WorkflowRunResult {
                run_id: run_id.to_string(),
                status: WorkflowStatus::Failed,
                result: None,
                error: Some("Workflow run not found".to_string()),
                current_phase: None,
                agent_count: 0,
                started_at: 0,
                finished_at: None,
            };
        };
        if !entry.is_running() {
            return to_result(&entry);
        }
        let deadline = timeout_ms.map(|timeout| workflow_now_ms().saturating_add(timeout.min(i64::MAX as u64) as i64));
        while entry.is_running() {
            if deadline.is_some_and(|deadline| workflow_now_ms() >= deadline) {
                return to_result(&entry);
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        to_result(&entry)
    }

    async fn cancel(&self, run_id: &str) {
        let entry = self.runs.lock().get(run_id).cloned();
        let Some(entry) = entry else {
            return;
        };
        if !entry.is_running() {
            return;
        }
        entry.abort_controller.abort(Some(Arc::new(AbortError::new(WORKFLOW_CANCELLED_ERROR_MESSAGE))));
        let run_promise = entry.run_promise.lock().clone();
        let Some(run_promise) = run_promise else {
            return;
        };
        let settled = tokio::time::timeout(Duration::from_millis(CANCEL_SETTLE_GRACE_MS), run_promise).await.is_ok();
        if settled {
            return;
        }
        self.log.warn("workflow.cancel.script-still-running", Some(json!({ "runId": run_id })));
        if entry.is_running() {
            *entry.status.lock() = WorkflowStatus::Cancelled;
            *entry.finished_at.lock() = Some(workflow_now_ms());
        }
    }

    fn list_builtins(&self) -> Vec<WorkflowMeta> {
        list_builtins()
    }
}

impl IDisposable for WorkflowService {
    fn dispose(&self) {
        self.abort_running();
    }
}

fn to_result(entry: &WorkflowRunEntry) -> WorkflowRunResult {
    WorkflowRunResult {
        run_id: entry.run_id.clone(),
        status: entry.status(),
        result: entry.result.lock().clone(),
        error: entry.error.lock().clone(),
        current_phase: entry.current_phase.lock().clone(),
        agent_count: entry.agent_count(),
        started_at: entry.started_at,
        finished_at: *entry.finished_at.lock(),
    }
}
