//! The `acp` external subagent backend, ported from v2
//! `session/subagent/backend/acpBackend.ts`.
//!
//! v2 drives `@agentclientprotocol/sdk`; that SDK is a thin JSON-RPC client over
//! ndjson stdio, the same transport as [`super::codex_wire`], so the backend
//! speaks `initialize` / `session/new` / `session/prompt` on it and reads
//! `session/update` chunks for the text.

use std::collections::BTreeMap;
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

pub const ACP_PROTOCOL_VERSION: u32 = 1;

/// v2 `AcpBackendConfig`. Unlike the other two, an ACP server has no default
/// command: without one the backend refuses to start.
#[derive(Debug, Clone)]
pub struct AcpBackendConfig {
    pub command: String,
    pub args: Option<Vec<String>>,
    pub env: Option<BTreeMap<String, String>>,
}

pub struct AcpBackend {
    config: Option<AcpBackendConfig>,
}

impl AcpBackend {
    pub fn new(config: Option<AcpBackendConfig>) -> Self {
        Self { config }
    }
}

impl SubagentBackend for AcpBackend {
    fn name(&self) -> &'static str {
        "acp"
    }

    fn start<'a>(
        &'a self,
        request: SubagentBackendStartRequest,
    ) -> BoxFuture<'a, Result<SubagentBackendRun, String>> {
        Box::pin(async move { self.start_backend(request).await })
    }
}

impl AcpBackend {
    async fn start_backend(
        &self,
        request: SubagentBackendStartRequest,
    ) -> Result<SubagentBackendRun, String> {
        let Some(config) = &self.config else {
            return Err(
                "the acp subagent backend needs a server configured in the [subagentBackend] config section: `acp = { command = \"...\" }`"
                    .to_string(),
            );
        };
        let mut args = vec![config.command.clone()];
        if let Some(extra) = &config.args {
            args.extend(extra.iter().cloned());
        }
        let mut process = BackendProcess::exec(
            &args,
            ProcessExecOptions {
                cwd: request.cwd.clone(),
                env: config.env.clone(),
            },
        )
        .await?;
        let stdout = process.stdout.take();
        let stdin = Arc::new(tokio::sync::Mutex::new(process.stdin.take()));
        let wire = Arc::new(JsonRpcWire::new(stdout, stdin));

        let cancel = request.cancel.clone();
        let run_cancel = cancel.clone();
        let (sender, receiver) = oneshot::channel();
        let task_wire = Arc::clone(&wire);
        let cwd = request.cwd.clone();
        let prompt = request.prompt.clone();
        tokio::spawn(async move {
            let _ = sender.send(run_acp(task_wire, cwd, prompt, cancel).await);
        });

        Ok(SubagentBackendRun {
            id: format!("acp-{}", ulid::Ulid::new()),
            result: receiver,
            cancel: run_cancel,
            process: Arc::new(StdMutex::new(Some(process))),
            wire: Some(wire),
        })
    }
}

async fn run_acp(
    wire: Arc<JsonRpcWire>,
    cwd: String,
    prompt: String,
    cancel: CancellationToken,
) -> Result<SubagentBackendResult, String> {
    let output = Arc::new(StdMutex::new(String::new()));
    let sink = Arc::clone(&output);
    wire.on_notification(
        "session/update",
        Arc::new(move |params| {
            if let Some(text) = params.pointer("/update/content/text").and_then(Value::as_str) {
                sink.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push_str(text);
            }
        }),
    );

    let initialize = async {
        wire.request(
            "initialize",
            json!({
                "protocolVersion": ACP_PROTOCOL_VERSION,
                "clientCapabilities": {},
            }),
            Some(&cancel),
        )
        .await?;
        let session = wire
            .request(
                "session/new",
                json!({ "cwd": cwd, "mcpServers": [] }),
                Some(&cancel),
            )
            .await?;
        let session_id = session
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| "ACP child published without a session id".to_string())?
            .to_string();
        wire.request(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": prompt }],
            }),
            Some(&cancel),
        )
        .await
    };

    match initialize.await {
        Ok(result) => Ok(SubagentBackendResult {
            output: take_output(&output),
            stop_reason: acp_stop_reason(result.get("stopReason").and_then(Value::as_str)),
        }),
        Err(error) => {
            if cancel.is_cancelled() {
                Ok(SubagentBackendResult {
                    output: take_output(&output),
                    stop_reason: SubagentBackendStopReason::Aborted,
                })
            } else {
                Err(error)
            }
        }
    }
}

/// v2 `acpStopReason`.
pub fn acp_stop_reason(reason: Option<&str>) -> SubagentBackendStopReason {
    match reason {
        Some("end_turn") => SubagentBackendStopReason::Completed,
        Some("max_tokens") => SubagentBackendStopReason::MaxTokens,
        Some("refusal") => SubagentBackendStopReason::Refusal,
        Some("cancelled") => SubagentBackendStopReason::Aborted,
        _ => SubagentBackendStopReason::Error,
    }
}

fn take_output(buffer: &Arc<StdMutex<String>>) -> String {
    std::mem::take(&mut *buffer.lock().unwrap_or_else(|poisoned| poisoned.into_inner()))
}
