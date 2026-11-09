//! Subagent lifecycle and concurrency manager.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::RwLock;

use serde::{Deserialize, Serialize};

use crate::environment::{
    AgentEnvironment, AgentEnvironmentRequest, EnvironmentSnapshot, WorkspaceChange,
    WorkspaceContext, WorkspaceRoots,
};
use crate::i18n::{LocalizedText, i18n_params};
use crate::session::sqlite_store::SqliteSessionStore;
use crate::subagent::types::*;

/// A session's frozen environment and the roots it was resolved against.
///
/// Pushed by the entry that owns the environment declaration (the server
/// engine). A child resolves against both: the snapshot supplies the inherited
/// id, cwd and variables, the roots supply the boundary it may not leave.
#[derive(Debug, Clone)]
struct SessionEnvironment {
    snapshot: EnvironmentSnapshot,
    roots: WorkspaceRoots,
}

type InstanceEntry = (SubagentInstance, Arc<AtomicBool>);
type InstanceMap = Arc<RwLock<HashMap<String, InstanceEntry>>>;
type DefinitionMap = Arc<RwLock<HashMap<String, SubagentDefinition>>>;
type PersistentMap = Arc<RwLock<HashMap<String, PersistentInstance>>>;
type ForegroundRuns = Arc<Mutex<HashSet<String>>>;
type ActiveTurnSessions = Arc<Mutex<HashMap<String, String>>>;

/// Single-flight claim for one foreground turn per instance id
/// (v2 `createAwaitingClose`): two concurrent `resume` calls for the same
/// agent must not both enter the same conversation — the second is refused
/// with `already_running`, not raced.
#[derive(Debug)]
struct ForegroundTurnClaim {
    runs: ForegroundRuns,
    /// Agent id → session id of the turn it is running, so the session
    /// activity fold can count a background subagent turn (v2
    /// `sessionActivityService`'s agent folds).
    sessions: ActiveTurnSessions,
    id: String,
}

impl Drop for ForegroundTurnClaim {
    fn drop(&mut self) {
        let mut runs = self.runs.lock().unwrap_or_else(|error| error.into_inner());
        runs.remove(&self.id);
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        sessions.remove(&self.id);
    }
}

/// How many completed subagent scopes stay resident for a fast resume
/// (v2 `SUBAGENT_SCOPE_CACHE_SIZE_ENV`).
pub const SUBAGENT_SCOPE_CACHE_SIZE_ENV: &str = "KIMI_CODE_SUBAGENT_SCOPE_CACHE_SIZE";

/// Wall-clock bound on a single scope eviction
/// (v2 `SUBAGENT_SCOPE_EVICT_TIMEOUT_ENV`).
pub const SUBAGENT_SCOPE_EVICT_TIMEOUT_ENV: &str = "KIMI_CODE_SUBAGENT_SCOPE_EVICT_TIMEOUT_MS";

pub const DEFAULT_SUBAGENT_SCOPE_CACHE_SIZE: usize = 32;

pub const DEFAULT_SUBAGENT_SCOPE_EVICT_TIMEOUT_MS: u64 = 15_000;

/// How often one scope may refuse eviction before it is left resident
/// (v2 `MAX_EVICT_ATTEMPTS`).
const MAX_SCOPE_EVICT_ATTEMPTS: u32 = 3;

/// The retry cadence of `createAgentAwaitingClose` (v2
/// `session/agentLifecycle/createAwaitingClose.ts`).
const SCOPE_CLOSE_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// How long `kill` waits for the running turn to observe the cancellation
/// before shutting the rest of the scope down (v2
/// `REMOVE_PROMPT_QUIESCE_TIMEOUT_MS`).
const REMOVE_PROMPT_QUIESCE_TIMEOUT: Duration = Duration::from_secs(3);

/// The quiescence poll cadence (v2 `REMOVE_PROMPT_QUIESCE_POLL_MS`).
const REMOVE_PROMPT_QUIESCE_POLL: Duration = Duration::from_millis(10);

/// Parses `KIMI_CODE_SUBAGENT_SCOPE_CACHE_SIZE` (v2
/// `resolveSubagentScopeCacheSize`). Missing or blank values yield the
/// default; a negative size means "never evict"; a non-integer errors.
pub fn resolve_subagent_scope_cache_size(env: &HashMap<String, String>) -> Result<usize, String> {
    let Some(raw) = env.get(SUBAGENT_SCOPE_CACHE_SIZE_ENV) else {
        return Ok(DEFAULT_SUBAGENT_SCOPE_CACHE_SIZE);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(DEFAULT_SUBAGENT_SCOPE_CACHE_SIZE);
    }
    match trimmed.parse::<i64>() {
        Ok(value) => Ok(value.max(0) as usize),
        Err(_) => Err(format!(
            "{SUBAGENT_SCOPE_CACHE_SIZE_ENV} must be an integer, got {raw:?}."
        )),
    }
}

/// Parses `KIMI_CODE_SUBAGENT_SCOPE_EVICT_TIMEOUT_MS` (v2
/// `resolveSubagentScopeEvictTimeoutMs`). Missing or blank values yield the
/// default; anything but a positive integer errors.
pub fn resolve_subagent_scope_evict_timeout_ms(
    env: &HashMap<String, String>,
) -> Result<u64, String> {
    let Some(raw) = env.get(SUBAGENT_SCOPE_EVICT_TIMEOUT_ENV) else {
        return Ok(DEFAULT_SUBAGENT_SCOPE_EVICT_TIMEOUT_MS);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(DEFAULT_SUBAGENT_SCOPE_EVICT_TIMEOUT_MS);
    }
    match trimmed.parse::<u64>() {
        Ok(value) if value > 0 => Ok(value),
        _ => Err(format!(
            "{SUBAGENT_SCOPE_EVICT_TIMEOUT_ENV} must be a positive integer, got {raw:?}."
        )),
    }
}

/// Why a scope eviction did or did not happen (v2 `EvictOutcome`).
enum EvictOutcome {
    Removed,
    Missing,
    /// The scope is running again; it stays retired and is retried later.
    Deferred,
    /// The eviction did not finish inside the configured bound.
    Timeout,
}

/// Completed-scope LRU (v2 `subagentScopeCacheService`): once more than
/// `capacity` completed scopes are resident, the oldest completion is
/// evicted. Only the resident instance and its in-memory conversation are
/// dropped — the persisted resume record is left alone, so the next `resume`
/// rebuilds both from it.
struct ScopeCache {
    capacity: usize,
    evict_timeout: Duration,
    /// Completed ids in least-recently-completed order, with the eviction
    /// attempts already spent on each (v2 `retired`).
    retired: Vec<(String, u32)>,
    /// Set when the two env vars failed validation; the first spawn reports
    /// it (v2 throws when the session scope is created).
    error: Option<String>,
}

impl ScopeCache {
    fn from_env() -> Self {
        let env: HashMap<String, String> = std::env::vars().collect();
        let resolved = resolve_subagent_scope_cache_size(&env).and_then(|capacity| {
            resolve_subagent_scope_evict_timeout_ms(&env).map(|timeout_ms| (capacity, timeout_ms))
        });
        match resolved {
            Ok((capacity, timeout_ms)) => Self {
                capacity,
                evict_timeout: Duration::from_millis(timeout_ms),
                retired: Vec::new(),
                error: None,
            },
            Err(error) => Self {
                capacity: DEFAULT_SUBAGENT_SCOPE_CACHE_SIZE,
                evict_timeout: Duration::from_millis(DEFAULT_SUBAGENT_SCOPE_EVICT_TIMEOUT_MS),
                retired: Vec::new(),
                error: Some(error),
            },
        }
    }

    /// Move `id` to the most-recently-completed end (v2 `retire`).
    fn retire(&mut self, id: &str) {
        self.retired.retain(|(entry, _)| entry != id);
        self.retired.push((id.to_string(), 0));
    }

    /// Drop `id` from the LRU (v2 `revive`): a scope that is live again must
    /// not be evicted.
    fn revive(&mut self, id: &str) {
        self.retired.retain(|(entry, _)| entry != id);
    }

    /// The oldest entry still worth trying, skipping the ones this pass
    /// already failed on and the ones that exhausted their attempts
    /// (v2 `oldestCandidate`).
    fn oldest_candidate(&self, skipped: &HashSet<String>) -> Option<(String, u32)> {
        self.retired
            .iter()
            .find(|(id, attempts)| *attempts < MAX_SCOPE_EVICT_ATTEMPTS && !skipped.contains(id))
            .cloned()
    }

    fn remove(&mut self, id: &str) {
        self.retired.retain(|(entry, _)| entry != id);
    }

    fn reinsert(&mut self, id: &str, attempts: u32) {
        if !self.retired.iter().any(|(entry, _)| entry == id) {
            self.retired.push((id.to_string(), attempts));
        }
    }
}

/// The last non-empty assistant text of a turn — the subagent's summary
/// the `Agent` tool reports to the caller (v2 `r.summary`).
pub(crate) fn final_assistant_summary(messages: &[crate::turn_loop::types::LLMMessage]) -> String {
    messages
        .iter()
        .rev()
        .find(|message| message.role == "assistant" && !message.content.is_empty())
        .map(|message| message.content.clone())
        .unwrap_or_default()
}

/// The partial-output section of a stopped subagent's report: the last
/// assistant text an aborted turn produced, rendered as a `[partial output]`
/// block (leading newlines included, ready to append). A stop used to throw
/// that work away — the caller got the bare stopped message and the turn's
/// messages were only kept for resume. `None` when the turn died before any
/// assistant text, so the report stays the bare v2 message then.
pub(crate) fn partial_output_section(turn: &crate::turn_loop::types::TurnResult) -> Option<String> {
    let partial = final_assistant_summary(&turn.messages);
    let partial = partial.trim();
    (!partial.is_empty()).then(|| format!("\n\n[partial output]\n{partial}"))
}

/// Shared prefix of the v2 `AGENT_NO_FINAL_MESSAGE` failure text, so the
/// report layer can name the reason without re-parsing prose.
pub const NO_FINAL_MESSAGE_PREFIX: &str = "Subagent turn ended without a final message";

/// v2 `runAgentTurn` `awaitRun`: a turn that ends without any assistant text
/// has no handoff to report, so it fails instead of completing with an empty
/// summary (v2 `AGENT_NO_FINAL_MESSAGE`). The loop only ever forces one stop
/// reason — the repeat breaker — and v2 renders it in the message.
fn no_final_message_error(turn: &crate::turn_loop::types::TurnResult) -> Option<String> {
    if !final_assistant_summary(&turn.messages).trim().is_empty() {
        return None;
    }
    Some(match turn.stop_reason {
        crate::turn_loop::types::LoopTurnStopReason::RepeatBreaker => {
            format!("{NO_FINAL_MESSAGE_PREFIX} (stop reason: repeat_breaker).")
        }
        _ => format!("{NO_FINAL_MESSAGE_PREFIX}."),
    })
}

/// A profile's tool policy (v2 `resolveActiveToolNames` subset semantics):
/// an explicit allowlist wins; an empty allowlist means every tool except
/// the disallowed names.
#[derive(Clone)]
pub struct ToolPolicyFilter {
    allowlist: Vec<String>,
    disallowed: Vec<String>,
}

impl ToolPolicyFilter {
    pub fn from_definition(def: &SubagentDefinition) -> Self {
        Self {
            allowlist: def.tools.clone(),
            disallowed: def.disallowed_tools.clone(),
        }
    }

    /// The allowlist-only form (v2 `resolveActiveToolNames`): evaluates a
    /// profile's declared names against that same allowlist, for rendering
    /// what the profile actually admits.
    pub fn from_allowlist(tools: &[String]) -> Self {
        Self {
            allowlist: tools.to_vec(),
            disallowed: Vec::new(),
        }
    }

    /// Whether the profile admits `tool_name` (v2 `isToolActive`): an
    /// explicit allowlist wins, otherwise every tool except the disallowed
    /// names. Tool names are matched with the same source-dependent rule as
    /// the global `[tools]` switch — MCP patterns are globs
    /// (`mcp__*` / `mcp__github__*`), built-ins match exactly and
    /// case-insensitively (the engine dispatches on the lowercase wire name
    /// while profile tables spell them `Read`).
    pub fn allows(&self, tool_name: &str) -> bool {
        let matches = |pattern: &str| {
            crate::tools::tool_policy::matches_profile_tool_pattern(pattern, tool_name)
        };
        if !self.allowlist.is_empty() {
            return self.allowlist.iter().any(|pattern| matches(pattern));
        }
        !self.disallowed.iter().any(|pattern| matches(pattern))
    }

    /// Whether a tool *call* from this profile must be refused.
    ///
    /// Mirrors the global switch's
    /// [`crate::tools::tool_policy::ToolsFilter::blocks_call`] and its one
    /// deliberate divergence: enforcement is case-insensitive for built-ins,
    /// because the engine dispatches on the lowercase wire name while a
    /// profile table spells them `Read`. So a profile that excludes `Bash`
    /// refuses it even when the subagent's model calls it by the wire name
    /// from memory — the advertised table is a hint to the model, not a
    /// boundary around it.
    pub fn blocks_call(&self, tool_name: &str) -> bool {
        let matches = |pattern: &str| {
            crate::tools::tool_policy::matches_profile_tool_pattern(pattern, tool_name)
        };
        if !self.allowlist.is_empty() {
            return !self.allowlist.iter().any(|pattern| matches(pattern));
        }
        self.disallowed.iter().any(|pattern| matches(pattern))
    }
}

/// The host's tool table narrowed by a subagent profile's policy.
///
/// A failed enumeration is an error, not an empty catalogue. Degrading to
/// `Ok(vec![])` handed the model a subagent with no tools at all and then
/// reported the turn as `Completed` with whatever the model improvised — a
/// plausible-looking answer to a task it was never equipped to do. The caller
/// turns this into a failed subagent turn, which is at least truthful.
async fn subagent_tool_defs(
    callbacks: &Arc<dyn crate::callbacks::HostCallbacks>,
    filter: &ToolPolicyFilter,
) -> Result<Vec<crate::turn_loop::types::ToolInfo>, String> {
    let response = callbacks
        .list_tools()
        .await
        .map_err(|err| format!("Could not read the tool table for this subagent: {err}"))?;
    Ok(response
        .tools
        .into_iter()
        .filter(|tool| filter.allows(&tool.name))
        .collect())
}

/// [`crate::callbacks::HostCallbacks`] decorator that narrows the tool
/// table a subagent's turn sees to its profile policy (`list_tools`
/// filter); every other seam passes through untouched.
///
/// Agent identity needs no decorator: a subagent's turn runs inside a
/// [`CALLER_AGENT_ID`] scope (`subagent/manager.rs`), and every event the turn
/// produces reads it when it is constructed — the same shape v2 gets from
/// `IAgentScopeContext` (`loopService.ts:1426-1455`).
struct ToolFilterCallbacks {
    inner: Arc<dyn crate::callbacks::HostCallbacks>,
    filter: ToolPolicyFilter,
    /// The subagent instance this turn belongs to. The native LLM's stream
    /// sink emits bare `llm.delta` payloads with no agent attribution, so the
    /// wrapper stamps every event that crosses it — without this the SDK's
    /// attribution fallback resolves the deltas to `"main"` and a subagent's
    /// body text streams into the main transcript.
    agent_id: String,
}

impl crate::callbacks::HostCallbacks for ToolFilterCallbacks {
    fn hook_guard(&self) -> Option<Arc<crate::tools::external_hooks::HookGuard>> {
        // The subagent turn reads its lifecycle hooks from the callbacks it
        // runs with; without this delegation the filter wrapper hid the
        // pipeline's guard and subagent turns ran unguarded.
        self.inner.hook_guard()
    }

    fn llm_chat(
        &self,
        request: crate::rpc::types::LlmChatRequest,
    ) -> crate::rpc::types::BoxFuture<'static, Result<crate::rpc::types::LlmChatResponse, String>>
    {
        self.inner.llm_chat(request)
    }

    fn execute_tool(
        &self,
        request: crate::rpc::types::ToolExecuteRequest,
    ) -> crate::rpc::types::BoxFuture<'static, Result<crate::rpc::types::ToolExecuteResponse, String>>
    {
        // The profile's advertised table is not a boundary. A subagent's model
        // can name a tool the profile excludes — from its prompt, from a
        // remembered name, from a guess — and the shared `NativeToolset` only
        // re-checks the *global* `[tools]` switch before a native call runs
        // (`tools/mod.rs:1333-1341`). Without this second check a read-only
        // profile that excludes `Bash` is only as safe as the model's
        // cooperation, which is the one thing a profile exists not to depend
        // on.
        let blocked_name = request.tool_name.clone();
        if self.filter.blocks_call(&blocked_name) {
            return Box::pin(async move {
                Ok(crate::rpc::types::ToolExecuteResponse {
                    content: format!("Tool \"{blocked_name}\" is not available to this agent."),
                    is_error: true,
                    note: None,
                    stop_turn: false,
                    delivery: None,
                })
            });
        }
        self.inner.execute_tool(request)
    }

    fn check_permission(
        &self,
        request: crate::rpc::types::PermissionCheckRequest,
    ) -> crate::rpc::types::BoxFuture<'static, Result<crate::rpc::types::PermissionDecision, String>>
    {
        self.inner.check_permission(request)
    }

    fn ask_question(
        &self,
        request: crate::rpc::types::AskQuestionRequest,
    ) -> crate::rpc::types::BoxFuture<'static, Result<crate::rpc::types::AskQuestionResponse, String>>
    {
        self.inner.ask_question(request)
    }

    fn state_read(
        &self,
        request: crate::rpc::types::StateReadRequest,
    ) -> crate::rpc::types::BoxFuture<'static, Result<crate::rpc::types::StateReadResponse, String>>
    {
        self.inner.state_read(request)
    }

    fn state_write(
        &self,
        request: crate::rpc::types::StateWriteRequest,
    ) -> crate::rpc::types::BoxFuture<'static, Result<crate::rpc::types::StateWriteResponse, String>>
    {
        self.inner.state_write(request)
    }

