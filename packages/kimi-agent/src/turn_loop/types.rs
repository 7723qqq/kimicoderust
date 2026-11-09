//! Core type definitions for the stateless turn loop.
//!
//! These correspond to the types in
//! `packages/agent-core-v2/src/agent/loop/loop.ts`.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::rpc::types::TokenUsage;
use crate::tool_input_display::ToolInputDisplay;

pub use crate::rpc::types::{
    ContentBlock, GoalContext, GoalStatus, TelemetryContext, ToolDelivery, ToolInfo,
};

// ── TurnResult ─────────────────────────────────────────────────────────────

/// The final result of a completed turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnResult {
    /// Why the turn stopped.
    pub stop_reason: LoopTurnStopReason,
    /// Number of steps taken.
    pub steps: u32,
    /// Token usage for the entire turn.
    pub usage: TokenUsage,
    /// Host-visible engine events emitted during the turn (step lifecycle,
    /// deltas, native tool results, goal budget limits). Filled by the
    /// composition root (stdio CLI / napi addon) which owns the counting
    /// callbacks wrapper; zero when `run_turn` is called directly.
    pub events_emitted: u32,
    /// LLM retries performed during the turn: attempts beyond the first
    /// per step, summed over all steps.
    pub llm_retries: u32,
    /// Which LLM transport served this turn — `native-http`, `host-proxy` or
    /// `multi`. Filled by the composition root, which is what selects the
    /// transport; empty when `run_turn` is called directly.
    #[serde(default)]
    pub llm_transport: String,
    /// Tool calls executed inside this process instead of round-tripping to
    /// the host. Filled by the composition root (see
    /// [`crate::callbacks::NativeToolCallbacks`]); zero also means "native tool
    /// execution was not engaged for this turn".
    #[serde(default)]
    pub native_tool_calls: u32,
    /// The turn's final message history, system message included (index 0).
    /// Callers that own cross-turn conversation history (the engine session,
    /// the REPL) adopt `messages[1..]` as the next turn's input; the wire
    /// mappings (napi / stdio) ignore this field — the host owns the
    /// transcript there.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<LLMMessage>,
    /// The usage of the turn's **last** LLM request, as opposed to `usage`
    /// which sums every request of the turn. Its total (input + output) is the
    /// measured anchor the context-token counting service puts on the
    /// conversation (v2 `TokenCountingAgentModel.measured`), together with the
    /// input scope the request actually served.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_request_usage: Option<RequestMeasurement>,
    /// Usage of the compactions this turn ran (auto, or the emergency round
    /// that recovered a context overflow). Kept apart from `usage` so session
    /// accounting can bill it without mixing it into per-turn request totals;
    /// `None` when no compaction reached the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_usage: Option<TokenUsage>,
    /// Tokens the turn's active tool schemas add to every request (v2
    /// `countRequestTokens` overhead). Persisted with the context gauge so the
    /// gauge and the compaction trigger share one basis.
    #[serde(default)]
    pub tool_schema_tokens: u32,
    /// Stop-hook continuation (v2 `runStopHooks`): when a text-finished turn
    /// trips a matching `Stop` hook, its veto text rides here as the user
    /// message the driver should re-prompt with. `None` = the stop proceeds.
    /// Built-in drivers never see it set — `run_turn_continued` consumes the
    /// veto transparently inside the turn; only direct `run_turn` users
    /// observe (and must act on) this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_hook_continuation: Option<String>,
    /// The prompt gate rejected this turn (v2 `onBeforeSubmitPrompt` block,
    /// `loopService.ts:678-690`). A driver that owns a prompt row settles it
    /// as `blocked` instead of `completed`; direct `run_turn` callers can
    /// ignore it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub gate_blocked: bool,
    /// Prompt-cache observations, one per LLM response in step order (v2
    /// `llmRequesterLastTurnRequest.observation`, consumed by
    /// `AgentCacheBreakService.observe`). Engine-internal: the wire mappings
    /// ignore this field and it is not serialized.
    #[serde(skip)]
    pub cache_observations: Vec<crate::session::cache_observation::CacheObservation>,
    /// Every request of the turn as `prompt_cache_probe` compares it (v2
    /// `llmRequesterLastTurnRequest`): model, tool/system-prompt hashes,
    /// history shape and timing, in step order. The first one is what a fork's
    /// first record compares against its parent; the last one is the agent's
    /// new baseline. Engine-internal; not serialized.
    #[serde(skip)]
    pub cache_probe_requests: Vec<crate::session::cache_observation::CacheProbeRequest>,
    /// Indices into [`Self::cache_observations`] where a compaction replaced
    /// the history before that step. v2's `AgentCacheBreakService` resets on
    /// `context.compactionCompleted`, so the engine's end-of-turn fold drops
    /// the break baseline at each boundary instead of comparing a
    /// post-compaction cache read with a pre-compaction one. Engine-internal;
    /// not serialized.
    #[serde(skip)]
    pub cache_observation_resets: Vec<usize>,
}

