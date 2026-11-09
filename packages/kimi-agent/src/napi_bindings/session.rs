//! Engine-session lifecycle: the session registry, per-session TSFN
//! wiring, and the `session_*` napi exports.

use super::*;
use crate::session::{
    Admission, EngineSession, GoalProvider, SessionConfig, ToolDefsProvider, TurnOutcome,
    TurnRequest,
};
use crate::turn_loop::types::*;
use napi::{
    JsObject,
    bindgen_prelude::{Env, JsFunction},
    threadsafe_function::{ErrorStrategy, ThreadSafeCallContext, ThreadsafeFunction},
};
use napi_derive::napi;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use tokio::sync::oneshot;

/// In-flight compaction cancel flags keyed by session id. `session_compact`
/// registers one for the duration of the summarizer call;
/// `session_cancel_compaction` triggers it so a long summarizer request can
/// be aborted from the UI instead of finishing the manual `/compact`.
static COMPACTION_CANCEL: LazyLock<Mutex<HashMap<String, Arc<AtomicBool>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// A live (non-shutdown) session entry in `workspace`, if any.
pub(crate) fn live_session_for_workspace(workspace: &str) -> Option<SessionEntry> {
    let registry = SESSION_REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    registry
        .values()
        .find(|entry| entry.workspace == workspace && !entry.session.is_shutdown())
        .cloned()
}

// ── EngineSession handle (M1d) ─────────────────────────────────────────────
// The napi boundary upgrades from "one call per turn" to a session handle:
// the pipeline is built once, and admission (four modes), the pending FIFO,
// the pump, turn ids, cancellation, and quiescence live engine-side. This is
// the foundation for deleting `executeTurnViaEngine` and flipping turn
// ownership; the JS side addresses sessions by id (the CANCEL_MAP registry
// style — no napi class surface yet).

/// Live sessions keyed by id. One CLI process runs one session today; the
/// registry keeps the surface uniform for tests and future multi-session
/// hosts. `session_dispose` signals the session's pump to stop, so a disposed
/// session's task — and the conversation it holds — is released rather than
/// parked for the life of the process.
static SESSION_REGISTRY: LazyLock<Mutex<HashMap<String, SessionEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Outcome receivers for enqueued turns, keyed by (session, turn). Enqueue
/// stores the receiver; `session_turn_outcome` takes it and resolves the JS
/// promise when the pump finishes the turn.
type SessionOutcomeMap = HashMap<(String, u64), oneshot::Receiver<Result<TurnOutcome, String>>>;

static SESSION_OUTCOMES: LazyLock<Mutex<SessionOutcomeMap>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Sessions whose `session_dispose` is still waiting for the pump to exit.
/// The registry entry is gone by then, so a second `dispose()` would find no
/// session and resolve instantly — while the first is still waiting. That is
/// the same lie the async dispose exists to remove, so the pending wait is
/// remembered here and both callers await the same `released`. Entries are
/// removed as each dispose future ends.
static RELEASE_PENDING: LazyLock<Mutex<HashMap<String, Arc<EngineSession>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static SESSION_NEXT_ID: AtomicU32 = AtomicU32::new(1);

#[derive(Clone)]
pub(crate) struct SessionEntry {
    pub(crate) session: Arc<EngineSession>,
    /// The live permission engine, so a host can switch the mode mid-turn
    /// (`session_set_permission_mode`) without rebuilding the session. `None`
    /// when the pipeline built without a policy snapshot.
    permission_engine: Option<Arc<crate::permission::PermissionEngine>>,
    /// The roots and policy the engine's skill catalog is scanned with, so
    /// `session_skills` can serve that catalog to the host.
    skill_scan: crate::skills::SkillScanRoots,
    turn_event_count: Arc<std::sync::atomic::AtomicU32>,
    native_tool_count: Arc<std::sync::atomic::AtomicU32>,
    llm_transport: String,
    /// The session's own LLM, kept for operations that run outside a turn
    /// (the `/compact` summarizer call).
    llm: Arc<dyn LLM>,
    /// The context window the host resolved for the session's model, used to
    /// derive the compaction config for `/compact`.
    max_context_tokens: Option<u32>,
    /// The live quiescence guard (M1c RAII). Acquire stores it; release drops
    /// it — the drop replays held turns and wakes the pump.
    quiescence_guard: Arc<Mutex<Option<crate::session::QuiescenceGuard>>>,
    /// The MCP manager this session's pipeline connected from
    /// `params.mcp_servers`. Kept so the host can read the roster: the manager
    /// is built once per session, and without a handle the host had no way to
    /// see servers the engine had already connected.
    mcp_manager: Option<Arc<crate::mcp::McpManager>>,
    /// The session's workspace root, so the process-wide cron dispatcher can
    /// find a live session to run a fired job in.
    workspace: String,
    /// The session's host callbacks, so the cron dispatcher can read the cron
    /// registry and publish `cron.fired` through a live session's channel.
    pub(crate) callbacks: Arc<dyn crate::callbacks::HostCallbacks>,
    /// The session's MCP status subscription. Held here (rather than inside the
    /// pipeline) so it lives exactly as long as the session: dropping the
    /// registry entry on `session_dispose` unsubscribes the shared manager.
    /// The value is never read — the field exists for its `Drop`.
    #[allow(dead_code)]
    mcp_status: Option<Arc<McpStatusBridge>>,
    /// Whether the session's last finished turn ended aborted — the next
    /// turn's interruption-reminder trigger (v2 filters `TurnEnded` on the
    /// cancelled reason). Written when a turn settles, read and cleared by
    /// the next `run_turn`.
    pub(crate) last_turn_aborted: Arc<std::sync::atomic::AtomicBool>,
    /// Whether the *user* asked for the running turn to stop (v2
    /// `TurnEnded.interruptReason === 'user_cancelled'`). Set by the
    /// `cancel_turn` binding and consumed when the turn settles: a teardown or
    /// a parent abort flips the same cancel flag but must not arm the
    /// "interrupted by the user" reminder.
    pub(crate) user_cancelled: Arc<std::sync::atomic::AtomicBool>,
    /// The session's hook dispatcher, kept so `session_dispose` can fire the
    /// `SessionEnd` hooks the way the REPL and the HTTP delete route do
    /// (v2 `sessionExternalHooksService.triggerSessionEnd`).
    hook_guard: Option<Arc<crate::tools::external_hooks::HookGuard>>,
}

pub(crate) fn session_entry(session_id: &str) -> napi::Result<SessionEntry> {
    SESSION_REGISTRY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(session_id)
        .cloned()
        .ok_or_else(|| {
            errors::napi_error(
                "session.unknown",
                format!("unknown session: {session_id}"),
                Some(serde_json::json!({ "sessionId": session_id })),
            )
        })
}

fn make_tsfn(
    cb: Option<JsFunction>,
) -> napi::Result<Option<ThreadsafeFunction<u32, ErrorStrategy::Fatal>>> {
    match cb {
        Some(cb) => Ok(Some(cb.create_threadsafe_function(
            0,
            |ctx: ThreadSafeCallContext<u32>| {
                let id = ctx.value;
                let js_num = ctx.env.create_uint32(id)?;
                let args: Vec<napi::JsUnknown> = vec![js_num.into_unknown()];
                Ok(args)
            },
        )?)),
        None => Ok(None),
    }
}

fn make_required_tsfn(
    cb: JsFunction,
) -> napi::Result<ThreadsafeFunction<u32, ErrorStrategy::Fatal>> {
    cb.create_threadsafe_function(0, |ctx: ThreadSafeCallContext<u32>| {
        let id = ctx.value;
        let js_num = ctx.env.create_uint32(id)?;
        let args: Vec<napi::JsUnknown> = vec![js_num.into_unknown()];
        Ok(args)
    })
}

