//! Native tool execution inside the Rust engine.
//!
//! Read-only tools (`Read` / `Grep` / `Glob`) execute directly in this
//! process instead of round-tripping to the JS host; mutating tools
//! (`Write` / `Edit` / `Bash`) do too, but only after the host granted
//! permission for the specific call (see
//! [`crate::callbacks::HostCallbacks::check_permission`]). Execution is
//! sandboxed to the workspace root.
//!
//! `None` means "this module cannot serve the call" — a tool it does not
//! implement, or a host-owned interaction (an image read's `region`, a
//! background Bash task). It is *not* an error channel: the native harness
//! has no host tool runtime, so a `None` for a call the engine should have
//! answered reaches the model as `tool "X" is host-owned and not yet wired on
//! the native harness`. A bad path, a mistyped argument, or a file that is
//! not readable text is an error result.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::i18n::{LocalizedText, i18n_params};
use crate::native::shell_path_bridge::ShellPathBridge;

/// P57: mid-execution output stream callback (bash stdout/stderr chunks).
pub type OutputUpdate<'a> = &'a (dyn Fn(&str, &str) + Send + Sync);

use crate::turn_loop::types::{ExecutableToolResult, ToolInfo};

/// Maximum file size a native Read serves (addon TRANSCODE_MAX_BYTES; larger
/// files fall back to the host, which streams them).
const READ_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// v2 `MAX_TASK_OUTPUT_BYTES` (`agent/task/taskService.ts:151`): a background
/// process that produces more than this is terminated instead of being allowed
/// to run the session out of memory and disk. v2 applies it to `kind ===
/// 'process'` only, and so does this — a subagent's output is bounded by its
/// own turn.
///
/// The same v2 number caps the persisted log in
/// [`crate::storage::state_store::MAX_PERSISTED_TASK_OUTPUT_BYTES`]. v2 has one
/// constant serving both because it streams the log; this engine receives one
/// finished `String`, so the two ceilings are enforced in different places.
/// They must stay equal — if you change one, change both.
const MAX_TASK_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

/// v2 `MAX_OUTPUT_BYTES` (`taskService.ts:147`): how much of a stream is kept.
/// v2 drops whole chunks from the front (`appendRetainedOutput`,
/// `taskService.ts:1001-1019`) — the tail is what answers "what happened".
///
/// Bounded **per stream** rather than per task: v2 appends stdout and stderr
/// into one shared buffer, while this collects them separately and
/// concatenates stdout-then-stderr. Merging them would make the recorded
/// output's order depend on pipe scheduling, so the two buffers are capped
/// individually instead. The total is still bounded, at 2× this.
const MAX_RETAINED_OUTPUT_BYTES: usize = 1024 * 1024;

/// v2 `outputLimitReason()` (`taskService.ts:153-160`).
///
/// It lives under the `engine.` namespace because `check-engine-i18n-parity`
/// requires every `LocalizedText` name in this crate to be engine-owned; the
/// catalog already carries the same sentence at the top-level `background.*`
/// key for the TypeScript side, so the text is shared, not reworded.
fn output_limit_reason() -> String {
    LocalizedText::with_params(
        "engine.background.outputLimitExceeded",
        i18n_params!["mib" => MAX_TASK_OUTPUT_BYTES / (1024 * 1024)],
    )
    .render()
}

