use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::_base::utils::abort::{AbortController, abort_error};
use crate::app::config::config::IConfigService;
use crate::session::process::process_runner::{ISessionProcessRunner, ProcessExecOptions};
use crate::wire::record::WireAsyncError;

use super::codex_wire::CodexWire;
use super::config_section::{CodexBackendConfig, SUBAGENT_BACKEND_SECTION, SubagentBackendConfig};
use super::subagent_backend::{
    ISubagentBackend, SubagentBackendName, SubagentBackendResult, SubagentBackendRun,
    SubagentBackendStartRequest, SubagentBackendStopReason,
};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TurnUpdatedParams {
    thread_id: Option<Value>,
    turn_id: Option<Value>,
    text: Option<Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TurnCompleteParams {
    thread_id: Option<Value>,
    turn_id: Option<Value>,
    status: Option<Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApprovalRequestedParams {
    id: Option<Value>,
}

pub struct CodexBackend {
    process_runner: Arc<dyn ISessionProcessRunner>,
    config: Option<CodexBackendConfig>,
}

impl CodexBackend {
    pub fn new(
        process_runner: Arc<dyn ISessionProcessRunner>,
        config: Arc<dyn IConfigService>,
    ) -> Self {
        let config = config
            .get(SUBAGENT_BACKEND_SECTION)
            .and_then(|value| serde_json::from_value::<SubagentBackendConfig>(value).ok())
            .and_then(|config| config.codex);
        Self {
            process_runner,
            config,
        }
    }

    pub async fn start(
        &self,
        request: SubagentBackendStartRequest,
    ) -> Result<SubagentBackendRun, WireAsyncError> {
        let command = self
            .config
            .as_ref()
            .and_then(|config| config.command.clone())
            .unwrap_or_else(|| "codex".to_string());
        let process = self
            .process_runner
            .exec(
                &[command, "app-server".to_string(), "--stdio".to_string()],
                Some(ProcessExecOptions {
                    cwd: Some(request.cwd.clone()),
                    env: None,
                }),
            )
            .await?;
        let wire = CodexWire::new(process.clone());
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
        let model = self
            .config
            .as_ref()
            .and_then(|config| config.model.clone())
            .unwrap_or_else(|| "gpt-5".to_string());
        let output = Arc::new(Mutex::new(String::new()));
        let run_wire = wire.clone();
        let run_controller = controller.clone();
        let run_output = output.clone();
        let cwd = request.cwd.clone();
        let prompt = request.prompt.clone();
        let result = Box::pin(async move {
            let run = async move {
                run_wire
                    .request(
                        "initialize",
                        Some(json!({
                            "protocolVersion": 1,
                            "modelProvider": "openai",
                            "model": model,
                        })),
                        Some(run_controller.signal()),
                    )
                    .await?;
                let thread = run_wire
                    .request(
                        "startThread",
                        Some(json!({ "cwd": cwd })),
                        Some(run_controller.signal()),
                    )
                    .await?;
                let thread_id = thread
                    .get("threadId")
                    .and_then(Value::as_str)
                    .ok_or_else(|| -> WireAsyncError {
                        "codex startThread response did not include threadId".into()
                    })?
                    .to_string();
                let (complete_sender, complete_receiver) = tokio::sync::oneshot::channel::<bool>();
                let complete_sender = Arc::new(std::sync::Mutex::new(Some(complete_sender)));
                let updated_output = run_output.clone();
                run_wire.on_notification(
                    "turn/updated",
                    Arc::new(move |value| {
                        let params: TurnUpdatedParams = serde_json::from_value(value).unwrap_or_default();
                        if let Some(text) = params.text.and_then(|text| text.as_str().map(ToString::to_string)) {
                            updated_output.lock().push_str(&text);
                        }
                    }),
                );
                let complete = complete_sender.clone();
                run_wire.on_notification(
                    "turn/complete",
                    Arc::new(move |value| {
                        let params: TurnCompleteParams = serde_json::from_value(value).unwrap_or_default();
                        let completed = params
                            .status
                            .and_then(|status| status.as_str().map(ToString::to_string))
                            .as_deref()
                            == Some("completed");
                        if let Some(sender) = complete
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .take()
                        {
                            let _ = sender.send(completed);
                        }
                    }),
                );
                let approval_wire = Arc::downgrade(&run_wire);
                run_wire.on_notification(
                    "approval/requested",
                    Arc::new(move |value| {
                        let params: ApprovalRequestedParams = serde_json::from_value(value).unwrap_or_default();
                        let Some(id) = params.id.filter(|id| id.is_string() || id.is_number()) else {
                            return;
                        };
                        if let Some(wire) = approval_wire.upgrade() {
                            tokio::spawn(async move {
                                let _ = wire
                                    .notify(
                                        "approval/respond",
                                        Some(json!({ "id": id, "response": { "type": "cancel" } })),
                                    )
                                    .await;
                            });
                        }
                    }),
                );
                tokio::pin!(complete_receiver);
                let run_turn = run_wire.request(
                    "runTurn",
                    Some(json!({ "threadId": thread_id, "texts": [prompt] })),
                    Some(run_controller.signal()),
                );
                tokio::pin!(run_turn);
                let completed = tokio::select! {
                    biased;
                    completed = &mut complete_receiver => {
                        completed.map_err(|_| -> WireAsyncError { "codex turn completion channel closed".into() })?
                    }
                    result = &mut run_turn => {
                        result?;
                        (&mut complete_receiver).await.map_err(|_| -> WireAsyncError {
                            "codex turn completion channel closed".into()
                        })?
                    }
                };
                Ok(completed)
            }
            .await;
            match run {
                Ok(completed) => Ok(SubagentBackendResult {
                    output: take_output(&run_output),
                    stop_reason: if completed {
                        SubagentBackendStopReason::Completed
                    } else {
                        SubagentBackendStopReason::Error
                    },
                }),
                Err(error) => {
                    if run_controller.signal().aborted() {
                        Ok(SubagentBackendResult {
                            output: take_output(&run_output),
                            stop_reason: SubagentBackendStopReason::Aborted,
                        })
                    } else {
                        Err(error)
                    }
                }
            }
        });
        let dispose_controller = controller.clone();
        let dispose_signal = request.signal.clone();
        let dispose_wire = wire.clone();
        let dispose_process = process.clone();
        Ok(SubagentBackendRun {
            id: format!("codex-{}", uuid::Uuid::new_v4()),
            result,
            dispose: Arc::new(move || {
                dispose_controller.abort(Some(Arc::new(abort_error()) as crate::_base::errors::errors::SharedError));
                dispose_signal.remove_event_listener("abort", abort_from_signal.clone());
                dispose_wire.dispose();
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
impl ISubagentBackend for CodexBackend {
    fn name(&self) -> SubagentBackendName {
        SubagentBackendName::Codex
    }

    async fn start(
        &self,
        request: SubagentBackendStartRequest,
    ) -> Result<SubagentBackendRun, WireAsyncError> {
        CodexBackend::start(self, request).await
    }
}

fn take_output(output: &Arc<Mutex<String>>) -> String {
    std::mem::take(&mut *output.lock())
}
