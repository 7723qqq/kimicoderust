use std::sync::Arc;

use async_trait::async_trait;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::_base::di::descriptors::DynamicValue;
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
use crate::agent::scope_context::scope_context::IAgentScopeContext;
use crate::app::bootstrap::bootstrap::IBootstrapService;
use crate::app::workflow::workflow_registry::{get_builtin, resolve_user_workflow};
use crate::app::workflow::workflow_runtime::workflow_now_ms;
use crate::app::workflow::workflow_service::{IWorkflowService, WorkflowStartInput};
use crate::app::workflow::workflow_types::{WorkflowRunResult, WorkflowStatus};
use crate::session::agent_lifecycle::agent_lifecycle::IAgentLifecycleService;
use crate::session::session_context::session_context::ISessionContext;
use crate::session::subagent::subagent::ISessionSubagentService;
use crate::wire::record::WireAsyncError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkflowOperation {
    Run,
    Status,
    Wait,
    Cancel,
}

impl WorkflowOperation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::Status => "status",
            Self::Wait => "wait",
            Self::Cancel => "cancel",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowToolInput {
    pub operation: WorkflowOperation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_ms: Option<u64>,
}

pub static WORKFLOW_TOOL_INPUT_SCHEMA: Lazy<Value> = Lazy::new(|| json!({
    "type": "object",
    "additionalProperties": false,
    "required": ["operation"],
    "properties": {
        "operation": {
            "type": "string",
            "enum": ["run", "status", "wait", "cancel"],
            "description": "The workflow operation to perform."
        },
        "name": {
            "type": "string",
            "description": "Built-in or user workflow name (for `run`). Mutually exclusive with `script`."
        },
        "script": {
            "type": "string",
            "description": "Inline workflow script (for `run`). Mutually exclusive with `name`."
        },
        "args": {
            "type": "string",
            "description": "Arguments to pass to the workflow (for `run`)."
        },
        "run_id": {
            "type": "string",
            "description": "Workflow run ID (for `status`, `wait`, `cancel`)."
        },
        "timeout_ms": {
            "type": "number",
            "description": "Timeout in milliseconds (for `wait`)."
        },
        "deadline_ms": {
            "type": "number",
            "description": "Total runtime limit in milliseconds for the script (for `run`). Defaults to 1800000 (30 minutes)."
        }
    }
}));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSideEffects {
    Shell,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowToolOutput {
    pub output: String,
    #[serde(rename = "isError")]
    pub is_error: bool,
    #[serde(rename = "stopTurn")]
    pub stop_turn: bool,
}

impl WorkflowToolOutput {
    fn ok(output: String) -> Self {
        Self {
            output,
            is_error: false,
            stop_turn: false,
        }
    }

    fn error(output: String) -> Self {
        Self {
            output,
            is_error: true,
            stop_turn: false,
        }
    }

    fn completed(output: String) -> Self {
        Self {
            output,
            is_error: false,
            stop_turn: true,
        }
    }
}

pub trait IWorkflowTool: Send + Sync {
    fn name(&self) -> &'static str;
    fn side_effect(&self) -> ToolSideEffects;
    fn description(&self) -> &'static str;
    fn parameters(&self) -> Value;
    fn resolve_execution(self: Arc<Self>, args: WorkflowToolInput) -> WorkflowToolExecution;
}

pub static I_WORKFLOW_TOOL: Lazy<ServiceIdentifier<DynamicValue>> = Lazy::new(|| create_decorator("workflowTool"));

pub struct WorkflowTool {
    workflow: Arc<dyn IWorkflowService>,
    bootstrap: Arc<dyn IBootstrapService>,
    session_context: Arc<dyn ISessionContext>,
    lifecycle: Arc<dyn IAgentLifecycleService>,
    subagents: Arc<dyn ISessionSubagentService>,
    scope_context: Arc<dyn IAgentScopeContext>,
}

impl WorkflowTool {
    pub fn new(
        workflow: Arc<dyn IWorkflowService>,
        bootstrap: Arc<dyn IBootstrapService>,
        session_context: Arc<dyn ISessionContext>,
        lifecycle: Arc<dyn IAgentLifecycleService>,
        subagents: Arc<dyn ISessionSubagentService>,
        scope_context: Arc<dyn IAgentScopeContext>,
    ) -> Self {
        Self {
            workflow,
            bootstrap,
            session_context,
            lifecycle,
            subagents,
            scope_context,
        }
    }

    async fn handle_run(&self, args: WorkflowToolInput) -> Result<WorkflowToolOutput, WireAsyncError> {
        let name = args.name.as_deref().filter(|name| !name.is_empty());
        let inline_script = args.script.as_deref().filter(|script| !script.is_empty());
        if name.is_none() && inline_script.is_none() {
            return Ok(WorkflowToolOutput::error("Either `name` or `script` is required for `run`.".to_string()));
        }
        if name.is_some() && inline_script.is_some() {
            return Ok(WorkflowToolOutput::error("`name` and `script` are mutually exclusive.".to_string()));
        }
        let (script, workflow_name) = if let Some(name) = name {
            if let Some(builtin) = get_builtin(name) {
                (builtin.script.clone(), builtin.meta.name.clone())
            } else {
                let Some(user_workflow) = resolve_user_workflow(&self.bootstrap.home_dir(), name).await else {
                    return Ok(WorkflowToolOutput::error(format!("Workflow not found: {name}")));
                };
                (user_workflow.script, user_workflow.meta.name)
            }
        } else {
            (inline_script.unwrap_or_default().to_string(), "inline".to_string())
        };
        let started = self.workflow.clone().start(WorkflowStartInput {
            script,
            args: args.args.map(Value::String),
            caller_agent_id: self.scope_context.agent_id(),
            session_context: self.session_context.clone(),
            lifecycle: self.lifecycle.clone(),
            subagents: self.subagents.clone(),
            deadline_ms: args.deadline_ms,
        }).await?;
        let run_id = started.run_id;
        Ok(WorkflowToolOutput::ok(format!(
            "Workflow \"{workflow_name}\" started. run_id: {run_id}\nUse Workflow({{ operation: \"wait\", run_id: \"{run_id}\" }}) to block until it completes, or Workflow({{ operation: \"status\", run_id: \"{run_id}\" }}) to check progress."
        )))
    }

    async fn handle_status(&self, args: WorkflowToolInput) -> WorkflowToolOutput {
        let Some(run_id) = args.run_id.as_deref().filter(|run_id| !run_id.is_empty()) else {
            return WorkflowToolOutput::error("`run_id` is required for `status`.".to_string());
        };
        match self.workflow.status(run_id) {
            Some(result) => WorkflowToolOutput::ok(format_status(&result)),
            None => WorkflowToolOutput::ok(format!("Workflow run not found: {run_id}")),
        }
    }

    async fn handle_wait(&self, args: WorkflowToolInput) -> WorkflowToolOutput {
        let Some(run_id) = args.run_id.as_deref().filter(|run_id| !run_id.is_empty()) else {
            return WorkflowToolOutput::error("`run_id` is required for `wait`.".to_string());
        };
        let result = self.workflow.wait(run_id, args.timeout_ms).await;
        if result.status == WorkflowStatus::Completed {
            let result_str = match result.result.as_ref() {
                Some(Value::String(text)) => text.clone(),
                Some(value) => serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()),
                None => "undefined".to_string(),
            };
            let duration = (result.finished_at.unwrap_or(0) - result.started_at) as f64 / 1000.0;
            return WorkflowToolOutput::completed(format!(
                "Workflow completed.\nAgent runs: {}\nDuration: {duration}s\n\nResult:\n{result_str}",
                result.agent_count
            ));
        }
        WorkflowToolOutput::ok(format_status(&result))
    }

    async fn handle_cancel(&self, args: WorkflowToolInput) -> WorkflowToolOutput {
        let Some(run_id) = args.run_id.as_deref().filter(|run_id| !run_id.is_empty()) else {
            return WorkflowToolOutput::error("`run_id` is required for `cancel`.".to_string());
        };
        self.workflow.cancel(run_id).await;
        WorkflowToolOutput::ok(format!("Workflow cancelled: {run_id}"))
    }
}