/// Drain one of a background process's streams, keeping at most
/// [`MAX_RETAINED_OUTPUT_BYTES`] of its tail and counting every byte the task
/// produced.
///
/// The counter is shared between the two streams because v2's
/// `entry.outputSizeBytes` is a per-task total (`taskService.ts:959`), so
/// output split across stdout and stderr counts once against the cap.
///
/// Once the cap trips the reader stops draining. That is deliberate: the
/// caller kills the process next, and a pipe nobody reads fills and wedges the
/// writer — so returning here is what makes the kill land instead of hanging.
async fn read_bounded_stream<R: tokio::io::AsyncRead + Unpin>(
    pipe: &mut R,
    stream: &str,
    emit: &(dyn Fn(&str, &[u8]) + Sync),
    total: &std::sync::atomic::AtomicU64,
    tripped: &std::sync::atomic::AtomicBool,
) -> Vec<u8> {
    use std::sync::atomic::Ordering;
    use tokio::io::AsyncReadExt;

    let mut kept: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        if tripped.load(Ordering::Relaxed) {
            break;
        }
        match pipe.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                emit(stream, &chunk[..n]);
                let seen = total.fetch_add(n as u64, Ordering::Relaxed) + n as u64;
                if seen > MAX_TASK_OUTPUT_BYTES as u64 {
                    tripped.store(true, Ordering::Relaxed);
                    break;
                }
                kept.extend_from_slice(&chunk[..n]);
                // Amortised: trimming on every chunk would memmove a megabyte
                // per 8 KiB read, so let one chunk of overshoot accumulate
                // first. The result is the same tail, just cheaper.
                if kept.len() > MAX_RETAINED_OUTPUT_BYTES + chunk.len() {
                    let excess = kept.len() - MAX_RETAINED_OUTPUT_BYTES;
                    kept.drain(..excess);
                }
            }
        }
    }
    kept
}

/// Grep caps: scanned files, and wall-clock budget (host/addon
/// DEFAULT_TIMEOUT_MS = 20s).
const GREP_MAX_FILES: usize = 5000;
/// Soft memory guard for the parallel walk. Aggregated `content`-mode windows
/// are held in memory until the caller sorts and truncates them, and one
/// matching file can contribute nearly [`GREP_MAX_FILE_BYTES`] of rendered
/// lines, so workers stop scanning (best-effort, like the deadline) once twice
/// the hard cap has been visited. The hard cap is still applied exactly — but
/// only after sorting, so what survives it is deterministic.
const GREP_WALK_SCAN_CAP: usize = GREP_MAX_FILES * 2;
/// Worker threads for one parallel grep walk. `ignore`'s `WalkParallel`
/// defaults to one worker per CPU, which multiplies with tokio's blocking pool
/// (`spawn_blocking` × `MAX_PARALLEL_TOOLS` concurrent native calls): a 16-core
/// box would peak at ~272 threads for grep alone. The walk is syscall-bound
/// rather than CPU-bound, so four workers keep the fan-out benefit while
/// bounding the worst case to 64 extra threads.
const GREP_WALK_THREADS: usize = 4;
const GREP_TIME_BUDGET: Duration = Duration::from_secs(20);
/// Largest file native Grep will pull into memory (matches the Read cap).
/// Bigger ones are skipped and reported as truncation rather than risking a
/// multi-gigabyte allocation per matching file.
const GREP_MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// Cap on rendered Grep output bytes. This is the engine's own memory guard,
/// deliberately tighter than the host's rg stdout buffer
/// (`runRg.ts` `MAX_OUTPUT_BYTES` = 10 MiB): the host streams rg's output and
/// pages it, whereas the engine builds the whole rendered result in process
/// before paging, so context lines across thousands of files would otherwise be
/// materialised in full. Paging itself matches the host
/// ([`GREP_HEAD_LIMIT`] = `DEFAULT_HEAD_LIMIT`).
const GREP_MAX_OUTPUT_BYTES: usize = 512 * 1024;
/// Default result cap for native Grep (host DEFAULT_HEAD_LIMIT).
const GREP_HEAD_LIMIT: usize = 250;
/// VCS metadata directories excluded from every grep walk, regardless of
/// `.gitignore` (mirrors the host `VCS_DIRECTORIES_TO_EXCLUDE`).
const VCS_DIRECTORIES_TO_EXCLUDE: [&str; 6] = [".git", ".svn", ".hg", ".bzr", ".jj", ".sl"];

