use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::{Map, Value};

use crate::agent::permission_mode::permission_mode::{IAgentPermissionModeService, PermissionMode};
use crate::agent::scope_context::scopeContext::IAgentScopeContext;
use crate::agent::tool_executor::tool_executor::{
    ExecutableTool, ExecutableToolResult, RunnableToolExecution, ToolExecution,
};
use crate::app::bootstrap::bootstrap::IBootstrapService;
use crate::features::tower::protocol::paths::TOWER_NAME;
use crate::features::tower::protocol::store::TowerReviewInput;
use crate::features::tower::protocol::types::{TowerMissionKind, TowerMissionStatus};
use crate::features::tower::tools::delivery::{TowerDeliveryHost, deliver_tower_message};
use crate::features::tower::tools::support::{new_tower_store, run_tower_tool};
use crate::session::sessionContext::sessionContext::ISessionContext;
use crate::wire::record::WireAsyncError;

use super::review::{TowerReviewToolInput, tower_review_tool_input_schema};

pub const TOWER_REVIEW_TOOL_NAME: &str = "TowerReview";

#[derive(Clone)]
pub struct TowerReviewTool {
    session_context: Arc<dyn ISessionContext>,
    scope_context: Arc<dyn IAgentScopeContext>,
    delivery: Arc<dyn TowerDeliveryHost>,
    permission_mode: Arc<dyn IAgentPermissionModeService>,
    bootstrap: Arc<dyn IBootstrapService>,
}

impl TowerReviewTool {
    pub fn new(
        session_context: Arc<dyn ISessionContext>,
        scope_context: Arc<dyn IAgentScopeContext>,
        delivery: Arc<dyn TowerDeliveryHost>,
        permission_mode: Arc<dyn IAgentPermissionModeService>,
        bootstrap: Arc<dyn IBootstrapService>,
    ) -> Self {
        Self {
            session_context,
            scope_context,
            delivery,
            permission_mode,
            bootstrap,
        }
    }