impl IWorkflowTool for WorkflowTool {
    fn name(&self) -> &'static str {
        "Workflow"
    }

    fn side_effect(&self) -> ToolSideEffects {
        ToolSideEffects::Shell
    }

    fn description(&self) -> &'static str {
        DESCRIPTION
    }

    fn parameters(&self) -> Value {
        WORKFLOW_TOOL_INPUT_SCHEMA.clone()
    }

    fn resolve_execution(self: Arc<Self>, args: WorkflowToolInput) -> WorkflowToolExecution {
        WorkflowToolExecution {
            description: format!("Workflow: {}", args.operation.as_str()),
            approval_rule: "Workflow".to_string(),
            tool: self,
            args,
        }
    }
}

pub struct WorkflowToolExecution {
    pub description: String,
    pub approval_rule: String,
    tool: Arc<WorkflowTool>,
    args: WorkflowToolInput,
}

impl WorkflowToolExecution {
    pub async fn execute(&self) -> Result<WorkflowToolOutput, WireAsyncError> {
        match self.args.operation {
            WorkflowOperation::Run => self.tool.handle_run(self.args.clone()).await,
            WorkflowOperation::Status => Ok(self.tool.handle_status(self.args.clone()).await),
            WorkflowOperation::Wait => Ok(self.tool.handle_wait(self.args.clone()).await),
            WorkflowOperation::Cancel => Ok(self.tool.handle_cancel(self.args.clone()).await),
        }
    }
}

