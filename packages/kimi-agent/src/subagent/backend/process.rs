//! External subagent backend child processes, ported from v2
//! `session/process/processRunner.ts`.
//!
//! The backends (`codex`, `acp`) drive an external CLI over its stdio. v2 wraps
//! that in a session-scoped `ISessionProcessRunner`; the engine has no DI scope,
//! so this module is the process half and the backend service owns the lifetime.

use std::collections::BTreeMap;
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// v2 `ProcessExecOptions`.
pub struct ProcessExecOptions {
    pub cwd: String,
    pub env: Option<BTreeMap<String, String>>,
}

/// v2 `IProcess`: the spawned child plus its stdio and lifecycle.
pub struct BackendProcess {
    child: Child,
    pub stdin: Option<ChildStdin>,
    pub stdout: Option<ChildStdout>,
    pid: u32,
}

impl BackendProcess {
    /// v2 `ISessionProcessRunner.exec(args, { cwd, env })`.
    ///
    /// `env` is merged over the inherited environment (v2 does the same), so a
    /// backend can add a token without losing `PATH`.
    pub async fn exec(args: &[String], options: ProcessExecOptions) -> Result<Self, String> {
        let (program, rest) = args.split_first().ok_or_else(|| {
            "a subagent backend command needs at least a program name".to_string()
        })?;
        let mut command = Command::new(program);
        command
            .args(rest)
            .current_dir(&options.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(env) = &options.env {
            command.envs(env);
        }
        let mut child = command.spawn().map_err(|error| {
            format!("cannot spawn the subagent backend {program}: {error}")
        })?;
        let pid = child.id().unwrap_or(0);
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        // A backend that writes enough to stderr without a reader would block on
        // a full pipe, so the drain runs for the process's lifetime.
        if let Some(mut stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut buffer = [0u8; 4096];
                while let Ok(read) = stderr.read(&mut buffer).await
                    && read > 0
                {
                }
            });
        }
        Ok(Self {
            child,
            stdin,
            stdout,
            pid,
        })
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// v2 `IProcess.exitCode`.
    pub fn exit_code(&mut self) -> Option<i32> {
        self.child.try_wait().ok().flatten().and_then(|status| status.code())
    }

    /// v2 `IProcess.wait`.
    pub async fn wait(&mut self) -> Result<i32, String> {
        let status = self
            .child
            .wait()
            .await
            .map_err(|error| format!("the subagent backend wait failed: {error}"))?;
        Ok(status.code().unwrap_or(-1))
    }

    /// v2 `IProcess.kill(signal)`. Only `SIGTERM` and `SIGKILL` are meaningful
    /// here; anything else is refused rather than silently mapped.
    pub async fn kill(&mut self, signal: &str) -> Result<(), String> {
        match signal {
            "SIGTERM" => {
                self.child
                    .start_kill()
                    .map_err(|error| format!("cannot terminate the subagent backend: {error}"))?;
            }
            "SIGKILL" => {
                self.child
                    .kill()
                    .await
                    .map_err(|error| format!("cannot kill the subagent backend: {error}"))?;
            }
            other => {
                return Err(format!(
                    "unsupported subagent backend signal {other} (expected SIGTERM or SIGKILL)"
                ));
            }
        }
        Ok(())
    }

    /// v2 `IProcess.dispose`: stop the child and close its stdin.
    pub async fn dispose(&mut self) {
        let _ = self.child.start_kill();
        self.stdin = None;
    }
}

/// Write one line to the child's stdin, appending the newline the ndjson
/// transports require.
pub async fn write_line(stdin: &mut Option<ChildStdin>, line: &str) -> Result<(), String> {
    let stdin = stdin
        .as_mut()
        .ok_or_else(|| "the subagent backend stdin is closed".to_string())?;
    stdin
        .write_all(line.as_bytes())
        .await
        .map_err(|error| format!("cannot write to the subagent backend: {error}"))?;
    stdin
        .write_all(b"\n")
        .await
        .map_err(|error| format!("cannot write to the subagent backend: {error}"))?;
    stdin
        .flush()
        .await
        .map_err(|error| format!("cannot flush the subagent backend: {error}"))?;
    Ok(())
}

/// Read one newline-delimited frame from the child's stdout. `None` means the
/// transport closed.
pub async fn read_line(reader: &mut tokio::io::BufReader<ChildStdout>) -> Option<String> {
    let mut line = String::new();
    match reader.read_line(&mut line).await {
        Ok(0) => None,
        Ok(_) => Some(line.trim_end_matches(['\r', '\n']).to_string()),
        Err(_) => None,
    }
}

/// Parse the child's stdout as newline-delimited JSON frames, the shape the
/// Claude Code CLI's `stream-json` output uses.
pub fn spawn_json_line_reader(
    stdout: ChildStdout,
) -> tokio::sync::mpsc::UnboundedReceiver<serde_json::Value> {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(stdout);
        loop {
            match read_line(&mut reader).await {
                None => break,
                Some(line) => {
                    if line.trim().is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<serde_json::Value>(&line) {
                        Ok(value) => {
                            if sender.send(value).is_err() {
                                break;
                            }
                        }
                        Err(_) => continue,
                    }
                }
            }
        }
    });
    receiver
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_cwd() -> String {
        std::env::temp_dir().to_string_lossy().to_string()
    }

    #[tokio::test]
    async fn exec_reports_the_pid_and_the_exit_code() {
        let mut process = BackendProcess::exec(
            &["git".to_string(), "--version".to_string()],
            ProcessExecOptions {
                cwd: temp_cwd(),
                env: None,
            },
        )
        .await
        .expect("git is available in the test environment");
        assert!(process.pid() > 0);
        assert_eq!(process.wait().await.unwrap(), 0);
        assert_eq!(process.exit_code(), Some(0));
    }

    #[tokio::test]
    async fn kill_terminates_a_running_child() {
        let mut process = BackendProcess::exec(
            &[
                "ping".to_string(),
                "-n".to_string(),
                "30".to_string(),
                "127.0.0.1".to_string(),
            ],
            ProcessExecOptions {
                cwd: temp_cwd(),
                env: None,
            },
        )
        .await
        .expect("ping is available on Windows");
        process.kill("SIGTERM").await.expect("terminate");
        assert_ne!(process.wait().await.unwrap(), 0);
        process.dispose().await;
    }

    #[tokio::test]
    async fn an_empty_command_is_refused() {
        let error = match BackendProcess::exec(
            &[],
            ProcessExecOptions {
                cwd: temp_cwd(),
                env: None,
            },
        )
        .await
        {
            Ok(_) => panic!("an empty command must be refused"),
            Err(error) => error,
        };
        assert!(error.contains("at least a program name"), "{error}");
    }
}