/// What one LLM request measured, and the input scope it measured it on.
///
/// v2's `TokenCountingAgentModel.measured(input, context)` anchors the
/// conversation only when the request's input is still the current context,
/// and at the input's length — not at the transcript's final length, which has
/// the reply appended after it. `input_messages` is that input length in the
/// loop's coordinates (system message included; callers that do not carry the
/// system prompt subtract one), and `spliced_after` records that a compaction
/// replaced the history prefix after this request, which makes the
/// measurement stale the same way v2's identity check does.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestMeasurement {
    pub usage: TokenUsage,
    /// Messages the request served, system message included.
    pub input_messages: usize,
    /// A compaction spliced the history after this request.
    pub spliced_after: bool,
}

impl TurnResult {
    /// Attach the turn's last-request measurement — the measured context
    /// anchor callers put on the conversation (v2 `TokenCountingAgentModel`).
    pub fn with_last_request_usage(mut self, usage: Option<RequestMeasurement>) -> Self {
        self.last_request_usage = usage;
        self
    }

    /// Mark the turn as rejected by the prompt gate (`blocked`, not
    /// `completed`/`failed`).
    pub fn with_gate_blocked(mut self) -> Self {
        self.gate_blocked = true;
        self
    }

    /// Attach the turn's compaction usage and tool-schema overhead so session
    /// accounting and the context gauge see them on every exit path.
    pub fn with_accounting(
        mut self,
        compaction_usage: Option<TokenUsage>,
        tool_schema_tokens: u32,
    ) -> Self {
        self.compaction_usage = compaction_usage;
        self.tool_schema_tokens = tool_schema_tokens;
        self
    }
}

/// Reasons a turn can stop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LoopTurnStopReason {
    EndTurn,
    MaxTokens,
    Filtered,
    Paused,
    Unknown,
    Aborted,
    /// Goal budget exhausted (token, turn, or wall-clock).
    BudgetLimited,
    /// Repeat breaker stopped the turn after tool deduplication streak (upstream #3459).
    RepeatBreaker,
    /// The step budget ran out while the model was still requesting tools —
    /// v2 raises `MaxStepsExceededError` there (loopService.ts:841-843) and
    /// the turn fails with interrupt_reason `max_steps` instead of ending as
    /// a normal completion.
    MaxSteps,
}

// ── LLM interface ──────────────────────────────────────────────────────────

/// The LLM abstraction that the loop calls.
pub use kimi_core::types::{ArgsParseError, LLM, LLMChatParams, LLMChatResponse, LLMMessage, ToolCall};


// ── ExecutableTool trait ───────────────────────────────────────────────────

/// The result of resolving a tool execution.
pub enum ToolExecution {
    Runnable(RunnableToolExecution),
    Error(ExecutableToolErrorResult),
}

/// A tool that can be executed.
pub struct RunnableToolExecution {
    pub accesses: ToolAccesses,
    pub approval_rule: String,
    /// What the tool is about to do, for hosts that render a card instead of
    /// the raw argument string (v2 `RunnableToolExecution.display`). Declared
    /// before execution, so it rides the `tool.call.started` event.
    pub display: Option<ToolInputDisplay>,
    /// The actual execution logic.
    pub execute: ToolExecutor,
}

/// Tool execution closure: takes the execution context and produces a result.
pub type ToolExecutor = Box<
    dyn FnOnce(ToolExecContext) -> Result<ExecutableToolResult, Box<dyn std::error::Error>> + Send,
>;

/// Context passed to a tool's execute function.
#[derive(Debug, Clone)]
pub struct ToolExecContext {
    pub turn_id: String,
    pub tool_call_id: String,
}

/// Metadata a tool returns when it already streamed its own output to a file
/// (v2 `ToolResultSpill`, `toolContract.ts:11-15`). The truncation service
/// points the model at that file instead of saving a second copy, and appends
/// `suffix` after the pointer so a trailing status line survives truncation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolResultSpill {
    /// The file the tool already wrote; `None` means the truncator must save
    /// the retained text itself.
    pub output_path: Option<String>,
    /// Characters the tool actually produced, which can exceed the retained
    /// text when the tool saturated its buffer.
    pub total_chars: Option<usize>,
    /// A trailing line the tool wants kept verbatim (e.g. the shell's
    /// `Command failed with exit code: N`).
    pub suffix: Option<String>,
}

/// The result of a tool execution.
#[derive(Debug, Clone)]
pub struct ExecutableToolResult {
    pub stop_turn: bool,
    pub content: String,
    pub is_error: bool,
    /// Model-facing prose appended to the rendered result (v2
    /// `ToolResult.note`): Read's `<system>…</system>` summary, the encoding
    /// guidance after a mangled read, the readMedia video note. Appended last,
    /// after the status wrapper and the wall-time header. `None` when the tool
    /// produces none — never an internal tag.
    pub note: Option<String>,
    /// Rich content delivered to the model as a follow-up user message
    /// (see [`ToolDelivery`]). `None` for every text-only tool.
    pub delivery: Option<ToolDelivery>,
    /// Media and other non-text blocks returned inside the tool result itself
    /// (v2 `ExecutableToolOutput = string | ContentPart[]`). Populated by
    /// readMedia, skill and MCP results; the turn loop still routes delivery as
    /// a follow-up message until its message assembly is coordinated.
    pub parts: Vec<ContentBlock>,
    /// What the tool ended up doing, when it differs from what it declared
    /// (v2 `ToolResult.display`). A `diff` that failed to apply, a `file_io`
    /// whose write was rejected: the host shows this on the terminal card.
    ///
    /// Boxed because the union's widest variant carries several owned strings,
    /// and inlining it pushed `ExecutableToolResult` past clippy's
    /// `result_large_err` threshold on every function that returns one in its
    /// `Err` variant. The block is absent for the overwhelming majority of
    /// results, so the indirection costs nothing on the hot path.
    pub display: Option<Box<ToolInputDisplay>>,
    /// Spill metadata from a tool that already persisted its own output
    /// (v2 `ExecutableToolResult.spill`). Consumed by the truncation service;
    /// `None` for tools that produce everything in memory.
    pub spill: Option<ToolResultSpill>,
}

