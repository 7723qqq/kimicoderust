use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::i18n::{LocalizedText, i18n_params};
use crate::native::shell_path_bridge::ShellPathBridge;

/// One structured progress update from a running tool (v2 `ToolUpdate`,
/// `toolContract.ts:57-66`). `kind` is one of `stdout` / `stderr` /
/// `progress` / `status` / `custom`; `replace` tells the host renderer to
/// replace the previous line instead of appending.
#[derive(Debug, Clone)]
pub struct ToolUpdate<'a> {
    pub kind: &'a str,
    pub text: &'a str,
    pub percent: Option<f64>,
    pub custom_kind: Option<&'a str>,
    pub custom_data: Option<&'a Value>,
    pub replace: bool,
}

impl<'a> ToolUpdate<'a> {
    /// A plain stdout/stderr chunk: the only shape the native shell tools
    /// produce (v2 bashTool/powershellTool `onUpdate({ kind, text })`).
    pub fn text(kind: &'a str, text: &'a str) -> Self {
        Self {
            kind,
            text,
            percent: None,
            custom_kind: None,
            custom_data: None,
            replace: false,
        }
    }
}

pub type OutputUpdate<'a> = &'a (dyn Fn(&ToolUpdate<'_>) + Send + Sync);

use crate::turn_loop::types::{ExecutableToolResult, ToolInfo};

const READ_MAX_BYTES: u64 = 10 * 1024 * 1024;

const MAX_TASK_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

const MAX_RETAINED_OUTPUT_BYTES: usize = 1024 * 1024;

fn output_limit_reason() -> String {
    LocalizedText::with_params(
        "engine.background.outputLimitExceeded",
        i18n_params!["mib" => MAX_TASK_OUTPUT_BYTES / (1024 * 1024)],
    )
    .render()
}

const GREP_TIME_BUDGET: Duration = Duration::from_secs(20);

const GREP_WALK_THREADS: usize = 4;

const GREP_MAX_OUTPUT_BYTES: usize = 512 * 1024;

const GREP_HEAD_LIMIT: usize = 250;

const VCS_DIRECTORIES_TO_EXCLUDE: [&str; 6] = [".git", ".svn", ".hg", ".bzr", ".jj", ".sl"];

const BASH_MAX_SECONDS: u64 = 300;

const BASH_TASK_DEFAULT_TIMEOUT_S: u64 = 600;

const BASH_TASK_MAX_SECONDS: u64 = 86_400;

const PROGRESS_MIN_INTERVAL_MS: u64 = 50;

struct ProgressThrottle {
    stdout: std::sync::Mutex<Option<std::time::Instant>>,
    stderr: std::sync::Mutex<Option<std::time::Instant>>,
}

impl ProgressThrottle {
    fn new() -> Self {
        Self {
            stdout: std::sync::Mutex::new(None),
            stderr: std::sync::Mutex::new(None),
        }
    }

    fn admit(&self, kind: &str) -> bool {
        let slot = match kind {
            "stderr" => &self.stderr,
            _ => &self.stdout,
        };
        let now = std::time::Instant::now();
        let Ok(mut last) = slot.lock() else {
            return false;
        };
        match *last {
            Some(previous)
                if now.duration_since(previous).as_millis() < PROGRESS_MIN_INTERVAL_MS as u128 =>
            {
                false
            }
            _ => {
                *last = Some(now);
                true
            }
        }
    }
}

pub mod agent_tool;
mod args_format;
pub mod args_validate;
pub mod ask_user_question;
mod bash_exec;
pub mod bash_null_redirect;
pub mod bash_target_dirs;
pub mod before_execute;
pub mod core_tool_defs;
pub mod create_goal;
pub mod cron_tools;
mod dispatch;
pub mod encoding;
pub mod exit_plan_mode;
pub mod external_hooks;
pub mod fetch_url;
mod file_ops;
pub mod get_goal;
pub mod github;
pub mod goal_guard;
pub mod goal_tools;
pub mod knowledge_tool;
pub mod list_directory;
pub mod lsp_tool;
pub mod memory_action;
pub mod memory_delete;
pub mod memory_filing;
pub mod memory_history;
pub mod memory_list;
pub mod memory_list_handler;
pub mod memory_lock;
pub mod memory_paths;
pub mod memory_read;
pub mod memory_read_handler;
pub mod memory_repo;
pub mod memory_search;
pub mod memory_store;
pub mod memory_tool;
pub mod memory_write;
pub mod memory_write_handler;
pub mod mode_mutex;
pub mod moonshot_service;
pub mod plan_mode;
pub mod powershell;
pub mod project_identity;
pub mod read_media;
mod regex_js;
pub mod registry;
pub mod resolve;
pub mod run_code;
pub mod sandbox;
mod search;
pub mod select_tools;
pub mod session_query;
pub mod shell_results;
pub mod skill;
pub mod spec_mode;
pub mod stale_guard;
pub mod stream_sink;
pub mod subagent_tools;
pub mod swarm_tool;
pub mod task_format;
pub mod task_tools;
pub mod todo_item;
pub mod todo_list;
pub mod tool_dedupe;
pub mod tool_display;
pub mod tool_policy;
pub mod tool_retriever;
pub mod tool_select_state;
mod toolset;
pub mod tower;
pub mod web_search;
pub mod workflow;

mod grep_types;

pub fn is_mutating_tool(tool_name: &str) -> bool {
    matches!(
        tool_name.to_ascii_lowercase().as_str(),
        "write" | "edit" | "bash" | "powershell"
    )
}

pub const NATIVE_TOOL_NAMES: &[&str] = &[
    "read",
    "grep",
    "glob",
    "write",
    "edit",
    "bash",
    "powershell",
    "fetchurl",
    "fetch_url",
    "websearch",
    "web_search",
    "lsp",
    "invokesubagent",
    "invoke_subagent",
    "managesubagents",
    "manage_subagents",
    "definesubagent",
    "define_subagent",
    "askuserquestion",
    "ask_user_question",
    "getgoal",
    "get_goal",
    "todolist",
    "todo_list",
    "cronlist",
    "cron_list",
    "croncreate",
    "cron_create",
    "crondelete",
    "cron_delete",
    "updategoal",
    "update_goal",
    "setgoalbudget",
    "set_goal_budget",
    "tasklist",
    "task_list",
    "taskoutput",
    "task_output",
    "taskstop",
    "task_stop",
    "taskwait",
    "task_wait",
    "exitplanmode",
    "exit_plan_mode",
    "enterspecmode",
    "enter_spec_mode",
    "exitspecmode",
    "exit_spec_mode",
    "creategoal",
    "create_goal",
    "skill",
    "knowledge",
    "memory",
    "workflow",
    "run_code",
    "runcode",
    "sessionquery",
    "session_query",
    "notifyuser",
    "notify_user",
    "agent",
    "agentswarm",
    "agent_swarm",
    "waitfor",
    "wait_for",
    "towerinit",
    "tower_init",
    "towerplan",
    "tower_plan",
    "towerspawn",
    "tower_spawn",
    "towermerge",
    "tower_merge",
    "towerteardown",
    "tower_teardown",
    "towersend",
    "tower_send",
    "towerinbox",
    "tower_inbox",
    "towerfinding",
    "tower_finding",
    "towerreview",
    "tower_review",
    "towermission",
    "tower_mission",
    "towerstatus",
    "tower_status",
    "select_tools",
];

pub fn is_native_tool_name(tool_name: &str) -> bool {
    is_builtin_tool_name(tool_name) || tool_name.starts_with("mcp__")
}

pub fn is_builtin_tool_name(tool_name: &str) -> bool {
    let lowered = tool_name.to_ascii_lowercase();
    NATIVE_TOOL_NAMES.contains(&lowered.as_str()) || github::is_github_tool(tool_name)
}

pub fn canonical_tool_name<'a>(name: &'a str, table: &'a [ToolInfo]) -> &'a str {
    table
        .iter()
        .find(|tool| tool.name.eq_ignore_ascii_case(name))
        .map_or(name, |tool| tool.name.as_str())
}

tokio::task_local! {



















    pub static CALLER_AGENT_ID: String;


    pub static CURRENT_CONVERSATION_HISTORY: std::sync::Arc<std::sync::Mutex<Vec<crate::turn_loop::types::LLMMessage>>>;
}

#[derive(Debug, Clone)]
pub struct Sandbox {
    primary: PathBuf,
    extra: Vec<PathBuf>,
}

impl Sandbox {
    pub fn new(primary: PathBuf) -> Self {
        Self {
            primary,
            extra: Vec::new(),
        }
    }

    pub fn with_extra(mut self, extra: impl IntoIterator<Item = PathBuf>) -> Self {
        for root in extra {
            let Ok(root) = std::fs::canonicalize(&root) else {
                continue;
            };
            if !root.is_dir() || self.contains(&root) {
                continue;
            }
            self.extra.push(root);
        }
        self
    }

    pub fn primary(&self) -> &Path {
        &self.primary
    }

    pub fn extra(&self) -> &[PathBuf] {
        &self.extra
    }

    pub fn roots(&self) -> impl Iterator<Item = &Path> {
        std::iter::once(self.primary.as_path()).chain(self.extra.iter().map(PathBuf::as_path))
    }

    pub fn contains(&self, path: &Path) -> bool {
        path.starts_with(&self.primary) || self.extra.iter().any(|root| path.starts_with(root))
    }
}

pub(crate) fn user_home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

#[cfg(windows)]
pub(crate) fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    let raw = path.to_string_lossy();
    if let Some(rest) = raw.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = raw.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path
    }
}

