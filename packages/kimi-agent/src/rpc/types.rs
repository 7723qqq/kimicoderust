//! JSON-RPC 2.0 protocol types for kimi-agent stdio communication.
//!
//! The agent process speaks JSON-RPC 2.0 over stdio:
//! - Reads JSON-RPC requests from stdin
//! - Writes JSON-RPC responses (and notifications) to stdout
//! - Uses stderr for logging/diagnostics

use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;

/// A boxed future type alias for async handlers.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

// ── JSON-RPC 2.0 base types ────────────────────────────────────────────────

/// Unique identifier for a JSON-RPC request.
pub type RequestId = serde_json::Value;

/// A JSON-RPC 2.0 request.
#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    #[serde(default)]
    pub id: serde_json::Value,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

/// A JSON-RPC 2.0 response (success).
#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: RequestId,
    pub result: serde_json::Value,
}

/// A JSON-RPC 2.0 error response.
#[derive(Debug, Serialize)]
pub struct JsonRpcErrorResponse {
    pub jsonrpc: String,
    pub id: RequestId,
    pub error: JsonRpcError,
}

/// A JSON-RPC 2.0 error object.
#[derive(Debug, Serialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl std::fmt::Display for JsonRpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}

impl std::error::Error for JsonRpcError {}

/// A JSON-RPC 2.0 notification (no response expected).
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct JsonRpcNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

// ── Agent RPC method names ─────────────────────────────────────────────────

/// RPC method names for the kimi-agent protocol.
pub mod methods {
    /// Run a single turn. Corresponds to `runTurn()` in the JS loop.
    pub const RUN_TURN: &str = "agent/run_turn";

    /// Cancel a running turn.
    pub const CANCEL_TURN: &str = "agent/cancel_turn";

    /// Health check.
    pub const HEALTH: &str = "agent/health";

    /// Shutdown the agent process.
    pub const SHUTDOWN: &str = "agent/shutdown";

    /// LLM chat request (Rust → JS host proxy).
    pub const HOST_LLM_CHAT: &str = "host/llm_chat";

    /// Execute a tool call (Rust → JS host proxy).
    pub const HOST_EXECUTE_TOOL: &str = "host/execute_tool";

    /// Permission check for native execution of a mutating tool
    /// (Rust → JS host). The host stays the permission authority; a deny
    /// answer makes the engine return the denial as the tool result
    /// instead of executing (natively or via the host).
    pub const HOST_CHECK_PERMISSION: &str = "host/check_permission";

    /// Fire-and-forget event notification (Rust → JS host).
    /// Used by the native LLM / native tool paths to report step
    /// boundaries, streaming deltas, and natively-executed tool results
    /// so the host can record them in the transcript.
    pub const HOST_EVENT: &str = "host/event";

    /// Turn lifecycle notification (Rust → JS host). Carries a
    /// [`crate::turn_events::TurnEvent`]: the durable records (`turn.prompt` /
    /// `turn.cancel` / `turn.ended`) the host appends to its log and folds into
    /// its turn state, plus the observable `turn.started`. Fire-and-forget — the
    /// engine has already assigned the turn id locally.
    pub const HOST_TURN_EVENT: &str = "host/turn_event";

    /// Turn telemetry notification (Rust → JS host, M1c). Carries a JSON
    /// object whose `event` field names the v2 telemetry event
    /// (`turn_started` / `turn_ended` / `turn_interrupted`) followed by the
    /// v2 payload fields (turn_id, mode, provider_type, protocol,
    /// thinking_effort, reason, duration_ms, steps, at_step,
    /// interrupt_reason). Fire-and-forget — the host forwards one track2 per
    /// event.
    pub const HOST_TELEMETRY: &str = "host/telemetry";

    /// Ask the host an interactive question and wait for a human answer
    /// (Rust → JS host). The host owns the interaction runtime — pending
    /// key, dismiss, turn-end cancellation — and answers with the v2
    /// `QuestionResult` three states (answered / dismissed / cancelled).
    pub const HOST_ASK_QUESTION: &str = "host/ask_question";

    /// Read host-owned durable state (Rust → JS host). The engine's native
    /// todo/plan tools read through this seam; the host stays the
    /// persistence and undo authority.
    pub const HOST_STATE_READ: &str = "host/state_read";

    /// Write host-owned durable state (Rust → JS host). The host applies
    /// its domain semantics (re-normalization, undoable events) and returns
    /// the resulting state.
    pub const HOST_STATE_WRITE: &str = "host/state_write";

    /// Host-side file checkpoint for native write executions (Rust → JS
    /// host, P53). `phase: "prepare"` runs before the engine writes — the
    /// host must capture the pre-image before the response arrives;
    /// `phase: "record"` notes the post-image after execution. Fail-open:
    /// an unwired or failing host skips the snapshot (the pre-P53 status
    /// quo — native writes were never checkpointed).
    pub const HOST_CHECKPOINT: &str = "host/checkpoint";

    /// Fetch the host's current tool table (Rust → JS host, M1d). Called
    /// before each LLM call on native transports so mid-turn registry
    /// changes (feature tools, MCP reconnects) reach the model — the
    /// turn-start snapshot in `agent/run_turn` is only the fallback for
    /// hosts without this seam. Host-proxy mode rebuilds tools host-side
    /// per call and never consults this.
    pub const HOST_LIST_TOOLS: &str = "host/list_tools";

    /// Fetch the host's current goal snapshot (Rust → JS host, M1d 3b).
    ///
    /// The session's goal provider reads it fresh per turn (budget checks
    /// plus steering), mirroring the napi `goal_cb` seam. The result is the
    /// snake_case wire goal object, or JSON null when no goal is active.
    pub const HOST_GOAL: &str = "host/goal";

    /// Fetch a bearer token for an OAuth-managed provider (Rust → JS host).
    /// The host owns the OAuth store (single-flight refresh); `force` asks it
    /// to refresh past the cache after a 401/403 from the provider. The
    /// response is `{ "token": "..." }`.
    pub const HOST_AUTH_TOKEN: &str = "host/auth_token";

    // ── EngineSession handle over stdio (M1d 3b) ─────────────────────────
    // The stdio transport gets the same session surface as the napi addon:
    // the pipeline is built once per session, and admission / the pending
    // FIFO / the pump / turn ids / cancellation / quiescence live
    // engine-side across turns.