/// Error result from tool resolution.
#[derive(Debug, Clone)]
pub struct ExecutableToolErrorResult {
    pub message: String,
}

// ── ToolAccesses (resource conflict detection) ───────────────────────────────
//
// Matches `ToolAccesses` in `packages/agent-core-v2/src/tool/toolContract.ts`.

/// File access operation type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileOperation {
    Read,
    Write,
    ReadWrite,
    Search,
}

/// Access to a single file or directory tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolFileAccess {
    pub operation: FileOperation,
    pub path: String,
    pub recursive: bool,
}

/// A single resource access entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolResourceAccess {
    File(ToolFileAccess),
    /// Global exclusive — conflicts with everything.
    All,
}

/// Collection of resource accesses for a tool call.
pub type ToolAccesses = Vec<ToolResourceAccess>;

impl ToolResourceAccess {
    fn file_operation_writes(op: FileOperation) -> bool {
        matches!(op, FileOperation::Write | FileOperation::ReadWrite)
    }

    fn file_operations_conflict(left: FileOperation, right: FileOperation) -> bool {
        Self::file_operation_writes(left) || Self::file_operation_writes(right)
    }

    fn normalize_path(path: &str) -> String {
        // v2 `normalizePath` (toolContract.ts:228) does three things:
        //   `path.replaceAll('\\', '/').replaceAll(/\/+/g, '/')` then
        //   `toLowerCase()`, then strip one trailing slash.
        // The slash-collapse step is what makes `a//b` and `a/b` compare equal;
        // without it two writers on the same file look disjoint and get
        // scheduled in parallel. The fold is full Unicode (`toLowerCase`, not
        // `toLocaleLowerCase`) so a path differing only in the case of a
        // non-ASCII character still folds together on a case-insensitive
        // filesystem.
        let normalized = path.replace('\\', "/");
        let collapsed =
            normalized
                .chars()
                .fold(String::with_capacity(normalized.len()), |mut acc, ch| {
                    if ch != '/' || !acc.ends_with('/') {
                        acc.push(ch);
                    }
                    acc
                });
        let folded = collapsed.to_lowercase();
        if folded.len() > 1 && folded.ends_with('/') {
            folded[..folded.len() - 1].to_string()
        } else {
            folded
        }
    }

    fn file_accesses_overlap(left: &ToolFileAccess, right: &ToolFileAccess) -> bool {
        let left_path = Self::normalize_path(&left.path);
        let right_path = Self::normalize_path(&right.path);
        if left_path == right_path {
            return true;
        }
        let left_prefix = if left_path.ends_with('/') {
            left_path.clone()
        } else {
            format!("{left_path}/")
        };
        let right_prefix = if right_path.ends_with('/') {
            right_path.clone()
        } else {
            format!("{right_path}/")
        };
        (left.recursive && right_path.starts_with(&left_prefix))
            || (right.recursive && left_path.starts_with(&right_prefix))
    }

    /// Returns true if this access conflicts with another.
    pub fn conflicts_with(&self, other: &ToolResourceAccess) -> bool {
        match (self, other) {
            (ToolResourceAccess::All, _) | (_, ToolResourceAccess::All) => true,
            (ToolResourceAccess::File(l), ToolResourceAccess::File(r)) => {
                Self::file_operations_conflict(l.operation, r.operation)
                    && Self::file_accesses_overlap(l, r)
            }
        }
    }
}

/// Returns true if any access in `left` conflicts with any in `right`.
pub fn tool_accesses_conflict(left: &ToolAccesses, right: &ToolAccesses) -> bool {
    left.iter()
        .any(|l| right.iter().any(|r| l.conflicts_with(r)))
}

/// Helper to construct a read-file access.
pub fn read_file_access(path: &str) -> ToolResourceAccess {
    ToolResourceAccess::File(ToolFileAccess {
        operation: FileOperation::Read,
        path: path.to_string(),
        recursive: false,
    })
}

/// Helper to construct a write-file access.
pub fn write_file_access(path: &str) -> ToolResourceAccess {
    ToolResourceAccess::File(ToolFileAccess {
        operation: FileOperation::Write,
        path: path.to_string(),
        recursive: false,
    })
}

/// Helper to construct a read-tree access.
pub fn read_tree_access(path: &str) -> ToolResourceAccess {
    ToolResourceAccess::File(ToolFileAccess {
        operation: FileOperation::Read,
        path: path.to_string(),
        recursive: true,
    })
}

