//! Pure Rust configuration and credential manager for `kimi-agent` (P27 批 1).
//!
//! Loads and validates `config.toml` without depending on Node/Bun or any JS runtimes.
//! Discovers configuration from user-owned locations (`KIMI_CONFIG_PATH`,
//! `KIMI_CODE_HOME`, `~/.kimi-code/config.toml`, legacy `~/.kimi/config.toml`).
//! A `config.toml` in the current directory is deliberately not discovered: a
//! checked-out repository could otherwise inject providers, hooks and
//! permission defaults before any trust prompt.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::env::env_switch_default_off;
use crate::environment::EnvironmentsConfig;
use crate::permission::{HookDef, PermissionMode, PolicySnapshot};
use crate::rpc::types::{
    NativeLlmConfig, ResolvedMultiLlmProvider, SecondaryModelEntry, SecondaryModelPool,
};

pub mod write;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderConfig {
    #[serde(rename = "default_model", default)]
    pub default_model: Option<String>,
    #[serde(rename = "type", default)]
    pub provider_type: Option<String>,
    #[serde(rename = "api_key", default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(rename = "base_url", default)]
    pub base_url: Option<String>,
    #[serde(rename = "max_tokens", default)]
    pub max_tokens: Option<u32>,
    /// OAuth binding (v2 `providers.*.oauth`): its presence marks the
    /// provider OAuth-authenticated even without a static key.
    #[serde(default)]
    pub oauth: Option<serde_json::Value>,
    /// Custom-registry provenance (v2 `providers.*.source`): the api.json
    /// URL (and key) this provider was imported from. Its presence is what
    /// makes the refresh rediscover the provider's registry; without the
    /// field the blob would be dropped on read and the refresh blind.
    #[serde(default)]
    pub source: Option<serde_json::Value>,
    /// Extra HTTP headers sent with every request to this provider
    /// (schema `providers.*.customHeaders`). Self-hosted gateways put their
    /// auth/routing headers here; without it those deployments 401.
    #[serde(rename = "custom_headers", alias = "customHeaders", default)]
    pub custom_headers: Option<HashMap<String, String>>,
    /// The antigravity bridge's `agy` binary (v2 `AntigravityChatProvider`
    /// binary override). Absent probes the reference paths and then `PATH`.
    #[serde(rename = "binary_path", alias = "binaryPath", default)]
    pub binary_path: Option<String>,
    /// Bounded waits for the antigravity subprocess bridge.
    #[serde(default)]
    pub antigravity: Option<crate::rpc::types::AntigravityLimits>,
    /// Where the provider's model list comes from (v2 `modelSource`:
    /// `static` | `discover` | `oauth-catalog`). The host model refresh reads
    /// it; the engine carries it so the field is not dropped on read.
    #[serde(rename = "model_source", alias = "modelSource", default)]
    pub model_source: Option<String>,
    /// Extra environment variables applied to this provider's transport
    /// (v2 `ProviderConfig.env`).
    #[serde(default)]
    pub env: Option<HashMap<String, String>>,
}

/// v2 `ModelUpgrade`: the model-migration metadata attached to a retiring
/// alias (`llm-adapter/model/model.ts:9-14`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelUpgradeConfig {
    pub model: String,
    #[serde(default, alias = "migrationMarkdown")]
    pub migration_markdown: String,
    #[serde(default, alias = "retirementAt")]
    pub retirement_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelAliasConfig {
    #[serde(default)]
    pub provider: Option<String>,
    /// The document schema's spelling of `provider` (v2
    /// `ModelRecord.providerId`); it wins when both are set.
    #[serde(rename = "provider_id", alias = "providerId", default)]
    pub provider_id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// The wire-facing model name (v2 `ModelRecord.name`): the name sent to
    /// the provider, ahead of `model`. Never the alias.
    #[serde(default)]
    pub name: Option<String>,
    /// Extra names this entry can be looked up by (v2 `ModelRecord.aliases`).
    #[serde(default)]
    pub aliases: Option<Vec<String>>,
    /// Per-model credential (v2 `ModelRecord.apiKey`): wins over the
    /// provider's key.
    #[serde(rename = "api_key", alias = "apiKey", default)]
    pub api_key: Option<String>,
    /// Per-model OAuth binding (v2 `ModelRecord.oauth`), same shape as
    /// `[providers.*].oauth`: its presence marks the model
    /// OAuth-authenticated even without a static key.
    #[serde(default)]
    pub oauth: Option<serde_json::Value>,
    #[serde(rename = "system_prompt", alias = "systemPrompt", default)]
    pub system_prompt: Option<String>,
    /// Catalog fields the REST surface exposes (v2 `ModelRecord`).
    #[serde(rename = "max_context_size", alias = "maxContextSize", default)]
    pub max_context_size: Option<u32>,
    #[serde(rename = "display_name", alias = "displayName", default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub capabilities: Option<Vec<String>>,
    #[serde(rename = "max_output_size", alias = "maxOutputSize", default)]
    pub max_output_size: Option<u32>,
    #[serde(rename = "support_efforts", alias = "supportEfforts", default)]
    pub support_efforts: Option<Vec<String>>,
    #[serde(rename = "default_effort", alias = "defaultEffort", default)]
    pub default_effort: Option<String>,
    #[serde(rename = "adaptive_thinking", alias = "adaptiveThinking", default)]
    pub adaptive_thinking: Option<bool>,
    /// Per-model wire protocol override (v2 `ModelRecord.protocol`).
    #[serde(default)]
    pub protocol: Option<String>,
    /// Route anthropic-protocol models through the beta Messages API.
    #[serde(rename = "beta_api", alias = "betaApi", default)]
    pub beta_api: Option<bool>,
    /// Per-model endpoint override (schema `models.*.baseUrl`): a gateway
    /// provider serving this alias over a different endpoint than the
    /// provider default. Wins over the provider's `base_url`.
    #[serde(rename = "base_url", alias = "baseUrl", default)]
    pub base_url: Option<String>,
    /// Declared prompt/input cap when below the total window (schema
    /// `models.*.maxInputSize`).
    #[serde(rename = "max_input_size", alias = "maxInputSize", default)]
    pub max_input_size: Option<u32>,
    /// The wire field carrying reasoning content for this model (schema
    /// `models.*.reasoningKey`).
    #[serde(rename = "reasoning_key", alias = "reasoningKey", default)]
    pub reasoning_key: Option<String>,
    /// The effort value that encodes "thinking off" on the wire (schema
    /// `models.*.offEffort`): models whose default is to reason need it sent
    /// instead of omitting the effort field.
    #[serde(rename = "off_effort", alias = "offEffort", default)]
    pub off_effort: Option<String>,
    /// Request-body knobs (v2 `responseFormat` / `sampling` / `determinism` /
    /// `extraParams`): carried to the native transport, which keeps them inert
    /// until declared.
    #[serde(
        rename = "response_format",
        alias = "responseFormat",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub response_format: Option<crate::rpc::types::NativeResponseFormat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampling: Option<crate::rpc::types::NativeSampling>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub determinism: Option<crate::rpc::types::NativeDeterminism>,
    #[serde(
        rename = "extra_params",
        alias = "extraParams",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub extra_params: Option<crate::rpc::types::NativeExtraParams>,
    /// Model-retirement metadata (v2 `ModelRecord.upgrade`): the suggested
    /// replacement alias, the migration note, and the retirement date.
    #[serde(default)]
    pub upgrade: Option<ModelUpgradeConfig>,
    /// Catalog fields overridden on top of the base record (v2
    /// `ModelRecord.overrides`): a provider-model refresh may rewrite the base
    /// fields, so a user override is the only value that survives it.
    #[serde(default)]
    pub overrides: Option<ModelOverrideConfig>,
}

/// `[models.<alias>.overrides]` (v2 `ModelOverrideSchema`): the catalog fields
/// an override may shadow. Identity and routing fields are deliberately absent
/// — an override cannot repoint a model at another provider, endpoint, or wire
/// protocol.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelOverrideConfig {
    #[serde(rename = "max_context_size", alias = "maxContextSize", default)]
    pub max_context_size: Option<u32>,
    #[serde(rename = "max_input_size", alias = "maxInputSize", default)]
    pub max_input_size: Option<u32>,
    #[serde(rename = "max_output_size", alias = "maxOutputSize", default)]
    pub max_output_size: Option<u32>,
    #[serde(default)]
    pub capabilities: Option<Vec<String>>,
    #[serde(rename = "display_name", alias = "displayName", default)]
    pub display_name: Option<String>,
    #[serde(rename = "reasoning_key", alias = "reasoningKey", default)]
    pub reasoning_key: Option<String>,
    #[serde(rename = "adaptive_thinking", alias = "adaptiveThinking", default)]
    pub adaptive_thinking: Option<bool>,
    #[serde(rename = "support_efforts", alias = "supportEfforts", default)]
    pub support_efforts: Option<Vec<String>>,
    #[serde(rename = "default_effort", alias = "defaultEffort", default)]
    pub default_effort: Option<String>,
    #[serde(rename = "off_effort", alias = "offEffort", default)]
    pub off_effort: Option<String>,
}

impl ModelAliasConfig {
    /// The entry with its `overrides` block applied (v2 `effectiveRecordOf`):
    /// an override wins over the base field it shadows, an absent one leaves
    /// the base value. Identity and routing fields are not overridable, so they
    /// always come from the base record.
    fn effective(&self) -> Self {
        let Some(overrides) = self.overrides.as_ref() else {
            return self.clone();
        };
        let mut effective = self.clone();
        effective.max_context_size = overrides.max_context_size.or(self.max_context_size);
        effective.max_input_size = overrides.max_input_size.or(self.max_input_size);
        effective.max_output_size = overrides.max_output_size.or(self.max_output_size);
        effective.capabilities = overrides
            .capabilities
            .clone()
            .or_else(|| self.capabilities.clone());
        effective.display_name = overrides
            .display_name
            .clone()
            .or_else(|| self.display_name.clone());
        effective.reasoning_key = overrides
            .reasoning_key
            .clone()
            .or_else(|| self.reasoning_key.clone());
        effective.adaptive_thinking = overrides.adaptive_thinking.or(self.adaptive_thinking);
        effective.support_efforts = overrides
            .support_efforts
            .clone()
            .or_else(|| self.support_efforts.clone());
        effective.default_effort = overrides
            .default_effort
            .clone()
            .or_else(|| self.default_effort.clone());
        effective.off_effort = overrides
            .off_effort
            .clone()
            .or_else(|| self.off_effort.clone());
        effective
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentConfig {
    #[serde(default)]
    pub engine: Option<String>,
    #[serde(rename = "multi_llm", default)]
    pub multi_llm: Option<Vec<String>>,
    #[serde(rename = "native_llm_provider", default)]
    pub native_llm_provider: Option<String>,
    #[serde(rename = "native_tools", default)]
    pub native_tools: Option<bool>,
    #[serde(rename = "rust_self_contained", default)]
    pub rust_self_contained: Option<bool>,
    #[serde(default)]
    pub yolo: Option<bool>,
    /// `[agent].plan_mode`: start turns in plan mode by default (schema).
    #[serde(rename = "plan_mode", alias = "planMode", default)]
    pub plan_mode: Option<bool>,
}

/// `[model_catalog]` section (schema `ModelCatalogConfig`): automatic
/// provider-model discovery.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelCatalogConfig {
    /// Interval (ms) between automatic provider-model refreshes. `0`/unset
    /// disables the periodic refresh.
    #[serde(rename = "refresh_interval_ms", alias = "refreshIntervalMs", default)]
    pub refresh_interval_ms: Option<u64>,
    /// Refresh once shortly after the daemon starts.
    #[serde(rename = "refresh_on_start", alias = "refreshOnStart", default)]
    pub refresh_on_start: Option<bool>,
}

/// `[shell]` section (v2 `configSection.ts`): pin the local command shell.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ShellConfig {
    /// `auto` (default) | `bash` | `powershell` | `pwsh` | `cmd`. Any other
    /// value falls back to auto-detection. `KIMI_SHELL_PATH` still wins.
    #[serde(default)]
    pub preference: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PermissionRuleConfig {
    #[serde(default)]
    pub decision: Option<String>,
    #[serde(default)]
    pub pattern: Option<String>,
    /// Why the rule exists (schema `permission.rules[].reason`): echoed in
    /// the denial so the model and the UI can explain the refusal.
    #[serde(default)]
    pub reason: Option<String>,
    /// Rule origin (schema `permission.rules[].scope`, default `user`):
    /// `turn-override` > `session-runtime` > `project` > `user`. The host
    /// merges rules by scope before handing the engine a snapshot, so the
    /// engine only carries the value through.
    #[serde(default)]
    pub scope: Option<String>,
    /// Legacy shorthand `{ tool = "Write", match = "src/**" }` (v2
    /// `transformPermissionRule`): the tool plus an optional argument pattern.
    #[serde(default)]
    pub tool: Option<String>,
    /// The argument pattern of the legacy `{ tool, match }` shorthand.
    #[serde(default, rename = "match")]
    pub arg_match: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PermissionConfig {
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub rules: Option<Vec<PermissionRuleConfig>>,
    /// `[permission] dangerous_command_guard` (v2
    /// `isDangerousCommandGuardEnabled`): `false` skips the engine's
    /// DangerousCommandAsk policy. Unset keeps the guard on. Snake_case is the
    /// documented file spelling (config-files.md); the camelCase alias covers
    /// the `POST /config` JSON body (`patchConfigRequestSchema`).
    #[serde(
        rename = "dangerous_command_guard",
        alias = "dangerousCommandGuard",
        default
    )]
    pub dangerous_command_guard: Option<bool>,
    /// Legacy `[permission] deny = [...]` / `allow` / `ask` lists (v2
    /// `permissionFromToml`). They are folded into `rules` with the matching
    /// decision, so an old file keeps enforcing its rules instead of having
    /// them silently dropped. An entry is either a bare pattern string or a
    /// `{ tool, match }` table.
    #[serde(default)]
    pub deny: Option<toml::Value>,
    #[serde(default)]
    pub allow: Option<toml::Value>,
    #[serde(default)]
    pub ask: Option<toml::Value>,
}

/// `[workspace]` workspace settings (v2 `workspaceTrust/configSection.ts`,
/// upstream #4517): the startup trust gate. See
/// [`KimiConfig::resolve_workspace_require_trust`].
/// `[default_workspace]` (upstream #4224, `app/defaultWorkspace/configSection.ts`):
/// where a session lives when the request names no workspace and no cwd.
/// `path` is absolute (or `~/…`); a relative value is refused, never guessed.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DefaultWorkspaceConfig {
    #[serde(rename = "path", default)]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WorkspaceConfig {
    /// Snake_case is the documented file spelling (config-files.md); the
    /// camelCase alias covers the upstream schema spelling (`requireTrust`).
    #[serde(rename = "require_trust", alias = "requireTrust", default)]
    pub require_trust: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct McpServerConfig {
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Option<Vec<String>>,
    #[serde(default)]
    pub env: Option<HashMap<String, String>>,
    /// Working directory for a stdio server, resolved against the process
    /// working directory when relative (v2 `McpServerStdioConfig.cwd`).
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub headers: Option<HashMap<String, String>>,
    /// Environment variable holding a bearer token for a remote server; the
    /// value is read at connect time and sent as `Authorization: Bearer …`
    /// (v2 `bearerTokenEnvVar`, client-remote.ts:9-23).
    #[serde(default, alias = "bearerTokenEnvVar")]
    pub bearer_token_env_var: Option<String>,
    /// The server's authentication mode (v2 `config.auth`). `"oauth"` opts a
    /// remote server into the OAuth flow, which keeps a 401 flipping to
    /// `needs-auth` even when extra headers are configured. Absent applies the
    /// header-vs-credential heuristic.
    #[serde(default)]
    pub auth: Option<String>,
    /// Explicit transport (`stdio` / `sse` / `http`). Without it a `url`
    /// defaults to Streamable HTTP and a `command` to stdio, matching the v2
    /// config preprocess (config-schema.ts:58-65).
    #[serde(default)]
    pub transport: Option<String>,
    /// `false` keeps the entry listed as `disabled` without ever connecting
    /// it (v2 `config.enabled`).
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Allowlist of tool names exposed to the model; `None` exposes every
    /// tool the server advertises (v2 `enabledTools`).
    #[serde(default, alias = "enabledTools")]
    pub enabled_tools: Option<Vec<String>>,
    /// Denylist applied after the allowlist (v2 `disabledTools`).
    #[serde(default, alias = "disabledTools")]
    pub disabled_tools: Option<Vec<String>>,
    /// Startup (connect + tool discovery) timeout in milliseconds
    /// (v2 per-server `startupTimeoutMs`).
    #[serde(default, alias = "startupTimeoutMs")]
    pub startup_timeout_ms: Option<u64>,
    /// Single tool-call timeout in milliseconds (v2 per-server `toolTimeoutMs`).
    #[serde(default, alias = "toolTimeoutMs")]
    pub tool_timeout_ms: Option<u64>,
    /// How the server is executed: `local` (default) or `kaos` (v2
    /// `McpServerStdioConfig.executor`). The engine only runs local processes;
    /// a non-local executor is reported at load time instead of silently
    /// running the server on the wrong host.
    #[serde(default)]
    pub executor: Option<String>,
    /// The declared environment a stdio server belongs to (v2
    /// `McpServerStdioConfig.environment_id`). Declaring a non-local
    /// environment is reported at load time: this engine cannot route a
    /// process into another environment.
    #[serde(default, alias = "environmentId")]
    pub environment_id: Option<String>,
    /// Keep this server's tools out of the model's top-level tool list and let
    /// it load them on demand through `select_tools` (v2 per-server
    /// `deferred`). Takes effect only when the `tool_select` experimental flag
    /// is on *and* the model declares `dynamically_loaded_tools`; otherwise the
    /// field is ignored and the tools are exposed inline, which is also the
    /// default when it is absent.
    #[serde(default)]
    pub deferred: Option<bool>,
}

