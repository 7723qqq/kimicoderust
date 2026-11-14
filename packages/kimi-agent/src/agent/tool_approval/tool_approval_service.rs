use std::fmt::{Display, Formatter};
use std::sync::{Arc, LazyLock};

use futures_util::future::{BoxFuture, FutureExt};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::_base::errors::errors::SharedError;
use crate::_base::utils::abort::{abortable, is_user_cancellation, abort_error, user_cancellation_reason};
use crate::agent::interaction::approval::{ApprovalRequest, ApprovalResponse, ApprovalScope};
use crate::agent::permission_mode::permission_mode::IAgentPermissionModeService;
use crate::agent::permission_policy::types::{PermissionPolicyResolution, PermissionPolicyResult};
use crate::agent::permission_rules::permission_rules::{
    IAgentPermissionRulesService, PermissionApprovalResultRecord,
};
use crate::agent::scope_context::scope_context::IAgentScopeContext;
use crate::agent::tool_executor::before_tool_execute_event::deny_tool_execution;
use crate::agent::tool_executor::tool_hooks::{BeforeExecuteDecision, ResolvedToolExecutionHookContext};
use crate::app::bootstrap::bootstrap::IBootstrapService;
use crate::app::event::event2::{Event2, Event2Class};
use crate::app::telemetry::telemetry::ITelemetryService;
use crate::session::agent_lifecycle::agent_lifecycle::MAIN_AGENT_ID;
use crate::human::interaction::facade::interactions;
use crate::human::interaction::interaction::{
    INTERACTION_TAG_AGENT_ID, INTERACTION_TAG_SESSION_ID, INTERACTION_TAG_TOOL_CALL_ID,
    INTERACTION_TAG_TURN_ID, InteractionKind, InteractionRequest, InteractionTags,
};
use crate::session::session_context::session_context::ISessionContext;
use crate::state::event_dispatcher::IEventDispatcher;
use crate::tool::tool_input_display::ToolInputDisplay;
use crate::wire::record::WireAsyncError;

use super::tool_approval::IAgentToolApprovalService;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionApprovalRequestedPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub agent_id: String,
    pub turn_id: u64,
    pub tool_call_id: String,
    pub tool_name: String,
    pub action: String,
    pub display: ToolInputDisplay,
    pub tool_input: Value,
}

pub static PERMISSION_APPROVAL_REQUESTED_CLASS: LazyLock<Arc<Event2Class>> = LazyLock::new(|| {
    Arc::new(Event2Class {
        type_: "permission.approval.requested".to_string(),
        durable: false,
        observable: true,
        agent_domain: true,
        schema: None,
    })
});

#[derive(Debug, Clone, PartialEq)]
pub struct PermissionApprovalRequested {
    event: Event2,
}

impl PermissionApprovalRequested {
    pub const TYPE: &'static str = "permission.approval.requested";
    pub const OBSERVABLE: bool = true;

    pub fn new(payload: PermissionApprovalRequestedPayload) -> Self {
        Self {
            event: Event2::new(
                &PERMISSION_APPROVAL_REQUESTED_CLASS,
                serde_json::to_value(payload).expect("permission approval request is serializable"),
                None,
            ),
        }
    }

    pub fn class() -> Arc<Event2Class> {
        PERMISSION_APPROVAL_REQUESTED_CLASS.clone()
    }
}

