use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::env::env_switch_default_off;
use crate::environment::EnvironmentsConfig;
use crate::permission::{HookDef, PermissionMode, PolicySnapshot};
use crate::agent::permission_rules::config_section::{legacy_permission_patterns, rule_pattern};
pub use crate::agent::permission_rules::config_section::{
    LegacyPermissionConfig as PermissionConfig, PermissionRuleConfig,
};
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

    #[serde(default)]
    pub oauth: Option<serde_json::Value>,

    #[serde(default)]
    pub source: Option<serde_json::Value>,

    #[serde(rename = "custom_headers", alias = "customHeaders", default)]
    pub custom_headers: Option<HashMap<String, String>>,

    #[serde(rename = "binary_path", alias = "binaryPath", default)]
    pub binary_path: Option<String>,

    #[serde(default)]
    pub antigravity: Option<crate::rpc::types::AntigravityLimits>,

    #[serde(rename = "model_source", alias = "modelSource", default)]
    pub model_source: Option<String>,

    #[serde(default)]
    pub env: Option<HashMap<String, String>>,
}

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

    #[serde(rename = "provider_id", alias = "providerId", default)]
    pub provider_id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,

    #[serde(default)]
    pub name: Option<String>,

    #[serde(default)]
    pub aliases: Option<Vec<String>>,

    #[serde(rename = "api_key", alias = "apiKey", default)]
    pub api_key: Option<String>,

    #[serde(default)]
    pub oauth: Option<serde_json::Value>,
    #[serde(rename = "system_prompt", alias = "systemPrompt", default)]
    pub system_prompt: Option<String>,

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

    #[serde(default)]
    pub protocol: Option<String>,

    #[serde(rename = "beta_api", alias = "betaApi", default)]
    pub beta_api: Option<bool>,

    #[serde(rename = "base_url", alias = "baseUrl", default)]
    pub base_url: Option<String>,

    #[serde(rename = "max_input_size", alias = "maxInputSize", default)]
    pub max_input_size: Option<u32>,

    #[serde(rename = "reasoning_key", alias = "reasoningKey", default)]
    pub reasoning_key: Option<String>,

    #[serde(rename = "off_effort", alias = "offEffort", default)]
    pub off_effort: Option<String>,

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

    #[serde(default)]
    pub upgrade: Option<ModelUpgradeConfig>,

    #[serde(default)]
    pub overrides: Option<ModelOverrideConfig>,
}

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

    #[serde(rename = "plan_mode", alias = "planMode", default)]
    pub plan_mode: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelCatalogConfig {
    #[serde(rename = "refresh_interval_ms", alias = "refreshIntervalMs", default)]
    pub refresh_interval_ms: Option<u64>,

    #[serde(rename = "refresh_on_start", alias = "refreshOnStart", default)]
    pub refresh_on_start: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ShellConfig {
    #[serde(default)]
    pub preference: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DefaultPermissionMode {
    Manual,
    Auto,
    Yolo,
}

impl DefaultPermissionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Auto => "auto",
            Self::Yolo => "yolo",
        }
    }
}