    /// Create a session handle. Params are the full `RunTurnParams` shape;
    /// the per-turn fields (`messages` / `tools` / `goal`) are ignored.
    pub const SESSION_CREATE: &str = "session/create";
    /// Enqueue a prompt. Returns the engine-assigned turn id (monotonic).
    pub const SESSION_ENQUEUE_TURN: &str = "session/enqueue_turn";
    /// Resolve with the outcome of one enqueued turn (exactly once).
    pub const SESSION_TURN_OUTCOME: &str = "session/turn_outcome";
    /// Cancel a turn by id (active → step-boundary interrupt; queued or
    /// quiescence-held → dropped). Without an id the active turn is cancelled.
    pub const SESSION_CANCEL_TURN: &str = "session/cancel_turn";
    /// Live session shape: active turn id + pending turn ids.
    pub const SESSION_STATUS: &str = "session/status";
    /// Whether the session is fully idle right now.
    pub const SESSION_IS_SETTLED: &str = "session/is_settled";
    /// Resolve once the session is fully idle.
    pub const SESSION_SETTLED: &str = "session/settled";
    /// Try to acquire the quiescence window (exclusive; parked turns replay
    /// on release). Fails while a guard is held or a turn is outstanding.
    pub const SESSION_TRY_ACQUIRE_QUIESCENCE: &str = "session/try_acquire_quiescence";
    /// Release the quiescence window: held turns replay in FIFO order.
    pub const SESSION_RELEASE_QUIESCENCE: &str = "session/release_quiescence";
    /// Replace the session's cross-turn history.
    pub const SESSION_SET_HISTORY: &str = "session/set_history";
    /// Clear the session's cross-turn history.
    pub const SESSION_CLEAR_HISTORY: &str = "session/clear_history";
    /// Append messages to the cross-turn history.
    pub const SESSION_EXTEND_HISTORY: &str = "session/extend_history";
    /// Current cross-turn history length.
    pub const SESSION_HISTORY_LEN: &str = "session/history_len";
    /// Fetch the session's cross-turn history.
    pub const SESSION_GET_HISTORY: &str = "session/get_history";
    /// Drop the session handle.
    pub const SESSION_DISPOSE: &str = "session/dispose";
    /// Start a btw side-channel instance forked from the session's history.
    /// Returns the engine-assigned subagent id (`agent-btw-…`).
    pub const SESSION_START_BTW: &str = "session/start_btw";
    /// Run one btw side-channel turn. Resolves `{ content, stopReason }`.
    pub const SESSION_BTW_PROMPT: &str = "session/btw_prompt";
    /// Abort a running btw side-channel turn. Resolves whether a turn was
    /// pending for the agent id.
    pub const SESSION_BTW_CANCEL: &str = "session/btw_cancel";
    /// Derive the session title from the live cross-turn history. Resolves
    /// the title or null; `digest` is rejected.
    pub const SESSION_GENERATE_TITLE: &str = "session/generate_title";
    /// Every registered background task's entry wire, oldest first, output
    /// omitted — a JSON string. Empty array when no runner is live.
    pub const SESSION_BACKGROUND_TASK_LIST: &str = "session/background_task_list";
    /// One background task's output snapshot; null while running or unknown.
    pub const SESSION_BACKGROUND_TASK_OUTPUT: &str = "session/background_task_output";
    /// Request a cooperative stop for one background task. Resolves the
    /// entry wire (`killed` once settled).
    pub const SESSION_BACKGROUND_TASK_STOP: &str = "session/background_task_stop";
}

/// Permission check for a mutating tool call the engine wants to execute
/// natively. Sent to the host before running the tool; the host applies its
/// full permission machinery (mode, rules, policies, interactive approval).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionCheckRequest {
    pub tool_name: String,
    pub tool_call_id: String,
    /// Turn the tool call belongs to. The approval prompt names the same
    /// `${turn_id}:${tool_call_id}` wire id the tool card was created under
    /// (`acp::events_map::acp_tool_call_id`), so the client attaches the
    /// prompt to that card. `#[serde(default)]` keeps a request that carries
    /// no turn id on the raw tool-call id.
    #[serde(default)]
    pub turn_id: String,
    pub arguments: serde_json::Value,
    /// Why the engine is asking, in the host's locale — the local permission
    /// policy that fired (`SensitiveFileAccessAsk`, `DangerousCommandAsk`, …)
    /// rendered through `i18n::LocalizedText`. `None` when the engine has no
    /// policy-specific explanation, in which case the host falls back to
    /// describing the tool call itself.
    ///
    /// `#[serde(default)]` keeps a host built before this field wire-compatible:
    /// it simply never sees a reason, exactly as before.
    #[serde(default)]
    pub reason: Option<String>,
    /// The user-configured ask rule that produced this request, when one did.
    ///
    /// Approving it for the session remembers *this rule*, not the whole tool:
    /// it is a pattern the user already wrote in their own config, so nothing
    /// is inferred and nothing is widened. v2 carries the same value as
    /// `sessionApprovalRule` and promotes it only when the response says
    /// `scope: session` (permissionRulesOps.ts).
    ///
    /// `#[serde(default)]` keeps a host built before this field wire-compatible:
    /// it simply never sees a session approval rule, exactly as before.
    #[serde(default)]
    pub session_approval_rule: Option<String>,
}

/// The host's permission verdict for a [`PermissionCheckRequest`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionDecision {
    /// `"allow"` or `"deny"`.
    pub decision: String,
    #[serde(default)]
    pub reason: Option<String>,
}

impl PermissionDecision {
    pub fn allow() -> Self {
        Self {
            decision: "allow".into(),
            reason: None,
        }
    }

    pub fn is_allow(&self) -> bool {
        self.decision.eq_ignore_ascii_case("allow")
    }

    pub fn deny(reason: impl Into<String>) -> Self {
        Self {
            decision: "deny".into(),
            reason: Some(reason.into()),
        }
    }
}

// ── Reverse interaction types (Rust → JS host) ─────────────────────────────

/// An interactive question the engine asks the host, answered by a human.
/// Mirrors the v2 `QuestionItem` shape: 1–4 questions, each with 2–4
/// options and an optional multi-select flag.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskQuestionRequest {
    /// Engine-generated unique id (`question_<uuid>`); the host keys its
    /// pending interaction on it and echoes it back in the response.
    pub question_id: String,
    /// Turn the question belongs to; the host cancels pending questions by
    /// turn when the turn ends.
    pub turn_id: String,
    /// Tool call the question answers; lets the UI associate it with the
    /// tool card.
    pub tool_call_id: String,
    /// `true` = background question: the host registers a background task
    /// and returns its task_id immediately instead of waiting for a human.
    #[serde(default)]
    pub background: bool,
    /// Optional wait bound. `None` = wait indefinitely (v2 semantics, human
    /// in the loop — same as a permission check).
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// 1–4 questions, mirroring v2 `QuestionItem` fields.
    pub questions: Vec<AskQuestionItem>,
}

/// A single question within an [`AskQuestionRequest`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskQuestionItem {
    /// The question text; also the key under which the answer is returned.
    pub question: String,
    /// Short category label (≤12 chars in v2).
    #[serde(default)]
    pub header: Option<String>,
    /// 2–4 options; labels are unique within a question.
    pub options: Vec<AskQuestionOption>,
    /// `true` = the user may pick several options (answers are
    /// comma-separated labels).
    #[serde(default)]
    pub multi_select: bool,
}

