//! Host-callback plumbing: the callback/payload registries, the
//! `NapiHostCallbacks` implementation of `HostCallbacks`, and the napi
//! exports the JS side uses to resolve pending callbacks.

use super::*;
use crate::callbacks::{
    HOST_AUTH_TOKEN_TIMEOUT, HOST_LIST_TOOLS_TIMEOUT, HOST_LLM_TIMEOUT, HOST_TOOL_TIMEOUT,
    HostCallbacks,
};
use crate::rpc::types::{
    AskQuestionRequest, AskQuestionResponse, AuthTokenResponse, BoxFuture, CheckpointRequest,
    ListToolsResponse, LlmChatRequest, LlmChatResponse, PermissionCheckRequest, PermissionDecision,
    StateReadRequest, StateReadResponse, StateWriteRequest, StateWriteResponse, ToolExecuteRequest,
    ToolExecuteResponse,
};
use crate::turn_loop::types::*;
use napi::threadsafe_function::{ErrorStrategy, ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi_derive::napi;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;

// ── Global callback registry ───────────────────────────────────────────────

/// Result channel for a pending callback awaiting resolution from JS.
type PendingCallback = oneshot::Sender<Result<String, String>>;

/// Pending callbacks awaiting resolution from the JS side.
static CALLBACK_REGISTRY: LazyLock<Mutex<HashMap<u32, PendingCallback>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Payload registry — stores the JSON request payloads by callback ID.
/// The JS side fetches the payload via `getCallbackPayload(id)` after
/// receiving the callback ID via TSFN.
///
/// To prevent unbounded growth from unfetched event payloads, the registry
/// is pruned when it exceeds [`PAYLOAD_REGISTRY_MAX_ENTRIES`]. Pruning drops
/// the oldest entries (lowest IDs) first, since IDs are monotonically
/// increasing — which is why this is a `BTreeMap`: iterating a `HashMap`
/// yields IDs in arbitrary order and would drop payloads JS has not fetched
/// yet.
pub(crate) static PAYLOAD_REGISTRY: LazyLock<Mutex<BTreeMap<u32, String>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

/// Maximum number of entries in the payload registry before pruning kicks in.
/// Each entry is a small JSON string; 1000 entries is a generous ceiling that
/// prevents unbounded growth without affecting normal operation.
pub(crate) const PAYLOAD_REGISTRY_MAX_ENTRIES: usize = 1000;

/// Store a payload for JS to collect, pruning the oldest entries when the
/// registry is full — but never evicting a payload whose callback is still
/// awaiting resolution. Evicting one would make JS fetch null for a pending
/// request and strand its oneshot until the timeout (or, for a permission
/// check, forever).
pub(crate) fn store_payload(id: u32, payload: String) {
    let mut registry = PAYLOAD_REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    registry.insert(id, payload);
    let excess = registry.len().saturating_sub(PAYLOAD_REGISTRY_MAX_ENTRIES);
    if excess == 0 {
        return;
    }
    let pending = CALLBACK_REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    let mut evicted = 0usize;
    let mut doomed: Vec<u32> = Vec::new();
    for candidate in registry.keys() {
        if evicted >= excess {
            break;
        }
        if pending.contains_key(candidate) {
            continue;
        }
        doomed.push(*candidate);
        evicted += 1;
    }
    for candidate in doomed {
        registry.remove(&candidate);
    }
}

/// Monotonically increasing callback ID. Wrapping is fine because the ID
/// space is large enough that collisions are impossible in practice.
pub(crate) static NEXT_CALLBACK_ID: AtomicU32 = AtomicU32::new(1);

/// Called by JS to fetch the payload for a given callback ID.
/// Returns the JSON-serialized request payload, or null if not found.
#[napi]
pub fn get_callback_payload(id: u32) -> napi::Result<Option<String>> {
    guard_sync_panic(|| {
        let payload = PAYLOAD_REGISTRY
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
        Ok(payload)
    })
}

/// Called by JS to resolve a pending host callback.
///
/// * `id` — the callback ID that was passed to the JS function
/// * `error` — if present, the callback failed with this error message
/// * `result` — if present (and `error` is absent), the JSON-serialized response
#[napi]
pub fn resolve_callback(
    id: u32,
    error: Option<String>,
    result: Option<String>,
) -> napi::Result<()> {
    guard_sync_panic(|| {
        if let Some(tx) = CALLBACK_REGISTRY
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id)
        {
            let outcome = match (error, result) {
                (Some(err), _) => Err(err),
                (_, Some(res)) => Ok(res),
                (None, None) => Err("callback resolved with no result".to_string()),
            };
            let _ = tx.send(outcome);
        }
        Ok(())
    })
}