/// Helper to construct a write-tree access.
pub fn write_tree_access(path: &str) -> ToolResourceAccess {
    ToolResourceAccess::File(ToolFileAccess {
        operation: FileOperation::Write,
        path: path.to_string(),
        recursive: true,
    })
}

/// Helper to construct an all-access (globally exclusive).
pub fn all_access() -> ToolResourceAccess {
    ToolResourceAccess::All
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_read_no_conflict() {
        let a = read_file_access("/tmp/a.txt");
        let b = read_file_access("/tmp/a.txt");
        assert!(!a.conflicts_with(&b));
    }

    #[test]
    fn test_read_write_same_file_conflict() {
        let a = read_file_access("/tmp/a.txt");
        let b = write_file_access("/tmp/a.txt");
        assert!(a.conflicts_with(&b));
        assert!(b.conflicts_with(&a));
    }

    #[test]
    fn test_write_write_same_file_conflict() {
        let a = write_file_access("/tmp/a.txt");
        let b = write_file_access("/tmp/a.txt");
        assert!(a.conflicts_with(&b));
    }

    #[test]
    fn test_different_files_no_conflict() {
        let a = write_file_access("/tmp/a.txt");
        let b = write_file_access("/tmp/b.txt");
        assert!(!a.conflicts_with(&b));
    }

    #[test]
    fn test_recursive_overlap() {
        let a = read_tree_access("/tmp/project");
        let b = write_file_access("/tmp/project/src/main.rs");
        assert!(a.conflicts_with(&b));
        assert!(b.conflicts_with(&a));
    }

    #[test]
    fn test_recursive_no_overlap() {
        let a = read_tree_access("/tmp/project-a");
        let b = write_file_access("/tmp/project-b/main.rs");
        assert!(!a.conflicts_with(&b));
        assert!(!b.conflicts_with(&a));
    }

    #[test]
    fn test_all_access_conflicts_with_everything() {
        let all = all_access();
        let read = read_file_access("/tmp/x.txt");
        assert!(all.conflicts_with(&read));
        assert!(read.conflicts_with(&all));
        let write = write_file_access("/tmp/y.txt");
        assert!(all.conflicts_with(&write));
        assert!(write.conflicts_with(&all));
        let recursive = read_tree_access("/tmp");
        assert!(all.conflicts_with(&recursive));
    }

    #[test]
    fn test_all_access_self_conflict() {
        let all1 = all_access();
        let all2 = all_access();
        assert!(all1.conflicts_with(&all2));
    }

    #[test]
    fn test_readwrite_conflicts_with_both() {
        let rw = ToolResourceAccess::File(ToolFileAccess {
            operation: FileOperation::ReadWrite,
            path: "/tmp/x.txt".into(),
            recursive: false,
        });
        let read = read_file_access("/tmp/x.txt");
        let write = write_file_access("/tmp/x.txt");
        assert!(rw.conflicts_with(&read));
        assert!(read.conflicts_with(&rw));
        assert!(rw.conflicts_with(&write));
        assert!(write.conflicts_with(&rw));
    }

    #[test]
    fn test_read_read_different_dirs_no_conflict() {
        let a = read_tree_access("/a");
        let b = read_tree_access("/b");
        assert!(!a.conflicts_with(&b));
    }

    #[test]
    fn test_tool_accesses_conflict_detects() {
        let left = vec![read_file_access("/a.txt")];
        let right = vec![write_file_access("/a.txt")];
        assert!(tool_accesses_conflict(&left, &right));
    }

    #[test]
    fn test_tool_accesses_no_conflict() {
        let left = vec![read_file_access("/a.txt")];
        let right = vec![write_file_access("/b.txt")];
        assert!(!tool_accesses_conflict(&left, &right));
    }

    #[test]
    fn test_tool_accesses_mixed_conflict() {
        let left = vec![read_file_access("/a.txt"), read_file_access("/b.txt")];
        let right = vec![read_file_access("/c.txt"), write_file_access("/b.txt")];
        assert!(tool_accesses_conflict(&left, &right));
    }

    #[test]
    fn test_tool_accesses_empty_sides() {
        let left: ToolAccesses = Vec::new();
        let right = vec![write_file_access("/a.txt")];
        assert!(!tool_accesses_conflict(&left, &right));
        assert!(!tool_accesses_conflict(&right, &left));
        assert!(!tool_accesses_conflict(&left, &left));
    }

    #[test]
    fn test_normalize_path_backslashes() {
        let path = "C:\\\\Users\\\\test\\\\file.txt";
        let normalized = ToolResourceAccess::normalize_path(path);
        // This previously asserted `"c://users//test//file.txt"` — the doubled
        // separators a bare backslash→slash swap leaves behind, which is the
        // deviation v2 `normalizePath` does not have.
        assert_eq!(normalized, "c:/users/test/file.txt");
    }

    #[test]
    fn test_normalize_path_trailing_slash() {
        let path = "/tmp/project/";
        let normalized = ToolResourceAccess::normalize_path(path);
        assert_eq!(normalized, "/tmp/project");
    }

    #[test]
    fn test_normalize_path_no_change() {
        let path = "/home/user/file.rs";
        let normalized = ToolResourceAccess::normalize_path(path);
        assert_eq!(normalized, "/home/user/file.rs");
    }

    #[test]
    fn test_normalize_path_empty() {
        let normalized = ToolResourceAccess::normalize_path("");
        assert_eq!(normalized, "");
    }

    #[test]
    fn test_normalize_path_case_folding() {
        let path = "/TMP/PROJECT/File.Rs";
        let normalized = ToolResourceAccess::normalize_path(path);
        assert_eq!(normalized, "/tmp/project/file.rs");
    }

    #[test]
    fn test_search_does_not_conflict_with_read() {
        let search = ToolResourceAccess::File(ToolFileAccess {
            operation: FileOperation::Search,
            path: "/tmp/project".to_string(),
            recursive: true,
        });
        let read = read_file_access("/tmp/project/main.rs");
        assert!(!search.conflicts_with(&read));
        assert!(!read.conflicts_with(&search));
    }

    #[test]
    fn test_search_conflicts_with_write() {
        let search = ToolResourceAccess::File(ToolFileAccess {
            operation: FileOperation::Search,
            path: "/tmp/project".to_string(),
            recursive: true,
        });
        let write = write_file_access("/tmp/project/main.rs");
        assert!(search.conflicts_with(&write));
        assert!(write.conflicts_with(&search));
    }

    #[test]
    fn test_file_operation_writes() {
        assert!(!ToolResourceAccess::file_operation_writes(
            FileOperation::Read
        ));
        assert!(ToolResourceAccess::file_operation_writes(
            FileOperation::Write
        ));
        assert!(ToolResourceAccess::file_operation_writes(
            FileOperation::ReadWrite
        ));
        assert!(!ToolResourceAccess::file_operation_writes(
            FileOperation::Search
        ));
    }

    #[test]
    fn test_file_operations_conflict() {
        assert!(!ToolResourceAccess::file_operations_conflict(
            FileOperation::Read,
            FileOperation::Read
        ));
        assert!(ToolResourceAccess::file_operations_conflict(
            FileOperation::Read,
            FileOperation::Write
        ));
        assert!(ToolResourceAccess::file_operations_conflict(
            FileOperation::Write,
            FileOperation::Read
        ));
        assert!(ToolResourceAccess::file_operations_conflict(
            FileOperation::Write,
            FileOperation::Write
        ));
        assert!(ToolResourceAccess::file_operations_conflict(
            FileOperation::Read,
            FileOperation::ReadWrite
        ));
        assert!(!ToolResourceAccess::file_operations_conflict(
            FileOperation::Search,
            FileOperation::Read
        ));
        assert!(ToolResourceAccess::file_operations_conflict(
            FileOperation::Search,
            FileOperation::Write
        ));
    }

    #[test]
    fn test_file_accesses_overlap_identical() {
        let a = ToolFileAccess {
            operation: FileOperation::Read,
            path: "/tmp/x.rs".into(),
            recursive: false,
        };
        let b = ToolFileAccess {
            operation: FileOperation::Write,
            path: "/tmp/x.rs".into(),
            recursive: false,
        };
        assert!(ToolResourceAccess::file_accesses_overlap(&a, &b));
    }

    #[test]
    fn test_file_accesses_recursive_contains_child() {
        let a = ToolFileAccess {
            operation: FileOperation::Read,
            path: "/tmp/project".into(),
            recursive: true,
        };
        let b = ToolFileAccess {
            operation: FileOperation::Write,
            path: "/tmp/project/src/main.rs".into(),
            recursive: false,
        };
        assert!(ToolResourceAccess::file_accesses_overlap(&a, &b));
    }

    #[test]
    fn test_file_accesses_both_recursive_no_overlap() {
        let a = ToolFileAccess {
            operation: FileOperation::Read,
            path: "/tmp/a".into(),
            recursive: true,
        };
        let b = ToolFileAccess {
            operation: FileOperation::Write,
            path: "/tmp/b".into(),
            recursive: true,
        };
        assert!(!ToolResourceAccess::file_accesses_overlap(&a, &b));
    }

    #[test]
    fn test_file_accesses_non_recursive_no_overlap() {
        let a = ToolFileAccess {
            operation: FileOperation::Read,
            path: "/tmp/a.rs".into(),
            recursive: false,
        };
        let b = ToolFileAccess {
            operation: FileOperation::Write,
            path: "/tmp/b.rs".into(),
            recursive: false,
        };
        assert!(!ToolResourceAccess::file_accesses_overlap(&a, &b));
    }

    #[test]
    fn test_conflicts_with_recursive_child_write() {
        let a = write_tree_access("/project");
        let b = write_tree_access("/project/sub");
        assert!(a.conflicts_with(&b));
        assert!(b.conflicts_with(&a));
    }

    #[test]
    fn test_file_accesses_overlap_with_trailing_slash() {
        let a = ToolFileAccess {
            operation: FileOperation::Read,
            path: "/project/".into(),
            recursive: true,
        };
        let b = ToolFileAccess {
            operation: FileOperation::Write,
            path: "/project/src".into(),
            recursive: false,
        };
        assert!(ToolResourceAccess::file_accesses_overlap(&a, &b));
    }

    /// v2 `normalizePath` collapses runs of slashes (toolContract.ts:228).
    /// Without that step `a//b` and `a/b` name the same file but compare
    /// unequal, so two writers on one path were scheduled in parallel.
    #[test]
    fn test_normalize_path_collapses_repeated_slashes() {
        assert_eq!(
            ToolResourceAccess::normalize_path("C:/proj//file.ts"),
            ToolResourceAccess::normalize_path("C:/proj/file.ts")
        );
        assert_eq!(
            ToolResourceAccess::normalize_path("/a///b////c"),
            ToolResourceAccess::normalize_path("/a/b/c")
        );
        assert_eq!(
            ToolResourceAccess::normalize_path("C:\\\\proj\\\\\\file.ts"),
            "c:/proj/file.ts"
        );
    }

    /// The end-to-end consequence: a doubled separator must not hide a write
    /// conflict.
    #[test]
    fn test_write_write_conflict_across_repeated_slashes() {
        let a = write_file_access("C:/proj//file.ts");
        let b = write_file_access("C:/proj/file.ts");
        assert!(a.conflicts_with(&b));
        assert!(b.conflicts_with(&a));
    }

    /// Same for the tree case, where the overlap test uses prefix matching.
    #[test]
    fn test_recursive_overlap_across_repeated_slashes() {
        let a = write_tree_access("/project//");
        let b = write_tree_access("/project/sub");
        assert!(a.conflicts_with(&b));
        assert!(b.conflicts_with(&a));
    }

    /// The fold is full Unicode, matching v2's `toLowerCase()`.
    #[test]
    fn test_normalize_path_folds_non_ascii_case() {
        assert_eq!(
            ToolResourceAccess::normalize_path("/tmp/Ä.txt"),
            ToolResourceAccess::normalize_path("/tmp/ä.txt")
        );
    }

    /// Collapsing must not turn distinct paths into one.
    #[test]
    fn test_repeated_slash_collapse_keeps_paths_distinct() {
        assert_ne!(
            ToolResourceAccess::normalize_path("/a//b"),
            ToolResourceAccess::normalize_path("/a/bc")
        );
        let a = write_file_access("/proj//file.ts");
        let b = write_file_access("/proj/file.ts.bak");
        assert!(!a.conflicts_with(&b));
    }
}

