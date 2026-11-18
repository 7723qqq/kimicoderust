use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use futures_util::{Stream, StreamExt};
use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::_base::utils::abort::{AbortController, abort_error};
use crate::app::config::config::IConfigService;
use crate::session::process::process_runner::{IProcess, ISessionProcessRunner, ProcessExecOptions};
use crate::wire::record::WireAsyncError;

use super::config_section::{AcpBackendConfig, SUBAGENT_BACKEND_SECTION, SubagentBackendConfig};
use super::subagent_backend::{
    ISubagentBackend, SubagentBackendName, SubagentBackendResult, SubagentBackendRun,
    SubagentBackendStartRequest, SubagentBackendStopReason,
};

pub const ACP_PROTOCOL_VERSION: u32 = 1;

pub struct AcpBackend {
    process_runner: Arc<dyn ISessionProcessRunner>,
    config: Option<AcpBackendConfig>,
}

impl AcpBackend {
    pub fn new(
        process_runner: Arc<dyn ISessionProcessRunner>,
        config: Arc<dyn IConfigService>,
    ) -> Self {
        let config = config
            .get(SUBAGENT_BACKEND_SECTION)
            .and_then(|value| serde_json::from_value::<SubagentBackendConfig>(value).ok())
            .and_then(|config| config.acp);
        Self {
            process_runner,
            config,
        }
    }