/// Hard wall-clock cap for native Bash (host may configure less).
const BASH_MAX_SECONDS: u64 = 300;
/// Default timeout for a background Bash task when neither
/// `[background].bash_task_timeout_s` nor the call's own `timeout` sets one.
/// A foreground command that migrates to the background on timeout is
/// re-armed with the same bound.
const BASH_TASK_DEFAULT_TIMEOUT_S: u64 = 600;
/// Ceiling for a background Bash task's timeout (`[background]
/// bash_task_timeout_s` and per-call `timeout` are both clamped to it).
const BASH_TASK_MAX_SECONDS: u64 = 86_400;
/// P57: minimum spacing between `tool.native.progress` events (one event
/// per interval; intervening chunks are dropped from the UI stream only —
/// the final result always carries the full output).
const PROGRESS_MIN_INTERVAL_MS: u64 = 50;

/// Per-stream progress gate. The interval is tracked per stream and the first
/// chunk of a stream always passes: with one shared gate, whichever stream
/// wrote first consumed the whole window, so a banner on stderr arriving just
/// ahead of the command's real stdout swallowed that output from the live
/// stream completely.
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

    /// Whether a chunk on `kind` may be forwarded now, arming the stream's
    /// window when it says yes.
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
pub mod args_validate;
pub mod ask_user_question;
mod bash_exec;
pub mod bash_null_redirect;
pub mod bash_target_dirs;
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
pub mod memory_filing;
pub mod memory_paths;
pub mod memory_store;
pub mod memory_tool;
pub mod mode_mutex;
pub mod moonshot_service;
pub mod plan_mode;
pub mod read_media;
pub mod registry;
pub mod sandbox;
mod search;
pub mod select_tools;
pub mod shell_results;
pub mod skill;
pub mod stale_guard;
pub mod subagent_tools;
pub mod swarm_tool;
pub mod task_format;
pub mod task_tools;
pub mod todo_item;
pub mod todo_list;
pub mod tool_dedupe;
pub mod tool_policy;
pub mod tool_retriever;
pub mod tool_select_state;
mod toolset;
pub mod tower;
pub mod web_search;
pub mod workflow;

mod grep_types;

/// Tools whose native execution requires a host permission grant first.
pub fn is_mutating_tool(tool_name: &str) -> bool {
    matches!(
        tool_name.to_ascii_lowercase().as_str(),
        "write" | "edit" | "bash"
    )
}

/// Every lowercase tool name [`NativeToolset::handles`] accepts, in one
/// place. The set is pinned by `tool-name-contract.json` (see the
/// `native_tool_names_match_the_contract_file` test): adding or removing a
/// spelling must update that file in the same change, and the v2-side test
/// (`agent-core-v2/test/agent/toolRegistry/toolNameContract.test.ts`) fails
/// when a v2 tool name loses its classification. GitHub tool names are a
/// dynamic family (`github::is_github_tool`), not part of this list.
pub const NATIVE_TOOL_NAMES: &[&str] = &[
    "read",
    "grep",
    "glob",
    "write",
    "edit",
    "bash",
    "fetchurl",
    "fetch_url",
    "websearch",
    "web_search",
    // Lsp has an execution arm (`Self::execute_tool`) and a tool definition in
    // `all_native_tool_defs()`, which `GET /api/v1/tools` reports as an active
    // builtin. Leaving it out of this list made `handles()` false, so a host
    // that advertised Lsp had every call forwarded to a host with no tool
    // runtime — the call could only fail.
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
    "enterplanmode",
    "enter_plan_mode",
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
    "creategoal",
    "create_goal",
    "skill",
    "knowledge",
    // Memory tools: the def names are the snake spellings the memory section
    // documents (`memory_read`), so the squashed twins are the aliases here.
    "memoryread",
    "memory_read",
    "memorywrite",
    "memory_write",
    "memorystrreplace",
    "memory_str_replace",
    "memoryappend",
    "memory_append",
    "memorylist",
    "memory_list",
    "memorydelete",
    "memory_delete",
    "workflow",
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
    // Progressive tool disclosure: native in this fork (v2 kept it on the
    // host); without the entry the dedup guard would let a repeated call
    // re-execute instead of replaying the cached announcement.
    "select_tools",
];

