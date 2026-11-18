use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

use crate::_base::utils::abort::{AbortController, abort_error};
use crate::app::config::config::IConfigService;
use crate::wire::record::WireAsyncError;

use super::config_section::{
    ClaudeCodeBackendConfig, SUBAGENT_BACKEND_SECTION, SubagentBackendConfig,
};
use super::subagent_backend::{
    ISubagentBackend, SubagentBackendName, SubagentBackendResult, SubagentBackendRun,
    SubagentBackendStartRequest, SubagentBackendStopReason,
};

pub struct ClaudeCodeBackend {
    config: Option<ClaudeCodeBackendConfig>,
}

impl ClaudeCodeBackend {
    pub fn new(config: Arc<dyn IConfigService>) -> Self {
        let config = config
            .get(SUBAGENT_BACKEND_SECTION)
            .and_then(|value| serde_json::from_value::<SubagentBackendConfig>(value).ok())
            .and_then(|config| config.claude_code);
        Self { config }
    }

    pub async fn start(
        &self,
        request: SubagentBackendStartRequest,
    ) -> Result<SubagentBackendRun, WireAsyncError> {
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
        let config = self.config.clone().unwrap_or_default();
        let output = Arc::new(Mutex::new(String::new()));
        let child = Arc::new(tokio::sync::Mutex::new(None::<Child>));
        let request_cwd = request.cwd.clone();
        let request_prompt = request.prompt.clone();
        let result_controller = controller.clone();
        let result_output = output.clone();
        let result_child = child.clone();
        let result = Box::pin(async move {
            let (abort_sender, abort_receiver) = tokio::sync::oneshot::channel();
            let abort_sender = std::sync::Mutex::new(Some(abort_sender));
            let abort_signal = result_controller.signal();
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
            let run_output = result_output.clone();
            let run_child = result_child.clone();
            let run = async move {
                let command = config.command.unwrap_or_else(|| "claude".to_string());
                let mut args = vec![
                    "--print".to_string(),
                    "--output-format".to_string(),
                    "stream-json".to_string(),
                    "--verbose".to_string(),
                ];
                if let Some(model) = config.model {
                    args.push("--model".to_string());
                    args.push(model);
                }
                args.push(request_prompt);
                let mut spawned = Command::new(command)
                    .args(args)
                    .current_dir(&request_cwd)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()?;
                let stdout = spawned
                    .stdout
                    .take()
                    .ok_or_else(|| -> WireAsyncError {
                        "claude-code produced no output stream".into()
                    })?;
                *run_child.lock().await = Some(spawned);
                let mut lines = BufReader::new(stdout).lines();
                loop {
                    let Some(line) = lines.next_line().await? else {
                        return Ok(SubagentBackendResult {
                            output: take_output(&run_output),
                            stop_reason: SubagentBackendStopReason::Completed,
                        });
                    };
                    let event: Value = serde_json::from_str(&line)?;
                    match event.get("type").and_then(Value::as_str) {
                        Some("result") => {
                            return Ok(SubagentBackendResult {
                                output: take_output(&run_output),
                                stop_reason: if event
                                    .get("is_error")
                                    .and_then(Value::as_bool)
                                    .unwrap_or(false)
                                {
                                    SubagentBackendStopReason::Error
                                } else {
                                    SubagentBackendStopReason::Completed
                                },
                            });
                        }
                        Some("stream_event") => {
                            let delta = event.get("event").and_then(|event| event.get("delta"));
                            if event
                                .get("event")
                                .and_then(|event| event.get("type"))
                                .and_then(Value::as_str)
                                == Some("content_block_delta")
                                && delta
                                    .and_then(|delta| delta.get("type"))
                                    .and_then(Value::as_str)
                                    == Some("text_delta")
                                && let Some(text) = delta
                                    .and_then(|delta| delta.get("text"))
                                    .and_then(Value::as_str)
                            {
                                run_output.lock().push_str(text);
                            }
                        }
                        _ => {}
                    }
                }
            };
            tokio::pin!(run);
            tokio::select! {
                result = &mut run => {
                    match result {
                        Ok(result) => Ok(result),
                        Err(error) => {
                            if abort_signal.aborted() {
                                Ok(SubagentBackendResult {
                                    output: take_output(&result_output),
                                    stop_reason: SubagentBackendStopReason::Aborted,
                                })
                            } else {
                                Err(error)
                            }
                        }
                    }
                }
                _ = abort_receiver => {
                    if let Some(child) = result_child.lock().await.as_mut() {
                        let _ = child.kill().await;
                    }
                    Ok(SubagentBackendResult {
                        output: take_output(&result_output),
                        stop_reason: SubagentBackendStopReason::Aborted,
                    })
                }
            }
        });
        let dispose_controller = controller.clone();
        let dispose_signal = request.signal.clone();
        Ok(SubagentBackendRun {
            id: format!("claude-code-{}", uuid::Uuid::new_v4()),
            result,
            dispose: Arc::new(move || {
                dispose_controller.abort(Some(Arc::new(abort_error()) as crate::_base::errors::errors::SharedError));
                dispose_signal.remove_event_listener("abort", abort_from_signal.clone());
                Box::pin(async { Ok(()) })
            }),
        })
    }
}

#[async_trait]
impl ISubagentBackend for ClaudeCodeBackend {
    fn name(&self) -> SubagentBackendName {
        SubagentBackendName::ClaudeCode
    }

    async fn start(
        &self,
        request: SubagentBackendStartRequest,
    ) -> Result<SubagentBackendRun, WireAsyncError> {
        ClaudeCodeBackend::start(self, request).await
    }
}

fn take_output(output: &Arc<Mutex<String>>) -> String {
    std::mem::take(&mut *output.lock())
}