/// A selectable option within an [`AskQuestionItem`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskQuestionOption {
    pub label: String,
    /// Optional explanatory text shown under the label.
    #[serde(default)]
    pub description: Option<String>,
}

/// The host's answer to an [`AskQuestionRequest`]. Mirrors the v2
/// `QuestionResult` three states: answered (`answers` + optional `method`),
/// dismissed (empty `answers` + `note`), or cancelled (`cancelled` +
/// `reason`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskQuestionResponse {
    /// `question text → answer` (multi-select answers are comma-separated
    /// labels; "Other" carries the user's free text).
    #[serde(default)]
    pub answers: std::collections::HashMap<String, String>,
    /// How the user answered: `"enter"` / `"space"` / `"number_key"`.
    #[serde(default)]
    pub method: Option<String>,
    /// Present when the user dismissed the question without answering.
    #[serde(default)]
    pub note: Option<String>,
    /// `true` when the interaction was cancelled (turn ended, agent closed,
    /// or timeout).
    #[serde(default)]
    pub cancelled: Option<bool>,
    /// Cancellation reason: `turn_ended` / `agent_closed` / `timeout`.
    #[serde(default)]
    pub reason: Option<String>,
}

/// A state read request (Rust → JS host). The engine's native todo/plan
/// tools read host-owned durable state through this seam; the host stays
/// the persistence and undo authority.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateReadRequest {
    /// State domain discriminator: `"todo"` / `"plan"` (future `"goal"` /
    /// `"cron"` / `"task"`). Unknown domains map to `-32001`.
    pub domain: String,
    /// Key within the domain. First version always equals the domain;
    /// unknown keys map to `-32002`.
    pub key: String,
    /// Optional provenance; the host may ignore it.
    #[serde(default)]
    pub turn_id: String,
    /// Optional provenance; the host may ignore it.
    #[serde(default)]
    pub tool_call_id: String,
}

/// A host-side checkpoint request for a native write execution (Rust →
/// JS host, P53). `phase: "prepare"` carries the pre-write paths — the
/// host captures their pre-images before responding, and the engine does
/// not write until the response lands. `phase: "record"` carries the same
/// paths after execution so the host notes their post-images (used to
/// detect manual edits at undo time). Fail-open on both sides: an
/// unwired/failing host skips the snapshot (the pre-P53 status quo).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointRequest {
    pub turn_id: String,
    #[serde(default)]
    pub tool_call_id: String,
    /// `"prepare"` (pre-write snapshot) or `"record"` (post-write digest).
    pub phase: String,
    /// Absolute-or-workspace-relative write target paths.
    pub paths: Vec<String>,
    /// `record` only: whether the execution ran (failed prepares never
    /// reach record).
    #[serde(default)]
    pub executed: bool,
}

/// The host's answer to a [`StateReadRequest`]: the domain wire value,
/// opaque JSON serialized by the host (todo: `TodoItem[]`; plan:
/// `{active, id?, path?}`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateReadResponse {
    pub value: serde_json::Value,
}

/// A state write request (Rust → JS host). The engine submits a domain
/// wire value; the host re-normalizes it (authoritative) and applies its
/// domain semantics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateWriteRequest {
    /// State domain discriminator, same as [`StateReadRequest::domain`].
    pub domain: String,
    /// Key within the domain, same as [`StateReadRequest::key`].
    pub key: String,
    /// Domain wire value. todo: `TodoItem[]`; plan: `{active: true}` =
    /// enter, `{active: false}` = exit.
    pub value: serde_json::Value,
    /// The engine's undo-semantics declaration for this write. The host is
    /// authoritative and may ignore it (todo/plan are always undoable).
    pub undoable: bool,
    /// Optional provenance; the host may ignore it.
    #[serde(default)]
    pub turn_id: String,
    /// Optional provenance; the host may ignore it.
    #[serde(default)]
    pub tool_call_id: String,
}

/// The host's answer to a [`StateWriteRequest`]: `ok` plus the result
/// state after domain semantics were applied (may differ from the
/// submitted value — todo ids filled in, plan id/path attached).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateWriteResponse {
    pub ok: bool,
    pub value: serde_json::Value,
}

/// The host's answer to `host/list_tools` (M1d): the current tool table in
/// the same shape as the `agent/run_turn` snapshot entries, so the engine
/// can swap them interchangeably.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListToolsResponse {
    pub tools: Vec<ToolInfo>,
}

/// Response body of `host/auth_token`: the bearer token the transport puts in
/// the Authorization / x-api-key header.
#[derive(Debug, Deserialize)]
pub struct AuthTokenResponse {
    pub token: String,
}

// ── Message content blocks (multimodal) ─────────────────────────────────

/// The media family a [`ContentBlock::MediaRef`] belongs to (v2 `MediaKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Image,
    Video,
    Audio,
}

impl MediaKind {
    /// The lowercase name used in `<image path="…">` tags and in the
    /// omission placeholders (v2 `MediaKind`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Video => "video",
            Self::Audio => "audio",
        }
    }
}