/// Create a session handle: the engine pipeline is built once and every
/// enqueued turn runs through it. The turn clock is read from the host's
/// `turn` state domain at construction (M1b single-writer contract). The
/// tool table is pulled fresh per turn through `list_tools_cb` (native
/// transports only — host-proxy rebuilds tools inside `llm_chat`), and the
/// goal snapshot through `goal_cb` per turn (snake_case wire goal, or null).
/// The AGENTS.md reminder's bash-operand directories are asked through
/// `parse_bash_cb` (`{"command","cwd","home"}` in, a JSON array out, or
/// `'null'` to keep the cwd-only fallback).
#[napi]
#[allow(clippy::too_many_arguments)]
pub fn create_engine_session(
    env: Env,
    params: JsRunTurnParams,
    #[napi(ts_arg_type = "(callbackId: number) => void")] llm_chat_cb: JsFunction,
    #[napi(ts_arg_type = "(callbackId: number) => void")] execute_tool_cb: JsFunction,
    #[napi(ts_arg_type = "(callbackId: number) => void")] emit_event_cb: Option<JsFunction>,
    #[napi(ts_arg_type = "(callbackId: number) => void")] check_permission_cb: Option<JsFunction>,
    #[napi(ts_arg_type = "(callbackId: number) => void")] ask_question_cb: Option<JsFunction>,
    #[napi(ts_arg_type = "(callbackId: number) => void")] state_read_cb: Option<JsFunction>,
    #[napi(ts_arg_type = "(callbackId: number) => void")] state_write_cb: Option<JsFunction>,
    #[napi(ts_arg_type = "(callbackId: number) => void")] checkpoint_cb: Option<JsFunction>,
    #[napi(ts_arg_type = "(callbackId: number) => void")] turn_event_cb: Option<JsFunction>,
    #[napi(ts_arg_type = "(callbackId: number) => void")] telemetry_cb: Option<JsFunction>,
    #[napi(ts_arg_type = "(callbackId: number) => void")] list_tools_cb: Option<JsFunction>,
    #[napi(ts_arg_type = "(callbackId: number) => void")] goal_cb: Option<JsFunction>,
    #[napi(ts_arg_type = "(callbackId: number) => void")] auth_token_cb: Option<JsFunction>,
    #[napi(ts_arg_type = "(callbackId: number) => void")] parse_bash_cb: Option<JsFunction>,
) -> napi::Result<JsObject> {
    let llm_chat_tsfn = make_required_tsfn(llm_chat_cb)?;
    let execute_tool_tsfn = make_required_tsfn(execute_tool_cb)?;
    let emit_event_tsfn = make_tsfn(emit_event_cb)?;
    let check_permission_tsfn = make_tsfn(check_permission_cb)?;
    let ask_question_tsfn = make_tsfn(ask_question_cb)?;
    let state_read_tsfn = make_tsfn(state_read_cb)?;
    let state_write_tsfn = make_tsfn(state_write_cb)?;
    let checkpoint_tsfn = make_tsfn(checkpoint_cb)?;
    let turn_event_tsfn = make_tsfn(turn_event_cb)?;
    let telemetry_tsfn = make_tsfn(telemetry_cb)?;
    let list_tools_tsfn = make_tsfn(list_tools_cb)?;
    let goal_tsfn = make_tsfn(goal_cb)?;
    let auth_token_tsfn = make_tsfn(auth_token_cb)?;
    let parse_bash_tsfn = make_tsfn(parse_bash_cb)?;

    env.execute_tokio_future(
        guard_async_panic(async move {
            let agent_cancel_slot: Arc<
                std::sync::Mutex<Option<crate::subagent::types::ParentCancel>>,
            > = Arc::new(std::sync::Mutex::new(None));
            // #3697: the turn's steer signal, shared with the toolset through
            // the pipeline and refreshed per turn by the session pump.
            let steer_slot: Arc<std::sync::Mutex<Option<crate::subagent::types::ParentCancel>>> =
                Arc::new(std::sync::Mutex::new(None));
            let (pipeline, mcp_status) = build_engine_pipeline(
                &params,
                EngineCallbackTsfns {
                    llm_chat: llm_chat_tsfn,
                    execute_tool: execute_tool_tsfn,
                    emit_event: emit_event_tsfn,
                    check_permission: check_permission_tsfn,
                    ask_question: ask_question_tsfn,
                    state_read: state_read_tsfn,
                    state_write: state_write_tsfn,
                    checkpoint: checkpoint_tsfn,
                    turn_event: turn_event_tsfn,
                    telemetry: telemetry_tsfn,
                    list_tools: list_tools_tsfn,
                    goal: goal_tsfn.clone(),
                    parse_bash: parse_bash_tsfn,
                    auth_token: auth_token_tsfn,
                    cancellation: None,
                },
                None,
                Some(agent_cancel_slot.clone()),
                Some(steer_slot.clone()),
            )
            .await?;

            // Turn-start tool table: pulled fresh through pipeline.callbacks per turn
            // on native transports (host-proxy rebuilds tools inside llm_chat
            // and never consults the engine's table). run_turn's per-step
            // `host/list_tools` refresh stays the authoritative source; this
            // provider only seeds the snapshot fallback, merging MCP tools if attached.
            let is_host_proxy = pipeline.llm.transport() == "host-proxy";
            let callbacks_for_defs = pipeline.callbacks.clone();
            let tool_defs_provider: ToolDefsProvider = if is_host_proxy {
                Arc::new(|| Box::pin(async { Vec::new() }))
            } else {
                Arc::new(move || {
                    let callbacks = callbacks_for_defs.clone();
                    Box::pin(async move {
                        callbacks
                            .list_tools()
                            .await
                            .map(|r| r.tools)
                            .unwrap_or_default()
                    })
                })
            };

            // Fresh goal snapshot per turn (budget checks + steering). The
            // callback returns the snake_case wire goal JSON, or null.
            let goal_provider: Option<GoalProvider> = goal_tsfn.map(|tsfn| {
                let provider: GoalProvider = Arc::new(move || {
                    let tsfn = Arc::new(tsfn.clone());
                    Box::pin(async move {
                        let output = invoke_via_registry(
                            &tsfn,
                            "{}".to_string(),
                            "session_goal",
                            None,
                            None,
                        )
                        .await
                        .ok()?;
                        serde_json::from_str::<GoalContext>(&output).ok()
                    })
                });
                provider
            });

            let initial_usage = params
                .initial_usage_json
                .as_deref()
                .map(crate::session::usage::UsageStatus::from_json_str)
                .unwrap_or_default();
            let session = EngineSession::new_seeded(
                SessionConfig {
                    steer_trace: None,
                    llm: pipeline.llm.clone(),
                    callbacks: pipeline.callbacks.clone(),
                    max_steps: params.max_steps.unwrap_or(u32::MAX),
                    max_attempts: params.max_attempts,
                    max_context_tokens: compaction_window(
                        params.native_llm.as_ref(),
                        params.max_context_tokens,
                    ),
                    compaction_max_attempts: params.compaction_max_attempts,
                    permission_mode: pipeline.permission_mode,
                    tool_defs: tool_defs_provider,
                    goal: goal_provider,
                    on_before_turn: None,
                    agent_cancel_slot: Some(agent_cancel_slot),
                    steer_slot: Some(steer_slot),
                    hook_guard: pipeline.hook_guard.clone(),
                    print_background: print_background_policy(&params),
                    // The host's session id is also the task-notification key: the
                    // print settle drains only this session's completions.
                    session_id: params.session_id.clone(),
                    task_runner: SUBAGENT_MANAGER.get_task_runner_sync(),
                    toolset: pipeline.toolset.clone(),
                    // M1c (v2 #3963): the host-injected turn-telemetry context.
                    // Present on the napi path, the pump emits the turn lifecycle
                    // through the host/telemetry seam.
                    telemetry: params.telemetry.map(|t| {
                        crate::turn_loop::types::TelemetryContext {
                            mode: t.mode,
                            provider_type: t.provider_type,
                            protocol: t.protocol,
                            thinking_effort: t.thinking_effort,
                            enabled_plugins: t.enabled_plugins,
                        }
                    }),
                },
                initial_usage,
            )
            .await;
            if let Some(toolset) = pipeline.toolset.as_ref() {
                let driver = Arc::new(crate::session::wake::EngineSessionWakeDriver::new(
                    session.clone(),
                ));
                let handler = toolset.bind_tower_wake_driver(driver);
                session.set_wake_replay_handler(
                    crate::tools::tower::wake::TOWER_INBOX_WAKE_VARIANT,
                    handler,
                );
            }

            let session_id = format!("session-{}", SESSION_NEXT_ID.fetch_add(1, Ordering::SeqCst));
            // #3717 late-settle silence: the registry holds the only strong
            // handle, so once `session_dispose` removes the entry the `Weak`
            // dies and late task settles for this host session go silent —
            // the drain (the pump) is gone with it.
            let host_session_id = params.session_id.clone().unwrap_or_default();
            let session = Arc::new(session);
            // History-dependent policies read the live session history:
            // `CompactionAuthorizationGuard` asks again for irreversible Bash
            // commands once the context was compacted, and the guardian review
            // takes its transcript from it (v2 reads context memory the same
            // way). The weak handle keeps the provider from pinning the
            // session's registry entry.
            if let Some(engine) = pipeline.permission_engine.as_ref() {
                let session_for_history = Arc::downgrade(&session);
                engine.set_conversation_provider(Arc::new(move || {
                    session_for_history
                        .upgrade()
                        .map(|session| session.snapshot_history())
                        .unwrap_or_default()
                }));
            }
            // Interactive cron: the native host has no daemon tick loop, so the
            // engine starts one per workspace (see `spawn_cron_dispatcher`).
            spawn_cron_dispatcher(params.workspace_root.as_deref().unwrap_or_default());
            if let Some(runner) = pipeline.task_runner.clone() {
                let weak = Arc::downgrade(&session);
                runner.set_liveness_check(Arc::new(move |session: Option<&str>| {
                    let Some(live) = weak.upgrade() else {
                        return false;
                    };
                    if live.is_shutdown() {
                        return false;
                    }
                    match session {
                        None => true,
                        Some(s) => s == host_session_id.as_str(),
                    }
                }));
            }
            SESSION_REGISTRY
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(
                    session_id.clone(),
                    SessionEntry {
                        session,
                        permission_engine: pipeline.permission_engine.clone(),
                        skill_scan: pipeline.skill_scan.clone(),
                        turn_event_count: pipeline.turn_event_count,
                        native_tool_count: pipeline.native_tool_count,
                        llm_transport: pipeline.llm.transport().to_string(),
                        llm: pipeline.llm.clone(),
                        max_context_tokens: compaction_window(
                            params.native_llm.as_ref(),
                            params.max_context_tokens,
                        ),
                        quiescence_guard: Arc::new(Mutex::new(None)),
                        mcp_manager: pipeline.mcp_manager.clone(),
                        workspace: params.workspace_root.clone().unwrap_or_default(),
                        callbacks: pipeline.callbacks.clone(),
                        mcp_status,
                        last_turn_aborted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                        user_cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                        hook_guard: pipeline.hook_guard.clone(),
                    },
                );
            // v2 `sessionExternalHooksService.triggerSessionStart`: the napi
            // host path fires it at creation (source `startup`) like the REPL
            // and the HTTP create route, so session hooks behave the same on
            // every transport. Observational — matching hooks run and their
            // verdicts are dropped.
            if let Some(guard) = pipeline.hook_guard.clone() {
                guard
                    .notify_session_lifecycle(
                        "SessionStart",
                        "startup",
                        serde_json::json!({
                            "source": "startup",
                            "session_title": "",
                            "model": params.model_name,
                        }),
                    )
                    .await;
            }
            Ok(session_id)
        }),
        |env, id: String| env.create_string(&id),
    )
}