impl From<PermissionApprovalRequested> for Event2 {
    fn from(event: PermissionApprovalRequested) -> Self {
        event.event
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionApprovalResolvedDecision {
    Approved,
    Rejected,
    Cancelled,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionApprovalResolvedPayload {
    #[serde(flatten)]
    pub request: PermissionApprovalRequestedPayload,
    pub decision: PermissionApprovalResolvedDecision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<ApprovalScope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub static PERMISSION_APPROVAL_RESOLVED_CLASS: LazyLock<Arc<Event2Class>> = LazyLock::new(|| {
    Arc::new(Event2Class {
        type_: "permission.approval.resolved".to_string(),
        durable: false,
        observable: true,
        agent_domain: true,
        schema: None,
    })
});

#[derive(Debug, Clone, PartialEq)]
pub struct PermissionApprovalResolved {
    event: Event2,
}

impl PermissionApprovalResolved {
    pub const TYPE: &'static str = "permission.approval.resolved";
    pub const OBSERVABLE: bool = true;

    pub fn new(payload: PermissionApprovalResolvedPayload) -> Self {
        Self {
            event: Event2::new(
                &PERMISSION_APPROVAL_RESOLVED_CLASS,
                serde_json::to_value(payload).expect("permission approval resolution is serializable"),
                None,
            ),
        }
    }

    pub fn class() -> Arc<Event2Class> {
        PERMISSION_APPROVAL_RESOLVED_CLASS.clone()
    }
}

impl From<PermissionApprovalResolved> for Event2 {
    fn from(event: PermissionApprovalResolved) -> Self {
        event.event
    }
}

#[derive(Clone)]
pub struct AgentToolApprovalService {
    scope_context: Arc<dyn IAgentScopeContext>,
    mode_service: Arc<dyn IAgentPermissionModeService>,
    rules_service: Arc<dyn IAgentPermissionRulesService>,
    session: Arc<dyn ISessionContext>,
    telemetry: Arc<dyn ITelemetryService>,
    dispatcher: Arc<dyn IEventDispatcher>,
    bootstrap: Arc<dyn IBootstrapService>,
}

impl AgentToolApprovalService {
    pub fn new(
        scope_context: Arc<dyn IAgentScopeContext>,
        mode_service: Arc<dyn IAgentPermissionModeService>,
        rules_service: Arc<dyn IAgentPermissionRulesService>,
        session: Arc<dyn ISessionContext>,
        telemetry: Arc<dyn ITelemetryService>,
        dispatcher: Arc<dyn IEventDispatcher>,
        bootstrap: Arc<dyn IBootstrapService>,
    ) -> Self {
        Self {
            scope_context,
            mode_service,
            rules_service,
            session,
            telemetry,
            dispatcher,
            bootstrap,
        }
    }

    fn resolve(
        &self,
        result: PermissionPolicyResolution,
        context: ResolvedToolExecutionHookContext,
        origin: String,
    ) -> BoxFuture<'static, Result<Option<BeforeExecuteDecision>, WireAsyncError>> {
        let this = self.clone();
        async move {
            match result {
                PermissionPolicyResolution::Approve {
                    execution_metadata, ..
                } => Ok(execution_metadata.map(|execution_metadata| {
                    BeforeExecuteDecision {
                        veto: None,
                        execution_metadata: Some(execution_metadata),
                    }
                })),
                PermissionPolicyResolution::Deny { message, .. } => {
                    let message = message.unwrap_or_else(|| {
                        format!(
                            "Tool \"{}\" was denied by permission policy.",
                            context.base.tool_call.name
                        )
                    });
                    Ok(Some(BeforeExecuteDecision {
                        veto: Some(deny_tool_execution(&this.format_deny(&message))),
                        execution_metadata: None,
                    }))
                }
                PermissionPolicyResolution::Ask {
                    reason,
                    resolve_approval,
                    resolve_error,
                    resolve_non_interactive,
                } => {
                    this.request(
                        context,
                        PermissionPolicyResult::Ask {
                            reason,
                            resolve_approval,
                            resolve_error,
                            resolve_non_interactive,
                        },
                        origin,
                    )
                    .await
                }
                PermissionPolicyResolution::Result { result } => {
                    Ok(Some(BeforeExecuteDecision {
                        veto: Some(result),
                        execution_metadata: None,
                    }))
                }
            }
        }
        .boxed()
    }

    fn request(
        &self,
        context: ResolvedToolExecutionHookContext,
        result: PermissionPolicyResult,
        origin: String,
    ) -> BoxFuture<'static, Result<Option<BeforeExecuteDecision>, WireAsyncError>> {
        let this = self.clone();
        async move {
            let PermissionPolicyResult::Ask {
                resolve_approval,
                resolve_error,
                resolve_non_interactive,
                ..
            } = result
            else {
                unreachable!("requestToolApproval requires an ask resolution")
            };
            let name = context.base.tool_call.name.clone();
            if this.bootstrap.args().non_interactive == Some(true) {
                let resolution = resolve_non_interactive.map(|resolve| resolve()).unwrap_or_else(
                    || PermissionPolicyResolution::Deny {
                        reason: None,
                        message: Some(format!(
                            "Tool \"{name}\" needs approval, but nobody can approve it in a non-interactive run."
                        )),
                    },
                );
                return this.resolve(resolution, context, origin).await;
            }
            let action = context
                .execution
                .description
                .clone()
                .unwrap_or_else(|| format!("Approve {name}"));
            let display = context.execution.display.clone().unwrap_or_else(|| {
                ToolInputDisplay::Generic {
                    summary: action.clone(),
                    detail: Some(context.base.args.clone()),
                }
            });
            let id = format!("approval_{}", uuid::Uuid::new_v4());
            let session_id = this.session.session_id();
            let agent_id = this.scope_context.agent_id();
            let turn_id = context.base.turn_id;
            let tool_call_id = context.base.tool_call.id.clone();
            let approval_request = ApprovalRequest {
                id: Some(id.clone()),
                session_id: Some(session_id.clone()),
                agent_id: Some(agent_id.clone()),
                turn_id: Some(turn_id),
                tool_call_id: Some(tool_call_id.clone()),
                tool_name: name.clone(),
                action: action.clone(),
                display: display.clone(),
            };
            let approval_context = PermissionApprovalRequestedPayload {
                id: Some(id.clone()),
                session_id: Some(session_id.clone()),
                agent_id: agent_id.clone(),
                turn_id,
                tool_call_id: tool_call_id.clone(),
                tool_name: name.clone(),
                action: action.clone(),
                display: display.clone(),
                tool_input: context.base.args.clone(),
            };
            let started_at = chrono::Utc::now().timestamp_millis();
            let mut tags = InteractionTags::new();
            tags.insert(INTERACTION_TAG_AGENT_ID.to_string(), agent_id.into());
            tags.insert(INTERACTION_TAG_SESSION_ID.to_string(), session_id.into());
            tags.insert(INTERACTION_TAG_TURN_ID.to_string(), turn_id.into());
            tags.insert(
                INTERACTION_TAG_TOOL_CALL_ID.to_string(),
                tool_call_id.into(),
            );
            dispatch_event(
                &this.dispatcher,
                PermissionApprovalRequested::new(approval_context.clone()).into(),
            );
            let request = InteractionRequest {
                id: Some(id),
                kind: InteractionKind::Approval,
                payload: serde_json::to_value(approval_request)
                    .expect("approval interaction payload is serializable"),
                tags: Some(tags),
            };
            let signal = context.base.signal.clone();
            let interaction_facade = interactions();
            let wait = async move { interaction_facade.request(request).await };
            let attempt = match abortable(wait, signal.clone()).await {
                Ok(Ok(response)) => Ok(response),
                Ok(Err(error)) => Err(Arc::from(error) as SharedError),
                Err(error) => Err(error),
            };
            let attempt = match attempt {
                Ok(response) if signal.aborted() => Err(signal
                    .reason()
                    .unwrap_or_else(|| Arc::new(abort_error()) as SharedError)),
                attempt => attempt,
            };
            let attempt = attempt.and_then(|response| {
                serde_json::from_value::<ApprovalResponse>(response)
                    .map_err(|error| Arc::new(error) as SharedError)
            });
            let response = match attempt {
                Ok(response) => response,
                Err(error) => {
                    if is_user_cancellation(error.as_ref()) {
                        return Err(shared_to_wire(error));
                    }
                    this.track_approval(
                        &context,
                        &origin,
                        &display,
                        started_at,
                        "error",
                        false,
                        false,
                    );
                    dispatch_event(
                        &this.dispatcher,
                        PermissionApprovalResolved::new(PermissionApprovalResolvedPayload {
                            request: approval_context.clone(),
                            decision: PermissionApprovalResolvedDecision::Error,
                            scope: None,
                            feedback: None,
                            selected_label: None,
                            error: Some(error.to_string()),
                        })
                        .into(),
                    );
                    if let Some(resolve_error) = resolve_error
                        && let Some(resolved) = resolve_error(error.clone())
                    {
                        return this.resolve(resolved, context, origin).await;
                    }
                    return Err(shared_to_wire(error));
                }
            };
            let approved_for_session = response.decision
                == crate::agent::interaction::approval::ApprovalDecision::Approved
                && response.scope == Some(ApprovalScope::Session);
            let session_approval_rule = approved_for_session
                .then(|| context.execution.approval_rule.clone());
            dispatch_event(
                &this.dispatcher,
                PermissionApprovalResolved::new(PermissionApprovalResolvedPayload {
                    request: approval_context,
                    decision: match response.decision {
                        crate::agent::interaction::approval::ApprovalDecision::Approved => {
                            PermissionApprovalResolvedDecision::Approved
                        }
                        crate::agent::interaction::approval::ApprovalDecision::Rejected => {
                            PermissionApprovalResolvedDecision::Rejected
                        }
                        crate::agent::interaction::approval::ApprovalDecision::Cancelled => {
                            PermissionApprovalResolvedDecision::Cancelled
                        }
                    },
                    scope: response.scope,
                    feedback: response.feedback.clone(),
                    selected_label: response.selected_label.clone(),
                    error: None,
                })
                .into(),
            );
            this.rules_service.record_approval_result(PermissionApprovalResultRecord {
                turn_id: context.base.turn_id,
                tool_call_id: context.base.tool_call.id.clone(),
                tool_name: name.clone(),
                action,
                session_approval_rule: session_approval_rule.clone(),
                result: response.clone(),
            });
            this.track_approval(
                &context,
                &origin,
                &display,
                started_at,
                if approved_for_session {
                    "approved_for_session"
                } else {
                    approval_decision_name(&response)
                },
                session_approval_rule.is_some(),
                response
                    .feedback
                    .as_deref()
                    .is_some_and(|feedback| !feedback.is_empty()),
            );
            if let Some(resolve_approval) = resolve_approval
                && let Some(resolved) = resolve_approval(response.clone())
            {
                return this.resolve(resolved, context, origin).await;
            }
            if response.decision == crate::agent::interaction::approval::ApprovalDecision::Approved {
                return Ok(None);
            }
            Ok(Some(BeforeExecuteDecision {
                veto: Some(deny_tool_execution(
                    &this.format_approval_rejection(&name, &response),
                )),
                execution_metadata: None,
            }))
        }
        .boxed()
    }

    fn format_approval_rejection(&self, tool_name: &str, result: &ApprovalResponse) -> String {
        let suffix = result
            .feedback
            .as_deref()
            .filter(|feedback| !feedback.is_empty())
            .map(|feedback| format!(" Reason: {feedback}"))
            .unwrap_or_default();
        let prefix = if result.decision
            == crate::agent::interaction::approval::ApprovalDecision::Cancelled
        {
            format!(
                "Tool \"{tool_name}\" was not run because the approval request was cancelled."
            )
        } else {
            format!(
                "Tool \"{tool_name}\" was not run because the user rejected the approval request."
            )
        };
        if self.uses_worker_rejection_guidance() {
            format!(
                "{prefix}{suffix} Try a different approach — don't retry the same call, don't attempt to bypass the restriction."
            )
        } else {
            format!("{prefix}{suffix}")
        }
    }

    fn format_deny(&self, message: &str) -> String {
        if self.uses_worker_rejection_guidance() {
            format!(
                "{message} Try a different approach — don't retry the same call, don't attempt to bypass the restriction."
            )
        } else {
            message.to_string()
        }
    }

    fn uses_worker_rejection_guidance(&self) -> bool {
        self.scope_context.agent_id() != MAIN_AGENT_ID
    }

    fn track_approval(
        &self,
        context: &ResolvedToolExecutionHookContext,
        origin: &str,
        display: &ToolInputDisplay,
        started_at: i64,
        result: &str,
        session_cache_written: bool,
        has_feedback: bool,
    ) {
        let mut properties = telemetry_properties(json!({
            "turn_id": context.base.turn_id,
            "tool_call_id": context.base.tool_call.id,
            "policy_name": origin,
            "tool_name": context.base.tool_call.name,
            "permission_mode": self.mode_service.mode(),
            "result": result,
            "approval_surface": display_kind(display),
            "duration_ms": chrono::Utc::now().timestamp_millis() - started_at,
            "session_cache_written": session_cache_written,
            "has_feedback": has_feedback,
        }));
        if let Some(trace_id) = context
            .base
            .trace
            .as_ref()
            .and_then(|trace| trace.trace_id.clone())
        {
            properties.insert("trace_id".to_string(), Value::String(trace_id));
        }
        self.telemetry
            .track2("permission_approval_result", Some(properties));
    }
}

impl IAgentToolApprovalService for AgentToolApprovalService {
    fn resolve_permission_resolution(
        &self,
        result: PermissionPolicyResolution,
        context: ResolvedToolExecutionHookContext,
        origin: String,
    ) -> BoxFuture<'static, Result<Option<BeforeExecuteDecision>, WireAsyncError>> {
        self.resolve(result, context, origin)
    }

    fn request_tool_approval(
        &self,
        context: ResolvedToolExecutionHookContext,
        result: PermissionPolicyResult,
        origin: String,
    ) -> BoxFuture<'static, Result<Option<BeforeExecuteDecision>, WireAsyncError>> {
        self.request(context, result, origin)
    }