/// A single content block within a message. Text-only messages keep using
/// the plain `content` string; multimodal messages carry ordered blocks in
/// addition (blocks win over `content` when non-empty).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Plain text.
    Text { text: String },
    /// Base64-encoded image data with a MIME media type (e.g. `image/png`).
    Image {
        media_type: String,
        data: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// Media that lives in the daemon's file store, carried as a reference
    /// instead of bytes.
    ///
    /// The host emits this and the engine resolves it at request time (v2
    /// `kimi-file://` daemon refs): inline base64, a provider-side `ms://`
    /// reference, or a `<image path="…">` tag when the model cannot take it.
    /// Keeping the reference — rather than inlining at the host boundary —
    /// is what lets the resolver pick per model, key the request media budget
    /// by file identity, and name the saved path in an omission. The store
    /// holds the media type and the blob path, so the reference carries only
    /// what the store cannot say: which file, and which media family the
    /// submitting part claimed.
    MediaRef { file_id: String, kind: MediaKind },
    /// Image referenced by URL (https, data URL, or a provider-side
    /// `ms://<id>` reference), with an optional provider-side id.
    ImageUrl {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// Audio referenced by URL, with an optional provider-side id.
    AudioUrl {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// Video referenced by URL, with an optional provider-side id.
    VideoUrl {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// Model reasoning content. `encrypted` carries the provider's attestation
    /// signature (Anthropic `signature`), which must come back with the block.
    /// The JSON shape is the host's `ThinkPart`, so a block crosses into the
    /// transcript unchanged.
    ///
    /// **`think` is displayable text and nothing else.** A block that exists
    /// only to rebuild the provider's `reasoning_details` array carries its
    /// payload in `details_summary` and leaves `think` empty, so no client can
    /// render reasoning the user has already seen: the OpenAI replay
    /// (`llm/openai.rs::project_message`) rebuilds the array from
    /// `details_summary` and sends unstamped `think` in the model's declared
    /// reasoning field. Keeping both roles in one field is what made a
    /// details-derived summary show up a second time, inside the answer.
    Think {
        think: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        encrypted: Option<String>,
        /// Position in the provider's `reasoning_details` array (v2
        /// `ThinkPart.detailsIndex`): the identity that lets the OpenAI
        /// replay rebuild the array instead of flattening every part into
        /// one string (v2 #3910's unported half).
        #[serde(
            rename = "detailsIndex",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        details_index: Option<u32>,
        /// The provider field this part's text belongs to (v2
        /// `ThinkPart.reasoningKey`); `reasoning_details` marks a part
        /// extracted from the array dialect rather than a string field.
        #[serde(
            rename = "reasoningKey",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        reasoning_key: Option<String>,
        /// The `summary` text of this part's `reasoning_details` array entry
        /// (v2 `ThinkPart.detailsSummary`) — replay payload, never rendered.
        /// Set instead of `think` when the same reasoning already reached the
        /// user through the string dialect, so the array survives the round
        /// trip without a second copy on screen.
        #[serde(
            rename = "detailsSummary",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        details_summary: Option<String>,
    },
}

// ── Native LLM configuration (Rust-side HTTP transport) ───────────────────

/// Configuration for the native HTTP LLM transport. When present on
/// `RunTurnParams`, the Rust engine calls the provider directly over
/// HTTP with SSE streaming instead of proxying `llm_chat` to the JS host.
#[derive(Clone, Default, Deserialize)]
pub struct NativeLlmConfig {
    /// Wire protocol: `"openai"` (Chat Completions) or `"anthropic"` (Messages).
    pub protocol: String,
    /// Declared provider family (`[providers.*].type`, e.g. `"openai"`,
    /// `"kimi"`, `"anthropic"`). The protocol decides the request shape; this
    /// distinguishes providers that share one but encode thinking differently
    /// — v2's `kimiOpenAITrait` packs the effort into a `thinking` object
    /// instead of a top-level `reasoning_effort`.
    #[serde(default)]
    pub provider_type: Option<String>,
    /// API base URL including the version segment (e.g. `https://api.example.com/v1`).
    pub base_url: String,
    /// Bearer token (OpenAI) or x-api-key (Anthropic).
    pub api_key: String,
    /// Name of the environment variable the credential is read from at
    /// request time (v2 `provider.apiKeyEnv`). Mutually exclusive with an
    /// inline `api_key` and with `auth_provider`; a missing, empty, or
    /// non-unicode variable fails the request as `config.invalid` — it never
    /// falls back to any other credential. The key itself is never stored on
    /// the config.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Model name sent to the provider.
    pub model: String,
    /// `max_tokens` for the Anthropic Messages API (required there).
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Extra headers sent with every request.
    #[serde(default)]
    pub custom_headers: std::collections::HashMap<String, String>,
    /// Reasoning effort for OpenAI-compatible providers (e.g. `"low"`, `"medium"`, `"high"`, `"max"`).
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// Thinking budget in tokens for Anthropic Messages API.
    #[serde(default)]
    pub thinking_budget: Option<u32>,
    /// OAuth-managed auth: the host-side provider name the transport asks for
    /// a bearer token (`host/auth_token`) instead of using the static
    /// `api_key`. `api_key` stays the fallback for providers that carry both.
    #[serde(default)]
    pub auth_provider: Option<String>,
    /// Moonshot preserved-thinking passthrough (`thinking.keep`): `keep` on
    /// the kimi/openai body, a `clear_thinking_20251015` context-management
    /// edit on anthropic. The host filters off-values; `None` = no keep on
    /// the wire.
    #[serde(default)]
    pub thinking_keep: Option<String>,
    /// Route an anthropic-protocol model through the **beta** Messages API
    /// (`POST {base}/messages?beta=true`) instead of the standard endpoint
    /// (`[models.<alias>].beta_api`). Beta features then travel in the request
    /// body's `betas` field rather than the `anthropic-beta` header. Kimi's
    /// anthropic-compatible endpoint is served behind the beta API, so its
    /// managed aliases declare this. Ignored for non-anthropic protocols.
    #[serde(default)]
    pub beta_api: bool,
    /// The model's declared capabilities (`[models.<alias>].capabilities`):
    /// the image-read gate refuses a declared set that lacks `image_in`.
    #[serde(default)]
    pub capabilities: Option<Vec<String>>,
    /// The model's own system prompt (`[models.<alias>].system_prompt`),
    /// replacing the session prompt for this model's transport.
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// Declared input cap when below the window
    /// (`[models.<alias>].max_input_size`): the compaction budget prefers it
    /// over the host's total window.
    #[serde(default)]
    pub max_input_size: Option<u32>,
    /// Explicit adaptive-thinking support, overriding the model-name version
    /// inference (`[models.<alias>].adaptive_thinking`).
    #[serde(default)]
    pub adaptive_thinking: Option<bool>,
    /// The wire field carrying reasoning content
    /// (`[models.<alias>].reasoning_key`); the built-in probe list is the
    /// fallback when the model declares none.
    #[serde(default)]
    pub reasoning_key: Option<String>,
    /// The effort value that encodes "thinking off" on the wire
    /// (`[models.<alias>].off_effort`): it fills in only when the host sent no
    /// `reasoning_effort`.
    #[serde(default)]
    pub off_effort: Option<String>,
}

/// One `[agent].multi_llm` racer: the `[models]` alias it is named by and the
/// ready-built native transport it runs on.
#[derive(Clone, Debug, Deserialize)]
pub struct ResolvedMultiLlmProvider {
    pub name: String,
    pub llm: NativeLlmConfig,
}

/// One `[secondary_model.models]` pool entry: the alias the model passes via
/// the `model` parameter, the hint shown in the tool description, and the
/// host-resolved LLM that alias binds to.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct SecondaryModelEntry {
    pub alias: String,
    #[serde(default)]
    pub hint: String,
    pub llm: NativeLlmConfig,
}

/// The `[secondary_model]` subagent model pool (v2
/// `session/subagent/configSection.ts`), resolved by the host: the default
/// alias, whether every spawn is forced onto it, the caller's own alias (for
/// the `[main model]` marker), and one ready-built LLM per pool entry.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecondaryModelPool {
    #[serde(default)]
    pub force: bool,
    pub default_model: String,
    #[serde(default)]
    pub caller_model_alias: Option<String>,
    #[serde(default)]
    pub models: Vec<SecondaryModelEntry>,
}

/// One host-resolved `[services.moonshot_*]` entry (v2 `configSection.ts`):
/// the endpoint the native WebSearch / FetchURL tools call instead of the
/// built-in DuckDuckGo scrape / direct HTTP fetch.
#[derive(Clone, Debug, Deserialize)]
pub struct WebServiceConfig {
    pub base_url: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub custom_headers: std::collections::HashMap<String, String>,
}

/// Debug never renders the key: this struct is `{:?}`-formatted on paths that
/// reach logs and transcripts, and one accidental log line would publish the
/// credential there.
impl std::fmt::Debug for NativeLlmConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeLlmConfig")
            .field("protocol", &self.protocol)
            .field("provider_type", &self.provider_type)
            .field("base_url", &self.base_url)
            .field("api_key", &"[redacted]")
            .field("api_key_env", &self.api_key_env)
            .field("model", &self.model)
            .field("max_tokens", &self.max_tokens)
            .field("custom_headers", &self.custom_headers)
            .field("reasoning_effort", &self.reasoning_effort)
            .field("thinking_budget", &self.thinking_budget)
            .field("auth_provider", &self.auth_provider)
            .field("beta_api", &self.beta_api)
            .finish()
    }
}