/// v2 #4627 `SteerResult`: what a submitted prompt got at admission — the
/// prompt id the engine minted, the state observed, and whether the prompt
/// joined the running turn (a steer) instead of opening or queueing a turn.
#[napi(object)]
#[derive(Clone)]
pub struct TurnSubmission {
    pub turn_id: f64,
    pub prompt_id: String,
    pub created_at: String,
    pub state: String,
    pub steered: bool,
}

/// Enqueue a prompt. The turn id is assigned synchronously (monotonic, never
/// reused), so the caller can cancel by id immediately; the outcome resolves
/// through `session_turn_outcome`. `prompt` is a serialized `LLMMessage`
/// JSON (role/content/blocks/tool_calls/tool_call_id).
#[napi]
pub fn session_enqueue_turn(
    session_id: String,
    prompt: String,
    admission: String,
) -> napi::Result<TurnSubmission> {
    guard_sync_panic(|| {
        let entry = session_entry(&session_id)?;
        // The host may nest a prompt origin (v2 `PromptOrigin` JSON — the
        // `skill_activation` variant a user-slash activation carries) on the
        // prompt object. `LLMMessage` does not model origin variants, so it is
        // split off here and rides the `TurnRequest`, which echoes it on the
        // `turn.prompt` / `turn.started` events the host folds (v2 #3832).
        let mut prompt_value: serde_json::Value = serde_json::from_str(&prompt)
            .map_err(|e| napi::Error::from_reason(format!("prompt parse: {e}")))?;
        let origin = prompt_value
            .get("origin")
            .filter(|origin| origin.get("kind").and_then(|k| k.as_str()).is_some())
            .cloned();
        if origin.is_some()
            && let Some(object) = prompt_value.as_object_mut()
        {
            object.remove("origin");
        }
        let mut prompt: LLMMessage = serde_json::from_value(prompt_value)
            .map_err(|e| napi::Error::from_reason(format!("prompt parse: {e}")))?;
        // A client submits an uploaded file as a `kimi-file://` media URL; the
        // engine is the side that knows it is a daemon reference.
        prompt.blocks = crate::llm::media_resolver::normalize_media_refs(prompt.blocks);
        let admission = match admission.as_str() {
            "newTurn" => Admission::NewTurn,
            "activeOrNewTurn" => Admission::ActiveOrNewTurn,
            "activeOrNextTurn" => Admission::ActiveOrNextTurn,
            "activeTurnOnly" => Admission::ActiveTurnOnly,
            other => {
                return Err(napi::Error::from_reason(format!(
                    "unknown admission mode: {other}"
                )));
            }
        };
        let mut request = TurnRequest::user(prompt, admission);
        if let Some(origin) = origin {
            request.origin = origin;
        }
        let receipt = entry
            .session
            .enqueue_turn(request)
            .map_err(napi::Error::from_reason)?;
        let turn_id = receipt.turn_id;
        let submission = TurnSubmission {
            turn_id: turn_id as f64,
            prompt_id: receipt.prompt_id.clone(),
            created_at: receipt.created_at.clone(),
            state: receipt.state.as_str().to_string(),
            steered: receipt.steered,
        };
        let (_, outcome) = receipt.into_parts();
        SESSION_OUTCOMES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((session_id, turn_id), outcome);
        Ok(submission)
    })
}

/// Resolve with the outcome of one enqueued turn. Takes the stored receiver —
/// the outcome is delivered exactly once.
#[napi]
pub fn session_turn_outcome(env: Env, session_id: String, turn_id: f64) -> napi::Result<JsObject> {
    let receiver = {
        let mut outcomes = SESSION_OUTCOMES.lock().unwrap_or_else(|e| e.into_inner());
        outcomes
            .remove(&(session_id.clone(), turn_id as u64))
            .ok_or_else(|| {
                errors::napi_error(
                    "session.outcome_missing",
                    format!("no outcome pending for {session_id} turn {turn_id}"),
                    Some(serde_json::json!({
                        "sessionId": session_id,
                        "turnId": turn_id as u64,
                    })),
                )
            })?
    };
    let entry = session_entry(&session_id)?;
    let turn_event_count = entry.turn_event_count;
    let native_tool_count = entry.native_tool_count;
    let llm_transport = entry.llm_transport;

    env.execute_tokio_future(
        async move {
            let outcome = receiver
                .await
                .map_err(|_| {
                    errors::napi_error(
                        "session.dropped",
                        format!("session {session_id} dropped before the turn settled"),
                        Some(serde_json::json!({ "sessionId": session_id })),
                    )
                })?
                .map_err(|message| {
                    errors::napi_error_message("engine.turn_failed", message, None)
                })?;
            Ok(outcome)
        },
        move |env, outcome: TurnOutcome| {
            let mut obj = env.create_object()?;
            match outcome {
                TurnOutcome::Ran(result) => {
                    obj.set_named_property("status", env.create_string("ran")?)?;
                    let result_obj = js_object_from_run_turn_result(
                        env,
                        js_run_turn_result(
                            result,
                            &turn_event_count,
                            &native_tool_count,
                            &llm_transport,
                        ),
                    )?;
                    obj.set_named_property("result", result_obj)?;
                }
                TurnOutcome::CancelledBeforeStart => {
                    obj.set_named_property("status", env.create_string("cancelledBeforeStart")?)?;
                    obj.set_named_property("result", env.get_null()?)?;
                }
            }
            Ok(obj)
        },
    )
}

