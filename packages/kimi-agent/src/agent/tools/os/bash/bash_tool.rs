use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};

use super::bash::{BashInput, DEFAULT_BACKGROUND_TIMEOUT_S, IBashTool, validate_bash_input};
use super::process_task::{
    AgentTaskSettlement as LegacyAgentTaskSettlement, AgentTaskSink as LegacyAgentTaskSink,
    ProcessTask, ProcessTaskOutputCallback, ProcessTaskOutputKind,
};
use crate::_base::errors::unexpected_error::on_unexpected_error;
use crate::_base::utils::abort::{AbortSignal, abortable};
use crate::_base::utils::render_prompt::render_prompt;
use crate::agent::environment_binding::agent_environment::{
    IAgentEnvironmentService, acquire_or_when_ready, pinned_generation,
};
use crate::agent::task::config_section::resolve_agent_task_config;
use crate::agent::task::task::{
    ForegroundTaskReleaseReason, IAgentTaskService, RegisterAgentTaskOptions,
};
use crate::agent::task::types::{
    AgentTask, AgentTaskInfo, AgentTaskInfoBase, AgentTaskKind,
    AgentTaskSettlement as NewAgentTaskSettlement, AgentTaskSettlementStatus, AgentTaskSink,
    ProcessTaskInfo as AgentProcessTaskInfo,
};
use crate::agent::tool_executor::tool_executor::{
    ExecutableToolContext, ExecutableToolResult, RunnableToolExecution, ToolExecution,
};
use crate::agent::tool_executor::tool_executor_events::ToolUpdate;
use crate::agent::tool_policy::tool_policy::IAgentToolPolicyService;
use crate::agent::tools::os::shell::shell_tool_results::{
    BackgroundStartScenario, MS_PER_SECOND, ShellResultLabels, ShellTaskService, ShellToolResults,
    foreground_description, kill_spawned_process, normalize_timeout_ms,
};
use crate::app::config::config::IConfigService;
use crate::app::flag::flag::IFlagService;
use crate::environment::environment::{
    EnvironmentLease, EnvironmentResource, EnvironmentWorkspaceRoots,
};
use crate::environment::environment_workspace_view::EnvironmentWorkspaceView;
use crate::features::operationLog::flag::OPERATION_LOG_FLAG_ID;
use crate::os::interface::host_process::{IHostProcess, IHostProcessService};
use crate::session::session_context::session_context::ISessionContext;
use crate::session::workspace_context::workspace_context::ISessionWorkspaceContext;
use crate::storage::task_runner::TaskStatus;
use crate::subagent::types::ParentCancel;
use crate::tool::input_schema::to_input_json_schema;
use crate::tool::output_accumulator::ToolOutputAccumulator;
use crate::tool::rule_match::{literal_rule_pattern, matches_glob_rule_subject};
use crate::tool::tool_contract::DEFAULT_TOOL_RESULT_MAX_CHARS;
use crate::tool::tool_input_display::{CommandLanguage, ToolInputDisplay};
use crate::wire::record::WireAsyncError;

const BACKGROUND_GUIDANCE: &str = "For long-running builds, tests, watchers, or servers that the conversation should not wait on, set `run_in_background=true` with a short `description`; the call returns a task ID. Set `disable_timeout=true` only when the task should run without a timeout. You are notified automatically when the task completes, so return control to the user instead of waiting on it. Use `TaskOutput` only for a non-blocking snapshot, and `TaskStop` only to cancel.";
const NO_BACKGROUND_GUIDANCE: &str = "Background execution is disabled for this agent. Do not set `run_in_background=true`.";
const TIMEOUT_MOVES_TO_BACKGROUND: &str = "Set `timeout` for possibly long-running commands. A foreground command that hits its timeout is moved to the background instead of being killed, and you are notified when it completes. The user can also move a running foreground command to the background at any time.";
const TIMEOUT_KILLS_WITH_BACKGROUND: &str = "Set `timeout` for possibly long-running commands. A foreground command that hits its timeout is killed. The user can also move a running foreground command to the background at any time.";
const TIMEOUT_KILLS: &str = "Set `timeout` for possibly long-running commands. A command that hits its timeout is killed.";
const BASH_DESCRIPTION_TEMPLATE: &str = r#"Execute a `${SHELL_NAME}` command. Use this for shell semantics — pipes, env, and anything the dedicated tools do not cover.

- `ls` / `find` (list files) → `Glob`
- `grep` / `rg` (search file contents) → `Grep`
- `echo` / `printf` (talk to the user) → just output text directly

