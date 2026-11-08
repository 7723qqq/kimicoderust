/// Host callback trait for LLM chat and tool execution.
///
/// This trait abstracts the transport layer — whether it's JSON-RPC over
/// stdio (the RpcServer-based implementation) or direct napi-rs
/// ThreadsafeFunction calls. The turn loop uses this trait to call back
/// to the JS host for LLM inference and tool execution.
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::i18n::LocalizedText;
use crate::rpc::types::{
    AskQuestionRequest, AskQuestionResponse, AuthTokenResponse, BoxFuture, CheckpointRequest,
    ListToolsResponse, LlmChatRequest, LlmChatResponse, ParseBashRequest, PermissionCheckRequest,
    PermissionDecision, StateReadRequest, StateReadResponse, StateWriteRequest, StateWriteResponse,
    ToolExecuteRequest, ToolExecuteResponse,
};
use crate::turn_loop::types::{GoalContext, LLMMessage};

/// Sentinel returned by [`HostCallbacks::checkpoint`] when the host has no
/// checkpoint support — the trait default, or a napi host that never wired the
/// `checkpoint` TSFN. Callers treat it as fail-open (capability absent, not a
/// failure); one shared constant keeps the producers and the consumer in
/// [`StateStoreCallbacks`] from drifting apart.
pub const CHECKPOINT_UNSUPPORTED: &str = "host does not support checkpoint";

/// The root agent's id — the value every non-subagent event carries.
///
/// v2 spells the same constant `MAIN_AGENT_ID` in
/// `session/agentLifecycle/agentLifecycle.ts:13`.
pub const MAIN_AGENT_ID: &str = "main";

/// Adapts any host callback chain to the title generator's telemetry sink.
pub struct TitleTelemetry<'a>(pub &'a dyn HostCallbacks);

impl crate::session::title::TitleTelemetrySink for TitleTelemetry<'_> {
    fn track(&self, event: serde_json::Value) {
        self.0.telemetry(event);
    }
}

/// Host-provided callbacks that the turn loop needs to call back to JS.
pub trait HostCallbacks: Send + Sync {
    /// Send an LLM chat request to the JS host and return the response.
    fn llm_chat(
        &self,
        request: LlmChatRequest,
    ) -> BoxFuture<'static, Result<LlmChatResponse, String>>;

    /// The user-configured external-hook gate for this pipeline (G-6 #6),
    /// when one was built — lets engine subsystems that only hold
    /// `Arc<dyn HostCallbacks>` (the Agent tool spawners, the swarm
    /// launcher) dispatch the observe-only session hook events. `None`
    /// when hooks were not configured for this session.
    fn hook_guard(&self) -> Option<Arc<crate::tools::external_hooks::HookGuard>> {
        None
    }

    /// Send a tool execution request to the JS host and return the response.
    fn execute_tool(
        &self,
        request: ToolExecuteRequest,
    ) -> BoxFuture<'static, Result<ToolExecuteResponse, String>>;

    /// Whether the host itself owns execution of this tool, so the engine must
    /// route it to the host instead of running it natively. Default false; an
    /// ACP client that virtualizes the filesystem sets this for `Read`/`Write`
    /// when it advertises the `fs.readTextFile` / `fs.writeTextFile` capability.
    fn owns_tool(&self, _tool_name: &str) -> bool {
        false
    }

    /// Ask the host whether a mutating tool call may execute natively. The
    /// host runs its full permission machinery (mode, rules, policies,
    /// interactive approval) and answers `allow` or `deny`. A deny verdict
    /// must be returned to the model as the tool result — never retried on
    /// the host path, which would prompt the user twice.
    fn check_permission(
        &self,
        request: PermissionCheckRequest,
    ) -> BoxFuture<'static, Result<PermissionDecision, String>>;

    /// Ask the host an interactive question and wait for a human answer
    /// (ask-user-question class tools). The host owns the interaction
    /// runtime — pending key, dismiss, turn-end cancellation — and answers
    /// with the v2 `QuestionResult` three states (answered / dismissed /
    /// cancelled). No timeout: like a permission check, this one waits on a
    /// human. The default answers with an error so an unwired host gets a
    /// tool result telling the model not to call the tool again.
    fn ask_question(
        &self,
        request: AskQuestionRequest,
    ) -> BoxFuture<'static, Result<AskQuestionResponse, String>> {
        let _ = request;
        Box::pin(async { Err("host does not support interactive questions".into()) })
    }

    /// Read host-owned durable state (state-bridge class tools). The host
    /// stays the persistence and undo authority; the engine's native
    /// todo/plan tools read through this seam. Bounded by
    /// [`HOST_STATE_TIMEOUT`]: host bookkeeping, no human in the loop. The
    /// default answers with an error so an unwired host gets a tool result
    /// telling the model not to call the tool again.
    fn state_read(
        &self,
        request: StateReadRequest,
    ) -> BoxFuture<'static, Result<StateReadResponse, String>> {
        let _ = request;
        Box::pin(async { Err("host does not support state bridge".into()) })
    }

    /// Write host-owned durable state (state-bridge class tools). The host
    /// applies its domain semantics (re-normalization, undoable events) and
    /// returns the resulting state. Bounded by [`HOST_STATE_TIMEOUT`]. The
    /// default answers with an error so an unwired host gets a tool result
    /// telling the model not to call the tool again.
    fn state_write(
        &self,
        request: StateWriteRequest,
    ) -> BoxFuture<'static, Result<StateWriteResponse, String>> {
        let _ = request;
        Box::pin(async { Err("host does not support state bridge".into()) })
    }

    /// Host-side file checkpoint for native write executions (P53:
    /// `host/checkpoint`). `phase: "prepare"` must complete — pre-image
    /// captured — before the engine writes; `phase: "record"` notes the
    /// post-image after execution. Fail-open: the default errors and the
    /// caller skips checkpointing (the pre-P53 status quo, native writes
    /// never checkpointed).
    fn checkpoint(&self, request: CheckpointRequest) -> BoxFuture<'static, Result<(), String>> {
        let _ = request;
        Box::pin(async { Err(CHECKPOINT_UNSUPPORTED.into()) })
    }

    /// Push the durable-state snapshot an undo of the turn about to run
    /// restores (v2 undo anchor). Implemented by [`StateStoreCallbacks`] for
    /// sessions whose state lives in the engine's local store; the default
    /// is a no-op so hosts that own their state (stdio RPC, ACP) are
    /// unaffected.
    fn checkpoint_state(&self) -> BoxFuture<'static, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    /// Fetch the host's current tool table (M1d: `host/list_tools`). Called
    /// before each LLM call on native transports so mid-turn registry
    /// changes (feature tools, MCP reconnects) reach the model — the
    /// turn-start snapshot is only the fallback. Bounded by
    /// [`HOST_LIST_TOOLS_TIMEOUT`]: host bookkeeping, no human in the loop.
    /// The default answers with an error so run_turn falls back to the
    /// snapshot, matching the pre-seam behaviour.
    fn list_tools(&self) -> BoxFuture<'static, Result<ListToolsResponse, String>> {
        Box::pin(async { Err("host does not support list_tools".into()) })
    }

    /// Fetch the host's current goal snapshot (M1d 3b: `host/goal`). The
    /// session's goal provider reads it fresh per turn so host-side goal
    /// changes (pause, budget edits, terminal states) are reflected. Bounded
    /// by [`HOST_LIST_TOOLS_TIMEOUT`]-style host bookkeeping; the default
    /// answers with an error so sessions without the seam run without goal
    /// budgeting, matching the no-goal fallback.
    fn goal(&self) -> BoxFuture<'static, Result<Option<GoalContext>, String>> {
        Box::pin(async { Err("host does not support goal".into()) })
    }

    /// Parse a Bash command for the AGENTS.md reminder's operand directories
    /// (v2 `extractBashTargetDirs`, `host/parse_bash`).
    ///
    /// The engine parses bash itself (`tools::bash_target_dirs`), so this
    /// channel is an **override**: a host that supplies it is asked, and one
    /// that does not falls back to the engine's own parse — never to "no
    /// operands". The `Ok(None)` default below therefore means "this host has
    /// nothing to say", which is what [`HostCallbacks::supports_parse_bash`]
    /// distinguishes from "the command did not parse".
    fn parse_bash(
        &self,
        request: ParseBashRequest,
    ) -> BoxFuture<'static, Result<Option<Vec<String>>, String>> {
        let _ = request;
        Box::pin(async { Ok(None) })
    }

    /// Whether [`HostCallbacks::parse_bash`] is a real parser rather than the
    /// trait default. False keeps the engine's own parse in charge.
    fn supports_parse_bash(&self) -> bool {
        false
    }

    /// Fetch a bearer token for an OAuth-managed provider (`host/auth_token`).
    /// The host owns the OAuth store — single-flight refresh, expiry-aware
    /// cache — so the transport asks instead of holding credentials; `force`
    /// asks it to refresh past the cache after a 401/403 from the provider.
    /// The default errors so an unwired host (REPL, tests) keeps static-key
    /// transports working unchanged.
    fn auth_token(
        &self,
        provider: String,
        force: bool,
    ) -> BoxFuture<'static, Result<String, String>> {
        let _ = (provider, force);
        Box::pin(async { Err("host does not support oauth token fetch".into()) })
    }

    /// Install or refresh native file-history capture for the turn about to
    /// run: `write` / `edit` then record a `session_file_history` row per
    /// successful change, which is what `/file-history/*` and
    /// `undo {revert_files:true}` read (`turn_id` attributes the rows).
    ///
    /// No-op by default: a host without a native tool runtime has nothing to
    /// record, and non-server entries (stdio host-driven turns, REPL) keep
    /// their existing behaviour.
    fn set_file_history(
        &self,
        _store: Arc<crate::session::sqlite_store::SqliteSessionStore>,
        _session_id: String,
        _turn_id: usize,
    ) {
    }

    /// Release the steering prompts injected during the active turn. The
    /// engine-local steer queue (see `SteerQueueCallbacks`) serves this at
    /// every step head; the default answers with nothing.
    fn drain_steers(&self) -> BoxFuture<'static, Result<Vec<LLMMessage>, String>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    /// Record which goal was active when a turn started (G-6 #8). The turn
    /// loop calls this with its turn-start goal snapshot; the native-tool
    /// gate uses it to veto goal mutation calls from a turn whose goal has
    /// since changed. The default ignores the binding — paths without the
    /// gate never veto on staleness.
    fn set_turn_goal(&self, _turn_id: &str, _goal_id: Option<&str>) {}

    /// Fire-and-forget event notification to the JS host. Used by the
    /// native LLM / native tool paths to report step boundaries, streaming
    /// deltas, and natively-executed tool results so the host can record
    /// them in the transcript. The default implementation drops the event.
    fn emit_event(&self, event: serde_json::Value) {
        let _ = event;
    }

    /// Receive a turn lifecycle event from the engine (M1b: `host/turn_event`).
    /// The host is expected to dispatch the durable events (`prompt`,
    /// `cancel`, `ended`) into its append log + state fold (v2 `Event2`
    /// pipeline) and to surface the observable ones (`started`) to the UI.
    /// The default implementation drops the event — hosts that don't yet
    /// model the engine-owned turn lifecycle can safely no-op.
    fn turn_event(&self, _event: crate::turn_events::TurnEvent) {}

    /// Fire-and-forget turn telemetry from the engine (M1c: `host/telemetry`).
    /// The payload is a JSON object whose `event` field carries the event name
    /// (`turn_started` / `turn_ended` / `turn_interrupted`) plus the v2
    /// telemetry payload fields (turn_id, mode, provider_type, protocol,
    /// thinking_effort, reason, duration_ms, steps, at_step,
    /// interrupt_reason). The host forwards these to its telemetry sink —
    /// one track2 per event, no host-side duplicates. The default drops the
    /// event, so hosts without the seam keep owning their turn telemetry.
    fn telemetry(&self, _event: serde_json::Value) {}

    /// Tell the host it may stop working on an LLM request that lost a race.
    ///
    /// Aborting the Rust task is not enough: the request has already been
    /// handed to the host, and dropping the receiver leaves the host's
    /// provider call running to completion — billed — for every loser of
    /// every MultiLLM step. This is fire-and-forget, so the host is free to
    /// ignore an id it has already finished with.
    fn cancel_llm_chat(&self, request_id: &str) {
        self.emit_event(serde_json::json!({
            "type": "llm_chat.cancel",
            "request_id": request_id,
        }));
    }
}