/// Catch a panic in a synchronous napi export so it becomes a JS-side
/// error instead of unwinding across the FFI boundary and aborting the
/// whole Node process.
pub(crate) fn guard_sync_panic<T>(f: impl FnOnce() -> napi::Result<T>) -> napi::Result<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
        .map_err(|_| napi::Error::from_reason("internal panic in sync napi export"))?
}

/// Catch a panic in an async napi export so the JS promise rejects instead of
/// hanging.
///
/// `env.execute_tokio_future` runs the body on the tokio runtime, where a panic
/// is caught by the task boundary: the deferred is never resolved and the JS
/// promise waits forever. `Cargo.toml` sets no `panic = "abort"`, so the panic
/// unwinds and can be caught here.
pub(crate) async fn guard_async_panic<T>(
    body: impl std::future::Future<Output = napi::Result<T>>,
) -> napi::Result<T> {
    use futures_util::FutureExt;
    std::panic::AssertUnwindSafe(body)
        .catch_unwind()
        .await
        .map_err(|_| napi::Error::from_reason("internal panic in async napi export"))?
}

/// Removes a registry entry when the scope that registered it ends.
///
/// The explicit `remove` after the awaited work is not enough: an early `?`
/// return skips it, and a napi future dropped by the JS side never reaches it
/// at all — either way the entry outlives the turn it described.
pub(crate) struct MapEntryGuard<V: 'static> {
    map: &'static LazyLock<Mutex<HashMap<String, V>>>,
    key: String,
}

impl<V: 'static> MapEntryGuard<V> {
    pub(crate) fn insert(
        map: &'static LazyLock<Mutex<HashMap<String, V>>>,
        key: String,
        value: V,
    ) -> Self {
        map.lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.clone(), value);
        Self { map, key }
    }
}

impl<V: 'static> Drop for MapEntryGuard<V> {
    fn drop(&mut self) {
        self.map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.key);
    }
}

/// Parse a host-supplied JSON field, reporting the drop instead of silently
/// substituting an empty value.
///
/// A malformed `blocks_json` / `tool_calls_json` used to erase the message's
/// content with no trace anywhere, which reads as the model having said
/// nothing.
pub(crate) fn parse_host_json<T: serde::de::DeserializeOwned + Default>(
    json: Option<&str>,
    what: &str,
    role: &str,
) -> T {
    let Some(json) = json else {
        return T::default();
    };
    match serde_json::from_str(json) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%role, "dropping unparseable {what}: {error}");
            T::default()
        }
    }
}

/// Project the host's tool table onto the engine's, failing on a schema that
/// does not parse.
///
/// `unwrap_or_default()` used to turn a malformed schema into `Value::Null`,
/// which rode into the request body verbatim (`llm/openai.rs`,
/// `llm/anthropic.rs`, `llm/google_genai.rs`): the provider then rejected the
/// whole turn with a 400 that named no tool, and nothing was logged.
pub(crate) fn tool_defs_from_wire(tools: &[JsToolDef]) -> napi::Result<Vec<ToolInfo>> {
    tools
        .iter()
        .map(|t| {
            let input_schema = serde_json::from_str(&t.input_schema).map_err(|error| {
                napi::Error::from_reason(format!(
                    "tool schema parse failed for tool `{}`: {error}",
                    t.name
                ))
            })?;
            Ok(ToolInfo {
                name: t.name.clone(),
                description: t.description.clone(),
                input_schema,
            })
        })
        .collect()
}

// ── NapiHostCallbacks ──────────────────────────────────────────────────────