    pub async fn start(
        &self,
        request: SubagentBackendStartRequest,
    ) -> Result<SubagentBackendRun, WireAsyncError> {
        let Some(config) = self.config.clone() else {
            return Err("the acp subagent backend needs a server configured in the [subagentBackend] config section: `acp = { command = \"...\" }`".into());
        };
        let mut args = vec![config.command];
        args.extend(config.args.unwrap_or_default());
        let process = self
            .process_runner
            .exec(
                &args,
                Some(ProcessExecOptions {
                    cwd: Some(request.cwd.clone()),
                    env: config.env,
                }),
            )
            .await?;
        let controller = AbortController::new();
        let abort_from_signal = {
            let controller = controller.clone();
            let signal = request.signal.clone();
            Arc::new(move || {
                controller.abort(signal.reason());
            })
        };
        if request.signal.aborted() {
            abort_from_signal();
        }
        request
            .signal
            .add_event_listener("abort", abort_from_signal.clone(), true);
        let connection = Arc::new(AcpConnection::new(process.clone()));
        let run_connection = connection.clone();
        let cwd = request.cwd.clone();
        let prompt = request.prompt.clone();
        let run = async move {
            run_connection
                .request(
                    "initialize",
                    json!({
                        "protocolVersion": ACP_PROTOCOL_VERSION,
                        "clientCapabilities": {},
                    }),
                )
                .await?;
            let session = run_connection
                .request(
                    "session/new",
                    json!({ "cwd": cwd, "mcpServers": [] }),
                )
                .await?;
            let session_id = session
                .get("sessionId")
                .and_then(Value::as_str)
                .ok_or_else(|| -> WireAsyncError {
                    "ACP child published without a session id".into()
                })?
                .to_string();
            run_connection
                .request(
                    "session/prompt",
                    json!({
                        "sessionId": session_id,
                        "prompt": [{ "type": "text", "text": prompt }],
                    }),
                )
                .await
        };
        let output = connection.output.clone();
        let abort_signal = controller.signal();
        let result = Box::pin(async move {
            let (abort_sender, abort_receiver) = tokio::sync::oneshot::channel();
            let abort_sender = std::sync::Mutex::new(Some(abort_sender));
            abort_signal.add_event_listener(
                "abort",
                Arc::new(move || {
                    if let Some(sender) = abort_sender
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .take()
                    {
                        let _ = sender.send(());
                    }
                }),
                true,
            );
            tokio::pin!(run);
            tokio::select! {
                result = &mut run => {
                    match result {
                        Ok(result) => Ok(SubagentBackendResult {
                            output: take_output(&output),
                            stop_reason: acp_stop_reason(result.get("stopReason").and_then(Value::as_str)),
                        }),
                        Err(error) => {
                            if abort_signal.aborted() {
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
                _ = abort_receiver => {
                    Ok(SubagentBackendResult {
                        output: take_output(&output),
                        stop_reason: SubagentBackendStopReason::Aborted,
                    })
                }
            }
        });
        let dispose_controller = controller.clone();
        let dispose_signal = request.signal.clone();
        let dispose_process = process.clone();
        Ok(SubagentBackendRun {
            id: format!("acp-{}", uuid::Uuid::new_v4()),
            result,
            dispose: Arc::new(move || {
                dispose_controller.abort(Some(Arc::new(abort_error()) as crate::_base::errors::errors::SharedError));
                dispose_signal.remove_event_listener("abort", abort_from_signal.clone());
                let process = dispose_process.clone();
                Box::pin(async move {
                    let _ = process.kill("SIGTERM").await;
                    Ok(())
                })
            }),
        })
    }
}

#[async_trait]
impl ISubagentBackend for AcpBackend {
    fn name(&self) -> SubagentBackendName {
        SubagentBackendName::Acp
    }

    async fn start(
        &self,
        request: SubagentBackendStartRequest,
    ) -> Result<SubagentBackendRun, WireAsyncError> {
        AcpBackend::start(self, request).await
    }
}

pub fn acp_stop_reason(reason: Option<&str>) -> SubagentBackendStopReason {
    match reason {
        Some("end_turn") => SubagentBackendStopReason::Completed,
        Some("max_tokens") => SubagentBackendStopReason::MaxTokens,
        Some("refusal") => SubagentBackendStopReason::Refusal,
        Some("cancelled") => SubagentBackendStopReason::Aborted,
        _ => SubagentBackendStopReason::Error,
    }
}

struct AcpConnection {
    process: Arc<dyn IProcess>,
    reader: tokio::sync::Mutex<AcpReader>,
    next_id: AtomicU64,
    output: Arc<Mutex<String>>,
}

impl AcpConnection {
    fn new(process: Arc<dyn IProcess>) -> Self {
        Self {
            reader: tokio::sync::Mutex::new(AcpReader::new(process.stdout())),
            process,
            next_id: AtomicU64::new(0),
            output: Arc::new(Mutex::new(String::new())),
        }
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, WireAsyncError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        self.process
            .stdin_write(
                format!(
                    "{}\n",
                    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
                )
                .as_bytes(),
            )
            .await?;
        loop {
            let line = self.reader.lock().await.read_line().await?;
            let message: Value = serde_json::from_str(&line)?;
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(result) = message.get("result") {
                    return Ok(result.clone());
                }
                if let Some(error) = message.get("error") {
                    return Err(error.to_string().into());
                }
            }
            if message.get("id").is_some() {
                let method = message.get("method").and_then(Value::as_str).unwrap_or("");
                if matches!(method, "session/requestPermission" | "session/request_permission") {
                    self.process
                        .stdin_write(
                            format!(
                                "{}\n",
                                json!({
                                    "jsonrpc": "2.0",
                                    "id": message.get("id").cloned().unwrap_or(Value::Null),
                                    "result": { "outcome": { "outcome": "cancelled" } },
                                })
                            )
                            .as_bytes(),
                        )
                        .await?;
                    continue;
                }
            }
            if message.get("method").and_then(Value::as_str) == Some("session/update") {
                self.on_session_update(message.get("params").unwrap_or(&Value::Null));
            }
        }
    }

    fn on_session_update(&self, params: &Value) {
        let update = params.get("update").unwrap_or(&Value::Null);
        if update.get("sessionUpdate").and_then(Value::as_str) != Some("agent_message_chunk") {
            return;
        }
        let content = update.get("content").unwrap_or(&Value::Null);
        if content.get("type").and_then(Value::as_str) == Some("text")
            && let Some(text) = content.get("text").and_then(Value::as_str)
        {
            self.output.lock().push_str(text);
        }
    }
}

struct AcpReader {
    stream: Pin<Box<dyn Stream<Item = Result<String, WireAsyncError>> + Send>>,
    buffer: String,
}

impl AcpReader {
    fn new(stream: Pin<Box<dyn Stream<Item = Result<String, WireAsyncError>> + Send>>) -> Self {
        Self {
            stream,
            buffer: String::new(),
        }
    }

    async fn read_line(&mut self) -> Result<String, WireAsyncError> {
        loop {
            if let Some(newline) = self.buffer.find('\n') {
                let line = self.buffer[..newline].trim_end_matches('\r').to_string();
                self.buffer.drain(..=newline);
                return Ok(line);
            }
            let Some(chunk) = self.stream.next().await else {
                return Err("ACP child closed stdout".into());
            };
            self.buffer.push_str(&chunk?);
        }
    }
}

fn take_output(output: &Arc<Mutex<String>>) -> String {
    std::mem::take(&mut *output.lock())
}
