use super::*;

impl NativeToolset {
    // ── Bash ───────────────────────────────────────────────────────────

    /// [`Self::bash`] with a mid-execution output stream (P57): every chunk
    /// the child writes is handed to `on_update(kind, text)` so the host can
    /// drive live `tool.progress` cards. Chunks are throttled to one event
    /// per [`PROGRESS_MIN_INTERVAL_MS`] — a chatty command can write tens of
    /// MiBs per second, and flooding the host event line would slow the very
    /// turn the progress card is decorating. The model still receives the
    /// full output through the final result.
    pub(super) async fn bash_with(
        &self,
        args: &Value,
        on_update: Option<OutputUpdate<'_>>,
    ) -> Option<ExecutableToolResult> {
        let command = args.get("command")?.as_str()?;
        // Working directory defaults to the sandbox root; explicit cwd must
        // stay inside it. Returns `None` (host fallback) on escape — the
        // host applies its own cwd policy there.
        let working_dir = match args.get("cwd").and_then(|c| c.as_str()) {
            Some(cwd) => Self::resolve(&self.sandbox(), &self.shell_bridge, cwd)?,
            None => self.root.clone(),
        };

        // Background tasks: if TaskRunner is present, spawn natively and return task info;
        // otherwise hand back to host fallback.
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
                // `[background].bash_task_timeout_s` (per-call `timeout` wins,
                // `disable_timeout` opts out).
                let bg_timeout = self.background_bash_timeout(args);
                let progress_runner = runner.clone();
                let progress_task_id = task_id.clone();

                let bg_fut = async move {
                    let mut cmd = tokio::process::Command::new(&shell_cmd);
                    for arg in &shell_args {
                        cmd.arg(arg);
                    }
                    cmd.arg(&cmd_str)
                        .current_dir(&work_dir)
                        .env("NO_COLOR", "1")
                        .env("TERM", "dumb")
                        .env("GIT_TERMINAL_PROMPT", "0")
                        .env("SHELL", &shell_cmd)
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::piped())
                        .stderr(std::process::Stdio::piped())
                        // Killing on drop takes the process down if this future
                        // is abandoned.
                        .kill_on_drop(true);
                    let mut child = match cmd.spawn() {
                        Ok(child) => child,
                        Err(e) => {
                            return LocalizedText::with_params(
                                "engine.tools.bash.commandFailed",
                                i18n_params!["e" => e],
                            )
                            .render();
                        }
                    };
                    let mut stdout_pipe = child.stdout.take();
                    let mut stderr_pipe = child.stderr.take();
                    // Stream output and report it as `event.task.progress`, so
                    // the Web task card updates live instead of only at settle.
                    // Throttled the same way foreground Bash is.
                    let last_emit = ProgressThrottle::new();
                    let emit = |stream: &str, chunk: &[u8]| {
                        if last_emit.admit(stream) {
                            progress_runner.emit_progress(
                                &progress_task_id,
                                &String::from_utf8_lossy(chunk),
                                stream,
                            );
                        }
                    };
                    // Shared across both streams because v2 counts the whole
                    // task's output once (`entry.outputSizeBytes`).
                    let total_bytes = std::sync::atomic::AtomicU64::new(0);
                    let limit_tripped = std::sync::atomic::AtomicBool::new(false);
                    let collect = async {
                        // Both streams must be drained **concurrently**. Draining
                        // stdout to EOF before touching stderr deadlocks: a
                        // process that fills the stderr pipe blocks on writing
                        // it, never closes stdout, and the stdout read waits
                        // forever. `child.wait()` deliberately stays out of this
                        // join so the caller can still kill it below.
                        tokio::join!(
                            async {
                                match stdout_pipe.as_mut() {
                                    Some(pipe) => {
                                        read_bounded_stream(
                                            pipe,
                                            "stdout",
                                            &emit,
                                            &total_bytes,
                                            &limit_tripped,
                                        )
                                        .await
                                    }
                                    None => Vec::new(),
                                }
                            },
                            async {
                                match stderr_pipe.as_mut() {
                                    Some(pipe) => {
                                        read_bounded_stream(
                                            pipe,
                                            "stderr",
                                            &emit,
                                            &total_bytes,
                                            &limit_tripped,
                                        )
                                        .await
                                    }
                                    None => Vec::new(),
                                }
                            },
                        )
                    };
                    let (out, err) = match bg_timeout {
                        Some(limit) => match tokio::time::timeout(limit, collect).await {
                            Ok(result) => result,
                            Err(_) => {
                                let _ = child.kill().await;
                                let _ = child.wait().await;
                                return format!(
                                    "Background command timed out after {}s and was stopped.",
                                    limit.as_secs()
                                );
                            }
                        },
                        None => collect.await,
                    };
                    // v2 `appendOutput`'s limit arm (taskService.ts:962-969):
                    // trip once, stop the task, and stop accepting output. Both
                    // readers have returned by now — a pipe nobody drains fills
                    // and would wedge the process instead of letting this land.
                    let tripped = limit_tripped.load(std::sync::atomic::Ordering::Relaxed);
                    if tripped {
                        let _ = child.kill().await;
                    }
                    let _ = child.wait().await;
                    if tripped {
                        return output_limit_reason();
                    }
                    let mut s = String::from_utf8_lossy(&out).into_owned();
                    let err_text = String::from_utf8_lossy(&err);
                    if !err_text.trim().is_empty() {
                        if !s.is_empty() && !s.ends_with('\n') {
                            s.push('\n');
                        }
                        s.push_str(err_text.trim_end());
                    }
                    s
                };
                // A background command's exit status is part of its output, not
                // a verdict on the task: the command ran. Only the runner's
                // own `stop()` or a work error settles a task non-completed.
                let bg_fut = async move { crate::storage::TaskOutcome::Completed(bg_fut.await) };