    fn list_tools(
        &self,
    ) -> crate::rpc::types::BoxFuture<'static, Result<crate::rpc::types::ListToolsResponse, String>>
    {
        let inner = self.inner.clone();
        let filter = self.filter.clone();
        Box::pin(async move {
            let mut response = inner.list_tools().await?;
            // v2 registers the unified `Memory` tool for the main agent only
            // (`memoryTool.ts:516-519`); a subagent's advertised table must not
            // contain it even though the shared session table does.
            response.tools.retain(|tool| {
                filter.allows(&tool.name)
                    && !tool
                        .name
                        .eq_ignore_ascii_case(crate::tools::core_tool_defs::MEMORY_TOOL_NAME)
            });
            Ok(response)
        })
    }

    fn goal(
        &self,
    ) -> crate::rpc::types::BoxFuture<
        'static,
        Result<Option<crate::turn_loop::types::GoalContext>, String>,
    > {
        self.inner.goal()
    }

    fn drain_steers(
        &self,
    ) -> crate::rpc::types::BoxFuture<
        'static,
        Result<Vec<crate::turn_loop::types::LLMMessage>, String>,
    > {
        self.inner.drain_steers()
    }

    fn set_turn_goal(&self, turn_id: &str, goal_id: Option<&str>) {
        self.inner.set_turn_goal(turn_id, goal_id);
    }

    fn emit_event(&self, event: serde_json::Value) {
        // The native LLM's stream sink emits bare `{"type":"llm.delta",
        // "part":…}` payloads with no agent attribution, and the SDK's
        // attribution fallback resolves an unattributed event to `"main"` —
        // which streamed this subagent's body text into the main transcript.
        // Stamp the instance id onto every event crossing this wrapper; the
        // SDK prefers an explicit `agent_id` over its fallback.
        let stamped = match &event {
            serde_json::Value::Object(map) if !map.contains_key("agent_id") => {
                let mut stamped = map.clone();
                stamped.insert("agent_id".into(), self.agent_id.clone().into());
                serde_json::Value::Object(stamped)
            }
            _ => event,
        };
        self.inner.emit_event(stamped);
    }

    fn turn_event(&self, event: crate::turn_events::TurnEvent) {
        self.inner.turn_event(event);
    }

    fn telemetry(&self, event: serde_json::Value) {
        self.inner.telemetry(event);
    }

    fn cancel_llm_chat(&self, request_id: &str) {
        self.inner.cancel_llm_chat(request_id);
    }
}

/// State of a persistent subagent instance: message history, cumulative
/// usage, and the running guard that rejects concurrent turns.
pub struct PersistentInstance {
    /// Spawn-time execution context, recorded for the instance's lifetime.
    pub llm: Arc<dyn crate::turn_loop::types::LLM>,
    pub callbacks: Arc<dyn crate::callbacks::HostCallbacks>,
    /// Definition system prompt, inlined into the first turn's prompt.
    pub system_prompt: String,
    /// Conversation history accumulated across turns.
    pub messages: Vec<crate::turn_loop::types::LLMMessage>,
    /// Cumulative token usage across all turns.
    pub usage: crate::rpc::types::TokenUsage,
    /// True while a turn is running; concurrent turns are rejected.
    pub running: bool,
    /// Shared with the instance-map entry so `kill` aborts running turns.
    pub cancelled: Arc<AtomicBool>,
}

/// Execution runtime injected by the host so subagents can run real turns.
pub struct SubagentRuntime {
    pub llm: Arc<dyn crate::turn_loop::types::LLM>,
    pub callbacks: Arc<dyn crate::callbacks::HostCallbacks>,
    /// The parent session this runtime was built for, so background
    /// subagent tasks attribute their lifecycle events to the right lane.
    pub session_id: Option<String>,
    /// Resolves the media references a subagent turn carries (v2
    /// `AgentMediaResolverService`).
    pub media: crate::llm::media_resolver::MediaResolver,
    /// The runtime's cross-turn record of omitted media (v2
    /// `media.budgetDropped`).
    pub media_dropped: crate::llm::media_budget::DroppedMedia,
    /// The runtime's media recovery record (v2 #4413): subagent turns are
    /// their own agent scope, so the recovery does not leak into the parent's.
    pub media_recovery: crate::llm::media_budget::SharedMediaRecovery,
}

/// The advertise-hop raw material for the `Agent` tool description (v2's
/// allowlist-filtered profile listing): the caller's effective spawn
/// allowlist plus the turn's discovered definitions.
#[derive(Clone)]
pub struct SpawnCatalog {
    /// `None` = unrestricted (`['*']`).
    pub allowlist: Option<Vec<String>>,
    /// The turn's host-pushed (discovered) definitions, already registered.
    pub discovered: Vec<SubagentDefinition>,
}

pub struct SubagentManager {
    definitions: DefinitionMap,
    instances: InstanceMap,
    persistent: PersistentMap,
    runtime: RwLock<Option<Arc<SubagentRuntime>>>,
    /// Per-instance LLM bindings (`[secondary_model]` pool choices), keyed by
    /// instance id. Absent = inherit the session runtime LLM.
    instance_llms: Arc<RwLock<HashMap<String, Arc<dyn crate::turn_loop::types::LLM>>>>,
    /// P55: accumulated conversation history per foreground agent id, so a
    /// `resume` call continues the same subagent natively (v2 persistent
    /// scopes). Written on foreground completion; read by `resume` calls.
    foreground_histories: Arc<Mutex<HashMap<String, ForegroundResume>>>,
    /// Ids with an in-flight foreground/resume turn. Claimed for the whole
    /// turn so a concurrent resume gets `already running` instead of a second
    /// turn on the same conversation (v2 `createAwaitingClose`).
    foreground_runs: ForegroundRuns,
    /// Agent id → session id for the turns currently in flight, so the server
    /// can fold a background subagent turn into the session's activity
    /// (v2 `sessionActivityService`). Mirrors `foreground_runs`; both entries
    /// are removed when the claim drops.
    active_turn_sessions: ActiveTurnSessions,
    /// The session the installed runtime belongs to, mirrored synchronously so
    /// a foreground claim (a sync call) can label its turn without taking the
    /// runtime lock.
    current_session: std::sync::RwLock<Option<String>>,
    /// Monotonic counter behind `agent-N` auto ids (v2
    /// `agentLifecycleService.nextAgentId`). Bumped past every `agent-N` id
    /// that enters the registry through an explicit spawn or a cold rebuild.
    next_agent_number: std::sync::atomic::AtomicU64,
    /// Optional SQLite store for cold resume across restarts (#3478).
    session_store: RwLock<Option<Arc<SqliteSessionStore>>>,
    /// Optional TaskRunner for background subagent task management and inspection.
    task_runner: RwLock<Option<Arc<crate::storage::TaskRunner>>>,
    /// Host-resolved `[swarm] timeout_ms` (v2 `resolveSwarmTimeoutMs`; env
    /// `KIMI_CODE_SWARM_TIMEOUT_MS` wins host-side). Rides the manager because
    /// the toolset dispatch passes the `Agent` timeout to both tools — the
    /// swarm-specific value is read at `AgentSwarm` execution time.
    /// `None` keeps the previous behavior: the swarm follows the subagent
    /// timeout (or the 2h default).
    swarm_timeout_ms: Mutex<Option<u64>>,
    /// Completed-scope LRU (v2 `subagentScopeCache`), resolved once at
    /// construction.
    scope_cache: Mutex<ScopeCache>,
    /// Cumulative token usage per instance id (v2 `ISessionUsageService`
    /// per-agent status), folded at every turn completion. Session-lifetime
    /// bookkeeping: an evicted scope's entry goes with it (the persisted
    /// resume record carries the conversation, not the counters).
    usage_by_instance: Arc<Mutex<HashMap<String, crate::rpc::types::TokenUsage>>>,
    /// Who spawned each subagent (v2 `subagentLabels`).
    ///
    /// Recorded at spawn time from the spawning agent's scope, and the gate
    /// `AgentSwarm`'s `resume_agent_ids` runs before touching a member's
    /// conversation. Session-lifetime bookkeeping, like `usage_by_instance`:
    /// an evicted scope's entry goes with it, and the persisted resume record
    /// carries the parent across a restart.
    parents: Arc<RwLock<HashMap<String, SubagentParent>>>,
    /// The agent a fork started from (v2 `AgentMeta.forkedFrom`). Written when
    /// the `Agent` tool spawns a fork, so the roster entry records the lineage
    /// the same way v2's `create` does.
    forked_from: Arc<RwLock<HashMap<String, String>>>,
    /// The last turn-sourced request per agent id (v2
    /// `llmRequesterLastTurnRequest`): what a fork compares its first request
    /// against. The main turn publishes per step through its toolset; a
    /// subagent publishes its last request when the turn returns.
    probe_requests: Mutex<HashMap<String, crate::session::cache_observation::CacheProbeRequest>>,
    /// Per-agent context generation, bumped when compaction replaces the
    /// context (v2 `prefixRevision`). A fork captured under an older
    /// generation drops the parent comparison instead of mixing the
    /// pre-compaction baseline into the post-compaction trajectory.
    probe_generations: Mutex<HashMap<String, u64>>,
    /// Child agent id → the parent snapshot captured when its fork scope was
    /// created (v2 `AgentCacheProbeService`'s constructor). Consumed once by
    /// the child's first recorded request.
    pending_fork_probes: Mutex<HashMap<String, PendingForkProbe>>,
    /// The main agent's bound profile for this turn (`RunTurnParams.
    /// agent_profile`, refreshed next to the catalog snapshot; `None` keeps
    /// the default `agent`). A fork inherits the caller's profile (v2 #4152),
    /// so the main agent's name must be known engine-side.
    main_profile_name: RwLock<Option<String>>,
    /// The current turn's host-pushed profile names (v2's
    /// `isDiscoveredAgentProfileSource` set): the spawn allowlist's
    /// delegation extras for the main agent.
    discovered_types: RwLock<Vec<String>>,
    /// The session model's `[models.<alias>]` name, captured when the runtime
    /// LLM is installed (`LLM::model_alias`). v2's `forkIncompatibility` lets a
    /// fork pass the caller's own alias — it is a no-op inherit — and rejects
    /// every other explicit model; without this the engine rejected the
    /// caller's own alias too.
    caller_model_alias: RwLock<Option<String>>,
    /// The workspace root per session id, used to collect the `explore`
    /// profile's git-context prompt prefix at spawn (v2
    /// `AgentProfilePromptPrefixContext.cwd`). Keyed because this manager is
    /// process-wide: concurrent sessions must not read each other's
    /// repository state.
    workspace_roots: RwLock<HashMap<String, std::path::PathBuf>>,
    /// The frozen environment per session id, pushed by the entry that owns
    /// the declaration (v2 `environmentBindingService`). A spawn resolves the
    /// child against it; an entry without a declaration (stdio pipeline, napi
    /// scope) falls back to the session's workspace root.
    session_environments: RwLock<HashMap<String, SessionEnvironment>>,
    /// The session's live workspace roots (v2 `workspaceInfo`), one context per
    /// session so a root set resolved later carries a revision. Spawns read the
    /// snapshot instead of the roots frozen with the environment, so a root
    /// added between turns (host `/add-dir`, `local.toml`) widens the boundary
    /// the next child is resolved against.
    session_workspaces: std::sync::RwLock<HashMap<String, WorkspaceContext>>,
    /// The resolved environment per agent id (v2
    /// `agentWorkspaceContextService`): the inherited session environment id,
    /// the narrowed cwd and the overridden variables. Session-lifetime
    /// bookkeeping: a killed or evicted scope's entry goes with it.
    agent_environments: std::sync::RwLock<HashMap<String, AgentEnvironment>>,
    /// The last merged profile catalog (v2
    /// `ISessionAgentProfileCatalog.inspect`): which source won each name and
    /// which candidates were suppressed.
    profile_catalog: RwLock<Option<Arc<crate::prompt::profiles::MergedProfileCatalog>>>,
    /// Per-agent permission modes (v2 `IAgentPermissionModeService` living per
    /// agent scope). The session mode is broadcast here by the pipeline; an
    /// agent with its own binding (a tower worker pinned to `auto`) keeps it
    /// across those switches. Shared with the permission engine so tool calls
    /// evaluate under the executing agent's mode.
    permission_modes: crate::permission::SharedAgentPermissionModes,
}

/// A foreground subagent's resume record (P55).
#[derive(Clone)]
struct ForegroundResume {
    profile_name: String,
    role: String,
    messages: Vec<crate::turn_loop::types::LLMMessage>,
}

/// The parent snapshot a fork captured when its scope was created.
///
/// v2's `AgentCacheProbeService` constructor reads `parent.lastTurnRequest()`
/// and later re-checks the parent's `prefixRevision` before comparing; this is
/// the native form of that pair (snapshot + the generation it was read under).
#[derive(Clone)]
struct PendingForkProbe {
    parent_id: String,
    parent: Option<crate::session::cache_observation::CacheProbeRequest>,
    generation: u64,
}

/// Persisted state for subagent cold resume (#3478).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentPersistedState {
    pub id: String,
    pub profile_name: String,
    pub role: String,
    pub messages: Vec<crate::turn_loop::types::LLMMessage>,
    pub updated_at: i64,
    /// The agent that spawned this subagent (v2 `subagentParentAgentId`).
    ///
    /// `#[serde(default)]` because records written before this field existed
    /// carry no parent: they read back as "unowned", which the resume gate
    /// rejects rather than silently trusting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_agent_id: Option<String>,
    /// The tool call that spawned it, so a cold resume can re-attach the
    /// member to its `AgentSwarm` call (v2 `parentToolCallId`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_tool_call_id: Option<String>,
    /// The agent this one was forked from (v2 `AgentMeta.forkedFrom`).
    ///
    /// `#[serde(default)]` because records written before this field existed
    /// carry no lineage; they read back as an ordinary spawn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<String>,
    /// The `{{item}}` value this subagent was spawned for, when it was a
    /// swarm member (v2 `subagentSwarmItem`). Lets a later `resume_agent_ids`
    /// call label the member with the item it is continuing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swarm_item: Option<String>,
    /// The member's 1-based position in its batch (v2 `subagentIndex` /
    /// `SubagentSpawnedPayload.swarmIndex`).
    ///
    /// This is the discriminator between a swarm member and an ordinary child:
    /// v2 stamps `role: 'member'` on the tool frame's agent ref exactly when
    /// the index is present (`coreEventMap.ts`), and the session snapshot's
    /// `swarm_index` carries it across a resume. Losing it here is what makes a
    /// restored swarm render as a pile of plain subagent cards.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swarm_index: Option<u32>,
    /// The delegation depth stamped at spawn (v2 `subagentDepth`). A record
    /// written before this field existed reads back as depth 0, which admits a
    /// deeper chain than the live spawn would have — the alternative, refusing
    /// every resume of an old record, loses the conversation instead.
    #[serde(default)]
    pub depth: u32,
}

/// Who spawned a subagent, and as what (v2 `subagentLabels` /
/// `subagentParentAgentId` / `subagentSwarmItem`).
///
/// v2 keeps this in the session metadata as agent labels, and reads it back in
/// three places: `SessionSwarmService.requireOwnedSubagent` refuses to resume an
/// agent the caller does not own, `getSwarmItem` recovers a resumed member's
/// original item, and the snapshot reports the index that separates a member
/// from a child. All three are reproduced here — the native engine has no
/// session metadata, so the record rides the manager.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubagentParent {
    /// The spawning agent's id. The main agent when a tool spawned the child.
    pub agent_id: String,
    /// The tool call that spawned it, when the spawner knew (v2
    /// `parentToolCallId`). The snapshot pairs it with the index so a client
    /// can re-attach a member to its `AgentSwarm` call after a resume.
    pub parent_tool_call_id: Option<String>,
    /// The swarm `{{item}}` this subagent was spawned for, if any.
    pub swarm_item: Option<String>,
    /// The member's 1-based position in its batch; `None` for a plain child.
    pub swarm_index: Option<u32>,
}

impl Default for SubagentManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Outcome of a foreground subagent run (v2 `Agent` tool).
pub enum ForegroundTurnOutcome {
    /// The turn — including any summary-distillation continuations — ran
    /// to completion.
    Completed(crate::turn_loop::types::TurnResult),
    /// The parent turn was cancelled; the subagent run was aborted
    /// immediately at its current await point.
    ParentCancelled,
}

/// Why a foreground run ended without a [`ForegroundTurnOutcome::Completed`].
enum RunExit {
    ParentCancelled,
    Failed(String),
}

/// UTF-16 code-unit length (v2 `String.length`): the summary adequacy
/// floor counts characters the same way the JS policy does.
fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// v2 `classifyTurnResult` mirror: a max-token-truncated subagent turn
/// fails the run with the verbatim v2 error text.
pub const SUBAGENT_MAX_TOKENS_ERROR: &str =
    "Subagent turn failed before completing its final summary: reason=max_tokens";

/// v2 `distillSummary` (P51): while the final assistant text is shorter
/// than the policy floor, re-prompt with the continuation prompt and adopt
/// the newer non-empty assistant text (`final_assistant_summary` keeps the
/// previous one when the continuation answers empty, same as v2). Usage is
/// accumulated across continuation turns.
// Eight parameters, and that is deliberate: the continuation needs the whole
// subagent context (identity, callbacks, tool table, both cancellation flags,
// the turn being continued, and the policy) and there is no natural group
// among them that would not have to be threaded back through both call sites
// just to be taken apart again.
#[allow(clippy::too_many_arguments)]
async fn distill_continuations(
    runtime: &SubagentRuntime,
    agent_id: &str,
    callbacks: &Arc<dyn crate::callbacks::HostCallbacks>,
    tool_defs: Vec<crate::turn_loop::types::ToolInfo>,
    cancel_flag: &Arc<AtomicBool>,
    parent_cancel: Option<&crate::subagent::types::ParentCancel>,
    mut turn: crate::turn_loop::types::TurnResult,
    policy: &crate::subagent::types::SummaryPolicy,
) -> Result<crate::turn_loop::types::TurnResult, RunExit> {
    let mut usage_total = turn.usage.clone();
    let mut attempts = 0u32;
    while utf16_len(final_assistant_summary(&turn.messages).trim()) < policy.min_chars
        && attempts < policy.retries
    {
        attempts += 1;
        let mut history = turn.messages.clone();
        history.push(crate::turn_loop::types::LLMMessage {
            role: "user".into(),
            content: policy.continuation_prompt.clone(),
            ..Default::default()
        });
        turn = run_one(
            runtime,
            agent_id,
            callbacks,
            history,
            tool_defs.clone(),
            cancel_flag,
            parent_cancel,
        )
        .await?;
        usage_total = add_usage(&usage_total, &turn.usage);
    }
    turn.usage = usage_total;
    Ok(turn)
}