/// Outer bound on a host LLM call. Generous on purpose — a long generation on
/// a slow provider must still fit — but a host that never answers must not
/// hang the turn forever.
pub const HOST_LLM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(900);

/// Outer bound on a host tool call. Tools carry their own timeouts (native
/// Bash caps at 300s); this covers a stalled host.
pub const HOST_TOOL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// Outer bound on a host state bridge call (state_read / state_write). The
/// host applies domain semantics to durable state — bookkeeping with no
/// human in the loop — so a stalled answer must not hold the step open.
pub const HOST_STATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Outer bound on a `host/list_tools` call. Building the tool table is host
/// bookkeeping (registry read + schema shaping) with no human in the loop,
/// so a stalled answer must not hold the step open; on timeout run_turn
/// falls back to the turn-start snapshot.
pub const HOST_LIST_TOOLS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Outer bound on a `host/auth_token` call. A cache hit answers immediately;
/// a miss triggers an OAuth refresh round-trip (network, no human in the
/// loop), so the bound covers one slow refresh rather than a stalled host.
pub const HOST_AUTH_TOKEN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// A concrete implementation of [`HostCallbacks`] backed by the stdio
/// JSON-RPC server. Used in the CLI binary mode.
pub struct RpcHostCallbacks {
    pub server: Arc<crate::rpc::server::RpcServer>,
}

impl HostCallbacks for RpcHostCallbacks {
    fn llm_chat(
        &self,
        request: LlmChatRequest,
    ) -> BoxFuture<'static, Result<LlmChatResponse, String>> {
        let server = self.server.clone();
        Box::pin(async move {
            let params = serde_json::to_value(&request)
                .map_err(|e| format!("LLM chat serialize error: {e}"))?;
            let response_value = server
                .invoke(
                    crate::rpc::types::methods::HOST_LLM_CHAT,
                    params,
                    Some(HOST_LLM_TIMEOUT),
                )
                .await
                .map_err(|e| format!("LLM chat error: {e}"))?;
            serde_json::from_value(response_value)
                .map_err(|e| format!("LLM chat response parse error: {e}"))
        })
    }

    fn execute_tool(
        &self,
        request: ToolExecuteRequest,
    ) -> BoxFuture<'static, Result<ToolExecuteResponse, String>> {
        let server = self.server.clone();
        Box::pin(async move {
            let params = serde_json::to_value(&request)
                .map_err(|e| format!("Tool execute serialize error: {e}"))?;
            let response_value = server
                .invoke(
                    crate::rpc::types::methods::HOST_EXECUTE_TOOL,
                    params,
                    Some(HOST_TOOL_TIMEOUT),
                )
                .await
                .map_err(|e| format!("Tool execute error: {e}"))?;
            serde_json::from_value(response_value)
                .map_err(|e| format!("Tool execute response parse error: {e}"))
        })
    }

    fn check_permission(
        &self,
        request: PermissionCheckRequest,
    ) -> BoxFuture<'static, Result<PermissionDecision, String>> {
        let server = self.server.clone();
        Box::pin(async move {
            let params = serde_json::to_value(&request)
                .map_err(|e| format!("Permission check serialize error: {e}"))?;
            // No timeout: a permission check is answered by a human, and
            // giving up would discard an approval the user already granted.
            let response_value = server
                .invoke(
                    crate::rpc::types::methods::HOST_CHECK_PERMISSION,
                    params,
                    None,
                )
                .await
                .map_err(|e| format!("Permission check error: {e}"))?;
            serde_json::from_value(response_value)
                .map_err(|e| format!("Permission decision parse error: {e}"))
        })
    }

    fn ask_question(
        &self,
        request: AskQuestionRequest,
    ) -> BoxFuture<'static, Result<AskQuestionResponse, String>> {
        let server = self.server.clone();
        Box::pin(async move {
            let params = serde_json::to_value(&request)
                .map_err(|e| format!("Ask question serialize error: {e}"))?;
            // No timeout: a question is answered by a human, and giving up
            // would discard an answer the user already gave.
            let response_value = server
                .invoke(crate::rpc::types::methods::HOST_ASK_QUESTION, params, None)
                .await
                .map_err(|e| format!("Ask question error: {e}"))?;
            serde_json::from_value(response_value)
                .map_err(|e| format!("Ask question response parse error: {e}"))
        })
    }

    fn state_read(
        &self,
        request: StateReadRequest,
    ) -> BoxFuture<'static, Result<StateReadResponse, String>> {
        let server = self.server.clone();
        Box::pin(async move {
            let params = serde_json::to_value(&request)
                .map_err(|e| format!("State read serialize error: {e}"))?;
            // Bounded: host bookkeeping with no human in the loop.
            let response_value = server
                .invoke(
                    crate::rpc::types::methods::HOST_STATE_READ,
                    params,
                    Some(HOST_STATE_TIMEOUT),
                )
                .await
                .map_err(|e| format!("State read error: {e}"))?;
            serde_json::from_value(response_value)
                .map_err(|e| format!("State read response parse error: {e}"))
        })
    }

    fn checkpoint(&self, request: CheckpointRequest) -> BoxFuture<'static, Result<(), String>> {
        let server = self.server.clone();
        Box::pin(async move {
            let params = serde_json::to_value(&request)
                .map_err(|e| format!("Checkpoint serialize error: {e}"))?;
            // Bounded: pre-image capture is host bookkeeping, but the engine
            // waits for it before writing — the timeout bounds that wait.
            server
                .invoke(
                    crate::rpc::types::methods::HOST_CHECKPOINT,
                    params,
                    Some(HOST_STATE_TIMEOUT),
                )
                .await
                .map(|_| ())
                .map_err(|e| format!("Checkpoint error: {e}"))
        })
    }

    fn state_write(
        &self,
        request: StateWriteRequest,
    ) -> BoxFuture<'static, Result<StateWriteResponse, String>> {
        let server = self.server.clone();
        Box::pin(async move {
            let params = serde_json::to_value(&request)
                .map_err(|e| format!("State write serialize error: {e}"))?;
            // Bounded: host bookkeeping with no human in the loop.
            let response_value = server
                .invoke(
                    crate::rpc::types::methods::HOST_STATE_WRITE,
                    params,
                    Some(HOST_STATE_TIMEOUT),
                )
                .await
                .map_err(|e| format!("State write error: {e}"))?;
            serde_json::from_value(response_value)
                .map_err(|e| format!("State write response parse error: {e}"))
        })
    }

    fn list_tools(&self) -> BoxFuture<'static, Result<ListToolsResponse, String>> {
        let server = self.server.clone();
        Box::pin(async move {
            let response_value = server
                .invoke(
                    crate::rpc::types::methods::HOST_LIST_TOOLS,
                    serde_json::json!({}),
                    Some(HOST_LIST_TOOLS_TIMEOUT),
                )
                .await
                .map_err(|e| format!("List tools error: {e}"))?;
            serde_json::from_value(response_value)
                .map_err(|e| format!("List tools response parse error: {e}"))
        })
    }

    fn goal(&self) -> BoxFuture<'static, Result<Option<GoalContext>, String>> {
        let server = self.server.clone();
        Box::pin(async move {
            let response_value = server
                .invoke(
                    crate::rpc::types::methods::HOST_GOAL,
                    serde_json::json!({}),
                    Some(HOST_LIST_TOOLS_TIMEOUT),
                )
                .await
                .map_err(|e| format!("Goal error: {e}"))?;
            serde_json::from_value(response_value)
                .map_err(|e| format!("Goal response parse error: {e}"))
        })
    }

    fn auth_token(
        &self,
        provider: String,
        force: bool,
    ) -> BoxFuture<'static, Result<String, String>> {
        let server = self.server.clone();
        Box::pin(async move {
            let response_value = server
                .invoke(
                    crate::rpc::types::methods::HOST_AUTH_TOKEN,
                    serde_json::json!({ "provider": provider, "force": force }),
                    Some(HOST_AUTH_TOKEN_TIMEOUT),
                )
                .await
                .map_err(|e| format!("Auth token error: {e}"))?;
            let token = serde_json::from_value::<AuthTokenResponse>(response_value)
                .map_err(|e| format!("Auth token response parse error: {e}"))?;
            Ok(token.token)
        })
    }

    fn emit_event(&self, event: serde_json::Value) {
        // JSON-RPC notification over stdout — fire-and-forget by design.
        crate::rpc::server::RpcServer::notify_now(crate::rpc::types::methods::HOST_EVENT, &event);
    }

    fn turn_event(&self, event: crate::turn_events::TurnEvent) {
        // Structurally infallible — every field is already JSON — but a
        // failure must not pass silently: the host's schema rejects the
        // marker and reports it, which beats dropping a durable record.
        let payload = serde_json::to_value(&event)
            .unwrap_or_else(|e| serde_json::json!({ "error": format!("turn_event: {e}") }));
        crate::rpc::server::RpcServer::notify_now(
            crate::rpc::types::methods::HOST_TURN_EVENT,
            &payload,
        );
    }

    fn telemetry(&self, event: serde_json::Value) {
        // Fire-and-forget notification over stdout, same contract as
        // `host/event` / `host/turn_event`.
        crate::rpc::server::RpcServer::notify_now(
            crate::rpc::types::methods::HOST_TELEMETRY,
            &event,
        );
    }
}

