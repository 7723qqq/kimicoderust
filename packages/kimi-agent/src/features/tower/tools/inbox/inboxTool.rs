use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::{Map, Value};

use crate::agent::scope_context::scopeContext::IAgentScopeContext;
use crate::agent::tool_executor::tool_executor::{
    ExecutableTool, ExecutableToolResult, RunnableToolExecution, ToolExecution,
};
use crate::tool::tool_contract::{RepoMutation, ToolSideEffect};
use crate::features::tower::tools::support::{caller_name, new_tower_store, run_tower_tool};
use crate::session::sessionContext::sessionContext::ISessionContext;
use crate::wire::record::WireAsyncError;

use super::inbox::{TowerInboxToolInput, tower_inbox_tool_input_schema};

pub const TOWER_INBOX_TOOL_NAME: &str = "TowerInbox";
const DEFAULT_LIMIT: usize = 20;

#[derive(Clone)]
pub struct TowerInboxTool {
    session_context: Arc<dyn ISessionContext>,
    scope_context: Arc<dyn IAgentScopeContext>,
}

impl TowerInboxTool {
    pub fn new(
        session_context: Arc<dyn ISessionContext>,
        scope_context: Arc<dyn IAgentScopeContext>,
    ) -> Self {
        Self {
            session_context,
            scope_context,
        }
    }

    pub fn name(&self) -> &'static str {
        TOWER_INBOX_TOOL_NAME
    }

    pub fn description(&self) -> &'static str {
        include_str!("inbox.md")
    }

    pub fn parameters(&self) -> Map<String, Value> {
        tower_inbox_tool_input_schema()
            .as_object()
            .cloned()
            .expect("TowerInbox input schema is an object")
    }

    pub fn resolve_execution_value(&self, input: Value) -> ToolExecution {
        match serde_json::from_value::<TowerInboxToolInput>(input) {
            Ok(args) => self.resolve_execution(args),
            Err(error) => ToolExecution::Error(ExecutableToolResult::error(format!(
                "Invalid arguments for TowerInbox: {error}"
            ))),
        }
    }

    pub fn resolve_execution(&self, args: TowerInboxToolInput) -> ToolExecution {
        let this = self.clone();
        ToolExecution::Runnable(RunnableToolExecution {
            accesses: None,
            display: None,
            description: Some("Reading tower inbox".to_string()),
            stop_batch_after_this: None,
            approval_rule: TOWER_INBOX_TOOL_NAME.to_string(),
            matches_rule: None,
            execute: Arc::new(move |_context| {
                let this = this.clone();
                let args = args.clone();
                Box::pin(async move { Ok(this.execute(args).await) })
            }),
        })
    }

    async fn execute(&self, args: TowerInboxToolInput) -> ExecutableToolResult {
        run_tower_tool(async move {
            let store = new_tower_store(&self.session_context.cwd());
            let state = store.load().await?;
            let caller = caller_name(&self.scope_context.agent_id(), &store, &state)?;
            let limit = args.limit.unwrap_or(DEFAULT_LIMIT);
            let items = if args.include_read == Some(true) {
                store.read_inbox(&caller, limit).await?
            } else {
                store.read_unread_inbox(&caller, limit).await?
            };
            if items.is_empty() {
                return Ok(ExecutableToolResult {
                    output: format!(
                        "inbox empty for {caller}; 0 unread message(s) remaining"
                    )
                    .into(),
                    ..Default::default()
                });
            }
            let remaining = store
                .ack_inbox(
                    &caller,
                    &items
                        .iter()
                        .map(|item| item.message_id.clone())
                        .collect::<Vec<_>>(),
                )
                .await?;
            let sections = items
                .iter()
                .map(|item| {
                    let mut lines = vec![
                        format!("file: {}", item.file),
                        format!("from: {}", item.from),
                        format!("to: {}", item.to),
                        format!("subject: {}", item.subject),
                        format!("sent_at: {}", item.sent_at),
                    ];
                    if let Some(scope) = &item.scope {
                        lines.push(format!("scope: {scope}"));
                    }
                    if let Some(action) = &item.action {
                        lines.push(format!("action: {action}"));
                    }
                    lines.push(String::new());
                    lines.push(item.body.clone());
                    lines.join("\n")
                })
                .collect::<Vec<_>>();
            Ok(ExecutableToolResult {
                output: [
                    format!("{} message(s) for {caller} (oldest first):", items.len()),
                    String::new(),
                    sections.join("\n\n---\n\n"),
                    String::new(),
                    format!("{remaining} unread message(s) remaining"),
                ]
                .join("\n")
                .into(),
                ..Default::default()
            })
        })
        .await
    }
}

impl ExecutableTool for TowerInboxTool {
    fn side_effect(&self) -> Option<ToolSideEffect> {
        Some(ToolSideEffect::Read)
    }

    fn repo_mutation(&self) -> Option<RepoMutation> {
        Some(RepoMutation::None)
    }

    fn name(&self) -> &str {
        TowerInboxTool::name(self)
    }

    fn description(&self) -> &str {
        TowerInboxTool::description(self)
    }

    fn parameters(&self) -> Map<String, Value> {
        TowerInboxTool::parameters(self)
    }

    fn resolve_execution(
        &self,
        input: Value,
    ) -> BoxFuture<'static, Result<ToolExecution, WireAsyncError>> {
        let execution = self.resolve_execution_value(input);
        Box::pin(async move { Ok(execution) })
    }
}
