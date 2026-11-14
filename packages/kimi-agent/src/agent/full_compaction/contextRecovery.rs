use crate::wire::record::WireLineRange;

pub use crate::compaction::*;

pub const CONTEXT_RECOVERY_HEADING: &str = "## Context Recovery";

const CONTEXT_RECOVERY_TEMPLATE: &str = "## Context Recovery\nEverything before this note is still on disk in this agent's event log (read-only, append-only):\n  ${wire_path}\n${window_lines}\nIf you need exact command output, file contents, error text, or the wording of an earlier request, look it up there instead of guessing. How to read it:\n- Layout: one file per agent. agents/main/ is the main agent; each subagent has its own agents/<agentId>/wire.jsonl. A parent's log holds only the Agent tool call and the subagent's returned result — the subagent's own steps are in its own file.\n- Format: one JSON record per line, append-only; `type` says what it is. The conversation is in `context.append_message` (user prompts) and `context.append_loop_event` (event.type: step.begin | content.part [text|think] | tool.call | tool.result | step.end). Every other type (llm.request, usage.record, token_counting.measured, metadata, profile.bind, …) is bookkeeping — skip it.\n- Boundaries: `context.apply_compaction` marks a compaction (older lines stay in the file; grep for it to find exact boundaries). `context.undo` count=N retracts the previous N messages — treat retracted content as never having happened. `context.clear` resets the conversation.\n- Externalized content: tool results over 50k chars may contain an `output_path` pointing to saved output; check the result's preservation notice before assuming the file contains everything. Read results are bounded by their own character budget and are not spilled again. Media parts are blob references, not inline.\n- Reading: lines are long JSON (often 10k+ chars). Grep the file for a keyword to get line numbers, then Read exactly that line (line_offset=N, n_lines=1). Long records can span several Read results: follow Next Read with its column_offset until the line is complete, joining fragments without inserting newlines. To pull one field with real newlines when Bash is available: sed -n 'Np' wire.jsonl | jq -r '.event.result.output'. Prefer individual records over large ranges.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextRecoveryPointer {
    pub journal_path: String,
    pub windows: Vec<WireLineRange>,
}

pub fn render_context_recovery_pointer(pointer: ContextRecoveryPointer) -> String {
    let summarized = pointer.windows.len() - 1;
    let mut lines = pointer
        .windows
        .iter()
        .enumerate()
        .map(|(index, range)| {
            let label = format!(
                "window {}: lines {}–{}",
                index + 1,
                range.start,
                range.end
            );
            if index == summarized {
                format!("{label}   ← the conversation this note summarizes")
            } else {
                label
            }
        })
        .collect::<Vec<_>>();
    let next_start = pointer.windows[summarized].end + 1;
    lines.push(format!(
        "window {} (the one you are in now) starts at line {} with the `context.apply_compaction` record that carries this note — it is already in your context; no need to read it.",
        pointer.windows.len() + 1,
        next_start
    ));
    let window_lines = lines
        .into_iter()
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    CONTEXT_RECOVERY_TEMPLATE
        .replace("${wire_path}", &pointer.journal_path)
        .replace("${window_lines}", &window_lines)
        .trim_end()
        .to_string()
}