/// Cancel a turn by id (active → interrupted at the next step boundary;
/// queued or quiescence-held → dropped with `cancelledBeforeStart`). Without
/// an id the active turn (if any) is cancelled. Returns whether anything was
/// cancelled.
#[napi]
pub fn session_cancel_turn(session_id: String, turn_id: Option<f64>) -> napi::Result<bool> {
    guard_sync_panic(|| {
        let entry = session_entry(&session_id)?;
        Ok(entry.session.cancel_turn(turn_id.map(|id| id as u64)))
    })
}

/// Evict a foreign session-occupancy holder so this process's next turn can
/// claim the session (v2 `takeOverSessionLock`, upstream #4246). A hold of
/// this process's own is left alone.
#[napi]
pub fn session_takeover_lock(session_id: String) -> napi::Result<()> {
    guard_sync_panic(|| {
        crate::session::lock::SESSION_LOCKS.takeover(&session_id);
        Ok(())
    })
}

/// Switch the permission mode on the live session, so the change takes
/// effect for the turn already running — the next tool evaluation reads the
/// new mode — instead of only after a pipeline rebuild. Returns `false` when
/// the session has no local permission engine; the caller then falls back to
/// rebuilding the handle.
#[napi]
pub fn session_set_permission_mode(session_id: String, mode: String) -> napi::Result<bool> {
    guard_sync_panic(|| {
        let entry = session_entry(&session_id)?;
        let Some(engine) = entry.permission_engine.as_ref() else {
            return Ok(false);
        };
        let new_mode = match mode.as_str() {
            "manual" => crate::permission::PermissionMode::Manual,
            "yolo" => crate::permission::PermissionMode::Yolo,
            "auto" => crate::permission::PermissionMode::Auto,
            other => {
                return Err(napi::Error::from_reason(format!(
                    "unknown permission mode: {other}"
                )));
            }
        };
        engine.set_mode(new_mode);
        // v2 `broadcastPermissionMode`: agents that inherited the session mode
        // follow the switch; a pinned agent (tower worker → `auto`) keeps its
        // binding.
        crate::napi_bindings::turn::SUBAGENT_MANAGER.broadcast_permission_mode(new_mode);
        Ok(true)
    })
}

#[napi(object)]
pub struct JsTowerStatusMissionTask {
    pub text: String,
    pub done: bool,
    pub dropped: bool,
}

#[napi(object)]
pub struct JsTowerStatusMission {
    pub id: String,
    pub title: String,
    pub slug: String,
    pub kind: String,
    pub scope: Vec<String>,
    pub branch: String,
    pub worktree: String,
    pub spawn_base: Option<String>,
    pub deps: Vec<String>,
    pub status: String,
    pub owner: Option<String>,
    pub context: Option<String>,
    pub tasks: Vec<JsTowerStatusMissionTask>,
    pub notes: Vec<String>,
    pub blockers: Vec<String>,
}

#[napi(object)]
pub struct JsTowerStatusRosterEntry {
    pub name: String,
    pub agent_id: String,
    pub session_id: Option<String>,
    pub kind: String,
    pub mission_id: Option<String>,
    pub review_target: Option<String>,
    pub review_mission_id: Option<String>,
    pub worktree: Option<String>,
    pub branch: Option<String>,
    pub spawned_at: String,
    pub died_at: Option<String>,
    pub death_status: Option<String>,
    pub death_reason: Option<String>,
    pub status: Option<String>,
}

#[napi(object)]
pub struct JsTowerStatusRoster {
    pub agents: Vec<JsTowerStatusRosterEntry>,
}

#[napi(object)]
pub struct JsTowerStatusState {
    pub base: String,
    pub mode: String,
    pub recovered_at: Option<String>,
    pub missions: Vec<JsTowerStatusMission>,
    pub roster: JsTowerStatusRoster,
}

#[napi(object)]
pub struct JsTowerStatusGateBlock {
    pub reason: String,
    pub message: String,
}

#[napi(object)]
pub struct JsTowerStatusReview {
    pub reviewer: String,
    pub target: String,
    pub round: u32,
    pub status: String,
    pub merge: String,
    pub reviewed_commit: String,
    pub date: String,
    pub file: String,
    pub mission: Option<String>,
    pub seq: Option<f64>,
    pub modified_at_ms: f64,
}

#[napi(object)]
pub struct JsTowerStatusGate {
    pub mission_id: String,
    pub branch: String,
    pub status: String,
    pub ready: bool,
    pub noop: bool,
    pub reasons: Vec<String>,
    pub blocks: Vec<JsTowerStatusGateBlock>,
    pub selected_review: Option<JsTowerStatusReview>,
    pub review_binding: Option<String>,
}

#[napi(object)]
pub struct JsTowerStatusInbox {
    pub count: u32,
}

#[napi(object)]
pub struct JsTowerStatusConcurrency {
    pub budget: u32,
    pub inflight: u32,
    pub blocked_until: Option<f64>,
}

/// Structured deterministic tower status. No model or prompt rendering is
/// involved at this boundary; the SDK chooses its own summary presentation.
#[napi(object)]
pub struct JsTowerStatus {
    pub active: bool,
    pub initialized: bool,
    pub state_lost: bool,
    pub state: Option<JsTowerStatusState>,
    pub caller: Option<String>,
    pub gate: Option<Vec<JsTowerStatusGate>>,
    pub inbox: Option<JsTowerStatusInbox>,
    pub concurrency: Option<JsTowerStatusConcurrency>,
    pub recent_activity: Option<Vec<String>>,
}

