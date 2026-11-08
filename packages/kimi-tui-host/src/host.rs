//! Host callbacks for a standalone terminal session.
//!
//! The engine asks its host for five things it cannot answer itself. Three of
//! them have a real answer here, and the other two are refusals with a reason —
//! a refusal the model can read is better than a call that blocks a terminal
//! nobody is watching.

use std::sync::Arc;

use kimi_agent::callbacks::HostCallbacks;
use kimi_agent::rpc::types::{
    AskQuestionRequest, AskQuestionResponse, BoxFuture, LlmChatRequest, LlmChatResponse,
    PermissionCheckRequest, PermissionDecision, StateReadRequest, StateReadResponse,
    StateWriteRequest, StateWriteResponse, ToolExecuteRequest, ToolExecuteResponse,
};
use kimi_agent::storage::StateStore;

/// A host with no UI dialogs and no host-side tool runtime.
pub struct TuiHostCallbacks {
    state_store: Arc<StateStore>,
    ui: Arc<kimi_tui_app::UiState>,
}

impl TuiHostCallbacks {
    pub fn new(state_store: Arc<StateStore>, ui: Arc<kimi_tui_app::UiState>) -> Self {
        Self { state_store, ui }
    }
}

impl HostCallbacks for TuiHostCallbacks {
    /// The session runs its own native transport; this seam would proxy a
    /// second LLM, and there is none.
    fn llm_chat(
        &self,
        _req: LlmChatRequest,
    ) -> BoxFuture<'static, Result<LlmChatResponse, String>> {
        Box::pin(async { Err("this host runs the engine's native transport".into()) })
    }

    /// Every tool the engine executes natively is served before this seam is
    /// reached; what arrives here is a tool this host does not have.
    fn execute_tool(
        &self,
        req: ToolExecuteRequest,
    ) -> BoxFuture<'static, Result<ToolExecuteResponse, String>> {
        let tool_name = req.tool_name;
        Box::pin(async move {
            Err(format!(
                "Tool \"{tool_name}\" is not available in this session. The native tool set runs in-process; do not retry a host-owned tool."
            ))
        })
    }

    /// Allowed without asking. The slice has no approval dialog yet, and a
    /// session that cannot answer a permission question at all would be
    /// unusable; ROADMAP §21 records the gap.
    fn check_permission(
        &self,
        req: PermissionCheckRequest,
    ) -> BoxFuture<'static, Result<PermissionDecision, String>> {
        let ui = Arc::clone(&self.ui);
        Box::pin(async move {
            let (tx, rx) = tokio::sync::oneshot::channel();
            let approval = kimi_tui_app::PendingApproval::new(
                req.tool_name.clone(),
                format!("run {}", req.tool_name),
                req.reason.clone(),
                req.arguments.clone(),
                req.session_approval_rule.clone(),
                move |response| {
                    let decision = if response.allowed {
                        PermissionDecision {
                            decision: "allow".into(),
                            reason: response.reason,
                            remember: response.remember,
                        }
                    } else {
                        PermissionDecision::deny(
                            response
                                .reason
                                .unwrap_or_else(|| "User denied tool execution".into()),
                        )
                    };
                    let _ = tx.send(decision);
                },
            );
            ui.set_pending_approval(Arc::new(approval));
            ui.set_status(Some("Waiting for approval (y/a/n, Esc cancel)".into()));
            let decision = rx
                .await
                .unwrap_or_else(|_| PermissionDecision::deny("approval channel closed"));
            ui.clear_pending_approval();
            ui.set_status(None);
            Ok(decision)
        })
    }

    /// There is no question dialog on the terminal yet, and reading stdin from
    /// a tool would fight the UI for the same file descriptor.
    fn ask_question(
        &self,
        request: AskQuestionRequest,
    ) -> BoxFuture<'static, Result<AskQuestionResponse, String>> {
        let ui = Arc::clone(&self.ui);
        Box::pin(async move {
            let questions: Vec<String> = request
                .questions
                .iter()
                .map(|q| q.question.clone())
                .collect();
            let options: Vec<Vec<String>> = request
                .questions
                .iter()
                .map(|q| q.options.iter().map(|o| o.label.clone()).collect())
                .collect();
            let (tx, rx) = tokio::sync::oneshot::channel();
            let question =
                kimi_tui_app::PendingQuestion::new(questions.clone(), options, move |response| {
                    let resp = AskQuestionResponse {
                        answers: response.answers,
                        method: response.method,
                        note: None,
                        cancelled: Some(response.cancelled),
                        reason: if response.cancelled {
                            Some("user_cancelled".into())
                        } else {
                            None
                        },
                    };
                    let _ = tx.send(resp);
                });
            ui.set_pending_question(Arc::new(question));
            ui.set_status(Some("Answer question (1-9, Enter, Esc)".into()));
            let response = rx
                .await
                .map_err(|_| "question channel closed".to_string())?;
            ui.clear_pending_question();
            ui.set_status(None);
            Ok(response)
        })
    }

    fn state_read(
        &self,
        request: StateReadRequest,
    ) -> BoxFuture<'static, Result<StateReadResponse, String>> {
        let store = Arc::clone(&self.state_store);
        Box::pin(async move {
            let value = store.read_state(&request.domain, &request.key)?;
            Ok(StateReadResponse { value })
        })
    }

    /// The domain's own write semantics, then the persist step they leave to
    /// the caller — the same pair the REPL and the engine's own host serve.
    fn state_write(
        &self,
        request: StateWriteRequest,
    ) -> BoxFuture<'static, Result<StateWriteResponse, String>> {
        let store = Arc::clone(&self.state_store);
        Box::pin(async move {
            let outcome = store.apply_write(&request.domain, &request.value)?;
            store.write_domain(&request.domain, &outcome.stored)?;
            Ok(StateWriteResponse {
                ok: true,
                value: outcome.response,
            })
        })
    }
}
