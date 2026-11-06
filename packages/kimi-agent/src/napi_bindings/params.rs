//! `#[napi(object)]` parameter and result types exchanged between the
//! JS side and the Rust engine across the napi boundary.

use napi_derive::napi;
use std::collections::HashMap;

// ── napi JS-side types ─────────────────────────────────────────────────────

#[napi(object)]
#[derive(Clone)]
pub struct JsRunTurnParams {
    pub turn_id: String,
    pub system_prompt: String,
    pub model_name: String,
    pub messages: Vec<JsMessage>,
    pub tools: Vec<JsToolDef>,
    /// Step cap for the turn loop. `None` = unbounded (JS-loop semantics).
    pub max_steps: Option<u32>,
    /// LLM retry attempts per step (v2 `loopControl.maxAttemptsPerStep`).
    /// `None` = engine default (10).
    pub max_attempts: Option<u32>,
    /// Total requests one compaction round may issue (v2 #3750,
    /// `loopControl.compactionMaxAttempts`). `None` = engine default (5);
    /// the engine floors a provided value at 1.
    pub compaction_max_attempts: Option<u32>,
    /// Context window the host resolved for the active model. `None` keeps the
    /// engine's default compaction budget.
    pub max_context_tokens: Option<u32>,
    pub goal: Option<JsGoalContext>,
    /// Native HTTP LLM transport. When present, Rust calls the provider
    /// directly (SSE streaming) instead of proxying through the host.
    pub native_llm: Option<JsNativeLlmConfig>,
    /// Concurrent MultiLLM providers (first-past-the-post race). When
    /// non-empty, the loop dispatches every step to all providers in
    /// parallel and accepts the first successful response. Wins over
    /// `native_llm` only when set.
    pub providers: Option<Vec<JsLlmProviderDef>>,
    /// Workspace root used to sandbox native tool execution.
    pub workspace_root: Option<String>,
    /// When true (with `workspace_root`), the in-process toolset
    /// (Read/Grep/Glob/Write/Edit/Bash, each gated on a host permission
    /// grant) runs inside the Rust process. Any tool not in that set, or
    /// any argument shape the toolset cannot handle, falls back to the
    /// host (`host/execute_tool`).
    ///
    /// Absent means `false`: executing on the host stays the fail-safe for a
    /// caller that does not state an intent, and the product default
    /// (native on) is resolved by the TS adapter — matching the stdio wire,
    /// where an absent `native_tools` is likewise false.
    pub native_tools: Option<bool>,
    /// Host-authorized extra roots (`/add-dir` → `additionalDirs`). Paths that
    /// canonicalize under one of them are served by the native toolset even
    /// though they sit outside `workspace_root`; without this they could only
    /// fall back to the host, which has no tool runtime on this transport.
    pub additional_dirs: Option<Vec<String>>,
    /// Rust engine self-contained mode. When true, the engine refuses to
    /// fall back to the host proxy for LLM calls — the user must
    /// configure either `providers` (concurrent MultiLLM race) or
    /// `native_llm` (single provider direct HTTP), or the engine errors
    /// out at construction time instead of silently routing through
    /// `host/llm_chat`. Mirrors `agent.rustSelfContained` from config.
    pub rust_self_contained: Option<bool>,
    /// Host shell for native Bash (bash everywhere, Git Bash on Windows).
    /// Absent on Windows → native Bash stays with the host.
    pub shell_path: Option<String>,
    /// Optional JSON-serialized PolicySnapshot for local permission evaluation (P26 批 3).
    pub policy_snapshot_json: Option<String>,
    /// Optional JSON-serialized `UsageStatus` snapshot (`{ byModel, total,
    /// currentTurn }`) the host persisted for this session. The engine seeds
    /// its buckets with it and owns every later fold; the host never re-derives
    /// them.
    pub initial_usage_json: Option<String>,
    /// Optional JSON-serialized `[secondary_model]` subagent model pool: the
    /// default alias, `force`, and one resolved LLM config per pool entry.
    /// Absent = subagents inherit the caller's model.
    pub secondary_model_json: Option<String>,
    /// Host-resolved `[github]` config credentials for the native GitHub
    /// tools (v2 `configSection.ts`). Env fallbacks are applied Rust-side
    /// (v2 `envOverlay.ts` semantics: config wins, env fills the gap).
    pub github_token: Option<String>,
    pub github_base_url: Option<String>,
    /// Host-injected telemetry context (M1c): the host's model configuration
    /// merged into the engine-emitted `host/telemetry` events.
    pub telemetry: Option<JsTelemetryContext>,
    /// Session profile catalog snapshot (P46): profiles the native `Agent`
    /// tool may spawn. Empty/absent = every `Agent` call falls back to
    /// the host tool.
    pub subagent_profiles: Option<Vec<JsSubagentProfile>>,
    /// Host-resolved foreground subagent timeout in ms (v2
    /// `resolveSubagentTimeoutMs`). Absent → engine default (2h). `i64`
    /// because napi cannot read JS numbers as `u64`.
    pub subagent_timeout_ms: Option<i64>,
    /// Host-resolved `AgentSwarm` timeout in ms (v2 `resolveSwarmTimeoutMs`).
    /// Absent → the 2h swarm default; swarms never inherit
    /// `subagent_timeout_ms`. `i64` because napi cannot read JS numbers as
    /// `u64`.
    pub swarm_timeout_ms: Option<i64>,
    /// The swarm mode the host wants for this turn (v2
    /// `IAgentSwarmService.enter` / `.exit`). `None` leaves the engine's mode
    /// alone; `Some(false)` exits it.
    ///
    /// The napi surface has no other way to express this: the engine owns the
    /// mode (the `swarm_mode` reminder and the turn-end auto-exit read it), so
    /// without this field a host-side `/swarm` toggle was a status flag the
    /// turn never saw.
    pub swarm_mode: Option<bool>,
    /// Why the host opened the mode: `manual` | `task` | `tool`. Absent means
    /// `manual`, matching v2's profile route. Only read when `swarm_mode` is
    /// `Some(true)`.
    pub swarm_mode_trigger: Option<String>,
    /// P52 native-path vetoes (host-formatted deny reasons; see
    /// `RunTurnParams`).
    pub agent_tool_veto: Option<String>,
    pub tools_veto: Option<String>,
    pub todo_tool_veto: Option<String>,
    pub tower_worktree_root: Option<String>,
    /// Host-resolved tower enablement (`KIMI_CODE_EXPERIMENTAL_TOWER` /
    /// `[experimental].tower`). `None` falls back to the engine's own env
    /// probe.
    pub tower_enabled: Option<bool>,
    /// Host-resolved NotifyUser availability (`KIMI_CODE_EXPERIMENTAL_NOTIFY_USER`
    /// flag AND a host update panel — upstream #4054's two-input gate).
    /// `None` = `true`: the TUI host has an updates panel.
    pub notify_user_enabled: Option<bool>,
    /// Main-agent profile name (`--agent`, or the name a `--agent-file`
    /// defines). Selects the role overlay built into the session prompt;
    /// `None` keeps the default `agent` profile. Unlike `subagent_profiles`
    /// (which only feeds the `Agent` tool's spawn catalog), this one shapes
    /// the main turn's system prompt.
    pub agent_profile: Option<String>,
    /// Host-resolved progressive tool disclosure (`[experimental].
    /// tool_select`). `None`/`false` keeps every tool advertised inline.
    pub tool_select: Option<bool>,
    pub sandbox_mode: Option<String>,
    pub caller_agent_id: Option<String>,
    pub session_id: Option<String>,
    /// Native MCP servers configuration (P73).
    pub mcp_servers: Option<Vec<JsMcpServerConfig>>,
    /// Directory of the MCP OAuth credential store (`<home>/credentials/mcp`).
    /// Installed on the session's MCP manager so stored tokens are injected
    /// and refreshed by the engine; `None` leaves OAuth uninstalled.
    pub mcp_oauth_dir: Option<String>,
    /// Host-resolved `[mcp]` defaults (v2 `resolveDefaults`). Per-server
    /// values win; `None` keeps the engine's built-in defaults.
    pub mcp_defaults: Option<JsMcpDefaults>,
    /// Host-resolved `[services.moonshot_search]` / `KIMI_WEB_SEARCH_*`
    /// backend (v2 `configSection.ts`). When set, the native WebSearch tool
    /// calls this endpoint instead of scraping DuckDuckGo.
    pub web_search: Option<JsWebServiceConfig>,
    /// Host-resolved `[services.moonshot_fetch]` / `KIMI_WEB_FETCH_*`
    /// backend. When set, the native FetchURL tool tries this endpoint
    /// first and falls back to the direct fetch on failure (v2 semantics).
    pub web_fetch: Option<JsWebServiceConfig>,
    /// Host-resolved `[services.bing_api]` / `KIMI_BING_*` backend. When set,
    /// the native WebSearch tool falls back to the Bing Web Search API if the
    /// Bing HTML scrape fails.
    pub bing_api: Option<JsWebServiceConfig>,
    /// Host-resolved `[image].read_byte_budget` (v2
    /// `resolveReadImageByteBudget`). `None` keeps the 256KB default.
    pub image_read_byte_budget: Option<i64>,
    /// Host-resolved `[image].max_edge_px`. `None` keeps the 2000px default.
    /// `i64` because napi cannot read JS numbers as `u32`.
    pub image_max_edge_px: Option<i64>,
    /// The session model's declared capabilities (`[models.<alias>]
    /// .capabilities`). `None`/empty = unknown.
    pub model_capabilities: Option<Vec<String>>,
    /// Host-resolved `[background]` knobs (v2 `configSection.ts`). Absent
    /// fields keep the engine default (5s stop grace / unlimited concurrency /
    /// auto-background on / 600s background Bash timeout); `bash_task_timeout_s
    /// = 0` means "no timeout". `i64` because napi cannot read JS numbers as
    /// `u64`.
    pub kill_grace_period_ms: Option<i64>,
    pub max_running_tasks: Option<i64>,
    pub bash_auto_background_on_timeout: Option<bool>,
    pub bash_task_timeout_s: Option<i64>,
    /// `[background].print_background_mode` — what a print-mode (`kimi -p`)
    /// session does once its main turn ends with background tasks still
    /// running: `exit` resolves the turn receipt at once, `drain` / `steer`
    /// hold it until the task runner drains. Absent keeps the engine's
    /// exit-on-turn-end default.
    pub print_background_mode: Option<String>,
    /// `[background].print_wait_ceiling_s`: wall-clock bound on that hold, in
    /// seconds. Absent / non-positive keeps the documented default. `i64`
    /// because napi cannot read JS numbers as `u64`.
    pub print_wait_ceiling_s: Option<i64>,
    /// `[background].print_max_turns`: cap on the steer turns the engine may
    /// add for background completions. Absent / non-positive keeps the
    /// documented default.
    pub print_max_turns: Option<i64>,
}