fn format_status(result: &WorkflowRunResult) -> String {
    let elapsed = (result.finished_at.unwrap_or_else(workflow_now_ms) - result.started_at) as f64 / 1000.0;
    let mut lines = vec![
        format!("run_id: {}", result.run_id),
        format!("status: {}", result.status.as_str()),
        format!("agents: {}", result.agent_count),
        format!("elapsed: {elapsed:.1}s"),
    ];
    if let Some(phase) = result.current_phase.as_deref().filter(|phase| !phase.is_empty()) {
        lines.push(format!("phase: {phase}"));
    }
    if let Some(error) = result.error.as_deref().filter(|error| !error.is_empty()) {
        lines.push(format!("error: {error}"));
    }
    lines.join("\n")
}

pub fn workflow_tool_registration_matches(agent_id: &str) -> bool {
    agent_id == "main"
}

pub const DESCRIPTION: &str = r##"Run multi-phase orchestrated agent workflows.

## Operations

### `run` — start a workflow
Launches a workflow in the background. Returns immediately with a `run_id`; the result is delivered as a notification when the workflow completes.

```
Workflow({ operation: "run", name: "deep-research", args: "How does RAG compare to fine-tuning?" })
```

You can also run an inline script:
```
Workflow({ operation: "run", script: "export const meta = { name: 'quick', description: '...' };\nphase('search');\nconst result = await agent('Search for...');\nreturn result;", args: "..." })
```

The script runs under a 30-minute default deadline, counted from launch and covering the whole script, not just its synchronous start. Pass `deadline_ms` to raise or lower it for a single run — a long workflow that intentionally runs for hours must ask for the time it needs:
```
Workflow({ operation: "run", name: "deep-research", args: "...", deadline_ms: 7200000 })
```
A script that exceeds the deadline is aborted: the run ends as `failed` with `Workflow script deadline exceeded`, and any subagent it started is cancelled. The same applies to a script stuck in a tight synchronous loop with no `await`: that kind of loop blocks the process it runs in, so it is cut off at 30 seconds and the run fails.

### `status` — check progress
```
Workflow({ operation: "status", run_id: "wf_1" })
```