/// Global MCP defaults, mirroring the v2 `[mcp]` config section
/// (`app/mcpConfig/configSection.ts:9-12`). The timeout values are also
/// settable per server and through `KIMI_MCP_STARTUP_TIMEOUT_MS` /
/// `KIMI_MCP_TOOL_TIMEOUT_MS`, which win over this section; `defaultDeferred`
/// is the disclosure default for every server that does not declare its own.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct McpConfig {
    #[serde(default, alias = "startupTimeoutMs")]
    pub startup_timeout_ms: Option<u64>,
    #[serde(default, alias = "toolTimeoutMs")]
    pub tool_timeout_ms: Option<u64>,
    /// Keep every server's tools out of the top-level tool list unless the
    /// server declares `deferred` itself (v2 #4210, `defaultDeferred`).
    #[serde(default, alias = "defaultDeferred")]
    pub default_deferred: Option<bool>,
    /// Retry a failed startup automatically (v2 `reconnectRetryEnabled`, which
    /// the workspace host sets to `true`). Default `true`; `false` keeps a
    /// transient failure at `failed` until an explicit reconnect.
    #[serde(default, alias = "reconnectRetryEnabled")]
    pub reconnect_retry_enabled: Option<bool>,
    /// First retry delay in milliseconds (v2
    /// `DEFAULT_RECONNECT_INITIAL_DELAY_MS`, 5 000).
    #[serde(default, alias = "reconnectInitialDelayMs")]
    pub reconnect_initial_delay_ms: Option<u64>,
    /// Retry delay cap in milliseconds (v2 `DEFAULT_RECONNECT_MAX_DELAY_MS`,
    /// 300 000).
    #[serde(default, alias = "reconnectMaxDelayMs")]
    pub reconnect_max_delay_ms: Option<u64>,
    /// Defer a server's tools once it advertises at least this many of them
    /// (v2 `autoDeferToolThreshold`, default 24; `0` disables). An explicit
    /// per-server `deferred` still wins, and
    /// `KIMI_MCP_AUTO_DEFER_TOOL_THRESHOLD` overrides this value.
    #[serde(default, alias = "autoDeferToolThreshold")]
    pub auto_defer_tool_threshold: Option<u64>,
    /// Whether discovered MCP input schemas are compacted before the model
    /// sees them (v2 `compactToolSchemas`, default `false`).
    #[serde(default, alias = "compactToolSchemas")]
    pub compact_tool_schemas: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GitHubConfig {
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
}

/// One `[services.moonshot_search]` / `[services.moonshot_fetch]` entry (v2
/// `MoonshotServiceConfig`). Snake_case is the file contract; the camelCase
/// aliases match the spelling the host schema also accepts.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MoonshotServiceConfig {
    #[serde(rename = "base_url", alias = "baseUrl", default)]
    pub base_url: Option<String>,
    #[serde(rename = "api_key", alias = "apiKey", default)]
    pub api_key: Option<String>,
    #[serde(rename = "custom_headers", alias = "customHeaders", default)]
    pub custom_headers: Option<HashMap<String, String>>,
    /// Managed-login binding (v2 `MoonshotServiceConfig.oauth`): its presence
    /// marks the service OAuth-authenticated instead of key-authenticated.
    /// Carried so the field survives a read; the engine has no managed-login
    /// store yet, so resolution still requires a base URL.
    #[serde(default)]
    pub oauth: Option<serde_json::Value>,
}

/// The `[services]` section (v2 `configSection.ts`): optional Moonshot
/// backends that replace (search) or front (fetch) the built-in web tools,
/// plus the Bing Web Search API fallback.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ServicesConfig {
    #[serde(rename = "moonshot_search", alias = "moonshotSearch", default)]
    pub moonshot_search: Option<MoonshotServiceConfig>,
    #[serde(rename = "moonshot_fetch", alias = "moonshotFetch", default)]
    pub moonshot_fetch: Option<MoonshotServiceConfig>,
    #[serde(rename = "bing_api", alias = "bingApi", default)]
    pub bing_api: Option<MoonshotServiceConfig>,
}

/// One web-service backend after the `KIMI_WEB_*` env overlay, ready for the
/// native tool seam.
#[derive(Debug, Clone)]
pub struct ResolvedWebService {
    pub base_url: String,
    pub api_key: Option<String>,
    pub custom_headers: HashMap<String, String>,
}

/// The `[image]` section (v2 `agent/media/configSection.ts`): the limits
/// applied when the model reads an image for itself.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ImageConfig {
    #[serde(rename = "read_byte_budget", alias = "readByteBudget", default)]
    pub read_byte_budget: Option<u64>,
    #[serde(rename = "max_edge_px", alias = "maxEdgePx", default)]
    pub max_edge_px: Option<u32>,
}

/// The `[read]` section (v2 `agent/tools/os/read/configSection.ts`): the
/// character budget the native Read tool applies. Both keys are
/// positive-only; a non-positive value is a load-time warning and is ignored,
/// so the built-in limits stand instead of the tool silently reading nothing.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReadConfig {
    /// Characters returned when the call does not name a budget.
    #[serde(rename = "default_max_chars", alias = "defaultMaxChars", default)]
    pub default_max_chars: Option<u64>,
    /// Hard ceiling a call may request.
    #[serde(rename = "max_chars", alias = "maxChars", default)]
    pub max_chars: Option<u64>,
}

/// The `[llm_requester]` section (v2 `agent/llmRequester/configSection.ts`):
/// the serialized request-size budget. v2 snake-cases the domain, so
/// `[llm_requester]` is the file spelling and `[llmRequester]` the alias.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LlmRequesterConfig {
    /// Request byte budget (v2 `requestByteBudget`, default 32 MiB).
    #[serde(rename = "request_byte_budget", alias = "requestByteBudget", default)]
    pub request_byte_budget: Option<u64>,
}

/// The `[sandbox]` section (v2 `workspace/sandbox/sandbox.ts`): the file
/// effect mode the pipeline confines mutating tools to.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SandboxConfig {
    /// `off` (default) | `read-only` | `workspace-write`. An unknown spelling
    /// is a load-time warning and resolves to `None` (no sandbox), matching
    /// v2's section-level schema rejection; it never silently downgrades to a
    /// weaker mode the user did not write.
    #[serde(default)]
    pub mode: Option<String>,
}

/// The `[model_overrides]` section (v2 `ModelOverridesSchema`): request
/// parameters that apply to every model unless the model entry overrides them.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelOverridesConfig {
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(rename = "top_p", alias = "topP", default)]
    pub top_p: Option<f64>,
    /// Preserved-thinking passthrough override (v2 `thinkingKeep`); ahead of
    /// `[thinking].keep`.
    #[serde(rename = "thinking_keep", alias = "thinkingKeep", default)]
    pub thinking_keep: Option<String>,
    /// Output-token cap override (v2 `maxCompletionTokens`).
    #[serde(
        rename = "max_completion_tokens",
        alias = "maxCompletionTokens",
        default
    )]
    pub max_completion_tokens: Option<u32>,
}

/// The effective `[model_overrides]` values after the environment overlay
/// (v2 `resolveModelOverrides`): what the request builder should apply to a
/// model that does not declare its own.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResolvedModelOverrides {
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub thinking_keep: Option<String>,
    pub max_completion_tokens: Option<u32>,
}
/// v2 `CronConfig` (`features/cron/configSection.ts:5-20`). `debug` and
/// `manualTick` are test-only upstream switches with no engine consumer
/// here, so they are accepted and ignored rather than rejected as
/// unknown keys.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CronConfig {
    /// Kill switch: the daemon tick loop publishes nothing when set.
    /// Env `KIMI_DISABLE_CRON` wins over the file value (v2
    /// `cronEnvBindings.disabled`).
    #[serde(default)]
    pub disabled: Option<bool>,
    /// Suppress the anti-herd jitter offset (v2 `noJitter`).
    #[serde(rename = "no_jitter", alias = "noJitter", default)]
    pub no_jitter: Option<bool>,
    /// Suppress the 7-day stale-recurring prune (v2 `noStale`).
    #[serde(rename = "no_stale", alias = "noStale", default)]
    pub no_stale: Option<bool>,
}

/// The `[subagent]` section (v2 `session/subagent/configSection.ts`): the
/// timeout one `Agent` subagent turn may run for, foreground and background.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SubagentConfig {
    #[serde(rename = "timeout_ms", alias = "timeoutMs", default)]
    pub timeout_ms: Option<u64>,
}

/// The `[swarm]` section (v2 `features/swarm/configSection.ts`): the timeout
/// for one `AgentSwarm` subagent, independent of `[subagent]`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SwarmConfig {
    #[serde(rename = "timeout_ms", alias = "timeoutMs", default)]
    pub timeout_ms: Option<u64>,
}

/// The `[secondary_model]` subagent model pool section (v2
/// `session/subagent/configSection.ts`): either a declared pool
/// (`default_model` + the `models` alias table) or the recipe shape
/// (`model` pointing at a `[models]` entry, a single-entry pool).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SecondaryModelConfig {
    #[serde(rename = "default_model", default)]
    pub default_model: Option<String>,
    /// `[secondary_model.models]`: alias → hint shown in the tool description.
    #[serde(default)]
    pub models: Option<HashMap<String, String>>,
    #[serde(default)]
    pub force: Option<bool>,
    /// Recipe pointer to a `[models]` entry; the single-entry pool default.
    #[serde(default)]
    pub model: Option<String>,
    /// Subagent thinking effort override (v2 `default_effort`).
    #[serde(rename = "default_effort", default)]
    pub default_effort: Option<String>,
}

/// The `[loop_control]` section (v2 `loopControl`): per-step limits the host
/// threads into the turn loop. The deprecated spellings
/// (`max_retries_per_step`, `max_steps_per_run`) still resolve when the
/// current key is unset.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LoopControlConfig {
    #[serde(rename = "max_steps_per_turn", default)]
    pub max_steps_per_turn: Option<u32>,
    #[serde(rename = "max_steps_per_run", default)]
    pub max_steps_per_run: Option<u32>,
    #[serde(rename = "max_attempts_per_step", default)]
    pub max_attempts_per_step: Option<u32>,
    #[serde(rename = "max_retries_per_step", default)]
    pub max_retries_per_step: Option<u32>,
    /// Total requests one compaction round may issue (v2 #3750
    /// `loopControl.compactionMaxAttempts`); see
    /// [`KimiConfig::resolve_compaction_max_attempts`].
    #[serde(
        rename = "compaction_max_attempts",
        alias = "compactionMaxAttempts",
        default
    )]
    pub compaction_max_attempts: Option<u32>,
    /// Headroom to keep free below the context window (v2
    /// `loopControl.reservedContextSize`, min 0). Compaction also triggers
    /// when `used + reserved >= window` — but only once this key is wired to
    /// [`crate::compaction::CompactionConfig`]; today the engine reads no
    /// value from here (accepted for config parity, not yet consumed), and
    /// `reserved_context_size` stays at the 50k engine default.
    #[serde(
        rename = "reserved_context_size",
        alias = "reservedContextSize",
        default
    )]
    pub reserved_context_size: Option<u32>,
    /// Ralph-loop iteration cap (v2 `loopControl.maxRalphIterations`, min
    /// -1; `-1` means unbounded). The engine has no Ralph loop, so this is
    /// accepted for config parity and not consumed.
    #[serde(rename = "max_ralph_iterations", alias = "maxRalphIterations", default)]
    pub max_ralph_iterations: Option<i64>,
    /// Fraction of the window that triggers compaction (v2
    /// `loopControl.compactionTriggerRatio`, 0.5..=0.99). Same gap as
    /// [`LoopControlConfig::reserved_context_size`]: parsed and validated, but
    /// not yet consumed, so `trigger_ratio` stays at the 0.85 engine default.
    #[serde(
        rename = "compaction_trigger_ratio",
        alias = "compactionTriggerRatio",
        default
    )]
    pub compaction_trigger_ratio: Option<f64>,
}

/// The `[thinking]` section (v2 `thinking`): the enable switch and the
/// preserved-thinking passthrough (`keep`) it gates.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ThinkingConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub keep: Option<String>,
    /// The user's chosen reasoning effort (schema `thinking.effort`), sent as
    /// `reasoning_effort` on providers that support it.
    #[serde(default)]
    pub effort: Option<String>,
    /// Env-forced reasoning effort (schema `thinking.forcedEffort`, bound to
    /// `KIMI_MODEL_THINKING_EFFORT`): wins over `effort` while thinking is on.
    /// v2 only lets a trait-driven provider take it; the engine has no
    /// trait gate here, so an explicit value is honored for every provider
    /// and surfaced by [`KimiConfig::resolve_effort`].
    #[serde(rename = "forced_effort", alias = "forcedEffort", default)]
    pub forced_effort: Option<String>,
}

/// One `[experimental]` flag value (schema `experimental`): flags are free
/// booleans or strings, so the engine carries them verbatim and only the
/// features that read them interpret the value.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ExperimentalValue {
    Bool(bool),
    String(String),
}

/// The `[background]` section (schema `background`): the knobs governing
/// background task execution — how many may run at once, how long a shell
/// task may live, and how it is torn down.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BackgroundConfig {
    /// Concurrent running background tasks; further spawns are refused.
    /// This is the legacy location of the shared task cap; `[task]` wins when
    /// both sections configure it.
    #[serde(rename = "max_running_tasks", alias = "maxRunningTasks", default)]
    pub max_running_tasks: Option<u32>,
    /// When a foreground Bash command times out, move it to the background
    /// instead of killing it. Defaults to `true` when unset.
    #[serde(
        rename = "bash_auto_background_on_timeout",
        alias = "bashAutoBackgroundOnTimeout",
        default
    )]
    pub bash_auto_background_on_timeout: Option<bool>,
    /// Default timeout (seconds) for background Bash tasks; `0` means no
    /// timeout. Defaults to the tool's built-in 600s when unset.
    #[serde(rename = "bash_task_timeout_s", alias = "bashTaskTimeoutS", default)]
    pub bash_task_timeout_s: Option<u64>,
    /// How long `stop` waits for a cooperative exit before the task is
    /// considered killed. Defaults to 5s when unset.
    #[serde(rename = "kill_grace_period_ms", alias = "killGracePeriodMs", default)]
    pub kill_grace_period_ms: Option<u64>,
    /// `keepAliveOnExit`: leave background tasks running when the session
    /// closes (v2). Defaults to false.
    #[serde(rename = "keep_alive_on_exit", alias = "keepAliveOnExit", default)]
    pub keep_alive_on_exit: Option<bool>,
    /// Settled tasks retained for inspection (v2 `maxSettledTasks`, default
    /// 10). The engine keeps settled records in memory without a cap, so this
    /// is accepted for config parity and not consumed yet.
    #[serde(rename = "max_settled_tasks", alias = "maxSettledTasks", default)]
    pub max_settled_tasks: Option<u32>,
    /// `--print` mode's wait ceiling in seconds (v2 `printWaitCeilingS`,
    /// default 3600). `--print` is an app-level path here, so the engine
    /// carries the value without consuming it.
    #[serde(rename = "print_wait_ceiling_s", alias = "printWaitCeilingS", default)]
    pub print_wait_ceiling_s: Option<u64>,
    /// `--print` background dispatch mode (v2 `printBackgroundMode`:
    /// `exit` | `drain` | `steer`). App-level, carried without being consumed.
    #[serde(
        rename = "print_background_mode",
        alias = "printBackgroundMode",
        default
    )]
    pub print_background_mode: Option<String>,
    /// `--print` turn cap (v2 `printMaxTurns`, min 1). App-level, carried
    /// without being consumed.
    #[serde(rename = "print_max_turns", alias = "printMaxTurns", default)]
    pub print_max_turns: Option<u32>,
}