// ── RunTurn request/response types ─────────────────────────────────────────

/// Input for a run_turn RPC call.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct RunTurnParams {
    pub turn_id: String,
    pub system_prompt: String,
    pub model_name: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
    /// Step cap for the turn loop. `None` = unbounded (JS-loop semantics).
    pub max_steps: Option<u32>,
    /// LLM retry attempts per step (v2 `loopControl.maxAttemptsPerStep`).
    /// `None` = engine default (10).
    #[serde(default)]
    pub max_attempts: Option<u32>,
    /// Context window the host resolved for the active model. `None` keeps the
    /// engine's default compaction budget.
    pub max_context_tokens: Option<u32>,
    /// Multiple LLM providers for concurrent execution (MultiLLM).
    /// When present, overrides `system_prompt` + `model_name`.
    #[serde(default)]
    pub providers: Vec<LlmProviderDef>,
    /// Optional goal context for budget-aware execution.
    /// When present, the loop checks budgets before each step and
    /// injects steering text into the system prompt.
    #[serde(default)]
    pub goal: Option<GoalContext>,
    /// Native HTTP LLM transport. When present, the Rust engine calls the
    /// provider directly (streaming) instead of proxying through the host.
    #[serde(default)]
    pub native_llm: Option<NativeLlmConfig>,
    /// Workspace root used to sandbox native tool execution.
    #[serde(default)]
    pub workspace_root: Option<String>,
    /// When true (and `workspace_root` is set), tools the sandbox can
    /// execute (Read/Grep/Glob/Write/Edit/Bash, each gated on a host
    /// permission grant) run inside the Rust process.
    #[serde(default)]
    pub native_tools: bool,
    /// Host-authorized extra roots (`/add-dir` → `additionalDirs`): paths that
    /// canonicalize under one of them are served by the native toolset even
    /// though they sit outside `workspace_root`. `#[serde(default)]` keeps
    /// older callers that do not send the field wire-compatible.
    #[serde(default)]
    pub additional_dirs: Vec<String>,
    /// Rust engine self-contained mode. When true, the engine refuses to
    /// fall back to the host proxy for LLM calls — the caller must set
    /// `native_llm` or `providers`, or the engine returns a JSON-RPC
    /// internal error instead of routing through `host/llm_chat`. See
    /// kimi-agent ROADMAP P26 批 1.
    #[serde(default)]
    pub rust_self_contained: bool,
    /// Host shell for native Bash (bash everywhere, Git Bash on Windows).
    /// Absent on Windows → native Bash stays with the host.
    #[serde(default)]
    pub shell_path: Option<String>,
    /// Optional permission policy snapshot for local evaluation (P26 批 3).
    #[serde(default)]
    pub policy_snapshot: Option<crate::permission::PolicySnapshot>,
    /// Host-resolved `[github]` config credentials for the native GitHub
    /// tools (v2 `configSection.ts`). Env fallbacks are applied Rust-side
    /// (v2 `envOverlay.ts` semantics: config wins, env fills the gap).
    #[serde(default)]
    pub github_token: Option<String>,
    #[serde(default)]
    pub github_base_url: Option<String>,
    /// Host-injected telemetry context (M1c): the host's model configuration
    /// merged into the engine-emitted `host/telemetry` events.
    #[serde(default)]
    pub telemetry: Option<TelemetryContext>,
    /// Session profile catalog snapshot (P46): the profiles the host lets
    /// the native `Agent` tool spawn. Empty = every `Agent` call falls
    /// back to the host tool.
    #[serde(default)]
    pub subagent_profiles: Vec<SubagentProfileWire>,
    /// Host-resolved foreground subagent timeout in ms (v2
    /// `resolveSubagentTimeoutMs`). `None` → engine default (2h).
    #[serde(default)]
    pub subagent_timeout_ms: Option<u64>,
    /// Host-resolved `AgentSwarm` timeout in ms (v2 `resolveSwarmTimeoutMs`).
    /// `None` → the 2h swarm default; swarms never inherit
    /// `subagent_timeout_ms`.
    #[serde(default)]
    pub swarm_timeout_ms: Option<u64>,
    /// The swarm mode the host wants for this turn (v2
    /// `IAgentSwarmService.enter` / `.exit`). `None` leaves the engine's mode
    /// alone; `Some(false)` exits it. Read only at the turn head.
    #[serde(default)]
    pub swarm_mode: Option<bool>,
    /// Why the host opened the mode: `manual` | `task` | `tool`. Absent means
    /// `manual`, matching v2's profile route.
    #[serde(default)]
    pub swarm_mode_trigger: Option<String>,
    /// Host-resolved `[services.moonshot_search]` / `KIMI_WEB_SEARCH_*`
    /// backend (v2 `configSection.ts`). When set, the native WebSearch tool
    /// calls this endpoint instead of scraping DuckDuckGo.
    #[serde(default)]
    pub web_search: Option<WebServiceConfig>,
    /// Host-resolved `[services.moonshot_fetch]` / `KIMI_WEB_FETCH_*`
    /// backend. When set, the native FetchURL tool tries this endpoint
    /// first and falls back to the direct fetch on failure (v2 semantics).
    #[serde(default)]
    pub web_fetch: Option<WebServiceConfig>,
    /// Host-resolved `[services.bing_api]` / `KIMI_BING_*` backend. When set,
    /// the native WebSearch tool falls back to the Bing Web Search API if the
    /// Bing HTML scrape fails.
    #[serde(default)]
    pub bing_api: Option<WebServiceConfig>,
    /// Host-resolved `[image].read_byte_budget` (v2
    /// `resolveReadImageByteBudget`): raw-byte budget for model-initiated
    /// image reads. `None` keeps the 256KB default.
    #[serde(default)]
    pub image_read_byte_budget: Option<u64>,
    /// Host-resolved `[image].max_edge_px` for model-initiated image reads.
    /// `None` keeps the 2000px default.
    #[serde(default)]
    pub image_max_edge_px: Option<u32>,
    /// The session model's declared capabilities (`[models.<alias>]
    /// .capabilities`). `None`/empty = unknown, and image reads are allowed.
    #[serde(default)]
    pub model_capabilities: Option<Vec<String>>,
    /// P52 native-path vetoes: non-empty reason = the engine rejects the
    /// affected native executions with this text as the tool result.
    /// `agent_tool_veto` narrows that to the native `Agent` tool;
    /// `tools_veto` denies every native tool (btw side-channel contexts).
    /// Both are the host's seam and unset by every production caller.
    #[serde(default)]
    pub agent_tool_veto: Option<String>,
    #[serde(default)]
    pub tools_veto: Option<String>,
    #[serde(default)]
    pub todo_tool_veto: Option<String>,
    #[serde(default)]
    pub tower_worktree_root: Option<String>,
    /// Tower enablement resolved by the host (`KIMI_CODE_EXPERIMENTAL_TOWER` /
    /// `[experimental].tower`). `None` leaves the decision to the engine.
    #[serde(default)]
    pub tower_enabled: Option<bool>,
    /// Host-resolved `[sandbox] mode`. Typed rather than a bare `String` so an
    /// unknown spelling fails **deserialization** here — the wire boundary,
    /// which is where v2 rejects the same values (`z.enum` in
    /// `workspace/sandbox/sandbox.ts:20`). A `String` field would have to defer
    /// the decision to the turn pipeline, where a bad value could no longer be
    /// reported as a bad value.
    #[serde(default)]
    pub sandbox_mode: Option<crate::tools::sandbox::SandboxMode>,
    #[serde(default)]
    pub caller_agent_id: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    /// Host-resolved `[secondary_model]` subagent model pool (v2
    /// `session/subagent/configSection.ts`): one ready-built LLM per alias.
    /// Rides the turn params like every other host-resolved setting; `None`
    /// keeps the v2 default (subagents inherit the caller's model).
    #[serde(default)]
    pub secondary_model: Option<SecondaryModelPool>,
    /// Host-resolved `[background]` knobs (v2 `configSection.ts`): the
    /// cooperative-stop grace and the concurrent-task cap for this turn's task
    /// runner. Absent fields keep the engine default (5s / unlimited).
    #[serde(default)]
    pub kill_grace_period_ms: Option<u64>,
    #[serde(default)]
    pub max_running_tasks: Option<u32>,
    /// `[background].bash_auto_background_on_timeout`: a timed-out foreground
    /// Bash call migrates to the background instead of being killed. Absent
    /// keeps the built-in `true`.
    #[serde(default)]
    pub bash_auto_background_on_timeout: Option<bool>,
    /// `[background].bash_task_timeout_s`: default timeout (seconds) for
    /// background Bash tasks. Absent keeps the built-in 600s; `0` means "no
    /// timeout".
    #[serde(default)]
    pub bash_task_timeout_s: Option<u64>,
    /// `[background].print_background_mode` — what a print-mode (`kimi -p`)
    /// session does once its main turn ends with background tasks still
    /// running. Absent keeps the engine's exit-on-turn-end default, so every
    /// non-print entry is unaffected.
    #[serde(default)]
    pub print_background_mode: Option<String>,
    /// `[background].print_wait_ceiling_s`: wall-clock bound on the run's
    /// whole settle phase (seconds). Absent / `0` keeps the documented
    /// default.
    #[serde(default)]
    pub print_wait_ceiling_s: Option<u64>,
    /// `[background].print_max_turns`: cap on the steer turns the engine may
    /// add for background completions. Absent / `0` keeps the documented
    /// default.
    #[serde(default)]
    pub print_max_turns: Option<u32>,
}