fn tower_status_to_napi(
    snapshot: crate::tools::tower::status_reader::TowerStatusSnapshot,
) -> JsTowerStatus {
    let crate::tools::tower::status_reader::TowerStatusSnapshot {
        active,
        initialized,
        state_lost,
        state,
        recovered_at,
        caller,
        gates,
        inbox_count,
        concurrency,
        recent_activity,
    } = snapshot;
    let state = state.map(|state| JsTowerStatusState {
        base: state.base,
        mode: state.mode,
        recovered_at,
        missions: state
            .missions
            .into_iter()
            .map(|mission| JsTowerStatusMission {
                id: mission.id,
                title: mission.title,
                slug: mission.slug,
                kind: mission.kind.as_str().to_string(),
                scope: mission.scope,
                branch: mission.branch,
                worktree: mission.worktree,
                spawn_base: mission.spawn_base,
                deps: mission.deps,
                status: mission.status.as_str().to_string(),
                owner: mission.owner,
                context: mission.context,
                tasks: mission
                    .tasks
                    .into_iter()
                    .map(|task| JsTowerStatusMissionTask {
                        text: task.text,
                        done: task.done,
                        dropped: task.dropped,
                    })
                    .collect(),
                notes: mission.notes,
                blockers: mission.blockers,
            })
            .collect(),
        roster: JsTowerStatusRoster {
            agents: state
                .roster
                .agents
                .into_iter()
                .map(|agent| JsTowerStatusRosterEntry {
                    name: agent.name,
                    agent_id: agent.agent_id,
                    session_id: agent.session_id,
                    kind: agent.kind.as_str().to_string(),
                    mission_id: agent.mission_id,
                    review_target: agent.review_target,
                    review_mission_id: agent.review_mission_id,
                    worktree: agent.worktree,
                    branch: agent.branch,
                    spawned_at: agent.spawned_at,
                    died_at: agent.died_at,
                    death_status: agent.death_status,
                    death_reason: agent.death_reason,
                    status: agent.status,
                })
                .collect(),
        },
    });
    JsTowerStatus {
        active,
        initialized,
        state_lost,
        state,
        caller: initialized.then_some(caller),
        gate: initialized.then(|| {
            gates
                .into_iter()
                .map(|gate| JsTowerStatusGate {
                    mission_id: gate.mission_id,
                    branch: gate.branch,
                    status: gate.status,
                    ready: gate.ready,
                    noop: gate.noop,
                    reasons: gate.reasons,
                    blocks: gate
                        .blocks
                        .into_iter()
                        .map(|block| JsTowerStatusGateBlock {
                            reason: block.reason.to_string(),
                            message: block.message,
                        })
                        .collect(),
                    selected_review: gate.selected_review.map(|review| JsTowerStatusReview {
                        reviewer: review.reviewer,
                        target: review.target,
                        round: review.round,
                        status: review.status,
                        merge: review.merge,
                        reviewed_commit: review.reviewed_commit,
                        date: review.date,
                        file: review.file,
                        mission: review.mission,
                        seq: review.seq.map(|seq| seq as f64),
                        modified_at_ms: review.modified_at_ms as f64,
                    }),
                    review_binding: gate.binding,
                })
                .collect()
        }),
        inbox: initialized.then_some(JsTowerStatusInbox {
            count: inbox_count as u32,
        }),
        concurrency: initialized.then_some(JsTowerStatusConcurrency {
            budget: concurrency.budget as u32,
            inflight: concurrency.inflight as u32,
            blocked_until: concurrency.blocked_until.map(|until| until as f64),
        }),
        recent_activity: initialized.then_some(recent_activity),
    }
}

/// Read the session repository's deterministic tower snapshot for the SDK.
/// `active` is the host's tower-mode flag; it changes presentation, not the
/// persisted state or gate evaluation.
#[napi]
pub fn session_tower_status(
    env: Env,
    session_id: String,
    active: bool,
    max_running_tasks: Option<u32>,
) -> napi::Result<JsObject> {
    let entry = session_entry(&session_id)?;
    let repo_root = crate::tools::tower::paths::resolve_tower_repo_root(&entry.workspace);
    let store = crate::tools::tower::store::TowerStore::new(std::path::PathBuf::from(repo_root));
    let task_runner = SUBAGENT_MANAGER.get_task_runner_sync();
    env.execute_tokio_future(
        async move {
            let state_lock = store.state_lock();
            let _state_guard = state_lock.lock().await;
            let active_agent_ids =
                crate::tools::tower::active_tower_agent_ids(task_runner.as_deref());
            let mut snapshot = crate::tools::tower::status_reader::read_tower_status(
                &store,
                "main",
                active_agent_ids.as_ref(),
                max_running_tasks,
            )
            .await
            .map_err(napi::Error::from_reason)?;
            snapshot.active = active;
            Ok(tower_status_to_napi(snapshot))
        },
        |_env, status| Ok(status),
    )
}

/// Live session shape: the active turn id and the queued turn ids.
#[napi]
pub fn session_status(session_id: String) -> napi::Result<JsSessionStatus> {
    guard_sync_panic(|| {
        let entry = session_entry(&session_id)?;
        let status = entry.session.status();
        Ok(JsSessionStatus {
            active_turn_id: status.active_turn_id.map(|id| id as f64),
            pending_turn_ids: status
                .pending_turn_ids
                .into_iter()
                .map(|id| id as f64)
                .collect(),
            engine: status.engine.map(|e| JsEngineExecSummary {
                transport: e.transport,
                native_tool_calls: e.native_tool_calls.map(|n| n as f64),
                steps: e.steps.map(|s| s as f64),
                stop_reason: e.stop_reason,
            }),
            usage_json: status.usage.to_json().map(|value| value.to_string()),
        })
    })
}

/// The MCP roster this session's pipeline connected, as a JSON array of
/// `McpServerEntry` (name / transport / status / tool_count / error / tools).
///
/// An empty array means the session was built without `mcp_servers` — the
/// host used to answer `[]` unconditionally, so a configured server was
/// invisible to `/mcp` and to the VS Code MCP panel.
#[napi]
pub fn session_mcp_servers(env: Env, session_id: String) -> napi::Result<JsObject> {
    let manager = session_entry(&session_id)?.mcp_manager;
    env.execute_tokio_future(
        async move {
            let Some(manager) = manager else {
                return Ok("[]".to_string());
            };
            let entries = manager.server_entries().await;
            serde_json::to_string(&entries)
                .map_err(|e| napi::Error::from_reason(format!("serialize MCP roster: {e}")))
        },
        |env, json: String| env.create_string(&json),
    )
}

/// The engine's skill catalog for this session, as a JSON array of
/// `SkillDescriptor` — the same shape the `/api/v1/…/skills` routes serve.
///
/// v2 serves the catalog from the engine, so a host renders the builtin product
/// skills, the dotted sub-skill commands and the configured `extra_skill_dirs`
/// from it. A host that re-scanned the filesystem on its own would list none of
/// those (and would disagree with the system prompt's `# Skills` section).
#[napi]
pub fn session_skills(env: Env, session_id: String) -> napi::Result<JsObject> {
    let scan = session_entry(&session_id)?.skill_scan;
    env.execute_tokio_future(
        async move {
            serde_json::to_string(&scan.catalog())
                .map_err(|e| napi::Error::from_reason(format!("serialize skill catalog: {e}")))
        },
        |env, json: String| env.create_string(&json),
    )
}

/// Render the prompt a `/skill:` activation submits, resolved and expanded by
/// the engine — the same renderer the `Skill` tool uses, so a host cannot
/// drift from it. The object carries `status` (`"ok"` | `"not_found"` |
/// `"type_unsupported"`), plus on `"ok"` the prompt `text`, the resolved
/// `name`, and the `path` / `source` / `skillType` provenance the activation
/// origin and the `skill.activated` event carry.
///
/// v2 keeps the renderer behind its catalog for the same reason
/// (`features/skill/skillService.ts:207`): a host that rendered the prompt
/// itself resolved only `<workDir>/.kimi-code/skills/<name>`, so the builtin
/// skills and the configured `extra_skill_dirs` failed to activate, and it
/// skipped the `$ARGUMENTS` / `${KIMI_SKILL_DIR}` expansion entirely.
#[napi]
pub fn session_render_skill_prompt(
    env: Env,
    session_id: String,
    name: String,
    args: String,
) -> napi::Result<JsObject> {
    use crate::tools::skill::{SkillPromptError, SkillScan, render_user_slash_skill_prompt};
    let roots = session_entry(&session_id)?.skill_scan;
    let scan = SkillScan {
        root: roots.root.as_deref(),
        extra_dirs: &roots.extra_dirs,
        plugin_dirs: &roots.plugin_dirs,
        merge_all_available_skills: roots.merge_all_available_skills,
    };
    let mut obj = env.create_object()?;
    match render_user_slash_skill_prompt(&scan, &name, &args, &session_id) {
        Ok(prompt) => {
            obj.set_named_property("status", env.create_string("ok")?)?;
            obj.set_named_property("text", env.create_string(&prompt.text)?)?;
            obj.set_named_property("name", env.create_string(&prompt.name)?)?;
            if let Some(path) = prompt.path.as_deref() {
                obj.set_named_property("path", env.create_string(path)?)?;
            }
            if let Some(source) = prompt.source.as_deref() {
                obj.set_named_property("source", env.create_string(source)?)?;
            }
            if let Some(skill_type) = prompt.skill_type.as_deref() {
                obj.set_named_property("skillType", env.create_string(skill_type)?)?;
            }
        }
        Err(SkillPromptError::NotFound) => {
            obj.set_named_property("status", env.create_string("not_found")?)?;
        }
        Err(SkillPromptError::TypeUnsupported(skill_name)) => {
            obj.set_named_property("status", env.create_string("type_unsupported")?)?;
            obj.set_named_property("name", env.create_string(&skill_name)?)?;
        }
    }
    Ok(obj)
}