/// The `[task]` section (v2 `agent/task/configSection.ts`). It carries the
/// same schema as `[background]` and its values override the legacy keys
/// while task configuration is being renamed.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TaskConfig {
    #[serde(rename = "max_running_tasks", alias = "maxRunningTasks", default)]
    pub max_running_tasks: Option<u32>,
    #[serde(rename = "max_settled_tasks", alias = "maxSettledTasks", default)]
    pub max_settled_tasks: Option<u32>,
    #[serde(
        rename = "bash_auto_background_on_timeout",
        alias = "bashAutoBackgroundOnTimeout",
        default
    )]
    pub bash_auto_background_on_timeout: Option<bool>,
    #[serde(rename = "bash_task_timeout_s", alias = "bashTaskTimeoutS", default)]
    pub bash_task_timeout_s: Option<u64>,
    #[serde(rename = "kill_grace_period_ms", alias = "killGracePeriodMs", default)]
    pub kill_grace_period_ms: Option<u64>,
    #[serde(rename = "keep_alive_on_exit", alias = "keepAliveOnExit", default)]
    pub keep_alive_on_exit: Option<bool>,
    #[serde(rename = "print_wait_ceiling_s", alias = "printWaitCeilingS", default)]
    pub print_wait_ceiling_s: Option<u64>,
    #[serde(
        rename = "print_background_mode",
        alias = "printBackgroundMode",
        default
    )]
    pub print_background_mode: Option<String>,
    #[serde(rename = "print_max_turns", alias = "printMaxTurns", default)]
    pub print_max_turns: Option<u32>,
}

/// The global `[tools]` switch (schema `tools.enabled` / `tools.disabled`):
/// an allowlist applied first, then a denylist, intersected with every
/// agent's own tool policy.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolsConfig {
    #[serde(default)]
    pub enabled: Vec<String>,
    #[serde(default)]
    pub disabled: Vec<String>,
}

/// The `[tokenCounting]` section (v2
/// `agent/tokenCounting/configSection.ts`): how the context gauge is derived.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TokenCountingConfig {
    /// `measured+estimated` (default), `measured` or `estimated`. Kept as the
    /// raw string here so an invalid value can be reported by name instead of
    /// failing the whole section; [`KimiConfig::resolve_token_counting_strategy`]
    /// parses it.
    #[serde(default)]
    pub strategy: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct KimiConfig {
    #[serde(rename = "default_model", default)]
    pub default_model: Option<String>,
    /// Global default provider pointer (v2 `default_provider`): the fallback
    /// owner for model aliases that do not name a provider.
    #[serde(rename = "default_provider", default)]
    pub default_provider: Option<String>,
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
    #[serde(default)]
    pub models: HashMap<String, ModelAliasConfig>,
    #[serde(default)]
    pub agent: AgentConfig,
    /// `[shell]` preference for the local command shell (v2 `configSection.ts`).
    #[serde(default)]
    pub shell: ShellConfig,
    /// Turn-loop limits (v2 `[loop_control]` section); see
    /// [`KimiConfig::resolve_max_attempts_per_step`].
    #[serde(rename = "loop_control", default)]
    pub loop_control: LoopControlConfig,
    /// Thinking configuration (v2 `[thinking]` section); see
    /// [`KimiConfig::resolve_thinking_keep`].
    #[serde(default)]
    pub thinking: ThinkingConfig,
    /// Context-gauge strategy (v2 `[tokenCounting]` section); see
    /// [`KimiConfig::resolve_token_counting_strategy`].
    #[serde(rename = "tokenCounting", alias = "token_counting", default)]
    pub token_counting: TokenCountingConfig,
    #[serde(default)]
    pub permission: Option<PermissionConfig>,
    /// `[workspace]` settings (v2 `workspaceTrust/configSection.ts`); see
    /// [`KimiConfig::resolve_workspace_require_trust`].
    #[serde(default)]
    pub workspace: WorkspaceConfig,
    /// `[default_workspace]` (upstream #4224 `app/defaultWorkspace`): where a
    /// session lives when the request names no workspace and no cwd. See
    /// [`crate::default_workspace`].
    #[serde(rename = "default_workspace", alias = "defaultWorkspace", default)]
    pub default_workspace: DefaultWorkspaceConfig,
    /// `[environments]` declarations (v2 `remote/declaration/configSection.ts`):
    /// the default id plus every declared remote transport. See
    /// [`crate::environment::EnvironmentDeclaration`]; this engine serves the
    /// built-in local environment and refuses a declared transport explicitly.
    #[serde(default)]
    pub environments: EnvironmentsConfig,
    #[serde(rename = "mcp_servers", default)]
    pub mcp_servers: HashMap<String, McpServerConfig>,
    /// Global MCP defaults (v2 `[mcp]` section).
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub github: GitHubConfig,
    /// `[services]` web-service backends (v2 `configSection.ts`); see
    /// [`KimiConfig::resolve_web_search_service`].
    #[serde(default)]
    pub services: ServicesConfig,
    /// Per-subagent timeout (v2 `[subagent]` section); see
    /// [`KimiConfig::resolve_subagent_timeout_ms`].
    #[serde(default)]
    pub subagent: SubagentConfig,
    /// Per-swarm timeout (v2 `[swarm]` section); see
    /// [`KimiConfig::resolve_swarm_timeout_ms`].
    #[serde(default)]
    pub swarm: SwarmConfig,
    /// Model-initiated image-read limits (v2 `[image]` section); see
    /// [`KimiConfig::resolve_image_read_byte_budget`].
    #[serde(default)]
    pub image: ImageConfig,
    /// Native Read tool character budget (v2 `[read]` section); see
    /// [`KimiConfig::resolve_read_limits`].
    #[serde(default)]
    pub read: ReadConfig,
    /// Serialized request-size budget (v2 `[llm_requester]` section); see
    /// [`KimiConfig::resolve_llm_request_byte_budget`].
    #[serde(rename = "llm_requester", alias = "llmRequester", default)]
    pub llm_requester: LlmRequesterConfig,
    /// File-effect sandbox mode (v2 `[sandbox]` section); see
    /// [`KimiConfig::resolve_sandbox_mode`].
    #[serde(default)]
    pub sandbox: SandboxConfig,
    /// Global request-parameter overrides (v2 `[model_overrides]` section);
    /// see [`KimiConfig::resolve_model_overrides`].
    #[serde(rename = "model_overrides", alias = "modelOverrides", default)]
    pub model_overrides: ModelOverridesConfig,
    /// Whether the built-in product skills are installed (v2
    /// `[builtinProductSkills]`, bound to `KIMI_CODE_BUILTIN_PRODUCT_SKILLS`);
    /// see [`KimiConfig::resolve_builtin_product_skills`].
    #[serde(
        rename = "builtin_product_skills",
        alias = "builtinProductSkills",
        default
    )]
    pub builtin_product_skills: Option<bool>,
    /// Cron scheduler switches (v2 `[cron]` section, `features/cron/configSection.ts`);
    /// see [`KimiConfig::cron_disabled`] and [`KimiConfig::cron_no_jitter`].
    #[serde(default)]
    pub cron: CronConfig,
    /// Subagent model pool (v2 `[secondary_model]` section); see
    /// [`KimiConfig::extract_secondary_model_pool`].
    #[serde(rename = "secondary_model", default)]
    pub secondary_model: Option<SecondaryModelConfig>,
    /// User-configured external hooks (v2 `[hooks]` section). The engine
    /// executes the `PreToolUse` ones before native tool calls (G-6 #6).
    #[serde(default)]
    pub hooks: Vec<HookDef>,
    /// Global tool switch (schema `[tools]`): applied to every agent in all
    /// sessions. The host-driven paths get it through their own snapshot; the
    /// file-reading entries (REPL / `--serve` / `--acp`) build it here.
    #[serde(default)]
    pub tools: ToolsConfig,
    /// Background task knobs (legacy `[background]` location).
    #[serde(default)]
    pub background: BackgroundConfig,
    /// Current task knobs; individual values override `[background]`.
    #[serde(default)]
    pub task: TaskConfig,
    /// Free-form feature flags (schema `[experimental]`).
    #[serde(default)]
    pub experimental: HashMap<String, ExperimentalValue>,
    /// The default permission mode when `[permission].mode` is unset (schema
    /// `default_permission_mode`); `[agent].yolo` still wins.
    #[serde(
        rename = "default_permission_mode",
        alias = "defaultPermissionMode",
        default
    )]
    pub default_permission_mode: Option<String>,
    /// Top-level `yolo` (schema): the v1 flat form of `[agent].yolo`.
    #[serde(default)]
    pub yolo: Option<bool>,
    /// Top-level `plan_mode` / `default_plan_mode` (schema): the initial plan
    /// mode for a new session. `plan_mode` is the v1 flat name; the sectioned
    /// `[agent].plan_mode` is preferred when both are present.
    #[serde(rename = "plan_mode", alias = "planMode", default)]
    pub plan_mode: Option<bool>,
    #[serde(rename = "default_plan_mode", alias = "defaultPlanMode", default)]
    pub default_plan_mode: Option<bool>,
    /// `[model_catalog]` automatic provider-model refresh (schema).
    #[serde(rename = "model_catalog", alias = "modelCatalog", default)]
    pub model_catalog: Option<ModelCatalogConfig>,
    /// Top-level telemetry switch (schema `telemetry`).
    #[serde(default)]
    pub telemetry: Option<bool>,
    /// Whether clients may automatically generate session titles (schema
    /// `auto_session_title`, upstream #3962). `None`/absent means enabled —
    /// only an explicit `false` disables it, matching upstream's
    /// "disabled only when explicitly set to false" semantics. The engine
    /// generates titles only when the host asks (`session/generate_title`),
    /// so this is the host's gate, carried on the config surface.
    #[serde(rename = "auto_session_title", alias = "autoSessionTitle", default)]
    pub auto_session_title: Option<bool>,
    /// Extra directories scanned for skills (schema `extra_skill_dirs`) and
    /// for agent definitions (schema `extra_agent_dirs`), plus the
    /// "merge every discovered skill" switch.
    #[serde(rename = "extra_skill_dirs", alias = "extraSkillDirs", default)]
    pub extra_skill_dirs: Vec<String>,
    #[serde(rename = "extra_agent_dirs", alias = "extraAgentDirs", default)]
    pub extra_agent_dirs: Vec<String>,
    #[serde(
        rename = "merge_all_available_skills",
        alias = "mergeAllAvailableSkills",
        default
    )]
    pub merge_all_available_skills: Option<bool>,
    /// `[models]` entries that cannot resolve, collected by
    /// [`KimiConfig::from_file`] on the real load path (v2 #3681). Not a
    /// config-file key: the server stages them and broadcasts
    /// `event.config.warning` once the global lane is live, which is what
    /// gives the v3 `config.warning` entity its producer. Empty for a config
    /// built by parsing alone ([`std::str::FromStr`] cannot see the raw TOML
    /// an entry's shape lives in).
    #[serde(skip)]
    pub config_warnings: Vec<String>,
    /// Top-level keys serde did not recognize (v2 `Ignored invalid config
    /// section`). Kept only so [`KimiConfig::from_file`] can report them; they
    /// are never written back.
    #[serde(flatten, skip_serializing, default)]
    pub(crate) unknown_sections: HashMap<String, toml::Value>,
    /// The known sections [`KimiConfig::from_file`] had to drop because they
    /// failed their schema (v2 `buildValidated`). Typed rather than parsed back
    /// out of [`Self::config_warnings`]: a caller deciding whether a reload may
    /// keep its previous `[hooks]` must be able to ask "was `hooks` dropped?"
    /// without matching human-readable diagnostic text.
    #[serde(skip)]
    pub(crate) dropped_sections: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedNativeLlm {
    pub protocol: String,
    /// The `[models.<alias>]` key (or declared alias name) this entry was
    /// resolved for — v2 `ApiErrorEvent.alias`. Carried so telemetry can name
    /// the model the user selected rather than the wire model behind it.
    #[serde(default)]
    pub alias: Option<String>,
    /// Declared provider family (`[providers.*].type`), lowercased. The
    /// transport uses it for provider-specific wire encodings that the
    /// protocol alone cannot express (the Kimi `thinking` object).
    #[serde(default)]
    pub provider_type: String,
    pub base_url: String,
    pub api_key: String,
    /// Name of the environment variable the credential is read from at
    /// request time (`[providers.*].api_key_env`); passed through to the
    /// transport's `api_key_env` channel. `None` when the provider carries a
    /// static key or an OAuth binding.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// OAuth-managed auth: the provider name the transport asks for a bearer
    /// token instead of using `api_key`. `None` when the provider carries a
    /// static key.
    #[serde(default)]
    pub auth_provider: Option<String>,
    pub model: String,
    pub max_tokens: Option<u32>,
    /// The alias's declared capabilities (`[models.<alias>].capabilities`),
    /// when the file declares them. The image-read gate refuses only a
    /// declared set that lacks `image_in`; `None`/empty stays unknown.
    #[serde(default)]
    pub capabilities: Option<Vec<String>>,
    /// Extra HTTP headers from `[providers.*].custom_headers`.
    #[serde(default)]
    pub custom_headers: HashMap<String, String>,
    /// Route an anthropic-protocol model through the beta Messages API
    /// (`[models.<alias>].beta_api`).
    #[serde(default)]
    pub beta_api: bool,
    /// The alias's own system prompt (`[models.<alias>].system_prompt`).
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// Declared output cap (`[models.<alias>].max_output_size`).
    #[serde(default)]
    pub max_output_size: Option<u32>,
    /// Declared input cap when below the window
    /// (`[models.<alias>].max_input_size`).
    #[serde(default)]
    pub max_input_size: Option<u32>,
    /// Explicit adaptive-thinking support, overriding the model-name version
    /// inference (`[models.<alias>].adaptive_thinking`).
    #[serde(default)]
    pub adaptive_thinking: Option<bool>,
    /// The wire field carrying reasoning content
    /// (`[models.<alias>].reasoning_key`).
    #[serde(default)]
    pub reasoning_key: Option<String>,
    /// The effort value that encodes "thinking off" on the wire
    /// (`[models.<alias>].off_effort`).
    #[serde(default)]
    pub off_effort: Option<String>,
    /// The antigravity bridge's `agy` binary (`[providers.*].binaryPath`).
    ///
    /// `pub(crate)`, unlike the fields above: the host's `JsNativeLlmConfig`
    /// never carries subprocess knobs (the engine reads them from the config
    /// file only), so they sit outside the host-facing transport contract that
    /// `check:parity` compares field-for-field.
    #[serde(default)]
    pub(crate) binary_path: Option<String>,
    /// Bounded waits for the antigravity subprocess bridge
    /// (`[providers.*.antigravity]`). Same visibility rationale as
    /// [`Self::binary_path`].
    #[serde(default)]
    pub(crate) antigravity: Option<crate::rpc::types::AntigravityLimits>,
}

impl ResolvedNativeLlm {
    /// The subprocess-bridge knobs (`binaryPath` and
    /// `[providers.*.antigravity]`) of a resolved provider.
    ///
    /// Read through a method rather than a public field so the host-facing
    /// transport contract (`check:parity` against `JsNativeLlmConfig`) keeps
    /// describing exactly the HTTP fields both sides share, while entry points
    /// in the binary crate can still hand these to `NativeLlmConfig`.
    pub fn subprocess_bridge(
        &self,
    ) -> (Option<String>, Option<crate::rpc::types::AntigravityLimits>) {
        (self.binary_path.clone(), self.antigravity.clone())
    }
}