/// Run one `run_turn` for the foreground subagent, aborting immediately
/// when the parent cancels: dropping the run future interrupts the
/// subagent at its current await point (in-flight LLM call / tool wait,
/// the v2 AbortSignal semantics the step-top flag checks cannot reach);
/// the instance flag also flips so the scheduler's own cancellation
/// checks observe it.
async fn run_one(
    runtime: &SubagentRuntime,
    agent_id: &str,
    callbacks: &Arc<dyn crate::callbacks::HostCallbacks>,
    messages: Vec<crate::turn_loop::types::LLMMessage>,
    tool_defs: Vec<crate::turn_loop::types::ToolInfo>,
    cancel_flag: &Arc<AtomicBool>,
    parent_cancel: Option<&crate::subagent::types::ParentCancel>,
) -> Result<crate::turn_loop::types::TurnResult, RunExit> {
    // v2 instantiates the external-hooks service per agent; the guard rides
    // the callbacks this session already runs with, so a subagent fires
    // `UserPromptSubmit` / `PreCompact` / `Stop` without sharing another
    // session's hooks.
    let hook_guard = callbacks.hook_guard();
    // v2 `AGENT_RUN_PROMPT_ORIGIN`: the subagent's prompt is a system trigger
    // named `subagent`. `TurnStarted` hooks fire per agent in v2
    // (`agentExternalHooksService`), so the subagent announces its own turn;
    // the numeric suffix keeps the hook payload's turn id unique.
    let origin = crate::subagent::types::agent_run_prompt_origin();
    let turn_seq = fastrand::u64(..);
    if let Some(guard) = hook_guard.as_ref() {
        let prompt = messages
            .iter()
            .rev()
            .find(|message| message.role == "user")
            .map(|message| message.content.clone())
            .unwrap_or_default();
        guard.notify_turn_started(turn_seq, &origin, &prompt).await;
    }
    let end_guard = hook_guard.clone();
    let run_input = crate::turn_loop::types::RunTurnInput {
        agent_id: agent_id.to_string(),
        previous_turn_aborted: false,
        max_attempts: None,
        turn_id: format!("subturn-{turn_seq}"),
        llm: runtime.llm.as_ref(),
        messages,
        tools: &[],
        tool_defs,
        // Foreground subagents run until done — the caller owns the
        // timeout (v2 `resolveSubagentTimeoutMs`), not a step cap.
        max_steps: u32::MAX,
        max_context_tokens: None,
        compaction_max_attempts: None,
        // Subagent turns carry no policy snapshot, and the reminders address
        // the main agent's user interaction (AskUserQuestion / ExitPlanMode).
        permission_mode: None,
        goal: None,
        cancellation: Some(cancel_flag.clone()),
        // v2 instantiates the external-hooks service per agent, so a subagent
        // turn fires `UserPromptSubmit` / `PreCompact` / `Stop` like the main
        // turn, and `run_turn_continued` honors the Stop hook's one-shot
        // continuation inside the same turn.
        hook_guard,
        media: Some(&runtime.media),
        media_dropped: Some(runtime.media_dropped.clone()),
        media_recovery: Some(runtime.media_recovery.clone()),
        toolset: None,
    };
    let run_future = crate::turn_loop::run_turn::run_turn_continued(run_input, callbacks);
    tokio::pin!(run_future);
    let outcome = match parent_cancel {
        Some(signal) => {
            tokio::select! {
                result = &mut run_future => {
                    result.map_err(|err| RunExit::Failed(err.to_string()))
                }
                () = signal.wait() => Err(RunExit::ParentCancelled),
            }
        }
        None => run_future
            .await
            .map_err(|err| RunExit::Failed(err.to_string())),
    };
    if let Some(guard) = end_guard.as_ref() {
        let end = match &outcome {
            Ok(turn) => crate::session::AgentTurnEnd::Ran(turn),
            Err(RunExit::ParentCancelled) => crate::session::AgentTurnEnd::Cancelled,
            Err(RunExit::Failed(message)) => crate::session::AgentTurnEnd::Failed(message),
        };
        crate::session::notify_agent_turn_end(guard, turn_seq, end).await;
    }
    outcome
}

impl SubagentManager {
    pub fn new() -> Self {
        let mut defs = HashMap::new();
        // Built-in research subagent
        defs.insert(
            "research".into(),
            SubagentDefinition {
                name: "research".into(),
                description: "Research subagent with read-only tools for exploring codebase and fetching web info.".into(),
                system_prompt: "You are a specialized research subagent. Use read, grep, glob, fetch_url, and web_search to find information and report concise findings.".into(),
                tools: vec![
                    "read".into(),
                    "grep".into(),
                    "glob".into(),
                    "fetch_url".into(),
                    "web_search".into(),
                    "list_directory".into(),
                ],
                disallowed_tools: Vec::new(),
                when_to_use: None,
                prompt_prefix: None,
                summary_policy: None,
                model: None,
                subagents: None,
            },
        );

        // Pre-register the standard builtin profiles (agent / coder / explore
        // / plan) so spawn lookups resolve them in every transport path. v2
        // registers these from its profile catalog; the Rust side had
        // `register_builtin_profiles` but it was only called from tests,
        // leaving TowerSpawn / AgentSwarm / Agent with builtin
        // profiles to fail with "not available" on every host.
        for p in crate::prompt::ProfileCatalog::with_builtins().list() {
            defs.insert(
                p.name.clone(),
                SubagentDefinition {
                    name: p.name.clone(),
                    description: p.description.clone(),
                    system_prompt: if p.role_additional.is_empty() {
                        format!("You are {}. Complete the user's task accurately.", p.name)
                    } else {
                        p.role_additional.clone()
                    },
                    tools: p.tools.iter().map(|t| t.to_lowercase()).collect(),
                    disallowed_tools: Vec::new(),
                    when_to_use: None,
                    prompt_prefix: None,
                    subagents: p.subagents.clone(),
                    summary_policy: None,
                    model: None,
                },
            );
        }

        // Tower worker profile (v2 `TOWER_WORKER_PROFILE_DEF`):
        // executes one tower mission in its own git worktree, coordinating
        // only through Tower* tools. Without this registration, every
        // `TowerSpawn` lookup returns None and the host falls back with
        // "not available". The role overlay emphasizes the handoff: the
        // worker's final message IS the entire handoff to the parent.
        let tower_worker_tools: Vec<String> = [
            "Agent",
            "Bash",
            "TowerFinding",
            "TowerInbox",
            "TowerMission",
            "TowerReview",
            "TowerSend",
            "TowerStatus",
            "CronCreate",
            "CronDelete",
            "CronList",
            "Edit",
            "ExitPlanMode",
            "Glob",
            "Grep",
            "Read",
            "Skill",
            "TaskList",
            "TaskOutput",
            "TaskStop",
            "TodoList",
            "WaitFor",
            "WebSearch",
            "FetchURL",
            "Write",
            "mcp__*",
        ]
        .iter()
        .map(|s| s.to_lowercase())
        .collect();
        const TOWER_WORKER_ROLE_OVERLAY: &str = "\
Tower worker protocol. Your final message is the entire handoff — the parent sees nothing \
else from your run. Make it technically complete: what you changed and why, the path of every \
file you touched, how you verified the change (tests or commands run, with results), and \
anything left undone or worth follow-up. Coordinate only through the Tower* tools; treat the \
worktree root the tower assigns you as your full authority scope.";
        defs.insert(
            "tower-worker".into(),
            SubagentDefinition {
                name: "tower-worker".into(),
                description: "Tower worker/reviewer agent — executes one tower mission in its own git worktree (or reviews one branch), coordinating only through Tower* tools. Spawned via the TowerSpawn tool.".into(),
                system_prompt: format!(
                    "{}\n\n{}",
                    "You are a tower worker. Complete the assigned mission accurately.",
                    TOWER_WORKER_ROLE_OVERLAY,
                ),
                tools: tower_worker_tools,
                disallowed_tools: Vec::new(),
                when_to_use: None,
                prompt_prefix: None,
                subagents: None,
                summary_policy: Some(crate::subagent::types::SummaryPolicy {
                    min_chars: 200,
                    continuation_prompt: "Your summary was too brief. Expand it: what you \
                        changed and why, the path of every file you touched, how you \
                        verified the change, and anything left undone or worth follow-up."
                        .into(),
                    retries: 1,
                }),
                model: None,
            },
        );

        Self {
            definitions: Arc::new(RwLock::new(defs)),
            instances: Arc::new(RwLock::new(HashMap::new())),
            persistent: Arc::new(RwLock::new(HashMap::new())),
            runtime: RwLock::new(None),
            foreground_histories: Arc::new(Mutex::new(HashMap::new())),
            foreground_runs: Arc::new(Mutex::new(HashSet::new())),
            active_turn_sessions: Arc::new(Mutex::new(HashMap::new())),
            current_session: std::sync::RwLock::new(None),
            next_agent_number: std::sync::atomic::AtomicU64::new(0),
            instance_llms: Arc::new(RwLock::new(HashMap::new())),
            session_store: RwLock::new(None),
            task_runner: RwLock::new(None),
            swarm_timeout_ms: Mutex::new(None),
            scope_cache: Mutex::new(ScopeCache::from_env()),
            usage_by_instance: Arc::new(Mutex::new(HashMap::new())),
            parents: Arc::new(RwLock::new(HashMap::new())),
            forked_from: Arc::new(RwLock::new(HashMap::new())),
            probe_requests: Mutex::new(HashMap::new()),
            probe_generations: Mutex::new(HashMap::new()),
            pending_fork_probes: Mutex::new(HashMap::new()),
            main_profile_name: RwLock::new(None),
            discovered_types: RwLock::new(Vec::new()),
            caller_model_alias: RwLock::new(None),
            workspace_roots: RwLock::new(HashMap::new()),
            session_environments: RwLock::new(HashMap::new()),
            session_workspaces: std::sync::RwLock::new(HashMap::new()),
            agent_environments: std::sync::RwLock::new(HashMap::new()),
            profile_catalog: RwLock::new(Some(Arc::new(
                crate::prompt::profiles::MergedProfileCatalog::merge(
                    crate::prompt::ProfileCatalog::with_builtins().list(),
                    &[],
                ),
            ))),
            permission_modes: Arc::new(crate::permission::AgentPermissionModes::default()),
        }
    }

    /// The per-agent permission mode registry shared with the permission
    /// engine (v2 `IAgentPermissionModeService` per agent scope plus
    /// `broadcastPermissionMode`).
    pub fn permission_modes(&self) -> crate::permission::SharedAgentPermissionModes {
        self.permission_modes.clone()
    }

    /// Switch the session mode and broadcast it to every agent without a
    /// binding of its own (v2 `broadcastPermissionMode`). Unbound agents
    /// inherit the session mode from the registry, so this is one write; a
    /// bound agent (tower worker pinned to `auto`) keeps its mode.
    pub fn broadcast_permission_mode(&self, mode: crate::permission::PermissionMode) {
        self.permission_modes.set_session_mode(mode);
    }

    /// Pin one agent to a mode (v2 `IAgentPermissionModeService.setMode` at
    /// `TowerSpawn`: workers run under `auto`).
    pub fn bind_permission_mode(&self, agent_id: &str, mode: crate::permission::PermissionMode) {
        self.permission_modes.bind(agent_id, mode);
    }

    /// The mode one agent currently runs under (its binding, else the session
    /// default broadcast by the pipeline).
    pub fn permission_mode_for(&self, agent_id: &str) -> Option<crate::permission::PermissionMode> {
        self.permission_modes.mode_for(agent_id)
    }

    /// Whether any subagent turn belonging to `session_id` is in flight (v2
    /// `sessionActivityService`'s agent folds). Detached tower/swarm turns keep
    /// the session busy after the main turn ends, so the idle reaper and the
    /// prompt queue must see them.
    pub fn session_has_active_turn(&self, session_id: &str) -> bool {
        self.active_turn_sessions
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .values()
            .any(|session| session == session_id)
    }

    /// Host-pushed `[swarm] timeout_ms` for the native `AgentSwarm` tool (v2
    /// `resolveSwarmTimeoutMs`). `None` clears the override so the swarm
    /// falls back to the 2h swarm default; swarms never inherit the
    /// subagent timeout. `Some(0)` means "explicitly no timeout" and rides
    /// through — the tool maps it to the never-expiring sentinel (v2
    /// `taskService` arms only when `timeoutMs > 0`). Set per pipeline build.
    pub fn set_swarm_timeout_ms(&self, timeout_ms: Option<u64>) {
        *self
            .swarm_timeout_ms
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = timeout_ms;
    }