/// The warnings this session should surface at startup, as a JSON array of
/// `{ code, message, severity }`.
///
/// Derived from the MCP roster: a server the engine could not connect, or one
/// waiting on the user's authorization. A healthy session produces `[]`.
#[napi]
pub fn session_warnings(env: Env, session_id: String) -> napi::Result<JsObject> {
    let manager = session_entry(&session_id)?.mcp_manager;
    env.execute_tokio_future(
        async move {
            let Some(manager) = manager else {
                return Ok("[]".to_string());
            };
            let warnings = manager.session_warnings().await;
            serde_json::to_string(&warnings)
                .map_err(|e| napi::Error::from_reason(format!("serialize session warnings: {e}")))
        },
        |env, json: String| env.create_string(&json),
    )
}

/// Whether the session is fully idle right now (nothing active, pending, or
/// held).
#[napi]
pub fn session_is_settled(session_id: String) -> napi::Result<bool> {
    guard_sync_panic(|| {
        let entry = session_entry(&session_id)?;
        Ok(entry.session.is_settled())
    })
}

/// Resolves once the session is fully idle: no active turn, no pending or
/// held turns.
#[napi]
pub fn session_settled(env: Env, session_id: String) -> napi::Result<JsObject> {
    let entry = session_entry(&session_id)?;
    let session = entry.session;
    env.execute_tokio_future(
        async move {
            // The napi shape is a `Promise<void>`, so the drain bound is
            // reported rather than turned into a rejection.
            if let Err(timeout) = session.settled().await {
                tracing::warn!(%timeout, "session settle did not finish before the drain bound");
            }
            Ok(())
        },
        |env, ()| env.get_undefined(),
    )
}

/// Replace the session's cross-turn history (the next enqueued turn starts
/// from it, with the new prompt appended).
#[napi]
pub fn session_set_history(session_id: String, history_json: String) -> napi::Result<()> {
    guard_sync_panic(|| {
        let entry = session_entry(&session_id)?;
        let history: Vec<LLMMessage> = serde_json::from_str(&history_json)
            .map_err(|e| napi::Error::from_reason(format!("history parse: {e}")))?;
        entry.session.set_history(history);
        Ok(())
    })
}

#[napi]
pub fn session_clear_history(session_id: String) -> napi::Result<()> {
    guard_sync_panic(|| {
        let entry = session_entry(&session_id)?;
        entry.session.clear_history();
        Ok(())
    })
}

/// Append messages to the cross-turn history (e.g. a resumed transcript).
#[napi]
pub fn session_extend_history(session_id: String, history_json: String) -> napi::Result<()> {
    guard_sync_panic(|| {
        let entry = session_entry(&session_id)?;
        let history: Vec<LLMMessage> = serde_json::from_str(&history_json)
            .map_err(|e| napi::Error::from_reason(format!("history parse: {e}")))?;
        entry.session.extend_history(history);
        Ok(())
    })
}

#[napi]
pub fn session_history_len(session_id: String) -> napi::Result<u32> {
    guard_sync_panic(|| {
        let entry = session_entry(&session_id)?;
        Ok(entry.session.history_len() as u32)
    })
}

/// The session's current cross-turn history as a JSON `LLMMessage[]` — the
/// inverse of `session_set_history`. Lets the host carry the conversation
/// across an engine-session rebuild (a mid-session model / permission change)
/// and implement undo / fork without losing context.
#[napi]
pub fn session_get_history(session_id: String) -> napi::Result<String> {
    guard_sync_panic(|| {
        let entry = session_entry(&session_id)?;
        serde_json::to_string(&entry.session.snapshot_history())
            .map_err(|e| napi::Error::from_reason(format!("history serialize: {e}")))
    })
}

/// Drop the session handle: the pump task is signalled to stop, its
/// background tasks are stopped, and the conversation it owns is released —
/// and this resolves only once that has actually happened, so a host that
/// deletes the session's directory after awaiting it is not racing the
/// engine (ROADMAP §15 D1, v2 `disposeAsync`). Pending outcome receivers are
/// dropped too, so a JS `session_turn_outcome` awaiting one rejects instead
/// of hanging on a pump that will never run again.
///
/// The signature changed from `()` to a promise: an addon that predates the
/// export returns `undefined`, which `await` accepts — that host keeps the old
/// "signalled, not released" semantics rather than getting a stronger promise
/// than the engine keeps.
///
/// If the release bound expires, the promise **rejects** with
/// `session.release_timeout`: the host must not delete the session's files
/// while the engine still owns them. A second `session_dispose` for the same
/// id shares the same wait.
#[napi]
pub fn session_dispose(env: Env, session_id: String) -> napi::Result<JsObject> {
    // Synchronous section: everything here runs under the registry lock, and
    // nothing here awaits. Waiting while holding the lock would deadlock —
    // the pump takes the same lock on its way out.
    let (session, hook_guard) = guard_sync_panic(|| {
        let entry = SESSION_REGISTRY
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&session_id);
        let (session, hook_guard) = match entry {
            Some(entry) => {
                entry.session.shutdown();
                // Remember the wait, so a second dispose for this id shares it
                // rather than resolving against an already-empty registry.
                RELEASE_PENDING
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(session_id.clone(), entry.session.clone());
                (Some(entry.session), entry.hook_guard)
            }
            // Already being disposed: join that wait instead of pretending
            // the session is gone.
            None => (
                RELEASE_PENDING
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&session_id)
                    .cloned(),
                None,
            ),
        };
        SESSION_OUTCOMES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(session, _), _| session != &session_id);
        Ok((session, hook_guard))
    })?;
    let wait_id = session_id.clone();
    env.execute_tokio_future(
        async move {
            // Cleanup first, so the entry goes even if the wait below is
            // dropped mid-flight (a runtime shutdown aborts the task).
            let _cleanup = PendingReleaseCleanup(wait_id);
            // v2 `sessionExternalHooksService.triggerSessionEnd`: disposing the
            // handle is the close reason `exit` (archival is the server's
            // DELETE). Fired before the drain wait, like the REPL's exit hook.
            if let Some(guard) = hook_guard {
                guard
                    .notify_session_lifecycle(
                        "SessionEnd",
                        "exit",
                        serde_json::json!({ "reason": "exit", "session_title": "" }),
                    )
                    .await;
            }
            if let Some(session) = session {
                if let Err(timeout) = session.released().await {
                    // The release bound is a real failure: the host is about to
                    // delete the session's directory and must not proceed while
                    // the engine still owns its files. Report it with the code
                    // and the stage, instead of resolving as if it were clean.
                    let release_id = _cleanup.0.clone();
                    return Err(errors::napi_error(
                        "session.release_timeout",
                        format!(
                            "session {release_id} did not release before the drain bound: {timeout}"
                        ),
                        Some(serde_json::json!({
                            "sessionId": release_id,
                            "stage": timeout.stage,
                            "limitMs": timeout.limit.as_millis() as u64,
                            "waitedMs": timeout.waited.as_millis() as u64,
                        })),
                    ));
                }
            }
            Ok(())
        },
        |env, ()| env.get_undefined(),
    )
}

/// Removes a [`RELEASE_PENDING`] entry when a dispose future ends, however it
/// ends.
struct PendingReleaseCleanup(String);

impl Drop for PendingReleaseCleanup {
    fn drop(&mut self) {
        RELEASE_PENDING
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.0);
    }
}

/// Try to acquire quiescence (M1c): an exclusive window in which enqueued
/// turns are parked instead of admitted. Fails when a guard is already held
/// or any turn is active, pending, or held — the caller waits for
/// `session_settled` and retries. The guard lives in the registry; release
/// with `session_release_quiescence`.
#[napi]
pub fn session_try_acquire_quiescence(session_id: String) -> napi::Result<bool> {
    guard_sync_panic(|| {
        let entry = session_entry(&session_id)?;
        match entry.session.try_acquire_quiescence() {
            Some(guard) => {
                *entry
                    .quiescence_guard
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(guard);
                Ok(true)
            }
            None => Ok(false),
        }
    })
}

