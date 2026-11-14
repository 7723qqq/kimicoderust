use std::collections::{BTreeMap, HashMap, HashSet};
use std::error::Error;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::future::BoxFuture;
use futures_util::StreamExt;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::_base::di::lifecycle::DisposableRef;
use crate::_base::errors::errors::{Error2, Error2Options};
use crate::_base::log::log::{ILogService, LogContext};
use crate::_base::utils::abort::AbortSignal;
use crate::_base::utils::env::parse_boolean_env;
use crate::_base::utils::retry::{RetrySleepTimers, retry_backoff_delay, sleep_for_retry};
use crate::agent::context_memory::context_memory::IAgentContextMemoryService;
use crate::agent::context_memory::types::ContextMessage;
use crate::agent::context_projector::context_projector::{
    IAgentContextProjectorService, MEDIA_DEGRADE_KEEP_RECENT, MediaEvictionPlan, MediaProjection,
    MediaStripSnapshot, ProjectionPolicy, ProjectionStructure,
};
use crate::agent::context_projector::request_size::estimate_llm_request_bytes;
use crate::agent::llm_requester::config_section::resolve_request_byte_budget;
use crate::agent::llm_requester::llm_request_ops::{
    LlmRequest, LlmRequestKind, LlmRequestPayload, LlmRequestProjection, LlmRequestToolSchema,
    LlmToolsSnapshot, LlmToolsSnapshotPayload, llm_request_trace_key,
};
use crate::agent::llm_requester::llm_requester::{
    AgentLLMRequestFinish, AgentLLMRequestLogFields, AgentLLMRequestOverrides,
    AgentLLMRequestPartHandler, AgentLLMRequestSource, AgentLLMRequestTask,
    AgentLLMTurnRequestSnapshot, IAgentLLMRequesterService, PreparedTurnRequestConfig,
    PromptCacheProbe,
};
use crate::agent::media::media_ref::{
    MediaPathTag, MediaPathTagKind, build_media_path_tag, match_media_path_tags,
};
use crate::agent::media::media_resolver::IAgentMediaResolverService;
use crate::agent::profile::profile::{IAgentProfileService, ProfileModelContext};
use crate::agent::profile::profile_ops::{WarningIssued, WarningIssuedPayload};
use crate::agent::scope_context::scope_context::IAgentScopeContext;
use crate::agent::state::agent_state_v2::IAgentStateService;
use crate::agent::tool_select::tool_select::IAgentToolSelectService;
use crate::agent::tools::read_media_file::read_media_file::READ_MEDIA_FILE_TOOL_NAME;
use crate::session::cache_observation::{
    CacheObservationInput, build_cache_observation, fingerprint,
};
use crate::app::bootstrap::bootstrap::IBootstrapService;
use crate::app::config::config::IConfigService;
use crate::app::kosong_config::config_section::{MODEL_OVERRIDES_SECTION, THINKING_SECTION};
use crate::app::telemetry::telemetry::ITelemetryService;
use crate::human::llm::finish_reason::FinishReason;
use crate::human::llm::message::{
    ContentPart, Role, StreamedMessagePart, ToolCall, ToolDescription as Tool,
};
use crate::human::llm::requester::requester::LlmCredentialProvider;
use crate::human::llm::thinking::ThinkingEffort;
use crate::human::llm::tool_call_id_normalizer::{
    ToolCallIdNormalizer, ToolCallIdSeed, ToolCallIdSeedMessage,
};
use crate::human::llm::usage::{TokenUsage, input_total};
use crate::llm_adapter::contract::errors::{
    APIContextOverflowError, APIProviderOverloadedError, APIProviderQuotaExhaustedError,
    APIProviderRateLimitError, APIRequestTooLargeError, APIStatusError, ApiErrorKind,
    PROVIDER_API_ERROR_CODE, classify_api_error, create_abort_error, is_abort_error,
    is_image_format_error, is_recoverable_request_structure_error, is_retryable_generate_error,
};
use crate::llm_adapter::contract::message::Message;
use crate::llm_adapter::contract::request_trace::LLMRequestTrace;
use crate::llm_adapter::model::catalog::{IModelCatalog, Model};
use crate::llm_adapter::model::completion_budget::{
    completion_budget_params, resolve_completion_budget,
};
use crate::llm_adapter::model::model::IModelService;
use crate::llm_adapter::model::model_requester::{
    ModelRequestEvent, ModelRequestInput, ModelRequestParams, ModelRequestTiming, ModelRequester,
    effective_max_completion_tokens,
};
use crate::llm_adapter::model::model_types::ModelOverrides;
use crate::llm_adapter::model::thinking::{ThinkingConfig, resolve_thinking_keep};
use crate::llm_adapter::protocol::protocol::Protocol;
use crate::os::interface::host_timers::IHostTimers;
use crate::session::session_token_counting::session_token_counting::ISessionTokenCountingService;
use crate::session::session_usage::session_usage::ISessionUsageService;
use crate::state::event_dispatcher::IEventDispatcher;
use crate::state::state::{ReplayableOptions, ReplayableStateKey};
use crate::wire::record::WireAsyncError;

pub const KIMI_CODE_INFINITE_RETRY_ENV: &str = "KIMI_CODE_INFINITE_RETRY";

const ABORT_SIGNAL_POLL_INTERVAL: Duration = Duration::from_millis(25);

pub type RequestTraceCallback = Arc<dyn Fn(Option<String>) + Send + Sync>;

#[derive(Clone)]
struct PromptCacheProbeState {
    source: PromptCacheProbe,
    baseline: Option<AgentLLMTurnRequestSnapshot>,
}

#[derive(Clone)]
struct ResolvedLLMRequest {
    requester: Arc<dyn ModelRequester>,
    model: Model,
    params: ModelRequestParams,
    model_alias: String,
    thinking_effort: ThinkingEffort,
    system_prompt: String,
    tools: Vec<Tool>,
    messages: Vec<ContextMessage>,
    context_revision: u64,
    prompt_cache_probe: Option<PromptCacheProbeState>,
    source: Option<AgentLLMRequestSource>,
    log_fields: AgentLLMRequestLogFields,
}

#[derive(Clone)]
struct LLMRequestLogInput {
    protocol: Protocol,
    provider_type: Option<String>,
    model_name: String,
    model_alias: Option<String>,
    thinking_effort: Option<ThinkingEffort>,
    max_tokens: Option<u32>,
    system_prompt: String,
    tools: Vec<Tool>,
    messages: Vec<Message>,
    fields: AgentLLMRequestLogFields,
}

#[derive(Clone)]
struct TurnRequestConfig {
    resolved: ProfileModelContext,
    params: ModelRequestParams,
    system_prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelRequestParamsSnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cache_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sampling: Option<crate::llm_adapter::model::model_requester::SamplingOptions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    thinking_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    thinking_keep: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    used_context_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_context_tokens: Option<u32>,
}

impl ModelRequestParamsSnapshot {
    fn from_params(params: &ModelRequestParams) -> Self {
        Self {
            cache_key: params.cache_key.clone(),
            sampling: params.sampling.clone(),
            thinking_effort: params.thinking_effort.clone(),
            thinking_keep: params.thinking_keep.clone(),
            max_completion_tokens: params.max_completion_tokens,
            used_context_tokens: params.used_context_tokens,
            max_context_tokens: params.max_context_tokens,
        }
    }