/// A subagent profile from the host's session catalog snapshot (P46).
/// Mirrors the v2 `AgentProfile` fields the engine needs to run a
/// foreground subagent: prompt, tool policy, description.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentProfileWire {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub system_prompt: String,
    /// Explicit tool allowlist; empty means every tool minus
    /// `disallowed_tools` (v2 `resolveActiveToolNames` subset).
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub disallowed_tools: Vec<String>,
    /// Host-resolved prompt prefix (v2 `applyProfilePromptPrefix`),
    /// prepended to the prompt as `{prefix}\n\n{prompt}`.
    #[serde(default)]
    pub prompt_prefix: Option<String>,
    /// Summary distillation policy (v2 `AgentProfileSummaryPolicy`).
    #[serde(default)]
    pub summary_policy: Option<crate::subagent::types::SummaryPolicy>,
}

/// LLM provider definition for MultiLLM.
#[derive(Debug, Clone, Deserialize)]
pub struct LlmProviderDef {
    pub name: String,
    pub model: String,
    pub system_prompt: String,
    /// The racer's own native HTTP transport. Absent means this racer proxies
    /// `host/llm_chat`, which only works on a host that serves it.
    #[serde(default)]
    pub native: Option<NativeLlmConfig>,
}

/// Input for a cancel_turn RPC call.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct CancelTurnParams {
    pub turn_id: String,
}

/// A message in the conversation history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
    /// Optional multimodal content blocks. When non-empty, providers
    /// project these instead of the plain `content` string.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocks: Vec<ContentBlock>,
    /// Tool calls issued by an `assistant` message (empty otherwise).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<LlmToolCall>,
    /// For a `tool` message: the id of the tool call this result answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

/// Tool definition passed from the JS side.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub input_schema: serde_json::Value,
}

// ── EngineSession wire types (M1d 3b) ─────────────────────────────────────

/// Params for `session/enqueue_turn`: one prompt as a wire `Message`.
#[derive(Debug, Clone, Deserialize)]
pub struct SessionEnqueueParams {
    pub session_id: String,
    pub prompt: Message,
    pub admission: String,
}

/// Params for the session RPCs that address a session only.
#[derive(Debug, Clone, Deserialize)]
pub struct SessionIdParams {
    pub session_id: String,
}

/// Params for `session/btw_prompt`.
#[derive(Debug, Clone, Deserialize)]
pub struct SessionBtwPromptParams {
    pub session_id: String,
    pub agent_id: String,
    pub prompt: String,
}

/// Params for `session/btw_cancel`.
#[derive(Debug, Clone, Deserialize)]
pub struct SessionBtwCancelParams {
    pub agent_id: String,
}

/// Params for `session/generate_title`. `source` is `first_turn` (default)
/// or `user_prompts`; `digest` is rejected.
#[derive(Debug, Clone, Deserialize)]
pub struct SessionGenerateTitleParams {
    pub session_id: String,
    #[serde(default)]
    pub source: Option<String>,
}

/// Params for `session/background_task_output`.
#[derive(Debug, Clone, Deserialize)]
pub struct SessionBackgroundTaskOutputParams {
    pub session_id: String,
    pub task_id: String,
}

/// Params for `session/background_task_stop`.
#[derive(Debug, Clone, Deserialize)]
pub struct SessionBackgroundTaskStopParams {
    pub session_id: String,
    pub task_id: String,
    #[serde(default)]
    pub reason: Option<String>,
}

/// Params for `session/turn_outcome`.
#[derive(Debug, Clone, Deserialize)]
pub struct SessionTurnOutcomeParams {
    pub session_id: String,
    pub turn_id: u64,
}