impl std::str::FromStr for KimiConfig {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        toml::from_str(s).map_err(|e| format!("Invalid TOML configuration: {e}"))
    }
}

impl KimiConfig {
    /// The context-gauge strategy (v2
    /// `SessionTokenCountingService.strategy`): the env binding wins, then
    /// `[tokenCounting].strategy`, then the historical `measured+estimated`.
    ///
    /// An invalid value is never silently swapped: the env spelling is warned
    /// about once and the resolver falls through to the file value; an invalid
    /// file value is warned about once and, at load time, also staged in
    /// [`Self::config_warnings`] so the server broadcasts
    /// `event.config.warning`.
    pub fn resolve_token_counting_strategy(
        &self,
    ) -> crate::session::token_counting::TokenCountingStrategy {
        use crate::session::token_counting::TokenCountingStrategy;
        if let Some(raw) = std::env::var(TokenCountingStrategy::ENV)
            .ok()
            .filter(|value| !value.trim().is_empty())
        {
            match TokenCountingStrategy::parse(&raw) {
                Some(strategy) => return strategy,
                None => warn_invalid_token_counting_strategy(TokenCountingStrategy::ENV, &raw),
            }
        }
        let Some(raw) = self
            .token_counting
            .strategy
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return TokenCountingStrategy::default();
        };
        match TokenCountingStrategy::parse(raw) {
            Some(strategy) => strategy,
            None => {
                warn_invalid_token_counting_strategy("[tokenCounting].strategy", raw);
                TokenCountingStrategy::default()
            }
        }
    }

    /// The load-time diagnostic for an invalid `[tokenCounting].strategy`
    /// (staged on [`Self::config_warnings`] by [`Self::from_file`]).
    fn token_counting_warnings(&self) -> Vec<String> {
        use crate::session::token_counting::TokenCountingStrategy;
        let Some(raw) = self
            .token_counting
            .strategy
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return Vec::new();
        };
        if TokenCountingStrategy::parse(raw).is_some() {
            return Vec::new();
        }
        vec![format!(
            "[tokenCounting].strategy '{raw}' is invalid; expected one of {}. Using 'measured+estimated'.",
            TokenCountingStrategy::VALUES.join(", ")
        )]
    }

    /// Load-time diagnostics for sections whose values parse but fall outside
    /// their schema (v2 rejects the whole section and falls back to defaults).
    /// Reporting them keeps an ignored value from looking applied.
    fn section_value_warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        for (key, value) in [
            ("default_max_chars", self.read.default_max_chars),
            ("max_chars", self.read.max_chars),
        ] {
            if value == Some(0) {
                warnings.push(format!(
                    "[read].{key} must be a positive integer; the value is ignored and the built-in limit applies."
                ));
            }
        }
        if self.llm_requester.request_byte_budget == Some(0) {
            warnings.push(
                "[llmRequester].requestByteBudget must be a positive integer; the value is ignored and the 32 MiB default applies."
                    .to_string(),
            );
        }
        if let Some(mode) = self.sandbox.mode.as_deref() {
            if let Err(error) = crate::tools::sandbox::SandboxMode::validate(mode) {
                warnings.push(format!(
                    "[sandbox].mode is invalid: {error}. The sandbox stays off."
                ));
            }
        }
        if let Some(iterations) = self.loop_control.max_ralph_iterations {
            if iterations < -1 {
                warnings.push(format!(
                    "[loopControl].maxRalphIterations {iterations} is below -1; the value is ignored."
                ));
            }
        }
        if let Some(ratio) = self.loop_control.compaction_trigger_ratio {
            if !ratio.is_finite() || !(0.5..=0.99).contains(&ratio) {
                warnings.push(format!(
                    "[loopControl].compactionTriggerRatio {ratio} is outside 0.5..=0.99; the value is ignored."
                ));
            }
        }
        if self.loop_control.compaction_max_attempts == Some(0) {
            warnings.push(
                "[loopControl].compactionMaxAttempts must be a positive integer; the value is ignored."
                    .to_string(),
            );
        }
        for (section, key, value) in [
            ("task", "max_running_tasks", self.task.max_running_tasks),
            (
                "background",
                "max_running_tasks",
                self.background.max_running_tasks,
            ),
            ("task", "max_settled_tasks", self.task.max_settled_tasks),
            (
                "background",
                "max_settled_tasks",
                self.background.max_settled_tasks,
            ),
        ] {
            if value == Some(0) {
                warnings.push(format!(
                    "[{section}].{key} must be a positive integer; the value is ignored."
                ));
            }
        }
        for (section, value) in [
            ("subagent", self.subagent.timeout_ms),
            ("swarm", self.swarm.timeout_ms),
        ] {
            if value == Some(0) {
                warnings.push(format!(
                    "[{section}].timeoutMs must be a positive integer; the value is ignored."
                ));
            }
        }
        for (key, value) in [
            ("startup_timeout_ms", self.mcp.startup_timeout_ms),
            ("tool_timeout_ms", self.mcp.tool_timeout_ms),
        ] {
            if let Some(value) = value {
                if !(1..=MAX_MCP_TIMEOUT_MS).contains(&value) {
                    warnings.push(format!(
                        "[mcp].{key} {value} is outside 1..={MAX_MCP_TIMEOUT_MS}; the value is ignored."
                    ));
                }
            }
        }
        if self.model_overrides.max_completion_tokens == Some(0) {
            warnings.push(
                "[modelOverrides].maxCompletionTokens must be a positive integer; the value is ignored."
                    .to_string(),
            );
        }
        for (id, server) in &self.mcp_servers {
            if let Some(environment) = server.environment_id.as_deref() {
                if environment != crate::environment::LOCAL_ENVIRONMENT_ID {
                    warnings.push(format!(
                        "[mcp_servers.{id}].environment_id '{environment}' cannot be reached by this engine; the server will not be started."
                    ));
                }
            }
        }
        warnings
    }

    /// The load-time diagnostic for an invalid `[environments]` declaration set
    /// (staged on [`Self::config_warnings`] by [`Self::from_file`]).
    ///
    /// v2 validates the section while the config is read (`configSection.ts`
    /// through `buildValidated`): a bad declaration set yields a warning
    /// diagnostic at startup, and only a consumer that needs the section throws
    /// `config.invalid`. The same split is kept here — the warning names the
    /// problem now, and the declaration stays parsed so
    /// `freeze_session_environment` still reports
    /// `environment.declaration_invalid` with a typed code, which is the
    /// backstop for a file edited while the server runs.
    fn environment_warnings(&self) -> Vec<String> {
        match self.environments.validate() {
            Ok(_) => Vec::new(),
            Err(error) => vec![format!(
                "Invalid config section 'environments': {}; new sessions will fail until it is fixed",
                error.detail()
            )],
        }
    }

    /// Load configuration from a specific file path.
    ///
    /// The malformed-`[models]`-entry warnings are both logged and staged on
    /// the returned config ([`KimiConfig::config_warnings`]): logging alone
    /// left `event.config.warning` with no producer anywhere in the engine.
    /// Config load happens before the server exists, so the list has to travel
    /// with the config until the server picks it up and broadcasts it
    /// (`server/mod.rs` `publish_config_warnings`).
    pub fn from_file(path: &Path) -> Result<Self, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read config at {}: {e}", path.display()))?;
        let (validated, mut warnings, dropped) = Self::validated_toml(&content)?;
        let mut config: Self = validated.parse()?;
        config.dropped_sections = dropped;
        // v2 `buildValidated`: a section with no schema is reported and
        // dropped instead of disappearing silently.
        warnings.extend(
            config
                .unknown_sections
                .keys()
                .map(|key| format!("Ignored invalid config section '{key}': unknown section")),
        );
        warnings.extend(Self::malformed_model_entries(&content));
        warnings.extend(config.token_counting_warnings());
        warnings.extend(config.section_value_warnings());
        warnings.extend(config.environment_warnings());
        config.config_warnings = warnings;
        for warning in &config.config_warnings {
            tracing::warn!(path = %path.display(), "{warning}");
        }
        Ok(config)
    }

    /// v2 `kimiModelEnvOverlay` (`app/kosongConfig/envOverlay.ts`): deployment
    /// by environment. `KIMI_MODEL_API_KEY` / `KIMI_MODEL_PROVIDER_TYPE` /
    /// `KIMI_MODEL_BASE_URL` bind the reserved `__kimi_env__` provider, and
    /// `KIMI_MODEL_NAME` adds the reserved `__kimi_env_model__` alias, points
    /// `default_model` at it and fills the catalog fields. A malformed numeric
    /// or boolean variable fails the load (v2 raises `CONFIG_INVALID`), so an
    /// env-only deployment never runs with half its settings applied.
    fn apply_model_env_overlay(&mut self) -> Result<(), String> {
        let provider_type = env_non_empty("KIMI_MODEL_PROVIDER_TYPE");
        let api_key = env_non_empty("KIMI_MODEL_API_KEY");
        let base_url = env_non_empty("KIMI_MODEL_BASE_URL");
        let model = env_non_empty("KIMI_MODEL_NAME");

        if provider_type.is_some() || api_key.is_some() || base_url.is_some() {
            let provider = self
                .providers
                .entry(ENV_MODEL_PROVIDER_KEY.to_string())
                .or_default();
            if let Some(value) = provider_type {
                provider.provider_type = Some(value);
            }
            if let Some(value) = base_url {
                provider.base_url = Some(value);
            }
            if let Some(value) = api_key {
                provider.api_key = Some(value);
                provider.api_key_env = None;
            }
        }

        let Some(model) = model else {
            return Ok(());
        };
        let max_context_size = match env_non_empty("KIMI_MODEL_MAX_CONTEXT_SIZE") {
            Some(raw) => parse_positive_u32(&raw, "KIMI_MODEL_MAX_CONTEXT_SIZE")?,
            None => ENV_MODEL_DEFAULT_MAX_CONTEXT_SIZE,
        };
        let max_output_size = env_non_empty("KIMI_MODEL_MAX_OUTPUT_SIZE")
            .map(|raw| parse_positive_u32(&raw, "KIMI_MODEL_MAX_OUTPUT_SIZE"))
            .transpose()?;
        let capabilities = match env_non_empty("KIMI_MODEL_CAPABILITIES") {
            Some(raw) => {
                let parsed: Vec<String> = raw
                    .split(',')
                    .map(|capability| capability.trim().to_ascii_lowercase())
                    .filter(|capability| !capability.is_empty())
                    .collect();
                if parsed.is_empty() {
                    ENV_MODEL_DEFAULT_CAPABILITIES
                        .iter()
                        .map(|capability| capability.to_string())
                        .collect()
                } else {
                    parsed
                }
            }
            None => ENV_MODEL_DEFAULT_CAPABILITIES
                .iter()
                .map(|capability| capability.to_string())
                .collect(),
        };
        let adaptive_thinking = match env_non_empty("KIMI_MODEL_ADAPTIVE_THINKING") {
            Some(raw) => Some(crate::env::parse_bool_env(Some(&raw)).ok_or_else(|| {
                format!(
                    "KIMI_MODEL_ADAPTIVE_THINKING must be a boolean (true/false/1/0/yes/no/on/off), got \"{raw}\"."
                )
            })?),
            None => None,
        };

        // A file that already declares the reserved provider keeps its own
        // type; the overlay only fills a missing one (v2 defaults to `kimi`).
        self.providers
            .entry(ENV_MODEL_PROVIDER_KEY.to_string())
            .or_default()
            .provider_type
            .get_or_insert_with(|| "kimi".to_string());

        self.models.insert(
            ENV_MODEL_ALIAS_KEY.to_string(),
            ModelAliasConfig {
                provider: Some(ENV_MODEL_PROVIDER_KEY.to_string()),
                model: Some(model),
                max_context_size: Some(max_context_size),
                max_output_size,
                capabilities: Some(capabilities),
                display_name: env_non_empty("KIMI_MODEL_DISPLAY_NAME"),
                reasoning_key: env_non_empty("KIMI_MODEL_REASONING_KEY"),
                adaptive_thinking,
                ..Default::default()
            },
        );
        self.default_model = Some(ENV_MODEL_ALIAS_KEY.to_string());
        Ok(())
    }

    /// v2 `buildValidated` (`configService.ts:753-771`): each section is
    /// validated on its own; a section that fails its schema produces a
    /// diagnostic and falls back to the section default without taking the
    /// rest of the file down. serde is all-or-nothing, so the check runs one
    /// top-level key at a time and removes the keys that do not parse.
    fn validated_toml(raw_toml: &str) -> Result<(String, Vec<String>, Vec<String>), String> {
        let mut table: toml::Table = raw_toml
            .parse()
            .map_err(|e| format!("Invalid TOML configuration: {e}"))?;
        let mut warnings = Vec::new();
        let mut dropped = Vec::new();
        for key in table.keys().cloned().collect::<Vec<_>>() {
            let mut probe = toml::Table::new();
            if let Some(value) = table.get(&key) {
                probe.insert(key.clone(), value.clone());
            }
            let probe_text =
                toml::to_string(&probe).map_err(|e| format!("Invalid TOML configuration: {e}"))?;
            if let Err(error) = toml::from_str::<Self>(&probe_text) {
                table.remove(&key);
                warnings.push(format!("Ignored invalid config section '{key}': {error}"));
                dropped.push(key);
            }
        }
        warnings.extend(Self::deprecated_key_warnings(&table));
        let text =
            toml::to_string(&table).map_err(|e| format!("Invalid TOML configuration: {e}"))?;
        Ok((text, warnings, dropped))
    }

    /// v2 `collectKeyDeprecations` (`deprecations.ts:5-31`): the renamed
    /// `[loop_control]` keys still resolve, but the diagnostic asks the user to
    /// rename them instead of dropping them silently.
    fn deprecated_key_warnings(table: &toml::Table) -> Vec<String> {
        let Some(section) = table.get("loop_control").and_then(toml::Value::as_table) else {
            return Vec::new();
        };
        [
            ("max_retries_per_step", "max_attempts_per_step"),
            ("max_steps_per_run", "max_steps_per_turn"),
        ]
        .into_iter()
        .filter(|(old, _)| section.contains_key(*old))
        .map(|(old, new)| {
            format!(
                "[loop_control] '{old}' is deprecated and no longer used; rename it to '{new}'. Run /update-config to fix it."
            )
        })
        .collect()
    }

    /// `[models]` entries that declare no `model` field (v2 #3681
    /// `collectMalformedModelEntries`): the entry cannot resolve to a wire
    /// model, so it is inert. The usual cause is an unquoted dotted alias —
    /// `[models.a.b]` parses as a nested table under `models.a` — which the
    /// message spells out so the fix is a quoted table name.
    ///
    /// Reads the raw TOML rather than the deserialized config: serde drops the
    /// nested table that makes the entry malformed, and with it the evidence
    /// of what the alias was meant to be.
    pub fn malformed_model_entries(raw_toml: &str) -> Vec<String> {
        let Ok(value) = raw_toml.parse::<toml::Value>() else {
            return Vec::new();
        };
        let Some(models) = value.get("models").and_then(toml::Value::as_table) else {
            return Vec::new();
        };
        let mut warnings = Vec::new();
        for (alias, entry) in models {
            let Some(entry) = entry.as_table() else {
                continue;
            };
            if entry.contains_key("model") {
                continue;
            }
            let base = format!(
                "[models] entry '{alias}' is missing the 'model' field and cannot be used as a model"
            );
            match dotted_alias_suffix(alias, entry) {
                Some(dotted) => warnings.push(format!(
                    "{base}; if the alias contains dots, quote the table name (e.g. [models.\"{dotted}\"])."
                )),
                None => warnings.push(format!("{base}.")),
            }
        }
        warnings
    }

    /// 探测 `config.toml` 路径（严格支持环境变量隔离）：
    /// 1. `KIMI_CONFIG_PATH` 显式指定的文件路径
    /// 2. `KIMI_CODE_HOME/config.toml` 隔离目录
    /// 3. `~/.kimi-code/config.toml` 全局配置
    /// 4. `~/.kimi/config.toml` 遗留兼容配置
    ///
    /// `./config.toml` 不会被发现：仓库可以提交该文件来注入 providers/hooks/
    /// permission 默认值，且在信任提示之前就会生效。需要工作区级配置时用
    /// `KIMI_CONFIG_PATH` 显式指定。
    pub fn discover() -> Result<(Self, PathBuf), String> {
        // 1. 显式指定的配置文件路径
        if let Some(explicit) = std::env::var_os("KIMI_CONFIG_PATH") {
            let path = PathBuf::from(explicit);
            if path.is_file() {
                let config = Self::load_or_default(&path);
                return Ok((config, path));
            }
        }

        // 2. 显式隔离的 HOME 目录
        if let Some(code_home) = std::env::var_os("KIMI_CODE_HOME") {
            let path = PathBuf::from(code_home).join("config.toml");
            if path.is_file() {
                let config = Self::load_or_default(&path);
                return Ok((config, path));
            }
        }

        // 3. 用户全局配置
        if let Some(home) = dirs_home() {
            let kimi_code_config = home.join(".kimi-code").join("config.toml");
            if kimi_code_config.is_file() {
                let config = Self::load_or_default(&kimi_code_config);
                return Ok((config, kimi_code_config));
            }

            let legacy_config = home.join(".kimi").join("config.toml");
            if legacy_config.is_file() {
                let config = Self::load_or_default(&legacy_config);
                return Ok((config, legacy_config));
            }
        }

        Err(
            "No config.toml discovered in KIMI_CONFIG_PATH, KIMI_CODE_HOME, ~/.kimi-code/config.toml, or ~/.kimi/config.toml"
                .into(),
        )
    }

    /// v2 `configService.ts:668-687`: a file that fails to load is reported and
    /// replaced by the built-in defaults instead of aborting startup. The error
    /// travels in `config_warnings`, which the server publishes, so the failure
    /// stays observable.
    pub fn load_or_default(path: &Path) -> Self {
        match Self::from_file(path) {
            Ok(mut config) => {
                // v2 `kimiModelEnvOverlay` runs on the effective config, after
                // the file is parsed. A malformed value fails the load (v2
                // raises `CONFIG_INVALID`) instead of silently running the
                // file as if the env were unset.
                if let Err(error) = config.apply_model_env_overlay() {
                    tracing::error!(path = %path.display(), "{error}");
                    let mut fallback = Self::default();
                    fallback.config_warnings.push(error);
                    return fallback;
                }
                config
            }
            Err(error) => {
                tracing::error!(path = %path.display(), "{error}");
                let mut config = Self::default();
                config.config_warnings.push(error);
                config
            }
        }
    }

    /// Extract the active Native LLM configuration for the given or default model.
    pub fn extract_native_llm(&self, target_model: Option<&str>) -> Option<ResolvedNativeLlm> {
        let model_key = target_model
            .or(self.default_model.as_deref())
            .unwrap_or("default");

        // v2 `findByName`: the table key first, then any entry that declares
        // the name in its `aliases`, so `default_model` may point at an alias
        // rather than the table key.
        let entry = self.models.get_key_value(model_key).or_else(|| {
            self.models.iter().find(|(_, entry)| {
                entry
                    .aliases
                    .as_ref()
                    .is_some_and(|names| names.iter().any(|name| name == model_key))
            })
        });

        let (provider_name, wire_model) = if let Some((key, alias)) = entry {
            // v2 `buildModel`: `providerId` is the document schema's spelling of
            // `provider`, and the wire name is `name` first, then `model` —
            // never the alias.
            let p = alias
                .provider_id
                .as_deref()
                .or(alias.provider.as_deref())
                .unwrap_or(key.as_str());
            let m = alias
                .name
                .as_deref()
                .or(alias.model.as_deref())
                .unwrap_or(key.as_str());
            (p, m)
        } else if let Some((p_name, _)) = self.providers.iter().find(|(name, _)| *name == model_key)
        {
            (p_name.as_str(), model_key)
        } else {
            let fallback = self.agent.native_llm_provider.as_deref()?;
            (fallback, model_key)
        };

        let provider = self.providers.get(provider_name)?;
        // v2 `effectiveRecordOf`: the entry's `overrides` block wins over the
        // base fields it shadows, so every catalog read below sees the
        // effective value.
        let effective = entry.map(|(_, alias)| alias.effective());
        let alias = effective.as_ref();
        // A declared alias endpoint wins: gateway providers serve one alias
        // over a different path than the provider default
        // (schema `models.*.baseUrl`).
        let p_type = provider
            .provider_type
            .as_deref()
            .unwrap_or("openai")
            .to_lowercase();
        // v2 `resolveModelConnection` (`human/llm/protocol/connection.ts`):
        // the declared endpoint wins, then the provider type's `baseUrlEnv`
        // read from the process environment, then the type's default — what
        // the vendor SDK would pick, which the engine carries itself because
        // it assembles the request URL. Google's default is the host root, so
        // a base-URL-less Gemini provider still resolves natively instead of
        // silently falling back to the host LLM proxy.
        // antigravity is a subprocess bridge, not an HTTP endpoint: there is
        // no base URL to resolve and no credential for the engine to hold
        // (the `agy` CLI authenticates itself). Every other provider still
        // needs both, so the antigravity branch only widens resolution.
        let is_antigravity = p_type == "antigravity"
            || alias.and_then(|alias| alias.protocol.as_deref()) == Some("antigravity");
        let raw_base_url = alias
            .and_then(|alias| alias.base_url.as_deref())
            .or(provider.base_url.as_deref())
            .map(str::to_string)
            .or_else(|| endpoint_fallback_base_url(&p_type));
        let raw_base_url = if is_antigravity {
            raw_base_url.unwrap_or_default()
        } else {
            raw_base_url?
        };
        // v2 `resolveModelAuthMaterial`: a model-level credential wins over the
        // provider's, and a static key wins over an OAuth binding at the same
        // level. An OAuth-bound model authenticates through the host token
        // channel instead, so its static key is optional; a model with neither
        // credential of its own falls back to the provider's, and one with no
        // credential anywhere cannot serve a request, so it does not resolve.
        let model_api_key = alias
            .and_then(|alias| alias.api_key.clone())
            .filter(|key| !key.is_empty());
        let model_oauth = alias.and_then(|alias| alias.oauth.as_ref());
        let provider_api_key = provider.api_key.clone().unwrap_or_default();
        let (api_key, auth_provider, api_key_env) = if let Some(key) = model_api_key {
            (key, None, None)
        } else if model_oauth.is_some() {
            (String::new(), Some(provider_name.to_string()), None)
        } else if !provider_api_key.is_empty() {
            (provider_api_key, None, provider.api_key_env.clone())
        } else if provider.oauth.is_some() {
            (String::new(), Some(provider_name.to_string()), None)
        } else if is_antigravity {
            // The CLI owns its credentials; no key and no env var is needed.
            (String::new(), None, None)
        } else {
            // An env-bound provider without a static key still resolves: the
            // transport reads the credential from the named variable at
            // request time (v2 `provider.apiKeyEnv`), so startup does not
            // require it to be set yet. Without any credential channel the
            // model cannot serve a request and does not resolve.
            let env_name = provider.api_key_env.as_deref().filter(|e| !e.is_empty())?;
            (String::new(), None, Some(env_name.to_string()))
        };

        let p_type = provider
            .provider_type
            .as_deref()
            .unwrap_or("openai")
            .to_lowercase();
        // The alias declares its wire protocol (schema `models.*.protocol`:
        // "anthropic" | "openai_responses"); the provider type is the
        // fallback, and everything else is Chat Completions.
        //
        // Google has to be matched explicitly: falling through to "openai"
        // sent Gemini traffic to `{base}/chat/completions` with a Chat
        // Completions body — the endpoint is the only thing a Gemini-compatible
        // relay accepts, so the request never even reached the model.
        //
        // `openai_responses` is a provider type of its own (v2 registers it as
        // a definition whose base protocol is the Responses API, and the host
        // config layer keys provider selection off it): a provider declared
        // with that type speaks Responses without the alias also having to
        // repeat the protocol.
        //
        // `vertexai` joins the Google family: the model catalog already maps
        // it to `google-genai` (`server/model_catalog.rs`), the docs describe
        // this fork's Vertex as Gemini-mode over a proxyable endpoint, and
        // v2 selects the Vertex endpoint inside the same Google connection.
        // Leaving it to fall through to Chat Completions contradicted the
        // catalog and sent Vertex-shaped URLs an OpenAI body.
        let protocol = match alias.and_then(|alias| alias.protocol.as_deref()) {
            Some("anthropic") => "anthropic",
            Some("openai_responses") => "openai_responses",
            Some("google") | Some("google-genai") | Some("gemini") | Some("vertexai") => {
                "google-genai"
            }
            // antigravity keeps its own protocol name: it is not an HTTP wire
            // format, it is the `agy` subprocess bridge
            // (`llm/antigravity.rs`).
            Some("antigravity") => "antigravity",
            _ if p_type == "anthropic" => "anthropic",
            _ if p_type == "antigravity" => "antigravity",
            _ if p_type == "google" || p_type == "google-genai" || p_type == "gemini" => {
                "google-genai"
            }
            _ if p_type == "vertexai" => "google-genai",
            _ if p_type == "openai_responses" || p_type == "openai-responses" => "openai_responses",
            _ => "openai",
        };

        let base_url = if raw_base_url.is_empty() {
            // antigravity has no endpoint; normalizing "" would fabricate
            // `/v1`.
            String::new()
        } else {
            normalize_base_url(&raw_base_url, protocol)
        };
        let capabilities = alias
            .and_then(|alias| alias.capabilities.clone())
            .filter(|caps| !caps.is_empty());

        Some(ResolvedNativeLlm {
            protocol: protocol.into(),
            alias: Some(model_key.to_string()),
            provider_type: p_type,
            base_url,
            api_key,
            api_key_env,
            auth_provider,
            model: wire_model.into(),
            max_tokens: provider.max_tokens,
            capabilities,
            custom_headers: provider.custom_headers.clone().unwrap_or_default(),
            beta_api: alias.and_then(|alias| alias.beta_api).unwrap_or(false),
            system_prompt: alias.and_then(|alias| alias.system_prompt.clone()),
            max_output_size: alias.and_then(|alias| alias.max_output_size),
            max_input_size: alias.and_then(|alias| alias.max_input_size),
            adaptive_thinking: alias.and_then(|alias| alias.adaptive_thinking),
            reasoning_key: alias.and_then(|alias| alias.reasoning_key.clone()),
            off_effort: alias.and_then(|alias| alias.off_effort.clone()),
            binary_path: provider.binary_path.clone(),
            antigravity: provider.antigravity.clone(),
        })
    }

    /// The effective `[models]` record for `alias` (table key first, then a
    /// declared alias name), with its `overrides` folded in.
    fn effective_model_for(&self, alias: &str) -> Option<ModelAliasConfig> {
        self.models
            .get(alias)
            .or_else(|| {
                self.models.values().find(|entry| {
                    entry
                        .aliases
                        .as_ref()
                        .is_some_and(|names| names.iter().any(|name| name == alias))
                })
            })
            .map(ModelAliasConfig::effective)
    }

    /// Resolve `[secondary_model]` into the engine's subagent model pool (v2
    /// `resolveSubagentModelPool` + `assertValidSubagentModelConfig`).
    ///
    /// `Ok(None)` when the section is absent or carries no pool keys (a
    /// patch-only recipe stays inert). A malformed section is an error naming
    /// the offending entry, so the standalone entry points fail at startup
    /// instead of silently disabling the user's configuration. The pool-wide
    /// `default_effort` is validated against every resolved entry (v2 #3785
    /// [`Self::validate_secondary_model_effort`]).
    pub fn extract_secondary_model_pool(
        &self,
        target_model: Option<&str>,
    ) -> Result<Option<SecondaryModelPool>, String> {
        let Some(section) = self.secondary_model.as_ref() else {
            return Ok(None);
        };
        // v2 #4418 removed `force`: a lone `default_model` already pins every
        // spawn, so the key is ignored with a warning instead of failing
        // session startup (v2 `SECONDARY_MODEL_FORCE_REMOVED_MESSAGE`).
        if section.force == Some(true) {
            tracing::warn!(
                "[secondary_model] 'force' is no longer supported and is ignored: a lone \
                 default_model already pins every Agent and AgentSwarm subagent to that model, \
                 and a [secondary_model.models] table only offers its own entries. Remove the key."
            );
        }
        let table = section.models.as_ref();
        // v2: the legacy `model` key forms the fallback pool only while no
        // table exists. With a table it is not a pool default, so a section
        // carrying both fails the `default_model` requirement below instead of
        // silently pinning the pool to whatever `model` happens to name.
        let default_model = match section.default_model.as_deref() {
            Some(default) => Some(default),
            None if table.is_none() => section.model.as_deref(),
            None => None,
        };

        if table.is_none() && default_model.is_none() {
            return Ok(None);
        }
        let entries: Vec<(String, String)> = match table {
            Some(table) => table
                .iter()
                .map(|(alias, hint)| (alias.clone(), hint.clone()))
                .collect(),
            None => vec![(
                default_model
                    .expect("a pool default is required without a table")
                    .to_string(),
                String::new(),
            )],
        };
        if entries
            .iter()
            .any(|(alias, _)| alias == crate::subagent::secondary::PRIMARY_MODEL_CHOICE)
        {
            // v2 `SECONDARY_MODEL_PRIMARY_MODEL_RESERVED_MESSAGE`.
            return Err(format!(
                "[secondary_model.models] key \"{}\" is reserved. Rename the pool entry.",
                crate::subagent::secondary::PRIMARY_MODEL_CHOICE
            ));
        }
        let Some(default_model) = default_model else {
            return Err(
                "[secondary_model].default_model is required when [secondary_model.models] is configured"
                    .into(),
            );
        };
        if table.is_some() && !entries.iter().any(|(alias, _)| alias == default_model) {
            let available = entries
                .iter()
                .map(|(alias, _)| alias.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(format!(
                "[secondary_model].default_model \"{default_model}\" is not a [secondary_model.models] key. Available models: {available}."
            ));
        }

        let thinking_keep = self.resolve_thinking_keep();
        let thinking_enabled = self.thinking.enabled.unwrap_or(true);
        let models = entries
            .into_iter()
            .map(|(alias, hint)| {
                let resolved = self.extract_native_llm(Some(alias.as_str())).ok_or_else(|| {
                    format!(
                        "[secondary_model.models] entry \"{alias}\" could not be resolved: add it to [models] with a provider that has credentials."
                    )
                })?;
                // v2 `resolveSubagentThinking`: with no pool-wide effort the
                // entry keeps the effort its own `[models]` record declares,
                // but only while thinking is enabled and the declared value is
                // one the record lists support for (v2
                // `declaredDefaultEffortForModel`). A value outside that list
                // resolves to "unset" rather than to the model's middle
                // effort, and an empty `support_efforts` declares nothing.
                let declared_effort = section.default_effort.is_none().then(|| {
                    let model = self.effective_model_for(&alias)?;
                    let declared = model.default_effort.as_deref()?.trim();
                    let supported = model
                        .support_efforts
                        .as_ref()
                        .is_some_and(|efforts| efforts.iter().any(|value| value.trim() == declared));
                    (thinking_enabled && supported && model_supports_thinking(Some(&model)))
                        .then(|| declared.to_string())
                });
                let effort = section
                    .default_effort
                    .clone()
                    .or(declared_effort.flatten());
                let request_model = self.effective_model_for(&alias);
                Ok(SecondaryModelEntry {
                    alias,
                    hint,
                    llm: native_llm_config(
                        resolved,
                        effort.as_deref(),
                        thinking_keep.as_deref(),
                        request_model.as_ref(),
                    ),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;

        self.validate_secondary_model_effort(
            section.default_effort.as_deref(),
            &models
                .iter()
                .map(|entry| entry.alias.as_str())
                .collect::<Vec<_>>(),
        )?;

        Ok(Some(SecondaryModelPool {
            default_model: default_model.to_string(),
            caller_model_alias: target_model
                .map(str::to_string)
                .or_else(|| self.default_model.clone()),
            models,
        }))
    }

    /// Resolve `[agent].multi_llm` into the engine's concurrent-provider race
    /// (`MultiLLM`, first-past-the-post). Each named entry is a `[models]`
    /// alias resolved through the same [`Self::extract_native_llm`] path the
    /// session model and the `[secondary_model]` pool use, so a racer carries
    /// a real native HTTP transport rather than a host proxy — the latter is
    /// what the race used to be limited to, and every config-reading entry
    /// point answers `host/llm_chat` with an error, so such a race could never
    /// produce a winner.
    ///
    /// `Ok(None)` when the key is absent or empty (the default: no race). An
    /// alias that cannot resolve is an error naming it, matching how the
    /// `[secondary_model]` pool reports an unresolvable entry, because
    /// silently dropping a racer would leave the user with a slower single
    /// provider and no explanation.
    pub fn extract_multi_llm(
        &self,
        _target_model: Option<&str>,
    ) -> Result<Option<Vec<ResolvedMultiLlmProvider>>, String> {
        let Some(aliases) = self.agent.multi_llm.as_ref() else {
            return Ok(None);
        };
        let aliases: Vec<String> = aliases
            .iter()
            .map(|alias| alias.trim().to_string())
            .filter(|alias| !alias.is_empty())
            .collect();
        if aliases.is_empty() {
            return Ok(None);
        }
        // A race of one is not a race: the single-provider case is the plain
        // session model, which `build_llm_for_spec` already handles. Refusing
        // it here keeps `providers` non-empty meaningful (it outranks
        // `native_llm`).
        if aliases.len() < 2 {
            return Err(
                "[agent].multi_llm needs at least two entries to race; a single provider should be set as default_model"
                    .into(),
            );
        }
        let thinking_keep = self.resolve_thinking_keep();
        let mut resolved = Vec::with_capacity(aliases.len());
        for alias in &aliases {
            let native = self.extract_native_llm(Some(alias)).ok_or_else(|| {
                format!(
                    "[agent].multi_llm entry \"{alias}\" could not be resolved: add it to [models] with a provider that has credentials."
                )
            })?;
            // Each racer carries its own declared effort: the entries are
            // different models with their own capabilities, so inheriting the
            // session model's effort would misconfigure them.
            let entry = self.models.get(alias).or_else(|| {
                self.models.values().find(|entry| {
                    entry
                        .aliases
                        .as_ref()
                        .is_some_and(|names| names.iter().any(|name| name == alias))
                })
            });
            let effort = entry.and_then(|entry| entry.default_effort.clone());
            let model = entry.map(ModelAliasConfig::effective);
            resolved.push(ResolvedMultiLlmProvider {
                name: alias.clone(),
                llm: native_llm_config(
                    native,
                    effort.as_deref(),
                    thinking_keep.as_deref(),
                    model.as_ref(),
                ),
            });
        }
        Ok(Some(resolved))
    }

    /// Reject a pool-wide `default_effort` no pool model can run (v2 #3785
    /// `assertValidSubagentDefaultEffort`). The effort is applied to every
    /// entry of the pool, so one model that cannot honor it would silently
    /// degrade that subagent's thinking; failing at startup names the model
    /// instead. Runs after the pool resolves, so an unknown alias still
    /// reports as unresolvable rather than as an effort mismatch.
    fn validate_secondary_model_effort(
        &self,
        effort: Option<&str>,
        aliases: &[&str],
    ) -> Result<(), String> {
        let Some(effort) = effort
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_ascii_lowercase)
        else {
            return Ok(());
        };
        for alias in aliases {
            // The effort is checked against the effective record: an override
            // that adds or narrows `support_efforts` is what the model runs
            // with, so validating the base fields would reject a valid pool.
            let effective = self.models.get(*alias).map(ModelAliasConfig::effective);
            let model = effective.as_ref();
            if effort == "off" && model_always_thinks(model) {
                return Err(format!(
                    "[secondary_model].default_effort \"off\" cannot disable thinking for model \"{alias}\", which always reasons. Choose a concrete thinking effort instead of \"off\"."
                ));
            }
            if model_supports_effort(&effort, model) {
                continue;
            }
            if !model_supports_thinking(model) {
                return Err(format!(
                    "[secondary_model].default_effort \"{effort}\" is set but model \"{alias}\" does not support thinking."
                ));
            }
            let supported = model
                .and_then(|model| model.support_efforts.as_ref())
                .map(|efforts| efforts.join(", "))
                .unwrap_or_default();
            return Err(format!(
                "[secondary_model].default_effort \"{effort}\" is not supported by model \"{alias}\". Supported efforts: {supported}."
            ));
        }
        Ok(())
    }

    /// Resolve the per-step LLM attempt cap (v2 `resolveMaxAttemptsPerStep`):
    /// env `KIMI_LOOP_MAX_ATTEMPTS_PER_STEP` > deprecated
    /// `KIMI_LOOP_MAX_RETRIES_PER_STEP` > `[loop_control].max_attempts_per_step`
    /// > the deprecated `max_retries_per_step`. `None` keeps the engine
    /// > default (10), so an unset section changes nothing.
    pub fn resolve_max_attempts_per_step(&self) -> Option<u32> {
        env_non_negative("KIMI_LOOP_MAX_ATTEMPTS_PER_STEP")
            .or_else(|| env_non_negative("KIMI_LOOP_MAX_RETRIES_PER_STEP"))
            .or(self.loop_control.max_attempts_per_step)
            .or(self.loop_control.max_retries_per_step)
    }

    /// Resolve the per-turn step cap (v2 `loopControl.maxStepsPerTurn`): env
    /// `KIMI_LOOP_MAX_STEPS_PER_TURN` > `[loop_control].max_steps_per_turn` >
    /// the deprecated `max_steps_per_run`. Unset or `0` means unlimited
    /// (v2 only enforces a positive cap), so both resolve to `None` and the
    /// engine keeps its own default.
    pub fn resolve_max_steps_per_turn(&self) -> Option<u32> {
        env_non_negative("KIMI_LOOP_MAX_STEPS_PER_TURN")
            .or(self.loop_control.max_steps_per_turn)
            .or(self.loop_control.max_steps_per_run)
            .filter(|value| *value > 0)
    }

    /// Resolve the total-request cap for one compaction round (v2 #3750
    /// `loopControl.compactionMaxAttempts`). Upstream binds no env var to this
    /// key, so the file is the only source. The schema floor is 1 (v2
    /// `z.number().int().min(1)`), so a `0` is ignored rather than read as "no
    /// attempts"; `None` keeps the engine default of
    /// [`crate::compaction::DEFAULT_COMPACTION_MAX_ATTEMPTS`].
    pub fn resolve_compaction_max_attempts(&self) -> Option<u32> {
        self.loop_control
            .compaction_max_attempts
            .filter(|value| *value > 0)
    }

    /// Resolve the compaction trigger ratio (v2
    /// `compactionTriggerRatio`). A value inside the schema range 0.5..=0.99 is
    /// returned as written; a value outside it (or a non-finite one) is
    /// **discarded, not clamped** — it resolves to `None` so the engine keeps
    /// its default rather than silently compacting on a number the user never
    /// wrote. Same convention as
    /// [`KimiConfig::resolve_compaction_max_attempts`].
    ///
    /// Accepted for config parity: nothing in the engine reads it yet, because
    /// `CompactionConfig` is host-injected through `RunTurnInput`, so setting
    /// the key changes no behaviour today.
    #[allow(dead_code)]
    pub fn resolve_compaction_trigger_ratio(&self) -> Option<f64> {
        self.loop_control
            .compaction_trigger_ratio
            .filter(|value| value.is_finite() && (0.5..=0.99).contains(value))
    }

    /// Resolve the reserved context headroom (v2 `reservedContextSize`).
    /// `0` is meaningful — in v2 it disables the headroom trigger — so only
    /// an absent value resolves to `None`.
    ///
    /// Accepted for config parity: nothing in the engine reads it yet, same
    /// gap as [`KimiConfig::resolve_compaction_trigger_ratio`].
    #[allow(dead_code)]
    pub fn resolve_reserved_context_size(&self) -> Option<u32> {
        self.loop_control.reserved_context_size
    }

    /// Resolve the preserved-thinking passthrough (v2 `resolveThinkingKeep`):
    /// env `KIMI_MODEL_THINKING_KEEP` > `[thinking].keep`. Off values
    /// (`false`/`0`/`no`/`off`/`none`/`null`) and `[thinking].enabled = false`
    /// resolve to `None`; unset stays `None` — the native wire is opt-in,
    /// unlike the host-proxy path's `"all"` default.
    pub fn resolve_thinking_keep(&self) -> Option<String> {
        if self.thinking.enabled == Some(false) {
            return None;
        }
        let raw = std::env::var("KIMI_MODEL_THINKING_KEEP")
            .ok()
            .or_else(|| self.thinking.keep.clone())?;
        let value = raw.trim();
        if value.is_empty()
            || matches!(
                value.to_ascii_lowercase().as_str(),
                "false" | "0" | "no" | "off" | "none" | "null"
            )
        {
            return None;
        }
        Some(value.to_string())
    }

    /// Resolve `[services.moonshot_search]` into the native WebSearch backend
    /// (v2 `isolateEnvServiceCredentials`: env `KIMI_WEB_SEARCH_BASE_URL` /
    /// `KIMI_WEB_SEARCH_API_KEY` over `config.toml`). An `oauth`-only entry
    /// (managed login) resolves without a credential, mirroring the host
    /// resolver; the tools then report or fall back accordingly.
    pub fn resolve_web_search_service(&self) -> Option<ResolvedWebService> {
        resolve_web_service(
            self.services.moonshot_search.as_ref(),
            "KIMI_WEB_SEARCH_BASE_URL",
            "KIMI_WEB_SEARCH_API_KEY",
        )
    }

    /// Resolve `[services.moonshot_fetch]` into the native FetchURL backend
    /// (env `KIMI_WEB_FETCH_BASE_URL` / `KIMI_WEB_FETCH_API_KEY` over
    /// `config.toml`).
    pub fn resolve_web_fetch_service(&self) -> Option<ResolvedWebService> {
        resolve_web_service(
            self.services.moonshot_fetch.as_ref(),
            "KIMI_WEB_FETCH_BASE_URL",
            "KIMI_WEB_FETCH_API_KEY",
        )
    }

    /// Resolve `[services.bing_api]` into the Bing Web Search API fallback
    /// (env `KIMI_BING_BASE_URL` / `KIMI_BING_API_KEY` over `config.toml`).
    pub fn resolve_bing_api_service(&self) -> Option<ResolvedWebService> {
        resolve_web_service(
            self.services.bing_api.as_ref(),
            "KIMI_BING_BASE_URL",
            "KIMI_BING_API_KEY",
        )
    }

    /// Resolve the per-`Agent` subagent timeout (v2
    /// `resolveSubagentTimeoutMs`): env `KIMI_SUBAGENT_TIMEOUT_MS` (a
    /// non-negative integer) over `[subagent].timeout_ms`. `None` keeps the
    /// engine default (2h); `0` is passed through and the tools treat it as
    /// the default, matching the host-driven paths.
    pub fn resolve_subagent_timeout_ms(&self) -> Option<u64> {
        env_non_negative_u64("KIMI_SUBAGENT_TIMEOUT_MS").or(self.subagent.timeout_ms)
    }

    /// Resolve the per-`AgentSwarm` subagent timeout (v2
    /// `resolveSwarmTimeoutMs`): env `KIMI_CODE_SWARM_TIMEOUT_MS` over
    /// `[swarm].timeout_ms`. A dedicated knob — swarms never inherit the
    /// subagent timeout.
    pub fn resolve_swarm_timeout_ms(&self) -> Option<u64> {
        env_non_negative_u64("KIMI_CODE_SWARM_TIMEOUT_MS").or(self.swarm.timeout_ms)
    }

    /// Whether the cron scheduler is switched off (v2 `CronConfig.disabled`):
    /// env `KIMI_DISABLE_CRON` over `[cron].disabled`, defaulting to off.
    pub fn cron_disabled(&self) -> bool {
        env_switch_default_off("KIMI_DISABLE_CRON") || self.cron.disabled.unwrap_or(false)
    }

    /// Whether to drop the anti-herd jitter offset (v2 `CronConfig.noJitter`):
    /// env `KIMI_CRON_NO_JITTER` over `[cron].no_jitter`.
    pub fn cron_no_jitter(&self) -> bool {
        env_switch_default_off("KIMI_CRON_NO_JITTER") || self.cron.no_jitter.unwrap_or(false)
    }

    /// Whether to keep stale recurring entries instead of pruning them after
    /// the 7-day window (v2 `CronConfig.noStale`): env `KIMI_CRON_NO_STALE`
    /// over `[cron].no_stale`.
    pub fn cron_no_stale(&self) -> bool {
        env_switch_default_off("KIMI_CRON_NO_STALE") || self.cron.no_stale.unwrap_or(false)
    }
    /// Resolve the raw-byte budget for model-initiated image reads (v2
    /// `resolveReadImageByteBudget`): env `KIMI_IMAGE_READ_BYTE_BUDGET` (a
    /// positive integer) over `[image].read_byte_budget`. `None` keeps the
    /// engine default (256KB).
    pub fn resolve_image_read_byte_budget(&self) -> Option<u64> {
        env_positive_u64("KIMI_IMAGE_READ_BYTE_BUDGET")
            .or(self.image.read_byte_budget.filter(|value| *value > 0))
    }

    /// Resolve the longest-edge ceiling for model-initiated image reads (v2
    /// `resolveMaxImageEdgePx`): env `KIMI_IMAGE_MAX_EDGE_PX` over
    /// `[image].max_edge_px`. `None` keeps the engine default (2000px).
    pub fn resolve_image_max_edge_px(&self) -> Option<u32> {
        env_positive_u64("KIMI_IMAGE_MAX_EDGE_PX")
            .and_then(|value| u32::try_from(value).ok())
            .or(self.image.max_edge_px.filter(|value| *value > 0))
    }

    /// Resolve the configured concurrent-task cap before the environment
    /// overlay: `[task].max_running_tasks` wins over the legacy
    /// `[background].max_running_tasks` (v2 `resolveAgentTaskConfig`).
    fn configured_max_running_tasks(&self) -> Option<u32> {
        self.task
            .max_running_tasks
            .filter(|value| *value > 0)
            .or(self.background.max_running_tasks.filter(|value| *value > 0))
    }

    /// Resolve the concurrent-task cap: env
    /// `KIMI_CODE_BACKGROUND_MAX_RUNNING_TASKS` over the configured task /
    /// legacy-background value. A non-positive or invalid value is ignored.
    pub fn resolve_background_max_running_tasks(&self) -> Option<u32> {
        env_positive_u64("KIMI_CODE_BACKGROUND_MAX_RUNNING_TASKS")
            .and_then(|value| u32::try_from(value).ok())
            .or_else(|| self.configured_max_running_tasks())
    }

    /// Resolve the default timeout for background Bash tasks (v2
    /// `taskEnvBindings.bashTaskTimeoutS`): env
    /// `KIMI_CODE_BACKGROUND_BASH_TASK_TIMEOUT_S` over `[task]` over the
    /// legacy `[background]`. `None` keeps the tool's built-in ceiling;
    /// `Some(0)` means "no timeout".
    pub fn resolve_bash_task_timeout_s(&self) -> Option<u64> {
        env_non_negative_u64("KIMI_CODE_BACKGROUND_BASH_TASK_TIMEOUT_S")
            .or(self.task.bash_task_timeout_s)
            .or(self.background.bash_task_timeout_s)
    }

    /// Whether background tasks survive the session closing (v2
    /// `taskEnvBindings.keepAliveOnExit`): env
    /// `KIMI_CODE_BACKGROUND_KEEP_ALIVE_ON_EXIT` over `[task]` over the
    /// legacy `[background]`.
    pub fn resolve_keep_alive_on_exit(&self) -> Option<bool> {
        env_bool("KIMI_CODE_BACKGROUND_KEEP_ALIVE_ON_EXIT")
            .or(self.task.keep_alive_on_exit)
            .or(self.background.keep_alive_on_exit)
    }

    /// Whether a timed-out foreground Bash command is moved to the background
    /// (v2 `bashAutoBackgroundOnTimeout`): `[task]` over the legacy
    /// `[background]`. No env binding in v2.
    pub fn resolve_bash_auto_background_on_timeout(&self) -> Option<bool> {
        self.task
            .bash_auto_background_on_timeout
            .or(self.background.bash_auto_background_on_timeout)
    }

    /// How long `stop` waits for a cooperative exit (v2 `killGracePeriodMs`):
    /// `[task]` over the legacy `[background]`. No env binding in v2.
    pub fn resolve_kill_grace_period_ms(&self) -> Option<u64> {
        self.task
            .kill_grace_period_ms
            .or(self.background.kill_grace_period_ms)
    }

    /// Settled tasks retained for inspection (v2 `maxSettledTasks`, min 1).
    /// Accepted for config parity; the engine's task store has no consumer.
    pub fn resolve_max_settled_tasks(&self) -> Option<u32> {
        self.task
            .max_settled_tasks
            .filter(|value| *value > 0)
            .or(self.background.max_settled_tasks.filter(|value| *value > 0))
    }

    /// `--print` wait ceiling in seconds (v2 `printWaitCeilingS`,
    /// `KIMI_CODE_BACKGROUND_PRINT_WAIT_CEILING_S` over the file, min 1).
    pub fn resolve_print_wait_ceiling_s(&self) -> Option<u64> {
        env_positive_u64("KIMI_CODE_BACKGROUND_PRINT_WAIT_CEILING_S")
            .or(self.task.print_wait_ceiling_s.filter(|value| *value > 0))
            .or(self
                .background
                .print_wait_ceiling_s
                .filter(|value| *value > 0))
    }

    /// `--print` background mode (v2 `resolvePrintBackgroundMode`): env
    /// `KIMI_CODE_BACKGROUND_PRINT_BACKGROUND_MODE`, then the file, then
    /// `drain` when `keepAliveOnExit` is on and `steer` otherwise. An
    /// unrecognized value is ignored so the default applies.
    pub fn resolve_print_background_mode(&self) -> &'static str {
        let configured = std::env::var("KIMI_CODE_BACKGROUND_PRINT_BACKGROUND_MODE")
            .ok()
            .map(|value| value.trim().to_ascii_lowercase())
            .filter(|value| matches!(value.as_str(), "exit" | "drain" | "steer"))
            .or_else(|| {
                self.task
                    .print_background_mode
                    .as_deref()
                    .map(|value| value.trim().to_ascii_lowercase())
                    .filter(|value| matches!(value.as_str(), "exit" | "drain" | "steer"))
            })
            .or_else(|| {
                self.background
                    .print_background_mode
                    .as_deref()
                    .map(|value| value.trim().to_ascii_lowercase())
                    .filter(|value| matches!(value.as_str(), "exit" | "drain" | "steer"))
            });
        match configured.as_deref() {
            Some("exit") => "exit",
            Some("drain") => "drain",
            Some("steer") => "steer",
            _ => {
                if self.resolve_keep_alive_on_exit() == Some(true) {
                    "drain"
                } else {
                    "steer"
                }
            }
        }
    }

    /// `--print` turn cap (v2 `printMaxTurns`, min 1): env
    /// `KIMI_CODE_BACKGROUND_PRINT_MAX_TURNS` over the file.
    pub fn resolve_print_max_turns(&self) -> Option<u32> {
        env_positive_u64("KIMI_CODE_BACKGROUND_PRINT_MAX_TURNS")
            .and_then(|value| u32::try_from(value).ok())
            .or(self.task.print_max_turns.filter(|value| *value > 0))
            .or(self.background.print_max_turns.filter(|value| *value > 0))
    }

    /// The whole `[background]` section, normalized, for the entries that read
    /// the file themselves and hand the knobs to one engine context
    /// (`PipelineSpec`): host-driven entries pass their resolved values as
    /// session params.
    ///
    /// Every key follows v2's `resolveAgentTaskConfig` merge: `[task]` wins
    /// over the legacy `[background]`, and the env bindings win over both.
    pub fn background_limits(&self) -> crate::storage::BackgroundLimits {
        crate::storage::BackgroundLimits {
            kill_grace_period_ms: self.resolve_kill_grace_period_ms(),
            max_running_tasks: self.resolve_background_max_running_tasks(),
            bash_auto_background_on_timeout: self.resolve_bash_auto_background_on_timeout(),
            bash_task_timeout_s: self.resolve_bash_task_timeout_s(),
            keep_alive_on_exit: self.resolve_keep_alive_on_exit(),
        }
    }

    /// Resolve the Native Read tool's character budget (v2
    /// `[read].defaultMaxChars` / `maxChars`). A non-positive file value is
    /// ignored, so the tool's built-in limits stand. No env binding in v2.
    pub fn resolve_read_limits(&self) -> (Option<u64>, Option<u64>) {
        (
            self.read.default_max_chars.filter(|value| *value > 0),
            self.read.max_chars.filter(|value| *value > 0),
        )
    }

    /// Resolve the serialized request byte budget (v2
    /// `llmRequester.requestByteBudget`, env `KIMI_LLM_REQUEST_BYTE_BUDGET`
    /// over the file, positive only). The engine default is 32 MiB
    /// (`DEFAULT_LLM_REQUEST_BYTE_BUDGET`).
    pub fn resolve_llm_request_byte_budget(&self) -> u64 {
        env_positive_u64("KIMI_LLM_REQUEST_BYTE_BUDGET")
            .or(self
                .llm_requester
                .request_byte_budget
                .filter(|value| *value > 0))
            .unwrap_or(DEFAULT_LLM_REQUEST_BYTE_BUDGET)
    }

    /// Resolve the `[sandbox].mode` (v2 `SandboxModeSchema` enum: `off` |
    /// `read-only` | `workspace-write`). An invalid spelling resolves to
    /// `None` — the sandbox stays off, matching v2's section rejection — and
    /// is already reported by [`Self::section_value_warnings`] on load.
    pub fn resolve_sandbox_mode(&self) -> Option<crate::tools::sandbox::SandboxMode> {
        self.sandbox
            .mode
            .as_deref()
            .and_then(|mode| crate::tools::sandbox::SandboxMode::validate(mode).ok())
    }

    /// Resolve the global request-parameter overrides (v2
    /// `[model_overrides]`): env `KIMI_MODEL_TEMPERATURE`, `KIMI_MODEL_TOP_P`,
    /// `KIMI_MODEL_THINKING_KEEP`, `KIMI_MODEL_MAX_COMPLETION_TOKENS` (then
    /// the legacy `KIMI_MODEL_MAX_TOKENS`) over the file. v2's schema does not
    /// bound temperature/topP, so they are passed through as written;
    /// `maxCompletionTokens` is positive-only and a zero is dropped.
    pub fn resolve_model_overrides(&self) -> ResolvedModelOverrides {
        ResolvedModelOverrides {
            temperature: env_f64("KIMI_MODEL_TEMPERATURE").or(self.model_overrides.temperature),
            top_p: env_f64("KIMI_MODEL_TOP_P").or(self.model_overrides.top_p),
            thinking_keep: env_non_empty("KIMI_MODEL_THINKING_KEEP")
                .or_else(|| self.model_overrides.thinking_keep.clone()),
            max_completion_tokens: env_positive_u64("KIMI_MODEL_MAX_COMPLETION_TOKENS")
                .or_else(|| env_positive_u64("KIMI_MODEL_MAX_TOKENS"))
                .and_then(|value| u32::try_from(value).ok())
                .or(self
                    .model_overrides
                    .max_completion_tokens
                    .filter(|value| *value > 0)),
        }
    }

    /// Whether the built-in product skills are installed (v2
    /// `builtinProductSkills`, env `KIMI_CODE_BUILTIN_PRODUCT_SKILLS` over the
    /// file, default on).
    pub fn resolve_builtin_product_skills(&self) -> bool {
        env_bool("KIMI_CODE_BUILTIN_PRODUCT_SKILLS")
            .or(self.builtin_product_skills)
            .unwrap_or(true)
    }

    /// [`KimiConfig::resolve_builtin_product_skills`] on the discovered config
    /// file, for call sites that do not already hold a config. The env switch
    /// is checked first (no file read); an unreadable file falls back to the
    /// documented default.
    pub fn resolved_builtin_product_skills() -> bool {
        if let Some(on) = env_bool("KIMI_CODE_BUILTIN_PRODUCT_SKILLS") {
            return on;
        }
        match Self::discover() {
            Ok((config, _)) => config.builtin_product_skills.unwrap_or(true),
            Err(_) => true,
        }
    }

    /// The user's `extra_skill_dirs` as paths (schema): relative entries
    /// resolve against the current directory.
    pub fn extra_skill_dirs_paths(&self) -> Vec<PathBuf> {
        self.extra_skill_dirs
            .iter()
            .map(std::path::PathBuf::from)
            .collect()
    }

    /// Resolve the `merge_all_available_skills` switch (schema; docs
    /// `config-files.md`): unset means the documented default `true`, so an
    /// untouched file keeps merging every discovered skill directory.
    pub fn resolve_merge_all_available_skills(&self) -> bool {
        self.merge_all_available_skills.unwrap_or(true)
    }

    /// v2 `workspaceTrustRequired` (`workspaceTrust/configSection.ts`,
    /// upstream #4517): whether opening an untrusted workspace asks for trust
    /// confirmation. Unset keeps the gate on — only an explicit `false`
    /// treats every workspace as trusted and skips the prompt.
    pub fn resolve_workspace_require_trust(&self) -> bool {
        self.workspace.require_trust.unwrap_or(true)
    }

    /// Resolve the thinking effort for the wire (schema `thinking.effort`,
    /// stored globally rather than per model): the off-effort when the model
    /// declares one and thinking is disabled, else the chosen effort. With
    /// thinking off and no declared off-effort, the host-level `"off"` token
    /// travels so wires with a protocol-level disable (Kimi's `thinking`
    /// object) can send it; generic wires drop it silently
    /// (`llm::effort::wire_reasoning_effort`).
    pub fn resolve_effort(&self, model_off_effort: Option<&str>) -> Option<String> {
        let enabled = self.thinking.enabled.unwrap_or(true);
        if !enabled {
            return Some(model_off_effort.unwrap_or("off").to_string());
        }
        // v2 `resolveForcedThinkingEffort`: the env-forced effort outranks the
        // chosen one while thinking is on. v2 only offers `forcedEffort` to a
        // trait-driven provider; this engine has no trait gate, so an explicit
        // value applies to every provider (documented in the parity report).
        let forced = env_non_empty("KIMI_MODEL_THINKING_EFFORT")
            .or_else(|| self.thinking.forced_effort.clone());
        forced
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .or_else(|| {
                self.thinking
                    .effort
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
            })
    }

    /// Build a [`PolicySnapshot`] from the configuration.
    pub fn build_policy_snapshot(&self, git_cwd: Option<PathBuf>) -> PolicySnapshot {
        // `[agent].yolo` (or the flat top-level `yolo`) wins, then
        // `[permission].mode`, then the top-level `default_permission_mode`
        // (schema); manual is the fallback.
        let mode = if self.agent.yolo == Some(true) || self.yolo == Some(true) {
            PermissionMode::Yolo
        } else {
            let declared = self
                .permission
                .as_ref()
                .and_then(|p| p.mode.as_deref())
                .or(self.default_permission_mode.as_deref());
            match declared {
                Some("yolo") => PermissionMode::Yolo,
                Some("auto") => PermissionMode::Auto,
                _ => PermissionMode::Manual,
            }
        };

        let mut deny_rules = Vec::new();
        let mut ask_rules = Vec::new();
        let mut allow_rules = Vec::new();

        let mut rule_reasons = std::collections::HashMap::new();
        if let Some(ref p) = self.permission
            && let Some(ref rules) = p.rules
        {
            for r in rules {
                let pattern = match rule_pattern(r) {
                    Some(p) => p,
                    None => continue,
                };
                if let Some(why) = r
                    .reason
                    .as_deref()
                    .map(str::trim)
                    .filter(|why| !why.is_empty())
                {
                    rule_reasons.insert(pattern.clone(), why.to_string());
                }
                match r.decision.as_deref() {
                    Some("deny") => deny_rules.push(pattern),
                    Some("ask") => ask_rules.push(pattern),
                    Some("allow") => allow_rules.push(pattern),
                    _ => {}
                }
            }
        }
        // Legacy `[permission] deny/allow/ask` lists (v2 `permissionFromToml`):
        // appended after `rules` with their implied decision.
        if let Some(ref p) = self.permission {
            for (value, target) in [
                (p.deny.as_ref(), &mut deny_rules),
                (p.ask.as_ref(), &mut ask_rules),
                (p.allow.as_ref(), &mut allow_rules),
            ] {
                if let Some(value) = value {
                    target.extend(legacy_permission_patterns(value));
                }
            }
        }

        // The global `[tools]` switch: an empty pair of lists constrains
        // nothing, so it stays `None` and every tool survives.
        let tools_filter = if self.tools.enabled.is_empty() && self.tools.disabled.is_empty() {
            None
        } else {
            Some(crate::tools::tool_policy::ToolsFilter {
                enabled: self.tools.enabled.clone(),
                disabled: self.tools.disabled.clone(),
            })
        };

        PolicySnapshot {
            mode,
            deny_rules,
            ask_rules,
            allow_rules,
            session_approvals: Vec::new(),
            git_cwd: git_cwd.map(|p| p.to_string_lossy().to_string()),
            tools_filter,
            pre_tool_hooks: self.hooks.clone(),
            rule_reasons,
            non_interactive: false,
            // v2 env binding (`KIMI_CODE_DANGEROUS_COMMAND_GUARD`): only the
            // literal `true` / `false` overrides the file setting; anything
            // else leaves the config in charge (`parseDangerousCommandGuardEnv`).
            dangerous_command_guard: match std::env::var("KIMI_CODE_DANGEROUS_COMMAND_GUARD") {
                Ok(v) if v == "true" => true,
                Ok(v) if v == "false" => false,
                _ => self
                    .permission
                    .as_ref()
                    .and_then(|p| p.dangerous_command_guard)
                    .unwrap_or(true),
            },
        }
    }
}

/// `merge_all_available_skills` as the process config resolves it — `true` when
/// no config file is discoverable (the documented default).
///
/// The prompt builder and the `Skill` tool must read the **same** value: an
/// entry that builds a [`PipelineSpec`] without a loaded config (the addon, the
/// stdio run-turn adapter) resolves it here, so both halves of the pair agree.
///
/// [`PipelineSpec`]: crate::pipeline::PipelineSpec
pub fn resolved_merge_all_available_skills() -> bool {
    KimiConfig::discover()
        .map(|(config, _)| config.resolve_merge_all_available_skills())
        .unwrap_or(true)
}

/// A non-blank trimmed string (v2 `nonBlankEnv` / `nonEmptyString`).
fn non_blank(value: Option<&str>) -> Option<String> {
    let trimmed = value?.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Resolve one `[services.moonshot_*]` entry with its `KIMI_WEB_*` env
/// overlay (v2 `isolateEnvServiceCredentials`): an env base URL is a
/// credential boundary — persisted api keys and custom headers never cross
/// into an env-selected endpoint — otherwise the env api key outranks the
/// persisted one. A missing (or blank) base URL resolves to `None`, leaving
/// the tool on its built-in path.
fn resolve_web_service(
    service: Option<&MoonshotServiceConfig>,
    base_url_env: &str,
    api_key_env: &str,
) -> Option<ResolvedWebService> {
    let env_base_url = non_blank(std::env::var(base_url_env).ok().as_deref());
    let env_api_key = non_blank(std::env::var(api_key_env).ok().as_deref());
    if let Some(base_url) = env_base_url {
        return Some(ResolvedWebService {
            base_url,
            api_key: env_api_key,
            custom_headers: HashMap::new(),
        });
    }
    let service = service?;
    let base_url = non_blank(service.base_url.as_deref())?;
    Some(ResolvedWebService {
        base_url,
        api_key: env_api_key.or_else(|| non_blank(service.api_key.as_deref())),
        custom_headers: service.custom_headers.clone().unwrap_or_default(),
    })
}

/// One provider type's endpoint declaration (v2 `ProtocolEndpoint`,
/// `human/llm/protocol/connection.ts`, with the registrations in
/// `llm-adapter/provider/provider-definition.ts` and
/// `human/llm/provider/providers/standard.ts`). `base_url_env` is the
/// process-environment variable the type falls back to when the config
/// declares no endpoint; `default_base_url` is the vendor SDK's own default,
/// which the engine has to carry itself because it assembles the request URL
/// rather than handing it to an SDK.
struct EndpointDeclaration {
    base_url_env: &'static str,
    default_base_url: Option<&'static str>,
}

/// The endpoint declaration for a provider type, or `None` for a type the
/// engine has no declaration for.
///
/// `vertexai` is deliberately absent: the engine has no Vertex wire protocol
/// — a Vertex provider resolves on the OpenAI one — so honoring
/// `GOOGLE_VERTEX_BASE_URL` here would pull traffic the host layer serves
/// correctly into a mis-shaped request. Vertex stays on the host proxy.
fn endpoint_declaration(provider_type: &str) -> Option<EndpointDeclaration> {
    let declaration = match provider_type {
        // v2 `kimiConnection` (`human/llm-kimi/trait.ts`).
        "kimi" => EndpointDeclaration {
            base_url_env: "KIMI_BASE_URL",
            default_base_url: Some("https://api.moonshot.ai/v1"),
        },
        // v2 `anthropicConnection`; the SDK's own default is the API root.
        "anthropic" => EndpointDeclaration {
            base_url_env: "ANTHROPIC_BASE_URL",
            default_base_url: Some("https://api.anthropic.com"),
        },
        // v2 `openAIConnection`, shared by the Responses protocol.
        "openai" | "openai_responses" | "openai-responses" => EndpointDeclaration {
            base_url_env: "OPENAI_BASE_URL",
            default_base_url: Some("https://api.openai.com/v1"),
        },
        // v2 `geminiEndpoint`; the SDK default is the host root.
        "google" | "google-genai" | "gemini" => EndpointDeclaration {
            base_url_env: "GOOGLE_GEMINI_BASE_URL",
            default_base_url: Some("https://generativelanguage.googleapis.com"),
        },
        _ => return None,
    };
    Some(declaration)
}

/// The base URL a provider falls back to when the config declares none (v2
/// `resolveModelConnection`): the type's `baseUrlEnv` read from the process
/// environment — a blank value counts as unset, same as v2's `read` — then
/// the type's default.
fn endpoint_fallback_base_url(provider_type: &str) -> Option<String> {
    let declaration = endpoint_declaration(provider_type)?;
    non_blank(std::env::var(declaration.base_url_env).ok().as_deref())
        .or_else(|| declaration.default_base_url.map(str::to_string))
}

fn normalize_base_url(url: &str, protocol: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    // GenerateContent lives under an API version segment, and the documented
    // contract is host-root-only — see [`crate::llm::http::google_api_base`],
    // which the endpoint builder also applies.
    if protocol == "google" || protocol == "google-genai" || protocol == "gemini" {
        return crate::llm::http::google_api_base(trimmed);
    }
    if protocol == "anthropic" {
        if trimmed.ends_with("/v1") {
            trimmed.to_string()
        } else {
            format!("{trimmed}/v1")
        }
    } else if trimmed.ends_with("/v1") || trimmed.ends_with("/v1/chat/completions") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/v1")
    }
}

/// Whether a `[models.<alias>]` entry declares thinking support (v2
/// `modelSupportsThinking`): the `thinking` / `always_thinking` capability, or
/// an explicit `adaptive_thinking`. The fork carries `always_thinking` as a
/// capability string rather than a field of its own.
fn model_supports_thinking(alias: Option<&ModelAliasConfig>) -> bool {
    alias.is_some_and(|alias| {
        alias.adaptive_thinking == Some(true)
            || model_capabilities(alias).any(is_thinking_capability)
    })
}

/// Whether the entry declares that it cannot stop reasoning (v2
/// `alwaysThinking`).
fn model_always_thinks(alias: Option<&ModelAliasConfig>) -> bool {
    alias.is_some_and(|alias| {
        model_capabilities(alias).any(|capability| capability == "always_thinking")
    })
}

/// v2 `modelSupportsThinkingEffort(effort, model, true)`: `off` is always
/// accepted, a model without thinking support never is, and an empty effort
/// list means "any effort". Declared efforts are compared verbatim (upstream
/// only normalizes the requested side).
fn model_supports_effort(effort: &str, alias: Option<&ModelAliasConfig>) -> bool {
    if effort == "off" {
        return true;
    }
    if !model_supports_thinking(alias) {
        return false;
    }
    let efforts: Vec<&str> = alias
        .and_then(|alias| alias.support_efforts.as_ref())
        .map(|efforts| {
            efforts
                .iter()
                .map(|effort| effort.trim())
                .filter(|effort| !effort.is_empty())
                .collect()
        })
        .unwrap_or_default();
    efforts.is_empty() || effort == "on" || efforts.contains(&effort)
}

fn model_capabilities(alias: &ModelAliasConfig) -> impl Iterator<Item = &str> {
    alias
        .capabilities
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|capability| capability.trim())
}

fn is_thinking_capability(capability: &str) -> bool {
    capability == "thinking" || capability == "always_thinking"
}

/// The dotted alias a malformed `[models]` entry was probably meant to be: the
/// first nested table it carries, walked to its deepest level. `None` when the
/// entry has no nested table at all.
fn dotted_alias_suffix(alias: &str, entry: &toml::value::Table) -> Option<String> {
    for (key, value) in entry {
        let Some(nested) = value.as_table() else {
            continue;
        };
        return Some(
            dotted_alias_suffix(&format!("{alias}.{key}"), nested)
                .unwrap_or_else(|| format!("{alias}.{key}")),
        );
    }
    None
}

/// Convert one resolved native model into the wire config, applying the
/// pool-level `default_effort` the way the host's `resolveNativeLlmForAlias`
/// does: an anthropic entry gets a thinking budget, an OpenAI-compatible one
/// the `reasoning_effort` passthrough (off values never enable thinking).
/// `thinking_keep` rides every entry, exactly as the host sets it.
///
/// `thinking_budget` stays anthropic-only on purpose. A Google model also
/// needs a numeric budget, but it resolves it from the `reasoning_effort`
/// every non-anthropic protocol already carries, inside the Google wire
/// encoder (`llm/google_genai.rs::thinking_budget_for`, the v2
/// `encodeGoogleGenAIThinking` switch). Encoding it here instead would give
/// the two wires one field and two owners, and — worse — would need a
/// `thinking_budget` on an OpenAI-compatible entry that has no use for one.
fn native_llm_config(
    resolved: ResolvedNativeLlm,
    effort: Option<&str>,
    thinking_keep: Option<&str>,
    model: Option<&ModelAliasConfig>,
) -> NativeLlmConfig {
    let mut llm = NativeLlmConfig {
        protocol: resolved.protocol,
        alias: resolved.alias,
        provider_type: Some(resolved.provider_type).filter(|value| !value.is_empty()),
        base_url: resolved.base_url,
        api_key: resolved.api_key,
        api_key_env: None,
        model: resolved.model,
        max_tokens: resolved.max_output_size.or(resolved.max_tokens),
        custom_headers: Default::default(),
        reasoning_effort: None,
        thinking_budget: None,
        auth_provider: None,
        thinking_keep: thinking_keep.map(str::to_string),
        beta_api: resolved.beta_api,
        capabilities: resolved.capabilities,
        system_prompt: resolved.system_prompt,
        max_input_size: resolved.max_input_size,
        adaptive_thinking: resolved.adaptive_thinking,
        reasoning_key: resolved.reasoning_key,
        off_effort: resolved.off_effort,
        response_format: None,
        sampling: None,
        determinism: None,
        extra_params: None,
        binary_path: resolved.binary_path,
        antigravity: resolved.antigravity,
    };
    if let Some(model) = model {
        llm.response_format = model.response_format.clone();
        llm.sampling = model.sampling.clone();
        llm.determinism = model.determinism.clone();
        llm.extra_params = model.extra_params.clone();
    }
    if let Some(effort) = effort
        && effort != "off"
        && effort != "none"
    {
        if llm.protocol == "anthropic" {
            llm.thinking_budget = Some(match effort {
                "low" => 1024,
                "medium" => 4096,
                "high" | "on" => 32000,
                other => other
                    .parse::<u32>()
                    .ok()
                    .filter(|value| *value > 0)
                    .unwrap_or(32000),
            });
        } else {
            llm.reasoning_effort = Some(effort.to_string());
        }
    }
    // Adaptive anthropic models take the level on `output_config.effort`, not
    // a token budget (v2 `encodeThinking` adaptive mode), so the effort string
    // — including an explicit `off` — is what the encoder needs.
    if llm.protocol == "anthropic" && llm.adaptive_thinking == Some(true) {
        llm.thinking_budget = None;
        llm.reasoning_effort = Some(effort.unwrap_or("off").to_string());
    }
    llm
}

pub(crate) fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

/// Parse a non-negative integer environment variable (v2 `nonNegativeInt`):
/// an invalid value is ignored so a bad entry degrades to the next source
/// instead of failing the session.
fn env_non_negative(name: &str) -> Option<u32> {
    std::env::var(name).ok()?.trim().parse::<u32>().ok()
}

/// The `u64` sibling of [`env_non_negative`] for millisecond timeouts.
fn env_non_negative_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok()?.trim().parse::<u64>().ok()
}