    fn into_params(self) -> ModelRequestParams {
        ModelRequestParams {
            cache_key: self.cache_key,
            sampling: self.sampling,
            thinking_effort: self.thinking_effort,
            thinking_keep: self.thinking_keep,
            max_completion_tokens: self.max_completion_tokens,
            used_context_tokens: self.used_context_tokens,
            max_context_tokens: self.max_context_tokens,
            on_trace_id: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TurnRequestConfigSnapshot {
    resolved: ProfileModelContext,
    params: ModelRequestParamsSnapshot,
    system_prompt: String,
}

impl TurnRequestConfigSnapshot {
    fn from_config(config: &TurnRequestConfig) -> Self {
        Self {
            resolved: config.resolved.clone(),
            params: ModelRequestParamsSnapshot::from_params(&config.params),
            system_prompt: config.system_prompt.clone(),
        }
    }

    fn into_config(self) -> TurnRequestConfig {
        TurnRequestConfig {
            resolved: self.resolved,
            params: self.params.into_params(),
            system_prompt: self.system_prompt,
        }
    }
}

#[derive(Clone)]
struct MediaRecovery {
    model_alias: String,
    context_revision: u64,
    media: MediaProjection,
}

enum RunOutcome {
    Finish(Box<AgentLLMRequestFinish>),
    Escalate(ProjectionPolicy),
}

struct RunState {
    policy: Option<ProjectionPolicy>,
    media_stripped_this_request: Arc<AtomicBool>,
    previous_media_count: Option<usize>,
    previous_media_policy: Option<MediaProjection>,
    media_paths: Option<HashMap<String, String>>,
    cache_probe_reported: bool,
}

struct FinishEvent {
    provider_finish_reason: Option<FinishReason>,
    raw_finish_reason: Option<String>,
    id: Option<String>,
    trace_id: Option<String>,
}

pub trait IAgentMicroCompactionService: Send + Sync {
    fn compact(&self, messages: Vec<ContextMessage>) -> Vec<ContextMessage>;
}

#[derive(Clone, Default)]
struct MutableLLMRequestTrace {
    trace_id: Arc<Mutex<Option<String>>>,
}

impl MutableLLMRequestTrace {
    fn new() -> Self {
        Self::default()
    }

    fn set(&self, trace_id: Option<String>) {
        *self.trace_id.lock() = trace_id;
    }

    fn current(&self) -> Option<String> {
        self.trace_id.lock().clone()
    }

    fn snapshot(&self) -> LLMRequestTrace {
        LLMRequestTrace {
            trace_id: self.current(),
        }
    }
}

pub struct AgentLLMRequesterServiceOptions {
    pub micro_compaction: Option<Arc<dyn IAgentMicroCompactionService>>,
    pub host_timers: Option<Arc<dyn IHostTimers>>,
}

impl Default for AgentLLMRequesterServiceOptions {
    fn default() -> Self {
        Self {
            micro_compaction: None,
            host_timers: None,
        }
    }
}

struct RequesterDeps {
    context: Arc<dyn IAgentContextMemoryService>,
    projector: Arc<dyn IAgentContextProjectorService>,
    token_counting: Arc<dyn ISessionTokenCountingService>,
    tool_select: Arc<dyn IAgentToolSelectService>,
    media_resolver: Arc<dyn IAgentMediaResolverService>,
    profile: Arc<dyn IAgentProfileService>,
    usage: Arc<dyn ISessionUsageService>,
    config: Arc<dyn IConfigService>,
    model_service: Arc<dyn IModelService>,
    model_catalog: Arc<dyn IModelCatalog>,
    log: Arc<dyn ILogService>,
    telemetry: Arc<dyn ITelemetryService>,
    dispatcher: Arc<dyn IEventDispatcher>,
    scope_context: Arc<dyn IAgentScopeContext>,
    states: Arc<dyn IAgentStateService>,
    bootstrap: Arc<dyn IBootstrapService>,
    micro_compaction: Option<Arc<dyn IAgentMicroCompactionService>>,
    host_timers: Option<Arc<dyn IHostTimers>>,
    tool_call_id_normalizer: Mutex<ToolCallIdNormalizer>,
}

pub struct AgentLLMRequesterService {
    deps: Arc<RequesterDeps>,
    _registrations: Vec<DisposableRef>,
}

pub static LLM_REQUESTER_LAST_CONFIG_LOG_SIGNATURE_KEY: LazyLock<Arc<ReplayableStateKey>> =
    LazyLock::new(|| {
        Arc::new(ReplayableStateKey::new(
            "llmRequester.lastConfigLogSignature".to_string(),
            Arc::new(|| Value::Null),
            ReplayableOptions {
                schema: json!({}),
                durable: Some(false),
                blobs: None,
                mutable_replay: None,
            },
        ))
    });

pub static LLM_REQUESTER_TURN_CONFIGS_KEY: LazyLock<Arc<ReplayableStateKey>> = LazyLock::new(|| {
    Arc::new(ReplayableStateKey::new(
        "llmRequester.turnConfigs".to_string(),
        Arc::new(|| json!({})),
        ReplayableOptions {
            schema: json!({}),
            durable: Some(false),
            blobs: None,
            mutable_replay: None,
        },
    ))
});

pub static LLM_REQUESTER_MEDIA_RECOVERY_KEY: LazyLock<Arc<ReplayableStateKey>> = LazyLock::new(|| {
    Arc::new(ReplayableStateKey::new(
        "llmRequester.mediaRecovery".to_string(),
        Arc::new(|| Value::Null),
        ReplayableOptions {
            schema: json!({}),
            durable: Some(false),
            blobs: None,
            mutable_replay: None,
        },
    ))
});

pub static LLM_REQUESTER_EMITTED_THINKING_EFFORT_WARNINGS_KEY: LazyLock<Arc<ReplayableStateKey>> =
    LazyLock::new(|| {
        Arc::new(ReplayableStateKey::new(
            "llmRequester.emittedThinkingEffortWarnings".to_string(),
            Arc::new(|| json!([])),
            ReplayableOptions {
                schema: json!({}),
                durable: Some(false),
                blobs: None,
                mutable_replay: None,
            },
        ))
    });

pub static LLM_REQUESTER_LAST_TURN_REQUEST_KEY: LazyLock<Arc<ReplayableStateKey>> =
    LazyLock::new(|| {
        Arc::new(ReplayableStateKey::new(
            "llmRequester.lastTurnRequest".to_string(),
            Arc::new(|| Value::Null),
            ReplayableOptions {
                schema: json!({}),
                durable: Some(false),
                blobs: None,
                mutable_replay: None,
            },
        ))
    });

pub fn llm_requester_last_config_log_signature_key() -> Arc<ReplayableStateKey> {
    LLM_REQUESTER_LAST_CONFIG_LOG_SIGNATURE_KEY.clone()
}

pub fn llm_requester_turn_configs_key() -> Arc<ReplayableStateKey> {
    LLM_REQUESTER_TURN_CONFIGS_KEY.clone()
}

pub fn llm_requester_media_recovery_key() -> Arc<ReplayableStateKey> {
    LLM_REQUESTER_MEDIA_RECOVERY_KEY.clone()
}

pub fn llm_requester_emitted_thinking_effort_warnings_key() -> Arc<ReplayableStateKey> {
    LLM_REQUESTER_EMITTED_THINKING_EFFORT_WARNINGS_KEY.clone()
}

pub fn llm_requester_last_turn_request_key() -> Arc<ReplayableStateKey> {
    LLM_REQUESTER_LAST_TURN_REQUEST_KEY.clone()
}

impl RequesterDeps {
    fn last_config_log_signature(&self) -> Option<String> {
        match self
            .states
            .get(&llm_requester_last_config_log_signature_key().state_key())
        {
            Value::String(signature) => Some(signature),
            _ => None,
        }
    }

    fn set_last_config_log_signature(&self, signature: Option<String>) {
        self.states.set(
            &llm_requester_last_config_log_signature_key().state_key(),
            signature.map_or(Value::Null, Value::String),
        );
    }

    fn turn_configs(&self) -> BTreeMap<u64, TurnRequestConfig> {
        let value = self.states.get(&llm_requester_turn_configs_key().state_key());
        let Some(object) = value.as_object() else {
            return BTreeMap::new();
        };
        object
            .iter()
            .filter_map(|(key, value)| {
                let turn_id = key.parse::<u64>().ok()?;
                let snapshot =
                    serde_json::from_value::<TurnRequestConfigSnapshot>(value.clone()).ok()?;
                Some((turn_id, snapshot.into_config()))
            })
            .collect()
    }

    fn set_turn_configs(&self, configs: &BTreeMap<u64, TurnRequestConfig>) {
        let mut object = Map::new();
        for (turn_id, config) in configs {
            let snapshot = TurnRequestConfigSnapshot::from_config(config);
            let Ok(value) = serde_json::to_value(snapshot) else {
                continue;
            };
            object.insert(turn_id.to_string(), value);
        }
        self.states.set(
            &llm_requester_turn_configs_key().state_key(),
            Value::Object(object),
        );
    }

    fn media_recovery(&self) -> Option<MediaRecovery> {
        let value = self.states.get(&llm_requester_media_recovery_key().state_key());
        let object = value.as_object()?;
        let model_alias = object.get("modelAlias")?.as_str()?.to_string();
        let context_revision = object.get("contextRevision")?.as_u64()?;
        let media = media_projection_from_value(object.get("media")?)?;
        Some(MediaRecovery {
            model_alias,
            context_revision,
            media,
        })
    }

    fn set_media_recovery(&self, recovery: &MediaRecovery) {
        self.states.set(
            &llm_requester_media_recovery_key().state_key(),
            json!({
                "modelAlias": recovery.model_alias,
                "contextRevision": recovery.context_revision,
                "media": media_projection_to_value(&recovery.media),
            }),
        );
    }

    fn emitted_thinking_effort_warnings(&self) -> HashSet<String> {
        let value = self
            .states
            .get(&llm_requester_emitted_thinking_effort_warnings_key().state_key());
        value
            .as_array()
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn add_emitted_thinking_effort_warning(&self, key: String) {
        let mut warnings = self.emitted_thinking_effort_warnings();
        warnings.insert(key);
        let entries = warnings.into_iter().map(Value::String).collect::<Vec<_>>();
        self.states.set(
            &llm_requester_emitted_thinking_effort_warnings_key().state_key(),
            Value::Array(entries),
        );
    }

    fn last_turn_request(&self) -> Option<AgentLLMTurnRequestSnapshot> {
        let value = self.states.get(&llm_requester_last_turn_request_key().state_key());
        if value.is_null() {
            return None;
        }
        serde_json::from_value(value).ok()
    }

    fn set_last_turn_request(&self, snapshot: AgentLLMTurnRequestSnapshot) {
        let Ok(value) = serde_json::to_value(snapshot) else {
            return;
        };
        self.states
            .set(&llm_requester_last_turn_request_key().state_key(), value);
    }

    fn retry_timers(&self) -> Option<Arc<dyn RetrySleepTimers>> {
        self.host_timers
            .clone()
            .map(|timers| Arc::new(HostTimersSleep::new(timers)) as Arc<dyn RetrySleepTimers>)
    }
}

impl AgentLLMRequesterService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        context: Arc<dyn IAgentContextMemoryService>,
        projector: Arc<dyn IAgentContextProjectorService>,
        token_counting: Arc<dyn ISessionTokenCountingService>,
        tool_select: Arc<dyn IAgentToolSelectService>,
        media_resolver: Arc<dyn IAgentMediaResolverService>,
        profile: Arc<dyn IAgentProfileService>,
        usage: Arc<dyn ISessionUsageService>,
        config: Arc<dyn IConfigService>,
        model_service: Arc<dyn IModelService>,
        model_catalog: Arc<dyn IModelCatalog>,
        log: Arc<dyn ILogService>,
        telemetry: Arc<dyn ITelemetryService>,
        dispatcher: Arc<dyn IEventDispatcher>,
        scope_context: Arc<dyn IAgentScopeContext>,
        states: Arc<dyn IAgentStateService>,
        bootstrap: Arc<dyn IBootstrapService>,
        options: AgentLLMRequesterServiceOptions,
    ) -> Self {
        let registrations = vec![
            states.contribute_state(llm_request_trace_key().state_key()),
            states.contribute_state(llm_requester_last_config_log_signature_key().state_key()),
            states.contribute_state(llm_requester_turn_configs_key().state_key()),
            states.contribute_state(llm_requester_media_recovery_key().state_key()),
            states.contribute_state(llm_requester_emitted_thinking_effort_warnings_key().state_key()),
            states.contribute_state(llm_requester_last_turn_request_key().state_key()),
        ];
        let deps = Arc::new(RequesterDeps {
            context,
            projector,
            token_counting,
            tool_select,
            media_resolver,
            profile,
            usage,
            config,
            model_service,
            model_catalog,
            log,
            telemetry,
            dispatcher,
            scope_context,
            states,
            bootstrap,
            micro_compaction: options.micro_compaction,
            host_timers: options.host_timers,
            tool_call_id_normalizer: Mutex::new(ToolCallIdNormalizer::new()),
        });
        Self {
            deps,
            _registrations: registrations,
        }
    }

    pub fn prepare_turn_config(&self, turn_id: u64) -> Option<PreparedTurnRequestConfig> {
        if !self.deps.profile.has_provider() {
            return None;
        }
        let config = get_or_create_turn_config(&self.deps, turn_id);
        Some(PreparedTurnRequestConfig {
            thinking_effort: config.resolved.thinking_level,
        })
    }

    pub fn current_credential_provider(&self) -> Option<Arc<dyn LlmCredentialProvider>> {
        if !self.deps.profile.has_provider() {
            return None;
        }
        let resolved = self.deps.profile.resolve_model_context();
        self.deps
            .model_catalog
            .get(&resolved.model_alias)
            .ok()
            .and_then(|model| model.credential_provider)
    }

    pub fn credential_provider_for_turn(
        &self,
        turn_id: u64,
    ) -> Option<Arc<dyn LlmCredentialProvider>> {
        if !self.deps.profile.has_provider() {
            return None;
        }
        let resolved = self
            .deps
            .turn_configs()
            .get(&turn_id)
            .map(|config| config.resolved.clone())
            .unwrap_or_else(|| self.deps.profile.resolve_model_context());
        self.deps
            .model_catalog
            .get(&resolved.model_alias)
            .ok()
            .and_then(|model| model.credential_provider)
    }

    pub fn last_turn_request(&self) -> Option<AgentLLMTurnRequestSnapshot> {
        self.deps.last_turn_request()
    }

    pub fn request(
        &self,
        overrides: Option<AgentLLMRequestOverrides>,
        on_part: Option<AgentLLMRequestPartHandler>,
        signal: Option<AbortSignal>,
    ) -> BoxFuture<'static, Result<AgentLLMRequestFinish, WireAsyncError>> {
        self.start(overrides, on_part, signal).result
    }

    pub fn start(
        &self,
        overrides: Option<AgentLLMRequestOverrides>,
        on_part: Option<AgentLLMRequestPartHandler>,
        signal: Option<AbortSignal>,
    ) -> AgentLLMRequestTask {
        let deps = self.deps.clone();
        let overrides = overrides.unwrap_or_default();
        let trace = MutableLLMRequestTrace::new();
        let trace_snapshot = trace.snapshot();
        let prompt_cache_probe =
            overrides
                .prompt_cache_probe
                .map(|source| PromptCacheProbeState {
                    source,
                    baseline: deps.last_turn_request(),
                });
        let result = Box::pin(request_with_trace(
            deps,
            trace,
            overrides,
            on_part,
            signal,
            prompt_cache_probe,
        ));
        AgentLLMRequestTask {
            trace: trace_snapshot,
            result,
        }
    }
}

async fn request_with_trace(
    deps: Arc<RequesterDeps>,
    trace: MutableLLMRequestTrace,
    overrides: AgentLLMRequestOverrides,
    on_part: Option<AgentLLMRequestPartHandler>,
    signal: Option<AbortSignal>,
    prompt_cache_probe: Option<PromptCacheProbeState>,
) -> Result<AgentLLMRequestFinish, WireAsyncError> {
    throw_if_aborted(signal.as_ref())?;
    let started_at = now_ms();
    let set_trace = make_set_trace(
        deps.clone(),
        trace.clone(),
        is_turn_source(overrides.source.as_ref()),
    );
    set_trace(None);
    let size_probe = Arc::new(Mutex::new(None));
    let request = match resolve_request(&deps, &overrides, prompt_cache_probe, signal.as_ref()).await
    {
        Ok(request) => request,
        Err(error) => {
            log_request_failure(
                &deps,
                &error,
                &overrides,
                signal.as_ref(),
                *size_probe.lock(),
            );
            set_trace(track_api_error(
                &deps,
                &error,
                started_at,
                signal.as_ref(),
                overrides.source.as_ref(),
                trace.current(),
                *size_probe.lock(),
            ));
            return Err(error);
        }
    };
    match run_request(
        deps.clone(),
        request,
        on_part,
        signal.clone(),
        set_trace.clone(),
        size_probe.clone(),
        overrides.on_attempt_retry.clone(),
    )
    .await
    {
        Ok(finish) => Ok(finish),
        Err(error) => {
            log_request_failure(
                &deps,
                &error,
                &overrides,
                signal.as_ref(),
                *size_probe.lock(),
            );
            set_trace(track_api_error(
                &deps,
                &error,
                started_at,
                signal.as_ref(),
                overrides.source.as_ref(),
                trace.current(),
                *size_probe.lock(),
            ));
            Err(error)
        }
    }
}

fn make_set_trace(
    deps: Arc<RequesterDeps>,
    trace: MutableLLMRequestTrace,
    is_turn: bool,
) -> RequestTraceCallback {
    Arc::new(move |trace_id: Option<String>| {
        trace.set(trace_id.clone());
        if is_turn {
            let mut patch = Map::new();
            patch.insert(
                "trace_id".to_string(),
                trace_id.map_or(Value::Null, Value::String),
            );
            deps.telemetry.set_context(patch);
        }
    })
}

fn log_request_failure(
    deps: &RequesterDeps,
    error: &WireAsyncError,
    overrides: &AgentLLMRequestOverrides,
    signal: Option<&AbortSignal>,
    estimated_request_bytes: Option<u64>,
) {
    if is_abort_error(error.as_ref()) || signal.is_some_and(AbortSignal::aborted) {
        return;
    }
    let mut payload = log_fields_for_source(overrides.source.as_ref());
    payload.insert(
        "model".to_string(),
        json!(deps
            .profile
            .data()
            .base
            .model_alias
            .unwrap_or_else(|| "unknown".to_string())),
    );
    payload.extend(retry_error_fields(error.as_ref()));
    if let Some(estimated_request_bytes) = estimated_request_bytes {
        payload.insert(
            "estimatedRequestBytes".to_string(),
            json!(estimated_request_bytes),
        );
    }
    deps.log
        .warn("llm request failed", Some(log_context_value(payload)));
}

fn track_api_error(
    deps: &RequesterDeps,
    error: &WireAsyncError,
    started_at: i64,
    signal: Option<&AbortSignal>,
    source: Option<&AgentLLMRequestSource>,
    request_trace_id: Option<String>,
    estimated_request_bytes: Option<u64>,
) -> Option<String> {
    if is_abort_error(error.as_ref()) || signal.is_some_and(AbortSignal::aborted) {
        return request_trace_id;
    }
    let model_alias = deps.profile.data().base.model_alias;
    let model = try_get_model(deps);
    let trace_id = request_trace_id.or_else(|| api_trace_id(error.as_ref()));
    let raw = unwrap_error_cause_ref(error.as_ref());
    let classification = classify_api_error(raw);
    let mut properties = Map::new();
    properties.insert(
        "error_type".to_string(),
        json!(api_error_kind_str(classification.kind)),
    );
    properties.insert(
        "model".to_string(),
        json!(model
            .as_ref()
            .map(|model| model.id.clone())
            .or_else(|| model_alias.clone())
            .unwrap_or_else(|| "unknown".to_string())),
    );
    if let Some(model_alias) = &model_alias {
        properties.insert("alias".to_string(), json!(model_alias));
    }
    let provider_type = model
        .as_ref()
        .and_then(|model| model.provider_type.clone())
        .or_else(|| model.as_ref().map(|model| model.protocol.as_str().to_string()));
    if let Some(provider_type) = provider_type {
        properties.insert("provider_type".to_string(), json!(provider_type));
    }
    if let Some(model) = &model {
        properties.insert("protocol".to_string(), json!(model.protocol.as_str()));
    }
    properties.insert(
        "retryable".to_string(),
        json!(is_retryable_generate_error(raw)),
    );
    properties.insert(
        "duration_ms".to_string(),
        json!((now_ms() - started_at).max(0)),
    );
    if let Some(turn_id) = source.and_then(AgentLLMRequestSource::turn_id) {
        properties.insert("turn_id".to_string(), json!(turn_id));
    }
    if let Some(request_kind) = request_kind_for_telemetry(source) {
        properties.insert("request_kind".to_string(), json!(request_kind));
    }
    if let Some(trace_id) = &trace_id {
        properties.insert("trace_id".to_string(), json!(trace_id));
    }
    if let Some(estimated_request_bytes) = estimated_request_bytes {
        properties.insert(
            "estimated_request_bytes".to_string(),
            json!(estimated_request_bytes),
        );
    }
    if let Some(AgentLLMRequestSource::Turn { step: Some(step), .. }) = source {
        properties.insert("step_no".to_string(), json!(step));
    }
    if let Some(status_code) = api_status_code(raw) {
        properties.insert("status_code".to_string(), json!(status_code));
    }
    let current_turn = deps
        .usage
        .status(&deps.scope_context.agent_context())
        .current_turn;
    if let Some(current_turn) = &current_turn {
        properties.insert("input_tokens".to_string(), json!(input_total(current_turn)));
    }
    deps.telemetry.track2("api_error", Some(properties));
    trace_id
}

fn try_get_model(deps: &RequesterDeps) -> Option<Model> {
    let model_alias = deps.profile.data().base.model_alias?;
    deps.model_catalog.get(&model_alias).ok()
}

async fn run_request(
    deps: Arc<RequesterDeps>,
    request: ResolvedLLMRequest,
    on_part: Option<AgentLLMRequestPartHandler>,
    signal: Option<AbortSignal>,
    on_request_trace: RequestTraceCallback,
    size_probe: Arc<Mutex<Option<u64>>>,
    on_attempt_retry: Option<Arc<dyn Fn() + Send + Sync>>,
) -> Result<AgentLLMRequestFinish, WireAsyncError> {
    {
        let mut normalizer = deps.tool_call_id_normalizer.lock();
        normalizer.seed_from(&seed_messages(&deps.context.get()));
    }
    let cache_probe_baseline = request
        .prompt_cache_probe
        .as_ref()
        .and_then(|probe| probe.baseline.clone());
    let compacted = match &deps.micro_compaction {
        Some(micro_compaction) => micro_compaction.compact(request.messages.clone()),
        None => request.messages.clone(),
    };
    let shaped_history = deps
        .tool_select
        .shape_history(compacted, None, Some(request.tools.clone()));
    let hide_read_media =
        !request.model.capabilities.image_in && !request.model.capabilities.video_in;
    let shaped = if hide_read_media {
        hide_read_media_file(shaped_history)
    } else {
        shaped_history
    };
    let tools = if hide_read_media {
        request
            .tools
            .iter()
            .filter(|tool| tool.name != READ_MEDIA_FILE_TOOL_NAME)
            .cloned()
            .collect::<Vec<_>>()
    } else {
        request.tools.clone()
    };
    let recovered_media = recovered_media(&deps, &request);
    let mut state = RunState {
        policy: recovered_media.map(|media| ProjectionPolicy {
            structure: None,
            media: Some(media),
        }),
        media_stripped_this_request: Arc::new(AtomicBool::new(false)),
        previous_media_count: None,
        previous_media_policy: None,
        media_paths: None,
        cache_probe_reported: false,
    };
    let mut infinite_retry_attempt = 0u32;
    loop {
        let outcome = run_once(
            &deps,
            &request,
            &shaped,
            &tools,
            on_part.as_ref(),
            signal.as_ref(),
            &on_request_trace,
            &size_probe,
            cache_probe_baseline.as_ref(),
            &mut state,
        )
        .await;
        match outcome {
            Ok(RunOutcome::Finish(finish)) => return Ok(*finish),
            Ok(RunOutcome::Escalate(policy)) => {
                state.policy = Some(policy);
            }
            Err(error) => {
                let media = state
                    .policy
                    .as_ref()
                    .and_then(|policy| policy.media.clone());
                let can_strip_media = !state.media_stripped_this_request.load(Ordering::SeqCst)
                    && (media.is_none()
                        || matches!(media, Some(MediaProjection::DegradeCount(_)))
                        || state.previous_media_count.unwrap_or(0) > 0);
                let can_degrade_media = match &media {
                    None => true,
                    Some(MediaProjection::DegradeCount(_)) => {
                        state.previous_media_count.unwrap_or(0) > MEDIA_DEGRADE_KEEP_RECENT
                    }
                    Some(MediaProjection::Strip(_)) => false,
                };
                let media_stripped_flag = state.media_stripped_this_request.clone();
                let mut capture_media_strip_policy =
                    || -> Result<MediaProjection, WireAsyncError> {
                        media_stripped_flag.store(true, Ordering::SeqCst);
                        let snapshot = deps.projector.capture_media_strip_snapshot(&shaped)?;
                        let media = MediaProjection::Strip(snapshot);
                        mark_media_recovery(&deps, &request, media.clone());
                        Ok(media)
                    };
                let mut capture_media_degrade_policy =
                    || -> Result<MediaProjection, WireAsyncError> {
                        let degrade_count =
                            deps.projector.capture_media_degrade_count(&shaped)?;
                        let media = MediaProjection::DegradeCount(degrade_count);
                        mark_media_recovery(&deps, &request, media.clone());
                        Ok(media)
                    };
                let degrade_capture: Option<
                    &mut dyn FnMut() -> Result<MediaProjection, WireAsyncError>,
                > = if can_degrade_media {
                    Some(&mut capture_media_degrade_policy)
                } else {
                    None
                };
                let next_policy = next_projection_policy_for_error(
                    &deps,
                    &error,
                    state.policy.as_ref(),
                    &request,
                    signal.as_ref(),
                    can_strip_media,
                    degrade_capture,
                    &mut capture_media_strip_policy,
                )?;
                if let Some(next_policy) = next_policy {
                    if let Some(on_attempt_retry) = &on_attempt_retry {
                        on_attempt_retry();
                    }
                    state.policy = Some(next_policy);
                    continue;
                }
                let raw = unwrap_error_cause_ref(error.as_ref());
                if !infinite_retry_enabled(&deps)
                    || is_abort_error(error.as_ref())
                    || signal.as_ref().is_some_and(AbortSignal::aborted)
                    || raw.downcast_ref::<APIContextOverflowError>().is_some()
                {
                    return Err(error);
                }
                infinite_retry_attempt += 1;
                let delay_ms = retry_after_ms_of(raw)
                    .unwrap_or_else(|| retry_backoff_delay(infinite_retry_attempt - 1));
                let mut payload = LogContext::new();
                payload.insert("model".to_string(), json!(request.model.name));
                payload.extend(request.log_fields.clone());
                payload.insert("attempt".to_string(), json!(infinite_retry_attempt));
                payload.insert("delayMs".to_string(), json!(delay_ms));
                payload.extend(retry_error_fields(error.as_ref()));
                deps.log.warn(
                    "llm request failed; retrying indefinitely (KIMI_CODE_INFINITE_RETRY)",
                    Some(log_context_value(payload)),
                );
                if let Some(on_attempt_retry) = &on_attempt_retry {
                    on_attempt_retry();
                }
                sleep_for_retry(delay_ms, signal.clone(), deps.retry_timers())
                    .await
                    .map_err(|_| create_abort_error())?;
            }
        }
    }
}

fn infinite_retry_enabled(deps: &RequesterDeps) -> bool {
    parse_boolean_env(deps.bootstrap.get_env(KIMI_CODE_INFINITE_RETRY_ENV).as_deref()) == Some(true)
}

#[allow(clippy::too_many_arguments)]
async fn run_once(
    deps: &Arc<RequesterDeps>,
    request: &ResolvedLLMRequest,
    shaped: &[ContextMessage],
    tools: &[Tool],
    on_part: Option<&AgentLLMRequestPartHandler>,
    signal: Option<&AbortSignal>,
    on_request_trace: &RequestTraceCallback,
    size_probe: &Arc<Mutex<Option<u64>>>,
    cache_probe_baseline: Option<&AgentLLMTurnRequestSnapshot>,
    state: &mut RunState,
) -> Result<RunOutcome, WireAsyncError> {
    on_request_trace(None);
    let projection = projection_name_of(state.policy.as_ref());
    let mut fields = request.log_fields.clone();
    if let Some(projection) = projection {
        fields.insert("projection".to_string(), json!(projection));
    }
    if state
        .policy
        .as_ref()
        .and_then(|policy| policy.media.as_ref())
        .is_some()
        && state.media_paths.is_none()
    {
        let shaped_messages = shaped
            .iter()
            .cloned()
            .map(Message::from)
            .collect::<Vec<_>>();
        state.media_paths = Some(deps.media_resolver.display_paths(&shaped_messages).await?);
    }
    let projected =
        deps.projector
            .project(shaped, state.policy.as_ref(), state.media_paths.as_ref())?;
    let current_media_count = media_part_count(&projected);
    let current_media_policy = state
        .policy
        .as_ref()
        .and_then(|policy| policy.media.clone());
    let media_policy_changed = state.previous_media_count.is_some()
        && state.previous_media_policy != current_media_policy;
    let dropped_media_count: i64 = match state.previous_media_count {
        Some(previous_media_count) => {
            previous_media_count as i64 - current_media_count as i64
        }
        None => 0,
    };
    state.previous_media_count = Some(current_media_count);
    state.previous_media_policy = current_media_policy;
    let resolved_messages = deps
        .media_resolver
        .resolve(&projected, request.requester.clone(), signal.cloned())
        .await?;
    let input = ModelRequestInput {
        system_prompt: request.system_prompt.clone(),
        tools: tools.to_vec(),
        messages: resolved_messages,
        response_format: None,
    };
    let estimated_bytes = estimate_llm_request_bytes(&input.messages);
    *size_probe.lock() = Some(estimated_bytes);
    let media_stripped_flag = state.media_stripped_this_request.clone();
    let mut capture_media_strip_policy = || -> Result<MediaProjection, WireAsyncError> {
        media_stripped_flag.store(true, Ordering::SeqCst);
        let snapshot = deps.projector.capture_media_strip_snapshot(shaped)?;
        let media = MediaProjection::Strip(snapshot);
        mark_media_recovery(deps, request, media.clone());
        Ok(media)
    };
    if let Some(escalation) = proactive_media_eviction(
        deps,
        &input.messages,
        estimated_bytes,
        state.policy.as_ref(),
        request,
        &mut capture_media_strip_policy,
    )? {
        return Ok(RunOutcome::Escalate(escalation));
    }
    let media_projection = match projection {
        Some("media-degraded" | "strict-media-degraded") => Some("media-degraded"),
        Some("media-stripped" | "strict-media-stripped") => Some("media-stripped"),
        _ => None,
    };
    if media_policy_changed
        && dropped_media_count > 0
        && let Some(media_projection) = media_projection
    {
        let message = if media_projection == "media-degraded" {
            "Provider rejected the request as too large; older media were dropped and the request was retried."
        } else {
            "Provider rejected the media in the request; all media were omitted and the request was retried."
        };
        dispatch_warning(
            deps,
            media_projection.to_string(),
            message.to_string(),
        );
    }
    warn_about_anthropic_thinking_effort(deps, request);
    let log_input = LLMRequestLogInput {
        protocol: request.model.protocol,
        provider_type: request.model.provider_type.clone(),
        model_name: request.model.name.clone(),
        model_alias: Some(request.model_alias.clone()),
        thinking_effort: Some(request.thinking_effort.clone()),
        max_tokens: effective_max_completion_tokens(Some(&request.params)),
        system_prompt: input.system_prompt.clone(),
        tools: input.tools.clone(),
        messages: input.messages.clone(),
        fields,
    };
    log_request(deps, &log_input);
    let recorded = record_request(deps, &log_input);
    let started_at = now_ms();
    let mut message: Option<Message> = None;
    let mut usage: Option<TokenUsage> = None;
    let mut timing: Option<ModelRequestTiming> = None;
    let mut finish: Option<FinishEvent> = None;
    let mut tool_call_ids = deps.tool_call_id_normalizer.lock().begin_response();
    let mut params = request.params.clone();
    params.on_trace_id = Some(on_request_trace.clone());
    let bridge = signal.map(bridge_abort_signal);
    let mut stream = request.requester.request(
        input.clone(),
        bridge.as_ref().map(|bridge| bridge.token.clone()),
        Some(params),
    );
    let stream_result: Result<(), WireAsyncError> = async {
        while let Some(event) = stream.next().await {
            match event? {
                ModelRequestEvent::Part { part } => {
                    let part = normalize_stream_part(&mut tool_call_ids, part);
                    if let Some(on_part) = on_part {
                        on_part(part).await?;
                    }
                }
                ModelRequestEvent::Usage {
                    usage: measured, ..
                } => {
                    usage = Some(measured);
                }
                ModelRequestEvent::Finish {
                    message: finalized,
                    provider_finish_reason,
                    raw_finish_reason,
                    id,
                    trace_id,
                } => {
                    message = Some(finalized);
                    on_request_trace(trace_id.clone());
                    finish = Some(FinishEvent {
                        provider_finish_reason,
                        raw_finish_reason,
                        id,
                        trace_id,
                    });
                }
                ModelRequestEvent::Timing(stream_timing) => {
                    timing = Some(stream_timing);
                }
            }
        }
        Ok(())
    }
    .await;
    if let Err(error) = stream_result {
        tool_call_ids.rollback();
        return Err(error);
    }
    let (Some(mut message), Some(finish)) = (message, finish) else {
        tool_call_ids.rollback();
        return Err(missing_finish_error());
    };
    let original_calls = message.tool_calls.clone();
    let finalized_calls = tool_call_ids.remap_finalized_calls(original_calls.clone());
    if finalized_calls != original_calls {
        message.tool_calls = finalized_calls;
    }
    for remap in &tool_call_ids.remapped {
        deps.log.warn(
            "Rewrote a duplicate provider tool call id into an agent-unique one.",
            Some(json!({
                "raw": remap.raw,
                "assigned": remap.assigned,
                "model": request.model_alias,
            })),
        );
    }
    if request
        .prompt_cache_probe
        .as_ref()
        .is_some_and(|probe| probe.source == PromptCacheProbe::Compaction)
        && !state.cache_probe_reported
    {
        state.cache_probe_reported = true;
        report_cache_probe(
            deps,
            request,
            &input.messages,
            usage.as_ref(),
            cache_probe_baseline,
            &recorded,
            started_at,
        );
    }
    if is_turn_source(request.source.as_ref()) {
        let history_hash = fingerprint(&serde_json::to_string(&input.messages).unwrap_or_default());
        let observation = build_cache_observation(CacheObservationInput {
            model_alias: &request.model_alias,
            protocol: Some(request.model.protocol.as_str()),
            provider_type: request.model.provider_type.as_deref(),
            message_count: recorded.message_count,
            history_hash: &history_hash,
            projection: recorded.projection.map(|projection| projection.as_str()),
            thinking_effort: recorded.thinking_effort.as_deref(),
            usage: usage.as_ref(),
        });
        deps.set_last_turn_request(AgentLLMTurnRequestSnapshot {
            model_alias: request.model_alias.clone(),
            context_revision: request.context_revision,
            tools_hash: recorded.tools_hash.clone(),
            system_prompt_hash: recorded.system_prompt_hash.clone(),
            message_count: input.messages.len(),
            history_hash,
            input_tokens: usage.as_ref().map(input_total),
            started_at,
            finished_at: now_ms(),
            observation: Some(observation),
        });
    }
    let usage_service = deps.usage.clone();
    let agent_context = deps.scope_context.agent_context();
    let recorded_model_alias = request.model_alias.clone();
    let recorded_usage = usage.clone().unwrap_or_default();
    let recorded_source = request.source.clone();
    tokio::spawn(async move {
        let _ = usage_service
            .record(agent_context, recorded_model_alias, recorded_usage, recorded_source)
            .await;
    });
    if let Some(measured_usage) = &usage {
        let request_messages = request
            .messages
            .iter()
            .cloned()
            .map(Message::from)
            .collect::<Vec<_>>();
        deps.token_counting.measured(
            &deps.scope_context.agent_context(),
            &request_messages,
            std::slice::from_ref(&message),
            measured_usage,
        );
    }
    log_response(
        deps,
        &request.log_fields,
        &usage.clone().unwrap_or_default(),
        timing.as_ref(),
    );
    Ok(RunOutcome::Finish(Box::new(AgentLLMRequestFinish {
        message,
        usage: usage.unwrap_or_default(),
        model: Some(request.model_alias.clone()),
        provider_finish_reason: finish.provider_finish_reason,
        raw_finish_reason: finish.raw_finish_reason,
        provider_message_id: finish.id,
        timing,
        trace_id: finish.trace_id,
    })))
}

fn report_cache_probe(
    deps: &RequesterDeps,
    request: &ResolvedLLMRequest,
    messages: &[Message],
    usage: Option<&TokenUsage>,
    baseline: Option<&AgentLLMTurnRequestSnapshot>,
    recorded: &LlmRequestPayload,
    started_at: i64,
) {
    let compared = baseline.is_some_and(|baseline| {
        baseline.model_alias == request.model_alias
            && baseline.context_revision == request.context_revision
    });
    let history_prefix_match = if compared {
        baseline.map(|baseline| {
            baseline.message_count <= messages.len()
                && fingerprint(
                    &serde_json::to_string(&messages[..baseline.message_count]).unwrap_or_default(),
                ) == baseline.history_hash
        })
    } else {
        None
    };
    let own_usage = usage.cloned().unwrap_or_default();
    let mut properties = Map::new();
    properties.insert("source".to_string(), json!("compaction"));
    if let Some(turn_id) = request.source.as_ref().and_then(AgentLLMRequestSource::turn_id) {
        properties.insert("turn_id".to_string(), json!(turn_id));
    }
    properties.insert(
        "provider_type".to_string(),
        json!(request
            .model
            .provider_type
            .clone()
            .unwrap_or_else(|| request.model.protocol.as_str().to_string())),
    );
    properties.insert(
        "protocol".to_string(),
        json!(request.model.protocol.as_str()),
    );
    properties.insert("input_tokens".to_string(), json!(input_total(&own_usage)));
    properties.insert(
        "input_cache_read".to_string(),
        json!(own_usage.input_cache_read),
    );
    properties.insert(
        "input_cache_creation".to_string(),
        json!(own_usage.input_cache_creation),
    );
    properties.insert("output_tokens".to_string(), json!(own_usage.output));
    if let Some(model_match) =
        baseline.map(|baseline| baseline.model_alias == request.model_alias)
    {
        properties.insert("model_match".to_string(), json!(model_match));
    }
    if compared && let Some(baseline) = baseline {
        properties.insert(
            "tools_match".to_string(),
            json!(recorded.tools_hash == baseline.tools_hash),
        );
        properties.insert(
            "system_prompt_match".to_string(),
            json!(recorded.system_prompt_hash == baseline.system_prompt_hash),
        );
        if let Some(parent_input_tokens) = baseline.input_tokens {
            properties.insert("parent_input_tokens".to_string(), json!(parent_input_tokens));
        }
        properties.insert(
            "parent_request_gap_ms".to_string(),
            json!(started_at - baseline.finished_at),
        );
    }
    if let Some(history_prefix_match) = history_prefix_match {
        properties.insert(
            "history_prefix_match".to_string(),
            json!(history_prefix_match),
        );
    }
    deps.telemetry
        .track2("prompt_cache_probe", Some(properties));
}

fn missing_finish_error() -> WireAsyncError {
    Box::new(Error2::new(
        PROVIDER_API_ERROR_CODE.to_string(),
        "LLM request stream ended without a finish event.",
        Error2Options::default(),
    ))
}

fn next_projection_policy_for_error(
    deps: &RequesterDeps,
    error: &WireAsyncError,
    policy: Option<&ProjectionPolicy>,
    request: &ResolvedLLMRequest,
    signal: Option<&AbortSignal>,
    can_strip_media: bool,
    mut capture_media_degrade_policy: Option<
        &mut dyn FnMut() -> Result<MediaProjection, WireAsyncError>,
    >,
    capture_media_strip_policy: &mut dyn FnMut() -> Result<MediaProjection, WireAsyncError>,
) -> Result<Option<ProjectionPolicy>, WireAsyncError> {
    if signal.is_some_and(AbortSignal::aborted) {
        return Ok(None);
    }
    let raw = unwrap_error_cause_ref(error.as_ref());
    let projection: &'static str;
    let next_policy: ProjectionPolicy;
    if raw.downcast_ref::<APIRequestTooLargeError>().is_some() && can_strip_media {
        throw_if_aborted(signal)?;
        match capture_media_degrade_policy.as_mut() {
            Some(capture_media_degrade_policy) => {
                deps.log.warn(
                    "provider rejected request as too large; resending with degraded media",
                    Some(log_payload_for_request(request)),
                );
                projection = "media-degraded";
                next_policy = policy_with_media(policy, capture_media_degrade_policy()?);
            }
            None => {
                deps.log.warn(
                    "provider rejected request as too large; resending with rejected media stripped",
                    Some(log_payload_for_request(request)),
                );
                projection = "media-stripped";
                next_policy = policy_with_media(policy, capture_media_strip_policy()?);
            }
        }
    } else if can_strip_media && is_image_format_error(raw) {
        throw_if_aborted(signal)?;
        deps.log.warn(
            "provider rejected an image in the request; resending with rejected media stripped",
            Some(log_payload_for_request(request)),
        );
        projection = "media-stripped";
        next_policy = policy_with_media(policy, capture_media_strip_policy()?);
    } else if policy.and_then(|policy| policy.structure).is_none()
        && is_recoverable_request_structure_error(raw)
    {
        throw_if_aborted(signal)?;
        deps.log.warn(
            "provider rejected request structure; resending with strict projection",
            Some(log_payload_for_request(request)),
        );
        projection = "strict";
        let mut strict_policy = policy.cloned().unwrap_or_default();
        strict_policy.structure = Some(ProjectionStructure::Strict);
        next_policy = strict_policy;
    } else {
        return Ok(None);
    }
    let mut properties = Map::new();
    properties.insert("projection".to_string(), json!(projection));
    properties.insert(
        "error_type".to_string(),
        json!(api_error_kind_str(classify_api_error(raw).kind)),
    );
    properties.insert("model".to_string(), json!(request.model.id));
    if let Some(turn_id) = request
        .source
        .as_ref()
        .and_then(AgentLLMRequestSource::turn_id)
    {
        properties.insert("turn_id".to_string(), json!(turn_id));
    }
    deps.telemetry
        .track2("llm_request_projection_fallback", Some(properties));
    Ok(Some(next_policy))
}

fn policy_with_media(
    policy: Option<&ProjectionPolicy>,
    media: MediaProjection,
) -> ProjectionPolicy {
    let mut next = policy.cloned().unwrap_or_default();
    next.media = Some(media);
    next
}

fn log_payload_for_request(request: &ResolvedLLMRequest) -> Value {
    let mut payload = LogContext::new();
    payload.insert("model".to_string(), json!(request.model.name));
    payload.extend(request.log_fields.clone());
    log_context_value(payload)
}

fn normalize_stream_part(
    tool_call_ids: &mut crate::human::llm::tool_call_id_normalizer::ToolCallIdResponseNormalizer,
    part: StreamedMessagePart,
) -> StreamedMessagePart {
    let StreamedMessagePart::ToolCall(call) = &part else {
        return part;
    };
    let assigned = tool_call_ids.remap_streamed_id(&call.id, call._stream_index);
    if assigned == call.id {
        return part;
    }
    let raw_id = call.raw_id.clone().or_else(|| Some(call.id.clone()));
    StreamedMessagePart::ToolCall(ToolCall {
        id: assigned,
        raw_id,
        ..call.clone()
    })
}

fn proactive_media_eviction(
    deps: &RequesterDeps,
    messages: &[Message],
    estimated_bytes: u64,
    policy: Option<&ProjectionPolicy>,
    request: &ResolvedLLMRequest,
    capture_media_strip_policy: &mut dyn FnMut() -> Result<MediaProjection, WireAsyncError>,
) -> Result<Option<ProjectionPolicy>, WireAsyncError> {
    let budget_bytes = resolve_request_byte_budget(deps.config.as_ref());
    if estimated_bytes <= budget_bytes {
        return Ok(None);
    }
    if policy.and_then(|policy| policy.media.as_ref()).is_some() {
        return Ok(None);
    }
    let Some(eviction) = deps
        .projector
        .plan_media_eviction(messages, estimated_bytes, budget_bytes)
    else {
        return Ok(None);
    };
    let (evicted_media_count, estimated_bytes_after, projection) = match &eviction {
        MediaEvictionPlan::MediaDegraded {
            evicted_media_count,
            estimated_bytes,
            ..
        } => (*evicted_media_count, *estimated_bytes, "media-degraded"),
        MediaEvictionPlan::MediaStripped {
            evicted_media_count,
            estimated_bytes,
        } => (*evicted_media_count, *estimated_bytes, "media-stripped"),
    };
    let mut payload = LogContext::new();
    payload.insert("model".to_string(), json!(request.model.name));
    payload.extend(request.log_fields.clone());
    payload.insert("budgetBytes".to_string(), json!(budget_bytes));
    payload.insert("estimatedBytesBefore".to_string(), json!(estimated_bytes));
    payload.insert(
        "estimatedBytesAfter".to_string(),
        json!(estimated_bytes_after),
    );
    payload.insert("evictedMediaCount".to_string(), json!(evicted_media_count));
    payload.insert("projection".to_string(), json!(projection));
    deps.log.warn(
        "estimated request size exceeds the budget; evicting older media before send",
        Some(log_context_value(payload)),
    );
    let mut next = policy.cloned().unwrap_or_default();
    if let MediaEvictionPlan::MediaDegraded { degrade_count, .. } = eviction {
        let media = MediaProjection::DegradeCount(degrade_count);
        mark_media_recovery(deps, request, media.clone());
        next.media = Some(media);
        return Ok(Some(next));
    }
    next.media = Some(capture_media_strip_policy()?);
    Ok(Some(next))
}

fn warn_about_anthropic_thinking_effort(deps: &RequesterDeps, request: &ResolvedLLMRequest) {
    if request.model.protocol != Protocol::Anthropic {
        return;
    }
    let effort = &request.thinking_effort;
    if effort == "on" || effort == "off" {
        return;
    }
    let Some(support_efforts) = request.model.support_efforts.as_ref() else {
        return;
    };
    let known_efforts = support_efforts
        .iter()
        .filter(|value| !value.is_empty())
        .cloned()
        .collect::<Vec<_>>();
    if known_efforts.is_empty() {
        return;
    }
    if known_efforts.iter().any(|value| value == effort) {
        return;
    }
    let code = "anthropic-thinking-effort-not-listed";
    let known_efforts_text = known_efforts.join(",");
    let message = format!(
        "Thinking effort \"{effort}\" is not listed for model \"{}\" (known: {}). The configured value will be sent unchanged to the Anthropic-compatible backend.",
        request.model.name,
        known_efforts.join(", ")
    );
    let key = [
        code,
        request.model_alias.as_str(),
        request.model.name.as_str(),
        effort.as_str(),
        known_efforts_text.as_str(),
    ]
    .join("\u{0000}");
    if deps.emitted_thinking_effort_warnings().contains(&key) {
        return;
    }
    deps.add_emitted_thinking_effort_warning(key);
    let mut payload = LogContext::new();
    payload.insert("modelAlias".to_string(), json!(request.model_alias));
    payload.insert("model".to_string(), json!(request.model.name));
    payload.insert("effort".to_string(), json!(effort));
    payload.insert("knownEfforts".to_string(), json!(known_efforts_text));
    deps.log.warn(&message, Some(log_context_value(payload)));
    dispatch_warning(deps, code.to_string(), message);
}

fn recovered_media(deps: &RequesterDeps, request: &ResolvedLLMRequest) -> Option<MediaProjection> {
    let recovery = deps.media_recovery()?;
    if recovery.model_alias != request.model_alias
        || recovery.context_revision != request.context_revision
    {
        return None;
    }
    Some(recovery.media)
}

fn mark_media_recovery(
    deps: &RequesterDeps,
    request: &ResolvedLLMRequest,
    media: MediaProjection,
) {
    if !is_turn_source(request.source.as_ref()) {
        return;
    }
    deps.set_media_recovery(&MediaRecovery {
        model_alias: request.model_alias.clone(),
        context_revision: request.context_revision,
        media,
    });
}

fn dispatch_warning(deps: &RequesterDeps, code: String, message: String) {
    let event = WarningIssued::new(WarningIssuedPayload {
        agent_id: deps.scope_context.agent_id(),
        message,
        code: Some(code),
    });
    let dispatcher = deps.dispatcher.clone();
    tokio::spawn(async move {
        let _ = dispatcher.dispatch(event.into()).await;
    });
}

fn throw_if_aborted(signal: Option<&AbortSignal>) -> Result<(), WireAsyncError> {
    if signal.is_some_and(AbortSignal::aborted) {
        return Err(create_abort_error());
    }
    Ok(())
}

fn is_turn_source(source: Option<&AgentLLMRequestSource>) -> bool {
    matches!(source, Some(AgentLLMRequestSource::Turn { .. }))
}

async fn resolve_request(
    deps: &Arc<RequesterDeps>,
    overrides: &AgentLLMRequestOverrides,
    prompt_cache_probe: Option<PromptCacheProbeState>,
    signal: Option<&AbortSignal>,
) -> Result<ResolvedLLMRequest, WireAsyncError> {
    let bridge = signal.map(bridge_abort_signal);
    deps.tool_select
        .prepare_for_request(bridge.as_ref().map(|bridge| bridge.token.clone()))
        .await?;
    let turn_config = resolve_turn_config(deps, overrides.source.as_ref());
    let resolved = turn_config
        .as_ref()
        .map(|config| config.resolved.clone())
        .unwrap_or_else(|| deps.profile.resolve_model_context());
    let base_params = turn_config
        .as_ref()
        .map(|config| config.params.clone())
        .unwrap_or_else(|| deps.profile.resolve_request_params());
    let requester = deps.model_catalog.get_requester(&resolved.model_alias)?;
    let model = requester.model().clone();
    let max_completion_tokens_cap = deps
        .config
        .get(MODEL_OVERRIDES_SECTION)
        .and_then(|value| serde_json::from_value::<ModelOverrides>(value).ok())
        .and_then(|overrides| overrides.max_completion_tokens);
    let used_context_tokens = match overrides.used_context_tokens {
        Some(used_context_tokens) => Some(used_context_tokens),
        None if overrides.messages.is_none() => Some(
            u32::try_from(
                deps.token_counting
                    .get(&deps.scope_context.agent_context(), None, None)
                    .size,
            )
            .unwrap_or(u32::MAX),
        ),
        None => None,
    };
    let budget = resolve_completion_budget(
        overrides.max_output_size.or(resolved.max_output_size),
        max_completion_tokens_cap,
    );
    let budget_params =
        completion_budget_params(budget, Some(&resolved.model_capabilities), used_context_tokens);
    let opted_out = max_completion_tokens_cap == Some(0);
    let messages = match &overrides.messages {
        Some(messages) => messages
            .iter()
            .cloned()
            .map(ContextMessage::from)
            .collect::<Vec<_>>(),
        None => deps.context.get(),
    };
    let mut params = base_params;
    if let Some(budget_params) = budget_params {
        params.max_completion_tokens = Some(budget_params.max_completion_tokens);
        params.used_context_tokens = budget_params.used_context_tokens;
        params.max_context_tokens = budget_params.max_context_tokens;
    }
    if let Some(used_context_tokens) = used_context_tokens {
        params.used_context_tokens = Some(used_context_tokens);
    }
    if opted_out {
        params.max_completion_tokens = max_completion_tokens_cap;
    }
    let system_prompt = overrides
        .system_prompt
        .clone()
        .or_else(|| {
            turn_config
                .as_ref()
                .map(|config| config.system_prompt.clone())
        })
        .unwrap_or_else(|| deps.profile.get_system_prompt());
    let tools = overrides
        .tools
        .clone()
        .unwrap_or_else(|| default_tools(deps));
    Ok(ResolvedLLMRequest {
        requester,
        model,
        params,
        model_alias: resolved.model_alias.clone(),
        thinking_effort: resolved.thinking_level.clone(),
        system_prompt,
        tools,
        messages,
        context_revision: deps.context.prefix_revision(),
        prompt_cache_probe,
        source: overrides.source.clone(),
        log_fields: log_fields_for_source(overrides.source.as_ref()),
    })
}

fn resolve_turn_config(
    deps: &RequesterDeps,
    source: Option<&AgentLLMRequestSource>,
) -> Option<TurnRequestConfig> {
    match source {
        Some(AgentLLMRequestSource::Turn { turn_id, .. }) => {
            Some(get_or_create_turn_config(deps, *turn_id))
        }
        _ => None,
    }
}

fn get_or_create_turn_config(deps: &RequesterDeps, turn_id: u64) -> TurnRequestConfig {
    let mut configs = deps.turn_configs();
    let previous_len = configs.len();
    configs.retain(|id, _| *id >= turn_id);
    let pruned = configs.len() != previous_len;
    let (config, created) = match configs.get(&turn_id) {
        Some(config) => (config.clone(), false),
        None => {
            let config = TurnRequestConfig {
                resolved: deps.profile.resolve_model_context(),
                params: deps.profile.resolve_request_params(),
                system_prompt: deps.profile.get_system_prompt(),
            };
            configs.insert(turn_id, config.clone());
            (config, true)
        }
    };
    if pruned || created {
        deps.set_turn_configs(&configs);
    }
    config
}

fn default_tools(deps: &RequesterDeps) -> Vec<Tool> {
    deps.tool_select.default_tools(None)
}

fn log_request(deps: &RequesterDeps, input: &LLMRequestLogInput) {
    let wire_tools = provider_visible_tools(&input.tools);
    let mut config = json!({
        "provider": input.protocol.as_str(),
        "model": input.model_name,
        "modelAlias": input.model_alias,
        "thinkingEffort": input.thinking_effort,
        "systemPromptChars": input.system_prompt.encode_utf16().count(),
        "toolCount": wire_tools.len(),
    });
    let mut signature = config.clone();
    if let Some(object) = signature.as_object_mut() {
        object.insert(
            "systemPromptHash".to_string(),
            json!(fingerprint(&input.system_prompt)),
        );
        object.insert(
            "toolsHash".to_string(),
            json!(fingerprint(
                &serde_json::to_string(&tool_signature(&wire_tools)).unwrap_or_default()
            )),
        );
    }
    let signature_text = signature.to_string();
    if deps.last_config_log_signature().as_deref() != Some(signature_text.as_str()) {
        deps.set_last_config_log_signature(Some(signature_text));
        let mut payload = input.fields.clone();
        if let Some(object) = config.as_object_mut() {
            for (key, value) in object.iter() {
                payload.insert(key.clone(), value.clone());
            }
        }
        deps.log
            .info("llm config", Some(log_context_value(payload)));
    }
    let partial_message_count = input
        .messages
        .iter()
        .filter(|message| message.partial == Some(true))
        .count();
    let mut request_fields = input.fields.clone();
    if partial_message_count > 0 {
        request_fields.insert(
            "partialMessageCount".to_string(),
            json!(partial_message_count),
        );
    }
    deps.log
        .info("llm request", Some(log_context_value(request_fields)));
}

fn record_request(deps: &RequesterDeps, input: &LLMRequestLogInput) -> LlmRequestPayload {
    let fields = &input.fields;
    let wire_tools = provider_visible_tools(&input.tools);
    let tools = tool_signature(&wire_tools);
    let tools_hash = fingerprint(&serde_json::to_string(&tools).unwrap_or_default());
    let seen_tools_hashes = deps
        .states
        .get(&llm_request_trace_key().state_key())
        .get("seenToolsHashes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if !seen_tools_hashes
        .iter()
        .any(|hash| hash.as_str() == Some(tools_hash.as_str()))
    {
        let snapshot = LlmToolsSnapshot::new(LlmToolsSnapshotPayload {
            agent_id: deps.scope_context.agent_id(),
            hash: tools_hash.clone(),
            tools,
        });
        let dispatcher = deps.dispatcher.clone();
        tokio::spawn(async move {
            let _ = dispatcher.dispatch(snapshot.into()).await;
        });
    }
    let system_prompt_hash = fingerprint(&input.system_prompt);
    let overrides = deps
        .config
        .get(MODEL_OVERRIDES_SECTION)
        .and_then(|value| serde_json::from_value::<ModelOverrides>(value).ok());
    let thinking_config = deps
        .config
        .get(THINKING_SECTION)
        .and_then(|value| serde_json::from_value::<ThinkingConfig>(value).ok());
    let model_config = input
        .model_alias
        .as_ref()
        .and_then(|model_alias| deps.model_service.get(model_alias));
    let thinking_effort = input.thinking_effort.as_deref().unwrap_or("off");
    let payload = LlmRequestPayload {
        agent_id: deps.scope_context.agent_id(),
        kind: request_kind_for_record(fields),
        provider: input.protocol.as_str().to_string(),
        model: input.model_name.clone(),
        model_alias: input.model_alias.clone(),
        thinking_effort: input.thinking_effort.clone(),
        thinking_keep: resolve_thinking_keep(
            overrides
                .as_ref()
                .and_then(|overrides| overrides.thinking_keep.as_deref()),
            thinking_config
                .as_ref()
                .and_then(|config| config.keep.as_deref()),
            thinking_effort,
        ),
        temperature: overrides.as_ref().and_then(|overrides| overrides.temperature),
        top_p: overrides.as_ref().and_then(|overrides| overrides.top_p),
        max_tokens: input.max_tokens.map(u64::from),
        beta_api: model_config.and_then(|model| model.beta_api),
        tool_select: deps.tool_select.enabled(None),
        system_prompt_hash,
        system_prompt: (input.system_prompt != deps.profile.data().base.system_prompt)
            .then(|| input.system_prompt.clone()),
        tools_hash,
        message_count: input.messages.len(),
        turn_step: string_field(fields, "turnStep"),
        attempt: string_field(fields, "attempt"),
        projection: projection_field(fields),
        dropped_count: number_field(fields, "droppedCount").map(|value| value.max(0.0) as usize),
    };
    let event = LlmRequest::new(payload.clone());
    let dispatcher = deps.dispatcher.clone();
    tokio::spawn(async move {
        let _ = dispatcher.dispatch(event.into()).await;
    });
    payload
}

fn log_response(
    deps: &RequesterDeps,
    fields: &AgentLLMRequestLogFields,
    usage: &TokenUsage,
    timing: Option<&ModelRequestTiming>,
) {
    let Some(timing) = timing else {
        return;
    };
    let mut payload = fields.clone();
    payload.insert("ttftMs".to_string(), json!(timing.first_token_latency_ms));
    payload.insert(
        "streamDurationMs".to_string(),
        json!(timing.stream_duration_ms),
    );
    payload.insert("outputTokens".to_string(), json!(usage.output));
    if let Some(request_build_ms) = timing.request_build_ms {
        payload.insert("requestBuildMs".to_string(), json!(request_build_ms));
    }
    if let Some(server_first_token_ms) = timing.server_first_token_ms {
        payload.insert(
            "serverFirstTokenMs".to_string(),
            json!(server_first_token_ms),
        );
    }
    if let Some(server_decode_ms) = timing.server_decode_ms {
        payload.insert("serverDecodeMs".to_string(), json!(server_decode_ms));
    }
    if let Some(client_consume_ms) = timing.client_consume_ms {
        payload.insert("clientConsumeMs".to_string(), json!(client_consume_ms));
    }
    if let Some(client_blocked_ms) = timing.client_blocked_ms {
        payload.insert("clientBlockedMs".to_string(), json!(client_blocked_ms));
    }
    deps.log
        .info("llm response", Some(log_context_value(payload)));
}

fn log_fields_for_source(source: Option<&AgentLLMRequestSource>) -> AgentLLMRequestLogFields {
    match source {
        Some(AgentLLMRequestSource::Turn {
            turn_id,
            step,
            log_fields,
        }) => {
            let mut fields = log_fields.clone().unwrap_or_default();
            if let Some(step) = step {
                fields.insert("turnStep".to_string(), json!(format!("{turn_id}.{step}")));
            }
            fields
        }
        Some(AgentLLMRequestSource::Operation {
            request_kind,
            log_fields,
            ..
        }) => {
            let mut fields = log_fields.clone().unwrap_or_default();
            if let Some(request_kind) = request_kind {
                fields.insert("requestKind".to_string(), json!(request_kind));
            }
            fields
        }
        None => AgentLLMRequestLogFields::new(),
    }
}

fn request_kind_for_telemetry(source: Option<&AgentLLMRequestSource>) -> Option<String> {
    match source {
        Some(AgentLLMRequestSource::Turn { .. }) => Some("turn".to_string()),
        Some(AgentLLMRequestSource::Operation { request_kind, .. }) => {
            Some(request_kind.clone().unwrap_or_else(|| "operation".to_string()))
        }
        None => None,
    }
}

fn request_kind_for_record(fields: &AgentLLMRequestLogFields) -> LlmRequestKind {
    if fields.get("kind").and_then(Value::as_str) == Some("compaction") {
        return LlmRequestKind::Compaction;
    }
    if fields.get("requestKind").and_then(Value::as_str) == Some("full_compaction") {
        return LlmRequestKind::Compaction;
    }
    LlmRequestKind::Loop
}

fn string_field(fields: &AgentLLMRequestLogFields, key: &str) -> Option<String> {
    fields.get(key).and_then(Value::as_str).map(str::to_string)
}

fn number_field(fields: &AgentLLMRequestLogFields, key: &str) -> Option<f64> {
    fields.get(key).and_then(Value::as_f64)
}

fn projection_field(fields: &AgentLLMRequestLogFields) -> Option<LlmRequestProjection> {
    match fields.get("projection").and_then(Value::as_str) {
        Some("strict") => Some(LlmRequestProjection::Strict),
        Some("media-degraded") => Some(LlmRequestProjection::MediaDegraded),
        Some("media-stripped") => Some(LlmRequestProjection::MediaStripped),
        Some("strict-media-degraded") => Some(LlmRequestProjection::StrictMediaDegraded),
        Some("strict-media-stripped") => Some(LlmRequestProjection::StrictMediaStripped),
        _ => None,
    }
}

fn projection_name_of(policy: Option<&ProjectionPolicy>) -> Option<&'static str> {
    let media = policy.and_then(|policy| policy.media.as_ref());
    if policy.and_then(|policy| policy.structure) == Some(ProjectionStructure::Strict) {
        return match media {
            None => Some("strict"),
            Some(MediaProjection::DegradeCount(_)) => Some("strict-media-degraded"),
            Some(MediaProjection::Strip(_)) => Some("strict-media-stripped"),
        };
    }
    match media {
        None => None,
        Some(MediaProjection::DegradeCount(_)) => Some("media-degraded"),
        Some(MediaProjection::Strip(_)) => Some("media-stripped"),
    }
}

fn media_part_count(messages: &[Message]) -> usize {
    messages
        .iter()
        .map(|message| {
            message
                .content
                .iter()
                .filter(|part| {
                    matches!(
                        part,
                        ContentPart::ImageUrl { .. }
                            | ContentPart::AudioUrl { .. }
                            | ContentPart::VideoUrl { .. }
                    )
                })
                .count()
        })
        .sum()
}

fn media_tag_of(message: &Message) -> Option<MediaPathTag> {
    let text = message
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    match_media_path_tags(&text)
        .into_iter()
        .find(|tag| matches!(tag.kind, MediaPathTagKind::Image | MediaPathTagKind::Video))
}

fn omitted_read_media_result(tag: &MediaPathTag) -> ContextMessage {
    let kind = tag.kind.as_str();
    let hint = format!(
        "[{kind} omitted: a ReadMediaFile tool result was hidden because the current model does not support {kind} input; use a model with {kind} input capability to view it]"
    );
    let text = format!("{hint}\n{}", build_media_path_tag(tag.kind, &tag.path));
    ContextMessage::from(Message::new(
        Role::User,
        vec![ContentPart::Text { text, meta: None }],
    ))
}

fn hide_read_media_file(messages: Vec<ContextMessage>) -> Vec<ContextMessage> {
    let mut read_media_call_ids = HashSet::new();
    for message in &messages {
        if message.role != Role::Assistant {
            continue;
        }
        for call in &message.tool_calls {
            if call.name == READ_MEDIA_FILE_TOOL_NAME {
                read_media_call_ids.insert(call.id.clone());
            }
        }
    }
    if read_media_call_ids.is_empty() {
        return messages;
    }
    let mut media_tags = HashMap::new();
    for message in &messages {
        if message.role != Role::Tool {
            continue;
        }
        let Some(tool_call_id) = &message.tool_call_id else {
            continue;
        };
        if !read_media_call_ids.contains(tool_call_id) {
            continue;
        }
        if let Some(tag) = media_tag_of(&Message::from(message.clone())) {
            media_tags.insert(tool_call_id.clone(), tag);
        }
    }
    let mut out = Vec::new();
    for message in messages {
        if message.role == Role::Tool {
            let tool_call_id = message.tool_call_id.clone();
            let Some(tool_call_id) = tool_call_id else {
                out.push(message);
                continue;
            };
            if !read_media_call_ids.contains(&tool_call_id) {
                out.push(message);
                continue;
            }
            if let Some(tag) = media_tags.get(&tool_call_id) {
                out.push(omitted_read_media_result(tag));
            }
            continue;
        }
        if message.role != Role::Assistant {
            out.push(message);
            continue;
        }
        let tool_calls = message
            .tool_calls
            .iter()
            .filter(|call| call.name != READ_MEDIA_FILE_TOOL_NAME)
            .cloned()
            .collect::<Vec<_>>();
        if tool_calls.len() == message.tool_calls.len() {
            out.push(message);
            continue;
        }
        if !tool_calls.is_empty() || !message.content.is_empty() {
            let mut message = message;
            message.tool_calls = tool_calls;
            out.push(message);
        }
    }
    out
}

fn provider_visible_tools(tools: &[Tool]) -> Vec<Tool> {
    if !tools.iter().any(|tool| tool.deferred == Some(true)) {
        return tools.to_vec();
    }
    tools
        .iter()
        .filter(|tool| tool.deferred != Some(true))
        .cloned()
        .collect()
}

fn tool_signature(tools: &[Tool]) -> Vec<LlmRequestToolSchema> {
    tools
        .iter()
        .map(|tool| LlmRequestToolSchema {
            name: tool.name.clone(),
            description: tool.description.clone(),
            parameters: tool.parameters.clone(),
        })
        .collect()
}

fn seed_messages(messages: &[ContextMessage]) -> Vec<ToolCallIdSeedMessage> {
    messages
        .iter()
        .map(|message| ToolCallIdSeedMessage {
            role: Some(
                match message.role {
                    Role::System => "system",
                    Role::User => "user",
                    Role::Assistant => "assistant",
                    Role::Tool => "tool",
                }
                .to_string(),
            ),
            tool_calls: Some(
                message
                    .tool_calls
                    .iter()
                    .map(|call| ToolCallIdSeed {
                        id: call.id.clone(),
                    })
                    .collect(),
            ),
            tool_call_id: message.tool_call_id.clone(),
        })
        .collect()
}

fn api_error_kind_str(kind: ApiErrorKind) -> &'static str {
    match kind {
        ApiErrorKind::ContextOverflow => "context_overflow",
        ApiErrorKind::Overloaded => "overloaded",
        ApiErrorKind::RateLimit => "rate_limit",
        ApiErrorKind::QuotaExhausted => "quota_exhausted",
        ApiErrorKind::Auth => "auth",
        ApiErrorKind::Server5xx => "5xx_server",
        ApiErrorKind::Client4xx => "4xx_client",
        ApiErrorKind::Network => "network",
        ApiErrorKind::Timeout => "timeout",
        ApiErrorKind::EmptyResponse => "empty_response",
        ApiErrorKind::Other => "other",
    }
}

