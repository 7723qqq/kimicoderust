//! Turn execution: `run_turn_rust` and the engine-pipeline wiring that
//! connects the napi callback surface to the turn loop.

use super::*;
use crate::mcp::manager::{McpServerOptions, McpServerRecipe};
use crate::pipeline::{self, EnginePipeline, PipelineHost, PipelineProvider, PipelineSpec};
use crate::rpc::types::{
    NativeLlmConfig, StateReadRequest, StateWriteRequest, SubagentProfileWire,
};
use crate::session::{Admission, TurnRequest};
use crate::turn_loop::{run_turn::run_turn_continued, run_turn::run_turn_with_telemetry, types::*};
use napi::{
    JsObject,
    bindgen_prelude::{Env, JsFunction},
    threadsafe_function::{ErrorStrategy, ThreadSafeCallContext, ThreadsafeFunction},
};
use napi_derive::napi;
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

/// Active-turn cancellation signals keyed by `turn_id`. `run_turn_rust`
/// registers a signal before running and removes it afterwards; `cancel_turn`
/// triggers the signal of a running turn from the JS side so the loop can
/// observe the cancellation at the next step boundary — and, since P51, the
/// foreground subagent's event-driven wait aborts immediately.
pub(crate) static CANCEL_MAP: LazyLock<
    Mutex<HashMap<String, crate::subagent::types::ParentCancel>>,
> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Process-wide subagent manager (P28). Instance state (running/completed
/// subagents) survives across turns; the execution runtime is re-injected
/// per turn because llm/callbacks are turn-scoped.
pub(crate) static SUBAGENT_MANAGER: LazyLock<Arc<crate::subagent::SubagentManager>> =
    LazyLock::new(|| Arc::new(crate::subagent::SubagentManager::new()));

/// Build the engine transport config from the host's napi object.
///
/// The structured request knobs travel as JSON strings (`serde_json::Value`
/// is not a napi value, the same convention as `JsToolDef.input_schema`); a
/// malformed string is warned about and dropped so one bad optional knob
/// cannot fail the turn. Absent knobs stay inert.
fn native_llm_config_from_js(cfg: &JsNativeLlmConfig) -> NativeLlmConfig {
    let llm = NativeLlmConfig {
        protocol: cfg.protocol.clone(),
        alias: cfg.alias.clone(),
        provider_type: cfg.provider_type.clone(),
        base_url: cfg.base_url.clone(),
        api_key: cfg.api_key.clone(),
        api_key_env: cfg.api_key_env.clone(),
        model: cfg.model.clone(),
        max_tokens: cfg.max_tokens,
        custom_headers: cfg.custom_headers.clone().unwrap_or_default(),
        reasoning_effort: cfg.reasoning_effort.clone(),
        thinking_budget: cfg.thinking_budget,
        auth_provider: cfg.auth_provider.clone(),
        thinking_keep: cfg.thinking_keep.clone(),
        beta_api: cfg.beta_api.unwrap_or(false),
        capabilities: cfg.capabilities.clone(),
        system_prompt: cfg.system_prompt.clone(),
        max_input_size: cfg.max_input_size,
        adaptive_thinking: cfg.adaptive_thinking,
        reasoning_key: cfg.reasoning_key.clone(),
        off_effort: cfg.off_effort.clone(),
        response_format: cfg
            .response_format
            .as_deref()
            .and_then(|json| parse_knob(json, "response_format")),
        sampling: cfg
            .sampling
            .as_ref()
            .map(|value| crate::rpc::types::NativeSampling {
                temperature: value.temperature,
                top_p: value.top_p,
            }),
        determinism: cfg
            .determinism
            .as_ref()
            .map(|value| crate::rpc::types::NativeDeterminism { seed: value.seed }),
        extra_params: cfg
            .extra_params
            .as_deref()
            .and_then(|json| parse_knob(json, "extra_params")),
        // The JS host resolves antigravity itself; the addon never carried a
        // binary path or bridge limits over the napi boundary.
        binary_path: None,
        antigravity: None,
    };
    llm
}

fn parse_knob<T: serde::de::DeserializeOwned>(json: &str, field: &str) -> Option<T> {
    match serde_json::from_str(json) {
        Ok(value) => Some(value),
        Err(error) => {
            tracing::warn!(field, %error, "ignoring an unparseable native request knob");
            None
        }
    }
}

/// Ask a running turn (identified by `turn_id`) to stop. The flag is
/// observed by the turn loop between LLM/tool steps; if the turn has already
/// finished this is a no-op.
#[napi]
pub fn cancel_turn(turn_id: String) -> napi::Result<()> {
    guard_sync_panic(|| {
        if let Some(cancel) = CANCEL_MAP
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&turn_id)
        {
            cancel.trigger();
        }
        // v2 `interruptReason`: this entry point is the user's stop, so only
        // this path arms the interruption reminder. A teardown flips the same
        // cancel flag without ever reaching here.
        if let Ok(entry) = session_entry(&turn_id) {
            entry
                .user_cancelled
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        Ok(())
    })
}

// ── napi exported functions ────────────────────────────────────────────────

/// Run a single turn of the agent loop via napi.
///
/// The two JS callbacks follow the **callback registry** pattern:
/// each receives a single `callbackId: number`. The JS side must:
/// 1. Call `getCallbackPayload(id)` to fetch the JSON request payload
/// 2. Process the request
/// 3. Call `resolveCallback(id, error?, result?)` to resolve
///
/// * `llm_chat_cb` — receives callback ID, fetches `LlmChatRequest` JSON
/// * `execute_tool_cb` — receives callback ID, fetches `ToolExecuteRequest` JSON
/// * `emit_event_cb` — optional; receives callback ID, fetches a JSON event
///   payload. Fire-and-forget: the JS side must NOT call `resolveCallback`.
/// * `ask_question_cb` — optional; receives callback ID, fetches an
///   `AskQuestionRequest` JSON payload and resolves with the host's answer.
/// * `state_read_cb` — optional; receives callback ID, fetches a
///   `StateReadRequest` JSON payload and resolves with the host's state
///   value.
/// * `state_write_cb` — optional; receives callback ID, fetches a
///   `StateWriteRequest` JSON payload and resolves with the host's result
///   state.
/// * `turn_event_cb` — optional; receives callback ID, fetches a
///   `TurnEvent` JSON payload (see `crate::turn_events`) and must NOT resolve
///   it. Only used once the engine owns the turn lifecycle; hosts that drive
///   `run_turn` per turn keep dispatching their own turn events.
///
/// JsFunction is converted to ThreadsafeFunction synchronously, then the
/// async work is dispatched via `env.execute_tokio_future` so the JS event
/// loop stays alive to process TSFN callbacks.
#[napi]
#[allow(clippy::too_many_arguments)]
pub fn run_turn_rust(
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
    #[napi(ts_arg_type = "(callbackId: number) => void")] auth_token_cb: Option<JsFunction>,
) -> napi::Result<JsObject> {
    // ── Convert JsFunction → ThreadsafeFunction synchronously ──────────
    // The TSFN passes only the callback ID (u32). The JS side fetches
    // the payload via getCallbackPayload(id) and resolves via resolveCallback.
    // ErrorStrategy::Fatal: no error-first null prepended, JS receives the id directly.
    let llm_chat_tsfn: ThreadsafeFunction<u32, ErrorStrategy::Fatal> = llm_chat_cb
        .create_threadsafe_function(0, |ctx: ThreadSafeCallContext<u32>| {
            let id = ctx.value;
            let js_num = ctx.env.create_uint32(id)?;
            let args: Vec<napi::JsUnknown> = vec![js_num.into_unknown()];
            Ok(args)
        })?;

    let execute_tool_tsfn: ThreadsafeFunction<u32, ErrorStrategy::Fatal> = execute_tool_cb
        .create_threadsafe_function(0, |ctx: ThreadSafeCallContext<u32>| {
            let id = ctx.value;
            let js_num = ctx.env.create_uint32(id)?;
            let args: Vec<napi::JsUnknown> = vec![js_num.into_unknown()];
            Ok(args)
        })?;

    let emit_event_tsfn: Option<ThreadsafeFunction<u32, ErrorStrategy::Fatal>> = match emit_event_cb
    {
        Some(cb) => Some(
            cb.create_threadsafe_function(0, |ctx: ThreadSafeCallContext<u32>| {
                let id = ctx.value;
                let js_num = ctx.env.create_uint32(id)?;
                let args: Vec<napi::JsUnknown> = vec![js_num.into_unknown()];
                Ok(args)
            })?,
        ),
        None => None,
    };

    let check_permission_tsfn: Option<ThreadsafeFunction<u32, ErrorStrategy::Fatal>> =
        match check_permission_cb {
            Some(cb) => Some(cb.create_threadsafe_function(
                0,
                |ctx: ThreadSafeCallContext<u32>| {
                    let id = ctx.value;
                    let js_num = ctx.env.create_uint32(id)?;
                    let args: Vec<napi::JsUnknown> = vec![js_num.into_unknown()];
                    Ok(args)
                },
            )?),
            None => None,
        };

    let ask_question_tsfn: Option<ThreadsafeFunction<u32, ErrorStrategy::Fatal>> =
        match ask_question_cb {
            Some(cb) => Some(cb.create_threadsafe_function(
                0,
                |ctx: ThreadSafeCallContext<u32>| {
                    let id = ctx.value;
                    let js_num = ctx.env.create_uint32(id)?;
                    let args: Vec<napi::JsUnknown> = vec![js_num.into_unknown()];
                    Ok(args)
                },
            )?),
            None => None,
        };

    let state_read_tsfn: Option<ThreadsafeFunction<u32, ErrorStrategy::Fatal>> = match state_read_cb
    {
        Some(cb) => Some(
            cb.create_threadsafe_function(0, |ctx: ThreadSafeCallContext<u32>| {
                let id = ctx.value;
                let js_num = ctx.env.create_uint32(id)?;
                let args: Vec<napi::JsUnknown> = vec![js_num.into_unknown()];
                Ok(args)
            })?,
        ),
        None => None,
    };

    let state_write_tsfn: Option<ThreadsafeFunction<u32, ErrorStrategy::Fatal>> =
        match state_write_cb {
            Some(cb) => Some(cb.create_threadsafe_function(
                0,
                |ctx: ThreadSafeCallContext<u32>| {
                    let id = ctx.value;
                    let js_num = ctx.env.create_uint32(id)?;
                    let args: Vec<napi::JsUnknown> = vec![js_num.into_unknown()];
                    Ok(args)
                },
            )?),
            None => None,
        };

    let checkpoint_tsfn: Option<ThreadsafeFunction<u32, ErrorStrategy::Fatal>> = match checkpoint_cb
    {
        Some(cb) => Some(
            cb.create_threadsafe_function(0, |ctx: ThreadSafeCallContext<u32>| {
                let id = ctx.value;
                let js_num = ctx.env.create_uint32(id)?;
                let args: Vec<napi::JsUnknown> = vec![js_num.into_unknown()];
                Ok(args)
            })?,
        ),
        None => None,
    };

    let turn_event_tsfn: Option<ThreadsafeFunction<u32, ErrorStrategy::Fatal>> = match turn_event_cb
    {
        Some(cb) => Some(
            cb.create_threadsafe_function(0, |ctx: ThreadSafeCallContext<u32>| {
                let id = ctx.value;
                let js_num = ctx.env.create_uint32(id)?;
                let args: Vec<napi::JsUnknown> = vec![js_num.into_unknown()];
                Ok(args)
            })?,
        ),
        None => None,
    };

    let telemetry_tsfn: Option<ThreadsafeFunction<u32, ErrorStrategy::Fatal>> = match telemetry_cb {
        Some(cb) => Some(
            cb.create_threadsafe_function(0, |ctx: ThreadSafeCallContext<u32>| {
                let id = ctx.value;
                let js_num = ctx.env.create_uint32(id)?;
                let args: Vec<napi::JsUnknown> = vec![js_num.into_unknown()];
                Ok(args)
            })?,
        ),
        None => None,
    };

    let list_tools_tsfn: Option<ThreadsafeFunction<u32, ErrorStrategy::Fatal>> = match list_tools_cb
    {
        Some(cb) => Some(
            cb.create_threadsafe_function(0, |ctx: ThreadSafeCallContext<u32>| {
                let id = ctx.value;
                let js_num = ctx.env.create_uint32(id)?;
                let args: Vec<napi::JsUnknown> = vec![js_num.into_unknown()];
                Ok(args)
            })?,
        ),
        None => None,
    };

    let auth_token_tsfn: Option<ThreadsafeFunction<u32, ErrorStrategy::Fatal>> = match auth_token_cb
    {
        Some(cb) => Some(
            cb.create_threadsafe_function(0, |ctx: ThreadSafeCallContext<u32>| {
                let id = ctx.value;
                let js_num = ctx.env.create_uint32(id)?;
                let args: Vec<napi::JsUnknown> = vec![js_num.into_unknown()];
                Ok(args)
            })?,
        ),
        None => None,
    };

    // ── Dispatch async work via execute_tokio_future ───────────────────
    // The future is Send because JsFunction has been converted to TSFN
    // and dropped from scope before the async block.
    env.execute_tokio_future(
        guard_async_panic(async move {
            run_turn_rust_impl(
                params,
                llm_chat_tsfn,
                execute_tool_tsfn,
                emit_event_tsfn,
                check_permission_tsfn,
                ask_question_tsfn,
                state_read_tsfn,
                state_write_tsfn,
                checkpoint_tsfn,
                turn_event_tsfn,
                telemetry_tsfn,
                list_tools_tsfn,
                auth_token_tsfn,
            )
            .await
        }),
        |env: &mut Env, val: JsRunTurnResult| js_object_from_run_turn_result(env, val),
    )
}

/// The TSFN set the JS host wires for one engine attachment — per turn today
/// (`run_turn_rust`), per session once the M1d session handle lands.
pub(crate) struct EngineCallbackTsfns {
    pub(crate) llm_chat: ThreadsafeFunction<u32, ErrorStrategy::Fatal>,
    pub(crate) execute_tool: ThreadsafeFunction<u32, ErrorStrategy::Fatal>,
    pub(crate) emit_event: Option<ThreadsafeFunction<u32, ErrorStrategy::Fatal>>,
    pub(crate) check_permission: Option<ThreadsafeFunction<u32, ErrorStrategy::Fatal>>,
    pub(crate) ask_question: Option<ThreadsafeFunction<u32, ErrorStrategy::Fatal>>,
    pub(crate) state_read: Option<ThreadsafeFunction<u32, ErrorStrategy::Fatal>>,
    pub(crate) state_write: Option<ThreadsafeFunction<u32, ErrorStrategy::Fatal>>,
    pub(crate) checkpoint: Option<ThreadsafeFunction<u32, ErrorStrategy::Fatal>>,
    pub(crate) turn_event: Option<ThreadsafeFunction<u32, ErrorStrategy::Fatal>>,
    pub(crate) telemetry: Option<ThreadsafeFunction<u32, ErrorStrategy::Fatal>>,
    pub(crate) list_tools: Option<ThreadsafeFunction<u32, ErrorStrategy::Fatal>>,
    /// Optional current-goal channel (wired by the session handle; the
    /// per-turn legacy entry reads the goal from `JsRunTurnParams`).
    pub(crate) goal: Option<ThreadsafeFunction<u32, ErrorStrategy::Fatal>>,
    /// Optional bash-parse channel (`host/parse_bash`) for the AGENTS.md
    /// reminder's operand directories; absent keeps the reminder cwd-only.
    pub(crate) parse_bash: Option<ThreadsafeFunction<u32, ErrorStrategy::Fatal>>,
    /// Optional OAuth token channel (`host/auth_token`): the native transport
    /// asks the host for a bearer token when its config names an
    /// `auth_provider`. Absent means OAuth-managed providers are unsupported.
    pub(crate) auth_token: Option<ThreadsafeFunction<u32, ErrorStrategy::Fatal>>,
    /// The cancellation flag the host callbacks observe (per turn today; the
    /// session handle passes its own per-turn flag through `cancel_turn`).
    pub(crate) cancellation: Option<Arc<AtomicBool>>,
}

/// One interactive cron dispatcher per workspace root.
///
/// The cron registry is workspace-scoped (the state-bridge `cron` domain), so
/// a per-session tick loop would fire the same job once per live session. One
/// dispatcher per workspace ticks it; each tick resolves a *live* session for
/// the workspace through [`live_session_for_workspace`], so a settings rebuild
/// (which replaces the session handle) does not lose the dispatcher.
static CRON_DISPATCHERS: LazyLock<Mutex<std::collections::HashSet<String>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashSet::new()));