Each call runs in a fresh shell: variables and `cd` do not persist, so pass `cwd` or use absolute paths. stdout and stderr are combined and may be truncated; a non-zero exit ends the output with `Command failed with exit code: N`. Do not run interactive commands or commands that never exit. ${TIMEOUT_GUIDANCE}

${BACKGROUND_GUIDANCE}

- Use `&&` for commands that depend on each other. Issue independent read-only commands (separate `git show`, `ls`, or status checks) as parallel Bash calls in one response instead of chaining them or stitching their output with `echo` separators.
- Avoid `..` paths outside the working directory, and do not modify files outside it unless explicitly instructed.
- Do not use superuser privileges unless explicitly instructed. Run git-mutating commands such as `git commit`, `git push`, `git reset`, and `git rebase` only when the user asks for them.
- Command availability depends on the host; run `which <command>` before relying on an uncommon one. For GitHub work (PRs, issues, CI runs, API queries), prefer the `gh` CLI when installed.
"#;

static PARAMETERS: std::sync::LazyLock<Value> = std::sync::LazyLock::new(|| {
    to_input_json_schema(json!({
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "minLength": 1,
                "description": "The command to execute."
            },
            "cwd": {
                "type": "string",
                "description": "The working directory in which to run the command. When omitted, the command runs in the session's working directory."
            },
            "timeout": {
                "default": 60,
                "description": "Optional timeout in seconds for the command to execute. Foreground default 60s, max 300s. Background default 600s, max 86400s. Ignored for background commands when disable_timeout=true.",
                "anyOf": [
                    { "type": "integer", "exclusiveMinimum": 0, "maximum": 300 },
                    { "type": "integer", "exclusiveMinimum": 0, "maximum": 86400 }
                ]
            },
            "description": {
                "type": "string",
                "description": "A short description for the background task. Required when run_in_background is true."
            },
            "run_in_background": {
                "type": "boolean",
                "description": "Whether to run the command as a background task."
            },
            "disable_timeout": {
                "type": "boolean",
                "description": "If true, do not apply a timeout to the command. Only applies when run_in_background is true."
            }
        },
        "required": ["command"]
    }))
});

#[derive(Clone)]
pub struct BashTool {
    environment: Arc<dyn IAgentEnvironmentService>,
    ctx: Arc<dyn ISessionContext>,
    workspace_ctx: Arc<dyn ISessionWorkspaceContext>,
    tasks: Arc<dyn IAgentTaskService>,
    tool_policy: Arc<dyn IAgentToolPolicyService>,
    config: Arc<dyn IConfigService>,
    flags: Option<Arc<dyn IFlagService>>,
}

impl BashTool {
    pub fn new(
        environment: Arc<dyn IAgentEnvironmentService>,
        ctx: Arc<dyn ISessionContext>,
        workspace_ctx: Arc<dyn ISessionWorkspaceContext>,
        tasks: Arc<dyn IAgentTaskService>,
        tool_policy: Arc<dyn IAgentToolPolicyService>,
        config: Arc<dyn IConfigService>,
        flags: Option<Arc<dyn IFlagService>>,
    ) -> Self {
        Self {
            environment,
            ctx,
            workspace_ctx,
            tasks,
            tool_policy,
            config,
            flags,
        }
    }