    /// The host-resolved swarm timeout, if any. Read at `AgentSwarm` execution.
    pub fn swarm_timeout_ms(&self) -> Option<u64> {
        *self
            .swarm_timeout_ms
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Construct with a persistent TaskRunner for background subagents.
    pub fn with_task_runner(self, runner: Arc<crate::storage::TaskRunner>) -> Self {
        *self.task_runner.try_write().unwrap() = Some(runner);
        self
    }

    /// Inject or update TaskRunner asynchronously.
    pub async fn set_task_runner(&self, runner: Arc<crate::storage::TaskRunner>) {
        *self.task_runner.write().await = Some(runner);
    }

    /// Inject or update TaskRunner synchronously if possible.
    ///
    /// `try_write` fails whenever any reader holds the lock, and a dropped
    /// runner is permanent: `background_task_list` / `background_task_stop`
    /// read it back through [`Self::get_task_runner_sync`] for the rest of the
    /// process, so a single contended instant used to kill the `TaskList` /
    /// `TaskStop` tools until the next pipeline build. Retry briefly, then say
    /// so — [`Self::set_task_runner`] is the lossless path.
    pub fn set_task_runner_sync(&self, runner: Arc<crate::storage::TaskRunner>) {
        for _ in 0..8 {
            if let Ok(mut guard) = self.task_runner.try_write() {
                *guard = Some(runner);
                return;
            }
            std::thread::yield_now();
        }
        tracing::warn!(
            "task runner not attached: the subagent manager's runner lock was contended; \
             background TaskList/TaskStop will report no runner until the next pipeline build"
        );
    }

    /// Get the attached TaskRunner if present.
    pub async fn get_task_runner(&self) -> Option<Arc<crate::storage::TaskRunner>> {
        self.task_runner.read().await.clone()
    }

    /// Synchronously get the attached TaskRunner if the lock is immediately available.
    pub fn get_task_runner_sync(&self) -> Option<Arc<crate::storage::TaskRunner>> {
        self.task_runner.try_read().ok().and_then(|g| g.clone())
    }

    /// Construct with a persistent SQLite store for cold recovery (#3478).
    pub fn with_store(store: Arc<SqliteSessionStore>) -> Self {
        let manager = Self::new();
        *manager.session_store.try_write().unwrap() = Some(store);
        manager
    }

    /// Inject or update SQLite store.
    pub async fn set_session_store(&self, store: Arc<SqliteSessionStore>) {
        *self.session_store.write().await = Some(store);
    }

    /// Register standard builtin profiles from ProfileCatalog (agent, coder, explore, plan).
    pub async fn register_builtin_profiles(&self) {
        let catalog = crate::prompt::ProfileCatalog::with_builtins();
        for p in catalog.list() {
            self.register_definition(SubagentDefinition {
                name: p.name.clone(),
                description: p.description.clone(),
                system_prompt: if p.role_additional.is_empty() {
                    format!("You are {}. Complete the user's task accurately.", p.name)
                } else {
                    p.role_additional.clone()
                },
                tools: p.tools.iter().map(|t| t.to_lowercase()).collect(),
                disallowed_tools: Vec::new(),
                when_to_use: None,
                prompt_prefix: None,
                subagents: p.subagents.clone(),
                summary_policy: None,
                model: None,
            })
            .await;
        }
    }

    /// Bind one subagent instance to a specific LLM (the `[secondary_model]`
    /// pool choice). The run paths prefer this over the session runtime LLM,
    /// so concurrent subagents can run on different models; unbound instances
    /// keep inheriting the session model.
    pub async fn set_instance_llm(&self, id: &str, llm: Arc<dyn crate::turn_loop::types::LLM>) {
        self.instance_llms.write().await.insert(id.to_string(), llm);
    }

    async fn instance_llm(&self, id: &str) -> Option<Arc<dyn crate::turn_loop::types::LLM>> {
        self.instance_llms.read().await.get(id).cloned()
    }

    /// The LLM a specific instance is bound to, if any (v2 #4152: a fork
    /// inherits the caller's model verbatim, so a pool-bound caller's fork
    /// must bind the same pool entry, not the session model).
    pub async fn instance_llm_of(&self, id: &str) -> Option<Arc<dyn crate::turn_loop::types::LLM>> {
        self.instance_llm(id).await
    }

    /// Refresh the main agent's bound profile for this turn (called next to
    /// [`Self::register_profile_snapshot`]).
    pub async fn set_main_profile_name(&self, name: Option<&str>) {
        *self.main_profile_name.write().await = name
            .filter(|name| !name.trim().is_empty())
            .map(str::to_string);
    }

    /// The caller's profile name for a spawn (v2 `profile.data().profileName`):
    /// the main agent's bound profile (default `agent`), or a subagent
    /// caller's own type.
    pub async fn caller_profile_name(&self, caller_id: &str) -> Option<String> {
        if caller_id == crate::callbacks::MAIN_AGENT_ID {
            return Some(
                self.main_profile_name
                    .read()
                    .await
                    .clone()
                    .unwrap_or_else(|| "agent".to_string()),
            );
        }
        self.instances
            .read()
            .await
            .get(caller_id)
            .map(|(instance, _)| instance.type_name.clone())
    }

    /// The caller's effective spawn allowlist (v2 `planSpawn`'s
    /// `subagentAllowlistFor` + `withoutDelegatingTargets` halves, and the
    /// bind-time `rootDelegationExtras` for the main agent): the caller's
    /// own declared `subagents`, else the default profile's delegation set;
    /// the main agent additionally gains the turn's discovered types unless
    /// a custom main profile declared its own list. An undeclared caller
    /// may not chain delegation — targets that can themselves spawn are
    /// filtered out. `None` means unrestricted (`['*']`).
    pub async fn spawn_allowlist(&self, caller_id: &str) -> Option<Vec<String>> {
        use crate::subagent::allowlist as gate;
        let caller_profile = self.caller_profile_name(caller_id).await?;
        let is_main = caller_id == crate::callbacks::MAIN_AGENT_ID;
        let definitions = self.definitions.read().await;
        let own = definitions
            .get(&caller_profile)
            .and_then(|def| def.subagents.clone());
        let extras = if is_main && !(caller_profile != "agent" && own.is_some()) {
            // v2 `rootDelegationExtras`: the discovered set excludes the
            // default profile itself and the tower worker (v2's `feature:`
            // source) — builtins dedupe against the declared list anyway.
            Some(
                self.discovered_types
                    .read()
                    .await
                    .iter()
                    .filter(|name| {
                        // The tower worker is v2's `feature:` source — never
                        // a delegation extra (the const is private to
                        // tools/tower, so the filter names the literal).
                        name.as_str() != "agent" && name.as_str() != "tower-worker"
                    })
                    .cloned()
                    .collect::<Vec<String>>(),
            )
        } else {
            None
        };
        let default_subagents = definitions
            .get("agent")
            .and_then(|def| def.subagents.clone());
        let mut allowlist = gate::subagent_allowlist_for(
            default_subagents.as_deref(),
            own.as_deref(),
            extras.as_deref(),
        )?;
        if own.is_none() {
            allowlist = gate::without_delegating_targets(&definitions, &allowlist);
        }
        Some(allowlist)
    }

    /// v2 `planSpawn`'s `AGENT_TYPE_NOT_ALLOWED` arm: a non-fork spawn whose
    /// requested type is outside the caller's allowlist is refused with the
    /// guidance text. Forks skip the gate — a fork inherits the caller's own
    /// type, which the caller's allowlist already admitted once.
    pub async fn check_spawn_allowed(
        &self,
        caller_id: &str,
        requested: &str,
        is_fork: bool,
    ) -> Result<(), String> {
        if is_fork {
            return Ok(());
        }
        if let Some(allowlist) = self.spawn_allowlist(caller_id).await
            && !allowlist.iter().any(|name| name == requested)
        {
            return Err(
                crate::subagent::allowlist::subagent_type_not_allowed_message(
                    requested, &allowlist,
                ),
            );
        }
        Ok(())
    }

    /// The advertise-hop raw material for the `Agent` description (v2's
    /// allowlist-filtered profile listing): the caller's effective
    /// allowlist plus the discovered definitions. `None` when the caller's
    /// profile is unknown.
    pub async fn spawn_catalog(&self, caller_id: &str) -> Option<SpawnCatalog> {
        let allowlist = self.spawn_allowlist(caller_id).await;
        let discovered = {
            let definitions = self.definitions.read().await;
            self.discovered_types
                .read()
                .await
                .iter()
                .filter_map(|name| definitions.get(name).cloned())
                .collect()
        };
        Some(SpawnCatalog {
            allowlist,
            discovered,
        })
    }

    /// Inject the execution runtime (LLM + callbacks) so subagents can run
    /// autonomous turns. Called after the callback pipeline is assembled.
    pub async fn set_runtime(
        &self,
        llm: Arc<dyn crate::turn_loop::types::LLM>,
        callbacks: Arc<dyn crate::callbacks::HostCallbacks>,
        session_id: Option<String>,
    ) {
        // The alias belongs to the transport that is being installed, so it is
        // captured here rather than resolved at fork time.
        *self.caller_model_alias.write().await = llm.model_alias().map(str::to_owned);
        *self
            .current_session
            .write()
            .unwrap_or_else(|error| error.into_inner()) = session_id.clone();
        *self.runtime.write().await = Some(Arc::new(SubagentRuntime {
            llm,
            callbacks,
            session_id,
            media: crate::llm::media_resolver::MediaResolver::new(),
            media_dropped: Default::default(),
            media_recovery: Default::default(),
        }));
    }

    /// The session model's `[models.<alias>]` name, when the installed transport
    /// knows it. `None` for a transport that only knows the wire model name.
    pub async fn caller_model_alias(&self) -> Option<String> {
        self.caller_model_alias.read().await.clone()
    }

    /// Set a session's workspace root. The `explore` profile's git-context
    /// prefix is collected from it at spawn time (v2
    /// `AgentProfilePromptPrefixContext.cwd`).
    pub async fn set_workspace_root(&self, session_id: &str, root: Option<std::path::PathBuf>) {
        let mut roots = self.workspace_roots.write().await;
        match root {
            Some(root) => {
                roots.insert(session_id.to_string(), root);
            }
            None => {
                roots.remove(session_id);
            }
        }
    }

    /// Register a session's frozen environment.
    ///
    /// Called by the entry that owns the declaration (the server engine, at
    /// create and at resume). Spawns from then on resolve children against the
    /// frozen snapshot instead of re-deriving one from the current config.
    pub async fn set_session_environment(
        &self,
        session_id: &str,
        snapshot: EnvironmentSnapshot,
        roots: WorkspaceRoots,
    ) {
        self.refresh_session_workspace_roots(session_id, roots.clone());
        self.session_environments.write().await.insert(
            session_id.to_string(),
            SessionEnvironment { snapshot, roots },
        );
    }

    /// Forget a session's frozen environment (chat-only binding replaced it, or
    /// the session ended). Spawns fall back to the session workspace root.
    pub async fn clear_session_environment(&self, session_id: &str) {
        self.session_environments.write().await.remove(session_id);
        self.session_workspaces
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .remove(session_id);
    }

    /// The session's live workspace context, when one has been registered.
    /// Consumers that need to observe a root change read the snapshot; the
    /// broadcast feed has no push consumer in this engine because prompts,
    /// sandboxes and spawns all re-read the roots per turn or per call.
    #[must_use]
    pub fn session_workspace_context(&self, session_id: &str) -> Option<WorkspaceContext> {
        self.session_workspaces
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .get(session_id)
            .cloned()
    }

    /// The roots a spawn for `session_id` resolves against, without awaiting.
    #[must_use]
    pub fn session_workspace_roots_sync(&self, session_id: &str) -> Option<WorkspaceRoots> {
        self.session_workspace_context(session_id)
            .map(|context| context.roots())
    }

    /// The profiles the live catalog advertises (v2
    /// `ISessionAgentProfileCatalog.list`), sorted by name so a snapshot
    /// comparison never depends on registration order.
    pub async fn profile_listing(&self) -> Vec<SubagentProfileSummary> {
        let definitions = self.definitions.read().await;
        let mut out: Vec<SubagentProfileSummary> = definitions
            .values()
            .map(|definition| SubagentProfileSummary {
                name: definition.name.clone(),
                description: definition.description.clone(),
                when_to_use: definition.when_to_use.clone(),
            })
            .collect();
        out.sort_by(|left, right| left.name.cmp(&right.name));
        out
    }

    /// Publish the roots a session's next turn resolved (v2
    /// `workspaceInfo.onDidChange`): the engine-wide list, the session's
    /// recorded `additional_dirs` and the project's `local.toml` are re-derived
    /// per turn, and a change between turns must be visible to the spawns that
    /// follow. Returns the published revision only when the set actually
    /// changed; re-publishing the same roots is a no-op.
    pub fn refresh_session_workspace_roots(
        &self,
        session_id: &str,
        roots: WorkspaceRoots,
    ) -> Option<WorkspaceChange> {
        let context = {
            let mut contexts = self
                .session_workspaces
                .write()
                .unwrap_or_else(|error| error.into_inner());
            contexts
                .entry(session_id.to_string())
                .or_insert_with(|| WorkspaceContext::new(roots.clone()))
                .clone()
        };
        if context.roots() == roots {
            return None;
        }
        Some(context.set_roots(roots))
    }

    /// The resolved environment of one agent, if its spawn bound one.
    pub async fn agent_environment(&self, agent_id: &str) -> Option<AgentEnvironment> {
        self.agent_environment_sync(agent_id)
    }

    /// The same lookup without awaiting, for the synchronous tool layer: a tool
    /// call resolves relative paths and its execution directory against the
    /// caller's bound cwd, and must not block on a runtime lock to do it.
    #[must_use]
    pub fn agent_environment_sync(&self, agent_id: &str) -> Option<AgentEnvironment> {
        self.agent_environments
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .get(agent_id)
            .cloned()
    }

    /// Current execution runtime, if injected.
    pub async fn runtime(&self) -> Option<Arc<SubagentRuntime>> {
        self.runtime.read().await.clone()
    }

    /// Register or update a subagent definition.
    pub async fn register_definition(&self, def: SubagentDefinition) {
        let mut defs = self.definitions.write().await;
        defs.insert(def.name.clone(), def);
    }

    /// Register a definition AS a host-discovered profile (the per-turn
    /// snapshot path in miniature): the name also joins the spawn gate's
    /// delegation extras (v2 `rootDelegationExtras`). Runtime-registered
    /// custom subagents need this or the gate refuses their own spawns.
    pub async fn register_discovered_definition(&self, def: SubagentDefinition) {
        {
            let mut discovered = self.discovered_types.write().await;
            if !discovered.contains(&def.name) {
                discovered.push(def.name.clone());
            }
        }
        self.register_definition(def).await;
    }

    /// Register the host-pushed profile catalog snapshot (P46). Called per
    /// turn with the session's profiles; same-name entries overwrite, so
    /// the freshest snapshot wins (session-level snapshot semantics, same
    /// as the policy snapshot). The names are also the spawn gate's
    /// discovered set (v2 `isDiscoveredAgentProfileSource`): the main
    /// agent's allowlist gains them as delegation extras.
    ///
    /// The snapshot is merged through the session catalog's projection (v2
    /// `SessionAgentProfileCatalogService.reproject`): a name that wins keeps
    /// its declared `subagents`, while one that omits them inherits the
    /// replaced profile's list; a candidate that loses is never registered and
    /// is recorded for [`Self::inspect_profile`].
    pub async fn register_profile_snapshot(
        &self,
        profiles: &[crate::rpc::types::SubagentProfileWire],
    ) {
        *self.discovered_types.write().await = profiles
            .iter()
            .map(|profile| profile.name.clone())
            .collect();

        const SESSION_SOURCE_ID: &str = "session-agent-file";
        let builtins = crate::prompt::ProfileCatalog::with_builtins();
        let candidates: Vec<crate::prompt::ProfileCandidate> = profiles
            .iter()
            .map(|profile| crate::prompt::ProfileCandidate {
                profile: crate::prompt::AgentProfile {
                    name: profile.name.clone(),
                    description: profile.description.clone(),
                    when_to_use: profile.when_to_use.clone(),
                    role_additional: profile.system_prompt.clone(),
                    tools: profile.tools.clone(),
                    subagents: profile.subagents.clone(),
                },
                source_id: SESSION_SOURCE_ID.to_string(),
                priority: 0,
                // The host already applied the same rule when it discovered
                // the file (`node-sdk/src/agent-file.ts`: a same-name builtin
                // without `override: true` never reaches this snapshot), so an
                // arriving name is an approved replacement.
                override_builtin: true,
            })
            .collect();
        let merged = crate::prompt::MergedProfileCatalog::merge(builtins.list(), &candidates);

        for profile in profiles {
            let Some(inspection) = merged.inspect(&profile.name) else {
                continue;
            };
            // A suppressed candidate keeps the profile that won; the wire
            // entry must not overwrite it.
            if !inspection.source_id.eq_ignore_ascii_case(SESSION_SOURCE_ID) {
                tracing::warn!(
                    profile = %profile.name,
                    source_id = %inspection.source_id,
                    "agent profile snapshot ignored: a same-name profile already won the catalog"
                );
                continue;
            }
            self.register_definition(SubagentDefinition {
                name: profile.name.clone(),
                description: profile.description.clone(),
                when_to_use: profile.when_to_use.clone(),
                system_prompt: profile.system_prompt.clone(),
                tools: profile.tools.clone(),
                disallowed_tools: profile.disallowed_tools.clone(),
                prompt_prefix: profile.prompt_prefix.clone(),
                subagents: inspection.profile.subagents.clone(),
                summary_policy: profile.summary_policy.clone(),
                model: None,
            })
            .await;
        }
        *self.profile_catalog.write().await = Some(Arc::new(merged));
    }

    /// v2 `ISessionAgentProfileCatalog.inspect`: the winning source for a
    /// profile name and the candidates that were suppressed.
    pub async fn inspect_profile(
        &self,
        name: &str,
    ) -> Option<crate::prompt::profiles::ProfileInspection> {
        self.profile_catalog
            .read()
            .await
            .as_ref()?
            .inspect(name)
            .cloned()
    }

    /// The builtin `explore` profile's git-context prefix (v2
    /// `session/agentLifecycle/profile/profiles.ts:126-132` + `gitContext.ts`).
    /// A host-pushed profile replaces the builtin wholesale (v2 `override:
    /// true`), including its `promptPrefix`, so a discovered name is skipped.
    async fn builtin_explore_prompt_prefix(
        &self,
        agent_id: &str,
        def: &SubagentDefinition,
    ) -> Option<String> {
        if !def.name.eq_ignore_ascii_case("explore") {
            return None;
        }
        if self
            .discovered_types
            .read()
            .await
            .iter()
            .any(|name| name.eq_ignore_ascii_case(&def.name))
        {
            return None;
        }
        // A narrowed child reads the repository at its own cwd (v2 reads the
        // agent scope's workspace context, not the session's). The guard is
        // scoped: a std lock must not be held across the awaits below.
        let agent_cwd = {
            let environments = self
                .agent_environments
                .read()
                .unwrap_or_else(|error| error.into_inner());
            environments
                .get(agent_id)
                .map(|environment| environment.cwd().to_path_buf())
        };
        let root = match agent_cwd {
            Some(cwd) => cwd,
            None => {
                let session_id = self
                    .runtime
                    .read()
                    .await
                    .as_ref()
                    .and_then(|runtime| runtime.session_id.clone())?;
                self.workspace_roots
                    .read()
                    .await
                    .get(&session_id)
                    .cloned()?
            }
        };
        let context = crate::prompt::collect_git_context(&root).await;
        (!context.is_empty()).then_some(context)
    }

    /// Get a registered definition by name.
    pub async fn get_definition(&self, name: &str) -> Option<SubagentDefinition> {
        let defs = self.definitions.read().await;
        defs.get(name).cloned()
    }

    /// v2 `agentLifecycleService.ts:188-200`: auto ids are `agent-N`,
    /// monotonic per manager and never reusing a number already observed on a
    /// resident or rebuilt instance — or on an agent the session roster
    /// persisted (v2 `nextAvailableAgentId` reads `sessionMetadata.agents`).
    async fn allocate_agent_id(&self) -> String {
        self.seed_agent_counter_from_roster().await;
        format!(
            "agent-{}",
            self.next_agent_number.fetch_add(1, Ordering::Relaxed) + 1
        )
    }

    /// Keep the auto-id counter above every `agent-N` the session roster
    /// already holds: a restart starts from zero otherwise and reuses ids of
    /// agents whose scope is gone but whose metadata (and transcript) remains.
    async fn seed_agent_counter_from_roster(&self) {
        let Some((session_id, store)) = self.metadata_target().await else {
            return;
        };
        let Ok(Some(summary)) = store.get_session(&session_id) else {
            return;
        };
        let Some(agents) = summary
            .agents
            .as_ref()
            .and_then(serde_json::Value::as_object)
        else {
            return;
        };
        for id in agents.keys() {
            self.observe_agent_id(id);
        }
    }

    /// The session whose roster the manager writes, and its store.
    async fn metadata_target(&self) -> Option<(String, Arc<SqliteSessionStore>)> {
        let session_id = self
            .runtime
            .read()
            .await
            .as_ref()?
            .session_id
            .clone()
            .filter(|session| !session.is_empty())?;
        let store = self.session_store.read().await.as_ref()?.clone();
        Some((session_id, store))
    }

    /// v2 `registerAgent` (`sessionMetadataService.ts:170-188`): seed the
    /// session roster with one subagent's metadata. An identical entry is a
    /// no-op; a store failure only warns, like v2's fire-and-forget caller.
    async fn register_agent_metadata(&self, id: &str) {
        let Some((session_id, store)) = self.metadata_target().await else {
            return;
        };
        let Some(meta) = self.agent_meta_value(id).await else {
            return;
        };
        if let Err(error) = store.register_session_agent(&session_id, id, &meta) {
            tracing::warn!(
                subagent = %id,
                %error,
                "agent roster registration failed"
            );
        }
    }

    /// v2 `AgentMeta` for one agent (`sessionMetadata.ts:4-11`), built from the
    /// maps the engine keeps: `subagentLabels` (`parentAgentId`, `swarmItem`,
    /// `subagentDepth`), `withSubagentProfile` (`profileName`), and the fork
    /// lineage.
    async fn agent_meta_value(&self, id: &str) -> Option<serde_json::Value> {
        let (profile_name, depth) = {
            let instances = self.instances.read().await;
            let (instance, _) = instances.get(id)?;
            (instance.type_name.clone(), instance.depth)
        };
        let parent = self.parent_of(id).await;
        // v2 `create` defaults `meta.parentAgentId` to `'main'`; only a
        // subagent caller records a deeper parent (and then `subagentLabels`
        // carries the same id).
        let parent_agent_id = parent
            .as_ref()
            .map(|p| p.agent_id.clone())
            .unwrap_or_else(|| crate::callbacks::MAIN_AGENT_ID.to_string());
        let forked_from = match self.forked_from.read().await.get(id).cloned() {
            Some(source) => Some(source),
            None => self.persisted_forked_from(id).await,
        };
        let homedir = self.agent_homedir(id).await;

        let mut labels = serde_json::Map::new();
        labels.insert(
            "parentAgentId".into(),
            serde_json::Value::String(parent_agent_id.clone()),
        );
        if let Some(item) = parent.as_ref().and_then(|p| p.swarm_item.clone()) {
            labels.insert("swarmItem".into(), serde_json::Value::String(item));
        }
        labels.insert(
            "subagentDepth".into(),
            serde_json::Value::String(depth.to_string()),
        );
        labels.insert(
            "profileName".into(),
            serde_json::Value::String(profile_name),
        );

        let mut meta = serde_json::Map::new();
        meta.insert("type".into(), serde_json::Value::String("sub".into()));
        meta.insert(
            "parentAgentId".into(),
            serde_json::Value::String(parent_agent_id),
        );
        if let Some(source) = forked_from {
            meta.insert("forkedFrom".into(), serde_json::Value::String(source));
        }
        if let Some(homedir) = homedir {
            meta.insert("homedir".into(), serde_json::Value::String(homedir));
        }
        meta.insert("labels".into(), serde_json::Value::Object(labels));
        Some(serde_json::Value::Object(meta))
    }

    /// v2 `join(bootstrap.homeDir, scope('agents/${agentId}'))`: the per-agent
    /// home under the session's engine directory. Absent until the session's
    /// workspace root is known.
    async fn agent_homedir(&self, id: &str) -> Option<String> {
        let session_id = self
            .runtime
            .read()
            .await
            .as_ref()?
            .session_id
            .clone()
            .filter(|session| !session.is_empty())?;
        let root = self
            .workspace_roots
            .read()
            .await
            .get(&session_id)
            .cloned()?;
        let dir = crate::storage::paths::engine_session_dir(&root, &session_id).ok()?;
        Some(dir.join("agents").join(id).to_string_lossy().into_owned())
    }

    /// `forkedFrom` from the persisted resume record, for a fork whose resident
    /// entry the LRU already evicted.
    async fn persisted_forked_from(&self, id: &str) -> Option<String> {
        let store = self.session_store.read().await.as_ref().cloned()?;
        let value = store.get_state("subagent_resume", id).ok().flatten()?;
        serde_json::from_value::<SubagentPersistedState>(value)
            .ok()?
            .forked_from
    }

    /// Whether this engine knows `agent_id` — resident, persistent, or on disk.
    /// v2's fork source check (`agentLifecycleService.ts:446-447`) needs this
    /// to answer `AGENT_NOT_FOUND` for a caller that no longer exists.
    pub async fn agent_exists(&self, agent_id: &str) -> bool {
        if agent_id == crate::callbacks::MAIN_AGENT_ID {
            return true;
        }
        if self.instances.read().await.contains_key(agent_id) {
            return true;
        }
        if self.persistent.read().await.contains_key(agent_id) {
            return true;
        }
        if self.parents.read().await.contains_key(agent_id) {
            return true;
        }
        let Some(store) = self.session_store.read().await.as_ref().cloned() else {
            return false;
        };
        matches!(store.get_state("subagent_resume", agent_id), Ok(Some(_)))
    }

    /// Keep the auto-id counter above any `agent-N` id that entered the
    /// registry by another route (explicit spawn, cold rebuild after resume).
    fn observe_agent_id(&self, id: &str) {
        if let Some(number) = id
            .strip_prefix("agent-")
            .and_then(|number| number.parse::<u64>().ok())
        {
            self.next_agent_number.fetch_max(number, Ordering::Relaxed);
        }
    }

    /// Spawn a new subagent instance.
    pub async fn spawn(&self, type_name: &str, role: &str) -> Result<String, String> {
        let id = self.allocate_agent_id().await;
        self.spawn_with_id(&id, type_name, role).await
    }

    /// Spawn a subagent with an explicit per-agent environment request.
    ///
    /// The resolver inherits the caller's environment (a session id, a cwd and
    /// a set of variables) and only lets the child narrow: a relative cwd is
    /// joined to the caller's cwd, an absolute one must stay inside the parent
    /// workspace, and variables override key by key. There is no flag to widen
    /// that boundary. Internal spawn sites that know a narrower directory (a
    /// tower worker in its worktree) use this; the model-facing `Agent` tool
    /// never sees a cwd parameter.
    pub async fn spawn_in_environment(
        &self,
        type_name: &str,
        role: &str,
        environment: AgentEnvironmentRequest,
    ) -> Result<String, String> {
        let id = self.allocate_agent_id().await;
        let caller_id = crate::tools::CALLER_AGENT_ID
            .try_with(|id| id.clone())
            .unwrap_or_else(|_| crate::callbacks::MAIN_AGENT_ID.to_string());
        self.spawn_with_id_from_in(&id, &caller_id, type_name, role, environment)
            .await
    }

    /// Spawn a subagent for an explicitly named caller.
    ///
    /// A batch resolves its members on the scheduler's own tasks, which do not
    /// inherit the caller's [`crate::tools::CALLER_AGENT_ID`] scope, so the
    /// caller is passed in rather than read from the ambient scope. Everything
    /// else — the depth gate included — is the shared spawn path.
    pub async fn spawn_child(
        &self,
        caller_id: &str,
        type_name: &str,
        role: &str,
    ) -> Result<String, String> {
        let id = self.allocate_agent_id().await;
        self.spawn_with_id_from(&id, caller_id, type_name, role)
            .await
    }

    /// Spawn a new subagent instance with an explicit identifier.
    pub async fn spawn_with_id(
        &self,
        id: &str,
        type_name: &str,
        role: &str,
    ) -> Result<String, String> {
        let caller_id = crate::tools::CALLER_AGENT_ID
            .try_with(|id| id.clone())
            .unwrap_or_else(|_| crate::callbacks::MAIN_AGENT_ID.to_string());
        self.spawn_with_id_from(id, &caller_id, type_name, role)
            .await
    }

    /// The shared spawn path: resolve the child's delegation depth from its
    /// caller, refuse a caller already at [`MAX_SUBAGENT_DEPTH`] (v2
    /// `assertSubagentDepthAllowed`), resolve the child's environment against
    /// the caller's, then register the instance.
    pub async fn spawn_with_id_from(
        &self,
        id: &str,
        caller_id: &str,
        type_name: &str,
        role: &str,
    ) -> Result<String, String> {
        self.spawn_with_id_from_in(
            id,
            caller_id,
            type_name,
            role,
            AgentEnvironmentRequest::default(),
        )
        .await
    }

    /// [`Self::spawn_with_id_from`] with an explicit environment request.
    pub async fn spawn_with_id_from_in(
        &self,
        id: &str,
        caller_id: &str,
        type_name: &str,
        role: &str,
        environment: AgentEnvironmentRequest,
    ) -> Result<String, String> {
        self.scope_cache_error()?;
        {
            let defs = self.definitions.read().await;
            if !defs.contains_key(type_name) && type_name != "self" {
                return Err(format!("Unknown subagent type: '{type_name}'"));
            }
        }
        let depth = self.child_depth_for_spawn(caller_id).await?;
        // Outside the duplicate check below: a refused environment must not
        // leave a half-registered instance behind, and an existing instance
        // must not have its environment replaced by a failed spawn.
        let environment = self
            .resolve_agent_environment(id, caller_id, environment)
            .await?;

        let cancellation = Arc::new(AtomicBool::new(false));

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let instance = SubagentInstance {
            id: id.to_string(),
            type_name: type_name.to_string(),
            role: role.to_string(),
            state: SubagentState::Running,
            created_at_ms: now_ms,
            last_result: None,
            depth,
        };

        let mut instances = self.instances.write().await;
        // v2 `agentLifecycleService.ts:202-208`: explicit ids are unique; a
        // duplicate spawn must fail loudly instead of silently replacing the
        // running instance (and its cancellation flag) underneath its caller.
        if instances.contains_key(id) {
            return Err(format!(
                "agent.already_exists: a subagent with id \"{id}\" already exists — pass a different id"
            ));
        }
        instances.insert(id.to_string(), (instance, cancellation));
        drop(instances);
        if let Some(environment) = environment {
            self.agent_environments
                .write()
                .unwrap_or_else(|error| error.into_inner())
                .insert(id.to_string(), environment);
        }
        self.observe_agent_id(id);
        self.revive_scope(id);
        self.record_parent(id);
        self.register_agent_metadata(id).await;

        Ok(id.to_string())
    }

    /// Resolve a child's environment against its parent.
    ///
    /// The parent is the caller's own resolved environment when it has one — a
    /// grandchild narrows relative to its caller, not to the session — else the
    /// session's frozen snapshot, else the session workspace root (an entry
    /// without an environment declaration). A spawn with no resolvable root may
    /// proceed only when it asks for nothing: there is no boundary to confine
    /// a requested cwd to.
    async fn resolve_agent_environment(
        &self,
        agent_id: &str,
        caller_id: &str,
        request: AgentEnvironmentRequest,
    ) -> Result<Option<AgentEnvironment>, String> {
        let Some(session) = self.parent_session_environment().await? else {
            if request.cwd.is_some() || !request.env.is_empty() {
                return Err(format!(
                    "{}: agent '{agent_id}' requested a working directory or variables, but the session has no workspace root",
                    crate::environment::codes::WORKDIR_REQUIRED
                ));
            }
            return Ok(None);
        };
        let request = self.caller_relative_request(caller_id, request).await;
        AgentEnvironment::resolve(agent_id, &session.snapshot, &session.roots, request)
            .map(Some)
            .map_err(|error| format!("{}: {}", error.code(), error.detail()))
    }

    /// The session environment a new child inherits from.
    async fn parent_session_environment(&self) -> Result<Option<SessionEnvironment>, String> {
        let Some(session_id) = self
            .current_session
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
        else {
            return Ok(None);
        };
        let stored = self
            .session_environments
            .read()
            .await
            .get(&session_id)
            .cloned();
        if let Some(mut environment) = stored {
            // The live roots replace the frozen ones, so a root added between
            // turns is part of the boundary the next child resolves against.
            if let Some(roots) = self.session_workspace_roots_sync(&session_id) {
                environment.roots = roots;
            }
            return Ok(Some(environment));
        }
        let roots = match self.session_workspace_roots_sync(&session_id) {
            Some(roots) => roots,
            None => {
                let Some(root) = self.workspace_roots.read().await.get(&session_id).cloned() else {
                    return Ok(None);
                };
                WorkspaceRoots::new(root, Vec::new())
                    .map_err(|error| format!("{}: {}", error.code(), error.detail()))?
            }
        };
        let snapshot = EnvironmentSnapshot::capture(
            &crate::environment::EnvironmentDeclaration::default(),
            None,
            Some(roots.work_dir()),
            std::collections::BTreeMap::new(),
            &roots,
            false,
        )
        .map_err(|error| format!("{}: {}", error.code(), error.detail()))?;
        Ok(Some(SessionEnvironment { snapshot, roots }))
    }

    /// Rebase a request on the caller's own environment, so a nested spawn
    /// narrows from where the caller runs rather than from the session root.
    async fn caller_relative_request(
        &self,
        caller_id: &str,
        request: AgentEnvironmentRequest,
    ) -> AgentEnvironmentRequest {
        let Some(caller) = self.agent_environment_sync(caller_id) else {
            return request;
        };
        let cwd = match request.cwd {
            Some(cwd) if cwd.is_absolute() => cwd,
            Some(cwd) => caller.cwd().join(cwd),
            None => caller.cwd().to_path_buf(),
        };
        let mut env = caller.env().clone();
        env.extend(request.env);
        AgentEnvironmentRequest {
            cwd: Some(cwd),
            env,
        }
    }

    /// The delegation depth of `agent_id` (v2 `subagentDepthOf`): 0 for the
    /// main agent and for any id this engine never spawned, else the hop count
    /// recorded when it was spawned.
    pub async fn depth_of(&self, agent_id: &str) -> u32 {
        if let Some((instance, _)) = self.instances.read().await.get(agent_id) {
            return instance.depth;
        }
        self.persisted_depth(agent_id).await
    }

    /// Depth from the cold-resume record, for an agent whose resident scope
    /// the completed-scope LRU already evicted.
    async fn persisted_depth(&self, agent_id: &str) -> u32 {
        let store = self.session_store.read().await.as_ref().cloned();
        let Some(store) = store else {
            return 0;
        };
        store
            .get_state("subagent_resume", agent_id)
            .ok()
            .flatten()
            .and_then(|value| serde_json::from_value::<SubagentPersistedState>(value).ok())
            .map_or(0, |state| state.depth)
    }

    /// v2 `assertSubagentDepthAllowed`: the depth a child of `caller_id` gets,
    /// or the refusal when the caller already sits at the ceiling.
    async fn child_depth_for_spawn(&self, caller_id: &str) -> Result<u32, String> {
        let caller_depth = self.depth_of(caller_id).await;
        if caller_depth >= MAX_SUBAGENT_DEPTH {
            return Err(LocalizedText::with_params(
                "engine.subagent.depthExceeded",
                i18n_params!["maxDepth" => MAX_SUBAGENT_DEPTH],
            )
            .render());
        }
        Ok(caller_depth + 1)
    }

    /// Record who spawned `id` (v2 `subagents.spawn({ callerAgentId })`).
    ///
    /// The spawning agent is read from the `CALLER_AGENT_ID` scope the caller
    /// already runs in, so every spawn site is covered by construction rather
    /// than by remembering to pass the parent at each one. A spawn outside any
    /// agent scope (a test, a bootstrap) records nothing, and the resume gate
    /// then treats the child as unowned.
    fn record_parent(&self, id: &str) {
        let Ok(agent_id) = crate::tools::CALLER_AGENT_ID.try_with(|id| id.clone()) else {
            return;
        };
        if let Ok(mut parents) = self.parents.try_write() {
            parents.insert(
                id.to_string(),
                SubagentParent {
                    agent_id,
                    parent_tool_call_id: None,
                    swarm_item: None,
                    swarm_index: None,
                },
            );
        }
    }

    /// Who spawned `agent_id`, if the engine knows (v2 `isSubagentMeta` +
    /// `subagentParentAgentId`).
    ///
    /// `None` means "not a subagent this engine spawned" — the main agent and
    /// unknown ids both land here, and both are refused by a resume.
    pub async fn parent_of(&self, agent_id: &str) -> Option<SubagentParent> {
        if let Some(parent) = self.parents.read().await.get(agent_id) {
            return Some(parent.clone());
        }
        // Cold recovery: the resident record went with the scope, but the
        // persisted one still names the parent (#3478).
        let store = self.session_store.read().await.as_ref()?.clone();
        let stored = store.get_state("subagent_resume", agent_id).ok()??;
        let state: SubagentPersistedState = serde_json::from_value(stored).ok()?;
        let parent = SubagentParent {
            agent_id: state.parent_agent_id?,
            parent_tool_call_id: state.parent_tool_call_id,
            swarm_item: state.swarm_item,
            swarm_index: state.swarm_index,
        };
        self.parents
            .write()
            .await
            .insert(agent_id.to_string(), parent.clone());
        Some(parent)
    }

    /// Attach the swarm `{{item}}` a member was spawned for (v2
    /// `subagentLabels(callerAgentId, { swarmItem })`), so a later
    /// `resume_agent_ids` call can label it again.
    pub async fn set_swarm_item(&self, agent_id: &str, item: Option<String>) {
        {
            let mut parents = self.parents.write().await;
            if let Some(parent) = parents.get_mut(agent_id) {
                parent.swarm_item = item;
            }
        }
        // v2 `subagentLabels` writes the item into the agent's roster labels;
        // refresh them or a resume/restore loses which batch item spawned it.
        self.register_agent_metadata(agent_id).await;
    }

    /// Record the agent this one was forked from (v2 `AgentMeta.forkedFrom`,
    /// `agentLifecycleService.ts:464`). The roster entry is rewritten so the
    /// lineage survives with the rest of the metadata.
    pub async fn set_forked_from(&self, agent_id: &str, source: Option<String>) {
        match source {
            Some(source) => {
                self.forked_from
                    .write()
                    .await
                    .insert(agent_id.to_string(), source);
            }
            None => {
                self.forked_from.write().await.remove(agent_id);
            }
        }
        self.register_agent_metadata(agent_id).await;
    }

    /// Record the last turn-sourced request of `agent_id` (v2
    /// `llmRequesterLastTurnRequest`). The main agent publishes per step
    /// through its toolset; a subagent publishes its last request when the
    /// turn returns, which is also the baseline a nested fork inherits.
    pub fn record_probe_request(
        &self,
        agent_id: &str,
        request: crate::session::cache_observation::CacheProbeRequest,
    ) {
        self.probe_requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(agent_id.to_string(), request);
    }

    /// The last recorded request for an agent, if any.
    fn probe_request(
        &self,
        agent_id: &str,
    ) -> Option<crate::session::cache_observation::CacheProbeRequest> {
        self.probe_requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(agent_id)
            .cloned()
    }

    /// Bump an agent's context generation (v2 `prefixRevision`). Compaction
    /// replacement calls this so a fork captured before the replacement stops
    /// comparing against the old parent request.
    pub fn bump_probe_generation(&self, agent_id: &str) {
        let mut generations = self
            .probe_generations
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *generations.entry(agent_id.to_string()).or_insert(0) += 1;
    }

    fn probe_generation(&self, agent_id: &str) -> u64 {
        self.probe_generations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(agent_id)
            .copied()
            .unwrap_or(0)
    }

    /// Capture the parent's current request when a fork scope is created (v2
    /// `AgentCacheProbeService`'s constructor). The snapshot may be absent when
    /// the parent has not completed a request yet; the probe still fires on the
    /// child's first request, with every comparison undefined.
    pub fn arm_fork_probe(&self, child_id: &str, parent_id: &str) {
        let pending = PendingForkProbe {
            parent_id: parent_id.to_string(),
            parent: self.probe_request(parent_id),
            generation: self.probe_generation(parent_id),
        };
        self.pending_fork_probes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(child_id.to_string(), pending);
    }

    /// Fold one completed subagent turn's cache data: publish its new baseline
    /// and, for a fork's first turn, emit the `prompt_cache_probe` telemetry
    /// once. A non-fork agent (or a later turn) produces no probe.
    fn record_turn_cache_probes(
        &self,
        agent_id: &str,
        turn_label: Option<&str>,
        turn: &crate::turn_loop::types::TurnResult,
        callbacks: &dyn crate::callbacks::HostCallbacks,
    ) {
        if let Some(request) = turn.cache_probe_requests.last() {
            self.record_probe_request(agent_id, request.clone());
        }
        if turn.compaction_usage.is_some() {
            self.bump_probe_generation(agent_id);
        }
        let Some(pending) = self
            .pending_fork_probes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(agent_id)
        else {
            return;
        };
        // v2 re-checks the parent's `prefixRevision` at probe time; a
        // compaction under the fork invalidates the captured request.
        let parent = if self.probe_generation(&pending.parent_id) == pending.generation {
            pending.parent.as_ref()
        } else {
            None
        };
        let (Some(observation), Some(own)) = (
            turn.cache_observations.first(),
            turn.cache_probe_requests.first(),
        ) else {
            return;
        };
        let usage = observation.usage();
        let probe = crate::session::cache_observation::build_cache_probe(
            crate::session::cache_observation::CacheProbeSource::Fork,
            turn_label,
            observation.provider_type.as_deref(),
            observation.protocol.as_deref(),
            Some(&usage),
            own,
            parent,
            None,
        );
        callbacks.telemetry(probe.telemetry_payload());
    }

    /// Record a subagent's parent explicitly (v2
    /// `subagents.spawn({ callerAgentId })`).
    ///
    /// [`Self::spawn_with_id`] already records the `CALLER_AGENT_ID` scope, but
    /// that scope does not cross the scheduler's `tokio::spawn` — a caller that
    /// resolves its member asynchronously has to name the parent itself. Overwrites
    /// whatever the scope recorded.
    ///
    /// `swarm_index` is what separates a swarm member from a plain child
    /// everywhere downstream: the journal records it, the session snapshot
    /// reports it, and the client keys the member's card off it. `None` means
    /// an ordinary subagent.
    pub async fn set_parent(
        &self,
        agent_id: &str,
        parent_agent_id: String,
        parent_tool_call_id: Option<String>,
        swarm_item: Option<String>,
        swarm_index: Option<u32>,
    ) {
        {
            let mut parents = self.parents.write().await;
            parents.insert(
                agent_id.to_string(),
                SubagentParent {
                    agent_id: parent_agent_id,
                    parent_tool_call_id,
                    swarm_item,
                    swarm_index,
                },
            );
        }
        self.register_agent_metadata(agent_id).await;
    }

    /// Spawn a new subagent and launch an autonomous background execution loop.
    pub async fn spawn_and_run(
        self: &Arc<Self>,
        type_name: &str,
        role: &str,
        prompt: &str,
        llm: Arc<dyn crate::turn_loop::types::LLM>,
        callbacks: Arc<dyn crate::callbacks::HostCallbacks>,
    ) -> Result<String, String> {
        let id = self.spawn(type_name, role).await?;
        let def = self
            .get_definition(type_name)
            .await
            .unwrap_or_else(|| SubagentDefinition {
                name: type_name.to_string(),
                description: format!("Dynamic subagent for {role}"),
                system_prompt: format!("You are {role}. Complete the user's task accurately."),
                tools: vec![
                    "read".into(),
                    "grep".into(),
                    "glob".into(),
                    "fetch_url".into(),
                    "web_search".into(),
                    "list_directory".into(),
                ],
                disallowed_tools: Vec::new(),
                when_to_use: None,
                prompt_prefix: None,
                summary_policy: None,
                model: None,
                subagents: None,
            });

        let mgr = self.clone();
        let subagent_id = id.clone();
        let prompt_str = prompt.to_string();
        let subagent_role = role.to_string();

        let cancel_flag = {
            let instances = mgr.instances.read().await;
            instances.get(&subagent_id).map(|(_, c)| c.clone())
        };

        let runner = { self.task_runner.read().await.clone() };
        let parent_session = self
            .runtime
            .read()
            .await
            .as_ref()
            .and_then(|r| r.session_id.clone().filter(|session| !session.is_empty()));

        let subagent_run = async move {
            let origin = crate::subagent::types::agent_run_prompt_origin();
            let turn_seq = fastrand::u64(..);
            let hook_guard = callbacks.hook_guard();
            if let Some(guard) = hook_guard.as_ref() {
                guard
                    .notify_turn_started(turn_seq, &origin, &prompt_str)
                    .await;
            }
            let end_guard = hook_guard.clone();
            // Same seam as the foreground path: the turn's system message is
            // the profile's, and the user message is the task prompt alone
            // (v2 `runAgentTurn` submits `{ role: 'user', content: [{ text: prompt }] }`).
            let llm =
                crate::subagent::profile_prompt::ProfilePromptLlm::wrap(llm, &def.system_prompt);
            let messages = vec![crate::turn_loop::types::LLMMessage {
                role: "user".into(),
                content: prompt_str,
                blocks: Vec::new(),
                tool_calls: Vec::new(),
                tool_call_id: None,

                prompt_id: None,
                origin: Some(origin),
            }];

            let run_input = crate::turn_loop::types::RunTurnInput {
                agent_id: subagent_id.clone(),
                max_attempts: None,
                turn_id: format!("subturn-{turn_seq}"),
                llm: llm.as_ref(),
                messages,
                tools: &[],
                tool_defs: Vec::new(),
                max_steps: 15,
                max_context_tokens: None,
                compaction_max_attempts: None,
                permission_mode: None,
                goal: None,
                cancellation: cancel_flag,
                hook_guard,
                media: None,
                media_dropped: None,
                media_recovery: None,
                toolset: None,
                previous_turn_aborted: false,
            };

            let run_result = crate::tools::CALLER_AGENT_ID
                .scope(
                    subagent_id.clone(),
                    crate::turn_loop::run_turn::run_turn_continued(run_input, &callbacks),
                )
                .await
                .map_err(|e| e.to_string());

            if let Some(guard) = end_guard.as_ref() {
                let end = match &run_result {
                    Ok(turn) => crate::session::AgentTurnEnd::Ran(turn),
                    Err(message) => crate::session::AgentTurnEnd::Failed(message),
                };
                crate::session::notify_agent_turn_end(guard, turn_seq, end).await;
            }

            match run_result {
                Ok(turn_res) => {
                    let label = format!("subturn-{turn_seq}");
                    mgr.record_turn_cache_probes(
                        &subagent_id,
                        Some(label.as_str()),
                        &turn_res,
                        callbacks.as_ref(),
                    );
                    let result_text = format!(
                        "Subagent '{}' finished in {} steps (Tokens: {}).",
                        subagent_role, turn_res.steps, turn_res.usage.total_tokens
                    );
                    // Upstream #3847: fold the turn into the instance's
                    // cumulative counter (the tower tools read it as `tokens`).
                    mgr.record_instance_usage(&subagent_id, &turn_res.usage);
                    mgr.update_state(
                        &subagent_id,
                        SubagentState::Completed,
                        Some(result_text.clone()),
                    )
                    .await;
                    crate::storage::TaskOutcome::Completed(result_text)
                }
                Err(err_msg) => {
                    let err_text = format!("Error: {err_msg}");
                    mgr.update_state(&subagent_id, SubagentState::Failed, Some(err_text.clone()))
                        .await;
                    // The task registry has to be told too: settling this as a
                    // success is what made a failed subagent read as completed
                    // to the parent session and to whoever listed the tasks.
                    crate::storage::TaskOutcome::Failed(err_text)
                }
            }
        };

        if let Some(task_runner) = runner {
            let description = format!("Subagent {}: {}", role, prompt);
            let _ = task_runner.spawn_task_with_meta(
                crate::storage::TaskSpawnMeta {
                    session_id: parent_session.as_deref(),
                    kind: "subagent",
                    subagent_type: Some(type_name),
                    agent_id: Some(id.as_str()),
                    timeout_ms: None,
                },
                id.clone(),
                description,
                subagent_run,
            );
        } else {
            tokio::spawn(subagent_run);
        }

        Ok(id)
    }

    /// Run one foreground subagent turn inline (v2 `Agent` tool foreground
    /// semantics): the caller awaits the full turn and formats the result.
    /// The instance must already exist (see [`Self::spawn`]). Cancellation
    /// is event-driven (P51) — see [`run_one`]; a parent cancel that lands
    /// between awaits degrades to the engine's step-boundary abort.
    pub async fn run_foreground_turn(
        &self,
        id: &str,
        prompt: &str,
        parent_cancel: Option<&crate::subagent::types::ParentCancel>,
    ) -> Result<ForegroundTurnOutcome, String> {
        self.run_foreground_turn_with_history(id, prompt, None, parent_cancel)
            .await
    }

    /// Claim the single in-flight foreground turn for `id`. Held until the
    /// claim is dropped at the end of the turn, so concurrent `resume` calls
    /// fail fast with the v2 `already_running` semantics instead of running two
    /// turns against one conversation.
    fn claim_foreground_turn(&self, id: &str) -> Result<ForegroundTurnClaim, String> {
        let mut runs = self
            .foreground_runs
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !runs.insert(id.to_string()) {
            return Err(format!(
                "Agent instance \"{id}\" is already running and cannot be resumed concurrently — wait for the running turn to finish"
            ));
        }
        drop(runs);
        if let Some(session) = self
            .current_session
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
        {
            self.active_turn_sessions
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .insert(id.to_string(), session);
        }
        Ok(ForegroundTurnClaim {
            runs: self.foreground_runs.clone(),
            sessions: self.active_turn_sessions.clone(),
            id: id.to_string(),
        })
    }

    /// Run one foreground subagent turn with an optional inherited conversation
    /// history (v2 `fork: true` semantics). If `inherited_history` is present,
    /// any unclosed trailing tool calls are reconciled via `close_trailing_open_tool_exchange`
    /// before prepending to the new task prompt.
    pub async fn run_foreground_turn_with_history(
        &self,
        id: &str,
        prompt: &str,
        inherited_history: Option<Vec<crate::turn_loop::types::LLMMessage>>,
        parent_cancel: Option<&crate::subagent::types::ParentCancel>,
    ) -> Result<ForegroundTurnOutcome, String> {
        let _claim = self.claim_foreground_turn(id)?;
        let (cancel_flag, type_name, role) = {
            let instances = self.instances.read().await;
            let (instance, flag) = instances
                .get(id)
                .ok_or_else(|| format!("Unknown subagent instance: '{id}'"))?;
            (
                flag.clone(),
                instance.type_name.clone(),
                instance.role.clone(),
            )
        };
        if parent_cancel.is_some_and(|signal| signal.triggered()) {
            let _ = self.kill_now(id).await;
            return Ok(ForegroundTurnOutcome::ParentCancelled);
        }
        let runtime = self
            .runtime()
            .await
            .ok_or_else(|| "no subagent runtime injected".to_string())?;
        let def = self.definition_for(&type_name, &role).await;
        // v2 `AgentProfile.promptPrefix`: an explicit wire prefix wins;
        // otherwise the builtin `explore` profile collects the repository
        // state at spawn time.
        let prompt_prefix = def
            .prompt_prefix
            .clone()
            .filter(|prefix| !prefix.trim().is_empty())
            .or(self.builtin_explore_prompt_prefix(id, &def).await);
        // A `[secondary_model]` pool binding overrides the session model for
        // this instance only; the callbacks chain is shared either way.
        let bound_llm = self.instance_llm(id).await;
        let bound = bound_llm.is_some();
        // v2 `agentLifecycle.create({ binding: { profile } })`: a spawned
        // subagent's requests carry its profile's `systemPrompt`. A fork
        // inherits the caller's whole scope — system prompt included — so it
        // keeps the session transport's prompt, which is the caller's own.
        let llm = bound_llm.unwrap_or_else(|| runtime.llm.clone());
        let llm = if inherited_history.is_none() {
            crate::subagent::profile_prompt::ProfilePromptLlm::wrap(llm, &def.system_prompt)
        } else {
            llm
        };
        let runtime = if bound || inherited_history.is_none() {
            Arc::new(SubagentRuntime {
                llm,
                callbacks: runtime.callbacks.clone(),
                session_id: runtime.session_id.clone(),
                media: crate::llm::media_resolver::MediaResolver::new(),
                media_dropped: runtime.media_dropped.clone(),
                // v2 #4413 keys the recovery by model: an instance on its own
                // pool model gets its own cell — sharing the session's would
                // let one model's capture clobber the other's. A prompt-only
                // override stays on the same model, so it keeps the shared
                // cell.
                media_recovery: if bound {
                    Default::default()
                } else {
                    runtime.media_recovery.clone()
                },
            })
        } else {
            runtime
        };

        // The subagent sees the host table narrowed by its profile policy
        // (v2 `resolveActiveToolNames` subset): an explicit allowlist wins,
        // otherwise every tool except the disallowed names. Effective on
        // native transports; host-proxy rebuilds the table host-side.
        let filter = ToolPolicyFilter::from_definition(&def);
        let tool_defs = subagent_tool_defs(&runtime.callbacks, &filter).await?;
        let callbacks: Arc<dyn crate::callbacks::HostCallbacks> = Arc::new(ToolFilterCallbacks {
            inner: runtime.callbacks.clone(),
            filter,
            agent_id: id.to_string(),
        });

        // v2 `applyProfilePromptPrefix`: the prefix rides ahead of the
        // prompt as `{prefix}\n\n{prompt}`.
        let prompt = match prompt_prefix.as_deref() {
            Some(prefix) => format!("{prefix}\n\n{prompt}"),
            None => prompt.to_string(),
        };
        let origin = crate::subagent::types::agent_run_prompt_origin();

        let messages = if let Some(history) = inherited_history {
            let mut msgs = crate::subagent::fork::close_trailing_open_tool_exchange(&history);
            // The fork context notice rides as a `<system-reminder>` injection
            // ahead of the task prompt (v2 #3412), not concatenated into the
            // prompt text.
            msgs.push(crate::injection::injection_message(
                crate::injection::wrap_system_reminder(crate::subagent::fork::FORK_CONTEXT_NOTICE),
            ));
            msgs.push(crate::turn_loop::types::LLMMessage {
                role: "user".into(),
                content: prompt.clone(),
                blocks: Vec::new(),
                tool_calls: Vec::new(),
                tool_call_id: None,

                prompt_id: None,
                origin: Some(origin),
            });
            msgs
        } else {
            vec![crate::turn_loop::types::LLMMessage {
                role: "user".into(),
                content: prompt,
                blocks: Vec::new(),
                tool_calls: Vec::new(),
                tool_call_id: None,

                prompt_id: None,
                origin: Some(origin),
            }]
        };

        // Attribute this turn's native tool calls to the subagent, not to the
        // main agent: the tower tools read CALLER_AGENT_ID to enforce the
        // main-agent-only gate and to resolve the caller's roster name.
        let outcome: Result<crate::turn_loop::types::TurnResult, RunExit> =
            crate::tools::CALLER_AGENT_ID
                .scope(id.to_string(), async {
                    let turn_res = run_one(
                        &runtime,
                        id,
                        &callbacks,
                        messages.clone(),
                        tool_defs.clone(),
                        &cancel_flag,
                        parent_cancel,
                    )
                    .await?;
                    // Record before the max-token classification: the fork's
                    // probe describes the first request, which completed
                    // regardless of how the turn is later classified.
                    self.record_turn_cache_probes(
                        id,
                        Some("subturn-1"),
                        &turn_res,
                        callbacks.as_ref(),
                    );
                    // v2 `classifyTurnResult`: a max-token-truncated turn fails the
                    // run before any distillation attempt.
                    if matches!(
                        turn_res.stop_reason,
                        crate::turn_loop::types::LoopTurnStopReason::MaxTokens
                    ) {
                        return Err(RunExit::Failed(SUBAGENT_MAX_TOKENS_ERROR.into()));
                    }
                    match &def.summary_policy {
                        Some(policy) => {
                            distill_continuations(
                                &runtime,
                                id,
                                &callbacks,
                                tool_defs.clone(),
                                &cancel_flag,
                                parent_cancel,
                                turn_res,
                                policy,
                            )
                            .await
                        }
                        None => Ok(turn_res),
                    }
                })
                .await;

        match outcome {
            Ok(turn_res) => {
                let summary = final_assistant_summary(&turn_res.messages);
                // P55: keep the conversation for native `resume` calls.
                self.foreground_histories
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(
                        id.to_string(),
                        ForegroundResume {
                            profile_name: type_name.clone(),
                            role: role.clone(),
                            messages: turn_res.messages.clone(),
                        },
                    );
                // Upstream #3847: fold the turn into the instance's
                // cumulative counter (the tower tools read it as `tokens`).
                self.record_instance_usage(id, &turn_res.usage);
                // Persist to sqlite store if available (#3478)
                self.persist_resume_state(id, &type_name, &role, turn_res.messages.clone());
                // v2 `awaitRun`: no assistant text means the agent never wrote
                // its handoff — a failure, not an empty completion. The
                // conversation is persisted above, so a resume still picks it
                // up.
                if let Some(message) = no_final_message_error(&turn_res) {
                    self.update_state(id, SubagentState::Failed, Some(format!("Error: {message}")))
                        .await;
                    return Err(message);
                }
                // Completed last: the scope may be evicted right here, and
                // only a durable resume record makes that safe.
                self.update_state(id, SubagentState::Completed, Some(summary))
                    .await;
                Ok(ForegroundTurnOutcome::Completed(turn_res))
            }
            Err(RunExit::ParentCancelled) => {
                let _ = self.kill_now(id).await;
                Ok(ForegroundTurnOutcome::ParentCancelled)
            }
            Err(RunExit::Failed(message)) => {
                self.update_state(id, SubagentState::Failed, Some(format!("Error: {message}")))
                    .await;
                Err(message)
            }
        }
    }

    /// Native `resume` (P55): continue a previously foreground-completed
    /// subagent conversation. The resume appends the prompt to the stored
    /// history and runs one more turn under the same profile policy. Unknown
    /// ids answer `None` — the caller falls back to the host, which owns the
    /// v2 persistent-scope resume semantics.
    pub async fn resume_foreground_turn(
        &self,
        id: &str,
        prompt: &str,
        parent_cancel: Option<&crate::subagent::types::ParentCancel>,
    ) -> Option<Result<ForegroundTurnOutcome, String>> {
        let _claim = match self.claim_foreground_turn(id) {
            Ok(claim) => claim,
            Err(error) => return Some(Err(error)),
        };
        let fg_record = {
            let histories = self
                .foreground_histories
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            histories.get(id).cloned()
        };
        let record = match fg_record {
            Some(rec) => rec,
            None => {
                if let Some(p) = self.persistent.read().await.get(id) {
                    let instances = self.instances.read().await;
                    let (profile_name, role) = instances
                        .get(id)
                        .map(|(inst, _)| (inst.type_name.clone(), inst.role.clone()))
                        .unwrap_or_else(|| ("coder".to_string(), "coder".to_string()));
                    ForegroundResume {
                        profile_name,
                        role,
                        messages: p.messages.clone(),
                    }
                } else {
                    let guard = self.session_store.read().await;
                    let store = guard.as_ref()?;
                    // Cold recovery: restore from SQLite store (#3478)
                    if let Ok(Some(val)) = store.get_state("subagent_resume", id) {
                        if let Ok(state) = serde_json::from_value::<SubagentPersistedState>(val) {
                            let rec = ForegroundResume {
                                profile_name: state.profile_name,
                                role: state.role,
                                messages: state.messages,
                            };
                            self.foreground_histories
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .insert(id.to_string(), rec.clone());
                            rec
                        } else {
                            return None;
                        }
                    } else {
                        return None;
                    }
                }
            }
        };
        // v2 `rebuildSubagent`: an evicted scope is recreated from its
        // persisted record before the resumed turn runs. A scope that is
        // closing never gives the id back in time — report the same
        // `AGENT_ALREADY_EXISTS` the create would have raised.
        if let Err(error) = self
            .rebuild_instance(id, &record.profile_name, &record.role)
            .await
        {
            return Some(Err(error));
        }
        let runtime = self.runtime().await?;
        let def = self
            .definition_for(&record.profile_name, &record.role)
            .await;
        // A resumed agent keeps the profile's system prompt (v2 rebuilds the
        // scope from the same profile); a blank profile prompt — the btw side
        // channel — leaves the session transport's prompt in place.
        let llm = crate::subagent::profile_prompt::ProfilePromptLlm::wrap(
            runtime.llm.clone(),
            &def.system_prompt,
        );
        let runtime = if Arc::ptr_eq(&llm, &runtime.llm) {
            runtime
        } else {
            Arc::new(SubagentRuntime {
                llm,
                callbacks: runtime.callbacks.clone(),
                session_id: runtime.session_id.clone(),
                media: crate::llm::media_resolver::MediaResolver::new(),
                media_dropped: runtime.media_dropped.clone(),
                media_recovery: Default::default(),
            })
        };
        let filter = ToolPolicyFilter::from_definition(&def);
        // A tool-table failure is not "no persisted record": returning `None`
        // here would send the caller down the plain-foreground fallback and
        // run the turn with an unfiltered table. Fail the resume instead.
        let Ok(tool_defs) = subagent_tool_defs(&runtime.callbacks, &filter).await else {
            let message = "Could not read the tool table for this subagent.".to_string();
            self.update_state(id, SubagentState::Failed, Some(message.clone()))
                .await;
            return Some(Err(message));
        };
        let callbacks: Arc<dyn crate::callbacks::HostCallbacks> = Arc::new(ToolFilterCallbacks {
            inner: runtime.callbacks.clone(),
            filter,
            agent_id: id.to_string(),
        });

        let messages = {
            let mut history = record.messages.clone();
            history.push(crate::turn_loop::types::LLMMessage {
                role: "user".into(),
                content: prompt.to_string(),
                origin: Some(crate::subagent::types::agent_run_prompt_origin()),
                ..Default::default()
            });
            history
        };
        // The instance's *own* cancel flag, not a fresh one. `kill` (and so
        // `TaskStop` / `ManageSubagents`) flips this flag; a locally
        // constructed one would never be set, leaving a resumed agent running
        // with no way to stop it. The foreground path has always read the real
        // flag — this is the same lookup, so both paths honour a kill.
        let cancel_flag = {
            let instances = self.instances.read().await;
            match instances.get(id) {
                Some((_, flag)) => flag.clone(),
                // No instance: the caller falls back to a plain foreground
                // turn, which re-checks below and reports the unknown id.
                None => Arc::new(AtomicBool::new(false)),
            }
        };
        if cancel_flag.load(std::sync::atomic::Ordering::SeqCst) {
            return Some(Ok(ForegroundTurnOutcome::ParentCancelled));
        }
        let turn_res = match crate::tools::CALLER_AGENT_ID
            .scope(
                id.to_string(),
                run_one(
                    &runtime,
                    id,
                    &callbacks,
                    messages.clone(),
                    tool_defs.clone(),
                    &cancel_flag,
                    parent_cancel,
                ),
            )
            .await
        {
            Ok(turn) => turn,
            // The parent-cancelled outcome flows through verbatim so the
            // tool result carries the v2 user-interruption message.
            Err(RunExit::ParentCancelled) => {
                let _ = self.kill_now(id).await;
                return Some(Ok(ForegroundTurnOutcome::ParentCancelled));
            }
            Err(RunExit::Failed(message)) => {
                self.update_state(id, SubagentState::Failed, Some(format!("Error: {message}")))
                    .await;
                return Some(Err(message));
            }
        };
        self.record_turn_cache_probes(id, None, &turn_res, callbacks.as_ref());
        if matches!(
            turn_res.stop_reason,
            crate::turn_loop::types::LoopTurnStopReason::MaxTokens
        ) {
            self.update_state(
                id,
                SubagentState::Failed,
                Some(format!("Error: {SUBAGENT_MAX_TOKENS_ERROR}")),
            )
            .await;
            return Some(Err(SUBAGENT_MAX_TOKENS_ERROR.to_string()));
        }
        // P60: resume turns distill under the same policy as the initial
        // run (v2 runs every subagent turn through `distillSummary`).
        let turn_res = match &def.summary_policy {
            Some(policy) => {
                let distilled = distill_continuations(
                    &runtime,
                    id,
                    &callbacks,
                    tool_defs,
                    &Arc::new(AtomicBool::new(false)),
                    parent_cancel,
                    turn_res,
                    policy,
                )
                .await;
                match distilled {
                    Ok(turn) => turn,
                    Err(RunExit::ParentCancelled) => {
                        let _ = self.kill_now(id).await;
                        return Some(Ok(ForegroundTurnOutcome::ParentCancelled));
                    }
                    Err(RunExit::Failed(message)) => {
                        self.update_state(
                            id,
                            SubagentState::Failed,
                            Some(format!("Error: {message}")),
                        )
                        .await;
                        return Some(Err(message));
                    }
                }
            }
            None => turn_res,
        };

        let summary = final_assistant_summary(&turn_res.messages);
        self.foreground_histories
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                id.to_string(),
                ForegroundResume {
                    profile_name: record.profile_name.clone(),
                    role: record.role.clone(),
                    messages: turn_res.messages.clone(),
                },
            );
        self.persist_resume_state(
            id,
            &record.profile_name,
            &record.role,
            turn_res.messages.clone(),
        );
        {
            let mut persistent = self.persistent.write().await;
            if let Some(p) = persistent.get_mut(id) {
                p.messages = turn_res.messages.clone();
                p.usage = turn_res.usage.clone();
            }
        }
        // v2 `awaitRun`: a resumed turn that ends without assistant text fails
        // like any other (the conversation is already persisted above).
        if let Some(message) = no_final_message_error(&turn_res) {
            self.update_state(id, SubagentState::Failed, Some(format!("Error: {message}")))
                .await;
            return Some(Err(message));
        }
        // Completed last: the scope may be evicted right here, and only a
        // durable resume record makes that safe.
        self.update_state(id, SubagentState::Completed, Some(summary))
            .await;
        Some(Ok(ForegroundTurnOutcome::Completed(turn_res)))
    }