/// The pattern of one `[permission].rules[]` entry (v2
/// `transformPermissionRule`): `pattern` wins, then the legacy
/// `{ tool, match }` shorthand renders `tool(match)` (or the bare tool).
fn rule_pattern(rule: &PermissionRuleConfig) -> Option<String> {
    if let Some(pattern) = rule.pattern.as_deref() {
        return Some(pattern.to_string());
    }
    let tool = rule.tool.as_deref()?;
    let arg = rule.arg_match.as_deref();
    Some(match arg {
        Some(arg) => format!("{tool}({arg})"),
        None => tool.to_string(),
    })
}

/// Fold one legacy `deny` / `allow` / `ask` value into patterns (v2
/// `appendPermissionRules` + `transformPermissionRule`): a bare string is a
/// pattern, a `{ tool, match }` table becomes `tool(match)`, and anything
/// else is skipped.
fn legacy_permission_patterns(value: &toml::Value) -> Vec<String> {
    let entries: Vec<&toml::Value> = match value {
        toml::Value::Array(items) => items.iter().collect(),
        other => vec![other],
    };
    entries
        .into_iter()
        .filter_map(|entry| match entry {
            toml::Value::String(pattern) => Some(pattern.clone()),
            toml::Value::Table(table) => {
                if let Some(pattern) = table.get("pattern").and_then(toml::Value::as_str) {
                    return Some(pattern.to_string());
                }
                let tool = table.get("tool").and_then(toml::Value::as_str)?;
                let arg = table
                    .get("match")
                    .and_then(toml::Value::as_str)
                    .or_else(|| table.get("pattern").and_then(toml::Value::as_str));
                Some(match arg {
                    Some(arg) => format!("{tool}({arg})"),
                    None => tool.to_string(),
                })
            }
            _ => None,
        })
        .collect()
}