    pub fn name(&self) -> &'static str {
        "Bash"
    }

    pub fn side_effect(&self) -> &'static str {
        "shell"
    }

    pub fn parameters(&self) -> Map<String, Value> {
        PARAMETERS
            .as_object()
            .cloned()
            .expect("Bash input schema is an object")
    }

    pub fn description(&self) -> String {
        let allow_background = self.allow_background();
        let shell_name = self
            .environment
            .inspect()
            .ok()
            .and_then(|environment| environment.host())
            .map(|host| host.shell_name)
            .unwrap_or_else(|| "sh".to_string());
        let timeout_guidance = if !allow_background {
            TIMEOUT_KILLS
        } else if self.auto_background_on_timeout() {
            TIMEOUT_MOVES_TO_BACKGROUND
        } else {
            TIMEOUT_KILLS_WITH_BACKGROUND
        };
        render_prompt(
            BASH_DESCRIPTION_TEMPLATE,
            &HashMap::from([
                ("SHELL_NAME".to_string(), json!(shell_name)),
                ("TIMEOUT_GUIDANCE".to_string(), json!(timeout_guidance)),
                (
                    "BACKGROUND_GUIDANCE".to_string(),
                    json!(if allow_background {
                        BACKGROUND_GUIDANCE
                    } else {
                        NO_BACKGROUND_GUIDANCE
                    }),
                ),
            ]),
        )
    }

    pub fn resolve_execution_value(&self, input: Value) -> ToolExecution {
        match serde_json::from_value::<BashInput>(input) {
            Ok(args) => match validate_bash_input(&args) {
                Ok(()) => self.resolve_execution(args),
                Err(error) => ToolExecution::Error(ExecutableToolResult::error(error)),
            },
            Err(error) => ToolExecution::Error(ExecutableToolResult::error(format!(
                "Invalid arguments for Bash: {error}"
            ))),
        }
    }

    pub fn resolve_execution(&self, args: BashInput) -> ToolExecution {
        let expected_generation = self.reviewed_generation();
        let preview = if args.command.encode_utf16().count() > 50 {
            format!("{}…", utf16_prefix(&args.command, 50))
        } else {
            args.command.clone()
        };
        let description = if args.run_in_background == Some(true) {
            format!("Starting background: {preview}")
        } else {
            format!("Running: {preview}")
        };
        let rule_command = args.command.clone();
        let this = self.clone();
        ToolExecution::Runnable(RunnableToolExecution {
            accesses: None,
            display: Some(ToolInputDisplay::Command {
                command: args.command.clone(),
                cwd: Some(self.execution_cwd(&args)),
                description: args.description.clone(),
                language: Some(CommandLanguage::Bash),
            }),
            description: Some(description),
            stop_batch_after_this: None,
            approval_rule: literal_rule_pattern(self.name(), &args.command),
            matches_rule: Some(Arc::new(move |rule_args| {
                matches_glob_rule_subject(rule_args, &rule_command)
            })),
            execute: Arc::new(move |context| {
                let this = this.clone();
                let args = args.clone();
                let expected_generation = expected_generation.clone();
                Box::pin(async move {
                    this.execution(args, context, expected_generation).await
                })
            }),
        })
    }

    fn allow_background(&self) -> bool {
        self.flags
            .as_ref()
            .is_none_or(|flags| !flags.enabled(OPERATION_LOG_FLAG_ID))
            && self.tool_policy.is_tool_active("TaskList", None)
            && self.tool_policy.is_tool_active("TaskOutput", None)
            && self.tool_policy.is_tool_active("TaskStop", None)
    }

    fn auto_background_on_timeout(&self) -> bool {
        if self
            .flags
            .as_ref()
            .is_some_and(|flags| flags.enabled(OPERATION_LOG_FLAG_ID))
        {
            return false;
        }
        resolve_agent_task_config(self.config.as_ref())
            .and_then(|config| config.bash_auto_background_on_timeout)
            .unwrap_or(true)
    }

    fn detach_timeout_ms(&self) -> u64 {
        resolve_agent_task_config(self.config.as_ref())
            .and_then(|config| config.bash_task_timeout_s)
            .unwrap_or(DEFAULT_BACKGROUND_TIMEOUT_S)
            * MS_PER_SECOND
    }

    fn results(&self) -> ShellToolResults {
        let tasks = ShellTaskServiceAdapter {
            tasks: self.tasks.clone(),
        };
        let this = self.clone();
        ShellToolResults::new(
            Arc::new(tasks),
            Arc::new(move || this.allow_background()),
        )
    }

    fn reviewed_generation(&self) -> Option<String> {
        self.environment
            .inspect()
            .ok()
            .and_then(|environment| pinned_generation(environment.as_ref()))
    }

    fn execution_cwd(&self, args: &BashInput) -> String {
        let Ok(environment) = self.environment.inspect() else {
            return args
                .cwd
                .clone()
                .unwrap_or_else(|| self.ctx.cwd());
        };
        let view = EnvironmentWorkspaceView::new(
            environment,
            EnvironmentWorkspaceRoots {
                work_dir: self.workspace_ctx.work_dir(),
                additional_dirs: Some(self.workspace_ctx.additional_dirs()),
                ..Default::default()
            },
        );
        view.resolve(args.cwd.as_deref().unwrap_or(&view.work_dir), None)
    }

    async fn spawn(
        &self,
        process_service: Arc<dyn IHostProcessService>,
        view: &EnvironmentWorkspaceView,
        cwd: Option<&str>,
        command: &str,
    ) -> Result<Arc<dyn IHostProcess>, WireAsyncError> {
        let env = &view.host;
        let effective_cwd = view.resolve(cwd.unwrap_or(&view.work_dir), None);
        let shell_cwd = view
            .shell_path_bridge
            .as_ref()
            .map(|bridge| bridge.to_shell_path(&effective_cwd))
            .unwrap_or_else(|| effective_cwd.clone());
        let shell_command = format!("cd {} && {command}", shell_quote(&shell_cwd));
        let noninteractive_env = HashMap::from([
            ("NO_COLOR".to_string(), "1".to_string()),
            ("TERM".to_string(), "dumb".to_string()),
            (
                "GIT_TERMINAL_PROMPT".to_string(),
                std::env::var("GIT_TERMINAL_PROMPT").unwrap_or_else(|_| "0".to_string()),
            ),
            ("SHELL".to_string(), env.shell_path.clone()),
        ]);
        process_service
            .spawn(
                &env.shell_path,
                &["-c".to_string(), shell_command],
                Some(&effective_cwd),
                Some(&noninteractive_env),
            )
            .await
    }

    async fn execution(
        &self,
        args: BashInput,
        context: ExecutableToolContext,
        expected_generation: Option<String>,
    ) -> Result<ExecutableToolResult, WireAsyncError> {
        let results = self.results();
        let validation_args = crate::agent::tools::os::shell::shell_tool_results::ShellToolCommandInput {
            command: args.command.clone(),
            run_in_background: args.run_in_background,
            description: args.description.clone(),
        };
        if let Some(error) = results.validate_run_request(&validation_args, &context.signal) {
            return Ok(from_legacy_result(error));
        }
        let starts_in_background = args.run_in_background == Some(true);
        let foreground_timeout_ms = normalize_timeout_ms(args.timeout, false);
        let lease = acquire_or_when_ready(self.environment.as_ref(), &["process"]).await?;
        if expected_generation.as_ref().is_some_and(|expected| {
            lease.environment.identity().generation != *expected
        }) {
            lease.dispose();
            return Ok(ExecutableToolResult::error(
                "Environment changed before execution. Retry the tool call.",
            ));
        }
        let view = EnvironmentWorkspaceView::new(
            lease.environment.clone(),
            EnvironmentWorkspaceRoots {
                work_dir: self.workspace_ctx.work_dir(),
                additional_dirs: Some(self.workspace_ctx.additional_dirs()),
                ..Default::default()
            },
        );
        let command = if view.host.os_kind == "Windows" {
            rewrite_windows_null_redirect(&args.command)
        } else {
            args.command.clone()
        };
        let description = if starts_in_background {
            args.description
                .as_deref()
                .unwrap_or_default()
                .trim()
                .to_string()
        } else {
            foreground_description("Bash", &args.command, args.description.as_deref())
        };
        let timeout_ms = if starts_in_background {
            if args.disable_timeout == Some(true) {
                None
            } else {
                Some(normalize_timeout_ms(args.timeout, true))
            }
        } else {
            Some(foreground_timeout_ms)
        };
        let proc = match self
            .spawn(
                lease.environment.process(),
                &view,
                args.cwd.as_deref(),
                &command,
            )
            .await
        {
            Ok(proc) => proc,
            Err(error) => {
                lease.dispose();
                return Ok(ExecutableToolResult::error(error.to_string()));
            }
        };
        if let Err(error) = lease.track(
            Arc::new(TrackedProcess(proc.clone())),
            Some(self.ctx.session_id()),
        ) {
            lease.dispose();
            return Ok(ExecutableToolResult::error(error.to_string()));
        }
        if let Err(error) = proc.close_stdin().await {
            on_unexpected_error(error.to_string());
        }
        let lease = Arc::new(Mutex::new(Some(lease)));
        let collect_foreground_output = Arc::new(AtomicBool::new(!starts_in_background));
        let foreground_output_persisted = Arc::new(AtomicBool::new(false));
        let foreground_task_id = Arc::new(Mutex::new(None::<String>));
        let builder = Arc::new(Mutex::new(ToolOutputAccumulator::new()));
        let on_process_output = if starts_in_background {
            None
        } else {
            let collect = collect_foreground_output.clone();
            let persisted = foreground_output_persisted.clone();
            let task_id = foreground_task_id.clone();
            let builder = builder.clone();
            let on_update = context.on_update.clone();
            let tasks = self.tasks.clone();
            Some(Arc::new(
                move |kind: ProcessTaskOutputKind, text: &str| {
                    if !collect.load(Ordering::SeqCst) {
                        return;
                    }
                    if let Some(on_update) = on_update.as_ref() {
                        on_update(ToolUpdate::text(kind.as_str(), text));
                    }
                    let mut builder = builder.lock().unwrap_or_else(|error| error.into_inner());
                    builder.write(text);
                    if !persisted.load(Ordering::SeqCst)
                        && builder.total_chars() > DEFAULT_TOOL_RESULT_MAX_CHARS
                    {
                        let task_id = task_id
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .clone();
                        if let Some(task_id) = task_id {
                            tasks.persist_output(&task_id);
                            persisted.store(true, Ordering::SeqCst);
                        }
                    }
                },
            ) as ProcessTaskOutputCallback)
        };
        let release_lease = lease.clone();
        let process_task = ProcessTask::new(
            proc.clone(),
            command,
            description.clone(),
            on_process_output,
            Some(Box::new(move || {
                if let Some(lease) = release_lease
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take()
                {
                    lease.dispose();
                }
            })),
            Some(context.tool_call_id.clone()),
        );
        let foreground_signal = if starts_in_background {
            None
        } else {
            Some(AbortTokenBridge::new(context.signal.clone()))
        };
        let registration = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.tasks.register_task(
                Arc::new(process_task),
                Some(RegisterAgentTaskOptions {
                    detached: Some(starts_in_background),
                    detachable: Some(
                        self.flags
                            .as_ref()
                            .is_none_or(|flags| !flags.enabled(OPERATION_LOG_FLAG_ID)),
                    ),
                    timeout_ms,
                    detach_timeout_ms: Some(self.detach_timeout_ms()),
                    auto_background_on_timeout: Some(
                        self.allow_background() && self.auto_background_on_timeout(),
                    ),
                    signal: foreground_signal.as_ref().map(AbortTokenBridge::token),
                }),
            )
        }));
        let task_id = match registration {
            Ok(task_id) => task_id,
            Err(payload) => {
                collect_foreground_output.store(false, Ordering::SeqCst);
                kill_spawned_process(proc).await;
                if let Some(lease) = lease
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take()
                {
                    lease.dispose();
                }
                return Ok(ExecutableToolResult::error(panic_payload_message(payload)));
            }
        };
        if !starts_in_background {
            *foreground_task_id
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(task_id.clone());
            if let Some(on_start) = context.on_foreground_task_start.as_ref() {
                on_start(task_id.clone());
            }
        }
        if starts_in_background {
            return Ok(from_legacy_result(results.background_started_result(
                &task_id,
                proc,
                &description,
                &ShellResultLabels {
                    title: "Background task started".to_string(),
                    brief: format!("Started {task_id}"),
                },
                None,
                BackgroundStartScenario::BackgroundStarted,
            )));
        }
        let release = match self.tasks.wait_for_foreground_release(&task_id).await {
            Ok(release) => release,
            Err(error) => {
                collect_foreground_output.store(false, Ordering::SeqCst);
                return Err(error);
            }
        };
        if matches!(
            release,
            Some(ForegroundTaskReleaseReason::Detached)
                | Some(ForegroundTaskReleaseReason::TimeoutDetached)
        ) {
            collect_foreground_output.store(false, Ordering::SeqCst);
            let timed_out = release == Some(ForegroundTaskReleaseReason::TimeoutDetached);
            let labels = if timed_out {
                ShellResultLabels {
                    title: "Command timed out and moved to background".to_string(),
                    brief: format!("Backgrounded {task_id} after timeout"),
                }
            } else {
                ShellResultLabels {
                    title: "Task moved to background by the user".to_string(),
                    brief: format!("Backgrounded {task_id} by the user"),
                }
            };
            let builder = builder.lock().unwrap_or_else(|error| error.into_inner());
            return Ok(from_legacy_result(results.background_started_result(
                &task_id,
                proc,
                &description,
                &labels,
                Some(&builder),
                if timed_out {
                    BackgroundStartScenario::ForegroundDetached
                } else {
                    BackgroundStartScenario::ForegroundDetachedByUser
                },
            )));
        }
        collect_foreground_output.store(false, Ordering::SeqCst);
        let mut owned_builder = std::mem::take(
            &mut *builder.lock().unwrap_or_else(|error| error.into_inner()),
        );
        Ok(from_legacy_result(
            results
                .foreground_completion_result(
                    &task_id,
                    proc,
                    &mut owned_builder,
                    foreground_timeout_ms,
                )
                .await,
        ))
    }
}