    /// The profile a native resume record was spawned under (for the v2
    /// `actual_subagent_type` line in the resume result).
    pub async fn resume_profile(&self, id: &str) -> Option<String> {
        if let Some(name) = self
            .foreground_histories
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .map(|record| record.profile_name.clone())
        {
            return Some(name);
        }
        let instances = self.instances.read().await;
        if let Some((inst, _)) = instances.get(id) {
            return Some(inst.type_name.clone());
        }
        // Cold recovery check (#3478)
        if let Some(store) = self.session_store.read().await.as_ref()
            && let Ok(Some(val)) = store.get_state("subagent_resume", id)
            && let Ok(state) = serde_json::from_value::<SubagentPersistedState>(val)
        {
            let profile_name = state.profile_name.clone();
            self.foreground_histories
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(
                    id.to_string(),
                    ForegroundResume {
                        profile_name: state.profile_name,
                        role: state.role,
                        messages: state.messages,
                    },
                );
            return Some(profile_name);
        }
        None
    }

    /// Retrieve stored message history of a completed foreground subagent (for forking or resuming).
    pub fn get_foreground_history(
        &self,
        id: &str,
    ) -> Option<Vec<crate::turn_loop::types::LLMMessage>> {
        if let Some(msgs) = self
            .foreground_histories
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .map(|record| record.messages.clone())
        {
            return Some(msgs);
        }
        // Cold recovery check (#3478)
        if let Ok(guard) = self.session_store.try_read()
            && let Some(store) = guard.as_ref()
            && let Ok(Some(val)) = store.get_state("subagent_resume", id)
            && let Ok(state) = serde_json::from_value::<SubagentPersistedState>(val)
        {
            return Some(state.messages);
        }
        None
    }

