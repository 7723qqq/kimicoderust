use std::collections::HashSet;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::{Map, Value};

use crate::agent::scope_context::scopeContext::IAgentScopeContext;
use crate::agent::task::task::{AgentTaskInfo, IAgentTaskService};
use crate::agent::tool_executor::tool_executor::{
    ExecutableTool, ExecutableToolResult, RunnableToolExecution, ToolExecution,
};
use crate::app::session_manager::session_manager::ISessionManager;
use crate::features::tower::protocol::store::TowerTeardownOptions;
use crate::features::tower::tools::support::{TOWER_MAIN_AGENT_ONLY, new_tower_store, run_tower_tool};
use crate::session::agent_lifecycle::agent_lifecycle::MAIN_AGENT_ID;
use crate::session::sessionContext::sessionContext::ISessionContext;
use crate::wire::record::WireAsyncError;

use super::teardown::{TowerTeardownToolInput, tower_teardown_tool_input_schema};

pub const TOWER_TEARDOWN_TOOL_NAME: &str = "TowerTeardown";

#[derive(Clone)]
pub struct TowerTeardownTool {
    session_context: Arc<dyn ISessionContext>,
    sessions: Arc<dyn ISessionManager>,
    scope_context: Arc<dyn IAgentScopeContext>,
    tasks: Arc<dyn IAgentTaskService>,
}

impl TowerTeardownTool {
    pub fn new(
        session_context: Arc<dyn ISessionContext>,
        sessions: Arc<dyn ISessionManager>,
        scope_context: Arc<dyn IAgentScopeContext>,
        tasks: Arc<dyn IAgentTaskService>,
    ) -> Self {
        Self {
            session_context,
            sessions,
            scope_context,
            tasks,
        }
    }

    pub fn name(&self) -> &'static str {
        TOWER_TEARDOWN_TOOL_NAME
    }

    pub fn side_effect(&self) -> &'static str {
        "write"
    }

    pub fn repo_mutation(&self) -> &'static str {
        "none"
    }

    pub fn description(&self) -> &'static str {
        include_str!("teardown.md")
    }

    pub fn parameters(&self) -> Map<String, Value> {
        tower_teardown_tool_input_schema()
            .as_object()
            .cloned()
            .expect("TowerTeardown input schema is an object")
    }

    pub fn resolve_execution_value(&self, input: Value) -> ToolExecution {
        match serde_json::from_value::<TowerTeardownToolInput>(input) {
            Ok(args) => self.resolve_execution(args),
            Err(error) => ToolExecution::Error(ExecutableToolResult::error(format!(
                "Invalid arguments for TowerTeardown: {error}"
            ))),
        }
    }

    pub fn resolve_execution(&self, args: TowerTeardownToolInput) -> ToolExecution {
        if self.scope_context.agent_id() != MAIN_AGENT_ID {
            return ToolExecution::Error(ExecutableToolResult::error(TOWER_MAIN_AGENT_ONLY));
        }
        let suffix = if args.dry_run == Some(true) {
            " (dry run)"
        } else if args.force == Some(true) {
            " (force)"
        } else {
            ""
        };
        let this = self.clone();
        ToolExecution::Runnable(RunnableToolExecution {
            accesses: None,
            display: None,
            description: Some(format!("Tearing down tower workspace{suffix}")),
            stop_batch_after_this: None,
            approval_rule: TOWER_TEARDOWN_TOOL_NAME.to_string(),
            matches_rule: None,
            execute: Arc::new(move |_context| {
                let this = this.clone();
                let args = args.clone();
                Box::pin(async move { Ok(this.execute(args).await) })
            }),
        })
    }

    async fn execute(&self, args: TowerTeardownToolInput) -> ExecutableToolResult {
        run_tower_tool(async move {
            let store = new_tower_store(&self.session_context.cwd());
            let session_id = self.session_context.session_id();
            let prior_owner = store
                .load()
                .await
                .ok()
                .and_then(|state| state.session_id);
            if let Some(prior_owner) = &prior_owner {
                if prior_owner != &session_id && self.sessions.get(prior_owner).is_some() {
                    return Err(format!(
                        "tower workspace is owned by a live session ({prior_owner}) — tearing it down would dismantle that session's fleet. Use TowerTeardown from that session, or close it first."
                    ));
                }
            }
            let live_agent_ids = self
                .tasks
                .list(Some(true), None)
                .into_iter()
                .filter_map(|task| match task {
                    AgentTaskInfo::Agent(info) => info.agent_id,
                    _ => None,
                })
                .collect::<HashSet<_>>();
            let dry_run = args.dry_run.unwrap_or(false);
            let report = store
                .teardown(TowerTeardownOptions {
                    force: args.force.unwrap_or(false),
                    exclude: args.exclude.unwrap_or_default(),
                    dry_run,
                    live_agent_ids,
                })
                .await?;
            let mut lines = vec![if dry_run {
                "tower teardown (dry run — nothing was changed):".to_string()
            } else {
                "tower teardown:".to_string()
            }];
            lines.extend(report.into_iter().map(|line| format!("- {line}")));
            lines.push(String::new());
            lines.push("Tower mode stays active — the next objective starts with TowerInit, and the human can turn the mode off with /tower off. .tower/comms/ (state, inbox, findings, reviews, activity log) is kept as the audit trail — remove it by hand only if you are sure.".to_string());
            Ok(ExecutableToolResult {
                output: lines.join("\n").into(),
                ..Default::default()
            })
        })
        .await
    }
}

impl ExecutableTool for TowerTeardownTool {
    fn name(&self) -> &str {
        TowerTeardownTool::name(self)
    }

    fn description(&self) -> &str {
        TowerTeardownTool::description(self)
    }

    fn parameters(&self) -> Map<String, Value> {
        TowerTeardownTool::parameters(self)
    }

    fn resolve_execution(
        &self,
        input: Value,
    ) -> BoxFuture<'static, Result<ToolExecution, WireAsyncError>> {
        let execution = self.resolve_execution_value(input);
        Box::pin(async move { Ok(execution) })
    }
}