/// The static half of [`NativeToolset::handles`] without an instance: the
/// contracted native name list plus the dynamic GitHub family. The dedup
/// guard in `run_turn` uses it to scope engine-side dedup to calls that can
/// execute natively — host-forwarded calls stay under the host's own
/// `toolDedupeService`, so a repeated call is never deduped twice.
pub fn is_native_tool_name(tool_name: &str) -> bool {
    is_builtin_tool_name(tool_name) || tool_name.starts_with("mcp__")
}

/// The engine's own executable builtins, excluding the MCP names
/// [`is_native_tool_name`] also accepts.
///
/// Dispatch serves both, which is why `is_native_tool_name` lumps them
/// together, but "registered inline" is a different question: an `mcp__*` name
/// belongs to a server and is deferred or missing depending on that server's
/// state, so the disclosure decision must not treat it as a builtin.
pub fn is_builtin_tool_name(tool_name: &str) -> bool {
    let lowered = tool_name.to_ascii_lowercase();
    NATIVE_TOOL_NAMES.contains(&lowered.as_str()) || github::is_github_tool(tool_name)
}

/// The engine's own spelling for a tool name the model echoed back.
///
/// A wire may advertise a name the engine does not use internally: the opencode
/// free tier's gate demands the lowercase `bash` / `read` in the tools list
/// (`llm::opencode_adapter`), so a model on that line answers with `bash` where
/// every other wire says `Bash`. Dispatch, permission matching and the tool
/// policy are all case-insensitive, but the consumers that *name* a tool for a
/// human are not — the `tool.call.*` events, ACP's tool-kind map
/// (`acp::events_map::infer_tool_kind`), the TUI's result renderers and chips,
/// and a user's `[[hooks]]` matcher. Restoring the table's spelling before
/// anything acts on the call is what keeps them aligned.
///
/// `table` is the tool table the model was shown. A name the table does not
/// carry — a hallucinated or not-yet-loaded tool — is returned unchanged.
pub fn canonical_tool_name<'a>(name: &'a str, table: &'a [ToolInfo]) -> &'a str {
    table
        .iter()
        .find(|tool| tool.name.eq_ignore_ascii_case(name))
        .map_or(name, |tool| tool.name.as_str())
}

tokio::task_local! {
    /// The id of the agent executing the current turn — "main" for the root
    /// agent, a spawned subagent's id for its own turns. Scoped by the
    /// subagent turn runner ([`crate::subagent::SubagentManager`]). Each
    /// subagent turn runs in its own task, so concurrent workers carry
    /// independent values with no shared-mutable race. When unset (a direct
    /// toolset call, e.g. in a test), [`NativeToolset::effective_caller_agent_id`]
    /// falls back to the construction-time `caller_agent_id`.
    ///
    /// Scope: this task-local is a *tool-execution-time* convenience for code
    /// that runs on the turn runner's own task — the tower tools'
    /// main-agent-only gate ([`NativeToolset::effective_caller_agent_id`]),
    /// the caller's foreground history (`agent_tool.rs`,
    /// `swarm_tool.rs`), and the non-main permission rejection text
    /// (`callbacks.rs`). It deliberately does **not** carry turn or tool event
    /// ownership: a turn's tool execution is `tokio::spawn`ed by the scheduler,
    /// and a tokio task-local is not inherited by a spawned task, so reading it
    /// there yields the root agent. Events take their owner from the explicit
    /// `RunTurnInput.agent_id` → `ToolExecuteRequest.agent_id` chain instead
    /// (`turn_loop/types.rs`).
    pub static CALLER_AGENT_ID: String;
    /// Live snapshot of the current conversation history for the running turn,
    /// used by `Agent(fork: true)` and `AgentSwarm(fork: true)` to inherit context.
    pub static CURRENT_CONVERSATION_HISTORY: std::sync::Arc<std::sync::Mutex<Vec<crate::turn_loop::types::LLMMessage>>>;
}

