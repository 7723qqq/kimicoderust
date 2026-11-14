use crate::turn_loop::types::ExecutableToolResult;

pub const CRON_MAIN_AGENT_ONLY: &str = "Cron tools are only supported by the main agent.";
pub const GOAL_MAIN_AGENT_ONLY: &str = "Goal tools are only supported by the main agent.";

pub fn main_agent_only_execution(
    agent_id: &str,
    output: impl Into<String>,
) -> Option<ExecutableToolResult> {
    if agent_id == crate::callbacks::MAIN_AGENT_ID {
        return None;
    }
    Some(ExecutableToolResult {
        spill: None,
        parts: Vec::new(),
        delivery: None,
        stop_turn: false,
        content: output.into(),
        is_error: true,
        note: None,
        display: None,
    })
}
