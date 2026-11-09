use super::*;
use crate::native::powershell::{
    POWER_SHELL_ENV, PowerShellExecutable, PowerShellNotFound, ResolveDeps, build_args,
    normalize_output, resolve,
};

pub const POWERSHELL_TOOL_NAME: &str = "PowerShell";

const DEFAULT_TIMEOUT_S: u64 = 60;
const MAX_TIMEOUT_S: u64 = 5 * 60;
const DEFAULT_BACKGROUND_TIMEOUT_S: u64 = 10 * 60;
const MAX_BACKGROUND_TIMEOUT_S: u64 = 24 * 60 * 60;

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

fn render_description() -> String {
    DESCRIPTION_TEMPLATE
        .replace("${DEFAULT_TIMEOUT_S}", &DEFAULT_TIMEOUT_S.to_string())
        .replace(
            "${DEFAULT_BACKGROUND_TIMEOUT_S}",
            &DEFAULT_BACKGROUND_TIMEOUT_S.to_string(),
        )
        .replace("${MAX_TIMEOUT_S}", &MAX_TIMEOUT_S.to_string())
        .replace(
            "${MAX_BACKGROUND_TIMEOUT_S}",
            &MAX_BACKGROUND_TIMEOUT_S.to_string(),
        )
}

pub fn tool_def() -> ToolInfo {
    ToolInfo {
        name: POWERSHELL_TOOL_NAME.into(),
        description: render_description(),
        input_schema: serde_json::json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
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
            "required": [
                "command"
            ],
            "additionalProperties": false
        }),
    }
}

fn normalize_timeout_s(timeout: Option<u64>, is_background: bool) -> u64 {
    let (default, cap) = if is_background {
        (DEFAULT_BACKGROUND_TIMEOUT_S, MAX_BACKGROUND_TIMEOUT_S)
    } else {
        (DEFAULT_TIMEOUT_S, MAX_TIMEOUT_S)
    };
    timeout.unwrap_or(default).min(cap)
}

fn background_timeout(args: &Value) -> Option<Duration> {
    if args
        .get("disable_timeout")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return None;
    }
    Some(Duration::from_secs(normalize_timeout_s(
        args.get("timeout").and_then(Value::as_u64),
        true,
    )))
}

pub(super) fn resolve_powershell() -> Result<PowerShellExecutable, PowerShellNotFound> {
    let pwsh_override = std::env::var("KIMI_PWSH_PATH").ok();
    let path_env = std::env::var("PATH").ok();
    let system_root = std::env::var("SystemRoot").ok();
    resolve(ResolveDeps {
        pwsh_override: pwsh_override.as_deref(),
        path_env: path_env.as_deref(),
        system_root: system_root.as_deref(),
        is_file: &|path| path.is_file(),
    })
}

fn spawn_powershell(
    exe: &PowerShellExecutable,
    script: &str,
    working_dir: &Path,
    agent_env: &std::collections::BTreeMap<String, String>,
) -> std::io::Result<tokio::process::Child> {
    let mut command = tokio::process::Command::new(&exe.path);
    command
        .args(build_args(script))
        .current_dir(working_dir)
        .envs(agent_env)
        .envs(POWER_SHELL_ENV.iter().map(|(name, value)| (*name, *value)))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    #[cfg(unix)]
    command.process_group(0);
    command.spawn()
}

struct BackgroundStreams {
    throttle: Arc<ProgressThrottle>,
    runner: Arc<crate::storage::TaskRunner>,
    task_id: String,
    total_bytes: Arc<std::sync::atomic::AtomicU64>,
    limit_tripped: Arc<std::sync::atomic::AtomicBool>,
}

fn observe_background_pipe<R>(
    mut pipe: R,
    stream: &'static str,
    streams: &BackgroundStreams,
    sink: crate::tools::stream_sink::StreamTail,
) -> tokio::task::JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let throttle = Arc::clone(&streams.throttle);
    let runner = Arc::clone(&streams.runner);
    let task_id = streams.task_id.clone();
    let total_bytes = Arc::clone(&streams.total_bytes);
    let limit_tripped = Arc::clone(&streams.limit_tripped);
    tokio::spawn(async move {
        let emit = |kind: &str, chunk: &[u8]| {
            if throttle.admit(kind) {
                let text = normalize_output(&String::from_utf8_lossy(chunk));
                runner.emit_progress(&task_id, &text, kind);
            }
        };
        crate::tools::stream_sink::read_bounded_stream_to_sink(
            &mut pipe,
            stream,
            &emit,
            &total_bytes,
            &limit_tripped,
            &sink,
        )
        .await;
    })
}