impl IBashTool for BashTool {}

struct TrackedProcess(Arc<dyn IHostProcess>);

impl EnvironmentResource for TrackedProcess {
    fn dispose(&self) -> BoxFuture<'static, ()> {
        let proc = self.0.clone();
        Box::pin(async move { proc.dispose().await })
    }
}

struct ShellTaskServiceAdapter {
    tasks: Arc<dyn IAgentTaskService>,
}

impl ShellTaskService for ShellTaskServiceAdapter {
    fn get_task(&self, task_id: &str) -> Option<Value> {
        self.tasks
            .get_task(task_id)
            .and_then(|task| serde_json::to_value(task).ok())
    }

    fn get_output_snapshot(&self, task_id: &str, offset: u64) -> BoxFuture<'static, Value> {
        let tasks = self.tasks.clone();
        let task_id = task_id.to_string();
        Box::pin(async move {
            tasks
                .get_output_snapshot(&task_id, offset as i64)
                .await
                .ok()
                .and_then(|snapshot| serde_json::to_value(snapshot).ok())
                .unwrap_or(Value::Null)
        })
    }
}

struct ProcessTaskSinkAdapter {
    signal: ParentCancel,
    sink: AgentTaskSink,
}

impl LegacyAgentTaskSink for ProcessTaskSinkAdapter {
    fn signal(&self) -> ParentCancel {
        self.signal.clone()
    }