/// The set of directories a native tool call may touch.
///
/// `primary` is the workspace root (canonicalized by [`NativeToolset::new`]).
/// `extra` holds the host-authorized additional directories — the `/add-dir`
/// list the host hands over as `additionalDirs`. A path that canonicalizes
/// under any of them is served natively instead of being handed back to a
/// host that has no tool runtime to serve it with.
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

    /// Add host-authorized roots. Each is canonicalized here; entries that do
    /// not exist, are not directories, or are already covered by an existing
    /// root are dropped rather than failing the whole sandbox.
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

    /// The extra roots, deduplicated and canonicalized by [`Self::with_extra`].
    pub fn extra(&self) -> &[PathBuf] {
        &self.extra
    }

    /// Every root, primary first.
    pub fn roots(&self) -> impl Iterator<Item = &Path> {
        std::iter::once(self.primary.as_path()).chain(self.extra.iter().map(PathBuf::as_path))
    }

    /// Whether an already-canonicalized path lies inside any root.
    pub fn contains(&self, path: &Path) -> bool {
        path.starts_with(&self.primary) || self.extra.iter().any(|root| path.starts_with(root))
    }
}

/// User home directory, cross-platform (`HOME`, then `USERPROFILE`).
pub(crate) fn user_home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// `std::fs::canonicalize` on Windows returns verbatim (`\\?\C:\…`) paths;
/// strip the prefix so lexical comparisons against plain paths still work.
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

/// No verbatim prefixes off Windows.
#[cfg(not(windows))]
pub(crate) fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    path
}

