use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};

use super::powershell::{
    IPowerShellTool, PowerShellExecutable, PowerShellExecutableKind, PowerShellInput,
    build_power_shell_args, normalize_power_shell_output, validate_power_shell_input,
};
use crate::_base::utils::abort::{AbortSignal, abortable};
use crate::_base::utils::render_prompt::render_prompt;
use crate::agent::environment_binding::agent_environment::{
    IAgentEnvironmentService, acquire_or_when_ready,
};
use crate::agent::task::config_section::resolve_agent_task_config;
use crate::agent::task::task::{
    ForegroundTaskReleaseReason, IAgentTaskService, RegisterAgentTaskOptions,
};
use crate::agent::tool_executor::tool_executor::{
    ExecutableToolContext, ExecutableToolResult, RunnableToolExecution, ToolExecution,
};
use crate::agent::tool_executor::tool_executor_events::ToolUpdate;
use crate::agent::tool_policy::tool_policy::IAgentToolPolicyService;
use crate::agent::tools::os::bash::process_task::{
    ProcessTask, ProcessTaskOutputCallback, ProcessTaskOutputKind,
};
use crate::agent::tools::os::shell::shell_tool_results::{
    BackgroundStartScenario, DEFAULT_BACKGROUND_TIMEOUT_S, DEFAULT_TIMEOUT_S,
    MAX_BACKGROUND_TIMEOUT_S, MAX_TIMEOUT_S, MS_PER_SECOND, ShellResultLabels, ShellTaskService,
    ShellToolResults, foreground_description, kill_spawned_process, normalize_timeout_ms,
};
use crate::app::config::config::IConfigService;
use crate::app::flag::flag::IFlagService;
use crate::environment::environment::{
    EnvironmentResource, EnvironmentWorkspaceRoots,
};
use crate::environment::environment_workspace_view::EnvironmentWorkspaceView;
use crate::features::operationLog::flag::OPERATION_LOG_FLAG_ID;
use crate::native::environment_probe::ProbeShellNotFound;
use crate::native::shell_payload::{
    ShellPayloadKind, find_shell_payload_pin, resolve_shell_payload_home, shell_payload_exe_path,
};
use crate::os::interface::host_process::{IHostProcess, IHostProcessService};
use crate::session::session_context::session_context::ISessionContext;
use crate::session::workspace_context::workspace_context::ISessionWorkspaceContext;
use crate::tool::input_schema::to_input_json_schema;
use crate::tool::output_accumulator::ToolOutputAccumulator;
use crate::tool::rule_match::{literal_rule_pattern, matches_glob_rule_subject};
use crate::tool::tool_contract::DEFAULT_TOOL_RESULT_MAX_CHARS;
use crate::tool::tool_input_display::ToolInputDisplay;
use crate::wire::record::WireAsyncError;

const DESCRIPTION_TEMPLATE: &str = r#"Execute a Windows PowerShell script natively (PowerShell 7 `pwsh` when available, otherwise Windows PowerShell 5.1). The script runs as a direct PowerShell process — it is not routed through bash.

**When to use this instead of Bash:**

- The task is inherently PowerShell: Windows registry, services, scheduled tasks, winget, WinGet/MSI installs, COM/WinRM, Windows event logs, NTFS ACLs, certificate store.
- You need PowerShell cmdlets or .NET types (`Get-Service`, `Get-ItemProperty`, `[System.*]`).
- You are on Windows and about to run `powershell.exe` / `pwsh` from the Bash tool — do not: the bash→native-PowerShell bridge re-encodes output and corrupts non-ASCII text. Use this tool instead; output is always decoded as UTF-8.

**When to keep using Bash:** anything with POSIX shell semantics — pipes over coreutils, git, package managers of the project, build/test runners. File reads/edits/searches belong to `Read` / `Edit` / `Glob` / `Grep` regardless of platform.

**Behavior:**

- The script is passed as a single `-EncodedCommand` payload, so quoting and encoding are handled for you; write ordinary PowerShell script text. Interactive input is not supported.
- The session runs with `-NoProfile -NonInteractive`. Output encoding is pinned to UTF-8, so non-ASCII text (including Chinese) comes back clean.
- stdout and stderr are combined and returned as a string. The output may be truncated if it is too long. A non-zero exit ends the result with a `Command failed with exit code: N` line; a script killed by its timeout or interrupted by the user ends with its own message instead.
- Each call runs in a fresh process: variables and location do not persist between calls. To run in a particular directory, pass the `cwd` argument.
- Foreground calls default to ${DEFAULT_TIMEOUT_S}s and allow up to ${MAX_TIMEOUT_S}s; a foreground call that hits its timeout is moved to the background instead of being killed.