/// File write paths a native call targets (P53 checkpoint seam): the same
/// inference the tool scheduler uses for conflict detection, filtered to
/// file accesses that write.
fn checkpoint_write_paths(tool_name: &str, args: &serde_json::Value) -> Vec<String> {
    use crate::turn_loop::types::{FileOperation, ToolResourceAccess};
    crate::turn_loop::tool_scheduler::infer_tool_accesses(tool_name, args)
        .into_iter()
        .filter_map(|access| match access {
            ToolResourceAccess::File(file)
                if matches!(
                    file.operation,
                    FileOperation::Write | FileOperation::ReadWrite
                ) =>
            {
                Some(file.path)
            }
            _ => None,
        })
        .collect()
}

/// A [`HostCallbacks`] decorator that executes tools natively (inside the
/// Rust process, sandboxed to the workspace) and forwards everything the
/// sandbox cannot handle to the wrapped callbacks.
///
/// **Every** natively-executed call — read-only or mutating — is gated on a
/// host permission grant first: the host policy chain can require interactive
/// approval even for reads (sensitive-file access). A deny verdict becomes
/// the tool result without any execution, natively or on the host.
/// Natively-executed calls are reported to the host via
/// [`HostCallbacks::emit_event`] (`type: "tool.native"`) so the transcript
/// still records them.
pub struct NativeToolCallbacks {
    pub inner: Arc<dyn HostCallbacks>,
    pub toolset: Arc<crate::tools::NativeToolset>,
    /// Counts the calls this wrapper executed in-process. The composition root
    /// holds the same handle to fill in `TurnResult::native_tool_calls`.
    pub native_count: Arc<AtomicU32>,
    /// Optional in-process truncator (P26 batch 4). When `Some`, large native
    /// results are truncated and spilled locally. When `None` (no workspace
    /// root), results pass through untruncated.
    pub truncator: Option<Arc<crate::tool_result_truncation::ToolResultTruncator>>,
    /// Optional in-process permission engine (P26 batch 3). When `Some`, tool
    /// calls are evaluated against the per-turn `PolicySnapshot` locally in
    /// Rust, bypassing the host `host/check_permission` seam for allow/deny
    /// verdicts.
    pub permission_engine: Option<Arc<crate::permission::PermissionEngine>>,
    /// Optional plan-mode guard. When plan mode is active, write/edit tools
    /// may only target the plan file and TaskStop / CronCreate / CronDelete
    /// are denied; the closure returns the denial reason, or `None` to let
    /// the call through. The REPL reads its local store; the napi/stdio
    /// paths read the host's plan state through the state bridge per
    /// guarded call.
    pub plan_guard: Option<Arc<PlanGuard>>,
    /// Optional stale-write gate (fork-original, G-6 #3 — no v2 counterpart
    /// exists; see `tools/stale_guard.rs` header).
    /// Before a native Write/Edit executes, the gate vetoes targets that
    /// were never read or changed on disk since; after every completed
    /// read/write execution (native or host-forwarded) it records the
    /// target's mtime, so a read the host served also clears a later native
    /// write. State is per-session (mounted once by the pipeline builder).
    pub stale_guard: Option<Arc<crate::tools::stale_guard::StaleGate>>,
    /// Optional goal-operation guard (G-6 #7/#8; symbol names mirror v2
    /// `features/goal/goalService.ts`, routing shape is the fork's own).
    /// CreateGoal calls route to the host when the permission mode
    /// is not `auto` (so the host's goal-start review fires); goal mutation
    /// calls from a turn whose goal has changed since are vetoed.
    pub goal_guard: Option<Arc<crate::tools::goal_guard::GoalGuard>>,
    /// Optional PreToolUse hook gate (v2 `agentExternalHooksService` mirror,
    /// G-6 #6). User-configured hook commands run before native tool calls;
    /// exit 2 or a JSON deny blocks the call (fail-closed).
    pub hook_guard: Option<Arc<crate::tools::external_hooks::HookGuard>>,
    /// P52 native-path vetoes (the host `onBeforeExecuteTool` veto-chain
    /// listeners that have no engine-native counterpart). Non-empty reason:
    /// the affected native calls are rejected with the verbatim reason as
    /// the (error) tool result — no execution, no host fallback. This gate
    /// runs before permission, matching the veto chain's precedence.
    /// `agent_tool_veto` narrows that to the native `Agent` tool;
    /// `tools_veto` denies every native tool (btw side-channel contexts).
    /// No production caller sets either today — they are the host's seam, and
    /// a `None` means "the host has no opinion". (Swarm mode does *not* use
    /// this: v2's mode mutex exits the other mode, and the fork's refuses the
    /// batch outright, so neither denies `Agent` here.)
    /// Non-native calls still fall back to the host, whose own veto chain
    /// denies them there.
    pub agent_tool_veto: Option<String>,
    pub tools_veto: Option<String>,
    pub todo_tool_veto: Option<String>,
    /// Worker write-scope guard (G-6 #13): when set, a Write/Edit whose target
    /// resolves outside this worktree is refused. A *per-worker* guard, not the
    /// feature switch — see `tower_enabled` for the advertised-table gate.
    pub tower_worktree_root: Option<String>,
    /// Whether tower mode is enabled (`KIMI_CODE_EXPERIMENTAL_TOWER` /
    /// `[experimental].tower`). Gates the advertised Tower* tool table. The
    /// gate used to be `tower_worktree_root.is_some()`, which no production
    /// caller ever set, so the whole tower toolset was unreachable there.
    pub tower_enabled: bool,
    /// Whether NotifyUser is available (`KIMI_CODE_EXPERIMENTAL_NOTIFY_USER`
    /// flag AND a host that can show an update — upstream #4054's
    /// `notifyUserAvailable` two-input gate; #4153's nudge service keys off
    /// the same answer). Gates the advertised NotifyUser tool.
    pub notify_user_enabled: bool,
    pub sandbox_policy: Option<crate::tools::sandbox::SandboxExecutionPolicy>,
}

/// A plan-mode tool guard: `(tool_name, args) -> denial reason or None`,
/// resolved asynchronously (the product paths read the host's plan state
/// through the state bridge; see
/// [`crate::tools::plan_mode::plan_denial`] for the decision semantics).
pub type PlanGuard =
    dyn Fn(&str, &serde_json::Value) -> BoxFuture<'static, Option<String>> + Send + Sync;