/// Sandboxed native executor, rooted at the workspace.
pub struct NativeToolset {
    root: PathBuf,
    /// Host-authorized extra roots (`additionalDirs`). Already canonicalized;
    /// see [`Sandbox::with_extra`].
    extra_roots: Vec<PathBuf>,
    /// Extra skill scan roots (`extra_skill_dirs`), the same list the system
    /// prompt's `# Skills` section was rendered from. The `Skill` tool
    /// resolves from the engine's scan, so it must scan the same roots or it
    /// cannot load skills the prompt advertised.
    skill_dirs: Vec<PathBuf>,
    /// Skill roots the enabled plugins declare, with the contributing plugin's
    /// id and instructions. Kept out of `skill_dirs` so they keep v2's own
    /// precedence rank and so a loaded skill can be told which plugin it came
    /// from.
    plugin_skill_dirs: Vec<crate::skills::PluginSkillDir>,
    /// `merge_all_available_skills` (`true` = the documented default): whether
    /// each scope group scans every available directory or only its first
    /// existing one. The prompt is rendered with this switch, so the tool must
    /// apply the same policy or it resolves names the prompt never listed.
    merge_all_available_skills: bool,
    /// Host shell for Bash (the host always uses bash, including Git Bash on
    /// Windows). `None` on Windows means "host owns Bash" — native Bash would
    /// otherwise run commands under a different shell than the tool's
    /// documented contract.
    shell: Option<String>,
    subagent_manager: Option<std::sync::Arc<crate::subagent::SubagentManager>>,
    mcp_manager: Option<std::sync::Arc<crate::mcp::McpManager>>,
    /// Foreground `Agent` tool turn context (P46): the timeout the host
    /// resolved (`resolveSubagentTimeoutMs`) and the parent turn's
    /// cancellation signal. Both `None` outside a wired turn (the tool then
    /// runs with the 2h default and no parent abort).
    subagent_timeout_ms: Option<u64>,
    parent_cancel: Option<crate::subagent::types::ParentCancel>,
    /// P55: session-wide slot (shared with the session pump) holding the
    /// current turn's [`ParentCancel`]. Takes precedence over the static
    /// `parent_cancel`; lets a session-built toolset see per-turn signals.
    parent_cancel_slot: Option<Arc<std::sync::Mutex<Option<crate::subagent::types::ParentCancel>>>>,
    /// Session-wide slot holding the current turn's *steer* signal: fired when
    /// a steering message joins the running turn, and refreshed once that turn
    /// has consumed it. Distinct from `parent_cancel_slot` — steering ends a
    /// blocking `WaitFor` early without cancelling the turn or its tasks (v2
    /// `steerSignal`, #3697).
    steer_slot: Option<Arc<std::sync::Mutex<Option<crate::subagent::types::ParentCancel>>>>,
    /// Host callbacks for interactive tools (AskUserQuestion). `None` means
    /// the tool falls back to the host path, which owns the interaction
    /// runtime anyway.
    callbacks: Option<std::sync::Arc<dyn crate::callbacks::HostCallbacks>>,
    /// Host-resolved `[github]` config credentials for the native GitHub
    /// tools (v2 `configSection.ts`). Env fallbacks live in the github
    /// module itself (v2 `envOverlay.ts` semantics).
    github_credentials: Option<github::GitHubCredentials>,
    caller_agent_id: Option<String>,
    /// The session's live permission mode; see [`Self::with_permission_mode`].
    permission_mode: Option<crate::permission::PermissionMode>,
    session_id: Option<String>,
    task_runner: Option<std::sync::Arc<crate::storage::TaskRunner>>,
    /// Shared rich-wake driver and per-session buffers. Interior mutability
    /// lets the session composition root inject it after the pipeline has
    /// already put this toolset behind an `Arc`.
    tower_wake_runtime: Arc<std::sync::Mutex<Option<Arc<tower::wake::TowerWakeRuntime>>>>,
    /// Native file-history capture (v2 `fileHistoryService`): when set,
    /// mutating file tools (`write` / `edit`) record a row into
    /// `session_file_history` for each successful change. Shared so the host
    /// can install it per turn through
    /// [`crate::callbacks::HostCallbacks::set_file_history`] after the toolset
    /// has been moved into the callbacks wrapper.
    file_history: Arc<std::sync::Mutex<Option<FileHistoryCtx>>>,
    /// Turn number attributed to recorded file-history rows. Carried on the
    /// toolset (not a thread-local) because mutating tools run on tokio's
    /// blocking pool: a thread-local set on the async thread is invisible
    /// there, which is why the previous `scope_turn_id` helper was dead.
    /// Shared + atomic so it can be refreshed per turn after construction.
    turn_id: Arc<std::sync::atomic::AtomicUsize>,
    /// Names the model has loaded through `select_tools`. Shared (`Arc`) so the
    /// toolset can hand a `&mut` view to the loader while `execute` keeps its
    /// `&self` signature; persists for the pipeline's lifetime, so
    /// `already_available` is meaningful across turns.
    loaded_tools: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// The loadable names already announced via `<tools_added>` (v2
    /// `foldAnnouncedToolNames`): the turn-start announcement diffs the
    /// current deferred set against this, so a mid-session MCP change
    /// announces only the delta instead of the full list every turn.
    announced_tools: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// v2 #4719: the session's tool-select revision ledger (initial table +
    /// ordered changes). `take_disclosure_announcement` reconciles the live
    /// registry into it and walks changes since the last announced revision.
    tool_select_state: Arc<std::sync::Mutex<crate::tools::tool_select_state::ToolSelectState>>,
    /// The last announcement revision delivered to the model.
    announcement_revision: Arc<std::sync::atomic::AtomicUsize>,
    /// Upstream #4190: whether the empty loadable state ("No tools are
    /// currently available for dynamic loading…") has already been announced.
    /// Set when it goes out, cleared when a non-empty announcement follows,
    /// so a session hears it once per transition into the empty state.
    empty_disclosure_announced: Arc<std::sync::atomic::AtomicBool>,
    /// Upstream #4190 `failedSelectionAttempts` + `consecutiveFailures`:
    /// per-name failed-selection counts and the consecutive-failure streak
    /// for `select_tools`, session-scoped like [`Self::loaded_tools`].
    selection_failures: Arc<std::sync::Mutex<crate::tools::select_tools::SelectionFailureTracker>>,
    /// Upstream #4061 `TurnWaitTally`: the per-turn WaitFor repeat tally
    /// behind the `[wait_warning]` block, session-scoped like
    /// [`Self::selection_failures`].
    wait_tally: Arc<std::sync::Mutex<crate::tools::task_tools::WaitTally>>,
    /// The user's global `[tools]` enable/disable lists (v2 tool policy):
    /// applied to the advertised table and enforced again before execution.
    tools_filter: Option<tool_policy::ToolsFilter>,
    /// `[secondary_model]`: the subagent model pool the `Agent` / `AgentSwarm`
    /// tools advertise and bind. `None` = subagents inherit the caller's model.
    secondary_model: Option<std::sync::Arc<crate::subagent::secondary::SecondaryModelRuntime>>,
    /// `[image].read_byte_budget` (v2 `resolveReadImageByteBudget`) for
    /// model-initiated image reads; `None` keeps the 256KB default.
    image_read_byte_budget: Option<u64>,
    /// `[image].max_edge_px` for model-initiated image reads; `None` keeps
    /// the 2000px default.
    image_max_edge_px: Option<u32>,
    /// The session model's declared capabilities. `None`/empty means unknown,
    /// and an image read is allowed (v2 `isUnknownCapability`); a declared
    /// set without `image_in` refuses it.
    model_capabilities: Option<Vec<String>>,
    /// Whether progressive tool disclosure is active (v2 `toolSelectService`
    /// `enabled()`: the `tool_select` experimental flag). When on, the
    /// advertised table drops this toolset's deferred MCP tools and
    /// advertises `select_tools` instead; a call to a deferred-but-unloaded
    /// tool is refused with load guidance.
    tool_select_enabled: bool,
    /// The session's initial tool-name snapshot (v2 #4720 / #4719): when
    /// disclosure is off, later requests reconcile the live registry into
    /// this set instead of rebuilding it, so same-name updates use the
    /// current definition and runtime-only additions stay invisible.
    initial_tool_names: std::sync::Arc<std::sync::Mutex<Option<std::collections::HashSet<String>>>>,
    /// `[background].bash_auto_background_on_timeout`: migrate a timed-out
    /// foreground Bash call to the background instead of killing it.
    /// Defaults to `true`.
    bash_auto_background: bool,
    /// `[background].bash_task_timeout_s`: default timeout for background
    /// Bash tasks (also re-arms a foreground command migrated to the
    /// background on timeout). `None` keeps the built-in 600s; `Some(0)`
    /// means "no timeout".
    bash_task_timeout_s: Option<u64>,
    /// Resolved `[task].max_running_tasks` (with `[background]` fallback)
    /// used only as the Tower spawn budget's configured ceiling.
    tower_max_running_tasks: Option<u32>,
    /// Bound provider type (e.g. "kimi").
    provider: Option<String>,
    /// Translates the POSIX path dialect the shell speaks into native win32
    /// paths (v2 `shellPathBridge`). Built from the resolved shell, because
    /// `cygpath.exe` lives beside it.
    shell_bridge: std::sync::Arc<crate::native::shell_path_bridge::ShellPathBridge>,
}