/// The trait that all executable tools must implement.
pub trait ExecutableTool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn resolve_execution(
        &self,
        input: serde_json::Value,
    ) -> Result<ToolExecution, Box<dyn std::error::Error>>;
}

// ── RunTurnInput ───────────────────────────────────────────────────────────

/// Input to the `run_turn` function.
pub struct RunTurnInput<'a> {
    pub turn_id: String,
    /// The agent running this turn (v2 `IAgentScopeContext.agentId`).
    ///
    /// v2 injects it per agent scope and reads it when constructing every
    /// turn and tool event (`loopService.ts:1426-1455`,
    /// `toolExecutorService.ts:578-588`). This fork carries it explicitly for
    /// the same reason: `CALLER_AGENT_ID` is a tokio task-local, and a turn's
    /// tool execution runs outside the task the subagent turn runner scoped
    /// it on, so reading the task-local there yields the root agent. An event
    /// that names the wrong agent is announced in the main transcript, and its
    /// card truncates the main agent's own reasoning mid-sentence.
    pub agent_id: String,
    pub llm: &'a dyn LLM,
    pub messages: Vec<LLMMessage>,
    pub tools: &'a [&'a dyn ExecutableTool],
    /// Tool definitions passed from the JS side. These are sent to the
    /// LLM proxy so the JS host can include them in the actual LLM call.
    pub tool_defs: Vec<ToolInfo>,
    pub max_steps: u32,
    /// LLM retry attempts per step (v2 `loopControl.maxAttemptsPerStep`).
    /// `None` uses the engine default of 10 (`DEFAULT_MAX_RETRY_ATTEMPTS`).
    pub max_attempts: Option<u32>,
    /// Context window the host resolved for the active model. `None` keeps the
    /// engine's own default budget.
    pub max_context_tokens: Option<u32>,
    /// Total requests one compaction round may issue (v2 #3750
    /// `loopControl.compactionMaxAttempts`). `None` keeps the engine default
    /// of [`crate::compaction::DEFAULT_COMPACTION_MAX_ATTEMPTS`].
    pub compaction_max_attempts: Option<u32>,
    /// The live permission mode for this turn, when the host carries a policy
    /// snapshot. Drives the permission-mode reminders (v2
    /// `PermissionModeInjection`); `None` leaves them off.
    pub permission_mode: Option<crate::permission::PermissionMode>,
    /// Optional goal context for budget-aware execution and steering.
    /// When present, the loop checks budgets before each step and injects
    /// steering text into the system prompt.
    pub goal: Option<GoalContext>,
    /// Optional cancellation flag. When set to true, the loop aborts
    /// before the next step with `LoopTurnStopReason::Aborted`.
    pub cancellation: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// Optional hook guard for turn-lifecycle hook dispatch
    /// (`UserPromptSubmit` at the turn head, `PreCompact` before trimming,
    /// `Stop` on a clean text finish; `PreToolUse` / `PostToolUse` gating
    /// lives on the tool callbacks instead). `None` skips those dispatches —
    /// subagent turns always pass `None`, so lifecycle hooks are main-turn
    /// scoped while `PreToolUse` gating still applies inside subagents.
    pub hook_guard: Option<std::sync::Arc<crate::tools::external_hooks::HookGuard>>,
    /// Resolves the media references the request carries, and weighs the
    /// result against the request media budget. `None` leaves references
    /// unresolved — a caller with no daemon file store behind it.
    pub media: Option<&'a crate::llm::media_resolver::MediaResolver>,
    /// The media keys an earlier turn already omitted (v2
    /// `media.budgetDropped` is agent state; the engine's turn loop is
    /// stateless, so the caller owns the record and the budget writes every
    /// new omission through to it).
    pub media_dropped: Option<kimi_core::types::DroppedMedia>,
    /// The session's media recovery record (v2 #4413 `llmRequester.mediaRecovery`):
    /// a degrade/strip the provider forced on an earlier turn is re-applied to
    /// this turn's requests instead of re-sending the full media and paying
    /// the rejection round-trip again. `None` makes every turn start from the
    /// full projection — the pre-#4413 behavior.
    pub media_recovery: Option<kimi_core::types::SharedMediaRecovery>,
    /// The pipeline's native toolset, when native tools are on. The
    /// turn-start disclosure announcement (v2 `toolSelectAnnouncementsService`)
    /// reads the deferred set and the announced diff from it; `None` skips
    /// the provider — host-proxy turns and subagent turns.
    pub toolset: Option<std::sync::Arc<crate::tools::NativeToolset>>,
    /// The previous turn ended with `LoopTurnStopReason::Aborted` (the
    /// caller owns that outcome; the turn loop is stateless). When true, the
    /// interruption reminder (v2 `interruptionReminderService`) injects once
    /// at this turn's head — any partial output from the aborted turn is
    /// incomplete. Host-side aborts that are not user cancellations should
    /// not set this until the cancel reason travels with the request.
    pub previous_turn_aborted: bool,
}