If `run_in_background=true`, the script will be started as a background task and this tool will return a task ID instead of waiting for completion. When doing that, you must provide a short `description`. Background commands default to ${DEFAULT_BACKGROUND_TIMEOUT_S}s timeout and `timeout` is capped at ${MAX_BACKGROUND_TIMEOUT_S}s; set `disable_timeout=true` only when the task should run without a timeout. You will be automatically notified when the task completes. After starting one, default to returning control to the user instead of immediately waiting on it. Use `TaskOutput` only for a non-blocking status/output snapshot — do not wait on a task you just launched. Use `TaskStop` only if the task must be cancelled. If a human user wants to inspect background tasks themselves, point them to the background-task panel.

**Safety:** do not use this tool to escape the working directory, to modify files outside of it unless instructed, or to run commands that require superuser privileges unless explicitly instructed.
"#;

static PARAMETERS: std::sync::LazyLock<Value> = std::sync::LazyLock::new(|| {
    to_input_json_schema(json!({
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "minLength": 1,
                "description": "The PowerShell script to execute."
            },
            "cwd": {
                "type": "string",
                "description": "The working directory in which to run the script. When omitted, the script runs in the session's working directory."
            },
            "timeout": {
                "type": "integer",
                "exclusiveMinimum": 0,
                "maximum": 9007199254740991i64,
                "description": "Optional timeout in seconds for the script to execute."
            },
            "description": {
                "type": "string",
                "description": "A short description for the background task. Required when run_in_background is true."
            },
            "run_in_background": {
                "type": "boolean",
                "description": "Whether to run the script as a background task."
            },
            "disable_timeout": {
                "type": "boolean",
                "description": "If true, do not apply a timeout to the script. Only applies when run_in_background is true."
            }
        },
        "required": ["command"]
    }))
});

pub type PowerShellIsFile = Arc<dyn Fn(String) -> BoxFuture<'static, bool> + Send + Sync>;
pub type PowerShellExecutableResolver = Arc<
    dyn Fn(PowerShellResolveDeps) -> BoxFuture<'static, Result<PowerShellExecutable, WireAsyncError>>
        + Send
        + Sync,
>;

#[derive(Clone)]
pub struct PowerShellResolveDeps {
    pub env: HashMap<String, String>,
    pub os_home_dir: String,
    pub is_file: PowerShellIsFile,
}

#[derive(Clone, Default)]
pub struct PowerShellToolOverrides {
    pub resolve_executable: Option<PowerShellExecutableResolver>,
}

#[derive(Clone)]
pub struct PowerShellTool {
    environment: Arc<dyn IAgentEnvironmentService>,
    ctx: Arc<dyn ISessionContext>,
    workspace_ctx: Arc<dyn ISessionWorkspaceContext>,
    tasks: Arc<dyn IAgentTaskService>,
    tool_policy: Arc<dyn IAgentToolPolicyService>,
    config: Arc<dyn IConfigService>,
    overrides: PowerShellToolOverrides,
    flags: Option<Arc<dyn IFlagService>>,
}

impl PowerShellTool {
    pub fn new(
        environment: Arc<dyn IAgentEnvironmentService>,
        ctx: Arc<dyn ISessionContext>,
        workspace_ctx: Arc<dyn ISessionWorkspaceContext>,
        tasks: Arc<dyn IAgentTaskService>,
        tool_policy: Arc<dyn IAgentToolPolicyService>,
        config: Arc<dyn IConfigService>,
        overrides: PowerShellToolOverrides,
        flags: Option<Arc<dyn IFlagService>>,
    ) -> Self {
        Self {
            environment,
            ctx,
            workspace_ctx,
            tasks,
            tool_policy,
            config,
            overrides,
            flags,
        }
    }

