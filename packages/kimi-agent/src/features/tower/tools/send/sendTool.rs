use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::{Map, Value};

use crate::agent::scope_context::scopeContext::IAgentScopeContext;
use crate::agent::tool_executor::tool_executor::{
    ExecutableTool, ExecutableToolResult, RunnableToolExecution, ToolExecution,
};
use crate::tool::tool_contract::{RepoMutation, ToolSideEffect};
use crate::features::tower::protocol::store::TowerSendInput;
use crate::features::tower::tools::delivery::{TowerDeliveryHost, deliver_tower_message};
use crate::features::tower::tools::support::{new_tower_store, run_tower_tool};
use crate::session::sessionContext::sessionContext::ISessionContext;
use crate::wire::record::WireAsyncError;

use super::send::{TowerSendToolInput, tower_send_tool_input_schema};

pub const TOWER_SEND_TOOL_NAME: &str = "TowerSend";

#[derive(Clone)]
pub struct TowerSendTool {
    session_context: Arc<dyn ISessionContext>,
    scope_context: Arc<dyn IAgentScopeContext>,
    delivery: Arc<dyn TowerDeliveryHost>,
}

impl TowerSendTool {
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
        TOWER_SEND_TOOL_NAME
    }

    pub fn description(&self) -> &'static str {
        include_str!("send.md")
    }

    pub fn parameters(&self) -> Map<String, Value> {
        tower_send_tool_input_schema()
            .as_object()
            .cloned()
            .expect("TowerSend input schema is an object")
    }

    pub fn resolve_execution_value(&self, input: Value) -> ToolExecution {
        match serde_json::from_value::<TowerSendToolInput>(input) {
            Ok(args) => self.resolve_execution(args),
            Err(error) => ToolExecution::Error(ExecutableToolResult::error(format!(
                "Invalid arguments for TowerSend: {error}"
            ))),
        }
    }

    pub fn resolve_execution(&self, args: TowerSendToolInput) -> ToolExecution {
        let description = format!("Sending tower message to {}: {}", args.to, args.subject);
        let this = self.clone();
        ToolExecution::Runnable(RunnableToolExecution {
            accesses: None,
            display: None,
            description: Some(description),
            stop_batch_after_this: None,
            approval_rule: TOWER_SEND_TOOL_NAME.to_string(),
            matches_rule: None,
            execute: Arc::new(move |_context| {
                let this = this.clone();
                let args = args.clone();
                Box::pin(async move { Ok(this.execute(args).await) })
            }),
        })
    }

    async fn execute(&self, args: TowerSendToolInput) -> ExecutableToolResult {
        run_tower_tool(async move {
            let store = new_tower_store(&self.session_context.cwd());
            let state = store.load_or_recover().await?;
            let (caller, resolved_state, placeholder) = store
                .resolve_messaging_caller(&state, &self.scope_context.agent_id())
                .await?;
            let sent = store
                .send_detailed(
                    &caller,
                    TowerSendInput {
                        to: args.to.trim().to_string(),
                        subject: args.subject.clone(),
                        body: args.body.clone(),
                        scope: args.scope.clone(),
                        action: args.action.clone(),
                        consent_ref: args.consent_ref.clone(),
                    },
                )
                .await?;
            let delivery =
                deliver_tower_message(self.delivery.as_ref(), &resolved_state, &sent);
            let mut lines = vec![format!(
                "message sent to {}\nfile: {}{delivery}",
                args.to, sent.item.file
            )];
            if placeholder {
                lines.push(format!(
                    "note: the tower state was recovered after a loss, so your roster entry was reconstructed as placeholder \"{caller}\" — your original name and mission assignment are gone; the recovered state's recoveredAt marker reminds the tower to report the history loss to the user"
                ));
            }
            if let Some(error) = &sent.activity_log_error {
                lines.push(format!("activity log error: {error}"));
            }
            let is_error = sent.activity_log_error.is_some();
            Ok(ExecutableToolResult {
                output: lines.join("\n").into(),
                is_error: is_error.then_some(true),
                ..Default::default()
            })
        })
        .await
    }
}

impl ExecutableTool for TowerSendTool {
    fn side_effect(&self) -> Option<ToolSideEffect> {
        Some(ToolSideEffect::Read)
    }

    fn repo_mutation(&self) -> Option<RepoMutation> {
        Some(RepoMutation::None)
    }

    fn name(&self) -> &str {
        TowerSendTool::name(self)
    }

    fn description(&self) -> &str {
        TowerSendTool::description(self)
    }

    fn parameters(&self) -> Map<String, Value> {
        TowerSendTool::parameters(self)
    }

    fn resolve_execution(
        &self,
        input: Value,
    ) -> BoxFuture<'static, Result<ToolExecution, WireAsyncError>> {
        let execution = self.resolve_execution_value(input);
        Box::pin(async move { Ok(execution) })
    }
}