                if runner
                    .spawn_task_with_meta(
                        crate::storage::TaskSpawnMeta {
                            session_id: self.session_id.as_deref(),
                            kind: "bash",
                            subagent_type: None,
                            agent_id: None,
                            timeout_ms: bg_timeout.map(|d| d.as_millis() as u64),
                        },
                        task_id.clone(),
                        desc.clone(),
                        bg_fut,
                    )
                    .is_ok()
                {
                    return Some(ok_result(format!(
                        "Background task started (task_id: {task_id}).\ncommand: {command}\ndescription: {desc}\nUse TaskList or TaskOutput to inspect progress."
                    )));
                }
            }
            return None;
        }
        // Timeout semantics mirror the host Bash tool: seconds, default 60,
        // capped at 300 for foreground commands.
        let timeout_s = args
            .get("timeout")
            .and_then(|v| v.as_u64())
            .unwrap_or(60)
            .min(BASH_MAX_SECONDS);
        let timeout = Duration::from_secs(timeout_s.max(1));

        // The host Bash contract is bash everywhere (Git Bash on Windows);
        // without the host's shell path on Windows there is no faithful
        // native execution, so `None` sends the call back to the host.
        let shell = self.shell.as_ref()?;
        // Mirror the host's non-interactive env: colors and prompts corrupt
        // output parsing, and git must never hang on a credential prompt.
        let mut command_builder = tokio::process::Command::new(shell);
        for arg in crate::native::shell::ShellFlavor::from_path(shell).args_prefix() {
            command_builder.arg(arg);
        }
        let mut child = command_builder
            .arg(command)
            .current_dir(&working_dir)
            .env("NO_COLOR", "1")
            .env("TERM", "dumb")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("SHELL", shell)
            // The engine's own stdin is the host RPC transport. Inheriting it
            // would let any stdin-reading command (`cat`, `read`, `git`,
            // `npm init`) swallow host traffic and corrupt the protocol; on
            // Windows the inherited pipe also keeps Git Bash from exiting,
            // which hangs the turn. Commands get an empty stdin instead.
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .ok()?;
        let mut stdout_pipe = child.stdout.take();
        let mut stderr_pipe = child.stderr.take();
        // The command is already running here, so a timeout must be reported
        // as a killed command — never fall back to the host, which would
        // re-execute it.
        use tokio::io::AsyncReadExt;
        let throttle = ProgressThrottle::new();
        let emit = |kind: &str, text: &str| {
            if throttle.admit(kind)
                && let Some(cb) = on_update
            {
                cb(kind, text);
            }
        };
        let waited = tokio::time::timeout(timeout, async {
            let (out, err, status) = tokio::join!(
                async {
                    let mut buf = Vec::new();
                    if let Some(pipe) = stdout_pipe.as_mut() {
                        let mut chunk = [0u8; 8192];
                        loop {
                            match pipe.read(&mut chunk).await {
                                Ok(0) | Err(_) => break,
                                Ok(n) => {
                                    emit("stdout", &String::from_utf8_lossy(&chunk[..n]));
                                    buf.extend_from_slice(&chunk[..n]);
                                }
                            }
                        }
                    }
                    buf
                },
                async {
                    let mut buf = Vec::new();
                    if let Some(pipe) = stderr_pipe.as_mut() {
                        let mut chunk = [0u8; 8192];
                        loop {
                            match pipe.read(&mut chunk).await {
                                Ok(0) | Err(_) => break,
                                Ok(n) => {
                                    emit("stderr", &String::from_utf8_lossy(&chunk[..n]));
                                    buf.extend_from_slice(&chunk[..n]);
                                }
                            }
                        }
                    }
                    buf
                },
                child.wait(),
            );
            (out, err, status)
        })
        .await;
        let (stdout_bytes, stderr_bytes, exit_code) = match waited {
            Ok((out, err, Ok(status))) => (out, err, status.code().unwrap_or(-1)),
            // timeout or wait failure: if TaskRunner is available, migrate to background task!
            _ => {
                // `[background].bash_auto_background_on_timeout` (default
                // on): a timed-out foreground command migrates to the
                // background; when the user turns it off the command is
                // killed instead.
                if self.bash_auto_background
                    && let Some(runner) = &self.task_runner
                {
                    let task_id = crate::storage::generate_task_id("bash");
                    let desc = LocalizedText::with_params(
                        "engine.tools.bash.timedOut",
                        i18n_params!["command" => command],
                    )
                    .render();
                    // The migrated task is re-armed with the *background*
                    // bounds (600s unless the call or
                    // `[background].bash_task_timeout_s` says otherwise) — the
                    // 60s/300s foreground budget was computed separately.
                    let bg_timeout = self.background_bash_timeout(args);
                    let bg_fut = async move {
                        match bg_timeout {
                            Some(limit) => match tokio::time::timeout(limit, child.wait()).await {
                                Ok(_) => "Background command execution finished".to_string(),
                                Err(_) => {
                                    let _ = child.kill().await;
                                    let _ = child.wait().await;
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
                    // Same reasoning as the other background-bash arm: the
                    // command ran, and how it exited is its output.
                    let bg_fut =
                        async move { crate::storage::TaskOutcome::Completed(bg_fut.await) };
                    let _ = runner.spawn_task_with_meta(
                        crate::storage::TaskSpawnMeta {
                            session_id: self.session_id.as_deref(),
                            kind: "bash",
                            subagent_type: None,
                            agent_id: None,
                            timeout_ms: bg_timeout.map(|d| d.as_millis() as u64),
                        },
                        task_id.clone(),
                        desc,
                        bg_fut,
                    );
                    return Some(ok_result(format!(
                        "Command timed out after {}s and was moved to the background (task_id: {task_id}).\nUse TaskList to check status or TaskOutput to view output.",
                        timeout_s
                    )));
                }
                let _ = child.kill().await;
                // Reap it. Killing leaves the child a zombie until it is
                // waited on, and this loop can hit the timeout repeatedly
                // within one turn.
                let _ = child.wait().await;
                return Some(err_result(
                    LocalizedText::with_params(
                        "engine.tools.bash.killedByTimeout",
                        i18n_params!["timeout_s" => timeout_s],
                    )
                    .render(),
                ));
            }
        };

        let mut text = String::from_utf8_lossy(&stdout_bytes).into_owned();
        let stderr = String::from_utf8_lossy(&stderr_bytes);
        if !stderr.trim().is_empty() {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(stderr.trim_end());
        }
        // No byte ceiling here: v2's Bash retains the full output (the
        // accumulator caps at 10 MB, the same ceiling the truncation service
        // uses) and spills oversized results to a file the model can read. A
        // 256 KB cut here threw the tail away before the spill could keep it.
        if exit_code == 0 {
            // v2 `foregroundCompletionResult`: `builder.ok('Command executed
            // successfully.')` — the message is the output when nothing was
            // produced, and it keeps its period.
            let out = if text.trim().is_empty() {
                "Command executed successfully.".to_string()
            } else {
                text
            };
            Some(ok_result(out))
        } else {
            // v2 writes the exit line first when nothing was printed, so the
            // result never opens with a blank line.
            let mut out = if text.trim().is_empty() {
                format!("Process exited with code {exit_code}")
            } else {
                text
            };
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&format!("Command failed with exit code: {exit_code}."));
            Some(err_result(out))
        }
    }
}