    pub fn name(&self) -> &'static str {
        "PowerShell"
    }

    pub fn parameters(&self) -> Map<String, Value> {
        PARAMETERS
            .as_object()
            .cloned()
            .expect("PowerShell input schema is an object")
    }
    pub fn side_effect(&self) -> &'static str {
        "shell"
    }

    pub fn description(&self) -> String {
        let rendered = render_power_shell_description();
        if !self.allow_background() {
            return without_background_description(&rendered);
        }
        if !self.auto_background_on_timeout() {
            return without_auto_background_on_timeout(&rendered);
        }
        rendered
    }

    pub fn resolve_execution_value(&self, input: Value) -> ToolExecution {
        match serde_json::from_value::<PowerShellInput>(input) {
            Ok(args) => match validate_power_shell_input(&args) {
                Ok(()) => self.resolve_execution(args),
                Err(error) => ToolExecution::Error(ExecutableToolResult::error(error)),
            },
            Err(error) => ToolExecution::Error(ExecutableToolResult::error(format!(
                "Invalid arguments for PowerShell: {error}"
            ))),
        }
    }

    pub fn resolve_execution(&self, args: PowerShellInput) -> ToolExecution {
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
                cwd: Some(args.cwd.clone().unwrap_or_else(|| self.ctx.cwd())),
                description: args.description.clone(),
                language: None,
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
                Box::pin(async move { this.execution(args, context).await })
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

    fn resolve_executable_deps(&self) -> PowerShellResolveDeps {
        PowerShellResolveDeps {
            env: std::env::vars().collect(),
            os_home_dir: crate::default_workspace::user_home_dir()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            is_file: Arc::new(|path| {
                Box::pin(async move { tokio::fs::metadata(path).await.is_ok() })
            }),
        }
    }

    async fn resolve_executable(&self) -> Result<PowerShellExecutable, WireAsyncError> {
        if let Some(resolver) = self.overrides.resolve_executable.as_ref() {
            return resolver(self.resolve_executable_deps()).await;
        }
        resolve_power_shell_executable(self.resolve_executable_deps()).await
    }

    async fn spawn(
        &self,
        process_service: Arc<dyn IHostProcessService>,
        effective_cwd: &str,
        script: &str,
        executable: &PowerShellExecutable,
    ) -> Result<Arc<dyn IHostProcess>, WireAsyncError> {
        process_service
            .spawn(
                &executable.path.to_string_lossy(),
                &build_power_shell_args(script),
                Some(effective_cwd),
                Some(&HashMap::from([
                    ("NO_COLOR".to_string(), "1".to_string()),
                    ("POWERSHELL_TELEMETRY_OPTOUT".to_string(), "1".to_string()),
                    ("POWERSHELL_UPDATECHECK_OPTOUT".to_string(), "1".to_string()),
                ])),
            )
            .await
    }

    async fn execution(
        &self,
        args: PowerShellInput,
        context: ExecutableToolContext,
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
        let view = EnvironmentWorkspaceView::new(
            lease.environment.clone(),
            EnvironmentWorkspaceRoots {
                work_dir: self.workspace_ctx.work_dir(),
                additional_dirs: Some(self.workspace_ctx.additional_dirs()),
                ..Default::default()
            },
        );
        let effective_cwd = view.resolve(args.cwd.as_deref().unwrap_or(&view.work_dir), None);
        let description = if starts_in_background {
            args.description
                .as_deref()
                .unwrap_or_default()
                .trim()
                .to_string()
        } else {
            foreground_description("PowerShell", &args.command, args.description.as_deref())
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
        let (proc, _executable) = match async {
            let executable = self.resolve_executable().await?;
            let proc = self
                .spawn(
                    lease.environment.process(),
                    &effective_cwd,
                    &args.command,
                    &executable,
                )
                .await?;
            Ok::<_, WireAsyncError>((proc, executable))
        }
        .await
        {
            Ok(result) => result,
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
                    let normalized = normalize_power_shell_output(text);
                    if let Some(on_update) = on_update.as_ref() {
                        on_update(ToolUpdate::text(kind.as_str(), &normalized));
                    }
                    let mut builder = builder.lock().unwrap_or_else(|error| error.into_inner());
                    builder.write(&normalized);
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
            args.command.clone(),
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

impl IPowerShellTool for PowerShellTool {}

pub fn render_power_shell_description() -> String {
    render_prompt(
        DESCRIPTION_TEMPLATE,
        &HashMap::from([
            ("DEFAULT_TIMEOUT_S".to_string(), json!(DEFAULT_TIMEOUT_S)),
            (
                "DEFAULT_BACKGROUND_TIMEOUT_S".to_string(),
                json!(DEFAULT_BACKGROUND_TIMEOUT_S),
            ),
            ("MAX_TIMEOUT_S".to_string(), json!(MAX_TIMEOUT_S)),
            (
                "MAX_BACKGROUND_TIMEOUT_S".to_string(),
                json!(MAX_BACKGROUND_TIMEOUT_S),
            ),
        ]),
    )
}

pub fn without_background_description(description: &str) -> String {
    static PATTERN: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r"\r?\n\r?\nIf `run_in_background=true`,[\s\S]*?point them to the background-task panel\.",
        )
        .expect("PowerShell background description pattern compiles")
    });
    PATTERN
        .replace(
            description,
            "\n\nBackground execution is disabled for this agent. Do not set `run_in_background=true`.",
        )
        .to_string()
}

pub fn without_auto_background_on_timeout(description: &str) -> String {
    description.replace(
        "a foreground call that hits its timeout is moved to the background instead of being killed.",
        "a foreground call that hits its timeout is killed.",
    )
}

pub async fn resolve_power_shell_executable(
    deps: PowerShellResolveDeps,
) -> Result<PowerShellExecutable, WireAsyncError> {
    let mut checked = Vec::new();
    if let Some(override_path) = deps
        .env
        .get("KIMI_PWSH_PATH")
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    {
        checked.push(override_path.to_string());
        if (deps.is_file)(override_path.to_string()).await {
            return Ok(PowerShellExecutable {
                path: PathBuf::from(override_path),
                kind: PowerShellExecutableKind::Pwsh,
            });
        }
    }
    if let Some(pin) = find_shell_payload_pin(ShellPayloadKind::Pwsh) {
        let home = resolve_shell_payload_home(
            deps.env.get("KIMI_CODE_HOME").map(String::as_str),
            Path::new(&deps.os_home_dir),
        );
        let path = shell_payload_exe_path(&home, pin);
        checked.push(path.to_string_lossy().into_owned());
        if (deps.is_file)(path.to_string_lossy().into_owned()).await {
            return Ok(PowerShellExecutable {
                path,
                kind: PowerShellExecutableKind::Pwsh,
            });
        }
    }
    for path in pwsh_path_candidates(&deps.env) {
        checked.push(path.to_string_lossy().into_owned());
        if (deps.is_file)(path.to_string_lossy().into_owned()).await {
            return Ok(PowerShellExecutable {
                path,
                kind: PowerShellExecutableKind::Pwsh,
            });
        }
    }
    for path in windows_power_shell_candidates(&deps.env) {
        checked.push(path.to_string_lossy().into_owned());
        if (deps.is_file)(path.to_string_lossy().into_owned()).await {
            return Ok(PowerShellExecutable {
                path,
                kind: PowerShellExecutableKind::WindowsPowerShell,
            });
        }
    }
    Err(Box::new(ProbeShellNotFound {
        message: "PowerShell was not found on this Windows host. Run `kimi shells provision --pwsh` to install the bundled pwsh payload, or install PowerShell 7 from https://github.com/PowerShell/PowerShell, or set KIMI_PWSH_PATH to a pwsh.exe.".to_string(),
        checked,
    }))
}

fn pwsh_path_candidates(env: &HashMap<String, String>) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    for directory in env.get("PATH").map(String::as_str).unwrap_or("").split(';') {
        let trimmed = directory.trim();
        if !trimmed.is_empty() {
            candidates.push(Path::new(trimmed).join("pwsh.exe"));
        }
    }
    candidates.push(PathBuf::from(r"C:\Program Files\PowerShell\7\pwsh.exe"));
    candidates.push(PathBuf::from(r"C:\Program Files\PowerShell\7-preview\pwsh.exe"));
    dedupe_win_paths(candidates)
}

fn windows_power_shell_candidates(env: &HashMap<String, String>) -> Vec<PathBuf> {
    let system_root = env
        .get("SystemRoot")
        .map(|value| value.trim())
        .unwrap_or(r"C:\Windows");
    vec![
        Path::new(system_root)
            .join("System32")
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe"),
    ]
}

fn dedupe_win_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    let mut output = Vec::new();
    for path in paths {
        if seen.insert(path.to_string_lossy().to_lowercase()) {
            output.push(path);
        }
    }
    output
}

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