/// Params for `session/cancel_turn`. Without `turn_id` the active turn (if
/// any) is cancelled.
#[derive(Debug, Clone, Deserialize)]
pub struct SessionCancelParams {
    pub session_id: String,
    #[serde(default)]
    pub turn_id: Option<u64>,
}

/// Params for the history RPCs (`session/set_history` / `extend_history`).
#[derive(Debug, Clone, Deserialize)]
pub struct SessionHistoryParams {
    pub session_id: String,
    pub history: Vec<Message>,
}

/// Result of `session/turn_outcome`. Mirrors the napi `JsTurnOutcome`:
/// `status` is `ran` or `cancelledBeforeStart`; `result` is present only
/// for `ran`.
#[derive(Debug, Serialize)]
pub struct SessionOutcomeResult {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<RunTurnResult>,
}

/// P56 (G-5): execution-path summary of a session's last completed turn —
/// the cross-process half of `/status`'s engine line.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EngineExecSummary {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_tool_calls: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steps: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
}

/// Result of `session/status`.
#[derive(Debug, Clone, Serialize)]
pub struct SessionStatusResult {
    pub active_turn_id: Option<u64>,
    pub pending_turn_ids: Vec<u64>,
    /// P56 (G-5): absent until the session has run a turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<EngineExecSummary>,
}

/// Result of a run_turn RPC call.
#[derive(Debug, Serialize, Deserialize)]
pub struct RunTurnResult {
    pub stop_reason: String,
    pub steps: u32,
    pub usage: TokenUsage,
    /// Host-visible engine events emitted during the turn.
    #[serde(default)]
    pub events_emitted: u32,
    /// LLM retries performed during the turn (attempts beyond the first).
    #[serde(default)]
    pub llm_retries: u32,
    /// Which LLM transport served this turn: `native-http`, `host-proxy`, `multi`.
    #[serde(default)]
    pub llm_transport: String,
    /// Tool calls executed inside the engine; the rest round-tripped to the host.
    #[serde(default)]
    pub native_tool_calls: u32,
}

// ── LLM proxy types (Rust → JS host) ───────────────────────────────────────

/// Parameters for the host/llm_chat RPC call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmChatRequest {
    pub system_prompt: String,
    pub model_name: String,
    pub messages: Vec<LlmChatMessage>,
    pub tools: Vec<ToolDef>,
    /// Identifies this call so the host can abort it later. Set when the
    /// request is one of several racing providers (MultiLLM); `None` for the
    /// single-provider path, where nothing can lose a race.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

/// A message in the LLM chat request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmChatMessage {
    pub role: String,
    pub content: String,
    /// Optional multimodal content blocks (see [`ContentBlock`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocks: Vec<ContentBlock>,
}

/// Response from the host/llm_chat RPC call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmChatResponse {
    /// Assistant text content. The host proxy path may leave this empty
    /// (the host owns the transcript there); the native HTTP path fills it.
    #[serde(default)]
    pub content: String,
    pub tool_calls: Vec<LlmToolCall>,
    /// Provider reasoning blocks (may be empty). The host-proxy path used to
    /// drop these; they round-trip like message blocks so signatures that
    /// must come back with the block survive the hop.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub thinking: Vec<ContentBlock>,
    pub finish_reason: Option<String>,
    pub usage: TokenUsage,
}

/// A tool call from the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
    /// Provider-specific extras that must round-trip with the provider (e.g.
    /// Gemini `thoughtSignature`; mirrors `turn_loop::types::ToolCall.extras`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extras: Option<serde_json::Value>,
}

// ── Tool execution proxy types (Rust → JS host) ────────────────────────────

/// Serde default for a request that carries no `agent_id`.
///
/// A payload from before the field existed came from the root agent, and v2
/// treats a non-string `agentId` the same way when filtering
/// (`sessionEventBroadcaster.ts:1211-1214` passes it through).
pub(crate) fn main_agent_id() -> String {
    crate::callbacks::MAIN_AGENT_ID.to_string()
}

/// Parameters for the host/execute_tool RPC call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolExecuteRequest {
    pub turn_id: String,
    /// The agent that issued the call (v2 threads `IAgentScopeContext.agentId`
    /// into the tool executor, `toolExecutorService.ts:578-588`). The host
    /// routes the resulting events by it, so a subagent's tool call is filed
    /// under the subagent rather than announced in the main transcript.
    ///
    /// `default` keeps a request that predates the field readable — an older
    /// host's payload carries no agent, and it is the root agent's call.
    #[serde(default = "main_agent_id")]
    pub agent_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub arguments: serde_json::Value,
}

/// Response from the host/execute_tool RPC call.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct ToolExecuteResponse {
    pub content: String,
    pub is_error: bool,
    /// Host-notice annotation carried through to the tool result message
    /// (e.g. Read's `<system>…</system>` summary).
    #[serde(default)]
    pub note: Option<String>,
    /// v2 `TurnEngineToolResult.stopTurn` (loopService.ts:1705): the host
    /// asks the engine to end the turn as `completed` after this result
    /// lands in the history. `#[serde(default)]` keeps older hosts that
    /// omit the field wire-compatible.
    #[serde(default)]
    pub stop_turn: bool,
    /// Rich content the tool wants delivered to the model as a follow-up
    /// `user` message (see [`ToolDelivery`]). Native
    /// tools carry it through this bridge; hosts that produce none omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<ToolDelivery>,
}

/// Token usage tracking.
///
/// Same 4 fields the host uses (`inputOther` / `output` / `inputCacheRead` /
/// `inputCacheCreation`), so `input_tokens` is the **uncached** input
/// remainder: the providers subtract the cache fields from the provider's raw
/// prompt total (`llm/openai.rs::parse_usage`, `llm/anthropic.rs`
/// `parse_response` and its stream accumulator), which is also exactly what
/// the host-proxy leg supplies (the fork's own `rust-loop.ts:2262` maps
/// kosong's `inputOther` in; upstream has no such file). `total_tokens` is
/// `input_tokens + output_tokens` under the same convention — not the
/// provider's raw total.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct TokenUsage {
    #[serde(default)]
    pub input_tokens: u32,
    #[serde(default)]
    pub output_tokens: u32,
    #[serde(default)]
    pub total_tokens: u32,
    /// Prompt tokens served from the provider's cache.
    #[serde(default)]
    pub input_cache_read: u32,
    /// Prompt tokens written into the provider's cache by this call.
    #[serde(default)]
    pub input_cache_creation: u32,
}

impl TokenUsage {
    /// Fold another step/iteration's counts into this accumulator. The
    /// step loop and the Stop-hook continuation loop share it so token
    /// budgets never under-count across iterations.
    pub fn accumulate(&mut self, other: &TokenUsage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.total_tokens += other.total_tokens;
        self.input_cache_read += other.input_cache_read;
        self.input_cache_creation += other.input_cache_creation;
    }