/// Per-step prompt-cache telemetry the turn loop collects, shared out of band
/// with the caller (D14(a)).
///
/// [`TurnResult`] already carries these three vectors on every **completed**
/// turn, so a caller that gets `Ok` never needs this record. It exists for the
/// hard-failure path: when `run_turn` returns `Err` there is no `TurnResult`,
/// and without a sink every step that completed before the failure would be
/// dropped. A caller that supplies a [`TurnObservationsSink`] receives those
/// steps as they are produced, in the same order and with the same reset
/// indices the `Ok` path would report.
#[derive(Debug, Clone, Default)]
pub struct TurnObservations {
    pub cache_observations: Vec<crate::session::cache_observation::CacheObservation>,
    pub cache_probe_requests: Vec<crate::session::cache_observation::CacheProbeRequest>,
    pub cache_observation_resets: Vec<usize>,
}

/// Shared handle the turn loop appends observations to as each step completes.
/// See [`TurnObservations`].
pub type TurnObservationsSink = std::sync::Arc<std::sync::Mutex<TurnObservations>>;

// ── Step-level types ───────────────────────────────────────────────────────

/// Result of a single step.
#[derive(Debug, Clone)]
pub struct StepResult {
    pub usage: TokenUsage,
    pub stop_reason: LoopStepStopReason,
    /// Assistant text produced in this step (may be empty on the host-proxy
    /// path, where the host owns the transcript).
    pub content: String,
    /// Provider reasoning blocks (Anthropic thinking+signature, OpenAI
    /// reasoning, Gemini thought signatures). Carried so the turn loop can
    /// fold them back into the assistant history — providers that attested
    /// the reasoning require the exact blocks (signature included) on the
    /// next request, otherwise the follow-up call is rejected.
    pub thinking: Vec<ContentBlock>,
    /// LLM attempts used by this step (1 = no retry).
    pub attempts: u32,
    /// Provider finish reason for this step (`stop`, `length`, `max_tokens`,
    /// `tool_calls`, `content_filter`, …). Consumed by the turn loop to map
    /// provider-side truncation/filtering onto turn-level stop reasons.
    pub finish_reason: Option<String>,
    /// The step's LLM request timing (v2 `ModelRequestTiming`, upstream
    /// #3938); `None` when the transport did not measure (host-proxy).
    pub timing: Option<kimi_core::timing::LlmTiming>,
}