    /// Explicitly record or seed the message history for a foreground subagent.
    pub fn set_foreground_history(
        &self,
        id: &str,
        profile_name: &str,
        role: &str,
        messages: Vec<crate::turn_loop::types::LLMMessage>,
    ) {
        self.foreground_histories
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                id.to_string(),
                ForegroundResume {
                    profile_name: profile_name.to_string(),
                    role: role.to_string(),
                    messages: messages.clone(),
                },
            );
        // Persist to store if available (#3478)
        self.persist_resume_state(id, profile_name, role, messages);
    }

    /// Write a subagent's resume record, stamped with its parent (v2
    /// `subagentLabels`).
    ///
    /// The single place a `subagent_resume` record is written, so the parent
    /// cannot be forgotten at one of the call sites. Sync + `try_read` because
    /// [`Self::set_foreground_history`] is sync; a contended lock skips the
    /// write rather than blocking the turn.
    fn persist_resume_state(
        &self,
        id: &str,
        profile_name: &str,
        role: &str,
        messages: Vec<crate::turn_loop::types::LLMMessage>,
    ) {
        let Ok(guard) = self.session_store.try_read() else {
            return;
        };
        let Some(store) = guard.as_ref() else {
            return;
        };
        let parent = self
            .parents
            .try_read()
            .ok()
            .and_then(|parents| parents.get(id).cloned());
        let depth = self
            .instances
            .try_read()
            .ok()
            .and_then(|instances| instances.get(id).map(|(instance, _)| instance.depth))
            .unwrap_or(0);
        let state = SubagentPersistedState {
            id: id.to_string(),
            profile_name: profile_name.to_string(),
            role: role.to_string(),
            messages,
            updated_at: chrono::Utc::now().timestamp_millis(),
            parent_agent_id: parent.as_ref().map(|p| p.agent_id.clone()),
            parent_tool_call_id: parent.as_ref().and_then(|p| p.parent_tool_call_id.clone()),
            forked_from: self
                .forked_from
                .try_read()
                .ok()
                .and_then(|sources| sources.get(id).cloned()),
            swarm_item: parent.as_ref().and_then(|p| p.swarm_item.clone()),
            swarm_index: parent.and_then(|p| p.swarm_index),
            depth,
        };
        if let Ok(val) = serde_json::to_value(&state) {
            let _ = store.put_state("subagent_resume", id, &val);
        }
    }

    /// Spawn a persistent subagent instance that keeps its message history
    /// across turns. The instance is registered in the instance map with its
    /// own cancellation flag, so the existing `kill` mechanism aborts any
    /// running turn and blocks further ones.
    pub async fn spawn_persistent(
        &self,
        type_name: &str,
        role: &str,
        llm: Arc<dyn crate::turn_loop::types::LLM>,
        callbacks: Arc<dyn crate::callbacks::HostCallbacks>,
    ) -> Result<String, String> {
        self.scope_cache_error()?;
        let defs = self.definitions.read().await;
        if !defs.contains_key(type_name) && type_name != "self" {
            return Err(format!("Unknown subagent type: '{type_name}'"));
        }
        drop(defs);
        let caller_id = crate::tools::CALLER_AGENT_ID
            .try_with(|id| id.clone())
            .unwrap_or_else(|_| crate::callbacks::MAIN_AGENT_ID.to_string());
        let depth = self.child_depth_for_spawn(&caller_id).await?;

        let def = self.definition_for(type_name, role).await;
        let id = self.allocate_agent_id().await;
        let environment = self
            .resolve_agent_environment(&id, &caller_id, AgentEnvironmentRequest::default())
            .await?;
        let cancellation = Arc::new(AtomicBool::new(false));

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let instance = SubagentInstance {
            id: id.clone(),
            type_name: type_name.to_string(),
            role: role.to_string(),
            state: SubagentState::Running,
            created_at_ms: now_ms,
            last_result: None,
            depth,
        };

        {
            let mut instances = self.instances.write().await;
            instances.insert(id.clone(), (instance, cancellation.clone()));
        }
        if let Some(environment) = environment {
            self.agent_environments
                .write()
                .unwrap_or_else(|error| error.into_inner())
                .insert(id.clone(), environment);
        }
        {
            let mut persistent = self.persistent.write().await;
            persistent.insert(
                id.clone(),
                PersistentInstance {
                    llm,
                    callbacks,
                    system_prompt: def.system_prompt,
                    messages: Vec::new(),
                    usage: crate::rpc::types::TokenUsage::default(),
                    running: false,
                    cancelled: cancellation,
                },
            );
        }
        self.revive_scope(&id);
        // A persistent instance is owned by whoever created it, exactly like a
        // transient one: the `Agent` tool's resume path checks that ownership
        // (v2 `agentTool.ts` `AGENT_NOT_OWNED`), so an unrecorded persistent
        // subagent would read as "not a subagent" and become unresumable.
        self.record_parent(&id);
        self.register_agent_metadata(&id).await;

        Ok(id)
    }

    /// Run one turn on a persistent instance: append the prompt to the
    /// instance's message history and execute a full `run_turn` with that
    /// history. The turn runs with the caller-supplied LLM and callbacks;
    /// the instance's spawn-time context is recorded in
    /// [`PersistentInstance`]. Concurrent turns on the same instance are
    /// rejected, and a `kill` that lands mid-turn aborts the loop.
    pub async fn run_persistent_turn(
        &self,
        id: &str,
        prompt: &str,
        llm: Arc<dyn crate::turn_loop::types::LLM>,
        callbacks: Arc<dyn crate::callbacks::HostCallbacks>,
    ) -> Result<crate::turn_loop::types::TurnResult, String> {
        // Claim the instance before any work starts: unknown ids, concurrent
        // turns, and terminated instances are rejected up front.
        let (cancel_flag, system_prompt, history) = {
            let mut persistent = self.persistent.write().await;
            let entry = persistent
                .get_mut(id)
                .ok_or_else(|| format!("Unknown persistent subagent: '{id}'"))?;
            if entry.running {
                return Err(format!(
                    "Persistent subagent '{id}' is already running a turn"
                ));
            }
            if entry.cancelled.load(Ordering::SeqCst) {
                return Err(format!("Persistent subagent '{id}' has been terminated"));
            }
            entry.running = true;
            (
                entry.cancelled.clone(),
                entry.system_prompt.clone(),
                entry.messages.clone(),
            )
        };
        self.update_state(id, SubagentState::Running, None).await;

        // The first turn carries the definition's system prompt; later turns
        // append the raw prompt so the role text is not repeated.
        let user_content = if history.is_empty() && !system_prompt.is_empty() {
            format!("{system_prompt}\n\n{prompt}")
        } else {
            prompt.to_string()
        };
        let mut messages = history;
        messages.push(crate::turn_loop::types::LLMMessage {
            role: "user".into(),
            content: user_content.clone(),
            origin: Some(crate::subagent::types::agent_run_prompt_origin()),
            ..Default::default()
        });

        // Wrap the LLM so the final assistant response can be appended to
        // the instance history after the turn (TurnResult carries no content).
        let recording = RecordingLlm::new(llm);
        let origin = crate::subagent::types::agent_run_prompt_origin();
        let turn_seq = fastrand::u64(..);
        let hook_guard = callbacks.hook_guard();
        if let Some(guard) = hook_guard.as_ref() {
            guard.notify_turn_started(turn_seq, &origin, prompt).await;
        }
        let end_guard = hook_guard.clone();
        let run_input = crate::turn_loop::types::RunTurnInput {
            agent_id: id.to_string(),
            max_attempts: None,
            turn_id: format!("subturn-{turn_seq}"),
            llm: &recording,
            messages,
            tools: &[],
            tool_defs: Vec::new(),
            max_steps: 15,
            max_context_tokens: None,
            compaction_max_attempts: None,
            permission_mode: None,
            goal: None,
            cancellation: Some(cancel_flag.clone()),
            hook_guard,
            media: None,
            media_dropped: None,
            media_recovery: None,
            toolset: None,
            previous_turn_aborted: false,
        };
        let run_result = crate::tools::CALLER_AGENT_ID
            .scope(
                id.to_string(),
                crate::turn_loop::run_turn::run_turn_continued(run_input, &callbacks),
            )
            .await
            .map_err(|e| e.to_string());

        if let Some(guard) = end_guard.as_ref() {
            let end = match &run_result {
                Ok(turn) => crate::session::AgentTurnEnd::Ran(turn),
                Err(message) => crate::session::AgentTurnEnd::Failed(message),
            };
            crate::session::notify_agent_turn_end(guard, turn_seq, end).await;
        }

        match run_result {
            Ok(turn) => {
                let assistant_content = recording.last_content();
                {
                    let mut persistent = self.persistent.write().await;
                    if let Some(entry) = persistent.get_mut(id) {
                        entry.running = false;
                        entry.usage = add_usage(&entry.usage, &turn.usage);
                        entry.messages.push(crate::turn_loop::types::LLMMessage {
                            role: "user".into(),
                            content: user_content,
                            origin: Some(crate::subagent::types::agent_run_prompt_origin()),
                            ..Default::default()
                        });
                        if let Some(content) = assistant_content {
                            entry.messages.push(crate::turn_loop::types::LLMMessage {
                                role: "assistant".into(),
                                content,
                                ..Default::default()
                            });
                        }
                    }
                }
                // Upstream #3847: mirror into the per-instance counter so
                // `caller_tokens` sees persistent scopes through one path.
                self.record_instance_usage(id, &turn.usage);
                // A kill that landed mid-turn keeps the Terminated state.
                if !cancel_flag.load(Ordering::SeqCst) {
                    let summary = format!(
                        "Turn finished in {} steps (Tokens: {}).",
                        turn.steps, turn.usage.total_tokens
                    );
                    self.update_state(id, SubagentState::Idle, Some(summary))
                        .await;
                }
                Ok(turn)
            }
            Err(err_msg) => {
                {
                    let mut persistent = self.persistent.write().await;
                    if let Some(entry) = persistent.get_mut(id) {
                        entry.running = false;
                    }
                }
                self.update_state(id, SubagentState::Failed, Some(format!("Error: {err_msg}")))
                    .await;
                Err(err_msg)
            }
        }
    }

    /// Cumulative token usage of a persistent instance across all turns.
    /// Unknown ids report zero usage.
    pub async fn get_persistent_usage(&self, id: &str) -> crate::rpc::types::TokenUsage {
        let persistent = self.persistent.read().await;
        persistent
            .get(id)
            .map(|entry| entry.usage.clone())
            .unwrap_or_default()
    }

    /// Fold one finished turn's usage into the instance's cumulative
    /// counter (v2 `ISessionUsageService` per-agent status). Called from
    /// every turn-completion path so a mid-turn tool call (the tower tools'
    /// `tokens` field, upstream #3847) sees the prior turns' total.
    pub fn record_instance_usage(&self, id: &str, usage: &crate::rpc::types::TokenUsage) {
        let mut map = self
            .usage_by_instance
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let entry = map.entry(id.to_string()).or_default();
        entry.accumulate(usage);
    }

    /// The instance's cumulative usage: the per-turn counter first, the
    /// persistent instance's own accumulator as the fallback (it predates
    /// the counter and is authoritative for persistent scopes).
    pub async fn cumulative_usage(&self, id: &str) -> Option<crate::rpc::types::TokenUsage> {
        let counted = self
            .usage_by_instance
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned();
        if counted.is_some() {
            return counted;
        }
        let persistent = self.persistent.read().await;
        persistent.get(id).map(|entry| entry.usage.clone())
    }

    /// The caller's cumulative token total for tower records (v2
    /// `callerTokens`: `grandTotal(usage.status(agent).total)`, `-1` when
    /// unknown — the fork reports `None`). Covers subagent callers, whose
    /// turns fold into the per-instance counter; the main agent's session
    /// total lives in the host's SQLite turn records, which the toolset
    /// cannot reach, so it reports `None` (recorded divergence).
    pub async fn caller_tokens(&self, caller_agent_id: &str) -> Option<i64> {
        let usage = self.cumulative_usage(caller_agent_id).await?;
        Some(
            (u64::from(usage.input_tokens)
                + u64::from(usage.input_cache_read)
                + u64::from(usage.input_cache_creation)
                + u64::from(usage.output_tokens)) as i64,
        )
    }

    /// Terminate and remove a persistent instance: sets its cancellation
    /// flag (aborting any running turn) and drops it from both the instance
    /// map and the persistent map. Returns true if the instance existed.
    pub async fn destroy_persistent(&self, id: &str) -> bool {
        let removed = {
            let mut persistent = self.persistent.write().await;
            match persistent.remove(id) {
                Some(entry) => {
                    entry.cancelled.store(true, Ordering::SeqCst);
                    true
                }
                None => false,
            }
        };
        if removed {
            let mut instances = self.instances.write().await;
            instances.remove(id);
            drop(instances);
            self.usage_by_instance
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(id);
            self.revive_scope(id);
            self.agent_environments
                .write()
                .unwrap_or_else(|error| error.into_inner())
                .remove(id);
        }
        removed
    }

    /// Resolve a definition by name, falling back to a dynamic definition
    /// for ad-hoc types (e.g. `self`), mirroring `spawn_and_run`.
    async fn definition_for(&self, type_name: &str, role: &str) -> SubagentDefinition {
        self.get_definition(type_name)
            .await
            .unwrap_or_else(|| SubagentDefinition {
                name: type_name.to_string(),
                description: format!("Dynamic subagent for {role}"),
                system_prompt: format!("You are {role}. Complete the user's task accurately."),
                tools: vec![
                    "read".into(),
                    "grep".into(),
                    "glob".into(),
                    "fetch_url".into(),
                    "web_search".into(),
                    "list_directory".into(),
                ],
                disallowed_tools: Vec::new(),
                when_to_use: None,
                prompt_prefix: None,
                summary_policy: None,
                model: None,
                subagents: None,
            })
    }

    /// Retrieve an instance snapshot by ID.
    pub async fn get_instance(&self, id: &str) -> Option<SubagentInstance> {
        let instances = self.instances.read().await;
        instances.get(id).map(|(inst, _)| inst.clone())
    }

    /// Update the state of a subagent instance. A terminal state retires the
    /// scope into the completed-scope LRU (v2 `SubagentCompleted` /
    /// `SubagentFailed` / `SubagentCancelled`).
    pub async fn update_state(&self, id: &str, state: SubagentState, result: Option<String>) {
        {
            let mut instances = self.instances.write().await;
            if let Some((inst, _)) = instances.get_mut(id) {
                inst.state = state;
                if result.is_some() {
                    inst.last_result = result;
                }
            }
        }
        if state.is_terminal() {
            self.retire_completed_scope(id).await;
        }
    }

    /// Terminate a running subagent the way v2's `removeManaged` does:
    /// cancel the prompt first, wait up to three seconds for the turn to go
    /// quiet, then stop the remaining background tasks, retire the scope and
    /// surface a stop failure. Callers that are themselves inside the agent's
    /// turn use [`SubagentManager::kill_now`] — waiting on a claim they hold
    /// would deadlock until the timeout.
    pub async fn kill(&self, id: &str) -> Result<bool, String> {
        self.kill_inner(id, true).await
    }

    /// Cancel without the quiescence wait (v2's in-turn cancellation paths).
    pub(crate) async fn kill_now(&self, id: &str) -> Result<bool, String> {
        self.kill_inner(id, false).await
    }

    async fn kill_inner(&self, id: &str, await_quiet: bool) -> Result<bool, String> {
        // 1. Cancel the prompt. The state flips to `Terminated` right away so
        //    the roster never shows a killed agent as running while it drains.
        let killed = {
            let mut instances = self.instances.write().await;
            match instances.get_mut(id) {
                Some((inst, cancel_flag)) => {
                    cancel_flag.store(true, Ordering::SeqCst);
                    inst.state = SubagentState::Terminated;
                    true
                }
                None => false,
            }
        };
        if !killed {
            return Ok(false);
        }
        // 2. Wait for the in-flight turn to observe the cancel and release its
        //    claim (v2 `removeManaged`'s prompt-quiescence loop, 3 s bound).
        if await_quiet {
            self.await_foreground_quiet(id).await;
        }
        // 3. Stop whatever the turn left running in the background
        //    (v2 `stopAllOnExit('Session closed')`). The error is carried out
        //    after cleanup, like v2 throwing the stop failure.
        let mut stop_error = None;
        let runner = self.task_runner.read().await.clone();
        // A foreground agent usually owns no background task; `stop` reports
        // an unknown id as an error, which is not a shutdown failure. Only
        // call it when the runner actually tracks a task for this id.
        if let Some(runner) = runner
            && runner.entry(id).is_some()
        {
            match runner.stop(id, None).await {
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(
                        %error,
                        "could not stop a subagent's background task during kill"
                    );
                    stop_error = Some(error);
                }
            }
        }
        // 4. Re-assert the terminal state (the turn's own completion path may
        //    have raced the cancel) and retire the scope.
        {
            let mut instances = self.instances.write().await;
            if let Some((inst, _)) = instances.get_mut(id) {
                inst.state = SubagentState::Terminated;
            }
        }
        self.retire_completed_scope(id).await;
        // The binding belongs to the live scope; a rebuilt scope starts from
        // the session environment again.
        self.agent_environments
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .remove(id);
        match stop_error {
            Some(error) => Err(error),
            None => Ok(true),
        }
    }

    /// Poll until `id`'s foreground turn releases its claim, bounded by
    /// [`REMOVE_PROMPT_QUIESCE_TIMEOUT`]. A timeout is not an error: the turn
    /// was already told to cancel and the scope is terminated regardless.
    async fn await_foreground_quiet(&self, id: &str) {
        let deadline = tokio::time::Instant::now() + REMOVE_PROMPT_QUIESCE_TIMEOUT;
        loop {
            let running = self
                .foreground_runs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(id);
            if !running {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!(
                    subagent = %id,
                    "subagent turn did not go quiet within the removal bound; terminating anyway"
                );
                return;
            }
            tokio::time::sleep(REMOVE_PROMPT_QUIESCE_POLL).await;
        }
    }

    /// The scope-cache env vars are validated once per manager; a bad value
    /// fails the spawn that would create the first cached scope instead of
    /// silently running without eviction.
    fn scope_cache_error(&self) -> Result<(), String> {
        let cache = self.scope_cache.lock().unwrap_or_else(|e| e.into_inner());
        match cache.error.as_ref() {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }

    /// Drop `id` from the LRU: a scope that is live again must not be evicted
    /// (v2 `revive`).
    fn revive_scope(&self, id: &str) {
        self.scope_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .revive(id);
    }

    /// Retire a completed scope into the LRU and evict the overflow (v2
    /// `retire` → `evictOverflow`). Persistent instances are excluded — their
    /// conversation lives only in memory, so `destroy_persistent` stays their
    /// single removal path — and so is every manager without a session store:
    /// there the in-memory conversation is the only copy, so evicting it would
    /// break `resume` instead of rebuilding it.
    async fn retire_completed_scope(&self, id: &str) {
        if self.persistent.read().await.contains_key(id) {
            return;
        }
        if self.session_store.read().await.is_none() {
            return;
        }
        {
            let mut cache = self.scope_cache.lock().unwrap_or_else(|e| e.into_inner());
            if cache.capacity == 0 || cache.error.is_some() {
                return;
            }
            cache.retire(id);
        }
        self.evict_completed_scopes().await;
    }

    /// Evict completed scopes until the cache fits its capacity. A scope that
    /// refuses eviction is skipped for the rest of this pass and retried on a
    /// later one (v2 `evictOverflow`).
    async fn evict_completed_scopes(&self) {
        let mut skipped: HashSet<String> = HashSet::new();
        loop {
            let (timeout, candidate) = {
                let cache = self.scope_cache.lock().unwrap_or_else(|e| e.into_inner());
                if cache.retired.len() <= cache.capacity {
                    return;
                }
                let Some(candidate) = cache.oldest_candidate(&skipped) else {
                    return;
                };
                (cache.evict_timeout, candidate)
            };
            let (id, attempts) = candidate;
            self.scope_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            match self.evict_scope(&id, timeout).await {
                EvictOutcome::Removed | EvictOutcome::Missing => continue,
                EvictOutcome::Deferred => {
                    // Still running: keep it retired at the same attempt count
                    // and move on (v2 defers without spending an attempt).
                    skipped.insert(id.clone());
                    self.scope_cache
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .reinsert(&id, attempts);
                }
                EvictOutcome::Timeout => {
                    skipped.insert(id.clone());
                    let next_attempt = attempts + 1;
                    if next_attempt >= MAX_SCOPE_EVICT_ATTEMPTS {
                        tracing::warn!(
                            subagent = %id,
                            attempts = next_attempt,
                            "subagent scope eviction timed out; leaving it resident"
                        );
                    }
                    self.scope_cache
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .reinsert(&id, next_attempt);
                }
            }
        }
    }

    /// Drop one completed scope's resident state (v2 `evict`): the instance
    /// entry and the in-memory conversation. The persisted resume record is
    /// untouched — `resume` rebuilds both from it. The bound covers the whole
    /// removal, so a scope whose locks stay contended is skipped rather than
    /// stalling the queue.
    async fn evict_scope(&self, id: &str, timeout: Duration) -> EvictOutcome {
        {
            let instances = self.instances.read().await;
            match instances.get(id) {
                None => return EvictOutcome::Missing,
                Some((instance, _)) if instance.state == SubagentState::Running => {
                    return EvictOutcome::Deferred;
                }
                Some(_) => {}
            }
        }
        let removal = async {
            let mut instances = self.instances.write().await;
            instances.remove(id);
            self.foreground_histories
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(id);
            self.usage_by_instance
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(id);
            // Session-lifetime bookkeeping goes with the scope; the persisted
            // record still names the parent, so a cold resume recovers it.
            self.parents.write().await.remove(id);
            self.agent_environments
                .write()
                .unwrap_or_else(|error| error.into_inner())
                .remove(id);
        };
        match tokio::time::timeout(timeout, removal).await {
            Ok(()) => EvictOutcome::Removed,
            Err(_) => EvictOutcome::Timeout,
        }
    }

    /// Recreate the resident instance of a scope that was evicted (v2
    /// `rebuildSubagent`): the persisted resume record carries the profile and
    /// role, so the rebuilt instance is indistinguishable from the original
    /// one for the resumed turn — and the rebuilt conversation is tracked by
    /// the LRU again instead of leaking untracked.
    ///
    /// A terminated (killed) scope is still resident with its cancellation
    /// flag raised; v2 treats that as a closing scope: create refuses the id
    /// (`AGENT_ALREADY_EXISTS`) until the old scope is disposed. The poll loop
    /// below is `createAgentAwaitingClose`, bounded by the same eviction
    /// timeout.
    async fn rebuild_instance(&self, id: &str, type_name: &str, role: &str) -> Result<(), String> {
        {
            let instances = self.instances.read().await;
            if let Some((instance, _)) = instances.get(id)
                && instance.state != SubagentState::Terminated
            {
                return Ok(());
            }
        }
        if !self.await_scope_close(id).await {
            return Err(format!(
                "agent.already_exists: a subagent with id \"{id}\" already exists — pass a different id"
            ));
        }
        let depth = self.persisted_depth(id).await;
        if let Some(source) = self.persisted_forked_from(id).await {
            self.forked_from
                .write()
                .await
                .insert(id.to_string(), source);
        }
        let mut instances = self.instances.write().await;
        if instances.contains_key(id) {
            return Ok(());
        }
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        instances.insert(
            id.to_string(),
            (
                SubagentInstance {
                    id: id.to_string(),
                    type_name: type_name.to_string(),
                    role: role.to_string(),
                    state: SubagentState::Running,
                    created_at_ms: now_ms,
                    last_result: None,
                    depth,
                },
                Arc::new(AtomicBool::new(false)),
            ),
        );
        drop(instances);
        self.observe_agent_id(id);
        self.revive_scope(id);
        self.register_agent_metadata(id).await;
        Ok(())
    }

    /// v2 `createAgentAwaitingClose`: retry the create while a closing scope
    /// still holds `id`, every [`SCOPE_CLOSE_POLL_INTERVAL`], until the scope
    /// eviction timeout elapses.
    async fn await_scope_close(&self, id: &str) -> bool {
        let timeout = self
            .scope_cache
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .evict_timeout;
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match self
                .evict_scope(id, remaining.max(Duration::from_millis(1)))
                .await
            {
                EvictOutcome::Removed | EvictOutcome::Missing => return true,
                EvictOutcome::Deferred | EvictOutcome::Timeout => {}
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(SCOPE_CLOSE_POLL_INTERVAL).await;
        }
    }

    /// List summaries of all subagent instances.
    pub async fn list(&self) -> Vec<SubagentSummary> {
        let instances = self.instances.read().await;
        let mut list: Vec<SubagentSummary> = instances
            .values()
            .map(|(inst, _)| SubagentSummary {
                id: inst.id.clone(),
                type_name: inst.type_name.clone(),
                role: inst.role.clone(),
                state: inst.state,
                created_at_ms: inst.created_at_ms,
            })
            .collect();
        list.sort_by_key(|s| std::cmp::Reverse(s.created_at_ms));
        list
    }
}