/// Bundle the native file-history recorder needs: the store handle plus
/// the session to attribute the change to. `turn_id` is read from the
/// `TURN_ID` task-local at record time, which the turn runner scopes per
/// turn (REPL callers that don't scope it record under turn 0).
#[derive(Clone)]
struct FileHistoryCtx {
    store: std::sync::Arc<crate::session::sqlite_store::SqliteSessionStore>,
    session_id: String,
}

thread_local! {
    /// Per-call file-history context installed by
    /// `run_mutating_file_tool_on_blocking_pool` so the static `write` /
    /// `edit` fns can reach the recorder without changing their signature.
    static FILE_HISTORY: std::cell::RefCell<Option<FileHistoryCtx>> =
        const { std::cell::RefCell::new(None) };

    /// Turn number the current mutating-file call belongs to. Installed in the
    /// same blocking closure as `FILE_HISTORY` (see
    /// [`NativeToolset::spawn_mutating_file_tool`]); the mutating file tools
    /// read it when recording the file-history row.
    static TURN_ID: std::cell::RefCell<usize> =
        const { std::cell::RefCell::new(0) };
}

// ── Grep parallel-scan helpers ───────────────────────────────────────────

/// Boolean tool argument. Absent and explicit `null` both mean "the schema
/// default"; a present-but-mistyped value is an error the engine reports
/// itself. The host fallback this used to rely on does not exist on the
/// native harness, where the call came back as `tool "Grep" is host-owned and
/// not yet wired on the native harness` instead of naming the argument.
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