async fn run_background(
    mut child: tokio::process::Child,
    runner: Arc<crate::storage::TaskRunner>,
    task_id: String,
    timeout: Option<Duration>,
) -> String {
    let streams = BackgroundStreams {
        throttle: Arc::new(ProgressThrottle::new()),
        runner,
        task_id,
        total_bytes: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        limit_tripped: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    let out_sink = crate::tools::stream_sink::StreamTail::default();
    let err_sink = crate::tools::stream_sink::StreamTail::default();

    let observers: Vec<tokio::task::JoinHandle<()>> = [
        child
            .stdout
            .take()
            .map(|pipe| observe_background_pipe(pipe, "stdout", &streams, out_sink.clone())),
        child
            .stderr
            .take()
            .map(|pipe| observe_background_pipe(pipe, "stderr", &streams, err_sink.clone())),
    ]
    .into_iter()
    .flatten()
    .collect();
    let drained = async {
        for observer in observers {
            let _ = observer.await;
        }
    };

    let wait_and_drain = async {
        let _ = child.wait().await;
        let _ = tokio::time::timeout(Duration::from_millis(250), drained).await;
        (out_sink.take(), err_sink.take())
    };
    let (out, err) = match timeout {
        Some(limit) => match tokio::time::timeout(limit, wait_and_drain).await {
            Ok(result) => result,
            Err(_) => {
                crate::native::bash::kill_process_tree_async(&mut child).await;
                return format!(
                    "Background command timed out after {}s and was stopped.",
                    limit.as_secs()
                );
            }
        },
        None => wait_and_drain.await,
    };

    if streams
        .limit_tripped
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        crate::native::bash::kill_process_tree_async(&mut child).await;
        return output_limit_reason();
    }

    let mut combined = normalize_output(&String::from_utf8_lossy(&out));
    let err_text = normalize_output(&String::from_utf8_lossy(&err));
    if !err_text.trim().is_empty() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(err_text.trim_end());
    }
    combined
}