/// A boolean environment variable (v2 `parseBooleanEnv`): unset, empty, or
/// unrecognized yields `None` so the next source applies.
fn env_bool(name: &str) -> Option<bool> {
    crate::env::parse_bool_env(std::env::var(name).ok().as_deref())
}

/// A trimmed, non-empty environment variable (v2 `readEnvString`): a blank
/// value counts as unset so the next source applies.
fn env_non_empty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// A finite floating-point environment variable; a non-finite or malformed
/// value is ignored.
fn env_f64(name: &str) -> Option<f64> {
    std::env::var(name)
        .ok()?
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
}

/// v2 `llmRequester` default (`DEFAULT_REQUEST_BYTE_BUDGET`): 32 MiB.
pub const DEFAULT_LLM_REQUEST_BYTE_BUDGET: u64 = 32 * 1024 * 1024;

/// The reserved provider id the env-only model overlay writes
/// (`ENV_MODEL_PROVIDER_KEY`).
pub const ENV_MODEL_PROVIDER_KEY: &str = "__kimi_env__";
/// The reserved model alias the env-only model overlay writes
/// (`ENV_MODEL_ALIAS_KEY`).
pub const ENV_MODEL_ALIAS_KEY: &str = "__kimi_env_model__";
/// v2 `MAX_MCP_TIMEOUT_MS`: the largest accepted MCP timeout.
const MAX_MCP_TIMEOUT_MS: u64 = 2_147_483_647;
/// v2 `envOverlay` default context window for the env model.
const ENV_MODEL_DEFAULT_MAX_CONTEXT_SIZE: u32 = 262_144;
/// v2 `envOverlay` default capabilities for the env model.
const ENV_MODEL_DEFAULT_CAPABILITIES: [&str; 2] = ["image_in", "thinking"];