impl NativeToolCallbacks {
    /// The wiring a standalone host needs: native execution plus the
    /// in-process permission engine and truncator, and no host vetoes.
    ///
    /// Every veto and guard starts absent on purpose. A host without a
    /// plan / tower / swarm context gets the engine's defaults instead of
    /// stubs it would have to keep in step, and a guard added to the struct
    /// later defaults to "the host has no opinion" here rather than breaking
    /// every composition root.
    pub fn for_standalone(
        inner: Arc<dyn HostCallbacks>,
        toolset: Arc<crate::tools::NativeToolset>,
        native_count: Arc<std::sync::atomic::AtomicU32>,
        permission_engine: Option<Arc<crate::permission::PermissionEngine>>,
        truncator: Option<Arc<crate::tool_result_truncation::ToolResultTruncator>>,
    ) -> Self {
        Self {
            inner,
            toolset,
            native_count,
            truncator,
            permission_engine,
            plan_guard: None,
            stale_guard: None,
            goal_guard: None,
            hook_guard: None,
            agent_tool_veto: None,
            tools_veto: None,
            todo_tool_veto: None,
            tower_worktree_root: None,
            tower_enabled: true,
            notify_user_enabled: true,
            sandbox_policy: None,
        }
    }
}

impl HostCallbacks for NativeToolCallbacks {
    fn hook_guard(&self) -> Option<Arc<crate::tools::external_hooks::HookGuard>> {
        self.hook_guard.clone()
    }

    /// Arm native file-history capture on the wrapped toolset. This is the
    /// production wiring point: `write` / `edit` record a
    /// `session_file_history` row only when a recorder is installed, and
    /// without this call `session_file_history` stayed empty, so
    /// `/file-history/*` always answered `[]` and `undo {revert_files:true}`
    /// reported success without restoring anything.
    fn set_file_history(
        &self,
        store: Arc<crate::session::sqlite_store::SqliteSessionStore>,
        session_id: String,
        turn_id: usize,
    ) {
        self.toolset.set_file_history(store, session_id, turn_id);
    }