#[cfg(not(windows))]
pub(crate) fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    path
}

pub struct NativeToolset {
    root: PathBuf,

    extra_roots: Vec<PathBuf>,

    skill_dirs: Vec<PathBuf>,

    plugin_skill_dirs: Vec<crate::skills::PluginSkillDir>,

    merge_all_available_skills: bool,

    shell: Option<String>,
    subagent_manager: Option<std::sync::Arc<crate::subagent::SubagentManager>>,
    mcp_manager: Option<std::sync::Arc<crate::mcp::McpManager>>,

    subagent_timeout_ms: Option<u64>,
    parent_cancel: Option<crate::subagent::types::ParentCancel>,

    parent_cancel_slot: Option<Arc<std::sync::Mutex<Option<crate::subagent::types::ParentCancel>>>>,

    steer_slot: Option<Arc<std::sync::Mutex<Option<crate::subagent::types::ParentCancel>>>>,

    callbacks: Option<std::sync::Arc<dyn crate::callbacks::HostCallbacks>>,

    github_credentials: Option<github::GitHubCredentials>,
    caller_agent_id: Option<String>,

    permission_mode: Option<crate::permission::PermissionMode>,
    session_id: Option<String>,
    task_runner: Option<std::sync::Arc<crate::storage::TaskRunner>>,