/// The compaction window for one run: the model's declared input cap wins over
/// the host's total window (schema `models.*.maxInputSize`: prompt-budget
/// checks prefer it, completion budgeting keeps the window), and the host
/// window is the fallback.
pub(crate) fn compaction_window(
    native_llm: Option<&JsNativeLlmConfig>,
    host_window: Option<u32>,
) -> Option<u32> {
    native_llm
        .and_then(|cfg| cfg.max_input_size)
        .or(host_window)
}

/// Resolve the `[background]` print policy carried by the session params.
///
/// `None` (no `print_background_mode`) leaves the engine untouched: a turn
/// receipt resolves when the turn ends, whatever the background tasks do.
/// `i64` → `u64`/`u32` narrowing lives in
/// [`crate::session::PrintBackgroundPolicy::from_wire`].
pub(crate) fn print_background_policy(
    params: &JsRunTurnParams,
) -> Option<crate::session::PrintBackgroundPolicy> {
    crate::session::PrintBackgroundPolicy::from_wire(
        params.print_background_mode.as_deref(),
        params
            .print_wait_ceiling_s
            .and_then(|ceiling| u64::try_from(ceiling).ok()),
        params
            .print_max_turns
            .and_then(|max_turns| u32::try_from(max_turns).ok()),
    )
}