fn status_error_of<'a>(error: &'a (dyn Error + Send + Sync + 'static)) -> Option<&'a APIStatusError> {
    if let Some(error) = error.downcast_ref::<APIStatusError>() {
        return Some(error);
    }
    if let Some(error) = error.downcast_ref::<APIContextOverflowError>() {
        return Some(&error.0);
    }
    if let Some(error) = error.downcast_ref::<APIRequestTooLargeError>() {
        return Some(&error.0);
    }
    if let Some(error) = error.downcast_ref::<APIProviderRateLimitError>() {
        return Some(&error.0);
    }
    if let Some(error) = error.downcast_ref::<APIProviderQuotaExhaustedError>() {
        return Some(&error.0);
    }
    error
        .downcast_ref::<APIProviderOverloadedError>()
        .map(|error| &error.0)
}

fn api_status_code(error: &(dyn Error + Send + Sync + 'static)) -> Option<i64> {
    let raw = unwrap_error_cause_ref(error);
    if let Some(status_code) = status_error_of(raw).and_then(|error| error.status_code) {
        return Some(status_code);
    }
    if let Some(coded) = raw.downcast_ref::<Error2>()
        && let Some(details) = &coded.details
        && let Some(status_code) = details
            .get("statusCode")
            .or_else(|| details.get("status"))
            .and_then(Value::as_i64)
    {
        return Some(status_code);
    }
    if let Some(coded) = raw.downcast_ref::<kimi_core::primitives::errors::Error2>()
        && let Some(details) = coded.details()
        && let Some(status_code) = details
            .get("statusCode")
            .or_else(|| details.get("status"))
            .and_then(Value::as_i64)
    {
        return Some(status_code);
    }
    None
}

fn api_trace_id(error: &(dyn Error + Send + Sync + 'static)) -> Option<String> {
    let raw = unwrap_error_cause_ref(error);
    if let Some(trace_id) = status_error_of(raw).and_then(|error| error.trace_id.clone()) {
        return Some(trace_id);
    }
    if let Some(coded) = raw.downcast_ref::<Error2>()
        && let Some(details) = &coded.details
        && let Some(trace_id) = details.get("traceId").and_then(Value::as_str)
    {
        return Some(trace_id.to_string());
    }
    if let Some(coded) = raw.downcast_ref::<kimi_core::primitives::errors::Error2>()
        && let Some(details) = coded.details()
        && let Some(trace_id) = details.get("traceId").and_then(Value::as_str)
    {
        return Some(trace_id.to_string());
    }
    None
}

fn retry_after_ms_of(error: &(dyn Error + Send + Sync + 'static)) -> Option<f64> {
    status_error_of(error)
        .and_then(|error| error.retry_after_ms)
        .filter(|value| *value > 0)
        .map(|value| value as f64)
}

fn retry_error_fields(error: &(dyn Error + Send + Sync + 'static)) -> LogContext {
    let raw = unwrap_error_cause_ref(error);
    let mut fields = LogContext::new();
    fields.insert("error_name".to_string(), json!(error_name_of(raw)));
    fields.insert(
        "error_message".to_string(),
        json!(error_message_with_cause(raw)),
    );
    if let Some(status_code) = api_status_code(raw) {
        fields.insert("status_code".to_string(), json!(status_code as f64));
    }
    fields
}

fn error_name_of(error: &(dyn Error + Send + Sync + 'static)) -> String {
    if let Some(coded) = error.downcast_ref::<Error2>() {
        return coded.name.clone();
    }
    if let Some(coded) = error.downcast_ref::<kimi_core::primitives::errors::Error2>() {
        return coded.name().to_string();
    }
    if error.is::<crate::_base::utils::abort::AbortError>()
        || error.is::<crate::_base::utils::abort::UserCancellationError>()
    {
        return "AbortError".to_string();
    }
    std::any::type_name_of_val(error).to_string()
}

fn error_message_with_cause(error: &(dyn Error + Send + Sync + 'static)) -> String {
    let message = error.to_string();
    let mut parts = Vec::new();
    let mut current = error.source();
    while parts.len() < 3 {
        let Some(cause) = current else {
            break;
        };
        let cause_message = cause.to_string();
        if !cause_message.is_empty() {
            let text = if let Some(coded) = cause.downcast_ref::<Error2>() {
                if coded.code.is_empty() {
                    format!("{}: {cause_message}", coded.name)
                } else {
                    format!("{}: {cause_message}", coded.code)
                }
            } else if let Some(coded) = cause.downcast_ref::<kimi_core::primitives::errors::Error2>() {
                format!("{}: {cause_message}", coded.code().as_str())
            } else {
                format!("{}: {cause_message}", std::any::type_name_of_val(cause))
            };
            if !message.contains(&cause_message) && !parts.contains(&text) {
                parts.push(text);
            }
        }
        current = cause.source();
    }
    if parts.is_empty() {
        message
    } else if message.is_empty() {
        parts.join("; ")
    } else {
        format!("{message} ({})", parts.join("; "))
    }
}

fn unwrap_error_cause_ref<'a>(
    error: &'a (dyn Error + Send + Sync + 'static),
) -> &'a (dyn Error + Send + Sync + 'static) {
    if let Some(coded) = error.downcast_ref::<Error2>() {
        return match coded.cause.as_deref() {
            Some(cause) => unwrap_error_cause_ref(cause),
            None => error,
        };
    }
    if let Some(coded) = error.downcast_ref::<kimi_core::primitives::errors::Error2>() {
        return match coded.cause() {
            Some(cause) => unwrap_error_cause_ref(cause),
            None => error,
        };
    }
    error
}

fn log_context_value(context: LogContext) -> Value {
    Value::Object(context.into_iter().collect())
}

fn media_projection_to_value(media: &MediaProjection) -> Value {
    match media {
        MediaProjection::DegradeCount(degrade_count) => json!({ "degradeCount": degrade_count }),
        MediaProjection::Strip(snapshot) => {
            let mut keys = snapshot.keys.iter().cloned().collect::<Vec<_>>();
            keys.sort();
            json!({ "strip": { "keys": keys } })
        }
    }
}

fn media_projection_from_value(value: &Value) -> Option<MediaProjection> {
    if let Some(degrade_count) = value.get("degradeCount").and_then(Value::as_u64) {
        return Some(MediaProjection::DegradeCount(degrade_count as usize));
    }
    let keys = value.get("strip")?.get("keys")?.as_array()?;
    let keys = keys
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect::<HashSet<_>>();
    Some(MediaProjection::Strip(MediaStripSnapshot { keys }))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

struct HostTimersSleep {
    timers: Arc<dyn IHostTimers>,
}

impl HostTimersSleep {
    fn new(timers: Arc<dyn IHostTimers>) -> Self {
        Self { timers }
    }
}

impl RetrySleepTimers for HostTimersSleep {
    fn set_timeout(&self, callback: Box<dyn FnOnce() + Send>, timeout_ms: f64) {
        let callback = Arc::new(Mutex::new(Some(callback)));
        self.timers.set_timeout(
            Arc::new(move || {
                if let Some(callback) = callback.lock().take() {
                    callback();
                }
            }),
            timeout_ms.max(0.0) as u64,
        );
    }
}

struct AbortSignalBridge {
    token: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for AbortSignalBridge {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn bridge_abort_signal(signal: &AbortSignal) -> AbortSignalBridge {
    let token = CancellationToken::new();
    if signal.aborted() {
        token.cancel();
    }
    let source = signal.clone();
    let forward = token.clone();
    let task = tokio::spawn(async move {
        while !source.aborted() {
            tokio::time::sleep(ABORT_SIGNAL_POLL_INTERVAL).await;
        }
        forward.cancel();
    });
    AbortSignalBridge { token, task }
}

impl IAgentLLMRequesterService for AgentLLMRequesterService {
    fn prepare_turn_config(&self, turn_id: u64) -> Option<PreparedTurnRequestConfig> {
        AgentLLMRequesterService::prepare_turn_config(self, turn_id)
    }

    fn current_credential_provider(&self) -> Option<Arc<dyn LlmCredentialProvider>> {
        AgentLLMRequesterService::current_credential_provider(self)
    }

    fn credential_provider_for_turn(
        &self,
        turn_id: u64,
    ) -> Option<Arc<dyn LlmCredentialProvider>> {
        AgentLLMRequesterService::credential_provider_for_turn(self, turn_id)
    }

    fn last_turn_request(&self) -> Option<AgentLLMTurnRequestSnapshot> {
        AgentLLMRequesterService::last_turn_request(self)
    }

    fn request(
        &self,
        overrides: Option<AgentLLMRequestOverrides>,
        on_part: Option<AgentLLMRequestPartHandler>,
        signal: Option<AbortSignal>,
    ) -> BoxFuture<'static, Result<AgentLLMRequestFinish, WireAsyncError>> {
        AgentLLMRequesterService::request(self, overrides, on_part, signal)
    }

    fn start(
        &self,
        overrides: Option<AgentLLMRequestOverrides>,
        on_part: Option<AgentLLMRequestPartHandler>,
        signal: Option<AbortSignal>,
    ) -> AgentLLMRequestTask {
        AgentLLMRequesterService::start(self, overrides, on_part, signal)
    }
}