/// Reasons a single step can stop.
#[derive(Debug, Clone)]
pub enum LoopStepStopReason {
    /// The LLM returned a complete response (no more tool calls).
    Complete,
    /// The LLM made tool calls that need to be executed.
    ToolCalls(Vec<ToolCall>),
}

#[cfg(test)]
mod goal_tests {
    use super::*;

    fn active_goal() -> GoalContext {
        GoalContext {
            goal_id: "g".into(),
            objective: "test".into(),
            status: GoalStatus::Active,
            token_budget: Some(1000),
            turn_budget: Some(10),
            tokens_used: 0,
            turns_used: 0,
            wall_clock_budget_ms: None,
            wall_clock_ms: 0,
            updated_at: 0,
            terminal_reason: None,
        }
    }

    #[test]
    fn test_is_active() {
        assert!(GoalStatus::Active.is_active());
        assert!(!GoalStatus::Paused.is_active());
        assert!(!GoalStatus::Blocked.is_active());
        assert!(!GoalStatus::Complete.is_active());
        assert!(!GoalStatus::BudgetLimited.is_active());
        assert!(!GoalStatus::UsageLimited.is_active());
    }

    #[test]
    fn test_would_exceed_budget_token_at_limit() {
        let mut g = active_goal();
        g.tokens_used = 1000;
        assert!(g.would_exceed_budget(0, 0, 0), "at limit should exceed");
    }