    fn llm_chat(
        &self,
        request: LlmChatRequest,
    ) -> BoxFuture<'static, Result<LlmChatResponse, String>> {
        self.inner.llm_chat(request)
    }

    fn execute_tool(
        &self,
        request: ToolExecuteRequest,
    ) -> BoxFuture<'static, Result<ToolExecuteResponse, String>> {
        let this = NativeToolCallbacks {
            inner: self.inner.clone(),
            toolset: self.toolset.clone(),
            native_count: self.native_count.clone(),
            truncator: self.truncator.clone(),
            permission_engine: self.permission_engine.clone(),
            plan_guard: self.plan_guard.clone(),
            stale_guard: self.stale_guard.clone(),
            goal_guard: self.goal_guard.clone(),
            hook_guard: self.hook_guard.clone(),
            agent_tool_veto: self.agent_tool_veto.clone(),
            tools_veto: self.tools_veto.clone(),
            todo_tool_veto: self.todo_tool_veto.clone(),
            tower_worktree_root: self.tower_worktree_root.clone(),
            tower_enabled: self.tower_enabled,
            notify_user_enabled: self.notify_user_enabled,
            sandbox_policy: self.sandbox_policy.clone(),
        };
        Box::pin(async move {
            // G-6 #7: a CreateGoal that must be reviewed (permission mode is
            // not `auto`) runs on the host, whose full veto chain — goal-start
            // review included — then applies. Treated exactly like a tool the
            // sandbox cannot handle: host executes, observations still apply.
            if !this.toolset.handles(&request.tool_name)
                || this
                    .goal_guard
                    .as_ref()
                    .is_some_and(|g| g.requires_host(&request.tool_name))
            {
                let response = this.inner.execute_tool(request.clone()).await?;
                // v2 `observeExecution` covers host-served read/writes too —
                // recording here keeps a later native Write to this file
                // from tripping on a read the host path served.
                if let Some(gate) = &this.stale_guard {
                    gate.observe(&request.tool_name, &request.arguments, response.is_error);
                }
                return Ok(response);
            }
            if let Some(guard) = &this.plan_guard
                && let Some(reason) = guard(&request.tool_name, &request.arguments).await
            {
                // The refusal is the tool result the model sees — report it so
                // the host transcript records the card's terminal state too
                // (same contract as the permission denial below).
                this.inner.emit_event(serde_json::json!({
                    "type": "tool.native",
                    "agent_id": request.agent_id,
                    "turn_id": request.turn_id,
                    "tool_call_id": request.tool_call_id,
                    "tool_name": request.tool_name,
                    "arguments": request.arguments,
                    "content": reason,
                    "is_error": true,
                    "note": null,
                }));
                return Ok(ToolExecuteResponse {
                    delivery: None,
                    stop_turn: false,
                    content: reason,
                    is_error: true,
                    note: None,
                });
            }
            // P52 veto gate, before permission — the host veto chain outranks
            // everything (v2 `beforeToolExecuteEvent` aggregation). A denial
            // is the tool result the model sees; no execution, no fallback.
            let veto_reason = if let Some(reason) = &this.tools_veto {
                Some(reason.clone())
            } else if let Some(reason) = &this.agent_tool_veto
                && request.tool_name.eq_ignore_ascii_case("agent")
            {
                Some(reason.clone())
            } else if let Some(reason) = &this.todo_tool_veto
                && (request.tool_name.eq_ignore_ascii_case("todolist")
                    || request.tool_name.eq_ignore_ascii_case("todo_list"))
            {
                Some(reason.clone())
            } else if let Some(worktree) = &this.tower_worktree_root
                && (request.tool_name.eq_ignore_ascii_case("write")
                    || request.tool_name.eq_ignore_ascii_case("edit"))
            {
                if let Some(target_path) = request.arguments.get("path").and_then(|v| v.as_str()) {
                    let norm_target = target_path.replace('\\', "/");
                    let norm_worktree = worktree.replace('\\', "/");
                    if !norm_target.starts_with(&norm_worktree) {
                        Some(format!(
                            "tower workers may only write inside their own worktree ({}) — denied: {}. Out-of-scope changes are not yours to make: file them with TowerFinding or ask the tower via TowerSend.",
                            worktree, target_path
                        ))
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            };
            if let Some(reason) = veto_reason {
                this.inner.emit_event(serde_json::json!({
                    "type": "tool.native",
                    "agent_id": request.agent_id,
                    "turn_id": request.turn_id,
                    "tool_call_id": request.tool_call_id,
                    "tool_name": request.tool_name,
                    "arguments": request.arguments,
                    "content": reason,
                    "is_error": true,
                    "note": null,
                }));
                return Ok(ToolExecuteResponse {
                    delivery: None,
                    stop_turn: false,
                    content: reason,
                    is_error: true,
                    note: None,
                });
            }
            let sandbox_denial = if let Some(policy) = &this.sandbox_policy {
                let tool_lower = request.tool_name.to_ascii_lowercase();
                if tool_lower == "write" || tool_lower == "edit" {
                    if let Some(target_path) =
                        request.arguments.get("path").and_then(|v| v.as_str())
                    {
                        policy.sandbox_write_guard(target_path)
                    } else if policy.mode == crate::tools::sandbox::SandboxMode::ReadOnly {
                        policy.sandbox_write_guard("")
                    } else {
                        None
                    }
                // Bash is code execution *and* a filesystem writer. A ReadOnly
                // sandbox must not let a shell bypass the write guard by
                // running `echo x > file`, and a workspace-write sandbox must
                // confine both the working directory and any absolute write
                // target mentioned in the command (redirection, `tee`, `rm`,
                // `cd …`, …). SandboxGuard's own contract (tools/sandbox.rs)
                // scopes the execution guard to any tool that runs commands,
                // not just the (dead) run_code entry.
                } else if tool_lower == "bash" {
                    let cwd = request.arguments.get("cwd").and_then(|v| v.as_str());
                    policy.sandbox_code_execution_guard_for(cwd).or_else(|| {
                        request
                            .arguments
                            .get("command")
                            .and_then(|v| v.as_str())
                            .and_then(|command| policy.sandbox_bash_write_guard(command))
                    })
                } else if tool_lower == "run_code" {
                    policy.sandbox_code_execution_guard()
                } else {
                    None
                }
            } else {
                None
            };
            if let Some(reason) = sandbox_denial {
                this.inner.emit_event(serde_json::json!({
                    "type": "tool.native",
                    "agent_id": request.agent_id,
                    "turn_id": request.turn_id,
                    "tool_call_id": request.tool_call_id,
                    "tool_name": request.tool_name,
                    "arguments": request.arguments,
                    "content": reason,
                    "is_error": true,
                    "note": null,
                }));
                return Ok(ToolExecuteResponse {
                    delivery: None,
                    stop_turn: false,
                    content: reason,
                    is_error: true,
                    note: None,
                });
            }
            let decision = if let Some(ref engine) = this.permission_engine {
                let verdict = engine.evaluate(&request.tool_name, &request.arguments);
                match verdict.decision {
                    crate::permission::VerdictDecision::Allow => PermissionDecision::allow(),
                    crate::permission::VerdictDecision::Deny => {
                        PermissionDecision::deny(verdict.reason.unwrap_or_else(|| {
                            LocalizedText::new("engine.permission.deniedByLocalPolicy").render()
                        }))
                    }
                    crate::permission::VerdictDecision::Ask => {
                        // v2 `PermissionApprovalRequested` (fire-and-forget): the
                        // request edge of an interactive approval.
                        if let Some(ref guard) = this.hook_guard {
                            guard
                                .notify_permission_request(
                                    &request.tool_name,
                                    &request.tool_call_id,
                                    &request.turn_id,
                                    &request.arguments,
                                    verdict.reason.as_deref(),
                                )
                                .await;
                        }
                        let decision = this
                            .inner
                            .check_permission(PermissionCheckRequest {
                                tool_name: request.tool_name.clone(),
                                // The rule that matched is what a session approval
                                // would remember. Without it the host can only offer
                                // to allow the whole tool for the session, which
                                // grants far more than the rule the user wrote.
                                session_approval_rule: verdict.session_approval_rule.clone(),
                                tool_call_id: request.tool_call_id.clone(),
                                turn_id: request.turn_id.clone(),
                                arguments: request.arguments.clone(),
                                // Carry the policy's explanation to the host so
                                // the approval prompt can say *why* it is asking
                                // ("access to sensitive file …") instead of only
                                // naming the tool. Localized on the engine side.
                                reason: verdict.reason.clone(),
                            })
                            .await?;
                        // "Approve for this session" is recorded here, on the
                        // engine's own list: a host that keeps no list of its
                        // own (or a UI with no host at all) still stops asking.
                        if decision.is_allow()
                            && decision.remember
                            && let Some(ref rule) = verdict.session_approval_rule
                        {
                            engine.remember_session_approval(rule);
                        }
                        decision
                    }
                }
            } else {
                // The host owns this tool entirely: the Ask edge still exists
                // (the host will prompt), so the hook event fires with a
                // `None` reason, mirroring the local-engine ask path.
                if let Some(ref guard) = this.hook_guard {
                    guard
                        .notify_permission_request(
                            &request.tool_name,
                            &request.tool_call_id,
                            &request.turn_id,
                            &request.arguments,
                            None,
                        )
                        .await;
                }
                this.inner
                    .check_permission(PermissionCheckRequest {
                        tool_name: request.tool_name.clone(),
                        session_approval_rule: None,
                        tool_call_id: request.tool_call_id.clone(),
                        turn_id: request.turn_id.clone(),
                        arguments: request.arguments.clone(),
                        // The host owns this tool, so no local policy ran and
                        // there is no engine-side explanation to carry.
                        reason: None,
                    })
                    .await?
            };
            // v2 `PermissionApprovalResolved`: the decision edge, with the
            // allow/deny verdict mapped to upstream's approved/rejected.
            if let Some(ref guard) = this.hook_guard {
                guard
                    .notify_permission_result(
                        &request.tool_name,
                        &request.tool_call_id,
                        &request.turn_id,
                        &request.arguments,
                        if decision.is_allow() {
                            "approved"
                        } else {
                            "rejected"
                        },
                        decision.reason.as_deref(),
                    )
                    .await;
            }
            if !decision.is_allow() {
                let reason = decision.reason.unwrap_or_else(|| {
                    LocalizedText::new("engine.permission.deniedByHostPermission").render()
                });
                // v2 `formatDenyMessage` / `usesWorkerRejectionGuidance`: a
                // non-main agent's rejection appends the retry guidance — a
                // worker cannot ask the user, so it must change approach.
                let reason = if crate::tools::CALLER_AGENT_ID
                    .try_with(|id| id != "main")
                    .unwrap_or(false)
                {
                    format!(
                        "{reason} Try a different approach — don't retry the same call, don't attempt to bypass the restriction."
                    )
                } else {
                    reason
                };
                // The refusal is the tool result the model sees — report it so
                // the host transcript records the card's terminal state too.
                this.inner.emit_event(serde_json::json!({
                    "type": "tool.native",
                    "agent_id": request.agent_id,
                    "turn_id": request.turn_id,
                    "tool_call_id": request.tool_call_id,
                    "tool_name": request.tool_name,
                    "arguments": request.arguments,
                    "content": reason,
                    "is_error": true,
                    "note": null,
                }));
                return Ok(ToolExecuteResponse {
                    delivery: None,
                    stop_turn: false,
                    content: reason,
                    is_error: true,
                    note: None,
                });
            }
            // PreToolUse hooks (v2 `agentExternalHooksService`, G-6 #6),
            // after permission and plan — v2's chain order. A block is the
            // tool result the model sees (fail-closed on hook errors); no
            // execution, no host fallback.
            if let Some(guard) = &this.hook_guard
                && let Some(reason) = guard.denial(&request).await
            {
                this.inner.emit_event(serde_json::json!({
                    "type": "tool.native",
                    "agent_id": request.agent_id,
                    "turn_id": request.turn_id,
                    "tool_call_id": request.tool_call_id,
                    "tool_name": request.tool_name,
                    "arguments": request.arguments,
                    "content": reason,
                    "is_error": true,
                    "note": null,
                }));
                return Ok(ToolExecuteResponse {
                    delivery: None,
                    stop_turn: false,
                    content: reason,
                    is_error: true,
                    note: None,
                });
            }
            // Goal-operation stale veto (G-6 #8), after permission and
            // before the stale-write guard: a goal
            // mutation call from a turn whose goal changed is the tool
            // result the model sees; no execution, no host fallback.
            if let Some(guard) = &this.goal_guard
                && let Some(reason) = guard
                    .stale_denial(this.inner.as_ref(), &request.turn_id, &request.tool_name)
                    .await
            {
                this.inner.emit_event(serde_json::json!({
                    "type": "tool.native",
                    "agent_id": request.agent_id,
                    "turn_id": request.turn_id,
                    "tool_call_id": request.tool_call_id,
                    "tool_name": request.tool_name,
                    "arguments": request.arguments,
                    "content": reason,
                    "is_error": true,
                    "note": null,
                }));
                return Ok(ToolExecuteResponse {
                    delivery: None,
                    stop_turn: false,
                    content: reason,
                    is_error: true,
                    note: None,
                });
            }
            // Stale-write guard (fork-original, G-6 #3), after the
            // permission verdict and before execution — the chain order is
            // permission → plan → stale guard. A denial is the tool result
            // the model sees; no execution, no host fallback.
            if let Some(gate) = &this.stale_guard
                && let Some(reason) = gate
                    .denial(this.inner.as_ref(), &request.tool_name, &request.arguments)
                    .await
            {
                this.inner.emit_event(serde_json::json!({
                    "type": "tool.native",
                    "agent_id": request.agent_id,
                    "turn_id": request.turn_id,
                    "tool_call_id": request.tool_call_id,
                    "tool_name": request.tool_name,
                    "arguments": request.arguments,
                    "content": reason,
                    "is_error": true,
                    "note": null,
                }));
                return Ok(ToolExecuteResponse {
                    delivery: None,
                    stop_turn: false,
                    content: reason,
                    is_error: true,
                    note: None,
                });
            }
            // A host that virtualizes the filesystem (ACP clients advertising
            // `fs.*`) owns Read/Write: after the veto/plan/permission/stale
            // gates, run the tool through the host instead of the native
            // sandbox. The completed host execution is observed so a later
            // native Write does not trip the stale guard.
            if this.inner.owns_tool(&request.tool_name) {
                let response = this.inner.execute_tool(request.clone()).await?;
                if let Some(gate) = &this.stale_guard {
                    gate.observe(&request.tool_name, &request.arguments, response.is_error);
                }
                this.inner.emit_event(serde_json::json!({
                    "type": "tool.native",
                    "agent_id": request.agent_id,
                    "turn_id": request.turn_id,
                    "tool_call_id": request.tool_call_id,
                    "tool_name": request.tool_name,
                    "arguments": request.arguments,
                    "content": response.content,
                    "is_error": response.is_error,
                    "note": response.note,
                }));
                return Ok(response);
            }
            // P53 checkpoint prepare (v2 `onWillExecuteTool` counterpart):
            // the engine is about to write these files — the host captures
            // their pre-images before the write lands. Fail-open: a failure
            // or unwired host skips the snapshot. At this point every deny
            // gate (veto / plan / permission / hook / stale) has passed.
            let checkpoint_paths = checkpoint_write_paths(&request.tool_name, &request.arguments);
            if !checkpoint_paths.is_empty() {
                let _ = this
                    .inner
                    .checkpoint(CheckpointRequest {
                        turn_id: request.turn_id.clone(),
                        tool_call_id: request.tool_call_id.clone(),
                        phase: "prepare".into(),
                        paths: checkpoint_paths.clone(),
                        executed: false,
                    })
                    .await;
            }
            let started = std::time::Instant::now();
            // P57 tool.progress stream: bash output chunks flow to the host
            // as `tool.native.progress` events (fire-and-forget UI updates).
            let progress_inner = this.inner.clone();
            let progress_agent_id = request.agent_id.clone();
            let progress_turn_id = request.turn_id.clone();
            let progress_call_id = request.tool_call_id.clone();
            let on_update = |kind: &str, text: &str| {
                progress_inner.emit_event(serde_json::json!({
                    "type": "tool.native.progress",
                    "agent_id": progress_agent_id,
                    "turn_id": progress_turn_id,
                    "tool_call_id": progress_call_id,
                    "kind": kind,
                    "text": text,
                }));
            };
            let result = this
                .toolset
                .execute_tool_streaming(
                    Some(&request.tool_call_id),
                    &request.tool_name,
                    &request.arguments,
                    Some(&on_update),
                )
                .await;
            match result {
                Some(result) => {
                    this.native_count.fetch_add(1, Ordering::Relaxed);
                    // Self-write refresh + read recording: re-stat after the
                    // completed execution (v2 `observeExecution`), so
                    // consecutive writes never trip the guard.
                    if let Some(gate) = &this.stale_guard {
                        gate.observe(&request.tool_name, &request.arguments, result.is_error);
                    }
                    // P53 checkpoint record (v2 `onDidExecuteTool` counter-
                    // part): the write landed — note the post-image so undo
                    // can detect manual edits. Fire-and-forget: a failure
                    // leaves the group with an unresolvable after-state,
                    // which restore treats as a conflict, not data loss.
                    if !checkpoint_paths.is_empty() {
                        let _ = this
                            .inner
                            .checkpoint(CheckpointRequest {
                                turn_id: request.turn_id.clone(),
                                tool_call_id: request.tool_call_id.clone(),
                                phase: "record".into(),
                                paths: checkpoint_paths,
                                executed: true,
                            })
                            .await;
                    }
                    // P54 tool_call telemetry (v2 `trackToolCall` counter-
                    // part): outcome + duration for every native execution.
                    // `dup_type` is always `normal` — dedupe-supplied repeats
                    // never reach the execution layer. Field set must stay
                    // exactly the v2 `ToolCallEvent` shape (strict telemetry
                    // property check host-side).
                    let mut telemetry_event = serde_json::json!({
                        "event": "tool_call",
                        "turn_id": request.turn_id.parse::<u64>().unwrap_or(0),
                        "tool_call_id": request.tool_call_id,
                        "tool_name": request.tool_name,
                        "outcome": if result.is_error { "error" } else { "success" },
                        "duration_ms": started.elapsed().as_millis() as u64,
                        "dup_type": "normal",
                    });
                    if result.is_error {
                        telemetry_event["error_type"] = serde_json::Value::String("error".into());
                    }
                    this.inner.telemetry(telemetry_event);
                    if let Some(guard) = &this.hook_guard {
                        guard
                            .notify_post_tool_use(crate::tools::external_hooks::PostToolUse {
                                agent_id: &request.agent_id,
                                turn_id: &request.turn_id,
                                tool_name: &request.tool_name,
                                tool_call_id: &request.tool_call_id,
                                args: &request.arguments,
                                content: &result.content,
                                is_error: result.is_error,
                            })
                            .await;
                    }
                    let raw = ToolExecuteResponse {
                        delivery: result.delivery,
                        stop_turn: result.stop_turn,
                        content: result.content,
                        is_error: result.is_error,
                        note: result.note,
                    };
                    // The local truncator runs the result policy in-process
                    // (truncate + spill to `<workspace>/.kimi/spill`). The TS
                    // host still receives the *truncated* text via emit_event
                    // so its transcript shows what the model saw.
                    let finalized = match this.truncator.as_ref() {
                        Some(truncator) => {
                            let f = truncator.truncate_call(
                                &request.arguments,
                                crate::tool_result_truncation::TruncationRequest {
                                    tool_name: &request.tool_name,
                                    tool_call_id: &request.tool_call_id,
                                    content: &raw.content,
                                    is_error: raw.is_error,
                                    note: raw.note.as_deref(),
                                },
                            );
                            ToolExecuteResponse {
                                delivery: raw.delivery,
                                stop_turn: raw.stop_turn,
                                content: f.content,
                                is_error: f.is_error,
                                note: f.note,
                            }
                        }
                        None => raw,
                    };
                    // The request's own `agent_id`, never the
                    // `CALLER_AGENT_ID` task-local: the owner is carried
                    // across the scheduler's `tokio::spawn`, the task-local is
                    // not (`turn_loop/types.rs:864-866`), and v2 threads the
                    // same per-scope agent id into the tool executor
                    // (`toolExecutorService.ts:578-588`).
                    this.inner.emit_event(serde_json::json!({
                        "type": "tool.native",
                        "agent_id": request.agent_id,
                        "turn_id": request.turn_id,
                        "tool_call_id": request.tool_call_id,
                        "tool_name": request.tool_name,
                        "arguments": request.arguments,
                        "content": finalized.content,
                        "is_error": finalized.is_error,
                        "note": finalized.note,
                    }));
                    Ok(finalized)
                }
                // Sandbox escape or unrecognized argument shape — the host
                // already allowed the call, so run it there. The completed
                // host execution is observed like any other, so a later
                // native Write to this file doesn't trip on it.
                None => {
                    let response = this.inner.execute_tool(request.clone()).await?;
                    if let Some(gate) = &this.stale_guard {
                        gate.observe(&request.tool_name, &request.arguments, response.is_error);
                    }
                    Ok(response)
                }
            }
        })
    }

    fn check_permission(
        &self,
        request: PermissionCheckRequest,
    ) -> BoxFuture<'static, Result<PermissionDecision, String>> {
        self.inner.check_permission(request)
    }

    fn ask_question(
        &self,
        request: AskQuestionRequest,
    ) -> BoxFuture<'static, Result<AskQuestionResponse, String>> {
        self.inner.ask_question(request)
    }

    fn state_read(
        &self,
        request: StateReadRequest,
    ) -> BoxFuture<'static, Result<StateReadResponse, String>> {
        self.inner.state_read(request)
    }

    fn state_write(
        &self,
        request: StateWriteRequest,
    ) -> BoxFuture<'static, Result<StateWriteResponse, String>> {
        self.inner.state_write(request)
    }

    fn checkpoint_state(&self) -> BoxFuture<'static, Result<(), String>> {
        self.inner.checkpoint_state()
    }

    /// The tool table the model is offered: the engine-owned native
    /// definitions (the toolset executes those calls in-process) merged with
    /// the host's own table and MCP-discovered tools. A missing host table is
    /// normal on the napi path — the engine table then stands alone instead
    /// of leaving the model with no tools at all.
    fn list_tools(&self) -> BoxFuture<'static, Result<ListToolsResponse, String>> {
        let inner = self.inner.clone();
        let toolset = self.toolset.clone();
        let tower_enabled = self.tower_enabled;
        let notify_user_enabled = self.notify_user_enabled;
        let mcp_mgr = self.toolset.mcp_manager().cloned();
        Box::pin(async move {
            let filter = toolset.tools_filter().cloned();
            let mut tools = crate::tools::tool_policy::native_tool_defs_with_notify(
                toolset.github_credentials().is_some(),
                tower_enabled,
                notify_user_enabled,
                filter.as_ref(),
                toolset.secondary_model().map(|pool| pool.as_ref()),
            );
            let mut seen: std::collections::HashSet<String> =
                tools.iter().map(|t| t.name.clone()).collect();
            if let Ok(response) = inner.list_tools().await {
                for tool in response.tools {
                    if seen.insert(tool.name.clone()) {
                        tools.push(tool);
                    }
                }
            }
            if let Some(mcp) = mcp_mgr {
                // v2 awaits MCP readiness at `onWillBeginStep`
                // (mcpService.ts:70-73): the tool table must not be assembled
                // before the servers have answered, or the model would be
                // offered a tool list that silently omits them. The wait is
                // free once the initial load has settled — which it usually
                // has, because the manager is process-wide and its connect
                // started when the first session was built.
                mcp.wait_for_initial_load().await;
                for tool in mcp.list_tool_infos().await {
                    if seen.insert(tool.name.clone()) {
                        tools.push(tool);
                    }
                }
            }
            if let Some(filter) = &filter {
                tools.retain(|tool| filter.allows(&tool.name));
            }
            // v2 #4720 / #4719: the initial registry snapshot is the table
            // after MCP's first discovery has settled. Pin it once; without
            // dynamic loading, later requests reconcile the live table into
            // these names, so a runtime-only addition is not advertised as an
            // unannounced top-level tool.
            toolset.record_initial_tool_table_ref(&tools);
            // Rewrite the Agent tool's per-profile `Tools:` lines against the
            // table that actually exists: `native_tool_defs` rendered them from
            // each profile's static allowlist, before the host and MCP tables
            // merged, so a profile could advertise tools this session cannot
            // call (a disconnected MCP server, a name the global switch
            // disabled, GitHub tools with no token). The set is deliberately
            // pre-shape — a deferred MCP tool stays loadable via `select_tools`,
            // so it still counts as available.
            let available: std::collections::HashSet<String> =
                tools.iter().map(|tool| tool.name.clone()).collect();
            if let Some(slot) = tools.iter_mut().find(|tool| tool.name == "Agent") {
                let is_available = |pattern: &str| {
                    available.iter().any(|name| {
                        crate::tools::tool_policy::matches_profile_tool_pattern(pattern, name)
                    })
                };
                // v2's allowlist-filtered listing and background gate are
                // caller-relative: the live table must offer all three Task
                // tools, and the caller's own profile must enable them too.
                let caller_id = crate::tools::CALLER_AGENT_ID
                    .try_with(|id| id.clone())
                    .unwrap_or_else(|_| crate::callbacks::MAIN_AGENT_ID.to_string());
                let (spawn_catalog, background_available) = match toolset.subagent_manager() {
                    Some(manager) => {
                        let catalog = manager.spawn_catalog(&caller_id).await;
                        let available =
                            crate::tools::agent_tool::background_agent_available_for_caller(
                                manager,
                                &caller_id,
                                &is_available,
                            )
                            .await;
                        (catalog, available)
                    }
                    None => (None, false),
                };
                *slot = crate::tools::agent_tool::agent_tool_def_filtered_with_background(
                    toolset.secondary_model().map(|pool| pool.as_ref()),
                    &is_available,
                    spawn_catalog.as_ref(),
                    background_available,
                );
            }
            // Progressive tool disclosure (v2 `shapeTools`): with the
            // `tool_select` switch on and the model declaring
            // `dynamically_loaded_tools`, deferred MCP tools leave the table
            // and `select_tools` is advertised instead.
            let tools = toolset.shape_tool_table(tools);
            Ok(ListToolsResponse { tools })
        })
    }

    fn goal(&self) -> BoxFuture<'static, Result<Option<GoalContext>, String>> {
        self.inner.goal()
    }

    fn parse_bash(
        &self,
        request: ParseBashRequest,
    ) -> BoxFuture<'static, Result<Option<Vec<String>>, String>> {
        self.inner.parse_bash(request)
    }

    fn supports_parse_bash(&self) -> bool {
        self.inner.supports_parse_bash()
    }

    fn auth_token(
        &self,
        provider: String,
        force: bool,
    ) -> BoxFuture<'static, Result<String, String>> {
        self.inner.auth_token(provider, force)
    }

    fn set_turn_goal(&self, turn_id: &str, goal_id: Option<&str>) {
        if let Some(guard) = &self.goal_guard {
            guard.bind_turn(turn_id, goal_id);
        }
    }

    fn emit_event(&self, event: serde_json::Value) {
        self.inner.emit_event(event);
    }

    fn turn_event(&self, event: crate::turn_events::TurnEvent) {
        self.inner.turn_event(event);
    }

    fn telemetry(&self, event: serde_json::Value) {
        self.inner.telemetry(event);
    }
}