impl std::str::FromStr for DefaultPermissionMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "manual" => Ok(Self::Manual),
            "auto" => Ok(Self::Auto),
            "yolo" => Ok(Self::Yolo),
            _ => Err(format!("invalid defaultPermissionMode '{value}'")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DefaultWorkspaceConfig {
    #[serde(rename = "path", default)]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WorkspaceConfig {
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

    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub headers: Option<HashMap<String, String>>,

    #[serde(default, alias = "bearerTokenEnvVar")]
    pub bearer_token_env_var: Option<String>,

    #[serde(default)]
    pub auth: Option<String>,

    #[serde(default)]
    pub transport: Option<String>,

    #[serde(default)]
    pub enabled: Option<bool>,

    #[serde(default, alias = "enabledTools")]
    pub enabled_tools: Option<Vec<String>>,

    #[serde(default, alias = "disabledTools")]
    pub disabled_tools: Option<Vec<String>>,

    #[serde(default, alias = "startupTimeoutMs")]
    pub startup_timeout_ms: Option<u64>,

    #[serde(default, alias = "toolTimeoutMs")]
    pub tool_timeout_ms: Option<u64>,

    #[serde(default)]
    pub executor: Option<String>,

    #[serde(default, alias = "environmentId")]
    pub environment_id: Option<String>,

    #[serde(default)]
    pub deferred: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct McpConfig {
    #[serde(default, alias = "startupTimeoutMs")]
    pub startup_timeout_ms: Option<u64>,
    #[serde(default, alias = "toolTimeoutMs")]
    pub tool_timeout_ms: Option<u64>,

    #[serde(default, alias = "defaultDeferred")]
    pub default_deferred: Option<bool>,

    #[serde(default, alias = "reconnectRetryEnabled")]
    pub reconnect_retry_enabled: Option<bool>,

    #[serde(default, alias = "reconnectInitialDelayMs")]
    pub reconnect_initial_delay_ms: Option<u64>,

    #[serde(default, alias = "reconnectMaxDelayMs")]
    pub reconnect_max_delay_ms: Option<u64>,

    #[serde(default, alias = "autoDeferToolThreshold")]
    pub auto_defer_tool_threshold: Option<u64>,

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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MoonshotServiceConfig {
    #[serde(rename = "base_url", alias = "baseUrl", default)]
    pub base_url: Option<String>,
    #[serde(rename = "api_key", alias = "apiKey", default)]
    pub api_key: Option<String>,
    #[serde(rename = "custom_headers", alias = "customHeaders", default)]
    pub custom_headers: Option<HashMap<String, String>>,

    #[serde(default)]
    pub oauth: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ServicesConfig {
    #[serde(rename = "moonshot_search", alias = "moonshotSearch", default)]
    pub moonshot_search: Option<MoonshotServiceConfig>,
    #[serde(rename = "moonshot_fetch", alias = "moonshotFetch", default)]
    pub moonshot_fetch: Option<MoonshotServiceConfig>,
    #[serde(rename = "bing_api", alias = "bingApi", default)]
    pub bing_api: Option<MoonshotServiceConfig>,
}

#[derive(Debug, Clone)]
pub struct ResolvedWebService {
    pub base_url: String,
    pub api_key: Option<String>,
    pub custom_headers: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ImageConfig {
    #[serde(rename = "read_byte_budget", alias = "readByteBudget", default)]
    pub read_byte_budget: Option<u64>,
    #[serde(rename = "max_edge_px", alias = "maxEdgePx", default)]
    pub max_edge_px: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReadConfig {
    #[serde(rename = "default_max_chars", alias = "defaultMaxChars", default)]
    pub default_max_chars: Option<u64>,

    #[serde(rename = "max_chars", alias = "maxChars", default)]
    pub max_chars: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LlmRequesterConfig {
    #[serde(rename = "request_byte_budget", alias = "requestByteBudget", default)]
    pub request_byte_budget: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelOverridesConfig {
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(rename = "top_p", alias = "topP", default)]
    pub top_p: Option<f64>,

    #[serde(rename = "thinking_keep", alias = "thinkingKeep", default)]
    pub thinking_keep: Option<String>,

    #[serde(
        rename = "max_completion_tokens",
        alias = "maxCompletionTokens",
        default
    )]
    pub max_completion_tokens: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResolvedModelOverrides {
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub thinking_keep: Option<String>,
    pub max_completion_tokens: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CronConfig {
    #[serde(default)]
    pub disabled: Option<bool>,

    #[serde(rename = "no_jitter", alias = "noJitter", default)]
    pub no_jitter: Option<bool>,

    #[serde(rename = "no_stale", alias = "noStale", default)]
    pub no_stale: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SubagentConfig {
    #[serde(rename = "timeout_ms", alias = "timeoutMs", default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SwarmConfig {
    #[serde(rename = "timeout_ms", alias = "timeoutMs", default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SecondaryModelConfig {
    #[serde(rename = "default_model", default)]
    pub default_model: Option<String>,

    #[serde(default)]
    pub models: Option<HashMap<String, String>>,
    #[serde(default)]
    pub force: Option<bool>,

    #[serde(default)]
    pub model: Option<String>,

    #[serde(rename = "default_effort", default)]
    pub default_effort: Option<String>,
}

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

    #[serde(
        rename = "compaction_max_attempts",
        alias = "compactionMaxAttempts",
        default
    )]
    pub compaction_max_attempts: Option<u32>,

    #[serde(
        rename = "reserved_context_size",
        alias = "reservedContextSize",
        default
    )]
    pub reserved_context_size: Option<u32>,

    #[serde(rename = "max_ralph_iterations", alias = "maxRalphIterations", default)]
    pub max_ralph_iterations: Option<i64>,

    #[serde(
        rename = "compaction_trigger_ratio",
        alias = "compactionTriggerRatio",
        default
    )]
    pub compaction_trigger_ratio: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ThinkingConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub keep: Option<String>,

    #[serde(default)]
    pub effort: Option<String>,

    #[serde(rename = "forced_effort", alias = "forcedEffort", default)]
    pub forced_effort: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ExperimentalValue {
    Bool(bool),
    String(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BackgroundConfig {
    #[serde(rename = "max_running_tasks", alias = "maxRunningTasks", default)]
    pub max_running_tasks: Option<u32>,

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

    #[serde(rename = "max_settled_tasks", alias = "maxSettledTasks", default)]
    pub max_settled_tasks: Option<u32>,

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

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct OperationLogConfig {
    #[serde(
        rename = "maxOperations",
        alias = "max_operations",
        default,
        deserialize_with = "deserialize_positive_safe_u64"
    )]
    pub max_operations: Option<u64>,
    #[serde(
        rename = "maxAgeMs",
        alias = "max_age_ms",
        default,
        deserialize_with = "deserialize_non_negative_safe_u64"
    )]
    pub max_age_ms: Option<u64>,
    #[serde(
        rename = "maxTotalBytes",
        alias = "max_total_bytes",
        default,
        deserialize_with = "deserialize_positive_safe_u64"
    )]
    pub max_total_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolsConfig {
    #[serde(default)]
    pub enabled: Vec<String>,
    #[serde(default)]
    pub disabled: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TokenCountingConfig {
    #[serde(default)]
    pub strategy: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct KimiConfig {
    #[serde(rename = "default_model", default)]
    pub default_model: Option<String>,

    #[serde(rename = "default_provider", default)]
    pub default_provider: Option<String>,
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
    #[serde(default)]
    pub models: HashMap<String, ModelAliasConfig>,
    #[serde(default)]
    pub agent: AgentConfig,

    #[serde(default)]
    pub shell: ShellConfig,

    #[serde(rename = "loop_control", default)]
    pub loop_control: LoopControlConfig,

    #[serde(default)]
    pub thinking: ThinkingConfig,

    #[serde(rename = "tokenCounting", alias = "token_counting", default)]
    pub token_counting: TokenCountingConfig,
    #[serde(default)]
    pub permission: Option<PermissionConfig>,

    #[serde(default)]
    pub workspace: WorkspaceConfig,

    #[serde(rename = "default_workspace", alias = "defaultWorkspace", default)]
    pub default_workspace: DefaultWorkspaceConfig,

    #[serde(default)]
    pub environments: EnvironmentsConfig,
    #[serde(rename = "mcp_servers", default)]
    pub mcp_servers: HashMap<String, McpServerConfig>,

    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub github: GitHubConfig,

    #[serde(default)]
    pub services: ServicesConfig,

    #[serde(default)]
    pub subagent: SubagentConfig,

    #[serde(default)]
    pub swarm: SwarmConfig,

    #[serde(default)]
    pub image: ImageConfig,

    #[serde(default)]
    pub read: ReadConfig,

    #[serde(rename = "llm_requester", alias = "llmRequester", default)]
    pub llm_requester: LlmRequesterConfig,

    #[serde(rename = "model_overrides", alias = "modelOverrides", default)]
    pub model_overrides: ModelOverridesConfig,

    #[serde(
        rename = "builtin_product_skills",
        alias = "builtinProductSkills",
        default
    )]
    pub builtin_product_skills: Option<bool>,

    #[serde(default)]
    pub cron: CronConfig,

    #[serde(rename = "secondary_model", default)]
    pub secondary_model: Option<SecondaryModelConfig>,

    #[serde(default)]
    pub hooks: Vec<HookDef>,

    #[serde(default)]
    pub tools: ToolsConfig,

    #[serde(default)]
    pub background: BackgroundConfig,

    #[serde(default)]
    pub task: TaskConfig,

    #[serde(default)]
    pub experimental: HashMap<String, ExperimentalValue>,

    #[serde(rename = "operationLog", alias = "operation_log", default)]
    pub operation_log: OperationLogConfig,

    #[serde(
        rename = "default_permission_mode",
        alias = "defaultPermissionMode",
        default
    )]
    pub default_permission_mode: Option<DefaultPermissionMode>,

    #[serde(default)]
    pub yolo: Option<bool>,

    #[serde(rename = "plan_mode", alias = "planMode", default)]
    pub plan_mode: Option<bool>,
    #[serde(rename = "default_plan_mode", alias = "defaultPlanMode", default)]
    pub default_plan_mode: Option<bool>,

    #[serde(rename = "model_catalog", alias = "modelCatalog", default)]
    pub model_catalog: Option<ModelCatalogConfig>,

    #[serde(default)]
    pub telemetry: Option<bool>,

    #[serde(rename = "auto_session_title", alias = "autoSessionTitle", default)]
    pub auto_session_title: Option<bool>,

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

    #[serde(skip)]
    pub config_warnings: Vec<String>,

    #[serde(flatten, skip_serializing, default)]
    pub(crate) unknown_sections: HashMap<String, toml::Value>,

    #[serde(skip)]
    pub(crate) dropped_sections: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedNativeLlm {
    pub protocol: String,

    #[serde(default)]
    pub alias: Option<String>,

    #[serde(default)]
    pub provider_type: String,
    pub base_url: String,
    pub api_key: String,

    #[serde(default)]
    pub api_key_env: Option<String>,

    #[serde(default)]
    pub auth_provider: Option<String>,
    pub model: String,
    pub max_tokens: Option<u32>,

    #[serde(default)]
    pub capabilities: Option<Vec<String>>,

    #[serde(default)]
    pub custom_headers: HashMap<String, String>,

    #[serde(default)]
    pub beta_api: bool,

    #[serde(default)]
    pub system_prompt: Option<String>,

    #[serde(default)]
    pub max_output_size: Option<u32>,

    #[serde(default)]
    pub max_input_size: Option<u32>,

    #[serde(default)]
    pub adaptive_thinking: Option<bool>,

    #[serde(default)]
    pub reasoning_key: Option<String>,

    #[serde(default)]
    pub off_effort: Option<String>,

    #[serde(default)]
    pub(crate) binary_path: Option<String>,

    #[serde(default)]
    pub(crate) antigravity: Option<crate::rpc::types::AntigravityLimits>,
}