/// MCP server configuration for pure-Rust MCP manager (P73).
#[napi(object)]
#[derive(Clone, Debug)]
pub struct JsMcpServerConfig {
    pub name: String,
    pub transport: String,
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    pub env: Option<HashMap<String, String>>,
    pub url: Option<String>,
    pub headers: Option<HashMap<String, String>>,
    /// Working directory for a stdio server (v2 `cwd`).
    pub cwd: Option<String>,
    /// Env var holding a bearer token for a remote server (v2
    /// `bearerTokenEnvVar`).
    pub bearer_token_env_var: Option<String>,
    /// `false` keeps the server listed as `disabled` and skips connecting it.
    pub enabled: Option<bool>,
    /// Allowlist of tool names exposed to the model (v2 `enabledTools`).
    pub enabled_tools: Option<Vec<String>>,
    /// Denylist applied after the allowlist (v2 `disabledTools`).
    pub disabled_tools: Option<Vec<String>>,
    /// Startup (connect + discovery) timeout in milliseconds (v2
    /// `startupTimeoutMs`).
    pub startup_timeout_ms: Option<u32>,
    /// Single tool-call timeout in milliseconds (v2 `toolTimeoutMs`).
    pub tool_timeout_ms: Option<u32>,
    /// Keep this server's tools out of the top-level tool list and load them
    /// on demand through `select_tools` (v2 per-server `deferred`).
    pub deferred: Option<bool>,
}