    fn append_output(&self, text: &str) {
        self.sink.append_output(text.to_string());
    }

    fn settle(&self, settlement: LegacyAgentTaskSettlement) -> BoxFuture<'static, ()> {
        let status = match settlement.status {
            TaskStatus::Completed => AgentTaskSettlementStatus::Completed,
            TaskStatus::Killed => AgentTaskSettlementStatus::Killed,
            TaskStatus::TimedOut => AgentTaskSettlementStatus::TimedOut,
            TaskStatus::Failed | TaskStatus::Running | TaskStatus::Lost => {
                AgentTaskSettlementStatus::Failed
            }
        };
        let settled = self.sink.settle(NewAgentTaskSettlement {
            status,
            stop_reason: settlement.stop_reason,
        });
        Box::pin(async move {
            settled.await;
        })
    }
}

#[async_trait]
impl AgentTask for ProcessTask {
    fn id_prefix(&self) -> &str {
        ProcessTask::id_prefix(self)
    }

    fn kind(&self) -> AgentTaskKind {
        AgentTaskKind::Process
    }

    fn description(&self) -> &str {
        &self.description
    }

    async fn start(&self, sink: AgentTaskSink) -> Result<(), WireAsyncError> {
        let parent = ParentCancel::new();
        let bridge_parent = parent.clone();
        let signal = sink.signal.clone();
        let bridge = tokio::spawn(async move {
            let wait_parent = bridge_parent.clone();
            if abortable(async move { wait_parent.wait().await }, signal)
                .await
                .is_err()
            {
                bridge_parent.trigger();
            }
        });
        let adapter = Arc::new(ProcessTaskSinkAdapter {
            signal: parent.clone(),
            sink,
        });
        ProcessTask::start(self, adapter).await;
        parent.trigger();
        let _ = bridge.await;
        Ok(())
    }

