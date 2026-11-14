use std::future::Future;

use crate::agent::tool_executor::tool_executor::ExecutableToolResult;
use crate::features::tower::protocol::paths::resolve_tower_repo_root;
use crate::features::tower::protocol::store::TowerStore;
use crate::features::tower::protocol::types::TowerState;

pub const TOWER_MAIN_AGENT_ONLY: &str =
    "Tower orchestration tools are only supported by the main agent.";

pub const TOWER_MODE_USER_ENABLED_ONLY: &str = "tower mode is not active — only the user can enable it (with /tower on), never the agent. Ask the user to turn tower mode on, then drive the tower protocol.";

pub fn new_tower_store(cwd: &str) -> TowerStore {
    TowerStore::new(resolve_tower_repo_root(cwd))
}

pub fn caller_name(agent_id: &str, store: &TowerStore, state: &TowerState) -> Result<String, String> {
    store.resolve_caller_name(state, agent_id)
}

pub async fn run_tower_tool(
    execute: impl Future<Output = Result<ExecutableToolResult, String>>,
) -> ExecutableToolResult {
    match execute.await {
        Ok(result) => result,
        Err(message) => ExecutableToolResult::error(message),
    }
}