/// A positive `u32` environment value; anything else fails with the variable
/// named so the deployment log points at the offending key.
fn parse_positive_u32(raw: &str, name: &str) -> Result<u32, String> {
    let value = raw
        .trim()
        .parse::<u32>()
        .map_err(|_| format!("{name} must be a positive integer, got \"{raw}\"."))?;
    if value == 0 {
        return Err(format!("{name} must be a positive integer, got \"{raw}\"."));
    }
    Ok(value)
}

/// A positive integer environment variable (v2 `positiveInt`): zero and
/// invalid values are ignored so the next source applies.
fn env_positive_u64(name: &str) -> Option<u64> {
    env_non_negative_u64(name).filter(|value| *value > 0)
}

/// Warn once per distinct invalid strategy spelling. The resolver runs on
/// every status publish, so an unconditional `warn!` would flood the log; the
/// value cannot change without a process restart (env) or config reload
/// (file), and both are rare.
fn warn_invalid_token_counting_strategy(origin: &str, raw: &str) {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static WARNED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let warned = WARNED.get_or_init(|| Mutex::new(HashSet::new()));
    let mut guard = warned.lock().unwrap_or_else(|e| e.into_inner());
    if guard.insert(format!("{origin}\u{0}{raw}")) {
        tracing::warn!(
            origin,
            value = raw,
            accepted = ?crate::session::token_counting::TokenCountingStrategy::VALUES,
            "invalid token-counting strategy; using measured+estimated"
        );
    }
}