impl NativeToolset {
    pub(super) async fn powershell_with(
        &self,
        args: &Value,
        on_update: Option<OutputUpdate<'_>>,
    ) -> Option<ExecutableToolResult> {
        let Some(command) = args.get("command") else {
            return Some(err_result(arg_error_text(
                "PowerShell",
                "command",
                "engine.tools.argMissing",
            )));
        };
        let Some(command) = command.as_str() else {
            return Some(err_result(arg_error_text(
                "PowerShell",
                "command",
                "engine.tools.argMustBeString",
            )));
        };
        let runs_in_background =
            args.get("run_in_background").and_then(Value::as_bool) == Some(true);
        if let Some(timeout) = args.get("timeout").and_then(Value::as_u64) {
            let max = if runs_in_background {
                MAX_BACKGROUND_TIMEOUT_S
            } else {
                MAX_TIMEOUT_S
            };
            if timeout > max {
                return Some(err_result(format!("timeout must be ≤ {max}s")));
            }
        }
        let description = args.get("description").and_then(Value::as_str);

        if let Some(refusal) =
            crate::tools::shell_results::validate_run_request(command, false, None)
        {
            return Some(err_result(refusal));
        }

        let working_dir = match args.get("cwd").and_then(Value::as_str) {
            Some(cwd) => match Self::resolve_io(&self.sandbox(), &self.shell_bridge, cwd) {
                Ok(resolved) => resolved,
                Err(error) => {
                    return Some(err_result(
                        LocalizedText::with_params(
                            "engine.tools.bash.cwdUnavailable",
                            i18n_params!["cwd" => cwd, "e" => error],
                        )
                        .render(),
                    ));
                }
            },
            None => self.execution_root(),
        };
        let agent_env = self.execution_environment();
        let exe = match resolve_powershell() {
            Ok(exe) => exe,
            Err(error) => return Some(err_result(error.message())),
        };

        if runs_in_background {
            let runner = self.task_runner.as_ref()?;
            if let Some(refusal) =
                crate::tools::shell_results::validate_run_request(command, true, description)
            {
                return Some(err_result(refusal));
            }
            let timeout = background_timeout(args);
            let task_id = crate::storage::generate_task_id("bash");
            let desc = description.map(str::trim).unwrap_or(command).to_string();
            let child = match spawn_powershell(&exe, command, &working_dir, &agent_env) {
                Ok(child) => child,
                Err(error) => {
                    return Some(err_result(
                        LocalizedText::with_params(
                            "engine.tools.bash.commandFailed",
                            i18n_params!["e" => error],
                        )
                        .render(),
                    ));
                }
            };
            let runner_task = Arc::clone(runner);
            let progress_runner = Arc::clone(runner);
            let progress_task_id = task_id.clone();

            let bg_fut = async move {
                crate::storage::TaskOutcome::Completed(
                    run_background(child, progress_runner, progress_task_id, timeout).await,
                )
            };
            if let Err(e) = runner_task.spawn_task_with_meta(
                crate::storage::TaskSpawnMeta {
                    session_id: self.session_id.as_deref(),
                    kind: "bash",
                    subagent_type: None,
                    agent_id: None,
                    timeout_ms: timeout.map(|limit| limit.as_millis() as u64),
                },
                task_id.clone(),
                desc.clone(),
                bg_fut,
            ) {
                return Some(err_result(
                    LocalizedText::with_params(
                        "engine.tools.bash.backgroundStartRefused",
                        i18n_params!["e" => e],
                    )
                    .render(),
                ));
            }
            let next_steps = crate::tools::shell_results::next_step_lines(
                true,
                crate::tools::shell_results::BackgroundStart::Started,
            );
            return Some(ok_result(format!(
                "Background task started (task_id: {task_id}).\ncommand: {command}\ndescription: {desc}\n{next_steps}"
            )));
        }

        let timeout_s = normalize_timeout_s(args.get("timeout").and_then(Value::as_u64), false);
        let timeout = Duration::from_secs(timeout_s);
        let mut child = match spawn_powershell(&exe, command, &working_dir, &agent_env) {
            Ok(child) => child,
            Err(error) => {
                return Some(err_result(
                    LocalizedText::with_params(
                        "engine.tools.bash.commandFailed",
                        i18n_params!["e" => error],
                    )
                    .render(),
                ));
            }
        };
        let mut stdout_pipe = child.stdout.take();
        let mut stderr_pipe = child.stderr.take();
        let throttle = ProgressThrottle::new();
        let emit = |kind: &str, chunk: &[u8]| {
            if throttle.admit(kind)
                && let Some(cb) = on_update
            {
                cb(&crate::tools::ToolUpdate::text(
                    kind,
                    &normalize_output(&String::from_utf8_lossy(chunk)),
                ));
            }
        };

        let remaining = std::sync::atomic::AtomicUsize::new(MAX_TASK_OUTPUT_BYTES);
        let discarded = std::sync::atomic::AtomicU64::new(0);
        let mut stdout_bytes = Vec::new();
        let mut stderr_bytes = Vec::new();
        let waited = tokio::time::timeout(timeout, async {
            let (out_status, err_status, status) = tokio::join!(
                crate::tools::stream_sink::read_capped_stream(
                    stdout_pipe.as_mut(),
                    "stdout",
                    &emit,
                    &mut stdout_bytes,
                    &remaining,
                    &discarded,
                ),
                crate::tools::stream_sink::read_capped_stream(
                    stderr_pipe.as_mut(),
                    "stderr",
                    &emit,
                    &mut stderr_bytes,
                    &remaining,
                    &discarded,
                ),
                child.wait(),
            );
            (out_status, err_status, status)
        })
        .await;

        let (stdout_bytes, stderr_bytes, exit_status, read_errors) = match waited {
            Ok((out_status, err_status, Ok(status))) => (
                stdout_bytes,
                stderr_bytes,
                status,
                [out_status.err(), err_status.err()],
            ),

            Ok((_out_status, _err_status, Err(error))) => {
                return Some(err_result(
                    LocalizedText::with_params(
                        "engine.tools.bash.waitFailed",
                        i18n_params!["e" => error],
                    )
                    .render(),
                ));
            }
            Err(_) => {
                if self.bash_auto_background
                    && let Some(runner) = &self.task_runner
                {
                    let task_id = crate::storage::generate_task_id("bash");
                    let desc = crate::tools::shell_results::foreground_description(
                        POWERSHELL_TOOL_NAME,
                        command,
                        description,
                    );
                    let detach_timeout = self.background_bash_timeout(args);
                    let detached_pid = child.id();
                    let bg_fut = async move {
                        match detach_timeout {
                            Some(limit) => match tokio::time::timeout(limit, child.wait()).await {
                                Ok(_) => "Background command execution finished".to_string(),
                                Err(_) => {
                                    crate::native::bash::kill_process_tree_async(&mut child).await;
                                    format!(
                                        "Background command timed out after {}s and was stopped.",
                                        limit.as_secs()
                                    )
                                }
                            },
                            None => {
                                let _ = child.wait().await;
                                "Background command execution finished".to_string()
                            }
                        }
                    };
                    let bg_fut =
                        async move { crate::storage::TaskOutcome::Completed(bg_fut.await) };
                    if let Err(e) = runner.spawn_task_with_meta(
                        crate::storage::TaskSpawnMeta {
                            session_id: self.session_id.as_deref(),
                            kind: "bash",
                            subagent_type: None,
                            agent_id: None,
                            timeout_ms: detach_timeout.map(|d| d.as_millis() as u64),
                        },
                        task_id.clone(),
                        desc,
                        bg_fut,
                    ) {
                        if let Some(pid) = detached_pid {
                            crate::native::bash::kill_process_tree_by_pid(pid).await;
                        }
                        return Some(err_result(
                            LocalizedText::with_params(
                                "engine.tools.bash.backgroundStartFailed",
                                i18n_params!["timeout_s" => timeout_s, "e" => e],
                            )
                            .render(),
                        ));
                    }
                    let next_steps = crate::tools::shell_results::next_step_lines(
                        true,
                        crate::tools::shell_results::BackgroundStart::DetachedOnTimeout,
                    );
                    return Some(ok_result(format!(
                        "Command timed out after {timeout_s}s and was moved to the background (task_id: {task_id}).\n{next_steps}"
                    )));
                }
                crate::native::bash::kill_process_tree_async(&mut child).await;
                return Some(err_result(
                    LocalizedText::with_params(
                        "engine.tools.bash.killedByTimeout",
                        i18n_params!["timeout_s" => timeout_s],
                    )
                    .render(),
                ));
            }
        };

        let mut text = normalize_output(&String::from_utf8_lossy(&stdout_bytes));
        let stderr = normalize_output(&String::from_utf8_lossy(&stderr_bytes));
        if !stderr.trim().is_empty() {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(stderr.trim_end());
        }

        let discarded_bytes = discarded.load(std::sync::atomic::Ordering::Relaxed);
        if discarded_bytes > 0 {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(
                &LocalizedText::with_params(
                    "engine.tools.bash.outputTruncated",
                    i18n_params![
                        "mib" => MAX_TASK_OUTPUT_BYTES / (1024 * 1024),
                        "bytes" => discarded_bytes
                    ],
                )
                .render(),
            );
        }
        for error in read_errors.iter().flatten() {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(
                &LocalizedText::with_params(
                    "engine.tools.bash.outputReadFailed",
                    i18n_params!["e" => error],
                )
                .render(),
            );
        }
        if exit_status.success() {
            let out = if text.trim().is_empty() {
                "Command executed successfully.".to_string()
            } else {
                text
            };
            Some(ok_result(out))
        } else {
            Some(err_result(super::bash_exec::failed_command_output(
                text,
                &exit_status,
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_description_is_the_rendered_v2_template() {
        let description = render_description();
        assert!(description.starts_with(
            "Execute a Windows PowerShell script natively (PowerShell 7 `pwsh` when available"
        ));
        assert!(
            description.contains(
                "Foreground calls default to 60s and allow up to 300s; a foreground call that hits its timeout is moved to the background instead of being killed."
            ),
            "the four constants must be substituted"
        );
        assert!(description.contains(
            "Background commands default to 600s timeout and `timeout` is capped at 86400s"
        ));
        assert!(!description.contains("${"), "no placeholder may survive");
    }

    #[test]
    fn the_schema_is_the_v2_input_schema() {
        let def = tool_def();
        assert_eq!(def.name, "PowerShell");

        assert_eq!(
            def.input_schema,
            serde_json::json!({
                "$schema": "http://json-schema.org/draft-07/schema#",
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
                "required": ["command"],
                "additionalProperties": false
            })
        );
    }

    #[test]
    fn foreground_timeouts_default_to_sixty_and_cap_at_three_hundred() {
        assert_eq!(normalize_timeout_s(None, false), 60);
        assert_eq!(normalize_timeout_s(Some(1), false), 1);
        assert_eq!(normalize_timeout_s(Some(301), false), 300);
        assert_eq!(normalize_timeout_s(Some(86_400), false), 300);
    }

    #[test]
    fn background_timeouts_default_to_six_hundred_and_cap_at_a_day() {
        assert_eq!(normalize_timeout_s(None, true), 600);
        assert_eq!(normalize_timeout_s(Some(3_600), true), 3_600);
        assert_eq!(normalize_timeout_s(Some(86_401), true), 86_400);
    }

    #[test]
    fn disable_timeout_drops_a_background_bound() {
        assert_eq!(
            background_timeout(&serde_json::json!({ "disable_timeout": true })),
            None
        );
        assert_eq!(
            background_timeout(&serde_json::json!({ "timeout": 90 })),
            Some(Duration::from_secs(90))
        );
        assert_eq!(
            background_timeout(&serde_json::json!({})),
            Some(Duration::from_secs(600))
        );
    }
}