/// Global `[mcp]` defaults handed to the engine (v2 `resolveDefaults`):
/// per-server values always win, these fill the gap, and the built-in
/// defaults sit behind them. Without this field the napi transport had no
/// way to see the `[mcp]` section at all, so `startup_timeout_ms`,
/// `tool_timeout_ms` and `defaultDeferred` were silently ignored for every
/// SDK/TUI session.
#[napi(object)]
#[derive(Clone)]
pub struct JsMcpDefaults {
    pub startup_timeout_ms: Option<u32>,
    pub tool_timeout_ms: Option<u32>,
    /// Keep every server's tools out of the top-level tool list unless the
    /// server declares `deferred` itself (v2 `defaultDeferred`).
    pub default_deferred: Option<bool>,
}

/// A subagent profile from the host's session catalog snapshot (P46).
#[napi(object)]
#[derive(Clone)]
pub struct JsSubagentProfile {
    pub name: String,
    pub description: Option<String>,
    /// Optional usage guidance from the agent file's `whenToUse` frontmatter.
    pub when_to_use: Option<String>,
    pub system_prompt: Option<String>,
    /// Explicit tool allowlist; empty means every tool minus
    /// `disallowed_tools`.
    pub tools: Option<Vec<String>>,
    pub disallowed_tools: Option<Vec<String>>,
    /// Host-resolved prompt prefix (v2 `applyProfilePromptPrefix`),
    /// prepended to the prompt as `{prefix}\n\n{prompt}` (P51).
    pub prompt_prefix: Option<String>,
    /// Serialized summary distillation policy (v2
    /// `AgentProfileSummaryPolicy`): `{ minChars, continuationPrompt,
    /// retries }` (P51). Serialized JSON because napi cannot express
    /// nested optionals in a flat object cleanly.
    pub summary_policy_json: Option<String>,
    /// The subagent types this profile may itself spawn (v2
    /// `AgentProfile.subagents`, from the agent file's frontmatter).
    pub sub_agents: Option<Vec<String>>,
}

/// The host-side half of the turn telemetry payload (M1c): fields the host
/// knows from its model configuration; the engine contributes the outcome
/// fields (reason / duration_ms / steps / at_step / interrupt_reason).
#[napi(object)]
#[derive(Clone)]
pub struct JsTelemetryContext {
    pub mode: String,
    pub provider_type: String,
    pub protocol: String,
    pub thinking_effort: Option<String>,
    /// Comma-separated sorted ids of the enabled, loaded plugins (v2 #3963);
    /// an empty string is a known empty set, absent when the host has no
    /// plugin snapshot to report.
    pub enabled_plugins: Option<String>,
}

