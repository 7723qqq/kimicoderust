//! The `claude-code` external subagent backend, ported from v2
//! `session/subagent/backend/claudeCodeBackend.ts`.
//!
//! v2 drives the npm `@anthropic-ai/claude-agent-sdk`; that SDK is a wrapper over
//! the CLI's `--print --output-format stream-json` stream, which is what this
//! spawns. Text accumulates from `stream_event` content-block deltas and the
//! terminal `result` frame carries the outcome.

use std::sync::{Arc, Mutex as StdMutex};

use serde_json::Value;
use tokio::process::ChildStdout;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::rpc::types::BoxFuture;

use super::process::{BackendProcess, ProcessExecOptions, spawn_json_line_reader};
use super::subagent_backend::{
    SubagentBackend, SubagentBackendResult, SubagentBackendRun, SubagentBackendStartRequest,
    SubagentBackendStopReason,
};

/// v2 `ClaudeCodeBackendConfig`.
#[derive(Debug, Clone, Default)]
pub struct ClaudeCodeBackendConfig {
    pub command: Option<String>,
    pub model: Option<String>,
}

pub struct ClaudeCodeBackend {
    config: ClaudeCodeBackendConfig,
}

impl ClaudeCodeBackend {
    pub fn new(config: ClaudeCodeBackendConfig) -> Self {
        Self { config }
    }
}

impl SubagentBackend for ClaudeCodeBackend {
    fn name(&self) -> &'static str {
        "claude-code"
    }

    fn start<'a>(
        &'a self,
        request: SubagentBackendStartRequest,
    ) -> BoxFuture<'a, Result<SubagentBackendRun, String>> {
        Box::pin(async move { self.start_backend(request).await })
    }
}

impl ClaudeCodeBackend {
    async fn start_backend(
        &self,
        request: SubagentBackendStartRequest,
    ) -> Result<SubagentBackendRun, String> {
        let command = self
            .config
            .command
            .clone()
            .unwrap_or_else(|| "claude".to_string());
        let mut args = vec![
            command,
            "--print".to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--verbose".to_string(),
        ];
        if let Some(model) = &self.config.model {
            args.push("--model".to_string());
            args.push(model.clone());
        }
        args.push(request.prompt.clone());

        let mut process = BackendProcess::exec(
            &args,
            ProcessExecOptions {
                cwd: request.cwd.clone(),
                env: None,
            },
        )
        .await?;
        let stdout = process.stdout.take();
        let cancel = request.cancel.clone();
        let run_cancel = cancel.clone();
        let (sender, receiver) = oneshot::channel();
        tokio::spawn(async move {
            let _ = sender.send(run_claude(stdout, cancel).await);
        });

        Ok(SubagentBackendRun {
            id: format!("claude-code-{}", ulid::Ulid::new()),
            result: receiver,
            cancel: run_cancel,
            process: Arc::new(StdMutex::new(Some(process))),
            wire: None,
        })
    }
}

async fn run_claude(
    stdout: Option<ChildStdout>,
    cancel: CancellationToken,
) -> Result<SubagentBackendResult, String> {
    let Some(stdout) = stdout else {
        return Err("claude-code produced no output stream".to_string());
    };
    let mut frames = spawn_json_line_reader(stdout);
    let mut output = String::new();
    loop {
        let frame = tokio::select! {
            _ = cancel.cancelled() => {
                return Ok(SubagentBackendResult {
                    output,
                    stop_reason: SubagentBackendStopReason::Aborted,
                });
            }
            frame = frames.recv() => match frame {
                Some(frame) => frame,
                None => {
                    return Ok(SubagentBackendResult {
                        output,
                        stop_reason: SubagentBackendStopReason::Error,
                    });
                }
            },
        };
        match frame.get("type").and_then(Value::as_str) {
            Some("stream_event") => {
                if let Some(text) = frame.pointer("/event/delta/text").and_then(Value::as_str) {
                    output.push_str(text);
                }
            }
            Some("result") => {
                let reason = if frame
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    SubagentBackendStopReason::Error
                } else {
                    SubagentBackendStopReason::Completed
                };
                return Ok(SubagentBackendResult {
                    output,
                    stop_reason: reason,
                });
            }
            _ => {}
        }
    }
}