    async fn force_stop(&self) -> Result<(), WireAsyncError> {
        ProcessTask::force_stop(self).await
    }

    fn to_info(&self, base: AgentTaskInfoBase) -> AgentTaskInfo {
        AgentTaskInfo::Process(AgentProcessTaskInfo {
            base,
            command: self.command.clone(),
            pid: self.proc.pid(),
            exit_code: self.exit_code(),
            parent_tool_call_id: self.parent_tool_call_id.clone(),
        })
    }
}

struct AbortTokenBridge {
    token: tokio_util::sync::CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl AbortTokenBridge {
    fn new(signal: AbortSignal) -> Self {
        let token = tokio_util::sync::CancellationToken::new();
        let task_token = token.clone();
        let task = tokio::spawn(async move {
            let cancelled = task_token.clone().cancelled_owned();
            if abortable(cancelled, signal).await.is_err() {
                task_token.cancel();
            }
        });
        Self { token, task }
    }

    fn token(&self) -> tokio_util::sync::CancellationToken {
        self.token.clone()
    }
}

impl Drop for AbortTokenBridge {
    fn drop(&mut self) {
        self.token.cancel();
        self.task.abort();
    }
}

fn from_legacy_result(result: crate::turn_loop::types::ExecutableToolResult) -> ExecutableToolResult {
    ExecutableToolResult {
        output: result.content.into(),
        is_error: Some(result.is_error),
        stop_turn: Some(result.stop_turn),
        note: result.note,
        delivery: result.delivery,
        spill: result.spill,
        ..Default::default()
    }
}

fn panic_payload_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(error) = payload.downcast_ref::<WireAsyncError>() {
        return error.to_string();
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    if let Some(message) = payload.downcast_ref::<&str>() {
        return (*message).to_string();
    }
    "Background task registration failed".to_string()
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn utf16_prefix(value: &str, max_units: usize) -> &str {
    if value.encode_utf16().count() <= max_units {
        return value;
    }
    let mut units = 0;
    let mut end = 0;
    for (index, character) in value.char_indices() {
        let next = units + character.len_utf16();
        if next > max_units {
            break;
        }
        units = next;
        end = index + character.len_utf8();
    }
    &value[..end]
}

const NUL_TARGET_BOUNDARY: fn(char) -> bool =
    |ch: char| matches!(ch, ' ' | '\t' | '\n' | '\r' | '|' | '&' | ';' | ')');

struct HeredocDelimiter {
    delimiter: String,
    start: usize,
}

pub fn rewrite_windows_null_redirect(command: &str) -> String {
    rewrite_nul_redirects(&command.chars().collect::<Vec<_>>())
}

fn is_nul_redirect_at(chars: &[char], at: usize) -> bool {
    let Some(word) = chars.get(at..at + 3) else {
        return false;
    };
    if !word.iter().collect::<String>().eq_ignore_ascii_case("nul") {
        return false;
    }
    chars.get(at + 3).is_none_or(|ch| NUL_TARGET_BOUNDARY(*ch))
}

fn nul_redirect_word_start(chars: &[char], at: usize) -> Option<usize> {
    let mut cursor = at + 1;
    if chars.get(cursor) == Some(&'>') {
        cursor += 1;
    }
    while matches!(chars.get(cursor), Some(' ') | Some('\t')) {
        cursor += 1;
    }
    is_nul_redirect_at(chars, cursor).then_some(cursor)
}

fn read_heredoc_delimiter(chars: &[char], at: usize) -> Option<HeredocDelimiter> {
    let mut cursor = at + 2;
    if chars.get(cursor) == Some(&'-') {
        cursor += 1;
    }
    while matches!(chars.get(cursor), Some(' ') | Some('\t')) {
        cursor += 1;
    }
    let quote = *chars.get(cursor)?;
    if quote == '\'' || quote == '"' {
        let mut close = cursor + 1;
        while close < chars.len() && chars[close] != quote {
            close += 1;
        }
        if close >= chars.len() {
            return None;
        }
        let delimiter: String = chars[cursor + 1..close].iter().collect();
        if delimiter.is_empty() {
            return None;
        }
        return Some(HeredocDelimiter {
            delimiter,
            start: at,
        });
    }
    let word_start = cursor;
    while let Some(&ch) = chars.get(cursor) {
        if matches!(
            ch,
            ' ' | '\t' | '\n' | '\r' | '|' | '&' | ';' | '(' | ')' | '<' | '>'
        ) {
            break;
        }
        cursor += 1;
    }
    if cursor == word_start {
        return None;
    }
    Some(HeredocDelimiter {
        delimiter: chars[word_start..cursor].iter().collect(),
        start: at,
    })
}

fn end_of_line_including_newline(chars: &[char], at: usize) -> usize {
    chars
        .get(at..)
        .and_then(|tail| {
            tail.iter()
                .position(|ch| *ch == '\n')
                .map(|offset| at + offset + 1)
        })
        .unwrap_or(chars.len())
}

fn heredoc_body_end(chars: &[char], at: usize, delimiter: &str) -> usize {
    let mut cursor = at;
    while cursor < chars.len() {
        let line_end = end_of_line_including_newline(chars, cursor);
        let line: String = chars[cursor..line_end].iter().collect();
        cursor = line_end;
        let trimmed = line.trim();
        if trimmed == delimiter || trimmed == format!("{delimiter};") {
            return cursor;
        }
    }
    chars.len()
}

fn read_substitution_body(chars: &[char], at: usize) -> (String, usize) {
    let mut depth = 1usize;
    let mut cursor = at;
    while cursor < chars.len() {
        let ch = chars[cursor];
        if ch == '\\' {
            cursor += 2;
            continue;
        }
        if ch == '\'' || ch == '"' {
            match chars
                .get(cursor + 1..)
                .and_then(|tail| tail.iter().position(|c| *c == ch))
            {
                Some(close) => cursor += close + 2,
                None => return (chars[at..].iter().collect(), chars.len()),
            }
            continue;
        }
        if ch == '(' {
            depth += 1;
        }
        if ch == ')' {
            depth -= 1;
            if depth == 0 {
                return (chars[at..cursor].iter().collect(), cursor + 1);
            }
        }
        cursor += 1;
    }
    (chars[at..].iter().collect(), chars.len())
}

fn read_backtick_body(chars: &[char], at: usize) -> (String, usize) {
    let mut cursor = at;
    while cursor < chars.len() {
        if chars[cursor] == '\\' {
            cursor += 2;
            continue;
        }
        if chars[cursor] == '`' {
            return (chars[at..cursor].iter().collect(), cursor + 1);
        }
        cursor += 1;
    }
    (chars[at..].iter().collect(), chars.len())
}

fn slice(chars: &[char], from: usize, to: usize) -> String {
    chars
        .get(from..to.min(chars.len()))
        .unwrap_or(&[])
        .iter()
        .collect()
}

fn rewrite_nul_redirects(chars: &[char]) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut plain = String::new();
    let mut cursor = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    let flush = |out: &mut Vec<String>, plain: &mut String| {
        if !plain.is_empty() {
            out.push(std::mem::take(plain));
        }
    };
    while cursor < chars.len() {
        let ch = chars[cursor];
        if in_single {
            plain.push(ch);
            if ch == '\'' {
                in_single = false;
            }
            cursor += 1;
            continue;
        }
        if ch == '\\' {
            plain.push('\\');
            if let Some(&next) = chars.get(cursor + 1) {
                plain.push(next);
                cursor += 2;
            } else {
                cursor += 1;
            }
            continue;
        }
        if in_double {
            if ch == '"' {
                plain.push('"');
                in_double = false;
                cursor += 1;
                continue;
            }
            if ch == '$' && chars.get(cursor + 1) == Some(&'(') {
                let (body, end) = read_substitution_body(chars, cursor + 2);
                flush(&mut out, &mut plain);
                out.push("$(".to_string());
                out.push(rewrite_nul_redirects(&body.chars().collect::<Vec<_>>()));
                out.push(")".to_string());
                cursor = end;
                continue;
            }
            if ch == '`' {
                let (body, end) = read_backtick_body(chars, cursor + 1);
                flush(&mut out, &mut plain);
                out.push("`".to_string());
                out.push(rewrite_nul_redirects(&body.chars().collect::<Vec<_>>()));
                out.push("`".to_string());
                cursor = end;
                continue;
            }
            plain.push(ch);
            cursor += 1;
            continue;
        }
        if ch == '\'' || ch == '"' {
            flush(&mut out, &mut plain);
            in_single = ch == '\'';
            in_double = ch == '"';
            out.push(ch.to_string());
            cursor += 1;
            continue;
        }
        if ch == '`' {
            let (body, end) = read_backtick_body(chars, cursor + 1);
            flush(&mut out, &mut plain);
            out.push("`".to_string());
            out.push(rewrite_nul_redirects(&body.chars().collect::<Vec<_>>()));
            out.push("`".to_string());
            cursor = end;
            continue;
        }
        if ch == '$' && chars.get(cursor + 1) == Some(&'(') {
            let (body, end) = read_substitution_body(chars, cursor + 2);
            flush(&mut out, &mut plain);
            out.push("$(".to_string());
            out.push(rewrite_nul_redirects(&body.chars().collect::<Vec<_>>()));
            out.push(")".to_string());
            cursor = end;
            continue;
        }
        if ch == '#' {
            let previous = plain.chars().last();
            let starts_comment = previous.is_none()
                || previous
                    .is_some_and(|c| matches!(c, ' ' | '\t' | '\n' | '\r' | ';' | '|' | '&' | '('));
            if starts_comment {
                let line_end = end_of_line_including_newline(chars, cursor);
                plain.push_str(&slice(chars, cursor, line_end));
                cursor = line_end;
                continue;
            }
        }
        if ch == '<' && chars.get(cursor + 1) == Some(&'<') && chars.get(cursor + 2) != Some(&'<') {
            if let Some(heredoc) = read_heredoc_delimiter(chars, cursor) {
                let line_end = end_of_line_including_newline(chars, heredoc.start);
                let body_end = heredoc_body_end(chars, line_end, &heredoc.delimiter);
                plain.push_str(&slice(chars, cursor, line_end));
                flush(&mut out, &mut plain);
                out.push(slice(chars, line_end, body_end));
                cursor = body_end;
                continue;
            }
            plain.push_str("<<");
            cursor += 2;
            continue;
        }
        if ch == '>'
            && let Some(nul_start) = nul_redirect_word_start(chars, cursor)
        {
            plain.push_str(&slice(chars, cursor, nul_start));
            plain.push_str("/dev/null");
            cursor = nul_start + 3;
            continue;
        }
        plain.push(ch);
        cursor += 1;
    }
    flush(&mut out, &mut plain);
    out.concat()
}