#[napi(object)]
#[derive(Clone)]
pub struct JsLlmProviderDef {
    /// Provider label surfaced in events/telemetry for debugging.
    pub name: String,
    /// Per-provider model identifier (free-form string).
    pub model: String,
    /// Per-provider system prompt override.
    pub system_prompt: String,
    /// The racer's own native HTTP transport. Present for a provider the
    /// engine can call directly (`[agent].multi_llm` resolving each alias
    /// through `[providers.*]`); absent means this racer proxies
    /// `host/llm_chat`, which only works on a host that serves it.
    pub native: Option<JsNativeLlmConfig>,
}

#[napi(object)]
#[derive(Clone)]
pub struct JsNativeLlmConfig {
    /// "openai" (Chat Completions) or "anthropic" (Messages), or "google" / "openai_responses".
    pub protocol: String,
    /// The `[models.<alias>]` name the host resolved this entry from (v2
    /// `ApiErrorEvent.alias`), carried so the engine's telemetry can name the
    /// alias the user selected rather than only the wire model.
    pub alias: Option<String>,
    /// Declared provider family (`[providers.*].type`, e.g. "kimi"): the
    /// transport encodes the Kimi `thinking` object instead of a top-level
    /// `reasoning_effort` for it.
    pub provider_type: Option<String>,
    /// API base URL including the version segment (e.g. `.../v1`).
    pub base_url: String,
    pub api_key: String,
    /// Name of the environment variable the transport reads the credential
    /// from at request time (`[providers.*].api_key_env`). Set only when the
    /// provider carries neither a static key nor an OAuth binding; an empty
    /// `api_key` with this name and no `auth_provider` is the env-only channel
    /// the transport's `credential()` accepts. Absent means the credential is
    /// the static `api_key`.
    pub api_key_env: Option<String>,
    pub model: String,
    pub max_tokens: Option<u32>,
    /// Extra headers from `[providers.*].customHeaders`, sent with every
    /// request. Absent means none.
    pub custom_headers: Option<std::collections::HashMap<String, String>>,
    /// Reasoning effort for OpenAI-compatible models (e.g. "low", "medium", "high", "max").
    pub reasoning_effort: Option<String>,
    /// Thinking budget in tokens for Anthropic Messages API.
    pub thinking_budget: Option<u32>,
    /// OAuth-managed auth: the host-side provider name the transport asks for
    /// a bearer token (`host/auth_token`) instead of using the static
    /// `api_key`. Absent means static-key auth.
    pub auth_provider: Option<String>,
    /// Moonshot preserved-thinking passthrough (`thinking.keep`): `keep` on
    /// the kimi/openai body, a `clear_thinking_20251015` context-management
    /// edit on anthropic. Host filters off-values; absent = no keep on the
    /// wire.
    pub thinking_keep: Option<String>,
    /// Route an anthropic-protocol model through the beta Messages API
    /// (`POST {base}/messages?beta=true`); absent means the standard endpoint.
    pub beta_api: Option<bool>,
    /// The model's declared capabilities (`[models.<alias>].capabilities`).
    pub capabilities: Option<Vec<String>>,
    /// The model's own system prompt (`[models.<alias>].system_prompt`).
    pub system_prompt: Option<String>,
    /// Declared input cap when below the window
    /// (`[models.<alias>].max_input_size`).
    pub max_input_size: Option<u32>,
    /// Explicit adaptive-thinking support
    /// (`[models.<alias>].adaptive_thinking`).
    pub adaptive_thinking: Option<bool>,
    /// The wire field carrying reasoning content
    /// (`[models.<alias>].reasoning_key`).
    pub reasoning_key: Option<String>,
    /// The effort value that encodes "thinking off" on the wire
    /// (`[models.<alias>].off_effort`).
    pub off_effort: Option<String>,
}