    /// The context one request measured: every input bucket plus the output
    /// (v2 `tokenUsageTotal`). Unlike `total_tokens`, which some providers
    /// report and others leave at zero, this is always derivable.
    pub fn measured_context_tokens(&self) -> u64 {
        u64::from(self.input_tokens)
            + u64::from(self.input_cache_read)
            + u64::from(self.input_cache_creation)
            + u64::from(self.output_tokens)
    }
}

/// Health check response.
#[derive(Debug, Serialize)]
pub struct HealthStatus {
    pub status: String,
    pub version: String,
}

// ── Helper functions ───────────────────────────────────────────────────────

impl JsonRpcResponse {
    pub fn ok(id: RequestId, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result,
        }
    }
}

impl JsonRpcErrorResponse {
    pub fn new(id: RequestId, code: i32, message: String) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            error: JsonRpcError {
                code,
                message,
                data: None,
            },
        }
    }
}

impl JsonRpcError {
    pub fn parse_error() -> Self {
        Self {
            code: -32700,
            message: "Parse error".into(),
            data: None,
        }
    }
    pub fn invalid_request() -> Self {
        Self {
            code: -32600,
            message: "Invalid Request".into(),
            data: None,
        }
    }
    pub fn method_not_found(method: &str) -> Self {
        Self {
            code: -32601,
            message: format!("Method not found: {method}"),
            data: None,
        }
    }
    pub fn internal_error(msg: String) -> Self {
        Self {
            code: -32603,
            message: msg,
            data: None,
        }
    }
}

#[cfg(test)]
mod tests;

// ── Engine/LLM contract types shared with the turn loop ────────────────────

/// Information about an available tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// Media (or rich content) a tool attaches to its result. The turn loop
/// appends it as a follow-up `user` message right after the tool message:
/// content parts on a tool message are rejected by OpenAI-compatible APIs, so
/// an image the model asked to read reaches the conversation the same way a
/// user-pasted image does (v2 `ToolDelivery`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDelivery {
    pub blocks: Vec<ContentBlock>,
    /// Origin of the delivered user message (v2
    /// `ToolDeliveryMessage.origin`), e.g. a `skill_activation` origin so the
    /// transcript attributes it. `None` stays an ordinary user message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<serde_json::Value>,
}

/// Goal status, matching the 6-state machine in [`crate::goal::GoalStatus`].
///
/// This is the **host-wire** spelling: `budgetLimited` / `usageLimited` in
/// camelCase, because the field crosses the napi/stdio boundary. The persisted
/// form is the snake_case spelling in [`crate::goal::GoalStatus`] — the two cover
/// the same six states but are different contracts, so swapping one for the
/// other silently changes either the wire or the on-disk state files.
/// (The comment once pointed at `kimi-native-tools::goal::state` — that crate was
/// folded into this one and its goal module has since been retired.)
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
pub enum GoalStatus {
    #[serde(rename = "active")]
    Active,
    #[serde(rename = "paused")]
    Paused,
    #[serde(rename = "blocked")]
    Blocked,
    #[serde(rename = "complete")]
    Complete,
    #[serde(rename = "budgetLimited")]
    BudgetLimited,
    #[serde(rename = "usageLimited")]
    UsageLimited,
}

impl GoalStatus {
    /// Returns true if the goal is actively being pursued.
    pub fn is_active(self) -> bool {
        matches!(self, GoalStatus::Active)
    }
}

/// Goal context passed from the host for budget-aware turn execution.
///
/// The host owns the durable goal state; this struct carries a snapshot
/// so the Rust loop can check budgets locally and render steering text
/// without an extra round-trip per step.
#[derive(Debug, Clone, Deserialize)]
pub struct GoalContext {
    pub goal_id: String,
    pub objective: String,
    pub status: GoalStatus,
    /// Optional token budget (total tokens allowed).
    pub token_budget: Option<i64>,
    /// Optional turn budget (max turns).
    pub turn_budget: Option<i64>,
    /// Optional wall-clock budget (milliseconds for the whole goal).
    #[serde(default)]
    pub wall_clock_budget_ms: Option<i64>,
    /// Wall-clock milliseconds already consumed before this turn.
    #[serde(default)]
    pub wall_clock_ms: i64,
    /// Cumulative tokens consumed so far (before this turn).
    pub tokens_used: i64,
    /// Cumulative turns run so far (before this turn).
    pub turns_used: i64,
    /// When the host last changed the goal, in Unix milliseconds. The engine
    /// syncs a host-initiated pause / resume / cancel into its own goal store
    /// only when this is newer than the store's last write.
    #[serde(default)]
    pub updated_at: i64,
}

impl GoalContext {
    /// Returns true if adding `turn_tokens`, one more turn, and
    /// `turn_wall_clock_ms` of wall-clock time would exceed any configured
    /// budget.
    pub fn would_exceed_budget(
        &self,
        turn_tokens: i64,
        turns_this_turn: i64,
        turn_wall_clock_ms: i64,
    ) -> bool {
        if let Some(budget) = self.token_budget
            && self.tokens_used + turn_tokens >= budget
        {
            return true;
        }
        if let Some(budget) = self.turn_budget
            && self.turns_used + turns_this_turn >= budget
        {
            return true;
        }
        if let Some(budget) = self.wall_clock_budget_ms
            && self.wall_clock_ms + turn_wall_clock_ms >= budget
        {
            return true;
        }
        false
    }

    /// Maximum fraction of any budget dimension currently consumed
    /// (0.0 when no budgets configured).
    pub fn budget_fraction(
        &self,
        turn_tokens: i64,
        turns_this_turn: i64,
        turn_wall_clock_ms: i64,
    ) -> f64 {
        let mut fractions = Vec::new();
        if let Some(budget) = self.token_budget
            && budget > 0
        {
            fractions.push((self.tokens_used + turn_tokens) as f64 / budget as f64);
        }
        if let Some(budget) = self.turn_budget
            && budget > 0
        {
            fractions.push((self.turns_used + turns_this_turn) as f64 / budget as f64);
        }
        if let Some(budget) = self.wall_clock_budget_ms
            && budget > 0
        {
            fractions.push((self.wall_clock_ms + turn_wall_clock_ms) as f64 / budget as f64);
        }
        fractions.iter().cloned().fold(0.0_f64, f64::max)
    }
}

/// Host-injected context for the engine's turn telemetry (M1c). The host
/// knows the model configuration; the engine observes the turn outcome and
/// emits `turn_started` / `turn_ended` / `turn_interrupted` through
/// `host/telemetry` with both merged.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct TelemetryContext {
    pub mode: String,
    pub provider_type: String,
    pub protocol: String,
    pub thinking_effort: Option<String>,
    /// Comma-separated sorted ids of the enabled, loaded plugins (v2 #3963):
    /// an empty string is a known empty set, `None` means the host has no
    /// plugin snapshot to report.
    pub enabled_plugins: Option<String>,
}
