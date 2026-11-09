use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::Arc;

// ── Message content blocks (multimodal) ─────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Image,
    Video,
    Audio,
}

impl MediaKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Video => "video",
            Self::Audio => "audio",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        meta: Option<serde_json::Map<String, serde_json::Value>>,
    },
    Image {
        media_type: String,
        data: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    MediaRef { file_id: String, kind: MediaKind },
    ImageUrl {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    AudioUrl {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    VideoUrl {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    Think {
        think: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        encrypted: Option<String>,
        #[serde(
            rename = "detailsIndex",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        details_index: Option<u32>,
        #[serde(
            rename = "reasoningKey",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        reasoning_key: Option<String>,
        #[serde(
            rename = "detailsSummary",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        details_summary: Option<String>,
    },
}

// ── Native LLM configuration (Rust-side HTTP transport) ───────────────────

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NativeResponseFormat {
    JsonObject,
    JsonSchema {
        #[serde(rename = "jsonSchema")]
        json_schema: NativeJsonSchema,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NativeJsonSchema {
    pub name: String,
    pub schema: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeSampling {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NativeDeterminism {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeExtraParams {
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub openai: serde_json::Map<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub responses: serde_json::Map<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub anthropic: serde_json::Map<String, serde_json::Value>,
}

#[derive(Clone, Default, Deserialize, Serialize)]
pub struct NativeLlmConfig {
    pub protocol: String,
    #[serde(default)]
    pub alias: Option<String>,
    #[serde(default)]
    pub provider_type: Option<String>,
    pub base_url: String,
    pub api_key: String,
    #[serde(default)]
    pub api_key_env: Option<String>,
    pub model: String,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub custom_headers: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub thinking_budget: Option<u32>,
    #[serde(default)]
    pub auth_provider: Option<String>,
    #[serde(default)]
    pub thinking_keep: Option<String>,
    #[serde(default)]
    pub beta_api: bool,
    #[serde(default)]
    pub capabilities: Option<Vec<String>>,
    #[serde(default)]
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub max_input_size: Option<u32>,
    #[serde(default)]
    pub adaptive_thinking: Option<bool>,
    #[serde(default)]
    pub reasoning_key: Option<String>,
    #[serde(default)]
    pub off_effort: Option<String>,
    #[serde(
        default,
        alias = "responseFormat",
        skip_serializing_if = "Option::is_none"
    )]
    pub response_format: Option<NativeResponseFormat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampling: Option<NativeSampling>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub determinism: Option<NativeDeterminism>,
    #[serde(
        default,
        alias = "extraParams",
        skip_serializing_if = "Option::is_none"
    )]
    pub extra_params: Option<NativeExtraParams>,
    #[serde(default, alias = "binaryPath", skip_serializing_if = "Option::is_none")]
    pub binary_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub antigravity: Option<AntigravityLimits>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct AntigravityLimits {
    #[serde(default = "default_antigravity_spawn_ms", alias = "spawnTimeoutMs")]
    pub spawn_ms: u64,
    #[serde(
        default = "default_antigravity_first_frame_ms",
        alias = "firstFrameTimeoutMs"
    )]
    pub first_frame_ms: u64,
    #[serde(default = "default_antigravity_idle_ms", alias = "idleTimeoutMs")]
    pub idle_ms: u64,
    #[serde(default = "default_antigravity_total_ms", alias = "totalTimeoutMs")]
    pub total_ms: u64,
}

fn default_antigravity_spawn_ms() -> u64 {
    10_000
}

fn default_antigravity_first_frame_ms() -> u64 {
    60_000
}

fn default_antigravity_idle_ms() -> u64 {
    120_000
}

fn default_antigravity_total_ms() -> u64 {
    600_000
}

impl Default for AntigravityLimits {
    fn default() -> Self {
        Self {
            spawn_ms: default_antigravity_spawn_ms(),
            first_frame_ms: default_antigravity_first_frame_ms(),
            idle_ms: default_antigravity_idle_ms(),
            total_ms: default_antigravity_total_ms(),
        }
    }
}


#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    #[serde(default)]
    pub input_tokens: u32,
    #[serde(default)]
    pub output_tokens: u32,
    #[serde(default)]
    pub total_tokens: u32,
    #[serde(default)]
    pub input_cache_read: u32,
    #[serde(default)]
    pub input_cache_creation: u32,
}

impl TokenUsage {
    pub fn accumulate(&mut self, other: &TokenUsage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.total_tokens += other.total_tokens;
        self.input_cache_read += other.input_cache_read;
        self.input_cache_creation += other.input_cache_creation;
    }

    pub fn measured_context_tokens(&self) -> u64 {
        u64::from(self.input_tokens)
            + u64::from(self.input_cache_read)
            + u64::from(self.input_cache_creation)
            + u64::from(self.output_tokens)
    }
}

// ── Engine/LLM contract types shared with the turn loop ────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDelivery {
    pub blocks: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<serde_json::Value>,
}

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
    pub fn is_active(self) -> bool {
        matches!(self, GoalStatus::Active)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct GoalContext {
    pub goal_id: String,
    pub objective: String,
    pub status: GoalStatus,
    pub token_budget: Option<i64>,
    pub turn_budget: Option<i64>,
    #[serde(default)]
    pub wall_clock_budget_ms: Option<i64>,
    #[serde(default)]
    pub wall_clock_ms: i64,
    pub tokens_used: i64,
    pub turns_used: i64,
    #[serde(default)]
    pub updated_at: i64,
    #[serde(default)]
    pub terminal_reason: Option<String>,
}

impl GoalContext {
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

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct TelemetryContext {
    pub mode: String,
    pub provider_type: String,
    pub protocol: String,
    pub thinking_effort: Option<String>,
    pub enabled_plugins: Option<String>,
}


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

#[derive(Debug, Clone, Default)]
pub struct MediaTarget {
    pub protocol: String,
    pub base_url: String,
    pub provider_key: String,
    pub provider_type: String,
    pub image_in: bool,
    pub video_in: bool,
    pub audio_in: bool,
    pub uploads_media: bool,
    pub inline_video: bool,
}

impl MediaTarget {
    pub fn accepts(&self, kind: MediaKind) -> bool {
        match kind {
            MediaKind::Image => self.image_in,
            MediaKind::Video => self.video_in,
            MediaKind::Audio => self.audio_in,
        }
    }
}

pub trait LLM: Send + Sync {
    fn system_prompt(&self) -> &str;
    fn model_name(&self) -> &str;
    fn is_retryable_error(&self, error: &str) -> bool;
    fn transport(&self) -> &'static str {
        "custom"
    }

    fn model_alias(&self) -> Option<&str> {
        None
    }

    fn provider_type(&self) -> Option<&str> {
        None
    }

    fn protocol(&self) -> Option<&str> {
        None
    }

    fn thinking_effort(&self) -> Option<&str> {
        None
    }
    fn media_target(&self) -> Option<crate::types::MediaTarget> {
        None
    }
    fn media_upload_credential(&self) -> Option<crate::types::CredentialFuture<'_>> {
        None
    }
    fn set_tool_call_ids(
        &self,
        _ledger: Arc<crate::tool_call_id::ToolCallIdNormalizer>,
    ) {
    }
    fn set_stream_agent_id(&self, _agent_id: &str) {}
    fn set_turn_watch(&self, _watch: Option<Arc<crate::watch::TurnWatchState>>) {}
    fn chat(
        &self,
        params: LLMChatParams,
    ) -> BoxFuture<
        '_,
        Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>,
    >;
}

#[derive(Debug, Clone, Default)]
pub struct LLMChatParams {
    pub messages: Arc<[LLMMessage]>,
    pub tools: Arc<[ToolInfo]>,
    pub cancel: Option<tokio_util::sync::CancellationToken>,
    pub used_context_tokens: Option<u32>,
    pub max_context_tokens: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct LLMMessage {
    pub role: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocks: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<serde_json::Value>,
}

impl LLMMessage {
    pub fn new(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
            blocks: Vec::new(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            prompt_id: None,
            origin: None,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::new("user", content)
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::new("assistant", content)
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self::new("system", content)
    }

    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: content.into(),
            blocks: Vec::new(),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
            prompt_id: None,
            origin: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct LLMChatResponse {
    pub content: String,
    pub thinking: Vec<ContentBlock>,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: Option<String>,
    pub usage: TokenUsage,
    pub timing: Option<crate::timing::LlmTiming>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extras: Option<serde_json::Value>,
    #[serde(skip)]
    pub args_parse_error: Option<ArgsParseError>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgsParseError {
    pub raw: String,
    pub error: String,
}




pub type CredentialFuture<'a> = BoxFuture<'a, Result<String, String>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UploadError {
    Auth(String),
    Unsupported(String),
    Other(String),
}

impl UploadError {
    pub fn message(&self) -> &str {
        match self {
            Self::Auth(message) | Self::Unsupported(message) | Self::Other(message) => message,
        }
    }
}

impl std::fmt::Display for UploadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for UploadError {}

pub struct Entry {
    pub message_index: usize,
    pub block_index: usize,
    pub key: String,
    pub file_id: Option<String>,
    pub path: Option<String>,
    pub kind: MediaKind,
    pub bytes: usize,
}

pub type DroppedMedia = std::sync::Arc<std::sync::Mutex<HashSet<String>>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaRecovery {
    pub model: String,
    pub context_revision: u64,
    pub action: MediaRecoveryAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaRecoveryAction {
    Degraded(usize),
    Stripped(Vec<String>),
}

#[derive(Debug, Default)]
pub struct MediaRecoveryState {
    pub revision: u64,
    pub recovery: Option<MediaRecovery>,
}

pub type SharedMediaRecovery = std::sync::Arc<std::sync::Mutex<MediaRecoveryState>>;

pub struct ResolvedRequest<'a> {
    pub messages: Cow<'a, [LLMMessage]>,
    pub entries: Vec<Entry>,
}

impl MediaRecoveryState {
    pub fn applicable(&self, model: &str) -> Option<&MediaRecovery> {
        match &self.recovery {
            Some(recovery)
                if recovery.model == model && recovery.context_revision == self.revision =>
            {
                Some(recovery)
            }
            _ => None,
        }
    }

    pub fn mark(&mut self, model: String, action: MediaRecoveryAction) {
        self.recovery = Some(MediaRecovery {
            model,
            context_revision: self.revision,
            action,
        });
    }

    pub fn advance_revision(&mut self) {
        self.revision += 1;
    }
}