    pub fn name(&self) -> &'static str {
        TOWER_REVIEW_TOOL_NAME
    }

    pub fn side_effect(&self) -> &'static str {
        "read"
    }

    pub fn repo_mutation(&self) -> &'static str {
        "none"
    }

    pub fn description(&self) -> &'static str {
        include_str!("review.md")
    }

    pub fn parameters(&self) -> Map<String, Value> {
        tower_review_tool_input_schema()
            .as_object()
            .cloned()
            .expect("TowerReview input schema is an object")
    }

    pub fn resolve_execution_value(&self, input: Value) -> ToolExecution {
        match serde_json::from_value::<TowerReviewToolInput>(input) {
            Ok(args) => self.resolve_execution(args),
            Err(error) => ToolExecution::Error(ExecutableToolResult::error(format!(
                "Invalid arguments for TowerReview: {error}"
            ))),
        }
    }

    pub fn resolve_execution(&self, args: TowerReviewToolInput) -> ToolExecution {
        let description = format!(
            "Submitting tower review for {}: {}",
            args.target, args.status
        );
        let this = self.clone();
        ToolExecution::Runnable(RunnableToolExecution {
            accesses: None,
            display: None,
            description: Some(description),
            stop_batch_after_this: None,
            approval_rule: TOWER_REVIEW_TOOL_NAME.to_string(),
            matches_rule: None,
            execute: Arc::new(move |_context| {
                let this = this.clone();
                let args = args.clone();
                Box::pin(async move { Ok(this.execute(args).await) })
            }),
        })
    }

    async fn execute(&self, args: TowerReviewToolInput) -> ExecutableToolResult {
        run_tower_tool(async move {
            let store = new_tower_store(&self.session_context.cwd());
            let state = store.load().await?;
            let caller = store.resolve_caller_name(&state, &self.scope_context.agent_id())?;
            let artifact_mission = state
                .missions
                .iter()
                .find(|mission| {
                    mission.branch == args.target
                        && mission.kind == TowerMissionKind::Artifact
                        && mission.status != TowerMissionStatus::Merged
                        && mission.status != TowerMissionStatus::Abandoned
                })
                .cloned();
            let automatic_acceptance = self.bootstrap.args().non_interactive == Some(true)
                || self.permission_mode.mode() == PermissionMode::Auto;
            if caller == TOWER_NAME
                && args.status == "clean"
                && args.merge == "merge"
                && automatic_acceptance
            {
                if let Some(mission) = &artifact_mission {
                    return Ok(ExecutableToolResult::error(format!(
                        "artifact acceptance for {} cannot be recorded in an automatic or noninteractive run — leave mission {} completed and waiting for explicit human approval in an interactive session",
                        args.target, mission.id
                    )));
                }
            }
            let result = store
                .submit_review(
                    &caller,
                    TowerReviewInput {
                        target: args.target.clone(),
                        status: args.status.clone(),
                        merge: args.merge.clone(),
                        findings: args.findings.clone(),
                        checks: args.checks.clone(),
                        decision: args.decision.clone(),
                        acceptance_tip: args.acceptance_tip.clone(),
                    },
                )
                .await?;
            let delivery = result
                .stored_messages
                .iter()
                .map(|message| deliver_tower_message(self.delivery.as_ref(), &state, message))
                .collect::<Vec<_>>()
                .join("");
            let notified = result
                .stored_messages
                .iter()
                .map(|message| message.item.to.clone())
                .collect::<Vec<_>>();
            let mut lines = vec![
                format!("review submitted: {}", result.review.file),
                format!(
                    "notified: {}",
                    if notified.is_empty() {
                        "(none)".to_string()
                    } else {
                        notified.join(", ")
                    }
                ),
            ];
            if let Some(error) = &result.notification_error {
                lines.push(format!("notification error: {error}"));
            }
            if let Some(error) = &result.activity_log_error {
                lines.push(format!("activity log error: {error}"));
            }
            if let Some(artifact) = &artifact_mission {
                if args.status == "clean"
                    && args.merge == "merge"
                    && caller == TOWER_NAME
                    && result.review.mission.as_deref() == Some(artifact.id.as_str())
                {
                    lines.push(format!(
                        "next: explicit human acceptance for {} is recorded on the exact tip — the tower can TowerMerge it in Dependency Flow order.",
                        args.target
                    ));
                } else if args.status == "clean" && args.merge == "merge" {
                    lines.push(format!(
                        "next: {} is still waiting for mission-matching explicit human acceptance recorded by reviewer=tower; this review alone does not make the artifact merge-ready.",
                        args.target
                    ));
                } else if args.status == "clean" {
                    lines.push(format!(
                        "next: {} is not merge-approved — artifact acceptance requires status \"clean\" with merge verdict \"merge\", recorded by reviewer=tower after explicit user approval.",
                        args.target
                    ));
                } else {
                    lines.push(format!(
                        "next: {} is not merge-ready — the author must rework the artifacts, then the tower must obtain fresh explicit human acceptance on the exact tip.",
                        args.target
                    ));
                }
            } else if args.status == "clean" && args.merge == "merge" {
                lines.push(format!(
                    "next: {} is merge-ready — the tower can TowerMerge it in Dependency Flow order.",
                    args.target
                ));
            } else if args.status == "clean" {
                lines.push(if args.merge == "hold" {
                    format!(
                        "next: {} is blocked by the latest review's \"hold\" verdict — the merge gate rejects it; do not TowerMerge until a later clean review with merge verdict \"merge\" supersedes it.",
                        args.target
                    )
                } else {
                    format!(
                        "next: {} is not merge-approved — \"fix-then-merge\" is not approval; the merge gate requires a clean review with merge verdict \"merge\" before TowerMerge.",
                        args.target
                    )
                });
            } else {
                lines.push(format!(
                    "next: {} is not merge-ready — the gate requires a clean review with merge verdict \"merge\" on the exact tip; the author must fix and re-review.",
                    args.target
                ));
            }
            if !delivery.is_empty() {
                lines.push(delivery);
            }
            let is_error =
                result.notification_error.is_some() || result.activity_log_error.is_some();
            Ok(ExecutableToolResult {
                output: lines.join("\n").into(),
                is_error: is_error.then_some(true),
                ..Default::default()
            })
        })
        .await
    }
}

impl ExecutableTool for TowerReviewTool {
    fn name(&self) -> &str {
        TowerReviewTool::name(self)
    }

    fn description(&self) -> &str {
        TowerReviewTool::description(self)
    }

    fn parameters(&self) -> Map<String, Value> {
        TowerReviewTool::parameters(self)
    }

    fn resolve_execution(
        &self,
        input: Value,
    ) -> BoxFuture<'static, Result<ToolExecution, WireAsyncError>> {
        let execution = self.resolve_execution_value(input);
        Box::pin(async move { Ok(execution) })
    }
}