/// Implements [`HostCallbacks`] using napi [`ThreadsafeFunction`]s so the
/// Rust turn loop can call back into JS for LLM chat and tool execution.
///
/// The TSFN passes only the callback ID (u32). The JS side fetches the
/// payload via `getCallbackPayload(id)` and resolves via `resolveCallback`.
///
/// Uses `ErrorStrategy::Fatal` so the JS callback receives just the callback ID
/// without the error-first `null` argument that `CalleeHandled` prepends.
pub(crate) struct NapiHostCallbacks {
    pub(crate) llm_chat_fn: Arc<ThreadsafeFunction<u32, ErrorStrategy::Fatal>>,
    pub(crate) execute_tool_fn: Arc<ThreadsafeFunction<u32, ErrorStrategy::Fatal>>,
    /// Optional fire-and-forget event channel. The JS side fetches the
    /// payload via `getCallbackPayload(id)` but must NOT resolve it.
    pub(crate) emit_event_fn: Option<Arc<ThreadsafeFunction<u32, ErrorStrategy::Fatal>>>,
    /// Optional permission checker for native execution of mutating tools.
    /// Fail-closed when absent: without a checker the engine refuses native
    /// execution of Write/Edit/Bash and the call falls back to the host.
    pub(crate) check_permission_fn: Option<Arc<ThreadsafeFunction<u32, ErrorStrategy::Fatal>>>,
    /// Optional interactive question channel: the host owns the interaction
    /// runtime and answers with the v2 `QuestionResult` three states.
    /// Absent means the engine reports "host does not support interactive
    /// questions" as the tool result.
    pub(crate) ask_question_fn: Option<Arc<ThreadsafeFunction<u32, ErrorStrategy::Fatal>>>,
    /// Optional state bridge channels: the host reads/writes its durable
    /// state (todo/plan domains) on the engine's behalf. Absent means the
    /// engine reports "host does not support state bridge" as the tool
    /// result.
    pub(crate) state_read_fn: Option<Arc<ThreadsafeFunction<u32, ErrorStrategy::Fatal>>>,
    pub(crate) state_write_fn: Option<Arc<ThreadsafeFunction<u32, ErrorStrategy::Fatal>>>,
    /// Optional host-side file checkpoint channel (P53): native write
    /// executions snapshot their pre-images host-side. Absent means the
    /// host skips checkpointing (fail-open).
    pub(crate) checkpoint_fn: Option<Arc<ThreadsafeFunction<u32, ErrorStrategy::Fatal>>>,
    /// Optional turn lifecycle channel: the engine reports the durable
    /// `turn.prompt` / `turn.cancel` / `turn.ended` records and the observable
    /// `turn.started` so the host can append and fold them. Absent means the
    /// host keeps owning the turn lifecycle end to end, which is the
    /// pre-existing behaviour while `run_turn` is a stateless per-turn call.
    pub(crate) turn_event_fn: Option<Arc<ThreadsafeFunction<u32, ErrorStrategy::Fatal>>>,
    /// Optional turn telemetry channel (M1c): the engine emits
    /// `turn_started` / `turn_ended` / `turn_interrupted` payloads the host
    /// forwards to its telemetry sink. Absent means the host keeps owning
    /// its turn telemetry end to end.
    pub(crate) telemetry_fn: Option<Arc<ThreadsafeFunction<u32, ErrorStrategy::Fatal>>>,
    /// Optional tool-table channel (M1d: `host/list_tools`). The engine
    /// pulls the host's current tool table before each LLM call on native
    /// transports; absent means the turn-start snapshot is the only table.
    pub(crate) list_tools_fn: Option<Arc<ThreadsafeFunction<u32, ErrorStrategy::Fatal>>>,
    /// Optional current-goal channel (`host/goal`). The stale goal gate
    /// reads the host's live goal snapshot through it; absent means
    /// `goal()` reports the seam as unsupported (fail-open for staleness).
    pub(crate) goal_fn: Option<Arc<ThreadsafeFunction<u32, ErrorStrategy::Fatal>>>,
    /// Optional bash-parse channel (`host/parse_bash`): the host owns the
    /// bash syntax tree and answers the AGENTS.md reminder's operand
    /// directories. Absent keeps the reminder at its cwd-only shape — v2's
    /// own fallback when a command fails to parse.
    pub(crate) parse_bash_fn: Option<Arc<ThreadsafeFunction<u32, ErrorStrategy::Fatal>>>,
    /// Optional OAuth token channel (`host/auth_token`). The transport asks
    /// the host for a bearer token when the native LLM config names an
    /// `auth_provider`; absent means OAuth-managed providers are unsupported
    /// and static-key transports are unaffected.
    pub(crate) auth_token_fn: Option<Arc<ThreadsafeFunction<u32, ErrorStrategy::Fatal>>>,
    /// The current turn's cancellation flag. Awaiting a host callback then
    /// observes it, so `cancel_turn` also interrupts in-flight permission
    /// checks and host tool calls instead of stranding them until timeout.
    pub(crate) cancellation: Option<Arc<AtomicBool>>,
}