/// A [`HostCallbacks`] decorator that counts fire-and-forget events
/// forwarded through it, so the turn result can carry an event-count
/// telemetry figure.
///
/// The composition roots (stdio CLI, napi addon) wrap the base callbacks
/// with this **before** assembling the native tool wrapper and the native
/// LLM event sink, so every event path — step lifecycle, deltas, native
/// tools, goal budget limits — is counted exactly once.
pub struct CountingCallbacks {
    pub inner: Arc<dyn HostCallbacks>,
    pub event_count: Arc<AtomicU32>,
    /// Optional in-process event bus (P26 batch 5) for decoupled in-Rust event consumers.
    pub bus: Option<Arc<crate::events::EventBus>>,
    /// Installed once the pipeline has built its task runner. The slot lets the
    /// outermost callback observe native and host-proxy subagent streams alike.
    pub subagent_task_runner: Arc<std::sync::Mutex<Option<Arc<crate::storage::TaskRunner>>>>,
}

impl CountingCallbacks {
    pub fn new(inner: Arc<dyn HostCallbacks>, event_count: Arc<AtomicU32>) -> Self {
        Self {
            inner,
            event_count,
            bus: None,
            subagent_task_runner: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    pub fn with_bus(mut self, bus: Arc<crate::events::EventBus>) -> Self {
        self.bus = Some(bus);
        self
    }

    pub fn with_subagent_task_runner_slot(
        mut self,
        slot: Arc<std::sync::Mutex<Option<Arc<crate::storage::TaskRunner>>>>,
    ) -> Self {
        self.subagent_task_runner = slot;
        self
    }

    fn observe_subagent_event(&self, event: &serde_json::Value) {
        let runner = self
            .subagent_task_runner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        if let Some(runner) = runner {
            runner.observe_subagent_event(event);
        }
    }

    fn observe_subagent_telemetry(&self, event: &serde_json::Value) {
        let runner = self
            .subagent_task_runner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        if let Some(runner) = runner {
            runner.observe_subagent_telemetry(event);
        }
    }
}

impl HostCallbacks for CountingCallbacks {
    fn hook_guard(&self) -> Option<Arc<crate::tools::external_hooks::HookGuard>> {
        self.inner.hook_guard()
    }

    fn llm_chat(
        &self,
        request: LlmChatRequest,
    ) -> BoxFuture<'static, Result<LlmChatResponse, String>> {
        self.inner.llm_chat(request)
    }

    fn execute_tool(
        &self,
        request: ToolExecuteRequest,
    ) -> BoxFuture<'static, Result<ToolExecuteResponse, String>> {
        self.inner.execute_tool(request)
    }

