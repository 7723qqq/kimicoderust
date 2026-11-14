//! The `codex` external subagent backend, ported from v2
//! `session/subagent/backend/codexBackend.ts`.
//!
//! v2 drives `codex app-server --stdio` over its JSON-RPC transport and lets the
//! npm process runner own the child; here the child comes from
//! [`crate::subagent::backend::process`] and the wire from [`super::codex_wire`].

use std::sync::{Arc, Mutex as StdMutex};

use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::rpc::types::BoxFuture;

use super::codex_wire::JsonRpcWire;
use super::process::{BackendProcess, ProcessExecOptions};
use super::subagent_backend::{
    SubagentBackend, SubagentBackendResult, SubagentBackendRun, SubagentBackendStartRequest,
    SubagentBackendStopReason,
};

pub const SUBAGENT_BACKEND_SECTION: &str = "subagentBackend";

/// v2 `CodexBackendConfig`.
#[derive(Debug, Clone, Default)]
pub struct CodexBackendConfig {
    pub command: Option<String>,
    pub model: Option<String>,
}

pub struct CodexBackend {
    config: CodexBackendConfig,
}

impl CodexBackend {
    pub fn new(config: CodexBackendConfig) -> Self {
        Self { config }
    }
}

impl SubagentBackend for CodexBackend {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn start<'a>(
        &'a self,
        request: SubagentBackendStartRequest,
    ) -> BoxFuture<'a, Result<SubagentBackendRun, String>> {
        Box::pin(async move { self.start_backend(request).await })
    }
}

impl CodexBackend {
    async fn start_backend(
        &self,
        request: SubagentBackendStartRequest,
    ) -> Result<SubagentBackendRun, String> {
        let command = self
            .config
            .command
            .clone()
            .unwrap_or_else(|| "codex".to_string());
        let model = self
            .config
            .model
            .clone()
            .unwrap_or_else(|| "gpt-5".to_string());
        let mut process = BackendProcess::exec(
            &[
                command,
                "app-server".to_string(),
                "--stdio".to_string(),
            ],
            ProcessExecOptions {
                cwd: request.cwd.clone(),
                env: None,
            },
        )
        .await?;
        let stdout = process.stdout.take();
        let stdin = Arc::new(tokio::sync::Mutex::new(process.stdin.take()));
        let wire = Arc::new(JsonRpcWire::new(stdout, stdin));

        let SubagentBackendStartRequest {
            prompt,
            cwd,
            cancel,
        } = request;
        let run_cancel = cancel.clone();
        let (sender, receiver) = oneshot::channel();
        let task_wire = Arc::clone(&wire);
        tokio::spawn(async move {
            let _ = sender.send(run_codex(task_wire, model, prompt, cwd, cancel).await);
        });

        Ok(SubagentBackendRun {
            id: format!("codex-{}", ulid::Ulid::new()),
            result: receiver,
            cancel: run_cancel,
            process: Arc::new(StdMutex::new(Some(process))),
            wire: Some(wire),
        })
    }
}

async fn run_codex(
    wire: Arc<JsonRpcWire>,
    model: String,
    prompt: String,
    cwd: String,
    cancel: CancellationToken,
) -> Result<SubagentBackendResult, String> {
    let (done_tx, done_rx) = oneshot::channel::<SubagentBackendStopReason>();
    let done_tx = Arc::new(StdMutex::new(Some(done_tx)));
    let output = Arc::new(StdMutex::new(String::new()));

    let sink = Arc::clone(&output);
    wire.on_notification(
        "turn/updated",
        Arc::new(move |params| {
            if let Some(text) = params.get("text").and_then(Value::as_str) {
                sink.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push_str(text);
            }
        }),
    );
    let done_for_complete = Arc::clone(&done_tx);
    wire.on_notification(
        "turn/complete",
        Arc::new(move |params| {
            let reason = match params.get("status").and_then(Value::as_str) {
                Some("completed") => SubagentBackendStopReason::Completed,
                _ => SubagentBackendStopReason::Error,
            };
            if let Some(sender) = done_for_complete
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
            {
                let _ = sender.send(reason);
            }
        }),
    );
    let approval_wire = Arc::clone(&wire);
    wire.on_notification(
        "approval/requested",
        Arc::new(move |params| {
            let id = params.get("id").cloned().unwrap_or(Value::Null);
            let wire = Arc::clone(&approval_wire);
            tokio::spawn(async move {
                wire.notify(
                    "approval/respond",
                    json!({ "id": id, "response": { "type": "cancel" } }),
                )
                .await;
            });
        }),
    );

    let initialize = async {
        wire.request(
            "initialize",
            json!({
                "protocolVersion": 1,
                "modelProvider": "openai",
                "model": model,
            }),
            Some(&cancel),
        )
        .await?;
        let thread = wire
            .request("startThread", json!({ "cwd": cwd }), Some(&cancel))
            .await?;
        let thread_id = thread
            .get("threadId")
            .and_then(Value::as_str)
            .ok_or_else(|| "codex startThread returned no threadId".to_string())?
            .to_string();
        wire.request(
            "runTurn",
            json!({ "threadId": thread_id, "texts": [prompt] }),
            Some(&cancel),
        )
        .await
    };

    if let Err(error) = initialize.await {
        if cancel.is_cancelled() {
            return Ok(SubagentBackendResult {
                output: take_output(&output),
                stop_reason: SubagentBackendStopReason::Aborted,
            });
        }
        return Err(error);
    }

    let stop_reason = done_rx
        .await
        .unwrap_or(SubagentBackendStopReason::Error);
    Ok(SubagentBackendResult {
        output: take_output(&output),
        stop_reason,
    })
}

fn take_output(buffer: &Arc<StdMutex<String>>) -> String {
    std::mem::take(&mut *buffer.lock().unwrap_or_else(|poisoned| poisoned.into_inner()))
}