/// Release the quiescence window: held turns replay in FIFO order and the
/// pump wakes. A no-op when no guard is held.
#[napi]
pub fn session_release_quiescence(session_id: String) -> napi::Result<()> {
    guard_sync_panic(|| {
        let entry = session_entry(&session_id)?;
        *entry
            .quiescence_guard
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        Ok(())
    })
}

// ── Embedded harness capabilities (Wave 1) ────────────────────────────────
// Session-scoped capabilities the TUI harness drives outside the turn queue.
// The standalone server exposes the same state over HTTP (btw: POST
// /sessions/:id/btw, title: POST /sessions/:id/title/generate, tasks:
// /api/v1/tasks); here they cross the napi boundary directly.

/// Start a btw side-channel instance forked from the session's current
/// history (v2 `/btw`; mirrors `start_btw` at the standalone server).
/// Returns the engine-assigned subagent id (`agent-btw-…`); the side-channel
/// turns run through `session_btw_prompt` on the process-wide
/// [`SUBAGENT_MANAGER`] runtime.
#[napi]
pub fn session_start_btw(env: Env, session_id: String) -> napi::Result<JsObject> {
    guard_sync_panic(move || {
        let history = session_entry(&session_id)?.session.snapshot_history();
        env.execute_tokio_future(
            async move {
                crate::subagent::start_btw(&SUBAGENT_MANAGER, &history)
                    .await
                    .map_err(napi::Error::from_reason)
            },
            |env, agent_id: String| env.create_string(&agent_id),
        )
    })
}

/// Run one btw side-channel turn (v2 `/btw` panel). The turn runs on the
/// subagent instance's tool-free profile (resume semantics: the prompt is
/// appended to the forked conversation seeded by `session_start_btw`), and
/// its deltas stream through the owning session's `emit_event` callback
/// attributed to the btw agent id. The resolved value carries the final
/// assistant text plus the raw stop reason.
#[napi]
pub fn session_btw_prompt(
    env: Env,
    session_id: String,
    agent_id: String,
    prompt: String,
) -> napi::Result<JsObject> {
    guard_sync_panic(move || {
        // The session must be live, but the side channel owns its conversation:
        // the turn runs on the shared subagent runtime, outside the session's
        // turn queue.
        session_entry(&session_id)?;
        let manager = SUBAGENT_MANAGER.clone();
        env.execute_tokio_future(
            async move {
                // Register a parent-cancel under the agent id so
                // `session_btw_cancel` can abort the run mid-turn. The guard
                // removes it on every exit path, including a dropped future.
                let cancel = crate::subagent::types::ParentCancel::new();
                let _cancel_guard =
                    MapEntryGuard::insert(&CANCEL_MAP, agent_id.clone(), cancel.clone());
                let outcome = manager
                    .resume_foreground_turn(&agent_id, &prompt, Some(&cancel))
                    .await;
                match outcome {
                    Some(Ok(crate::subagent::manager::ForegroundTurnOutcome::Completed(
                        result,
                    ))) => {
                        let content = result
                            .messages
                            .iter()
                            .rev()
                            .find(|m| m.role == "assistant")
                            .map(|m| m.content.clone())
                            .unwrap_or_default();
                        Ok((content, format!("{:?}", result.stop_reason)))
                    }
                    Some(Ok(crate::subagent::manager::ForegroundTurnOutcome::ParentCancelled)) => {
                        Ok((String::new(), "Aborted".to_string()))
                    }
                    Some(Err(message)) => Err(errors::napi_error_message(
                        "engine.turn_failed",
                        message,
                        None,
                    )),
                    None => Err(errors::napi_error(
                        "btw.unknown_instance",
                        format!("unknown btw side-channel instance: {agent_id}"),
                        Some(serde_json::json!({ "agentId": agent_id })),
                    )),
                }
            },
            |env, (content, stop_reason): (String, String)| {
                let mut obj = env.create_object()?;
                obj.set_named_property("content", env.create_string_from_std(content)?)?;
                obj.set_named_property("stopReason", env.create_string_from_std(stop_reason)?)?;
                Ok(obj)
            },
        )
    })
}

/// Abort a running btw side-channel turn (v2 `/btw` panel cancel). Triggers
/// the parent-cancel registered by `session_btw_prompt`; returns whether a
/// turn was pending for this agent id.
#[napi]
pub fn session_btw_cancel(agent_id: String) -> napi::Result<bool> {
    guard_sync_panic(|| {
        let cancel = CANCEL_MAP
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&agent_id)
            .cloned();
        match cancel {
            Some(cancel) => {
                cancel.trigger();
                Ok(true)
            }
            None => Ok(false),
        }
    })
}

/// The session title from the live cross-turn history (v2 `generateTitle`).
///
/// `first_turn` / `user_prompts` derive it deterministically; `digest` asks
/// the managed platform through the `chat_title` tool, which needs an
/// OAuth-managed model — a session on a static API key rejects that source
/// rather than silently falling back to the deterministic title. Resolves
/// null when the history cannot supply an input. `force` marks a host-forced
/// regeneration in the title-generation telemetry event.
#[napi]
pub fn session_generate_title(
    env: Env,
    session_id: String,
    source: Option<String>,
    force: Option<bool>,
) -> napi::Result<JsObject> {
    let entry = session_entry(&session_id)?;
    let history = entry.session.snapshot_history();
    let llm = entry.llm.clone();
    let callbacks = entry.callbacks.clone();
    env.execute_tokio_future(
        async move {
            crate::session::title::generate_session_title_with_telemetry(
                &history,
                source.as_deref(),
                llm.as_ref(),
                force.unwrap_or(false),
                &crate::callbacks::TitleTelemetry(callbacks.as_ref()),
            )
            .await
            .map_err(napi::Error::from_reason)
        },
        |env, title: Option<String>| match title {
            Some(title) => env.create_string(&title).map(|value| value.into_unknown()),
            None => env.get_null().map(|null| null.into_unknown()),
        },
    )
}

/// Manually compact the session's cross-turn history with an LLM-written
/// summary — the embedded `/compact [instruction]` path (v2's compaction
/// operation). Resolves with a JSON compaction report (`changed`,
/// `messageCount`, `compactedCount`, `tokensBefore`, `tokensAfter`,
/// `summary`). Everything up to the deepest safe split is replaced by the
/// summary, so the smallest safe tail is kept verbatim. The host owns the
/// quiescence window around this call; cancellation goes through
/// [`session_cancel_compaction`], and a cancelled compaction leaves the
/// history untouched.
#[napi]
pub fn session_compact(
    env: Env,
    session_id: String,
    instruction: Option<String>,
) -> napi::Result<JsObject> {
    guard_sync_panic(move || {
        let entry = session_entry(&session_id)?;
        let flag = Arc::new(AtomicBool::new(false));
        env.execute_tokio_future(
            async move {
                // The guard removes the cancel flag on every exit path,
                // including a dropped future — a leaked flag would make the
                // next compaction look already-cancelled.
                let _cancel_guard =
                    MapEntryGuard::insert(&COMPACTION_CANCEL, session_id.clone(), flag.clone());
                compact_session_with_summary(&entry, instruction, &flag).await
            },
            |env, report: serde_json::Value| {
                env.create_string_from_std(
                    serde_json::to_string(&report).unwrap_or_else(|e| e.to_string()),
                )
            },
        )
    })
}

/// Abort the compaction `session_compact` is running; true when one was in
/// flight. The summarizer call is cancelled and the session history keeps
/// its pre-compaction content.
#[napi]
pub fn session_cancel_compaction(session_id: String) -> napi::Result<bool> {
    guard_sync_panic(|| {
        let flag = COMPACTION_CANCEL
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&session_id)
            .cloned();
        match flag {
            Some(flag) => {
                flag.store(true, Ordering::Relaxed);
                Ok(true)
            }
            None => Ok(false),
        }
    })
}