/// LLM wrapper that records the content of the final assistant response so
/// a persistent instance can append it to its message history after a turn.
struct RecordingLlm {
    inner: Arc<dyn crate::turn_loop::types::LLM>,
    last_content: Arc<Mutex<Option<String>>>,
}

impl RecordingLlm {
    fn new(inner: Arc<dyn crate::turn_loop::types::LLM>) -> Self {
        Self {
            inner,
            last_content: Arc::new(Mutex::new(None)),
        }
    }

    fn last_content(&self) -> Option<String> {
        self.last_content.lock().unwrap().clone()
    }
}

impl crate::turn_loop::types::LLM for RecordingLlm {
    fn system_prompt(&self) -> &str {
        self.inner.system_prompt()
    }

    fn model_name(&self) -> &str {
        self.inner.model_name()
    }

    fn is_retryable_error(&self, error: &str) -> bool {
        self.inner.is_retryable_error(error)
    }

    fn transport(&self) -> &'static str {
        self.inner.transport()
    }

    fn chat(
        &self,
        params: crate::turn_loop::types::LLMChatParams,
    ) -> crate::rpc::types::BoxFuture<
        '_,
        Result<crate::turn_loop::types::LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>,
    > {
        let inner = self.inner.clone();
        let last_content = self.last_content.clone();
        Box::pin(async move {
            let response = inner.chat(params).await?;
            if !response.content.is_empty() {
                *last_content.lock().unwrap() = Some(response.content.clone());
            }
            Ok(response)
        })
    }
}

/// Sum two token usage records field by field.
fn add_usage(
    a: &crate::rpc::types::TokenUsage,
    b: &crate::rpc::types::TokenUsage,
) -> crate::rpc::types::TokenUsage {
    crate::rpc::types::TokenUsage {
        input_tokens: a.input_tokens + b.input_tokens,
        output_tokens: a.output_tokens + b.output_tokens,
        total_tokens: a.total_tokens + b.total_tokens,
        input_cache_read: a.input_cache_read + b.input_cache_read,
        input_cache_creation: a.input_cache_creation + b.input_cache_creation,
    }
}

#[cfg(test)]
mod tests;
