use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::{Map, Value};

use crate::agent::scope_context::scopeContext::IAgentScopeContext;
use crate::agent::tool_executor::tool_executor::{
    ExecutableTool, ExecutableToolResult, RunnableToolExecution, ToolExecution,
};
use crate::features::tower::tools::delivery::{TowerDeliveryHost, deliver_tower_message};
use crate::features::tower::tools::support::{new_tower_store, run_tower_tool};
use crate::session::sessionContext::sessionContext::ISessionContext;
use crate::wire::record::WireAsyncError;

use super::complete::{TowerCompleteToolInput, tower_complete_tool_input_schema};

pub const TOWER_COMPLETE_TOOL_NAME: &str = "TowerComplete";

#[derive(Clone)]
pub struct TowerCompleteTool {
    session_context: Arc<dyn ISessionContext>,
    scope_context: Arc<dyn IAgentScopeContext>,
    delivery: Arc<dyn TowerDeliveryHost>,
}

impl TowerCompleteTool {
    pub fn new(
        session_context: Arc<dyn ISessionContext>,
        scope_context: Arc<dyn IAgentScopeContext>,
        delivery: Arc<dyn TowerDeliveryHost>,
    ) -> Self {
        Self {
            session_context,
            scope_context,
            delivery,
        }
    }

    pub fn name(&self) -> &'static str {
        TOWER_COMPLETE_TOOL_NAME
    }

    pub fn side_effect(&self) -> &'static str {
        "write"
    }

    pub fn repo_mutation(&self) -> &'static str {
        "none"
    }

    pub fn description(&self) -> &'static str {
        include_str!("complete.md")
    }

    pub fn parameters(&self) -> Map<String, Value> {
        tower_complete_tool_input_schema()
            .as_object()
            .cloned()
            .expect("TowerComplete input schema is an object")
    }

    pub fn resolve_execution_value(&self, input: Value) -> ToolExecution {
        match serde_json::from_value::<TowerCompleteToolInput>(input) {
            Ok(args) => self.resolve_execution(args),
            Err(error) => ToolExecution::Error(ExecutableToolResult::error(format!(
                "Invalid arguments for TowerComplete: {error}"
            ))),
        }
    }

    pub fn resolve_execution(&self, args: TowerCompleteToolInput) -> ToolExecution {
        let this = self.clone();
        ToolExecution::Runnable(RunnableToolExecution {
            accesses: None,
            display: None,
            description: Some("Completing tower mission and storing its notification".to_string()),
            stop_batch_after_this: None,
            approval_rule: TOWER_COMPLETE_TOOL_NAME.to_string(),
            matches_rule: None,
            execute: Arc::new(move |_context| {
                let this = this.clone();
                let args = args.clone();
                Box::pin(async move { Ok(this.execute(args).await) })
            }),
        })
    }

    async fn execute(&self, args: TowerCompleteToolInput) -> ExecutableToolResult {
        run_tower_tool(async move {
            let store = new_tower_store(&self.session_context.cwd());
            let state = store.load_or_recover().await?;
            let (caller, resolved_state, _placeholder) = store
                .resolve_messaging_caller(&state, &self.scope_context.agent_id())
                .await?;
            let result = store
                .complete(
                    &caller,
                    args.report.trim(),
                    args.key_artifacts.as_deref().unwrap_or_default(),
                )
                .await?;
            let delivery = match &result.message {
                Some(message) => deliver_tower_message(
                    self.delivery.as_ref(),
                    &resolved_state,
                    message,
                ),
                None => String::new(),
            };
            let mut lines = Vec::new();
            if let Some(mission) = &result.mission {
                lines.push(format!(
                    "mission {} persisted as {}",
                    mission.id,
                    mission.status.as_str()
                ));
                if let Some(message) = &result.message {
                    lines.push(format!(
                        "{} notification: {}",
                        message.item.subject, message.item.file
                    ));
                    lines.push(if result.notification_created {
                        "notification: stored once".to_string()
                    } else {
                        "notification: reused the stored message; no duplicate was written"
                            .to_string()
                    });
                }
            } else {
                lines.push(format!(
                    "the tower state was recovered at {} after a loss — your mission record is gone, so no mission was marked completed and nothing pretends otherwise",
                    result.recovered_at.as_deref().unwrap_or("an unknown time")
                ));
                if let Some(message) = &result.message {
                    lines.push(format!(
                        "your report was still delivered to the tower as a {} message: {}",
                        message.item.subject, message.item.file
                    ));
                    lines.push(if result.notification_created {
                        "notification: stored once".to_string()
                    } else {
                        "notification: reused the stored message; no duplicate was written"
                            .to_string()
                    });
                }
            }
            if let Some(error) = &result.notification_error {
                lines.push(format!("notification error: {error}"));
            }
            if let Some(error) = &result.activity_log_error {
                lines.push(format!("activity log error: {error}"));
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

impl ExecutableTool for TowerCompleteTool {
    fn name(&self) -> &str {
        TowerCompleteTool::name(self)
    }

    fn description(&self) -> &str {
        TowerCompleteTool::description(self)
    }

    fn parameters(&self) -> Map<String, Value> {
        TowerCompleteTool::parameters(self)
    }

    fn resolve_execution(
        &self,
        input: Value,
    ) -> BoxFuture<'static, Result<ToolExecution, WireAsyncError>> {
        let execution = self.resolve_execution_value(input);
        Box::pin(async move { Ok(execution) })
    }
}
