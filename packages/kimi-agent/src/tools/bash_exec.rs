use super::*;
use crate::agent::tools::os::shell::shell_tool_results::normalize_timeout_ms;

fn shell_git_terminal_prompt() -> std::ffi::OsString {
    std::env::var_os("GIT_TERMINAL_PROMPT").unwrap_or_else(|| "0".into())
}

impl NativeToolset {
    pub(super) async fn bash_with(
        &self,
        args: &Value,
        on_update: Option<OutputUpdate<'_>>,
        tool_call_id: Option<&str>,
    ) -> Option<ExecutableToolResult> {
        let Some(command) = args.get("command") else {
            return Some(err_result(arg_error_text(
                "Bash",
                "command",
                "engine.tools.argMissing",
            )));
        };
        let Some(command) = command.as_str() else {
            return Some(err_result(arg_error_text(
                "Bash",
                "command",
                "engine.tools.argMustBeString",
            )));
        };

        let command = if cfg!(windows) {
            crate::tools::bash_null_redirect::rewrite_windows_null_redirect(command)
        } else {
            command.to_string()
        };
        let command = command.as_str();
        let starts_in_background = args
            .get("run_in_background")
            .and_then(|value| value.as_bool())
            == Some(true);
        let requested_timeout = match shell_results::parse_timeout(args) {
            Ok(timeout) => timeout,
            Err(message) => return Some(err_result(message.into())),
        };
        let description = args.get("description").and_then(Value::as_str);
        if let Some(refusal) = shell_results::validate_run_request(
            command,
            starts_in_background,
            description,
            self.shell_background_available(),
        ) {
            return Some(err_result(refusal));
        }

        let working_dir = match args.get("cwd").and_then(|c| c.as_str()) {
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

        if args.get("run_in_background").and_then(|v| v.as_bool()) == Some(true) {
            if let Some(runner) = &self.task_runner {
                let task_id = crate::storage::generate_task_id("bash");
                let desc = args
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or(command)
                    .to_string();
                let shell_cmd = self.shell.as_ref()?.clone();
                let shell_args =
                    crate::native::shell::ShellFlavor::from_path(&shell_cmd).args_prefix();
                let cmd_str = command.to_string();
                let work_dir = working_dir.clone();

                let bg_timeout = shell_results::background_timeout(
                    requested_timeout,
                    args.get("disable_timeout").and_then(Value::as_bool) == Some(true),
                );
                let progress_runner = runner.clone();
                let progress_task_id = task_id.clone();

                let mut cmd = tokio::process::Command::new(&shell_cmd);
                for arg in &shell_args {
                    cmd.arg(arg);
                }
                cmd.arg(&cmd_str)
                    .current_dir(&work_dir)
                    .envs(&agent_env)
                    .env("NO_COLOR", "1")
                    .env("TERM", "dumb")
                    .env("GIT_TERMINAL_PROMPT", shell_git_terminal_prompt())
                    .env("SHELL", &shell_cmd)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .kill_on_drop(true);

                #[cfg(unix)]
                cmd.process_group(0);
                #[cfg(windows)]
                cmd.creation_flags(0x08000000);
                let mut child = match cmd.spawn() {
                    Ok(child) => child,
                    Err(e) => {
                        return Some(err_result(
                            LocalizedText::with_params(
                                "engine.tools.bash.commandFailed",
                                i18n_params!["e" => e],
                            )
                            .render(),
                        ));
                    }
                };
                let pid = child.id();

                let bg_fut = async move {
                    let mut stdout_pipe = child.stdout.take();
                    let mut stderr_pipe = child.stderr.take();

                    let throttle = std::sync::Arc::new(ProgressThrottle::new());
                    let total_bytes = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
                    let limit_tripped =
                        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                    let out_sink = crate::tools::stream_sink::StreamTail::default();
                    let err_sink = crate::tools::stream_sink::StreamTail::default();

                    let mut observers: Vec<tokio::task::JoinHandle<()>> = Vec::new();
                    if let Some(mut pipe) = stdout_pipe.take() {
                        let throttle = throttle.clone();
                        let runner = progress_runner.clone();
                        let task_id = progress_task_id.clone();
                        let total = total_bytes.clone();
                        let tripped = limit_tripped.clone();
                        let sink = out_sink.clone();
                        observers.push(tokio::spawn(async move {
                            let emit = |kind: &str, chunk: &[u8]| {
                                if throttle.admit(kind) {
                                    runner.emit_progress(
                                        &task_id,
                                        &String::from_utf8_lossy(chunk),
                                        kind,
                                    );
                                }
                            };
                            crate::tools::stream_sink::read_bounded_stream_to_sink(
                                &mut pipe, "stdout", &emit, &total, &tripped, &sink,
                            )
                            .await;
                        }));
                    }
                    if let Some(mut pipe) = stderr_pipe.take() {
                        let throttle = throttle.clone();
                        let runner = progress_runner.clone();
                        let task_id = progress_task_id.clone();
                        let total = total_bytes.clone();
                        let tripped = limit_tripped.clone();
                        let sink = err_sink.clone();
                        observers.push(tokio::spawn(async move {
                            let emit = |kind: &str, chunk: &[u8]| {
                                if throttle.admit(kind) {
                                    runner.emit_progress(
                                        &task_id,
                                        &String::from_utf8_lossy(chunk),
                                        kind,
                                    );
                                }
                            };
                            crate::tools::stream_sink::read_bounded_stream_to_sink(
                                &mut pipe, "stderr", &emit, &total, &tripped, &sink,
                            )
                            .await;
                        }));
                    }
                    let drained = async {
                        for observer in observers {
                            let _ = observer.await;
                        }
                    };

                    let (out, err, exit_code) = match bg_timeout {
                        Some(limit) => match tokio::time::timeout(limit, async {
                            let exit_code =
                                child.wait().await.ok().and_then(|status| status.code());
                            let _ = tokio::time::timeout(Duration::from_millis(250), drained).await;
                            (out_sink.take(), err_sink.take(), exit_code)
                        })
                        .await
                        {
                            Ok(result) => result,
                            Err(_) => {
                                crate::native::bash::kill_process_tree_async(&mut child).await;
                                return (
                                    format!(
                                        "Background command timed out after {}s and was stopped.",
                                        limit.as_secs()
                                    ),
                                    None,
                                );
                            }
                        },
                        None => {
                            let exit_code =
                                child.wait().await.ok().and_then(|status| status.code());
                            let _ = tokio::time::timeout(Duration::from_millis(250), drained).await;
                            (out_sink.take(), err_sink.take(), exit_code)
                        }
                    };

                    let tripped = limit_tripped.load(std::sync::atomic::Ordering::Relaxed);
                    if tripped {
                        crate::native::bash::kill_process_tree_async(&mut child).await;
                        return (output_limit_reason(), None);
                    }
                    let mut s = String::from_utf8_lossy(&out).into_owned();
                    let err_text = String::from_utf8_lossy(&err);
                    if !err_text.trim().is_empty() {
                        if !s.is_empty() && !s.ends_with('\n') {
                            s.push('\n');
                        }
                        s.push_str(err_text.trim_end());
                    }
                    (s, exit_code)
                };
                let bg_fut = async move {
                    let (output, exit_code) = bg_fut.await;
                    crate::storage::TaskOutcome::Process { output, exit_code }
                };

                if let Err(e) = runner.spawn_process_task(
                    crate::storage::TaskSpawnMeta {
                        session_id: self.session_id.as_deref(),
                        kind: "bash",
                        subagent_type: None,
                        agent_id: None,
                        timeout_ms: bg_timeout.map(|d| d.as_millis() as u64),
                    },
                    command,
                    pid,
                    tool_call_id,
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
                    self.shell_background_available(),
                    crate::tools::shell_results::BackgroundStart::Started,
                );
                return Some(ok_result(format!(
                    "Background task started (task_id: {task_id}).\ncommand: {command}\ndescription: {desc}\n{next_steps}"
                )));
            }
            return None;
        }

        let timeout = Duration::from_millis(normalize_timeout_ms(requested_timeout, false));
        let timeout_s = timeout.as_secs();

        let shell = self.shell.as_ref()?;

        let mut command_builder = tokio::process::Command::new(shell);
        for arg in crate::native::shell::ShellFlavor::from_path(shell).args_prefix() {
            command_builder.arg(arg);
        }
        command_builder
            .arg(command)
            .current_dir(&working_dir)
            .envs(&agent_env)
            .env("NO_COLOR", "1")
            .env("TERM", "dumb")
            .env("GIT_TERMINAL_PROMPT", shell_git_terminal_prompt())
            .env("SHELL", shell)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        #[cfg(unix)]
        command_builder.process_group(0);
        #[cfg(windows)]
        command_builder.creation_flags(0x08000000);
        let mut child = match command_builder.spawn() {
            Ok(child) => child,

            Err(e) => {
                return Some(err_result(
                    LocalizedText::with_params(
                        "engine.tools.bash.commandFailed",
                        i18n_params!["e" => e],
                    )
                    .render(),
                ));
            }
        };
        let mut stdout_pipe = child.stdout.take();
        let mut stderr_pipe = child.stderr.take();

        let output_accumulator = std::sync::Arc::new(std::sync::Mutex::new(
            crate::tool::output_accumulator::ToolOutputAccumulator::new(),
        ));
        let output_decoders =
            std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::<
                String,
                encoding_rs::Decoder,
            >::new()));
        let throttle = ProgressThrottle::new();
        let emit = {
            let accumulator = output_accumulator.clone();
            let decoders = output_decoders.clone();
            move |kind: &str, chunk: &[u8]| {
                let mut decoded =
                    String::with_capacity(chunk.len().saturating_mul(3).saturating_add(4));
                decoders
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .entry(kind.to_string())
                    .or_insert_with(|| encoding_rs::UTF_8.new_decoder())
                    .decode_to_string(chunk, &mut decoded, false);
                accumulator
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .write(&decoded);
                if throttle.admit(kind)
                    && let Some(cb) = on_update
                {
                    cb(&crate::tools::ToolUpdate::text(
                        kind,
                        &String::from_utf8_lossy(chunk),
                    ));
                }
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
                    && self.shell_background_available()
                    && let Some(runner) = &self.task_runner
                {
                    let task_id = crate::storage::generate_task_id("bash");
                    let desc = LocalizedText::with_params(
                        "engine.tools.bash.timedOut",
                        i18n_params!["command" => command],
                    )
                    .render();

                    let bg_timeout = self.shell_detach_timeout();

                    let detached_pid = child.id();
                    let seed = promoted_output_seed(
                        &stdout_bytes,
                        &stderr_bytes,
                        discarded.load(std::sync::atomic::Ordering::Relaxed),
                    );
                    let bg_fut = async move {
                        match bg_timeout {
                            Some(limit) => match tokio::time::timeout(limit, child.wait()).await {
                                Ok(status) => (
                                    "Background command execution finished".to_string(),
                                    status.ok().and_then(|status| status.code()),
                                ),
                                Err(_) => {
                                    crate::native::bash::kill_process_tree_async(&mut child).await;
                                    (
                                        format!(
                                            "Background command timed out after {}s and was stopped.",
                                            limit.as_secs()
                                        ),
                                        None,
                                    )
                                }
                            },
                            None => {
                                let exit_code =
                                    child.wait().await.ok().and_then(|status| status.code());
                                (
                                    "Background command execution finished".to_string(),
                                    exit_code,
                                )
                            }
                        }
                    };

                    let bg_fut = async move {
                        let (output, exit_code) = bg_fut.await;
                        crate::storage::TaskOutcome::Process { output, exit_code }
                    };

                    if let Err(e) = runner.spawn_promoted_process_task(
                        crate::storage::TaskSpawnMeta {
                            session_id: self.session_id.as_deref(),
                            kind: "bash",
                            subagent_type: None,
                            agent_id: None,
                            timeout_ms: bg_timeout.map(|d| d.as_millis() as u64),
                        },
                        command,
                        detached_pid,
                        tool_call_id,
                        task_id.clone(),
                        desc,
                        seed,
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
                    return Some(ok_result(format!(
                        "Command timed out after {}s and was moved to the background (task_id: {task_id}).\nUse TaskList to check status or TaskOutput to view output.",
                        timeout_s
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

        let discarded_bytes = discarded.load(std::sync::atomic::Ordering::Relaxed);
        let mut notices = String::new();
        if discarded_bytes > 0 {
            push_stream_notice(
                &mut notices,
                LocalizedText::with_params(
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
            push_stream_notice(
                &mut notices,
                LocalizedText::with_params(
                    "engine.tools.bash.outputReadFailed",
                    i18n_params!["e" => error],
                )
                .render(),
            );
        }

        for decoder in output_decoders
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .values_mut()
        {
            let mut tail = String::with_capacity(4);
            decoder.decode_to_string(&[], &mut tail, true);
            output_accumulator
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .write(&tail);
        }
        let mut builder = output_accumulator
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let empty_output = builder.n_chars() == 0;
        let accumulated = if exit_status.success() {
            builder.ok("Command executed successfully.", None)
        } else {
            if builder.n_chars() == 0
                && let Some(code) = exit_status.code()
            {
                builder.write(&format!("Process exited with code {code}"));
            }
            builder.error(
                &format!("{notices}{}", command_failure_suffix(&exit_status)),
                None,
            )
        };
        let total_chars = builder.total_chars();
        drop(builder);

        let mut content = if empty_output && exit_status.success() && !notices.is_empty() {
            String::new()
        } else {
            accumulated.content
        };
        if exit_status.success() && !notices.is_empty() {
            if !content.is_empty() && !content.ends_with('\n') {
                content.push('\n');
            }
            content.push_str(&notices);
        }
        let mut spill = accumulated.spill;
        if total_chars > crate::tool::tool_contract::DEFAULT_TOOL_RESULT_MAX_CHARS {
            spill.get_or_insert_with(Default::default).total_chars = Some(total_chars);
        }
        Some(ExecutableToolResult {
            spill,
            parts: Vec::new(),
            delivery: None,
            stop_turn: false,
            content,
            is_error: accumulated.is_error,
            note: None,
            display: None,
        })
    }
}

/// Stitch a command's two captured streams into one text block: stdout
/// verbatim, then stderr when it has non-whitespace content, separated by a
/// newline when stdout did not end with one.
fn merge_stream_text(stdout: &[u8], stderr: &[u8]) -> String {
    let mut text = String::from_utf8_lossy(stdout).into_owned();
    let stderr = String::from_utf8_lossy(stderr);
    if !stderr.trim().is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(stderr.trim_end());
    }
    text
}

/// The output a foreground command already produced when its timeout promoted
/// it to a task.
///
/// v2 registers the foreground process and its output sink at spawn
/// (`bashTool.ts:231-252`), so a task detached on timeout keeps everything
/// printed before the detach. This engine runs the foreground command inline
/// and only starts a task at promotion, so the text has to be carried over
/// explicitly; otherwise `TaskOutput` would start at the promotion point.
fn promoted_output_seed(stdout: &[u8], stderr: &[u8], discarded_bytes: u64) -> String {
    let mut seed = merge_stream_text(stdout, stderr);
    if discarded_bytes > 0 {
        if !seed.is_empty() && !seed.ends_with('\n') {
            seed.push('\n');
        }
        seed.push_str(
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
    seed
}

pub(super) fn failed_command_output(text: String, status: &std::process::ExitStatus) -> String {
    let mut out = if text.trim().is_empty() {
        String::new()
    } else {
        text
    };
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    if out.is_empty()
        && let Some(code) = status.code()
    {
        out.push_str(&format!("Process exited with code {code}"));
        out.push('\n');
    }
    out.push_str(&command_failure_suffix(status));
    out
}

fn command_failure_suffix(status: &std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("Command failed with exit code: {code}."),
        None => signal_termination_line(status).unwrap_or_else(|| {
            LocalizedText::new("engine.tools.bash.terminatedBySignalUnknown").render()
        }),
    }
}

fn push_stream_notice(notices: &mut String, message: String) {
    if !notices.is_empty() && !notices.ends_with('\n') {
        notices.push('\n');
    }
    notices.push_str(&message);
}

fn signal_termination_line(status: &std::process::ExitStatus) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        return status.signal().map(|signal| {
            LocalizedText::with_params(
                "engine.tools.bash.killedBySignal",
                i18n_params!["signal" => signal],
            )
            .render()
        });
    }
    #[cfg(not(unix))]
    {
        let _ = status;
        None
    }
}