impl HostCallbacks for NapiHostCallbacks {
    fn llm_chat(
        &self,
        request: LlmChatRequest,
    ) -> crate::rpc::types::BoxFuture<
        'static,
        std::result::Result<LlmChatResponse, std::string::String>,
    > {
        let tsfn = self.llm_chat_fn.clone();
        let input = serde_json::to_string(&request)
            .unwrap_or_else(|e| format!(r#"{{"error":"serialize: {}"}}"#, e));
        Box::pin(napi_llm_chat(tsfn, input, self.cancellation.clone()))
    }

    fn execute_tool(
        &self,
        request: ToolExecuteRequest,
    ) -> crate::rpc::types::BoxFuture<
        'static,
        std::result::Result<ToolExecuteResponse, std::string::String>,
    > {
        let tsfn = self.execute_tool_fn.clone();
        let input = serde_json::to_string(&request)
            .unwrap_or_else(|e| format!(r#"{{"error":"serialize: {}"}}"#, e));
        Box::pin(napi_execute_tool(tsfn, input, self.cancellation.clone()))
    }

    fn check_permission(
        &self,
        request: PermissionCheckRequest,
    ) -> BoxFuture<'static, Result<PermissionDecision, String>> {
        let Some(ref tsfn) = self.check_permission_fn else {
            return Box::pin(async {
                Ok(PermissionDecision::deny(
                    "host did not provide a permission checker; native execution of a mutating tool is refused",
                ))
            });
        };
        let tsfn = tsfn.clone();
        let input = serde_json::to_string(&request)
            .unwrap_or_else(|e| format!(r#"{{"error":"serialize: {}"}}"#, e));
        let cancel = self.cancellation.clone();
        Box::pin(async move {
            // No timeout: this one waits on a human, and giving up would
            // discard an approval the user has already granted. A turn
            // cancellation still interrupts the wait.
            let output =
                invoke_via_registry(&tsfn, input, "check_permission", None, cancel).await?;
            serde_json::from_str(&output).map_err(|e| format!("check_permission parse: {e}"))
        })
    }

    fn ask_question(
        &self,
        request: AskQuestionRequest,
    ) -> BoxFuture<'static, Result<AskQuestionResponse, String>> {
        let Some(ref tsfn) = self.ask_question_fn else {
            return Box::pin(async {
                Err("host does not support interactive questions".to_string())
            });
        };
        let tsfn = tsfn.clone();
        let input = serde_json::to_string(&request)
            .unwrap_or_else(|e| format!(r#"{{"error":"serialize: {}"}}"#, e));
        let cancel = self.cancellation.clone();
        Box::pin(async move {
            // No timeout: this one waits on a human, and giving up would
            // discard an answer the user has already given. A turn
            // cancellation still interrupts the wait.
            let output = invoke_via_registry(&tsfn, input, "ask_question", None, cancel).await?;
            serde_json::from_str(&output).map_err(|e| format!("ask_question parse: {e}"))
        })
    }

    fn state_read(
        &self,
        request: StateReadRequest,
    ) -> BoxFuture<'static, Result<StateReadResponse, String>> {
        let Some(ref tsfn) = self.state_read_fn else {
            return Box::pin(async { Err("host does not support state bridge".to_string()) });
        };
        let tsfn = tsfn.clone();
        let input = serde_json::to_string(&request)
            .unwrap_or_else(|e| format!(r#"{{"error":"serialize: {}"}}"#, e));
        let cancel = self.cancellation.clone();
        Box::pin(async move {
            // Bounded: host bookkeeping with no human in the loop. A turn
            // cancellation still interrupts the wait.
            let output = invoke_via_registry(
                &tsfn,
                input,
                "state_read",
                Some(crate::callbacks::HOST_STATE_TIMEOUT),
                cancel,
            )
            .await?;
            serde_json::from_str(&output).map_err(|e| format!("state_read parse: {e}"))
        })
    }

    fn checkpoint(&self, request: CheckpointRequest) -> BoxFuture<'static, Result<(), String>> {
        let Some(ref tsfn) = self.checkpoint_fn else {
            return Box::pin(async { Err(crate::callbacks::CHECKPOINT_UNSUPPORTED.to_string()) });
        };
        let tsfn = tsfn.clone();
        let input = serde_json::to_string(&request)
            .unwrap_or_else(|e| format!(r#"{{"error":"serialize: {}"}}"#, e));
        let cancel = self.cancellation.clone();
        Box::pin(async move {
            // Bounded: pre-image capture is host bookkeeping, but the engine
            // waits for it before writing — the timeout bounds that wait. A
            // turn cancellation still interrupts the wait.
            invoke_via_registry(
                &tsfn,
                input,
                "checkpoint",
                Some(crate::callbacks::HOST_STATE_TIMEOUT),
                cancel,
            )
            .await?;
            Ok(())
        })
    }

    fn state_write(
        &self,
        request: StateWriteRequest,
    ) -> BoxFuture<'static, Result<StateWriteResponse, String>> {
        let Some(ref tsfn) = self.state_write_fn else {
            return Box::pin(async { Err("host does not support state bridge".to_string()) });
        };
        let tsfn = tsfn.clone();
        let input = serde_json::to_string(&request)
            .unwrap_or_else(|e| format!(r#"{{"error":"serialize: {}"}}"#, e));
        let cancel = self.cancellation.clone();
        Box::pin(async move {
            // Bounded: host bookkeeping with no human in the loop. A turn
            // cancellation still interrupts the wait.
            let output = invoke_via_registry(
                &tsfn,
                input,
                "state_write",
                Some(crate::callbacks::HOST_STATE_TIMEOUT),
                cancel,
            )
            .await?;
            serde_json::from_str(&output).map_err(|e| format!("state_write parse: {e}"))
        })
    }

    fn emit_event(&self, event: serde_json::Value) {
        let Some(ref tsfn) = self.emit_event_fn else {
            return;
        };
        let Ok(payload) = serde_json::to_string(&event) else {
            return;
        };
        fire_payload_only(tsfn, payload);
    }

    fn turn_event(&self, event: crate::turn_events::TurnEvent) {
        let Some(ref tsfn) = self.turn_event_fn else {
            return;
        };
        let Ok(payload) = serde_json::to_string(&event) else {
            return;
        };
        fire_payload_only(tsfn, payload);
    }

    fn telemetry(&self, event: serde_json::Value) {
        let Some(ref tsfn) = self.telemetry_fn else {
            return;
        };
        let Ok(payload) = serde_json::to_string(&event) else {
            return;
        };
        fire_payload_only(tsfn, payload);
    }

    fn list_tools(&self) -> BoxFuture<'static, Result<ListToolsResponse, String>> {
        let Some(ref tsfn) = self.list_tools_fn else {
            return Box::pin(async { Err("host does not support list_tools".to_string()) });
        };
        let tsfn = tsfn.clone();
        let cancel = self.cancellation.clone();
        Box::pin(async move {
            // Bounded: host bookkeeping with no human in the loop; on timeout
            // run_turn falls back to the turn-start snapshot.
            let output = invoke_via_registry(
                &tsfn,
                "{}".to_string(),
                "list_tools",
                Some(HOST_LIST_TOOLS_TIMEOUT),
                cancel,
            )
            .await?;
            serde_json::from_str(&output).map_err(|e| format!("list_tools parse: {e}"))
        })
    }

    fn goal(&self) -> BoxFuture<'static, Result<Option<GoalContext>, String>> {
        let Some(ref tsfn) = self.goal_fn else {
            return Box::pin(async { Err("host does not support goal".to_string()) });
        };
        let tsfn = tsfn.clone();
        let cancel = self.cancellation.clone();
        Box::pin(async move {
            // Unbounded like the host-side goal read: host bookkeeping, no
            // human in the loop; the stale gate treats a failure as
            // fail-open, so no timeout contract is needed here.
            let output = invoke_via_registry(&tsfn, "{}".to_string(), "goal", None, cancel).await?;
            if output == "null" {
                return Ok(None);
            }
            serde_json::from_str::<GoalContext>(&output)
                .map(Some)
                .map_err(|e| format!("goal parse: {e}"))
        })
    }

    fn supports_parse_bash(&self) -> bool {
        self.parse_bash_fn.is_some()
    }

    fn parse_bash(
        &self,
        request: crate::rpc::types::ParseBashRequest,
    ) -> crate::rpc::types::BoxFuture<
        'static,
        std::result::Result<Option<Vec<String>>, std::string::String>,
    > {
        let Some(ref tsfn) = self.parse_bash_fn else {
            return Box::pin(async { Ok(None) });
        };
        let tsfn = tsfn.clone();
        let cancel = self.cancellation.clone();
        Box::pin(async move {
            let payload = serde_json::to_string(&request)
                .map_err(|e| format!("parse_bash serialize: {e}"))?;
            // Bounded like list_tools: the host runs a 500ms parse budget on
            // its side; a stuck channel must not stall the injection pass.
            let output = invoke_via_registry(
                &tsfn,
                payload,
                "parse_bash",
                Some(crate::callbacks::HOST_LIST_TOOLS_TIMEOUT),
                cancel,
            )
            .await?;
            if output == "null" {
                return Ok(None);
            }
            let dirs: Vec<String> =
                serde_json::from_str(&output).map_err(|e| format!("parse_bash parse: {e}"))?;
            Ok(Some(dirs))
        })
    }

    fn auth_token(
        &self,
        provider: String,
        force: bool,
    ) -> crate::rpc::types::BoxFuture<'static, std::result::Result<String, std::string::String>>
    {
        let Some(ref tsfn) = self.auth_token_fn else {
            return Box::pin(async { Err("host does not support oauth token fetch".to_string()) });
        };
        let tsfn = tsfn.clone();
        let cancel = self.cancellation.clone();
        Box::pin(async move {
            // Bounded like the stdio leg: a cache hit answers immediately, a
            // miss covers one OAuth refresh round-trip (network, no human).
            let payload = serde_json::json!({ "provider": provider, "force": force }).to_string();
            let output = invoke_via_registry(
                &tsfn,
                payload,
                "auth_token",
                Some(HOST_AUTH_TOKEN_TIMEOUT),
                cancel,
            )
            .await?;
            let response: AuthTokenResponse =
                serde_json::from_str(&output).map_err(|e| format!("auth_token parse: {e}"))?;
            Ok(response.token)
        })
    }
}

/// Fire a fire-and-forget TSFN that carries a payload the JS side collects by
/// callback id, with no oneshot to resolve.
///
/// Pruning matters here: an event the host never collects would otherwise
/// accumulate in the payload registry for the life of the process.
fn fire_payload_only(tsfn: &ThreadsafeFunction<u32, ErrorStrategy::Fatal>, payload: String) {
    let id = NEXT_CALLBACK_ID.fetch_add(1, Ordering::SeqCst);
    store_payload(id, payload);
    let status = tsfn.call(id, ThreadsafeFunctionCallMode::NonBlocking);
    if status != napi::Status::Ok {
        PAYLOAD_REGISTRY
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
    }
}

/// Register a callback with the global registry, store the payload for
/// JS-side retrieval, fire the JS function with just the callback ID,
/// and await the result.
///
/// Returns the JSON-serialized response string, or an error message.
///
/// `timeout` bounds how long the host may take to answer. `None` waits
/// indefinitely, which is right for a permission check: that one is
/// answered by a human, and a timeout would land after the user approved.
/// Either way, a turn cancellation interrupts the wait.
pub(crate) async fn invoke_via_registry(
    tsfn: &Arc<ThreadsafeFunction<u32, ErrorStrategy::Fatal>>,
    input: String,
    label: &str,
    timeout: Option<Duration>,
    cancel: Option<Arc<AtomicBool>>,
) -> std::result::Result<std::string::String, std::string::String> {
    let id = NEXT_CALLBACK_ID.fetch_add(1, Ordering::SeqCst);
    let (tx, rx) = oneshot::channel();

    // Store the payload so JS can fetch it via getCallbackPayload(id).
    store_payload(id, input);

    // Register the sender so resolve_callback can find it.
    CALLBACK_REGISTRY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id, tx);

    // Fire the JS function with just the callback ID (a number).
    // ErrorStrategy::Fatal: no error-first null prepended, JS receives the id directly.
    let status = tsfn.call(id, ThreadsafeFunctionCallMode::NonBlocking);
    if status != napi::Status::Ok {
        // Clean up on failure.
        PAYLOAD_REGISTRY
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
        CALLBACK_REGISTRY
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
        return Err(format!("{label} call: {status:?}"));
    }

    let pending = match timeout {
        Some(limit) => {
            match tokio::time::timeout(limit, wait_for_callback(rx, cancel, label)).await {
                Ok(outcome) => outcome,
                Err(_) => Err(format!("{label} timed out after {}s", limit.as_secs())),
            }
        }
        None => wait_for_callback(rx, cancel, label).await,
    };

    // Whatever ended the wait (resolution, timeout, cancellation, dropped
    // host), leave nothing behind: a late resolve_callback would find no
    // sender, and the payload would sit in the registry until pruned.
    PAYLOAD_REGISTRY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&id);
    if pending.is_err() {
        CALLBACK_REGISTRY
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
    }

    pending.and_then(|inner| inner)
}

/// Await a host-callback oneshot, optionally observing the turn's
/// cancellation flag so `cancel_turn` can interrupt permission waits and
/// host-tool round-trips instead of stranding them.
async fn wait_for_callback(
    rx: oneshot::Receiver<Result<String, String>>,
    cancel: Option<Arc<AtomicBool>>,
    label: &str,
) -> Result<Result<String, String>, String> {
    let Some(flag) = cancel else {
        return match rx.await {
            Ok(outcome) => Ok(outcome),
            Err(_) => Err(format!("{label} closed: receiver dropped")),
        };
    };
    let mut rx = rx;
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = tick.tick() => {
                if flag.load(Ordering::Relaxed) {
                    return Err(format!("{label} cancelled"));
                }
            }
            outcome = &mut rx => return outcome.map_err(|_| format!("{label} closed: receiver dropped")),
        }
    }
}

/// Standalone async function for LLM chat via callback registry.
async fn napi_llm_chat(
    tsfn: Arc<ThreadsafeFunction<u32, ErrorStrategy::Fatal>>,
    input: String,
    cancel: Option<Arc<AtomicBool>>,
) -> std::result::Result<LlmChatResponse, std::string::String> {
    let output =
        invoke_via_registry(&tsfn, input, "llm_chat", Some(HOST_LLM_TIMEOUT), cancel).await?;
    serde_json::from_str(&output).map_err(|e| format!("llm_chat parse: {e}"))
}

/// Standalone async function for tool execution via callback registry.
async fn napi_execute_tool(
    tsfn: Arc<ThreadsafeFunction<u32, ErrorStrategy::Fatal>>,
    input: String,
    cancel: Option<Arc<AtomicBool>>,
) -> std::result::Result<ToolExecuteResponse, std::string::String> {
    let output = invoke_via_registry(
        &tsfn,
        input,
        "execute_tool",
        Some(HOST_TOOL_TIMEOUT),
        cancel,
    )
    .await?;
    serde_json::from_str(&output).map_err(|e| format!("execute_tool parse: {e}"))
}