impl KimiConfig {
    /// The native transport config for a resolved provider entry, as the REPL
    /// and the terminal host build it.
    ///
    /// It is deliberately not shared with `server/engine.rs` and `main.rs`:
    /// those two pass the provider's OAuth binding through as `auth_provider`,
    /// while the REPL has always dropped it, and unifying them would change
    /// what the REPL does. Two copies that agree beat four that silently
    /// disagree; picking the right `auth_provider` behaviour is a separate
    /// decision.
    pub fn native_llm_config(&self, def: &ResolvedNativeLlm) -> crate::rpc::types::NativeLlmConfig {
        let mut llm = crate::rpc::types::NativeLlmConfig {
            protocol: def.protocol.clone(),
            alias: def.alias.clone(),
            provider_type: Some(def.provider_type.clone()).filter(|value| !value.is_empty()),
            base_url: def.base_url.clone(),
            api_key: def.api_key.clone(),
            api_key_env: def.api_key_env.clone(),
            model: def.model.clone(),
            max_tokens: def.max_output_size.or(def.max_tokens),
            custom_headers: def.custom_headers.clone(),
            reasoning_effort: self.resolve_effort(def.off_effort.as_deref()),
            thinking_budget: None,
            auth_provider: None,
            thinking_keep: self.resolve_thinking_keep(),
            beta_api: def.beta_api,
            capabilities: def.capabilities.clone(),
            system_prompt: def.system_prompt.clone(),
            max_input_size: def.max_input_size,
            adaptive_thinking: def.adaptive_thinking,
            reasoning_key: def.reasoning_key.clone(),
            off_effort: None,
            response_format: None,
            sampling: None,
            determinism: None,
            extra_params: None,
            binary_path: def.binary_path.clone(),
            antigravity: def.antigravity.clone(),
        };
        self.apply_model_request_options(&mut llm, def.alias.as_deref());
        llm
    }

    /// Copy the `[models.<alias>]` request-body knobs onto a transport config.
    ///
    /// The transport is built in several places (`main.rs`, `server/engine.rs`,
    /// the REPL and the secondary/multi pools); this is the one place that
    /// reads the knobs, so all of them agree and an absent entry leaves the
    /// request body untouched.
    pub fn apply_model_request_options(
        &self,
        llm: &mut crate::rpc::types::NativeLlmConfig,
        alias: Option<&str>,
    ) {
        let Some(model) = alias.and_then(|alias| self.effective_model_for(alias)) else {
            return;
        };
        llm.response_format = model.response_format;
        llm.sampling = model.sampling;
        llm.determinism = model.determinism;
        llm.extra_params = model.extra_params;
    }
}

#[cfg(test)]
mod tests;
