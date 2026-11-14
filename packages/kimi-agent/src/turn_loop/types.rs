use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::rpc::types::TokenUsage;
use crate::tool_input_display::ToolInputDisplay;

pub use crate::rpc::types::{
    ContentBlock, GoalContext, GoalStatus, TelemetryContext, ToolDelivery, ToolInfo,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnResult {
    pub stop_reason: LoopTurnStopReason,

    pub steps: u32,

    pub usage: TokenUsage,

    pub events_emitted: u32,

    pub llm_retries: u32,

    #[serde(default)]
    pub llm_transport: String,

    #[serde(default)]
    pub native_tool_calls: u32,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<LLMMessage>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_request_usage: Option<RequestMeasurement>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_usage: Option<TokenUsage>,

    #[serde(default)]
    pub tool_schema_tokens: u32,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_hook_continuation: Option<String>,

    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub gate_blocked: bool,

    #[serde(skip)]
    pub cache_observations: Vec<crate::session::cache_observation::CacheObservation>,

    #[serde(skip)]
    pub cache_probe_requests: Vec<crate::session::cache_observation::CacheProbeRequest>,

    #[serde(skip)]
    pub cache_observation_resets: Vec<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestMeasurement {
    pub usage: TokenUsage,

    pub input_messages: usize,

    pub spliced_after: bool,
}

impl TurnResult {
    pub fn with_last_request_usage(mut self, usage: Option<RequestMeasurement>) -> Self {
        self.last_request_usage = usage;
        self
    }

    pub fn with_gate_blocked(mut self) -> Self {
        self.gate_blocked = true;
        self
    }

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LoopTurnStopReason {
    EndTurn,
    MaxTokens,
    Filtered,
    Paused,
    Unknown,
    Aborted,

    BudgetLimited,

    RepeatBreaker,

    MaxSteps,
}

pub use kimi_core::types::{
    ArgsParseError, LLM, LLMChatParams, LLMChatResponse, LLMMessage, ToolCall,
};

pub enum ToolExecution {
    Runnable(RunnableToolExecution),
    Error(ExecutableToolErrorResult),
}

pub struct RunnableToolExecution {
    pub accesses: ToolAccesses,
    pub approval_rule: String,

    pub display: Option<ToolInputDisplay>,

    pub execute: ToolExecutor,
}

pub type ToolExecutor = Box<
    dyn FnOnce(ToolExecContext) -> Result<ExecutableToolResult, Box<dyn std::error::Error>> + Send,
>;

#[derive(Debug, Clone)]
pub struct ToolExecContext {
    pub turn_id: String,
    pub tool_call_id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolResultSpill {
    pub output_path: Option<String>,

    pub total_chars: Option<usize>,

    pub suffix: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ExecutableToolResult {
    pub stop_turn: bool,
    pub content: String,
    pub is_error: bool,

    pub note: Option<String>,

    pub delivery: Option<ToolDelivery>,

    pub parts: Vec<ContentBlock>,

    pub display: Option<Box<ToolInputDisplay>>,

    pub spill: Option<ToolResultSpill>,
}

#[derive(Debug, Clone)]
pub struct ExecutableToolErrorResult {
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileOperation {
    Read,
    Write,
    ReadWrite,
    Search,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolFileAccess {
    pub operation: FileOperation,
    pub path: String,
    pub recursive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolResourceAccess {
    File(ToolFileAccess),

    All,
}

pub type ToolAccesses = Vec<ToolResourceAccess>;

impl ToolResourceAccess {
    fn file_operation_writes(op: FileOperation) -> bool {
        matches!(op, FileOperation::Write | FileOperation::ReadWrite)
    }

    fn file_operations_conflict(left: FileOperation, right: FileOperation) -> bool {
        Self::file_operation_writes(left) || Self::file_operation_writes(right)
    }

    fn normalize_path(path: &str) -> String {
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

pub fn tool_accesses_conflict(left: &ToolAccesses, right: &ToolAccesses) -> bool {
    left.iter()
        .any(|l| right.iter().any(|r| l.conflicts_with(r)))
}

pub fn read_file_access(path: &str) -> ToolResourceAccess {
    ToolResourceAccess::File(ToolFileAccess {
        operation: FileOperation::Read,
        path: path.to_string(),
        recursive: false,
    })
}

pub fn write_file_access(path: &str) -> ToolResourceAccess {
    ToolResourceAccess::File(ToolFileAccess {
        operation: FileOperation::Write,
        path: path.to_string(),
        recursive: false,
    })
}

pub fn read_tree_access(path: &str) -> ToolResourceAccess {
    ToolResourceAccess::File(ToolFileAccess {
        operation: FileOperation::Read,
        path: path.to_string(),
        recursive: true,
    })
}

pub fn write_tree_access(path: &str) -> ToolResourceAccess {
    ToolResourceAccess::File(ToolFileAccess {
        operation: FileOperation::Write,
        path: path.to_string(),
        recursive: true,
    })
}

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

    #[test]
    fn test_write_write_conflict_across_repeated_slashes() {
        let a = write_file_access("C:/proj//file.ts");
        let b = write_file_access("C:/proj/file.ts");
        assert!(a.conflicts_with(&b));
        assert!(b.conflicts_with(&a));
    }

    #[test]
    fn test_recursive_overlap_across_repeated_slashes() {
        let a = write_tree_access("/project//");
        let b = write_tree_access("/project/sub");
        assert!(a.conflicts_with(&b));
        assert!(b.conflicts_with(&a));
    }

    #[test]
    fn test_normalize_path_folds_non_ascii_case() {
        assert_eq!(
            ToolResourceAccess::normalize_path("/tmp/Ä.txt"),
            ToolResourceAccess::normalize_path("/tmp/ä.txt")
        );
    }

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

pub trait ExecutableTool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn resolve_execution(
        &self,
        input: serde_json::Value,
    ) -> Result<ToolExecution, Box<dyn std::error::Error>>;
}

pub struct RunTurnInput<'a> {
    pub turn_id: String,

    pub agent_id: String,
    pub llm: &'a dyn LLM,
    pub messages: Vec<LLMMessage>,
    pub tools: &'a [&'a dyn ExecutableTool],

    pub tool_defs: Vec<ToolInfo>,
    pub max_steps: u32,

    pub max_attempts: Option<u32>,

    pub max_context_tokens: Option<u32>,

    pub compaction_max_attempts: Option<u32>,

    pub permission_mode: Option<crate::permission::PermissionMode>,

    pub goal: Option<GoalContext>,

    pub cancellation: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,

    pub hook_guard: Option<std::sync::Arc<crate::tools::external_hooks::HookGuard>>,

    pub media: Option<&'a crate::llm::media_resolver::MediaResolver>,

    pub media_dropped: Option<kimi_core::types::DroppedMedia>,

    pub media_recovery: Option<kimi_core::types::SharedMediaRecovery>,

    /// `[llm_requester].request_byte_budget` (v2 `resolveLlmRequestByteBudget`):
    /// the whole-request UTF-8 byte budget the projector checks before sending.
    /// `u64::MAX` disables the proactive eviction (tests).
    pub llm_request_byte_budget: u64,

    pub toolset: Option<std::sync::Arc<crate::tools::NativeToolset>>,

    pub previous_turn_aborted: bool,
}

#[derive(Debug, Clone, Default)]
pub struct TurnObservations {
    pub cache_observations: Vec<crate::session::cache_observation::CacheObservation>,
    pub cache_probe_requests: Vec<crate::session::cache_observation::CacheProbeRequest>,
    pub cache_observation_resets: Vec<usize>,
}

pub type TurnObservationsSink = std::sync::Arc<std::sync::Mutex<TurnObservations>>;

#[derive(Debug, Clone)]
pub struct StepResult {
    pub usage: TokenUsage,
    pub stop_reason: LoopStepStopReason,

    pub content: String,

    pub thinking: Vec<ContentBlock>,

    pub attempts: u32,

    pub finish_reason: Option<String>,
    pub trace_id: Option<String>,

    pub timing: Option<kimi_core::timing::LlmTiming>,
}

#[derive(Debug, Clone)]
pub enum LoopStepStopReason {
    Complete,

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

        assert!(!g.would_exceed_budget(50, 0, 0));

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
        let g = active_goal();

        assert_eq!(g.budget_fraction(500, 5, 0), 0.5);
    }

    #[test]
    fn test_budget_fraction_near_limit() {
        let g = active_goal();

        assert!(g.budget_fraction(750, 5, 0) >= 0.75);
    }

    #[test]
    fn test_budget_fraction_over_limit() {
        let g = active_goal();

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
        let g = active_goal();

        let frac = g.budget_fraction(100, 9, 0);
        assert!((frac - 0.9).abs() < 0.001, "expected 0.9, got {frac}");
    }
}