/// Compact `entry`'s history with the session's own LLM. The system prompt
/// heads the working list and is never compacted (it is stripped again before
/// the result is stored); injection messages survive the trim exactly as they
/// do in the turn loop.
async fn compact_session_with_summary(
    entry: &SessionEntry,
    instruction: Option<String>,
    flag: &Arc<AtomicBool>,
) -> napi::Result<serde_json::Value> {
    let llm = entry.llm.clone();
    let history = entry.session.snapshot_history();
    let mut messages = vec![LLMMessage::system(llm.system_prompt())];
    messages.extend(history);
    let config = crate::compaction::config_for_window(entry.max_context_tokens);
    let injections = crate::injection::split_injections(&mut messages);
    let tokens_before = crate::compaction::estimate_messages_tokens(&messages);
    let count = crate::compaction::compute_compact_count_manual(&messages, &config);
    if count == 0 {
        return Ok(serde_json::json!({
            "changed": false,
            "messageCount": messages.len() - 1 + injections.len(),
            "tokensBefore": tokens_before,
            "tokensAfter": tokens_before,
        }));
    }
    let cancel = crate::turn_loop::run_turn::TurnCancellation::from_flag(Some(flag.clone()));
    // v2 #3911: pre-shrink the summarizer's first request to the effective
    // window (configured, lowered by any in-process overflow observation).
    let window = crate::compaction::effective_max_tokens_for(
        crate::callbacks::MAIN_AGENT_ID,
        llm.model_name(),
        entry.max_context_tokens,
    );
    // v2 `postProcessSummary`: carry the todo state into the summary when the
    // session has one; a host without the state bridge degrades to None.
    let todos = crate::compaction::read_todos_for_summary(entry.callbacks.as_ref()).await;
    let (compacted, summary, usage) =
        crate::compaction::force_compact_messages_manual_with_summary_report(
            &messages,
            &config,
            llm.as_ref(),
            instruction.as_deref(),
            Some(cancel.token()),
            window,
            todos,
            crate::callbacks::MAIN_AGENT_ID,
        )
        .await
        .map_err(|error| napi::Error::from_reason(error.to_string()))?;
    // The summarizer request is billable even when the user cancels right
    // after it returns; record it before the cancellation check.
    if let Some(usage) = &usage {
        entry
            .session
            .record_compaction_usage(llm.model_name(), usage);
    }
    if flag.load(Ordering::Relaxed) {
        return Err(napi::Error::from_reason(
            crate::i18n::LocalizedText::new("engine.compaction.cancelled").render(),
        ));
    }
    let tokens_after = crate::compaction::estimate_messages_tokens(&compacted);
    // The compacted shape already closes with the continuation note (v2
    // `buildContextCompactionShape`), so `compacted[1..]` goes in as-is:
    // kept verbatim user input, prefixed summary, continuation, tail.
    let mut new_history = compacted[1..].to_vec();
    new_history.extend(injections);
    let message_count = new_history.len() as u32;
    entry.session.set_history(new_history);
    Ok(serde_json::json!({
        "changed": true,
        "messageCount": message_count,
        // `count` is the first kept index over the engine's view (which has
        // the system prompt at `[0]`); the host-side history never carries
        // it (`compacted[1..]` above), so its first kept index is one short.
        "compactedCount": count - 1,
        "tokensBefore": tokens_before,
        "tokensAfter": tokens_after,
        "summary": summary,
    }))
}

#[cfg(test)]
mod tower_status_tests {
    use super::*;
    use crate::tools::tower::mission_gate::TowerGateBlock;
    use crate::tools::tower::rate_limit::TowerRateLimitSnapshot;
    use crate::tools::tower::status_reader::{TowerStatusGate, TowerStatusSnapshot};
    use crate::tools::tower::types::{
        TowerAgentKind, TowerMission, TowerMissionStatus, TowerReviewInfo, TowerRoster,
        TowerRosterEntry, TowerState,
    };

    #[test]
    fn tower_status_napi_preserves_structured_state_gate_and_concurrency() {
        let mission = TowerMission {
            id: "M1".into(),
            title: "Build engine".into(),
            slug: "build-engine".into(),
            kind: Default::default(),
            scope: vec!["src/**".into()],
            branch: "feat/build".into(),
            worktree: "wt-build".into(),
            spawn_base: Some("base123".into()),
            deps: vec!["M0".into()],
            status: TowerMissionStatus::Active,
            owner: Some("w1".into()),
            context: Some("briefing".into()),
            tasks: Vec::new(),
            notes: vec!["note".into()],
            blockers: vec!["blocker".into()],
        };
        let agent = TowerRosterEntry {
            name: "w1".into(),
            agent_id: "agent-w1".into(),
            session_id: Some("session-main".into()),
            kind: TowerAgentKind::Worker,
            mission_id: Some("M1".into()),
            review_target: None,
            review_mission_id: None,
            worktree: Some("wt-build".into()),
            branch: Some("feat/build".into()),
            spawned_at: "2026-11-04T00:00:00Z".into(),
            died_at: Some("2026-11-04T01:00:00Z".into()),
            death_status: Some("failed".into()),
            death_reason: Some("provider blew up".into()),
            status: Some("dead".into()),
        };
        let review = TowerReviewInfo {
            reviewer: "r1".into(),
            target: "feat/build".into(),
            round: 1,
            status: "clean".into(),
            merge: "merge".into(),
            reviewed_commit: "abc123".into(),
            date: "2026-11-04T00:00:00Z".into(),
            file: "reviews/1.md".into(),
            mission: Some("M1".into()),
            seq: Some(1),
            modified_at_ms: 42,
        };
        let snapshot = TowerStatusSnapshot {
            active: true,
            initialized: true,
            state_lost: false,
            state: Some(TowerState {
                version: 1,
                base: "main".into(),
                mode: "branch".into(),
                created_at: "2026-11-04T00:00:00Z".into(),
                session_id: Some("session-main".into()),
                roster: TowerRoster {
                    agents: vec![agent],
                },
                missions: vec![mission],
            }),
            recovered_at: Some("2026-11-04T02:00:00Z".into()),
            caller: "tower".into(),
            gates: vec![TowerStatusGate {
                mission_id: "M1".into(),
                branch: "feat/build".into(),
                status: "active".into(),
                ready: false,
                noop: false,
                reasons: vec!["not-clean".into()],
                blocks: vec![TowerGateBlock::new(
                    "not-clean",
                    "merge blocked: fix findings",
                )],
                selected_review: Some(review),
                binding: Some("exact-tip".into()),
            }],
            inbox_count: 3,
            concurrency: TowerRateLimitSnapshot {
                budget: 2,
                inflight: 1,
                blocked_until: Some(1_800_000),
            },
            recent_activity: vec!["tower init".into()],
        };

        let status = tower_status_to_napi(snapshot);
        assert!(status.active);
        assert!(status.initialized);
        assert!(!status.state_lost);
        assert_eq!(status.caller.as_deref(), Some("tower"));
        assert_eq!(status.inbox.as_ref().map(|inbox| inbox.count), Some(3));
        let state = status.state.expect("initialized state");
        assert_eq!(state.base, "main");
        assert_eq!(state.recovered_at.as_deref(), Some("2026-11-04T02:00:00Z"));
        assert_eq!(state.missions[0].owner.as_deref(), Some("w1"));
        assert_eq!(state.missions[0].deps, vec!["M0"]);
        assert_eq!(state.roster.agents[0].kind, "worker");
        assert_eq!(
            state.roster.agents[0].death_status.as_deref(),
            Some("failed")
        );
        let gate = status.gate.expect("gate").remove(0);
        assert_eq!(gate.blocks[0].reason, "not-clean");
        assert_eq!(gate.blocks[0].message, "merge blocked: fix findings");
        assert_eq!(gate.selected_review.map(|review| review.round), Some(1));
        assert_eq!(gate.review_binding.as_deref(), Some("exact-tip"));
        let concurrency = status.concurrency.expect("concurrency");
        assert_eq!(concurrency.budget, 2);
        assert_eq!(concurrency.inflight, 1);
        assert_eq!(concurrency.blocked_until, Some(1_800_000.0));
        assert_eq!(status.recent_activity, Some(vec!["tower init".into()]));
    }
}