impl ResolvedNativeLlm {
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

    fn environment_warnings(&self) -> Vec<String> {
        match self.environments.validate() {
            Ok(_) => Vec::new(),
            Err(error) => vec![format!(
                "Invalid config section 'environments': {}; new sessions will fail until it is fixed",
                error.detail()
            )],
        }
    }

    pub fn from_file(path: &Path) -> Result<Self, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read config at {}: {e}", path.display()))?;
        let (validated, mut warnings, dropped) = Self::validated_toml(&content)?;
        let mut config: Self = validated.parse()?;
        config.dropped_sections = dropped;

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

    pub fn discover() -> Result<(Self, PathBuf), String> {
        if let Some(explicit) = std::env::var_os("KIMI_CONFIG_PATH") {
            let path = PathBuf::from(explicit);
            if path.is_file() {
                let config = Self::load_or_default(&path);
                return Ok((config, path));
            }
        }

        if let Some(code_home) = std::env::var_os("KIMI_CODE_HOME") {
            let path = PathBuf::from(code_home).join("config.toml");
            if path.is_file() {
                let config = Self::load_or_default(&path);
                return Ok((config, path));
            }
        }

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

    pub fn load_or_default(path: &Path) -> Self {
        match Self::from_file(path) {
            Ok(mut config) => {
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

    pub fn extract_native_llm(&self, target_model: Option<&str>) -> Option<ResolvedNativeLlm> {
        let model_key = target_model
            .or(self.default_model.as_deref())
            .unwrap_or("default");

        let entry = self.models.get_key_value(model_key).or_else(|| {
            self.models.iter().find(|(_, entry)| {
                entry
                    .aliases
                    .as_ref()
                    .is_some_and(|names| names.iter().any(|name| name == model_key))
            })
        });

        let (provider_name, wire_model) = if let Some((key, alias)) = entry {
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

        let effective = entry.map(|(_, alias)| alias.effective());
        let alias = effective.as_ref();

        let p_type = provider
            .provider_type
            .as_deref()
            .unwrap_or("openai")
            .to_lowercase();

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
            (String::new(), None, None)
        } else {
            let env_name = provider.api_key_env.as_deref().filter(|e| !e.is_empty())?;
            (String::new(), None, Some(env_name.to_string()))
        };

        let p_type = provider
            .provider_type
            .as_deref()
            .unwrap_or("openai")
            .to_lowercase();

        let protocol = match alias.and_then(|alias| alias.protocol.as_deref()) {
            Some("anthropic") => "anthropic",
            Some("openai_responses") => "openai_responses",
            Some("google") | Some("google-genai") | Some("gemini") | Some("vertexai") => {
                "google-genai"
            }

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

    pub fn extract_secondary_model_pool(
        &self,
        target_model: Option<&str>,
    ) -> Result<Option<SecondaryModelPool>, String> {
        let Some(section) = self.secondary_model.as_ref() else {
            return Ok(None);
        };

        if section.force == Some(true) {
            tracing::warn!(
                "[secondary_model] 'force' is no longer supported and is ignored: a lone \
                 default_model already pins every Agent and AgentSwarm subagent to that model, \
                 and a [secondary_model.models] table only offers its own entries. Remove the key."
            );
        }
        let table = section.models.as_ref();

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

    pub fn resolve_max_attempts_per_step(&self) -> Option<u32> {
        env_non_negative("KIMI_LOOP_MAX_ATTEMPTS_PER_STEP")
            .or_else(|| env_non_negative("KIMI_LOOP_MAX_RETRIES_PER_STEP"))
            .or(self.loop_control.max_attempts_per_step)
            .or(self.loop_control.max_retries_per_step)
    }

    pub fn resolve_max_steps_per_turn(&self) -> Option<u32> {
        env_non_negative("KIMI_LOOP_MAX_STEPS_PER_TURN")
            .or(self.loop_control.max_steps_per_turn)
            .or(self.loop_control.max_steps_per_run)
            .filter(|value| *value > 0)
    }

    pub fn resolve_compaction_max_attempts(&self) -> Option<u32> {
        self.loop_control
            .compaction_max_attempts
            .filter(|value| *value > 0)
    }

    #[allow(dead_code)]
    pub fn resolve_compaction_trigger_ratio(&self) -> Option<f64> {
        self.loop_control
            .compaction_trigger_ratio
            .filter(|value| value.is_finite() && (0.5..=0.99).contains(value))
    }

    #[allow(dead_code)]
    pub fn resolve_reserved_context_size(&self) -> Option<u32> {
        self.loop_control.reserved_context_size
    }

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

    pub fn resolve_web_search_service(&self) -> Option<ResolvedWebService> {
        resolve_web_service(
            self.services.moonshot_search.as_ref(),
            "KIMI_WEB_SEARCH_BASE_URL",
            "KIMI_WEB_SEARCH_API_KEY",
        )
    }

    pub fn resolve_web_fetch_service(&self) -> Option<ResolvedWebService> {
        resolve_web_service(
            self.services.moonshot_fetch.as_ref(),
            "KIMI_WEB_FETCH_BASE_URL",
            "KIMI_WEB_FETCH_API_KEY",
        )
    }

    pub fn resolve_bing_api_service(&self) -> Option<ResolvedWebService> {
        resolve_web_service(
            self.services.bing_api.as_ref(),
            "KIMI_BING_BASE_URL",
            "KIMI_BING_API_KEY",
        )
    }

    pub fn resolve_subagent_timeout_ms(&self) -> Option<u64> {
        env_non_negative_u64("KIMI_SUBAGENT_TIMEOUT_MS").or(self.subagent.timeout_ms)
    }

    pub fn resolve_swarm_timeout_ms(&self) -> Option<u64> {
        env_non_negative_u64("KIMI_CODE_SWARM_TIMEOUT_MS").or(self.swarm.timeout_ms)
    }

    pub fn cron_disabled(&self) -> bool {
        env_switch_default_off("KIMI_DISABLE_CRON") || self.cron.disabled.unwrap_or(false)
    }

    pub fn cron_no_jitter(&self) -> bool {
        env_switch_default_off("KIMI_CRON_NO_JITTER") || self.cron.no_jitter.unwrap_or(false)
    }

    pub fn cron_no_stale(&self) -> bool {
        env_switch_default_off("KIMI_CRON_NO_STALE") || self.cron.no_stale.unwrap_or(false)
    }

    pub fn resolve_image_read_byte_budget(&self) -> Option<u64> {
        env_positive_u64("KIMI_IMAGE_READ_BYTE_BUDGET")
            .or(self.image.read_byte_budget.filter(|value| *value > 0))
    }

    pub fn resolve_image_max_edge_px(&self) -> Option<u32> {
        env_positive_u64("KIMI_IMAGE_MAX_EDGE_PX")
            .and_then(|value| u32::try_from(value).ok())
            .or(self.image.max_edge_px.filter(|value| *value > 0))
    }

    fn configured_max_running_tasks(&self) -> Option<u32> {
        self.task
            .max_running_tasks
            .filter(|value| *value > 0)
            .or(self.background.max_running_tasks.filter(|value| *value > 0))
    }

    pub fn resolve_background_max_running_tasks(&self) -> Option<u32> {
        env_positive_u64("KIMI_CODE_BACKGROUND_MAX_RUNNING_TASKS")
            .and_then(|value| u32::try_from(value).ok())
            .or_else(|| self.configured_max_running_tasks())
    }

    pub fn resolve_bash_task_timeout_s(&self) -> Option<u64> {
        env_non_negative_u64("KIMI_CODE_BACKGROUND_BASH_TASK_TIMEOUT_S")
            .or(self.task.bash_task_timeout_s)
            .or(self.background.bash_task_timeout_s)
    }

    pub fn resolve_keep_alive_on_exit(&self) -> Option<bool> {
        env_bool("KIMI_CODE_BACKGROUND_KEEP_ALIVE_ON_EXIT")
            .or(self.task.keep_alive_on_exit)
            .or(self.background.keep_alive_on_exit)
    }

    pub fn resolve_bash_auto_background_on_timeout(&self) -> Option<bool> {
        self.task
            .bash_auto_background_on_timeout
            .or(self.background.bash_auto_background_on_timeout)
    }

    pub fn resolve_kill_grace_period_ms(&self) -> Option<u64> {
        self.task
            .kill_grace_period_ms
            .or(self.background.kill_grace_period_ms)
    }

    pub fn resolve_max_settled_tasks(&self) -> Option<u32> {
        self.task
            .max_settled_tasks
            .filter(|value| *value > 0)
            .or(self.background.max_settled_tasks.filter(|value| *value > 0))
    }

    pub fn resolve_print_wait_ceiling_s(&self) -> Option<u64> {
        env_positive_u64("KIMI_CODE_BACKGROUND_PRINT_WAIT_CEILING_S")
            .or(self.task.print_wait_ceiling_s.filter(|value| *value > 0))
            .or(self
                .background
                .print_wait_ceiling_s
                .filter(|value| *value > 0))
    }

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

    pub fn resolve_print_max_turns(&self) -> Option<u32> {
        env_positive_u64("KIMI_CODE_BACKGROUND_PRINT_MAX_TURNS")
            .and_then(|value| u32::try_from(value).ok())
            .or(self.task.print_max_turns.filter(|value| *value > 0))
            .or(self.background.print_max_turns.filter(|value| *value > 0))
    }

    pub fn background_limits(&self) -> crate::storage::BackgroundLimits {
        crate::storage::BackgroundLimits {
            kill_grace_period_ms: self.resolve_kill_grace_period_ms(),
            max_running_tasks: self.resolve_background_max_running_tasks(),
            max_settled_tasks: self.resolve_max_settled_tasks(),
            bash_auto_background_on_timeout: self.resolve_bash_auto_background_on_timeout(),
            bash_task_timeout_s: self.resolve_bash_task_timeout_s(),
            keep_alive_on_exit: self.resolve_keep_alive_on_exit(),
        }
    }

    pub fn resolve_read_limits(&self) -> (Option<u64>, Option<u64>) {
        (
            self.read.default_max_chars.filter(|value| *value > 0),
            self.read.max_chars.filter(|value| *value > 0),
        )
    }

    pub fn resolve_llm_request_byte_budget(&self) -> u64 {
        env_positive_u64("KIMI_LLM_REQUEST_BYTE_BUDGET")
            .or(self
                .llm_requester
                .request_byte_budget
                .filter(|value| *value > 0))
            .unwrap_or(DEFAULT_LLM_REQUEST_BYTE_BUDGET)
    }

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

    pub fn resolve_builtin_product_skills(&self) -> bool {
        env_bool("KIMI_CODE_BUILTIN_PRODUCT_SKILLS")
            .or(self.builtin_product_skills)
            .unwrap_or(true)
    }

    pub fn resolved_builtin_product_skills() -> bool {
        if let Some(on) = env_bool("KIMI_CODE_BUILTIN_PRODUCT_SKILLS") {
            return on;
        }
        match Self::discover() {
            Ok((config, _)) => config.builtin_product_skills.unwrap_or(true),
            Err(_) => true,
        }
    }

    pub fn extra_skill_dirs_paths(&self) -> Vec<PathBuf> {
        self.extra_skill_dirs
            .iter()
            .map(std::path::PathBuf::from)
            .collect()
    }

    pub fn resolve_merge_all_available_skills(&self) -> bool {
        self.merge_all_available_skills.unwrap_or(true)
    }

    pub fn resolve_workspace_require_trust(&self) -> bool {
        self.workspace.require_trust.unwrap_or(true)
    }

    pub fn resolve_effort(&self, model_off_effort: Option<&str>) -> Option<String> {
        let enabled = self.thinking.enabled.unwrap_or(true);
        if !enabled {
            return Some(model_off_effort.unwrap_or("off").to_string());
        }

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

    pub fn build_policy_snapshot(&self, git_cwd: Option<PathBuf>) -> PolicySnapshot {
        let mode = if self.agent.yolo == Some(true) || self.yolo == Some(true) {
            PermissionMode::Yolo
        } else {
            let declared = self
                .permission
                .as_ref()
                .and_then(|p| p.mode.as_deref())
                .or(self
                    .default_permission_mode
                    .map(DefaultPermissionMode::as_str));
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
                if r.scope.as_deref() == Some("session-runtime") {
                    continue;
                }
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

pub fn resolved_merge_all_available_skills() -> bool {
    KimiConfig::discover()
        .map(|(config, _)| config.resolve_merge_all_available_skills())
        .unwrap_or(true)
}

fn non_blank(value: Option<&str>) -> Option<String> {
    let trimmed = value?.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

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

struct EndpointDeclaration {
    base_url_env: &'static str,
    default_base_url: Option<&'static str>,
}

fn endpoint_declaration(provider_type: &str) -> Option<EndpointDeclaration> {
    let declaration = match provider_type {
        "kimi" => EndpointDeclaration {
            base_url_env: "KIMI_BASE_URL",
            default_base_url: Some("https://api.moonshot.ai/v1"),
        },

        "anthropic" => EndpointDeclaration {
            base_url_env: "ANTHROPIC_BASE_URL",
            default_base_url: Some("https://api.anthropic.com"),
        },

        "openai" | "openai_responses" | "openai-responses" => EndpointDeclaration {
            base_url_env: "OPENAI_BASE_URL",
            default_base_url: Some("https://api.openai.com/v1"),
        },

        "google" | "google-genai" | "gemini" => EndpointDeclaration {
            base_url_env: "GOOGLE_GEMINI_BASE_URL",
            default_base_url: Some("https://generativelanguage.googleapis.com"),
        },
        _ => return None,
    };
    Some(declaration)
}

fn endpoint_fallback_base_url(provider_type: &str) -> Option<String> {
    let declaration = endpoint_declaration(provider_type)?;
    non_blank(std::env::var(declaration.base_url_env).ok().as_deref())
        .or_else(|| declaration.default_base_url.map(str::to_string))
}

fn normalize_base_url(url: &str, protocol: &str) -> String {
    let trimmed = url.trim_end_matches('/');

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

fn model_supports_thinking(alias: Option<&ModelAliasConfig>) -> bool {
    alias.is_some_and(|alias| {
        alias.adaptive_thinking == Some(true)
            || model_capabilities(alias).any(is_thinking_capability)
    })
}

fn model_always_thinks(alias: Option<&ModelAliasConfig>) -> bool {
    alias.is_some_and(|alias| {
        model_capabilities(alias).any(|capability| capability == "always_thinking")
    })
}

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

pub const OPERATION_LOG_MAX_OPERATIONS_ENV: &str = "KIMI_CODE_OPERATION_LOG_MAX_OPERATIONS";
pub const OPERATION_LOG_MAX_AGE_MS_ENV: &str = "KIMI_CODE_OPERATION_LOG_MAX_AGE_MS";
pub const OPERATION_LOG_MAX_TOTAL_BYTES_ENV: &str = "KIMI_CODE_OPERATION_LOG_MAX_TOTAL_BYTES";

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

pub fn resolve_operation_log_retention(config: &KimiConfig) -> OperationLogConfig {
    OperationLogConfig {
        max_operations: env_safe_u64(OPERATION_LOG_MAX_OPERATIONS_ENV, true)
            .or(config.operation_log.max_operations),
        max_age_ms: env_safe_u64(OPERATION_LOG_MAX_AGE_MS_ENV, false)
            .or(config.operation_log.max_age_ms),
        max_total_bytes: env_safe_u64(OPERATION_LOG_MAX_TOTAL_BYTES_ENV, true)
            .or(config.operation_log.max_total_bytes),
    }
}

fn deserialize_positive_safe_u64<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<u64>::deserialize(deserializer)?;
    if value.is_some_and(|value| value == 0 || value > MAX_SAFE_INTEGER) {
        return Err(serde::de::Error::custom("expected a positive safe integer"));
    }
    Ok(value)
}

fn deserialize_non_negative_safe_u64<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<u64>::deserialize(deserializer)?;
    if value.is_some_and(|value| value > MAX_SAFE_INTEGER) {
        return Err(serde::de::Error::custom("expected a non-negative safe integer"));
    }
    Ok(value)
}

fn env_safe_u64(name: &str, positive: bool) -> Option<u64> {
    let raw = std::env::var(name).ok()?;
    let value = raw.trim();
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let parsed = value.parse::<u64>().ok()?;
    if parsed > MAX_SAFE_INTEGER || (positive && parsed == 0) {
        return None;
    }
    Some(parsed)
}

fn env_non_negative(name: &str) -> Option<u32> {
    std::env::var(name).ok()?.trim().parse::<u32>().ok()
}

fn env_non_negative_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok()?.trim().parse::<u64>().ok()
}

fn env_bool(name: &str) -> Option<bool> {
    crate::env::parse_bool_env(std::env::var(name).ok().as_deref())
}

fn env_non_empty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn env_f64(name: &str) -> Option<f64> {
    std::env::var(name)
        .ok()?
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
}

pub const DEFAULT_LLM_REQUEST_BYTE_BUDGET: u64 = 32 * 1024 * 1024;

pub const ENV_MODEL_PROVIDER_KEY: &str = "__kimi_env__";

pub const ENV_MODEL_ALIAS_KEY: &str = "__kimi_env_model__";

const MAX_MCP_TIMEOUT_MS: u64 = 2_147_483_647;

const ENV_MODEL_DEFAULT_MAX_CONTEXT_SIZE: u32 = 262_144;

const ENV_MODEL_DEFAULT_CAPABILITIES: [&str; 2] = ["image_in", "thinking"];

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

fn env_positive_u64(name: &str) -> Option<u64> {
    env_non_negative_u64(name).filter(|value| *value > 0)
}

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