    #[test]
    fn test_would_exceed_budget_token_near_limit() {
        let mut g = active_goal();
        g.tokens_used = 900;
        // 900 + 50 = 950 < 1000, should NOT exceed
        assert!(!g.would_exceed_budget(50, 0, 0));
        // 900 + 100 = 1000 >= 1000, should exceed
        assert!(g.would_exceed_budget(100, 0, 0));
    }

    #[test]
    fn test_would_exceed_budget_turn_at_limit() {
        let mut g = active_goal();
        g.turns_used = 10;
        assert!(g.would_exceed_budget(0, 0, 0));
    }

    #[test]
    fn test_would_exceed_budget_turn_near_limit() {
        let mut g = active_goal();
        g.turns_used = 8;
        assert!(
            !g.would_exceed_budget(0, 1, 0),
            "8+1=9 < 10, should not exceed"
        );
        assert!(
            g.would_exceed_budget(0, 2, 0),
            "8+2=10 >= 10, should exceed"
        );
    }

    #[test]
    fn test_would_exceed_budget_no_budgets() {
        let g = GoalContext {
            goal_id: "g".into(),
            objective: "test".into(),
            status: GoalStatus::Active,
            token_budget: None,
            turn_budget: None,
            tokens_used: 99999,
            turns_used: 99999,
            wall_clock_budget_ms: None,
            wall_clock_ms: 0,
            updated_at: 0,
            terminal_reason: None,
        };
        assert!(!g.would_exceed_budget(99999, 99999, 0));
    }

    #[test]
    fn test_budget_fraction_no_budgets() {
        let g = GoalContext {
            goal_id: "g".into(),
            objective: "test".into(),
            status: GoalStatus::Active,
            token_budget: None,
            turn_budget: None,
            tokens_used: 0,
            turns_used: 0,
            wall_clock_budget_ms: None,
            wall_clock_ms: 0,
            updated_at: 0,
            terminal_reason: None,
        };
        assert_eq!(g.budget_fraction(100, 5, 0), 0.0);
    }

    #[test]
    fn test_budget_fraction_half() {
        let g = active_goal(); // token_budget=1000, turn_budget=10
        // 500/1000 = 0.5, 5/10 = 0.5 → max = 0.5
        assert_eq!(g.budget_fraction(500, 5, 0), 0.5);
    }

    #[test]
    fn test_budget_fraction_near_limit() {
        let g = active_goal();
        // 750/1000 = 0.75, 5/10 = 0.5 → max = 0.75
        assert!(g.budget_fraction(750, 5, 0) >= 0.75);
    }

    #[test]
    fn test_budget_fraction_over_limit() {
        let g = active_goal();
        // 1500/1000 = 1.5
        assert!(g.budget_fraction(1500, 0, 0) > 1.0);
    }

    #[test]
    fn test_wall_clock_budget_at_limit() {
        let mut g = active_goal();
        g.wall_clock_budget_ms = Some(6000);
        assert!(
            !g.would_exceed_budget(0, 0, 5000),
            "within wall-clock budget"
        );
        assert!(
            g.would_exceed_budget(0, 0, 6000),
            "at wall-clock limit should exceed"
        );
        assert!(
            g.would_exceed_budget(0, 0, 7000),
            "beyond wall-clock limit should exceed"
        );
        assert_eq!(g.budget_fraction(0, 0, 3000), 0.5);
    }

    #[test]
    fn test_wall_clock_budget_with_cumulative_elapsed() {
        let g = GoalContext {
            goal_id: "g".into(),
            objective: "test".into(),
            status: GoalStatus::Active,
            token_budget: None,
            turn_budget: None,
            wall_clock_budget_ms: Some(10_000),
            wall_clock_ms: 9000,
            tokens_used: 0,
            turns_used: 0,
            updated_at: 0,
            terminal_reason: None,
        };
        assert!(
            g.would_exceed_budget(0, 0, 1000),
            "cumulative elapsed pushes over budget"
        );
        assert!(
            !g.would_exceed_budget(0, 0, 999),
            "just under cumulative budget"
        );
    }

    #[test]
    fn test_no_wall_clock_budget_never_triggers() {
        let g = active_goal();
        assert!(
            !g.would_exceed_budget(0, 0, i64::MAX / 2),
            "no wall-clock budget configured"
        );
    }

    #[test]
    fn test_budget_fraction_picks_max() {
        let g = active_goal(); // token=1000, turn=10
        // tokens: 100/1000 = 0.1, turns: 9/10 = 0.9 → max = 0.9
        let frac = g.budget_fraction(100, 9, 0);
        assert!((frac - 0.9).abs() < 0.001, "expected 0.9, got {frac}");
    }
}