/// The host's cron registry as scheduler entries (mirrors the print path's
/// `session::read_cron_registry`).
async fn read_cron_registry(
    callbacks: &Arc<dyn crate::callbacks::HostCallbacks>,
) -> Option<Vec<crate::cron::scheduler::CronEntry>> {
    let response = callbacks
        .state_read(StateReadRequest {
            domain: "cron".into(),
            key: "cron".into(),
            turn_id: String::new(),
            tool_call_id: String::new(),
        })
        .await
        .ok()?;
    Some(
        response
            .value
            .as_array()?
            .iter()
            .filter_map(|task| {
                Some(crate::cron::scheduler::CronEntry {
                    id: task.get("id")?.as_str()?.to_string(),
                    cron: task.get("cron")?.as_str()?.to_string(),
                    prompt: task.get("prompt")?.as_str()?.to_string(),
                    recurring: task
                        .get("recurring")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(true),
                    session_id: None,
                    created_at: task.get("createdAt").and_then(serde_json::Value::as_i64),
                })
            })
            .collect(),
    )
}

/// Start the workspace's interactive cron dispatcher once.
///
/// The native host has no daemon tick loop, so the engine owns one: every 15s
/// it reads the cron registry through a live session, fires due entries
/// (publishing `cron.fired` for the TUI's cron card and enqueuing the
/// `<cron-fire>` prompt as a turn), and deletes one-shot / stale jobs. Before
/// this the CLI's `CronCreate` jobs never fired at all.
pub(crate) fn spawn_cron_dispatcher(workspace: &str) {
    if workspace.is_empty() {
        return;
    }
    {
        let mut dispatchers = CRON_DISPATCHERS.lock().unwrap_or_else(|e| e.into_inner());
        if !dispatchers.insert(workspace.to_string()) {
            return;
        }
    }
    let workspace = workspace.to_string();
    tokio::spawn(async move {
        let mut last_tick = crate::session::now_ms_epoch();
        loop {
            tokio::time::sleep(Duration::from_secs(15)).await;
            let now = crate::session::now_ms_epoch();
            let Some(live) = live_session_for_workspace(&workspace) else {
                // No live session to run a fired job in; leave `last_tick`
                // alone so a job due while nothing was listening still fires
                // once a session comes back.
                continue;
            };
            let callbacks = live.callbacks.clone();
            let Some(entries) = read_cron_registry(&callbacks).await else {
                last_tick = now;
                continue;
            };
            let mut scheduler = crate::cron::scheduler::CronScheduler::new(
                entries,
                crate::session::local_utc_offset_minutes(),
            );
            // v2 `tickCron`: a round that would fire into a running agent loop
            // is skipped whole, and `last_tick` stays put so the next round
            // re-covers the same window — defer, never drop. The registry is
            // re-read every round, so "next round" also re-reads it.
            let fired = scheduler.tick_if_idle(last_tick, now, |_| {
                live.session.status().active_turn_id.is_some()
            });
            let Some(fired) = fired else {
                continue;
            };
            last_tick = now;
            for fired_entry in fired {
                let entry = &fired_entry.entry;
                let origin = crate::session::cron_fire_origin(
                    entry,
                    fired_entry.coalesced_count,
                    fired_entry.stale,
                );
                callbacks.emit_event(serde_json::json!({
                    "type": "cron.fired",
                    "origin": origin,
                    "prompt": entry.prompt,
                }));
                // One-shot jobs auto-delete after firing; stale recurring ones
                // go the same way (v2 removes them after firing).
                if !entry.recurring || fired_entry.stale {
                    let _ = callbacks
                        .state_write(StateWriteRequest {
                            domain: "cron".into(),
                            key: "cron".into(),
                            value: serde_json::json!({ "action": "delete", "id": entry.id }),
                            undoable: false,
                            turn_id: String::new(),
                            tool_call_id: String::new(),
                        })
                        .await;
                }
                let rendered = crate::session::render_cron_fire(
                    entry,
                    fired_entry.coalesced_count,
                    fired_entry.stale,
                );
                let _ = live.session.enqueue_turn(TurnRequest {
                    prompt: LLMMessage {
                        role: "user".into(),
                        content: rendered.clone(),
                        ..Default::default()
                    },
                    admission: Admission::NewTurn,
                    input: serde_json::json!([{ "type": "text", "text": rendered }]),
                    origin,
                });
            }
        }
    });
}