/// One host-resolved `[services.moonshot_*]` entry (v2 `configSection.ts`):
/// the endpoint the native WebSearch / FetchURL tools call instead of the
/// built-in DuckDuckGo scrape / direct HTTP fetch.
#[napi(object)]
#[derive(Clone)]
pub struct JsWebServiceConfig {
    pub base_url: String,
    pub api_key: Option<String>,
    pub custom_headers: Option<std::collections::HashMap<String, String>>,
}

#[napi(object)]
#[derive(Clone)]
pub struct JsMessage {
    pub role: String,
    pub content: String,
    /// JSON-serialized `ContentBlock[]` for multimodal messages
    /// (`[{"type":"text",...},{"type":"image_url",...}]`). Optional.
    pub blocks_json: Option<String>,
    /// JSON-serialized tool calls (`[{id,name,arguments}]`) for an
    /// assistant history message. Optional.
    pub tool_calls_json: Option<String>,
    /// For a `tool` history message: the tool call id it answers.
    pub tool_call_id: Option<String>,
}

#[napi(object)]
#[derive(Clone)]
pub struct JsToolDef {
    pub name: String,
    pub description: String,
    /// JSON string of the tool's input schema (e.g. `{"type":"object",...}`).
    /// serde_json::Value does not implement napi ToNapiValue/FromNapiValue,
    /// so we pass the schema as a serialized JSON string.
    pub input_schema: String,
}

#[napi(object)]
#[derive(Clone)]
pub struct JsGoalContext {
    pub goal_id: String,
    pub objective: String,
    pub status: String,
    pub token_budget: Option<i64>,
    pub turn_budget: Option<i64>,
    pub wall_clock_budget_ms: Option<i64>,
    pub wall_clock_ms: i64,
    pub tokens_used: i64,
    pub turns_used: i64,
    pub updated_at: i64,
    /// Why the host stopped the goal, when it stopped one (v2
    /// `GoalSnapshot.terminalReason`). Without it the engine's synced copy of a
    /// host-side pause has no reason to show.
    pub terminal_reason: Option<String>,
}

#[napi(object)]
pub struct JsRunTurnResult {
    pub stop_reason: String,
    pub steps: u32,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub total_tokens: u32,
    /// Prompt tokens served from the provider's cache.
    pub input_cache_read: u32,
    /// Prompt tokens written into the provider's cache by this turn.
    pub input_cache_creation: u32,
    /// Host-visible engine events emitted during the turn.
    pub events_emitted: u32,
    /// LLM retries performed during the turn (attempts beyond the first).
    pub llm_retries: u32,
    /// Which LLM transport served this turn: `native-http`, `host-proxy`, `multi`.
    pub llm_transport: String,
    /// Tool calls executed inside the engine; the rest round-tripped to the host.
    pub native_tool_calls: u32,
}

/// Live session shape for the JS side (v2 `AgentLoopStatus`).
#[napi(object)]
pub struct JsSessionStatus {
    pub active_turn_id: Option<f64>,
    pub pending_turn_ids: Vec<f64>,
    /// P56 (G-5): execution-path summary of the last completed turn.
    pub engine: Option<JsEngineExecSummary>,
    /// v2 `UsageStatus` as a JSON snapshot (`{ byModel, total, currentTurn }`).
    /// The host persists it opaquely and passes it back as
    /// `initial_usage_json`; it never re-derives the buckets itself.
    pub usage_json: Option<String>,
}

/// P56 (G-5): cross-process engine execution summary.
#[napi(object)]
#[derive(Clone, Default)]
pub struct JsEngineExecSummary {
    pub transport: Option<String>,
    pub native_tool_calls: Option<f64>,
    pub steps: Option<f64>,
    pub stop_reason: Option<String>,
}

/// The outcome of one enqueued turn. Engine-side failures reject the outcome
/// promise; `cancelledBeforeStart` means the turn was dropped from the queue
/// without running.
#[napi(object)]
pub struct JsTurnOutcome {
    pub status: String,
    pub result: Option<JsRunTurnResult>,
}