    fn owns_tool(&self, tool_name: &str) -> bool {
        self.inner.owns_tool(tool_name)
    }

    fn check_permission(
        &self,
        request: PermissionCheckRequest,
    ) -> BoxFuture<'static, Result<PermissionDecision, String>> {
        self.inner.check_permission(request)
    }

    fn ask_question(
        &self,
        request: AskQuestionRequest,
    ) -> BoxFuture<'static, Result<AskQuestionResponse, String>> {
        self.inner.ask_question(request)
    }

    fn state_read(
        &self,
        request: StateReadRequest,
    ) -> BoxFuture<'static, Result<StateReadResponse, String>> {
        self.inner.state_read(request)
    }

    fn state_write(
        &self,
        request: StateWriteRequest,
    ) -> BoxFuture<'static, Result<StateWriteResponse, String>> {
        self.inner.state_write(request)
    }

    fn checkpoint(&self, request: CheckpointRequest) -> BoxFuture<'static, Result<(), String>> {
        self.inner.checkpoint(request)
    }

    fn checkpoint_state(&self) -> BoxFuture<'static, Result<(), String>> {
        self.inner.checkpoint_state()
    }

    fn list_tools(&self) -> BoxFuture<'static, Result<ListToolsResponse, String>> {
        self.inner.list_tools()
    }

    fn goal(&self) -> BoxFuture<'static, Result<Option<GoalContext>, String>> {
        self.inner.goal()
    }

    fn parse_bash(
        &self,
        request: ParseBashRequest,
    ) -> BoxFuture<'static, Result<Option<Vec<String>>, String>> {
        self.inner.parse_bash(request)
    }

    fn supports_parse_bash(&self) -> bool {
        self.inner.supports_parse_bash()
    }

    fn auth_token(
        &self,
        provider: String,
        force: bool,
    ) -> BoxFuture<'static, Result<String, String>> {
        self.inner.auth_token(provider, force)
    }

    fn emit_event(&self, event: serde_json::Value) {
        self.event_count.fetch_add(1, Ordering::Relaxed);
        self.observe_subagent_event(&event);
        if let Some(ref bus) = self.bus {
            bus.publish_json(event.clone());
            // Web-vocabulary aliases ride the same lane (server-side only —
            // a private bus has no listeners). The originals stay untouched:
            // they are the TUI/napi transcript contract.
            for alias in crate::server::web_events::web_event_aliases(&event) {
                bus.publish_json(alias);
            }
        }
        self.inner.emit_event(event);
    }

    fn turn_event(&self, event: crate::turn_events::TurnEvent) {
        // Not counted: `event_count` reports content events (deltas, tool
        // results) as a per-turn overhead figure, and lifecycle records are a
        // fixed four per turn regardless of how much work the turn did.
        if let Some(bus) = &self.bus
            && let Ok(payload) = serde_json::to_value(&event)
        {
            bus.publish_json(payload);
        }
        self.inner.turn_event(event);
    }

    fn telemetry(&self, event: serde_json::Value) {
        // Not counted, same reasoning as `turn_event`: fixed three events per
        // turn (started / [interrupted] / ended), not overhead telemetry.
        self.observe_subagent_telemetry(&event);
        if let Some(ref bus) = self.bus {
            bus.publish_json(event.clone());
        }
        self.inner.telemetry(event);
    }
}

/// Host callbacks adapter that serves state bridge calls (`state_read`, `state_write`, `checkpoint`)
/// using a local [`crate::storage::StateStore`].
pub struct StateStoreCallbacks {
    pub inner: Arc<dyn HostCallbacks>,
    /// The session's state store (v2 agent scope): todo / plan / goal /
    /// task / turn. Checkpoints and goal context read from it.
    pub store: Arc<crate::storage::StateStore>,
    /// The workspace store, kept for the one workspace-level domain: `cron`.
    pub cron_store: Arc<crate::storage::StateStore>,
}

impl HostCallbacks for StateStoreCallbacks {
    fn hook_guard(&self) -> Option<Arc<crate::tools::external_hooks::HookGuard>> {
        self.inner.hook_guard()
    }

