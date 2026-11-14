use std::sync::Arc;

use crate::_base::errors::errors::SharedError;
use crate::_base::errors::unexpected_error::on_unexpected_error;
use crate::_base::utils::abort::{AbortError, AbortSignal};
use crate::agent::context_memory::compaction_handoff::build_compaction_summary_text;
use crate::agent::context_memory::context_memory::{
    ContextCompactionInput, IAgentContextMemoryService,
};
use crate::agent::context_memory::types::ContextMessage;
use crate::agent::state::agent_state::IAgentStateService;
use crate::agent::task::task::IAgentTaskService;
use crate::features::associativeMemory::service::AssociativeMemoryService;
use crate::features::associativeMemory::types::{MemoryScopeSelector, MemorySource};
use crate::features::todo::todoItem::render_todo_list;
use crate::features::todo::todoService::AgentTodoService;
use crate::human::llm::usage::TokenUsage;
use crate::wire::record::WireLineRange;
use crate::wire::wire::IWireService;

use super::compaction_ops::full_compaction_wire_ranges_key;
use super::compaction_utils::history_safe_to_compact;
use super::context_recovery::{ContextRecoveryPointer, render_context_recovery_pointer};

pub struct CompactionApplyDeps {
    pub context: Arc<dyn IAgentContextMemoryService>,
    pub todo: Arc<AgentTodoService>,
    pub task: Arc<dyn IAgentTaskService>,
    pub associative_memory: Arc<AssociativeMemoryService>,
    pub wire: Arc<dyn IWireService>,
    pub states: Arc<dyn IAgentStateService>,
    pub estimate_text: Arc<dyn Fn(&str) -> u64 + Send + Sync>,
}

#[derive(Debug, Clone)]
pub struct CompactionApplyInput {
    pub attempt_summary: String,
    pub attempt_usage: Option<TokenUsage>,
    pub original_history: Vec<ContextMessage>,
    pub non_history_state_before: String,
    pub tokens_before: u64,
    pub dropped_count: usize,
    pub request_overhead_tokens: u64,
    pub in_flight_turn: Vec<ContextMessage>,
}

#[derive(Debug, Clone)]
pub struct CompactionApplyResult {
    pub summary: String,
    pub compacted_count: usize,
    pub tokens_before: u64,
    pub tokens_after: u64,
    pub dropped_count: Option<usize>,
}

pub async fn apply_compaction_round(
    deps: &CompactionApplyDeps,
    input: &CompactionApplyInput,
    signal: AbortSignal,
    cancel: Arc<dyn Fn() -> SharedError + Send + Sync>,
) -> Result<CompactionApplyResult, SharedError> {
    if !history_safe_to_compact(&deps.context.get(), &input.original_history) {
        return Err(cancel());
    }
    if non_history_state_fingerprint(deps) != input.non_history_state_before {
        return Err(cancel());
    }
    let summary = post_process_summary(deps, &input.attempt_summary).await;
    let wire_lines = capture_wire_lines(deps).await;
    if signal.aborted() {
        return Err(signal.reason().unwrap_or_else(|| {
            Arc::new(AbortError::new("This operation was aborted")) as SharedError
        }));
    }
    let recovery_footer = render_recovery_footer(deps, wire_lines);
    let summary_text = build_compaction_summary_text(&summary);
    let context_summary = match recovery_footer.as_deref() {
        Some(footer) => format!("{summary_text}\n\n{footer}"),
        None => summary_text,
    };
    let summary_output_tokens = input.attempt_usage.as_ref().map(|usage| {
        usage.output
            + recovery_footer
                .as_deref()
                .map_or(0, |footer| (deps.estimate_text)(footer))
    });
    let result = deps.context.apply_compaction(ContextCompactionInput {
        summary: summary.clone(),
        context_summary: Some(context_summary),
        compacted_count: input.original_history.len(),
        tokens_before: input.tokens_before,
        tokens_after: None,
        in_flight_turn: Some(input.in_flight_turn.clone()),
        summary_output_tokens,
        request_overhead_tokens: Some(input.request_overhead_tokens),
        kept_user_message_count: None,
        kept_head_user_message_count: None,
        dropped_count: (input.dropped_count != 0).then_some(input.dropped_count),
        wire_lines,
    });
    if let Err(error) = deps
        .associative_memory
        .write_note(&summary, MemoryScopeSelector::Project, MemorySource::External)
        .await
    {
        on_unexpected_error(error);
    }
    Ok(CompactionApplyResult {
        summary,
        compacted_count: result.compacted_count,
        tokens_before: result.tokens_before,
        tokens_after: result.tokens_after,
        dropped_count: result.dropped_count,
    })
}

pub fn non_history_state_fingerprint(deps: &CompactionApplyDeps) -> String {
    let todos = serde_json::to_string(&deps.todo.get()).expect("todo state is serializable");
    let tasks = serde_json::to_string(&deps.task.list(Some(true), None))
        .expect("task state is serializable");
    format!("{{\"todos\":{todos},\"tasks\":{tasks}}}")
}

async fn post_process_summary(deps: &CompactionApplyDeps, summary: &str) -> String {
    let todos = deps.todo.get();
    if todos.is_empty() {
        return summary.to_string();
    }
    format!(
        "{}\n\n{}",
        summary.trim(),
        render_todo_list(&todos, "## TODO List")
    )
}

async fn capture_wire_lines(deps: &CompactionApplyDeps) -> Option<WireLineRange> {
    if let Err(error) = deps.wire.flush().await {
        on_unexpected_error(error.to_string());
        return None;
    }
    let end = deps.wire.line_count();
    let ranges = deps.states.get(&full_compaction_wire_ranges_key());
    let previous_end = ranges
        .as_array()
        .and_then(|ranges| ranges.last())
        .and_then(|range| range.get("end"))
        .and_then(serde_json::Value::as_u64);
    let start = previous_end
        .unwrap_or(0)
        .max(deps.wire.last_context_clear_line().unwrap_or(0))
        + 1;
    if end < start {
        return None;
    }
    Some(WireLineRange { start, end })
}

fn render_recovery_footer(
    deps: &CompactionApplyDeps,
    wire_lines: Option<WireLineRange>,
) -> Option<String> {
    let wire_lines = wire_lines?;
    let journal_path = deps.wire.journal_path()?;
    let ranges = deps.states.get(&full_compaction_wire_ranges_key());
    let mut windows = ranges
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|range| {
            Some(WireLineRange {
                start: range.get("start")?.as_u64()?,
                end: range.get("end")?.as_u64()?,
            })
        })
        .collect::<Vec<_>>();
    windows.push(wire_lines);
    Some(render_context_recovery_pointer(ContextRecoveryPointer {
        journal_path,
        windows,
    }))
}