/// The addon entry's view of the shared engine pipeline
/// (`kimi_agent::pipeline`). The chain itself — counting wrapper, native-tool
/// wrapper and its guards, LLM selection — lives there once; this normalizes
/// `JsRunTurnParams` into a `PipelineSpec` and applies the addon's host policy:
/// the process-wide subagent manager refreshed per turn, no cancel slot, and the
/// process-wide MCP manager resolved from `params.mcp_servers`.
pub(crate) async fn build_engine_pipeline(
    params: &JsRunTurnParams,
    tsfns: EngineCallbackTsfns,
    parent_cancel: Option<crate::subagent::types::ParentCancel>,
    parent_cancel_slot: Option<Arc<std::sync::Mutex<Option<crate::subagent::types::ParentCancel>>>>,
    steer_slot: Option<Arc<std::sync::Mutex<Option<crate::subagent::types::ParentCancel>>>>,
) -> napi::Result<(EnginePipeline, Option<Arc<McpStatusBridge>>)> {
    // Session profile catalog snapshot (P46): refresh the process-wide
    // manager's definitions per turn so the native `Agent` tool sees the
    // host's builtin/workspace/user profiles (plugin and external-backend
    // profiles never arrive here — those calls fall back to the host).
    if let Some(profiles) = &params.subagent_profiles {
        let wires: Vec<SubagentProfileWire> = profiles
            .iter()
            .map(|p| SubagentProfileWire {
                name: p.name.clone(),
                description: p.description.clone().unwrap_or_default(),
                when_to_use: p.when_to_use.clone(),
                system_prompt: p.system_prompt.clone().unwrap_or_default(),
                tools: p.tools.clone().unwrap_or_default(),
                disallowed_tools: p.disallowed_tools.clone().unwrap_or_default(),
                prompt_prefix: p.prompt_prefix.clone(),
                summary_policy: p.summary_policy_json.as_deref().and_then(|j| {
                    serde_json::from_str::<crate::subagent::types::SummaryPolicy>(j).ok()
                }),
                subagents: p.sub_agents.clone(),
            })
            .collect();
        SUBAGENT_MANAGER.register_profile_snapshot(&wires).await;
        // v2 #4152: a fork inherits the caller's profile, so the main
        // agent's bound profile must be known engine-side (None keeps the
        // default `agent`).
        SUBAGENT_MANAGER
            .set_main_profile_name(params.agent_profile.as_deref())
            .await;
    }

    // Native MCP servers (P73): the manager is process-wide and keyed by the
    // resolved server set, so every session built from the same configuration
    // shares one set of connections (v2's workspace-scoped
    // `WorkspaceMcpService`, workspaceMcpService.ts:61-83). Building one per
    // session re-spawned every server on `/new` — a stdio server that boots a
    // language runtime cost seconds each time.
    //
    // The connects are started, not awaited: the tool table reads the manager
    // live (`callbacks.rs::list_tools`), so a server that connects later still
    // contributes its tools, and the turn awaits readiness there (v2
    // `onWillBeginStep`, mcpService.ts:70-73). A server that fails to connect
    // is skipped rather than failing the turn.
    let plugin_mcp = plugin_mcp_configs();
    let host_mcp = params.mcp_servers.as_deref().unwrap_or_default();
    let mut servers: Vec<McpServerSpec> = Vec::new();
    for cfg in host_mcp {
        let recipe = match cfg.transport.as_str() {
            "stdio" => cfg.command.as_ref().map(|cmd| McpServerRecipe::Stdio {
                command: cmd.clone(),
                args: cfg.args.clone().unwrap_or_default(),
                env: cfg.env.clone().unwrap_or_default(),
                cwd: cfg.cwd.clone(),
            }),
            "sse" => cfg.url.as_ref().map(|url| McpServerRecipe::Sse {
                url: url.clone(),
                headers: cfg.headers.clone().unwrap_or_default(),
                bearer_token_env_var: cfg.bearer_token_env_var.clone(),
            }),
            "http" => cfg.url.as_ref().map(|url| McpServerRecipe::Http {
                url: url.clone(),
                headers: cfg.headers.clone().unwrap_or_default(),
                bearer_token_env_var: cfg.bearer_token_env_var.clone(),
            }),
            "mock" if allow_mock_mcp_transport() => Some(McpServerRecipe::Mock),
            _ => None,
        };
        let Some(recipe) = recipe else {
            // Without this the server simply vanishes from the roster, and a
            // mistyped `transport` is indistinguishable from a working config.
            tracing::warn!(
                server = %cfg.name,
                transport = %cfg.transport,
                "skipping MCP server: unsupported or unenabled transport"
            );
            continue;
        };
        servers.push((
            cfg.name.clone(),
            recipe,
            McpServerOptions {
                enabled: cfg.enabled.unwrap_or(true),
                enabled_tools: cfg.enabled_tools.clone(),
                disabled_tools: cfg.disabled_tools.clone(),
                startup_timeout_ms: cfg.startup_timeout_ms.map(u64::from),
                tool_timeout_ms: cfg.tool_timeout_ms.map(u64::from),
                deferred: cfg.deferred,
                oauth: cfg.auth.as_deref().map(|auth| auth == "oauth"),
            },
        ));
    }
    // Enabled plugins contribute their own MCP servers. A server the user
    // disabled for that plugin never reaches here (`plugin_mcp_configs`
    // filters it), and the name is namespaced so two plugins can declare the
    // same server name.
    for cfg in &plugin_mcp {
        let recipe = match cfg.transport.as_str() {
            "stdio" => cfg.command.as_ref().map(|cmd| McpServerRecipe::Stdio {
                command: cmd.clone(),
                args: cfg.args.clone(),
                env: cfg.env.clone(),
                cwd: cfg.cwd.clone(),
            }),
            "sse" => cfg.url.as_ref().map(|url| McpServerRecipe::Sse {
                url: url.clone(),
                headers: cfg.headers.clone(),
                bearer_token_env_var: None,
            }),
            "http" => cfg.url.as_ref().map(|url| McpServerRecipe::Http {
                url: url.clone(),
                headers: cfg.headers.clone(),
                bearer_token_env_var: None,
            }),
            _ => None,
        };
        let Some(recipe) = recipe else {
            tracing::warn!(
                server = %cfg.name,
                transport = %cfg.transport,
                "skipping plugin MCP server: unsupported transport"
            );
            continue;
        };
        servers.push((cfg.name.clone(), recipe, McpServerOptions::default()));
    }
    let mcp_defaults = params
        .mcp_defaults
        .as_ref()
        .map(|defaults| crate::config::McpConfig {
            startup_timeout_ms: defaults.startup_timeout_ms.map(u64::from),
            tool_timeout_ms: defaults.tool_timeout_ms.map(u64::from),
            default_deferred: defaults.default_deferred,
            ..Default::default()
        });
    let mcp_manager = (!servers.is_empty()).then(|| {
        shared_mcp_manager(
            servers,
            params
                .mcp_oauth_dir
                .as_deref()
                .map(std::path::PathBuf::from),
            mcp_defaults,
            // v2 `stdioCwd`: a stdio server whose config omits `cwd` starts in
            // the session's workspace, not in whatever directory the host
            // process happened to launch from.
            params
                .workspace_root
                .as_deref()
                .map(std::path::PathBuf::from),
            // v2 `resolveClientName`: the identity every MCP client announces.
            params.mcp_client_name.clone(),
        )
    });

    // `[subagent]`/`[background]` print defaults (docs config-files.md): an
    // *unset* wall-clock timeout means "no timeout" in print mode — the
    // settle phase waits for background work instead of the clock killing
    // it. The settle policy itself rides the SessionConfig at session build.
    let print_mode = print_background_policy(params).is_some();

    // Host-resolved swarm timeout (v2 `resolveSwarmTimeoutMs`): the
    // process-wide manager carries it so the native `AgentSwarm` tool reads
    // it at execution time. `try_from` drops negatives without wrapping;
    // `0` rides through as "explicitly no timeout" (v2 `taskService` arms
    // only when `timeoutMs > 0`).
    SUBAGENT_MANAGER.set_swarm_timeout_ms(crate::pipeline::print_timeout_default(
        params
            .swarm_timeout_ms
            .and_then(|timeout| u64::try_from(timeout).ok()),
        print_mode,
    ));

    // Host-resolved `[services.moonshot_*]` backends (v2 `configSection.ts`):
    // the tools read the process-global seam at execution time. Always
    // installed — including `None` — so a backend resolved for one session
    // never leaks into a later pipeline that resolves none.
    crate::tools::web_search::set_service_config(params.web_search.as_ref().map(|cfg| {
        crate::tools::web_search::WebSearchServiceConfig {
            base_url: cfg.base_url.clone(),
            api_key: cfg.api_key.clone(),
            custom_headers: cfg.custom_headers.clone().unwrap_or_default(),
        }
    }));
    crate::tools::fetch_url::set_service_config(params.web_fetch.as_ref().map(|cfg| {
        crate::tools::fetch_url::WebFetchServiceConfig {
            base_url: cfg.base_url.clone(),
            api_key: cfg.api_key.clone(),
            custom_headers: cfg.custom_headers.clone().unwrap_or_default(),
        }
    }));
    crate::tools::web_search::set_bing_api_config(params.bing_api.as_ref().map(|cfg| {
        crate::tools::web_search::BingApiConfig {
            base_url: cfg.base_url.clone(),
            api_key: cfg.api_key.clone(),
        }
    }));

    let spec = PipelineSpec {
        // A host that hands over a session rather than a prompt gets the engine's
        // own prompt. The napi entry points used to pass a one-line stub
        // ("You are Kimi Code, ...") straight through to `messages[0]`, so the
        // interactive CLI never saw `prompt/system.md` — no AGENTS.md cascade, no
        // skills catalog, no environment/CWD listing, no profile role — and the
        // memory-filing gate (`MEMORY_SECTION_MARKER`) never matched either.
        // Treat the stub and the `"sys"` sentinel the same way `ServerEngine::
        // session_spec` already does, and build the real prompt from the
        // workspace the caller passed.
        system_prompt: build_session_system_prompt(params),
        model_name: params.model_name.clone(),
        providers: params
            .providers
            .as_ref()
            .map(|providers| {
                providers
                    .iter()
                    .map(|p| PipelineProvider {
                        name: p.name.clone(),
                        system_prompt: p.system_prompt.clone(),
                        model: p.model.clone(),
                        native: p.native.as_ref().map(native_llm_config_from_js),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        native_llm: params.native_llm.as_ref().map(native_llm_config_from_js),
        workspace_root: params.workspace_root.clone(),
        native_tools: params.native_tools.unwrap_or(false),
        extra_roots: params.additional_dirs.clone().unwrap_or_default(),
        rust_self_contained: params.rust_self_contained.unwrap_or(false),
        shell_path: params.shell_path.clone(),
        // The host hands the permission policy over as JSON (napi has no typed
        // struct for it). A parse failure used to be swallowed by `.ok()`, which
        // dropped the *whole* policy: with no local engine, every native tool
        // call round-trips to the host's `check_permission`, so the user gets an
        // approval prompt no matter which mode the UI shows. The failure and the
        // symptom looked completely unrelated — and nothing was logged, so the
        // engine's own view of the mode was unobservable. Report both.
        policy_snapshot: params.policy_snapshot_json.as_deref().and_then(|json| {
            match serde_json::from_str::<crate::permission::PolicySnapshot>(json) {
                Ok(snapshot) => {
                    tracing::debug!(mode = ?snapshot.mode, "native policy snapshot accepted");
                    Some(snapshot)
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        "native policy snapshot rejected; no local permission engine, \
                         so every tool call will ask the host for permission"
                    );
                    None
                }
            }
        }),
        github_token: params.github_token.clone(),
        github_base_url: params.github_base_url.clone(),
        // `0` from the host means "no timeout" (v2 `taskService` arms only
        // when `timeoutMs > 0`); an *unset* value defaults to no timeout in
        // print mode (docs config-files.md), the 2h engine default otherwise.
        subagent_timeout_ms: crate::pipeline::print_timeout_default(
            params
                .subagent_timeout_ms
                .and_then(|timeout| u64::try_from(timeout).ok()),
            print_mode,
        ),
        agent_tool_veto: params.agent_tool_veto.clone(),
        tools_veto: params.tools_veto.clone(),
        todo_tool_veto: params.todo_tool_veto.clone(),
        tower_worktree_root: params.tower_worktree_root.clone(),
        // The host resolves the experiment itself (env + config) and passes the
        // resulting flag down; falling back to the env switch keeps a direct
        // napi caller working without plumbing a new param.
        notify_user_enabled: params.notify_user_enabled.unwrap_or(true),
        tower_enabled: params.tower_enabled.unwrap_or_else(|| {
            crate::tools::tower::paths::tower_enabled(
                crate::tools::tower::paths::tower_env_switch(),
                None,
            )
        }),
        // The host resolves `[experimental].tool_select` itself (the same
        // precedence as every experimental flag: env > config > master env >
        // default); falling back to `false` keeps a direct napi caller on the
        // pre-disclosure behaviour: every tool advertised inline.
        tool_select: params.tool_select.unwrap_or(false),
        // Same in-file convention as `policy_snapshot` above: report the bad
        // value and fail closed rather than swallow it. `sandboxMode` is a
        // documented optional *string* in `napi-contract.d.ts`, so it cannot be
        // rejected by deserialization the way `RunTurnParams.sandbox_mode` now
        // is; `parse` is the fail-closed shim, so an unknown spelling lands on
        // `ReadOnly` instead of silently lifting every boundary.
        sandbox_mode: params.sandbox_mode.as_deref().map(|mode_str| {
            match crate::tools::sandbox::SandboxMode::validate(mode_str) {
                Ok(mode) => mode,
                Err(error) => {
                    tracing::warn!("{error}; falling back to the most restrictive mode");
                    crate::tools::sandbox::SandboxMode::parse(mode_str)
                }
            }
        }),
        sandbox_policy: params.sandbox_mode.as_deref().map(|mode_str| {
            let mode = crate::tools::sandbox::SandboxMode::parse(mode_str);
            let root = params.workspace_root.clone().unwrap_or_default();
            // `additionalDirs` are part of the authorized boundary, not just a
            // toolset search path.
            crate::tools::sandbox::SandboxExecutionPolicy::new(mode, root)
                .with_extra_roots(params.additional_dirs.clone().unwrap_or_default())
        }),
        caller_agent_id: params.caller_agent_id.clone(),
        session_id: params.session_id.clone(),
        secondary_model: params.secondary_model_json.as_deref().and_then(|json| {
            serde_json::from_str::<crate::rpc::types::SecondaryModelPool>(json).ok()
        }),
        image_read_byte_budget: params
            .image_read_byte_budget
            .and_then(|value| u64::try_from(value).ok()),
        image_max_edge_px: params
            .image_max_edge_px
            .and_then(|value| u32::try_from(value).ok()),
        model_capabilities: params.model_capabilities.clone(),
        // Enabled plugins contribute skill roots; the host's own
        // `extra_skill_dirs` arrive through the config the host resolved.
        skill_dirs: Vec::new(),
        plugin_skill_dirs: plugin_skill_dirs(),
        merge_all_available_skills: crate::config::resolved_merge_all_available_skills(),
        background: crate::storage::BackgroundLimits::from_wire(
            params
                .kill_grace_period_ms
                .and_then(|value| u64::try_from(value).ok()),
            params
                .max_running_tasks
                .and_then(|value| u64::try_from(value).ok()),
            params.bash_auto_background_on_timeout,
            crate::pipeline::print_timeout_default(
                params
                    .bash_task_timeout_s
                    .and_then(|value| u64::try_from(value).ok()),
                print_mode,
            ),
            params.keep_alive_on_exit,
        ),
    };

    let host_callbacks: Arc<dyn crate::callbacks::HostCallbacks> = Arc::new(NapiHostCallbacks {
        llm_chat_fn: Arc::new(tsfns.llm_chat),
        execute_tool_fn: Arc::new(tsfns.execute_tool),
        emit_event_fn: tsfns.emit_event.map(Arc::new),
        check_permission_fn: tsfns.check_permission.map(Arc::new),
        ask_question_fn: tsfns.ask_question.map(Arc::new),
        state_read_fn: tsfns.state_read.map(Arc::new),
        checkpoint_fn: tsfns.checkpoint.map(Arc::new),
        state_write_fn: tsfns.state_write.map(Arc::new),
        turn_event_fn: tsfns.turn_event.map(Arc::new),
        telemetry_fn: tsfns.telemetry.map(Arc::new),
        list_tools_fn: tsfns.list_tools.map(Arc::new),
        goal_fn: tsfns.goal.map(Arc::new),
        parse_bash_fn: tsfns.parse_bash.map(Arc::new),
        auth_token_fn: tsfns.auth_token.map(Arc::new),
        cancellation: tsfns.cancellation,
    });

    // Swarm mode the host asked for (v2 `IAgentSwarmService.enter` / `.exit`).
    // Applied here, before the pipeline runs, so the `swarm_mode` injection
    // sees it at the turn head — and announced through the same event pair the
    // tool path emits, so the host's indicator tracks the engine's real mode
    // rather than its own flag. Without this the napi surface had no way to
    // express the mode at all and a `/swarm` toggle was status-only.
    if let Some(desired) = params.swarm_mode {
        let trigger = match params.swarm_mode_trigger.as_deref() {
            Some("task") => crate::swarm::mode::SwarmModeTrigger::Task,
            Some("tool") => crate::swarm::mode::SwarmModeTrigger::Tool,
            _ => crate::swarm::mode::SwarmModeTrigger::Manual,
        };
        crate::swarm::mode::set_swarm_mode(
            host_callbacks.as_ref(),
            crate::callbacks::MAIN_AGENT_ID,
            desired.then_some(trigger),
        );
    }
    let (plugin_sections, plugin_hooks) = plugin_contributions();
    let pipeline = pipeline::build_engine_pipeline(
        &spec,
        host_callbacks.clone(),
        PipelineHost {
            subagent_manager: SUBAGENT_MANAGER.clone(),
            plugin_sections,
            plugin_hooks,
            // The addon's runner is process-wide: sharing it between handle
            // builds keeps a background task visible across a rebuild and lets
            // the session pump watch the same notification queue its tools
            // settle into.
            task_runner: SUBAGENT_MANAGER.get_task_runner_sync(),
            parent_cancel,
            parent_cancel_slot,
            steer_slot,
            mcp_manager,
            event_bus: None,
            // Background-task lifecycle reaches the TUI through the same host
            // callbacks the turn events use: the pipeline owns its own runner,
            // so without this sink its `event.task.*` go nowhere.
            task_event_sink: Some({
                let callbacks = host_callbacks.clone();
                Arc::new(move |_session, event| callbacks.emit_event(event))
            }),
        },
    )
    .await
    .map_err(|error| napi::Error::from_reason(error.message))?;
    // Forward MCP status transitions on the raw host callbacks, not the
    // counting wrapper: an MCP transition is not turn work, so it must not
    // inflate the turn's event-count telemetry, and it must not ride the event
    // bus (the standalone server's web vocabulary has no such event).
    let mcp_status = attach_mcp_status(&pipeline.mcp_manager, host_callbacks);
    Ok((pipeline, mcp_status))
}

/// Inner async implementation — all captured values are `Send`.
#[allow(clippy::too_many_arguments)]
async fn run_turn_rust_impl(
    params: JsRunTurnParams,
    llm_chat_tsfn: ThreadsafeFunction<u32, ErrorStrategy::Fatal>,
    execute_tool_tsfn: ThreadsafeFunction<u32, ErrorStrategy::Fatal>,
    emit_event_tsfn: Option<ThreadsafeFunction<u32, ErrorStrategy::Fatal>>,
    check_permission_tsfn: Option<ThreadsafeFunction<u32, ErrorStrategy::Fatal>>,
    ask_question_tsfn: Option<ThreadsafeFunction<u32, ErrorStrategy::Fatal>>,
    state_read_tsfn: Option<ThreadsafeFunction<u32, ErrorStrategy::Fatal>>,
    state_write_tsfn: Option<ThreadsafeFunction<u32, ErrorStrategy::Fatal>>,
    checkpoint_tsfn: Option<ThreadsafeFunction<u32, ErrorStrategy::Fatal>>,
    turn_event_tsfn: Option<ThreadsafeFunction<u32, ErrorStrategy::Fatal>>,
    telemetry_tsfn: Option<ThreadsafeFunction<u32, ErrorStrategy::Fatal>>,
    list_tools_tsfn: Option<ThreadsafeFunction<u32, ErrorStrategy::Fatal>>,
    auth_token_tsfn: Option<ThreadsafeFunction<u32, ErrorStrategy::Fatal>>,
) -> napi::Result<JsRunTurnResult> {
    // Register the turn's cancellation signal up front so a JS-side
    // `cancel_turn` can interrupt host callbacks (permission waits
    // included) while this turn is in flight — and, since P51, abort a
    // foreground subagent immediately.
    let turn_id = params.turn_id.clone();
    // The interruption-reminder trigger for this turn: whether the session's
    // previous turn settled `Aborted` (read-and-clear; the next settle writes
    // it again from the outcome).
    let last_turn_aborted = session_entry(&turn_id)
        .ok()
        .map(|entry| entry.last_turn_aborted.clone());
    // Whether the user asked for this turn to stop (v2 `interruptReason`).
    // Read-and-cleared at settle; the teardown path never sets it.
    let user_cancelled = session_entry(&turn_id)
        .ok()
        .map(|entry| entry.user_cancelled.clone());
    let previous_turn_aborted = last_turn_aborted
        .as_ref()
        .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Relaxed));
    let cancellation = Arc::new(AtomicBool::new(false));
    let parent_cancel = crate::subagent::types::ParentCancel::from_flag(cancellation.clone());
    // The guard, not a trailing `remove`: the pipeline build below can return
    // early with `?`, and a dropped napi future never reaches a trailing
    // statement at all.
    let _cancel_guard = MapEntryGuard::insert(&CANCEL_MAP, turn_id.clone(), parent_cancel.clone());

    let (pipeline, _mcp_status) = build_engine_pipeline(
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
            // The legacy per-turn entry reads the goal from params, so the
            // callback channel stays unwired (goal() fails open).
            goal: None,
            // Same for the bash parser: the legacy one-shot entry wires no
            // `host/parse_bash` channel, so its AGENTS.md reminder keeps the
            // cwd-only shape (v2's own parse-failure fallback).
            parse_bash: None,
            auth_token: auth_token_tsfn,
            cancellation: Some(cancellation.clone()),
        },
        Some(parent_cancel.clone()),
        None,
        // The legacy one-shot entry owns no steer queue: nothing steers into
        // it mid-turn, so there is no signal to fire.
        None,
    )
    .await?;
    let llm = pipeline.llm;
    let callbacks = pipeline.callbacks;
    let turn_event_count = pipeline.turn_event_count;
    let native_tool_count = pipeline.native_tool_count;
    let hook_guard = pipeline.hook_guard.clone();
    // Read before `params.goal` is moved out below.
    let max_context_tokens =
        compaction_window(params.native_llm.as_ref(), params.max_context_tokens);

    let messages: Vec<LLMMessage> = params
        .messages
        .iter()
        .map(|m| LLMMessage {
            role: m.role.clone(),
            content: m.content.clone(),
            blocks: parse_host_json(m.blocks_json.as_deref(), "message blocks", &m.role),
            tool_calls: parse_host_json(
                m.tool_calls_json.as_deref(),
                "message tool calls",
                &m.role,
            ),
            tool_call_id: m.tool_call_id.clone(),
            prompt_id: None,
            origin: None,
        })
        .collect();

    // A malformed schema used to become `Value::Null` and ride into the request
    // body verbatim, where the provider rejected the whole turn with a 400 that
    // named no tool. Fail here instead, naming the tool.
    let tool_defs = tool_defs_from_wire(&params.tools)?;

    let goal = params.goal.map(|g| GoalContext {
        goal_id: g.goal_id,
        objective: g.objective,
        status: match g.status.as_str() {
            "active" => GoalStatus::Active,
            "paused" => GoalStatus::Paused,
            "blocked" => GoalStatus::Blocked,
            "complete" => GoalStatus::Complete,
            "budgetLimited" => GoalStatus::BudgetLimited,
            "usageLimited" => GoalStatus::UsageLimited,
            _ => GoalStatus::Active,
        },
        token_budget: g.token_budget,
        turn_budget: g.turn_budget,
        wall_clock_budget_ms: g.wall_clock_budget_ms,
        wall_clock_ms: g.wall_clock_ms,
        tokens_used: g.tokens_used,
        turns_used: g.turns_used,
        terminal_reason: g.terminal_reason,
        updated_at: g.updated_at,
    });

    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        max_attempts: params.max_attempts,
        turn_id: turn_id.clone(),
        llm: llm.as_ref(),
        messages,
        tools: &[],
        tool_defs,
        // None = unbounded, mirroring the JS loop (which only stops on a
        // configured `maxStepsPerTurn`).
        max_steps: params.max_steps.unwrap_or(u32::MAX),
        max_context_tokens,
        // The napi host passes its own `[loop_control]` caps through
        // `max_attempts` and `compaction_max_attempts`; `None` on either
        // keeps the engine default.
        compaction_max_attempts: params.compaction_max_attempts,
        permission_mode: pipeline.permission_mode,
        goal,
        cancellation: Some(cancellation),
        hook_guard: hook_guard.clone(),
        media: Some(&pipeline.media),
        media_dropped: Some(pipeline.media_dropped.clone()),
        media_recovery: Some(pipeline.media_recovery.clone()),
        toolset: pipeline.toolset.clone(),
        previous_turn_aborted,
    };

    let telemetry_context = params.telemetry.map(|t| TelemetryContext {
        mode: t.mode,
        provider_type: t.provider_type,
        protocol: t.protocol,
        thinking_effort: t.thinking_effort,
        enabled_plugins: t.enabled_plugins,
    });
    let result = match telemetry_context {
        Some(context) => run_turn_with_telemetry(input, context, &callbacks).await,
        None => run_turn_continued(input, &callbacks).await,
    };

    let result = result.map_err(|e| napi::Error::from_reason(format!("run_turn failed: {e}")))?;
    if let Some(flag) = &last_turn_aborted {
        let user_cancelled = user_cancelled
            .as_ref()
            .is_some_and(|user| user.swap(false, std::sync::atomic::Ordering::Relaxed));
        flag.store(
            crate::injection::interruption_reminder::arms_reminder(
                result.stop_reason == crate::turn_loop::types::LoopTurnStopReason::Aborted,
                user_cancelled,
            ),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    Ok(js_run_turn_result(
        result,
        &turn_event_count,
        &native_tool_count,
        llm.transport(),
    ))
}

/// Project a `TurnResult` onto the napi result shape. The counters come from
/// the pipeline's wrappers (per turn today; per session for the M1d handle).
pub(crate) fn js_run_turn_result(
    result: TurnResult,
    turn_event_count: &std::sync::atomic::AtomicU32,
    native_tool_count: &std::sync::atomic::AtomicU32,
    llm_transport: &str,
) -> JsRunTurnResult {
    JsRunTurnResult {
        stop_reason: format!("{:?}", result.stop_reason),
        steps: result.steps,
        input_tokens: result.usage.input_tokens,
        output_tokens: result.usage.output_tokens,
        total_tokens: result.usage.total_tokens,
        input_cache_read: result.usage.input_cache_read,
        input_cache_creation: result.usage.input_cache_creation,
        events_emitted: turn_event_count.load(std::sync::atomic::Ordering::Relaxed),
        llm_retries: result.llm_retries,
        llm_transport: llm_transport.to_string(),
        native_tool_calls: native_tool_count.load(std::sync::atomic::Ordering::Relaxed),
    }
}

/// Build the JS object for a `JsRunTurnResult` — shared by the per-turn
/// deferred and the session outcome deferred.
pub(crate) fn js_object_from_run_turn_result(
    env: &mut Env,
    val: JsRunTurnResult,
) -> napi::Result<JsObject> {
    let mut obj = env.create_object()?;
    obj.set_named_property("stopReason", env.create_string_from_std(val.stop_reason)?)?;
    obj.set_named_property("steps", env.create_uint32(val.steps)?)?;
    obj.set_named_property("inputTokens", env.create_uint32(val.input_tokens)?)?;
    obj.set_named_property("outputTokens", env.create_uint32(val.output_tokens)?)?;
    obj.set_named_property("totalTokens", env.create_uint32(val.total_tokens)?)?;
    obj.set_named_property("inputCacheRead", env.create_uint32(val.input_cache_read)?)?;
    obj.set_named_property(
        "inputCacheCreation",
        env.create_uint32(val.input_cache_creation)?,
    )?;
    obj.set_named_property("eventsEmitted", env.create_uint32(val.events_emitted)?)?;
    obj.set_named_property("llmRetries", env.create_uint32(val.llm_retries)?)?;
    obj.set_named_property(
        "llmTransport",
        env.create_string_from_std(val.llm_transport)?,
    )?;
    obj.set_named_property("nativeToolCalls", env.create_uint32(val.native_tool_calls)?)?;
    Ok(obj)
}