    tower_wake_runtime: Arc<std::sync::Mutex<Option<Arc<tower::wake::TowerWakeRuntime>>>>,

    file_history: Arc<std::sync::Mutex<Option<FileHistoryCtx>>>,

    session_store:
        Arc<std::sync::Mutex<Option<Arc<crate::session::sqlite_store::SqliteSessionStore>>>>,

    turn_id: Arc<std::sync::atomic::AtomicUsize>,

    loaded_tools: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,

    selection_failures: Arc<std::sync::Mutex<crate::tools::select_tools::SelectionFailureTracker>>,

    wait_tally: Arc<std::sync::Mutex<crate::tools::task_tools::WaitTally>>,

    progress_tracker: Arc<std::sync::Mutex<crate::turn_loop::progress_track::ProgressTracker>>,

    tools_filter: Option<tool_policy::ToolsFilter>,

    secondary_model: Option<std::sync::Arc<crate::subagent::secondary::SecondaryModelRuntime>>,

    image_read_byte_budget: Option<u64>,

    image_max_edge_px: Option<u32>,

    model_capabilities: Option<Vec<String>>,

    tool_select_enabled: bool,

    initial_tool_names: std::sync::Arc<std::sync::Mutex<Option<std::collections::HashSet<String>>>>,

    bash_auto_background: bool,

    bash_task_timeout_s: Option<u64>,

    tower_max_running_tasks: Option<u32>,

    provider: Option<String>,

    shell_bridge: std::sync::Arc<crate::native::shell_path_bridge::ShellPathBridge>,
}

#[derive(Clone)]
struct FileHistoryCtx {
    store: std::sync::Arc<crate::session::sqlite_store::SqliteSessionStore>,
    session_id: String,
}

thread_local! {



    static FILE_HISTORY: std::cell::RefCell<Option<FileHistoryCtx>> =
        const { std::cell::RefCell::new(None) };





    static TURN_ID: std::cell::RefCell<usize> =
        const { std::cell::RefCell::new(0) };
}

fn bool_arg(args: &Value, key: &str, default: bool) -> Result<bool, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(value) => value.as_bool().ok_or_else(|| {
            LocalizedText::with_params(
                "engine.tools.boolInvalid",
                i18n_params!["key" => key, "value" => value],
            )
            .render()
        }),
    }
}

fn u64_arg(args: &Value, key: &str, default: u64) -> Result<u64, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(value) => value.as_u64().ok_or_else(|| {
            LocalizedText::with_params(
                "engine.tools.nonNegativeIntInvalid",
                i18n_params!["key" => key, "value" => value],
            )
            .render()
        }),
    }
}

fn parse_integer_arg(value: &Value) -> Option<i64> {
    if let Some(n) = value.as_i64() {
        return Some(n);
    }
    let numeric = match value {
        Value::String(s) => s.trim().parse::<f64>().ok(),
        Value::Number(n) => n.as_f64(),
        _ => None,
    }?;
    if !numeric.is_finite() || numeric.fract() != 0.0 {
        return None;
    }
    if numeric < i64::MIN as f64 || numeric > i64::MAX as f64 {
        return None;
    }
    Some(numeric as i64)
}

fn blocking_pool_failure(mutating: bool, message: String) -> Option<ExecutableToolResult> {
    if mutating {
        Some(err_result(format!("native tool task failed: {message}")))
    } else {
        None
    }
}

fn ok_result(content: String) -> ExecutableToolResult {
    ExecutableToolResult {
        spill: None,
        parts: Vec::new(),
        delivery: None,
        stop_turn: false,
        content,
        is_error: false,
        note: None,
        display: None,
    }
}

fn err_result(content: String) -> ExecutableToolResult {
    ExecutableToolResult {
        spill: None,
        parts: Vec::new(),
        delivery: None,
        stop_turn: false,
        content,
        is_error: true,
        note: None,
        display: None,
    }
}

pub(crate) fn arg_error_text(tool: &str, field: &str, message_key: &'static str) -> String {
    crate::i18n::LocalizedText::with_params(
        message_key,
        crate::i18n::i18n_params!["tool" => tool, "field" => field],
    )
    .render()
}

pub(crate) fn arg_error(
    tool: &str,
    field: &str,
    message_key: &'static str,
) -> ExecutableToolResult {
    err_result(arg_error_text(tool, field, message_key))
}

#[cfg(test)]
mod tests;