/// Non-negative integer tool argument, with the same absent/null versus
/// mistyped distinction as [`bool_arg`] (the host schema is
/// `z.number().int().nonnegative()`).
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

/// Integer tool argument read leniently: besides a JSON integer, accept a
/// numeric string (`"30"`) and a float with no fractional part (`30.0`).
/// Models emit both against schemas that declare `"type": "integer"`, and
/// `serde_json`'s `as_i64` rejects them — which surfaced as
/// `"line_offset" must be an integer` on a value that prints as `30`. A
/// fractional or unparseable value is still rejected.
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

/// Map a blocking-pool `JoinError` onto the tool's mutability: read-only tools
/// (`read` / `grep` / `glob`) are idempotent, so `None` sends the call back to
/// the host instead of failing it; mutating tools (`write` / `edit`) hold a
/// permission grant and may have partially applied, so the failure is reported
/// rather than risking a double write through the host path.
fn blocking_pool_failure(mutating: bool, message: String) -> Option<ExecutableToolResult> {
    if mutating {
        Some(err_result(format!("native tool task failed: {message}")))
    } else {
        None
    }
}

fn ok_result(content: String) -> ExecutableToolResult {
    ExecutableToolResult {
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
        delivery: None,
        stop_turn: false,
        content,
        is_error: true,
        note: None,
        display: None,
    }
}

/// The rendered text of an `Invalid <tool> arguments: …` message.
///
/// The validation message is the same sentence at every call site apart from
/// the tool and the field name, so it is one key per *shape*
/// (`argMustBeString`, `argMustNotBeEmpty`, …) with both interpolated rather
/// than one key per (tool, field) pair: 20 call sites route through it, against
/// the 23 generic-shape literals the tree carried before the change. Passing the
/// key in also lets a tool that needs a tool-specific sentence opt out by naming
/// its own key instead.
///
/// The key is a plain argument rather than derived from `tool`, because a key
/// is a fact about the sentence's shape and must be greppable at the call site
/// — `check:engine-i18n` resolves the catalog against every `LocalizedText`
/// name, and a computed name would escape it.
pub(crate) fn arg_error_text(tool: &str, field: &str, message_key: &'static str) -> String {
    crate::i18n::LocalizedText::with_params(
        message_key,
        crate::i18n::i18n_params!["tool" => tool, "field" => field],
    )
    .render()
}

/// [`arg_error_text`] as a failed tool result, for the common case where the
/// validation failure *is* the tool's answer.
pub(crate) fn arg_error(
    tool: &str,
    field: &str,
    message_key: &'static str,
) -> ExecutableToolResult {
    err_result(arg_error_text(tool, field, message_key))
}

#[cfg(test)]
mod tests;