### `wait` — block until done
```
Workflow({ operation: "wait", run_id: "wf_1", timeout_ms: 60000 })
```

### `cancel` — cancel a running workflow
```
Workflow({ operation: "cancel", run_id: "wf_1" })
```

## Built-in workflows

- **deep-research** — Multi-source research with adversarial jury fact-checking. Plans search lines, runs parallel web searches, extracts facts from top sources, cross-checks each fact with 3 jurors (majority reject drops it), and writes a cited report.
- **code-review** — Multi-dimensional code review across security, performance, correctness, and maintainability. Runs parallel reviewers and produces a consolidated severity-ranked report.
- **test-generator** — Automated test generation. Analyzes source code, plans test cases, generates test files with proper mocks, and validates output.
- **refactor-planner** — Refactoring impact analysis and migration planner. Identifies improvement opportunities, assesses risk, and generates a phased plan with rollback steps.
- **bug-triage** — Automated bug triage. Parses error logs and stack traces, reads suspect source files, identifies root causes, and generates fixes with reproduction steps.
- **pr-description** — PR description generator. Analyzes code diffs, extracts semantic intent, and generates a comprehensive PR body with changelog entries and reviewer guidance.
- **architecture-review** — Architecture and dependency analysis. Detects circular dependencies, measures coupling, identifies layer violations, and generates architecture documentation.
- **security-audit** — Security vulnerability audit. Scans for OWASP Top 10, hardcoded secrets, injection points, auth flaws, and produces a prioritized remediation plan.
- **migration-planner** — Migration and upgrade planner. Analyzes dependency versions, identifies breaking changes, maps code changes, and generates a phased migration plan with compatibility shims.

## In-script primitives

Workflow scripts are JavaScript that runs in a sandbox with these injected globals:

- **`agent(prompt, opts?)`** — Spawn a subagent. Returns the agent's result (parsed JSON if `opts.schema` given, else text) or `null` on failure. **Never throws.**
  - `opts.agentType` — subagent profile (default 'coder')
  - `opts.schema` — JSON schema for structured output
  - `opts.label` — display label

- **`parallel(thunks)`** — Run thunks concurrently. Returns array of results.

- **`pipeline(items, ...stages)`** — Run each item through all stages. No barrier between stages — items flow through independently.

- **`phase(title)`** — Set the current phase (for progress tracking).

- **`log(message)`** — Log a message.

- **`readFile(path)` / `writeFile(path, content)` / `glob(pattern)` / `exists(path)`** — File IO jailed to the workspace root.

- **`args`** — The `args` value passed to `run`.

- **`fetch(url, opts?)`** — Make an HTTP request. Returns `{ ok, status, body }`. `opts` can have `method`, `headers`, `body`. 15s timeout. Never throws — failures return `{ ok: false, status: 0, body: errorMessage }`.

- **`search(query)`** — Web search. Returns an array of `{ title, url, snippet }`. Returns empty array when search is unavailable or fails. Never throws.

- **`exec(command, opts?)`** — Execute a shell command. Returns `{ stdout, stderr, exitCode }`. `opts` can have `timeoutMs` (default 30s) and `cwd` (relative to workspace root). The command runs in the workspace directory. Never throws — non-zero exit codes are returned, not thrown.

## When to use

- **Deep research** — when the user wants a thorough, multi-source, fact-checked answer
- **Code review** — when the user wants a multi-dimensional review of code changes
- **Test generation** — when the user wants to add tests for existing untested code
- **Refactoring** — when the user wants to plan safe, incremental code improvements
- **Bug triage** — when the user reports an error, crash, or test failure
- **PR descriptions** — when the user needs to document code changes or a pull request
- **Architecture review** — when evaluating system design or detecting design decay
- **Security audit** — when auditing code for OWASP Top 10 vulnerabilities
- **Migration planning** — when upgrading dependencies or migrating between frameworks
- **Complex orchestration** — when a task requires multiple phases of parallel agent work
- **Structured pipelines** — when you need to chain agent outputs through transformation stages
"##;