    fn format_deny_message(&self, message: &str) -> String {
        self.format_deny(message)
    }

    fn format_approval_rejection_message(
        &self,
        tool_name: &str,
        result: &ApprovalResponse,
    ) -> String {
        self.format_approval_rejection(tool_name, result)
    }
}

fn dispatch_event(dispatcher: &Arc<dyn IEventDispatcher>, event: Event2) {
    let dispatcher = dispatcher.clone();
    tokio::spawn(async move {
        let _ = dispatcher.dispatch(event).await;
    });
}

fn display_kind(display: &ToolInputDisplay) -> String {
    serde_json::to_value(display)
        .ok()
        .and_then(|value| value.get("kind").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_default()
}

fn approval_decision_name(response: &ApprovalResponse) -> &'static str {
    match response.decision {
        crate::agent::interaction::approval::ApprovalDecision::Approved => "approved",
        crate::agent::interaction::approval::ApprovalDecision::Rejected => "rejected",
        crate::agent::interaction::approval::ApprovalDecision::Cancelled => "cancelled",
    }
}

fn telemetry_properties(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

struct ApprovalError(SharedError);

impl std::fmt::Debug for ApprovalError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0.to_string())
    }
}

impl Display for ApprovalError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        Display::fmt(&self.0, formatter)
    }
}

impl std::error::Error for ApprovalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

fn shared_to_wire(error: SharedError) -> WireAsyncError {
    if is_user_cancellation(error.as_ref()) {
        Box::new(user_cancellation_reason())
    } else {
        Box::new(ApprovalError(error))
    }
}