    fn llm_chat(
        &self,
        request: LlmChatRequest,
    ) -> BoxFuture<'static, Result<LlmChatResponse, String>> {
        self.inner.llm_chat(request)
    }

    fn execute_tool(
        &self,
        request: ToolExecuteRequest,
    ) -> BoxFuture<'static, Result<ToolExecuteResponse, String>> {
        self.inner.execute_tool(request)
    }

    fn owns_tool(&self, tool_name: &str) -> bool {
        self.inner.owns_tool(tool_name)
    }

    fn check_permission(
        &self,
        request: PermissionCheckRequest,
    ) -> BoxFuture<'static, Result<PermissionDecision, String>> {
        self.inner.check_permission(request)
    }

    fn ask_question(
        &self,
        request: AskQuestionRequest,
    ) -> BoxFuture<'static, Result<AskQuestionResponse, String>> {
        self.inner.ask_question(request)
    }

    fn state_read(
        &self,
        request: StateReadRequest,
    ) -> BoxFuture<'static, Result<StateReadResponse, String>> {
        let session_store = self.store.clone();
        let workspace_store = self.cron_store.clone();
        let legacy_store = workspace_store.clone();
        let inner = self.inner.clone();
        Box::pin(async move {
            let store = if request.domain == "cron" {
                workspace_store
            } else {
                session_store
            };
            // Plan mode and goal are host-owned live session state. Consult
            // the host first; fall back to the local store only when the host
            // cannot answer (standalone REPL, or a host that has not wired the
            // state bridge).
            if request.domain == "plan" || request.domain == "goal" {
                if let Ok(resp) = inner.state_read(request.clone()).await {
                    return Ok(resp);
                }
                return store
                    .read_state(&request.domain, &request.key)
                    .map(|value| StateReadResponse { value });
            }
            // Engine-owned domains (todo / cron / turn, and the task
            // *list*) are authored by native tools against the local store, so
            // it is the read/write authority and the host is the fallback.
            // Task *output* (a task-id key) is host-owned live data and skips
            // the local list.
            if let Some(value) = (request.domain != "task" || request.key == "task")
                .then(|| store.read_domain(&request.domain))
                .flatten()
            {
                return Ok(StateReadResponse { value });
            }
            // Pre-scope installs kept every domain in the workspace store; a
            // session that has not written the domain yet still sees that
            // value (the next write moves it into the session store). Cron is
            // already the workspace store above.
            if request.domain != "cron"
                && let Some(value) = (request.domain != "task" || request.key == "task")
                    .then(|| legacy_store.read_domain(&request.domain))
                    .flatten()
            {
                return Ok(StateReadResponse { value });
            }
            match inner.state_read(request.clone()).await {
                Ok(resp) => Ok(resp),
                Err(host_err) => match store.read_state(&request.domain, &request.key) {
                    Ok(value) => Ok(StateReadResponse { value }),
                    Err(store_err) => {
                        // A domain the local store does not own (v2's
                        // host-owned ones — `skill` today) has no authoritative
                        // local answer, so `-32001 unknown state domain` is a
                        // fact about *this* store, not about the request.
                        // Returning it both hides why the host failed and
                        // defeats the caller's own error mapping: the Skill
                        // tool matches on the "does not support state bridge"
                        // phrase to tell the model to stop calling it.
                        if crate::storage::state_store::STATE_DOMAINS
                            .contains(&request.domain.as_str())
                        {
                            Err(store_err)
                        } else {
                            Err(host_err)
                        }
                    }
                },
            }
        })
    }

    fn state_write(
        &self,
        request: StateWriteRequest,
    ) -> BoxFuture<'static, Result<StateWriteResponse, String>> {
        let session_store = self.store.clone();
        let cron_store = self.cron_store.clone();
        let inner = self.inner.clone();
        Box::pin(async move {
            let store = if request.domain == "cron" {
                cron_store
            } else {
                session_store
            };
            // Plan and goal domains are host-owned: route to host inner first,
            // falling back to local store when the host does not implement it.
            if request.domain == "plan" || request.domain == "goal" {
                if let Ok(resp) = inner.state_write(request.clone()).await {
                    return Ok(resp);
                }
                // The goal lifecycle is host-owned on the native path, but the
                // engine store is where its side effects land. Sync it from the
                // host's live snapshot first, so a host-created goal accepts
                // the model's UpdateGoal / SetGoalBudget and the turn-end usage
                // fold, and a host-side pause / resume / cancel reaches the
                // store the continuation decision reads.
                if request.domain == "goal" {
                    match inner.goal().await {
                        Ok(Some(context)) => {
                            if let Err(error) = store.sync_goal_from_context(&context) {
                                tracing::warn!(%error, "could not sync the local goal from the host snapshot");
                            }
                        }
                        // A host that owns goals and reports none has cleared
                        // its goal (`cancelGoal`); the engine store must follow.
                        Ok(None) => {
                            if let Err(error) = store.clear_host_goal() {
                                tracing::warn!(%error, "could not clear the local goal");
                            }
                        }
                        Err(_) => {}
                    }
                }
                return match store.apply_write(&request.domain, &request.value) {
                    Ok(outcome) => {
                        // A failed persist must fail the call: reporting
                        // `ok: true` after dropping the write told the host the
                        // state was durable when it was not.
                        store.write_domain(&request.domain, &outcome.stored)?;
                        if request.domain == "goal"
                            && outcome
                                .stored
                                .get("goal")
                                .is_some_and(|goal| !goal.is_null())
                        {
                            // v2 `goal.updated`: the full snapshot, so the
                            // host's panel and its per-turn provider see the
                            // engine's folds (usage counters, status
                            // transitions) even for turns that never resolved
                            // a prompt receipt. The persisted value is the
                            // durable `GoalState`; the event carries the
                            // snapshot the host contract expects.
                            inner.emit_event(serde_json::json!({
                                "type": "goal.updated",
                                "domain": "goal",
                                "value": outcome.response,
                            }));
                        }
                        Ok(StateWriteResponse {
                            ok: true,
                            value: outcome.response,
                        })
                    }
                    Err(store_err) => Err(store_err),
                };
            }
            match store.apply_write(&request.domain, &request.value) {
                Ok(outcome) => {
                    store.write_domain(&request.domain, &outcome.stored)?;
                    // State-domain change notification (upstream: the
                    // `IAgentTodoService.onDidChange` / task-service events
                    // the v3 session projector subscribes to). The engine has
                    // no per-domain service, so the write path emits the
                    // stored value itself; the v3 live translator folds it
                    // into a `todo` entity on the session lane.
                    if request.domain == "todo" || request.domain == "task" {
                        inner.emit_event(serde_json::json!({
                            "type": "event.state.changed",
                            "domain": request.domain,
                            "value": outcome.stored,
                        }));
                    }
                    Ok(StateWriteResponse {
                        ok: true,
                        value: outcome.response,
                    })
                }
                Err(store_err) => match inner.state_write(request).await {
                    Ok(resp) => Ok(resp),
                    Err(_) => Err(store_err),
                },
            }
        })
    }

    fn checkpoint(&self, request: CheckpointRequest) -> BoxFuture<'static, Result<(), String>> {
        let store = self.store.clone();
        let inner = self.inner.clone();
        Box::pin(async move {
            let store_res = store.checkpoint();
            // The local StateStore snapshot is the durable undo anchor. The host
            // checkpoint is a secondary, fail-open anchor: the
            // CHECKPOINT_UNSUPPORTED sentinel means the host simply has no
            // checkpoint support (trait default / unwired napi TSFN), so the
            // local result stands. Any *other* host error means the host anchor
            // is incomplete and a later `/undo` could restore the wrong state —
            // surface it rather than swallowing it as before.
            match inner.checkpoint(request).await {
                Ok(()) => store_res,
                Err(err) if err == CHECKPOINT_UNSUPPORTED => store_res,
                Err(err) => Err(err),
            }
        })
    }

    fn checkpoint_state(&self) -> BoxFuture<'static, Result<(), String>> {
        let store = self.store.clone();
        let inner = self.inner.clone();
        Box::pin(async move {
            // The undo anchor is pushed at the *head* of a turn, and a
            // host-owned goal only reaches the store through a goal-domain
            // write (the turn-end fold, a model `UpdateGoal`). A goal the user
            // created after the previous turn therefore was not in the anchor:
            // an undo restored the "no goal" the store still held and the goal
            // vanished from the panel. v2 pushed the same checkpoint against
            // the store its goal writes landed in, so folding the host snapshot
            // in first is what makes this anchor equivalent. Fail-open — a host
            // without the seam (`Err`) leaves the store as it was, and a sync
            // error is logged rather than failing the checkpoint itself.
            match inner.goal().await {
                Ok(Some(context)) => {
                    if let Err(error) = store.sync_goal_from_context(&context) {
                        tracing::warn!(%error, "could not sync the goal before the checkpoint");
                    }
                }
                Ok(None) => {
                    if let Err(error) = store.clear_host_goal() {
                        tracing::warn!(%error, "could not clear the goal before the checkpoint");
                    }
                }
                Err(_) => {}
            }
            store.checkpoint()
        })
    }

    fn list_tools(&self) -> BoxFuture<'static, Result<ListToolsResponse, String>> {
        self.inner.list_tools()
    }

    fn goal(&self) -> BoxFuture<'static, Result<Option<GoalContext>, String>> {
        let store = self.store.clone();
        let legacy_store = self.cron_store.clone();
        let inner = self.inner.clone();
        Box::pin(async move {
            // `Ok(None)` means the host owns goals and has none; `Err` means it
            // has no goal bridge at all, so the local store is the authority
            // (standalone engine / REPL).
            match inner.goal().await {
                Ok(goal) => Ok(goal),
                Err(_) => Ok(store.goal_context().or_else(|| legacy_store.goal_context())),
            }
        })
    }

    fn parse_bash(
        &self,
        request: ParseBashRequest,
    ) -> BoxFuture<'static, Result<Option<Vec<String>>, String>> {
        self.inner.parse_bash(request)
    }

    fn supports_parse_bash(&self) -> bool {
        self.inner.supports_parse_bash()
    }

    fn auth_token(
        &self,
        provider: String,
        force: bool,
    ) -> BoxFuture<'static, Result<String, String>> {
        self.inner.auth_token(provider, force)
    }

    fn drain_steers(&self) -> BoxFuture<'static, Result<Vec<LLMMessage>, String>> {
        self.inner.drain_steers()
    }

    fn set_turn_goal(&self, turn_id: &str, goal_id: Option<&str>) {
        self.inner.set_turn_goal(turn_id, goal_id);
    }

    fn emit_event(&self, event: serde_json::Value) {
        self.inner.emit_event(event);
    }

    fn turn_event(&self, event: crate::turn_events::TurnEvent) {
        self.store.fold_turn_event(&event);
        self.inner.turn_event(event);
    }

    fn telemetry(&self, event: serde_json::Value) {
        self.inner.telemetry(event);
    }

    fn cancel_llm_chat(&self, request_id: &str) {
        self.inner.cancel_llm_chat(request_id);
    }
}

#[cfg(test)]
mod tests;
