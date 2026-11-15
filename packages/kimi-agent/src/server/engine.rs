use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::callbacks::HostCallbacks;
use crate::mcp::manager::McpManager;
use crate::pipeline::{PipelineHost, PipelineSpec, build_engine_pipeline};
use crate::primitives::read::ReadOutcome;
use crate::rpc::types::{
    AskQuestionRequest, AskQuestionResponse, BoxFuture, ContentPart, LlmChatRequest,
    LlmChatResponse, PermissionCheckRequest, PermissionDecision, ToolExecuteRequest,
    ToolExecuteResponse,
};
use kimi_core::types::TokenUsage;
use crate::server::hub::EventHub;
use crate::server::interaction::InteractionManager;
use crate::session::sqlite_store::SqliteSessionStore;
use crate::subagent::SubagentManager;
use crate::tools::{memory_filing, memory_store};
use crate::turn_loop::run_turn::run_turn_continued_with_observations;
use crate::turn_loop::types::{LLM, LLMMessage, RunTurnInput};
use serde_json::Value;

#[derive(Clone, Default)]
pub struct ServerHost {
    interaction_manager: Option<Arc<InteractionManager>>,
    session_id: Option<String>,

    oauth: Option<Arc<crate::server::oauth::OAuthManager>>,
}

impl ServerHost {
    pub fn standalone() -> Self {
        Self {
            interaction_manager: None,
            session_id: None,
            oauth: None,
        }
    }

    pub fn with_interaction(manager: Arc<InteractionManager>, session_id: String) -> Self {
        Self {
            interaction_manager: Some(manager),
            session_id: Some(session_id),
            oauth: None,
        }
    }

    #[must_use]
    pub fn with_oauth(mut self, oauth: Option<Arc<crate::server::oauth::OAuthManager>>) -> Self {
        self.oauth = oauth;
        self
    }
}

impl HostCallbacks for ServerHost {
    fn auth_token(
        &self,
        _provider: String,
        _force: bool,
    ) -> BoxFuture<'static, Result<String, String>> {
        match self.oauth.clone() {
            Some(oauth) => Box::pin(async move { oauth.managed_access_token().await }),
            None => Box::pin(async { Err("host does not support oauth token fetch".into()) }),
        }
    }

    fn llm_chat(
        &self,
        _request: LlmChatRequest,
    ) -> BoxFuture<'static, Result<LlmChatResponse, String>> {
        Box::pin(async {
            Err(
                "this server has no LLM host: configure providers or native_llm \
                 (the engine runs self-contained)"
                    .into(),
            )
        })
    }

    fn execute_tool(
        &self,
        request: ToolExecuteRequest,
    ) -> BoxFuture<'static, Result<ToolExecuteResponse, String>> {
        Box::pin(async move {
            Err(format!(
                "tool `{}` is not available in the standalone engine",
                request.tool_name
            ))
        })
    }

    fn check_permission(
        &self,
        request: PermissionCheckRequest,
    ) -> BoxFuture<'static, Result<PermissionDecision, String>> {
        if std::env::var("KIMI_AUTO_APPROVE")
            .map(|v| v == "1" || v == "true")
            .unwrap_or(false)
        {
            return Box::pin(async { Ok(PermissionDecision::allow()) });
        }
        if let (Some(mgr), Some(sid)) = (self.interaction_manager.clone(), self.session_id.clone())
        {
            Box::pin(async move {
                let (_aid, rx) = mgr.register_approval(&sid, request, "tool_execution");
                match rx.await {
                    Ok(decision) => Ok(decision),
                    Err(_) => Ok(PermissionDecision::deny("Interaction channel closed")),
                }
            })
        } else {
            Box::pin(async {
                Ok(PermissionDecision::deny(
                    "no interactive approver in the standalone engine",
                ))
            })
        }
    }

    fn ask_question(
        &self,
        request: AskQuestionRequest,
    ) -> BoxFuture<'static, Result<AskQuestionResponse, String>> {
        if let (Some(mgr), Some(sid)) = (self.interaction_manager.clone(), self.session_id.clone())
        {
            Box::pin(async move {
                let rx = mgr.register_question(&sid, request);
                match rx.await {
                    Ok(resp) => Ok(resp),
                    Err(_) => Ok(AskQuestionResponse {
                        answers: HashMap::new(),
                        method: None,
                        note: None,
                        cancelled: Some(true),
                        reason: Some("interaction_cancelled".into()),
                    }),
                }
            })
        } else {
            Box::pin(async {
                Err(
                    "The connected client does not support interactive questions. \
                     Do NOT call this tool again. Ask the user directly in your text response instead."
                        .into(),
                )
            })
        }
    }

    fn cancel_question(&self, question_id: &str, reason: &str) {
        if let Some(manager) = self.interaction_manager.as_ref() {
            manager.cancel_question(question_id, reason);
        }
    }
}

#[derive(Debug, Clone)]
pub struct TurnReport {
    pub turn_id: String,
    pub stop_reason: String,

    pub reply: String,
    pub steps: u32,
    pub usage: TokenUsage,

    pub events_emitted: u32,
    pub llm_transport: String,
    pub native_tool_calls: u32,

    pub context_window: Option<u32>,

    pub gate_blocked: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ModelThinking {
    pub always_thinking: bool,
    pub support_efforts: Vec<String>,
    pub default_effort: Option<String>,
}

#[derive(Debug)]
pub enum EngineError {
    NoModel(String),

    Turn(String),

    Store(rusqlite::Error),

    Occupied(String),

    Stale(String),

    Environment(String),
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoModel(why) => write!(f, "no model configured: {why}"),
            Self::Turn(why) => write!(f, "turn failed: {why}"),
            Self::Store(error) => write!(f, "session store error: {error}"),
            Self::Occupied(message) => f.write_str(message),
            Self::Stale(message) => f.write_str(message),
            Self::Environment(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for EngineError {}

pub struct ServerEngine {
    spec: PipelineSpec,
    hub: Arc<EventHub>,
    store: Arc<SqliteSessionStore>,
    max_steps: u32,

    max_attempts: Option<u32>,

    compaction_max_attempts: Option<u32>,
    active_turns: Mutex<HashMap<String, Arc<AtomicBool>>>,

    active_turn_numbers: Mutex<HashMap<String, u32>>,

    journal_heads: Mutex<HashMap<String, u64>>,
    mcp_manager: Mutex<Option<Arc<McpManager>>>,

    mcp_status: Mutex<HashMap<String, Arc<crate::mcp::status::McpStatusBridge>>>,
    interaction_manager: Mutex<Option<Arc<InteractionManager>>>,
    subagent_manager: Arc<SubagentManager>,

    oauth_manager: Mutex<Option<Arc<crate::server::oauth::OAuthManager>>>,

    steer_queues: Mutex<HashMap<String, Arc<Mutex<Vec<LLMMessage>>>>>,

    steer_slots: Mutex<HashMap<String, Arc<Mutex<Option<crate::subagent::types::ParentCancel>>>>>,

    config_source: Mutex<Option<Arc<tokio::sync::Mutex<Option<crate::config::KimiConfig>>>>>,

    hooks_source: Mutex<Option<crate::tools::external_hooks::HooksSource>>,

    token_counting_strategy_cache:
        Mutex<Option<crate::session::token_counting::TokenCountingStrategy>>,

    plugin_skill_roots: Mutex<Option<PluginSkillRoots>>,

    plugin_contributions: Mutex<Option<PluginContributions>>,

    host_factory: Mutex<Option<HostFactory>>,

    status_hashes: Mutex<HashMap<String, u64>>,

    last_turn_aborted: Mutex<HashMap<String, bool>>,

    agents_md_watches: Mutex<HashMap<String, crate::environment::AgentsMdWatch>>,

    change_notifier: Mutex<HashMap<String, crate::injection::change_notifier::ChangeNotifierState>>,

    pending_skill_listings: Mutex<HashMap<String, crate::primitives::read::ReadOutcome<String>>>,

    session_log_config: crate::session::session_log::SessionLogConfig,

    last_cancel_user: Mutex<HashMap<String, bool>>,

    last_assistant_at: Mutex<HashMap<String, i64>>,

    emitted_session_warnings: Mutex<std::collections::HashSet<String>>,

    activity_registry: Arc<crate::server::activity::ActivityRegistry>,

    session_mcp: Mutex<HashMap<String, Arc<McpManager>>>,

    media: crate::llm::media_resolver::MediaResolver,

    media_dropped: Mutex<HashMap<String, crate::llm::media_budget::DroppedMedia>>,

    media_recovery: Mutex<HashMap<String, crate::llm::media_budget::SharedMediaRecovery>>,

    cache_states: Mutex<HashMap<String, SessionCacheState>>,

    tower_wake_driver: Mutex<Option<Arc<dyn crate::session::wake::TowerWakeDriver>>>,

    tower_wake_runtimes: Mutex<HashMap<String, Arc<crate::features::tower::wake::TowerWakeRuntime>>>,
}

pub type HostFactory = Arc<dyn Fn(&str) -> Arc<dyn crate::callbacks::HostCallbacks> + Send + Sync>;

const CACHE_BREAK_RETENTION: usize = 32;

#[derive(Default)]
struct SessionCacheState {
    tracker: crate::session::cache_observation::CacheBreakTracker,

    breaks: std::collections::VecDeque<crate::session::cache_observation::CacheBreakObservation>,

    last: Option<crate::session::cache_observation::CacheObservation>,

    requests: HashMap<String, crate::session::cache_observation::CacheProbeRequest>,
}

pub type PluginSkillRoots = Arc<dyn Fn() -> Vec<crate::skills::PluginSkillDir> + Send + Sync>;

#[derive(Clone, Default)]
pub struct PluginContributionSet {
    pub sections: Option<String>,
    pub hooks: Vec<crate::permission::HookDef>,
}

pub type PluginContributions = Arc<dyn Fn() -> PluginContributionSet + Send + Sync>;

fn resolve_permission_mode(
    base: crate::permission::PermissionMode,
    recorded: Option<crate::permission::PermissionMode>,
    auto_env: bool,
    metadata: &serde_json::Value,
) -> crate::permission::PermissionMode {
    use crate::permission::PermissionMode;
    if let Some(mode) = recorded {
        return mode;
    }
    if base != PermissionMode::Manual {
        return base;
    }
    if auto_env {
        return PermissionMode::Auto;
    }
    if let Some(mode_str) = metadata
        .get("permission_mode")
        .and_then(|value| value.as_str())
    {
        match mode_str.to_ascii_lowercase().as_str() {
            "auto" => return PermissionMode::Auto,
            "yolo" => return PermissionMode::Yolo,
            _ => {}
        }
    }
    if metadata.get("yolo").and_then(|value| value.as_bool()) == Some(true) {
        return PermissionMode::Yolo;
    }
    base
}

impl ServerEngine {
    pub fn new(spec: PipelineSpec, hub: Arc<EventHub>, store: Arc<SqliteSessionStore>) -> Self {
        Self {
            spec,
            plugin_skill_roots: Mutex::new(None),
            plugin_contributions: Mutex::new(None),
            activity_registry: Arc::new(crate::server::activity::ActivityRegistry::new(
                hub.clone(),
            )),
            hub,
            store: store.clone(),
            max_steps: 0,
            max_attempts: None,
            compaction_max_attempts: None,
            active_turns: Mutex::new(HashMap::new()),
            active_turn_numbers: Mutex::new(HashMap::new()),
            journal_heads: Mutex::new(HashMap::new()),
            mcp_manager: Mutex::new(None),
            mcp_status: Mutex::new(HashMap::new()),
            interaction_manager: Mutex::new(None),
            subagent_manager: Arc::new(SubagentManager::with_store(store.clone())),
            oauth_manager: Mutex::new(None),
            steer_queues: Mutex::new(HashMap::new()),
            steer_slots: Mutex::new(HashMap::new()),
            config_source: Mutex::new(None),
            hooks_source: Mutex::new(None),
            token_counting_strategy_cache: Mutex::new(None),
            host_factory: Mutex::new(None),
            status_hashes: Mutex::new(HashMap::new()),
            last_turn_aborted: Mutex::new(HashMap::new()),
            agents_md_watches: Mutex::new(HashMap::new()),
            change_notifier: Mutex::new(HashMap::new()),
            pending_skill_listings: Mutex::new(HashMap::new()),
            session_log_config: crate::session::session_log::SessionLogConfig::from_env(),
            last_cancel_user: Mutex::new(HashMap::new()),
            last_assistant_at: Mutex::new(HashMap::new()),
            emitted_session_warnings: Mutex::new(std::collections::HashSet::new()),
            media: crate::llm::media_resolver::MediaResolver::new()
                .with_upload_cache(store.clone()),
            media_dropped: Mutex::new(HashMap::new()),
            media_recovery: Mutex::new(HashMap::new()),
            cache_states: Mutex::new(HashMap::new()),
            session_mcp: Mutex::new(HashMap::new()),
            tower_wake_driver: Mutex::new(None),
            tower_wake_runtimes: Mutex::new(HashMap::new()),
        }
    }

    pub fn install_tower_wake_driver(
        &self,
        driver: Arc<dyn crate::session::wake::TowerWakeDriver>,
    ) {
        let mut installed = self
            .tower_wake_driver
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if installed.is_none() {
            *installed = Some(driver);
        }
    }

    fn bind_tower_wake(
        &self,
        toolset: &crate::tools::NativeToolset,
        session_id: &str,
    ) -> Option<()> {
        let driver = self
            .tower_wake_driver
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()?;
        let runtime = self
            .tower_wake_runtimes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(session_id.to_string())
            .or_insert_with(|| crate::features::tower::wake::TowerWakeRuntime::new(driver.clone()))
            .clone();
        let handler = toolset.bind_tower_wake_runtime(runtime);
        driver.set_replay_handler(
            session_id,
            crate::features::tower::wake::TOWER_INBOX_WAKE_VARIANT,
            handler,
        );
        Some(())
    }

    pub fn set_session_mcp_manager(&self, session_id: &str, manager: Arc<McpManager>) {
        self.mcp_status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);
        self.session_mcp
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session_id.to_string(), manager);
    }

    fn ensure_mcp_status_bridge(&self, session_id: &str) {
        let Some(manager) = self.mcp_manager_for(session_id) else {
            return;
        };
        let mut bridges = self
            .mcp_status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if bridges.contains_key(session_id) {
            return;
        }
        let bus = self.hub.bus_for(session_id);
        let emit: crate::mcp::status::McpEventSink = Arc::new(move |event| {
            bus.publish(&crate::events::EngineEvent::Custom(event));
        });
        bridges.insert(
            session_id.to_string(),
            crate::mcp::status::attach(&manager, emit),
        );
    }

    pub fn forget_mcp_status(&self, session_id: &str) {
        self.mcp_status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(session_id);
    }

    pub(crate) fn mcp_manager_for(&self, session_id: &str) -> Option<Arc<McpManager>> {
        self.session_mcp
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(session_id)
            .cloned()
            .or_else(|| self.mcp_manager())
    }

    fn media_dropped_for(&self, session_id: &str) -> crate::llm::media_budget::DroppedMedia {
        self.media_dropped
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(session_id.to_string())
            .or_default()
            .clone()
    }

    fn media_recovery_for(
        &self,
        session_id: &str,
    ) -> crate::llm::media_budget::SharedMediaRecovery {
        self.media_recovery
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(session_id.to_string())
            .or_default()
            .clone()
    }

    pub fn set_host_factory(&self, factory: HostFactory) {
        *self.host_factory.lock().unwrap_or_else(|e| e.into_inner()) = Some(factory);
    }

    fn host_factory(&self) -> Option<HostFactory> {
        self.host_factory
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn set_oauth_manager(&self, manager: Arc<crate::server::oauth::OAuthManager>) {
        *self.oauth_manager.lock().unwrap_or_else(|e| e.into_inner()) = Some(manager);
    }

    pub fn set_config_source(
        &self,
        source: Arc<tokio::sync::Mutex<Option<crate::config::KimiConfig>>>,
    ) {
        *self.config_source.lock().unwrap_or_else(|e| e.into_inner()) = Some(source);
    }

    pub fn set_hooks_source(&self, source: crate::tools::external_hooks::HooksSource) {
        *self.hooks_source.lock().unwrap_or_else(|e| e.into_inner()) = Some(source);
    }

    fn hooks_source(&self) -> Option<crate::tools::external_hooks::HooksSource> {
        self.hooks_source
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn with_plugin_skill_roots(&self, roots: PluginSkillRoots) {
        *self
            .plugin_skill_roots
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(roots);
    }

    fn plugin_skill_roots(&self) -> Vec<crate::skills::PluginSkillDir> {
        self.plugin_skill_roots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|roots| roots())
            .unwrap_or_default()
    }

    pub fn with_plugin_contributions(&self, contributions: PluginContributions) {
        *self
            .plugin_contributions
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(contributions);
    }

    fn plugin_contributions(&self) -> PluginContributionSet {
        let reader = self
            .plugin_contributions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        reader.map(|read| read()).unwrap_or_default()
    }

    fn config_source(&self) -> Option<Arc<tokio::sync::Mutex<Option<crate::config::KimiConfig>>>> {
        self.config_source
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn token_counting_strategy(&self) -> crate::session::token_counting::TokenCountingStrategy {
        let mut cache = self
            .token_counting_strategy_cache
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(source) = self.config_source()
            && let Ok(guard) = source.try_lock()
        {
            let strategy = guard.as_ref().map_or_else(
                || crate::config::KimiConfig::default().resolve_token_counting_strategy(),
                |config| config.resolve_token_counting_strategy(),
            );
            *cache = Some(strategy);
            return strategy;
        }
        if let Some(strategy) = *cache {
            return strategy;
        }
        let strategy = crate::config::KimiConfig::default().resolve_token_counting_strategy();
        *cache = Some(strategy);
        strategy
    }

    async fn micro_compaction_config(
        &self,
    ) -> Option<crate::compaction::micro::MicroCompactionConfig> {
        let source = self.config_source()?;
        let config = {
            let guard = source.lock().await;
            guard.clone()
        }?;
        let enabled = match config.experimental.get("micro_compaction") {
            Some(crate::config::ExperimentalValue::Bool(value)) => *value,
            Some(crate::config::ExperimentalValue::String(value)) => {
                !value.is_empty() && value != "false"
            }
            None => false,
        };
        enabled.then(crate::compaction::micro::MicroCompactionConfig::default)
    }

    fn last_assistant_at_for(&self, session_id: &str) -> Option<i64> {
        self.last_assistant_at
            .lock()
            .ok()
            .and_then(|map| map.get(session_id).copied())
    }

    pub async fn model_context_window(&self, session_id: &str) -> Option<usize> {
        let model = self
            .store
            .get_state("agent_config", session_id)
            .ok()
            .flatten()
            .and_then(|profile| {
                profile
                    .get("model")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| self.spec.model_name.clone());
        let source = self.config_source()?;
        let config = {
            let guard = source.lock().await;
            guard.clone()
        }?;
        let alias = config.models.get(&model)?;
        let size = alias.max_context_size?;
        (size > 0).then_some(size as usize)
    }

    pub async fn session_provider_type(&self, session_id: &str) -> Option<String> {
        let model = self
            .store
            .get_state("agent_config", session_id)
            .ok()
            .flatten()
            .and_then(|profile| {
                profile
                    .get("model")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| self.spec.model_name.clone());
        let source = self.config_source()?;
        let config = {
            let guard = source.lock().await;
            guard.clone()
        }?;
        let native = config.extract_native_llm(Some(&model))?;
        Some(native.provider_type).filter(|value| !value.is_empty())
    }

    pub async fn model_thinking(&self, model: &str) -> Option<ModelThinking> {
        let source = self.config_source()?;
        let config = {
            let guard = source.lock().await;
            guard.clone()
        }?;
        let alias = config.models.get(model)?;
        let capabilities = alias.capabilities.clone().unwrap_or_default();
        let always_thinking = capabilities
            .iter()
            .any(|capability| capability == "always_thinking");
        let support_efforts = alias.support_efforts.clone().unwrap_or_default();
        let thinking = always_thinking
            || capabilities
                .iter()
                .any(|capability| capability == "thinking")
            || !support_efforts.is_empty();
        thinking.then(|| ModelThinking {
            always_thinking,
            support_efforts,
            default_effort: alias.default_effort.clone(),
        })
    }

    async fn publish_tool_pattern_warnings(&self, session_id: &str, profile_name: Option<&str>) {
        let mut known = std::collections::HashSet::new();
        for tool in crate::tools::tool_policy::native_tool_defs_with_notify(
            self.spec.github_token.is_some(),
            self.spec.tower_enabled,
            self.spec.notify_user_enabled,
            None,
            None,
        ) {
            known.insert(tool.name);
        }
        let catalog = crate::prompt::ProfileCatalog::with_builtins();
        for profile in catalog.list() {
            known.extend(crate::tools::tool_policy::literal_tool_names(
                &profile.tools,
            ));
        }
        if self.spec.tool_select {
            known.insert(crate::tools::select_tools::SELECT_TOOLS_TOOL_NAME.to_string());
        }
        let callbacks = self.session_callbacks_for(session_id);
        if let Ok(response) = callbacks.list_tools().await {
            known.extend(response.tools.into_iter().map(|tool| tool.name));
        }
        if let Some(mcp) = self.mcp_manager_for(session_id) {
            known.extend(
                mcp.list_tool_infos()
                    .await
                    .into_iter()
                    .map(|tool| tool.name),
            );
        }

        let profile = match profile_name {
            Some(name) => match catalog.get(name) {
                Some(profile) => Some((profile.name.clone(), profile.tools.clone(), Vec::new())),
                None => self
                    .subagent_manager
                    .get_definition(name)
                    .await
                    .map(|profile| (profile.name, profile.tools, profile.disallowed_tools)),
            },
            None => {
                let profile = catalog.default_profile();
                Some((profile.name.clone(), profile.tools.clone(), Vec::new()))
            }
        };
        let mut checks = Vec::new();
        if let Some((name, tools, disallowed)) = profile {
            checks.push((format!("profile \"{name}\""), "tools", tools));
            checks.push((format!("profile \"{name}\""), "disallowedTools", disallowed));
        }
        if let Some(filter) = self
            .spec
            .policy_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.tools_filter.as_ref())
        {
            checks.push((
                "the global [tools] config".to_string(),
                "enabled",
                filter.enabled.clone(),
            ));
            checks.push((
                "the global [tools] config".to_string(),
                "disabled",
                filter.disabled.clone(),
            ));
        }
        let is_known = |name: &str| known.contains(name);
        for (context, field, patterns) in checks {
            for issue in
                crate::tools::tool_policy::find_inactive_tool_patterns(&patterns, Some(&is_known))
            {
                let message = crate::tools::tool_policy::describe_inactive_tool_pattern(
                    &context, field, &issue,
                );
                self.publish_session_warning(session_id, "tool-pattern-no-match", &message);
            }
        }
    }

    fn publish_session_warning(&self, session_id: &str, code: &str, message: &str) {
        let key = format!("{session_id}\u{0}{code}\u{0}{message}");
        let inserted = self
            .emitted_session_warnings
            .lock()
            .map(|mut emitted| emitted.insert(key))
            .unwrap_or(false);
        if !inserted {
            return;
        }
        self.hub
            .bus_for(session_id)
            .publish(&crate::events::EngineEvent::Custom(serde_json::json!({
                "type": "warning",
                "code": code,
                "message": message,
            })));
    }

    fn stamp_last_assistant_at(&self, session_id: &str) {
        if let Ok(mut map) = self.last_assistant_at.lock() {
            map.insert(
                session_id.to_string(),
                chrono::Utc::now().timestamp_millis(),
            );
        }
    }

    async fn memory_filing_enabled(&self) -> bool {
        memory_filing::resolve_gate(
            memory_filing::env_switch(),
            self.memory_filing_config_flag().await,
        )
    }

    async fn memory_filing_config_flag(&self) -> Option<bool> {
        let source = self.config_source()?;
        let config = {
            let guard = source.lock().await;
            guard.clone()
        }?;
        memory_filing::config_flag(&config)
    }

    async fn apply_thinking_guard_setting(&self) {
        crate::llm::thinking_guard::set_enabled(
            self.thinking_guard_config_flag().await.unwrap_or(true),
        );
    }

    async fn thinking_guard_config_flag(&self) -> Option<bool> {
        let source = self.config_source()?;
        let config = {
            let guard = source.lock().await;
            guard.clone()
        }?;
        match config
            .experimental
            .get(crate::llm::thinking_guard::GUARD_CONFIG_KEY)
        {
            Some(crate::config::ExperimentalValue::Bool(value)) => Some(*value),
            Some(crate::config::ExperimentalValue::String(value)) => {
                Some(!value.is_empty() && value != "false")
            }
            None => None,
        }
    }

    async fn spawn_memory_filing(
        &self,
        system_prompt: &str,
        transcript: &[LLMMessage],
        prompt: &str,
        reply: &str,
        secondary_llm: Option<Arc<dyn LLM>>,
    ) {
        let memory_in_prompt =
            system_prompt.contains(crate::prompt::builder::MEMORY_SECTION_MARKER);
        let gate = self.memory_filing_enabled().await;
        let wrote = memory_filing::turn_wrote_memory(transcript);
        let Some(llm) =
            memory_filing::filing_model(gate, memory_in_prompt, secondary_llm.as_ref(), wrote)
        else {
            return;
        };

        let (Some(base), Some(workspace_root)) = (
            memory_store::memory_base(),
            self.spec.workspace_root.as_deref(),
        ) else {
            return;
        };
        memory_filing::FilingPass {
            llm: llm.clone(),
            base,
            project_id: memory_store::project_id_for(std::path::Path::new(workspace_root)),
            exchange: memory_filing::Exchange {
                user_message: prompt.to_string(),
                assistant_reply: reply.to_string(),
                tool_activity: memory_filing::tool_activity(transcript),
            },
        }
        .spawn();
    }

    async fn resolved_native_llm(&self, model: &str) -> Option<crate::rpc::types::NativeLlmConfig> {
        let source = self.config_source()?;
        let config = {
            let guard = source.lock().await;
            guard.clone()
        }
        .unwrap_or_else(|| {
            crate::config::KimiConfig::discover()
                .map(|(config, _)| config)
                .unwrap_or_default()
        });
        let native = config.extract_native_llm(Some(model))?;
        let mut llm = crate::rpc::types::NativeLlmConfig {
            protocol: native.protocol,
            alias: native.alias,
            provider_type: Some(native.provider_type.clone()).filter(|value| !value.is_empty()),
            base_url: native.base_url,
            api_key: native.api_key,

            api_key_env: native.api_key_env,
            model: native.model,
            max_tokens: native.max_output_size.or(native.max_tokens),
            custom_headers: native.custom_headers,
            reasoning_effort: config.resolve_effort(native.off_effort.as_deref()),
            thinking_budget: None,
            auth_provider: native.auth_provider.clone(),
            thinking_keep: config.resolve_thinking_keep(),
            beta_api: native.beta_api,
            capabilities: native.capabilities.clone(),
            system_prompt: native.system_prompt.clone(),
            max_input_size: native.max_input_size,
            adaptive_thinking: native.adaptive_thinking,
            reasoning_key: native.reasoning_key.clone(),

            off_effort: None,
            response_format: None,
            sampling: None,
            determinism: None,
            extra_params: None,
            binary_path: native.binary_path.clone(),
            antigravity: native.antigravity.clone(),
        };
        config.apply_model_request_options(&mut llm, Some(model));
        Some(llm)
    }

    fn oauth_manager(&self) -> Option<Arc<crate::server::oauth::OAuthManager>> {
        self.oauth_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn is_turn_active(&self, session_id: &str) -> bool {
        self.active_turns
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(session_id)
    }

    pub fn active_turn_number(&self, session_id: &str) -> Option<u32> {
        self.active_turn_numbers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(session_id)
            .copied()
    }

    pub fn is_turn_ending(&self, session_id: &str) -> bool {
        self.active_turns
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(session_id)
            .is_some_and(|flag| flag.load(Ordering::SeqCst))
    }

    pub fn activity_phase(&self, session_id: &str) -> crate::server::activity::AgentPhase {
        self.activity_registry.tracker(session_id).snapshot()
    }

    pub fn activity_tracker(
        &self,
        session_id: &str,
    ) -> Arc<crate::server::activity::ActivityTracker> {
        self.activity_registry.tracker(session_id)
    }

    pub fn enqueue_steer(&self, session_id: &str, mut message: LLMMessage) -> bool {
        if !self.is_turn_active(session_id) {
            return false;
        }

        if message.prompt_id.is_none() {
            message.prompt_id = Some(format!("prompt-{}", fastrand::u64(..)));
        }
        self.steer_queue(session_id)
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(message);

        if let Some(signal) = self
            .steer_slot(session_id)
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            signal.trigger();
        }
        true
    }

    pub fn queued_steer_count(&self, session_id: &str) -> usize {
        self.steer_queues
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(session_id)
            .map(|queue| queue.lock().unwrap_or_else(|e| e.into_inner()).len())
            .unwrap_or(0)
    }

    fn steer_queue(&self, session_id: &str) -> Arc<Mutex<Vec<LLMMessage>>> {
        let mut queues = self.steer_queues.lock().unwrap_or_else(|e| e.into_inner());
        queues
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(Vec::new())))
            .clone()
    }

    fn release_turn_steer_state(&self, session_id: &str) {
        let mut queues = self.steer_queues.lock().unwrap_or_else(|e| e.into_inner());
        let keep = queues
            .get(session_id)
            .is_some_and(|queue| !queue.lock().unwrap_or_else(|e| e.into_inner()).is_empty());
        if !keep {
            queues.remove(session_id);
        }
        drop(queues);
        let mut slots = self.steer_slots.lock().unwrap_or_else(|e| e.into_inner());
        slots.remove(session_id);
    }

    fn steer_slot(
        &self,
        session_id: &str,
    ) -> Arc<Mutex<Option<crate::subagent::types::ParentCancel>>> {
        let mut slots = self.steer_slots.lock().unwrap_or_else(|e| e.into_inner());
        slots
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(None)))
            .clone()
    }

    pub fn with_mcp_manager(self, mcp_manager: Arc<McpManager>) -> Self {
        *self.mcp_manager.lock().unwrap_or_else(|e| e.into_inner()) = Some(mcp_manager);
        self
    }

    pub fn set_mcp_manager(&self, mcp_manager: Arc<McpManager>) {
        *self.mcp_manager.lock().unwrap_or_else(|e| e.into_inner()) = Some(mcp_manager);
    }

    pub fn mcp_manager(&self) -> Option<Arc<McpManager>> {
        self.mcp_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn hub(&self) -> Arc<EventHub> {
        self.hub.clone()
    }

    pub fn with_interaction_manager(self, manager: Arc<InteractionManager>) -> Self {
        self.set_interaction_manager(manager);
        self
    }

    pub fn set_interaction_manager(&self, manager: Arc<InteractionManager>) {
        let registry = self.activity_registry.clone();
        manager.set_activity_notifier(Arc::new(move |session_id, signal| {
            let tracker = registry.tracker(session_id);
            match signal {
                crate::server::interaction::ActivitySignal::Pending {
                    approval_id,
                    tool_call_id,
                } => tracker.awaiting(&approval_id, &tool_call_id),
                crate::server::interaction::ActivitySignal::Resolved => {
                    tracker.interaction_resolved()
                }
            }
        }));
        *self
            .interaction_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(manager);
    }

    pub fn interaction_manager(&self) -> Option<Arc<InteractionManager>> {
        self.interaction_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn with_max_steps(mut self, max_steps: u32) -> Self {
        self.max_steps = max_steps;
        self
    }

    pub fn with_max_attempts(mut self, max_attempts: Option<u32>) -> Self {
        self.max_attempts = max_attempts;
        self
    }

    pub fn with_compaction_max_attempts(mut self, compaction_max_attempts: Option<u32>) -> Self {
        self.compaction_max_attempts = compaction_max_attempts;
        self
    }

    pub fn compaction_max_attempts(&self) -> Option<u32> {
        self.compaction_max_attempts
    }

    pub fn with_subagent_timeout_ms(mut self, timeout_ms: Option<u64>) -> Self {
        self.spec.subagent_timeout_ms = timeout_ms;
        self
    }

    pub fn with_swarm_timeout_ms(self, timeout_ms: Option<u64>) -> Self {
        self.subagent_manager.set_swarm_timeout_ms(timeout_ms);
        self
    }

    pub fn store(&self) -> &Arc<SqliteSessionStore> {
        &self.store
    }

    pub fn subagent_manager(&self) -> Arc<SubagentManager> {
        self.subagent_manager.clone()
    }

    pub fn model_name(&self) -> &str {
        &self.spec.model_name
    }

    pub fn is_busy(&self, session_id: &str) -> bool {
        if self
            .active_turns
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(session_id)
        {
            return true;
        }
        if self.has_background_work(session_id) {
            return true;
        }
        false
    }

    fn has_background_work(&self, session_id: &str) -> bool {
        if self.subagent_manager.session_has_active_turn(session_id) {
            return true;
        }
        self.subagent_manager
            .get_task_runner_sync()
            .is_some_and(|runner| runner.has_running_task_for_session(session_id))
    }

    pub(crate) fn publish_background_work_changed(&self, session_id: &str) {
        let turn_running = self
            .active_turns
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(session_id);
        if turn_running {
            return;
        }
        let busy = self.is_busy(session_id);
        let reason = self
            .store
            .get_session(session_id)
            .ok()
            .flatten()
            .and_then(|summary| summary.last_turn_reason);
        self.publish_work_changed(session_id, busy, reason.as_deref());
    }

    pub fn active_turns_snapshot(&self) -> Vec<(String, u32)> {
        let sessions: Vec<String> = self
            .active_turns
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .keys()
            .cloned()
            .collect();
        let numbers = self
            .active_turn_numbers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        sessions
            .into_iter()
            .map(|session| {
                let turn = numbers.get(&session).copied().unwrap_or(0);
                (session, turn)
            })
            .collect()
    }

    pub async fn drain_active_turns(&self) {
        while !self.active_turns_snapshot().is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    pub fn observe_journal_head(&self, session_id: &str) -> u64 {
        let head = self.store.latest_wire_event_seq(session_id).unwrap_or(0);
        self.journal_heads
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(session_id.to_string(), head);
        head
    }

    pub fn forget_journal_head(&self, session_id: &str) {
        self.journal_heads
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(session_id);

        self.hub.forget_persisted_head(session_id);
    }

    pub fn forget_change_notifier(&self, session_id: &str) {
        self.change_notifier
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(session_id);
    }

    fn session_log(&self, session_id: &str) -> Option<crate::session::session_log::SessionLog> {
        let root = self.spec.workspace_root.as_deref()?;
        let dir = crate::storage::paths::engine_session_dir(std::path::Path::new(root), session_id)
            .ok()?;
        Some(crate::session::session_log::SessionLog::open(
            &dir,
            self.session_log_config,
        ))
    }

    pub fn session_log_path(&self, session_id: &str) -> Option<std::path::PathBuf> {
        let root = self.spec.workspace_root.as_deref()?;
        let dir = crate::storage::paths::engine_session_dir(std::path::Path::new(root), session_id)
            .ok()?;
        Some(crate::session::session_log::resolve_session_log_path(&dir))
    }

    fn journal_moved(&self, session_id: &str) -> bool {
        let head = self.store.latest_wire_event_seq(session_id).unwrap_or(0);
        let own_tail = self.hub.persisted_head(session_id).unwrap_or(0);
        self.journal_heads
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(session_id)
            .is_some_and(|seen| head != (*seen).max(own_tail))
    }

    pub fn cancel_turn(&self, session_id: &str) -> bool {
        if let Some(mgr) = self.interaction_manager() {
            mgr.cancel_session(session_id);
        }
        let turns = self
            .active_turns
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(flag) = turns.get(session_id) {
            flag.store(true, Ordering::SeqCst);

            drop(turns);
            self.last_cancel_user
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(session_id.to_string(), true);
            true
        } else {
            false
        }
    }

    pub(crate) fn cancel_turn_at(&self, session_id: &str, turn_number: u32) -> bool {
        let active_numbers = self
            .active_turn_numbers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if active_numbers.get(session_id) != Some(&turn_number) {
            return false;
        }

        self.cancel_turn(session_id)
    }

    fn publish_work_changed(&self, session_id: &str, busy: bool, last_turn_reason: Option<&str>) {
        let pending_interaction = self
            .interaction_manager()
            .and_then(|mgr| mgr.pending_interaction_kind(session_id))
            .unwrap_or("none")
            .to_string();
        self.hub
            .bus_for(session_id)
            .publish(&crate::events::EngineEvent::SessionWorkChanged {
                busy,
                main_turn_active: busy,
                pending_interaction,
                last_turn_reason: last_turn_reason.map(str::to_string),
            });
    }

    fn publish_turn_ended(&self, session_id: &str, turn_number: u32, reason: &str) {
        let user_cancelled = self
            .last_cancel_user
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id)
            .unwrap_or(false);
        let aborted = crate::injection::interruption_reminder::arms_reminder(
            reason == "cancelled",
            user_cancelled,
        );
        self.last_turn_aborted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session_id.to_string(), aborted);
        self.hub
            .bus_for(session_id)
            .publish(&crate::events::EngineEvent::TurnEnded {
                agent_id: "main".to_string(),
                turn_id: format!("turn-{turn_number}"),
                reason: reason.to_string(),
            });
    }

    fn take_last_turn_aborted(&self, session_id: &str) -> bool {
        self.last_turn_aborted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id)
            .unwrap_or(false)
    }

    pub async fn publish_status_updated(&self, session_id: &str) {
        let payload = self.status_payload(session_id);
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(&payload.to_string(), &mut hasher);
        let hash = std::hash::Hasher::finish(&hasher);
        {
            let mut hashes = self
                .status_hashes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if hashes.get(session_id) == Some(&hash) {
                return;
            }
            hashes.insert(session_id.to_string(), hash);
        }
        self.hub
            .bus_for(session_id)
            .publish(&crate::events::EngineEvent::Custom(payload));
    }

    fn status_payload(&self, session_id: &str) -> serde_json::Value {
        let agent_config = self
            .store
            .get_state("agent_config", session_id)
            .ok()
            .flatten()
            .unwrap_or_else(|| serde_json::json!({}));
        let metadata = self
            .store
            .get_state("metadata", session_id)
            .ok()
            .flatten()
            .unwrap_or_else(|| serde_json::json!({}));

        let context_tokens = self.session_context_tokens(session_id);

        let model = agent_config
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or(self.spec.model_name.as_str());
        let mut payload = serde_json::json!({
            "type": "agent.status.updated",
            "model": model,
            "contextTokens": context_tokens,
        });
        let object = payload.as_object_mut().unwrap_or_else(|| {
            panic!("status payload is always an object");
        });
        if let Some(thinking) = agent_config.get("thinking").and_then(|v| v.as_str()) {
            object.insert("thinkingEffort".into(), serde_json::json!(thinking));
        }

        let permission = agent_config
            .get("permission_mode")
            .and_then(|v| v.as_str())
            .or_else(|| metadata.get("permission_mode").and_then(|v| v.as_str()));
        if let Some(permission) = permission {
            object.insert("permission".into(), serde_json::json!(permission));
        }
        if let Some(plan_mode) = agent_config.get("plan_mode").and_then(|v| v.as_bool()) {
            object.insert("planMode".into(), serde_json::json!(plan_mode));
        }

        if crate::swarm::mode::swarm_mode_registry().is_active(crate::callbacks::MAIN_AGENT_ID) {
            object.insert("swarmMode".into(), serde_json::json!(true));
        }
        if let Some(run) = crate::features::helloWorld::helloWorldOps::hello_world_mode_registry()
            .run(crate::callbacks::MAIN_AGENT_ID)
        {
            object.insert("helloWorldMode".into(), serde_json::json!(true));
            object.insert("helloWorldStage".into(), serde_json::json!(run.stage));
        }

        if let Some(usage) = crate::session::usage::status(&self.store, session_id, model).to_json()
        {
            object.insert("usage".into(), usage);
        }
        payload
    }

    async fn session_spec(&self, session_id: &str) -> PipelineSpec {
        let mut policy_snapshot = self.spec.policy_snapshot.clone().unwrap_or_default();

        if let Some(hooks) = self.hooks_source().and_then(|source| source.snapshot()) {
            policy_snapshot.pre_tool_hooks = hooks;
        }
        let session_metadata = self
            .store
            .get_state("metadata", session_id)
            .ok()
            .flatten()
            .unwrap_or_default();
        let recorded_mode = crate::agent::permission_mode::permission_mode_ops::recorded_mode(
            self.store.as_ref(),
            session_id,
            crate::callbacks::MAIN_AGENT_ID,
        );
        policy_snapshot.mode = resolve_permission_mode(
            policy_snapshot.mode,
            recorded_mode,
            std::env::var("KIMI_AUTO_APPROVE")
                .map(|v| v == "1" || v == "true")
                .unwrap_or(false),
            &session_metadata,
        );

        let mut extra_roots = self.spec.extra_roots.clone();
        if let Some(dirs) = session_metadata
            .get("additional_dirs")
            .and_then(|v| v.as_array())
        {
            extra_roots.extend(
                dirs.iter()
                    .filter_map(|dir| dir.as_str().map(str::to_string)),
            );
        }
        extra_roots.extend(self.project_local_roots(session_id));

        if let Some(root) = self.session_root_path(session_id) {
            let additional: Vec<std::path::PathBuf> = extra_roots
                .iter()
                .map(std::path::PathBuf::from)
                .filter(|path| path.is_dir())
                .collect();
            if let Ok(roots) = crate::environment::WorkspaceRoots::new(root, additional) {
                self.subagent_manager
                    .refresh_session_workspace_roots(session_id, roots);
            }
        }

        let plugin_skill_dirs = self.plugin_skill_roots();

        let skill_scan = crate::skills::SkillScanRoots {
            root: self
                .spec
                .workspace_root
                .as_deref()
                .map(std::path::PathBuf::from),
            extra_dirs: self.spec.skill_dirs.clone(),
            plugin_dirs: plugin_skill_dirs.clone(),
            merge_all_available_skills: self.spec.merge_all_available_skills,
        };
        let skill_roots = skill_scan.scanned_roots();
        let skill_listing = skill_scan
            .catalog_outcome()
            .map(|skills| {
                crate::prompt::skills_renderer::render_skills_markdown_with_roots(
                    &skills,
                    &skill_roots,
                )
            });
        self.pending_skill_listings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(session_id.to_string(), skill_listing.clone());
        let session_profile = match self.store.get_state("agent_config", session_id) {
            Ok(Some(profile)) => profile,
            Ok(None) => serde_json::Value::Null,
            Err(error) => {
                tracing::warn!(
                    %session_id,
                    %error,
                    "could not read the session's agent_config; the turn runs with the \
                     engine-wide profile instead of guessing the overrides"
                );
                serde_json::Value::Null
            }
        };
        let bound_profile = session_profile
            .get("profile")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string);
        let mut session_system_prompt = self.spec.system_prompt.clone();
        if (session_system_prompt.is_empty()
            || session_system_prompt == "sys"
            || session_system_prompt
                .starts_with("You are kimi-agent, running as a standalone service."))
            && let Some(ref ws) = self.spec.workspace_root
        {
            let builder = crate::prompt::SystemPromptBuilder::new(ws)
                .with_skill_dirs(self.spec.skill_dirs.clone())
                .with_plugin_skill_dirs(plugin_skill_dirs.clone())
                .with_merge_all_available_skills(self.spec.merge_all_available_skills)
                .with_additional_dirs(extra_roots.iter().map(std::path::PathBuf::from).collect())
                .with_memory(true);
            let builder = match &bound_profile {
                Some(name) => builder.with_profile(name.clone()),
                None => builder,
            };

            let builder = match &skill_listing {
                crate::primitives::read::ReadOutcome::Found(listing) => {
                    builder.with_custom_skills(listing.clone())
                }
                _ => builder,
            };
            let (prompt, warnings) = match builder.build_with_warnings() {
                Ok(rendered) => rendered,
                Err(error) => {
                    tracing::warn!(
                        %session_id,
                        %error,
                        "the system prompt cannot name a shell; session creation will fail on the same probe"
                    );
                    (session_system_prompt.clone(), Vec::new())
                }
            };
            session_system_prompt = prompt;

            for warning in warnings {
                self.publish_session_warning(session_id, "agents-md-oversized", &warning);
            }
        }

        let mut spec = PipelineSpec {
            rust_self_contained: true,
            policy_snapshot: Some(policy_snapshot),
            session_id: Some(session_id.to_string()),
            system_prompt: session_system_prompt,
            extra_roots,
            plugin_skill_dirs,
            ..clone_spec(&self.spec)
        };

        if let Some(model) = session_profile
            .get("model")
            .and_then(|v| v.as_str())
            .filter(|model| !model.is_empty())
            && let Some(native) = self.resolved_native_llm(model).await
        {
            spec.model_name = model.to_string();
            spec.native_llm = Some(native);
        }
        apply_session_overrides(&mut spec, &session_profile);
        if let Some(name) = bound_profile.as_deref()
            && !name.eq_ignore_ascii_case(crate::prompt::profiles::DEFAULT_AGENT_PROFILE_NAME)
            && let Some(profile) = crate::prompt::ProfileCatalog::with_builtins().get(name)
        {
            let snapshot = spec
                .policy_snapshot
                .get_or_insert_with(crate::permission::PolicySnapshot::default);
            let filter = snapshot
                .tools_filter
                .get_or_insert_with(crate::tools::tool_policy::ToolsFilter::default);
            crate::tools::tool_policy::restrict_to_profile(&mut filter.enabled, &profile.tools);
        }
        self.publish_tool_pattern_warnings(session_id, bound_profile.as_deref())
            .await;
        spec
    }

    fn session_workspace(
        &self,
        session_id: &str,
    ) -> Option<crate::session::sqlite_store::WorkspaceSummary> {
        let workspace_id = self
            .store
            .get_session(session_id)
            .ok()
            .flatten()
            .and_then(|session| session.workspace_id)?;
        self.store.get_workspace(&workspace_id).ok().flatten()
    }

    fn session_root_path(&self, session_id: &str) -> Option<std::path::PathBuf> {
        if let Some(workspace) = self.session_workspace(session_id) {
            return Some(std::path::PathBuf::from(workspace.root));
        }
        self.store
            .get_session(session_id)
            .ok()
            .flatten()
            .and_then(|session| session.cwd)
            .map(std::path::PathBuf::from)
    }

    async fn environment_declaration(
        &self,
    ) -> Result<crate::environment::EnvironmentDeclaration, crate::environment::EnvironmentError>
    {
        let config = match self.config_source() {
            Some(source) => source.lock().await.clone(),
            None => None,
        };
        match config {
            Some(config) => {
                crate::environment::EnvironmentDeclaration::from_config(&config.environments)
            }
            None => Ok(crate::environment::EnvironmentDeclaration::default()),
        }
    }

    fn observe_agents_md(&self, session_id: &str, work_dir: &std::path::Path) -> Option<String> {
        let home = crate::default_workspace::user_home_dir();
        let mut watches = self
            .agents_md_watches
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let watch = watches.entry(session_id.to_string()).or_insert_with(|| {
            crate::environment::AgentsMdWatch::start(work_dir, None, home.as_deref())
        });
        let changes = watch.check(work_dir, None, home.as_deref());
        for change in &changes {
            tracing::warn!(
                session_id,
                code = crate::environment::codes::AGENTS_MD_CHANGED,
                change = %change,
                "AGENTS.md changed after the prompt was built"
            );
        }
        for warning in watch.status().warnings() {
            tracing::warn!(
                session_id,
                code = crate::environment::codes::AGENTS_MD_UNREADABLE,
                warning = %warning,
                "AGENTS.md could not be read"
            );
        }

        agents_md_change_reminder(&changes)
    }

    async fn observe_catalog_changes(
        &self,
        session_id: &str,
        spec: &PipelineSpec,
        prompt: &str,
    ) -> Vec<(String, &'static str)> {
        use crate::injection::change_notifier::{
            ChangeNotifierState, ModelPoolSnapshot, SKILLS_CHANGE_VARIANT,
            SUBAGENTS_CHANGE_VARIANT, SubagentProfile, WORKSPACE_DIRS_CHANGE_VARIANT,
            WorkspaceSnapshot,
        };

        let profiles: Vec<SubagentProfile> = self
            .subagent_manager
            .profile_listing()
            .await
            .into_iter()
            .map(|profile| SubagentProfile {
                name: profile.name,
                description: profile.description,
                when_to_use: profile.when_to_use,
            })
            .collect();
        let pool = spec.secondary_model.as_ref().map(|pool| ModelPoolSnapshot {
            default_model: Some(pool.default_model.clone()),
            models: pool
                .models
                .iter()
                .map(|entry| (entry.alias.clone(), entry.hint.clone()))
                .collect(),
        });
        let workspace = self
            .subagent_manager
            .session_workspace_roots_sync(session_id);
        let workspace_snapshot = workspace.as_ref().map(|roots| WorkspaceSnapshot {
            additional: roots
                .additional_dirs()
                .iter()
                .map(|path| path.display().to_string())
                .collect(),
        });
        let cwd = workspace
            .as_ref()
            .map(|roots| roots.work_dir().display().to_string())
            .or_else(|| {
                self.session_root_path(session_id)
                    .map(|path| path.display().to_string())
            })
            .unwrap_or_default();

        let listing = self
            .pending_skill_listings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(session_id)
            .unwrap_or_else(|| {
                let scan = crate::skills::SkillScanRoots {
                    root: spec.workspace_root.as_deref().map(std::path::PathBuf::from),
                    extra_dirs: spec.skill_dirs.clone(),
                    plugin_dirs: spec.plugin_skill_dirs.clone(),
                    merge_all_available_skills: spec.merge_all_available_skills,
                };
                let roots = scan.scanned_roots();
                scan.catalog_outcome()
                    .map(|skills| {
                        crate::prompt::skills_renderer::render_skills_markdown_with_roots(
                            &skills,
                            &roots,
                        )
                    })
            });

        let mut states = self
            .change_notifier
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let state = states
            .entry(session_id.to_string())
            .or_insert_with(ChangeNotifierState::default);

        let tools_filter = spec
            .policy_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.tools_filter.as_ref());
        let is_tool_active = |name: &str| {
            spec.native_tools && tools_filter.map_or(true, |filter| filter.allows(name))
        };
        let mut reminders = Vec::new();

        if let Some(text) = crate::injection::plugin::take_plugin_change_notices(session_id) {
            reminders.push((text, crate::injection::plugin::PLUGIN_CHANGE_VARIANT));
        }
        if let Some(text) = state.skills(listing, prompt, &is_tool_active) {
            reminders.push((text, SKILLS_CHANGE_VARIANT));
        }
        if let Some(text) = state.subagents(&profiles, pool, &is_tool_active) {
            reminders.push((text, SUBAGENTS_CHANGE_VARIANT));
        }
        if let Some(text) = state.workspace_dirs(workspace_snapshot, &cwd) {
            reminders.push((text, WORKSPACE_DIRS_CHANGE_VARIANT));
        }
        reminders
    }

    pub async fn freeze_session_environment(
        &self,
        session_id: &str,
    ) -> Result<Option<crate::environment::EnvironmentSnapshot>, crate::environment::EnvironmentError>
    {
        use crate::environment::{EnvironmentSnapshot, WorkspaceRoots, codes};

        let declaration = self.environment_declaration().await?;
        let Some(root) = self.session_root_path(session_id) else {
            self.store
                .set_session_environment_snapshot(
                    session_id,
                    crate::environment::declaration::LOCAL_ENVIRONMENT_ID,
                    None,
                    &std::collections::BTreeMap::new(),
                )
                .map_err(|error| {
                    crate::environment::EnvironmentError::new(
                        codes::ENVIRONMENT_STORE_FAILED,
                        error.to_string(),
                    )
                })?;
            self.subagent_manager
                .clear_session_environment(session_id)
                .await;
            return Ok(None);
        };
        let roots = WorkspaceRoots::new(root, Vec::new())?;
        let snapshot = EnvironmentSnapshot::capture(
            &declaration,
            None,
            Some(roots.work_dir()),
            std::collections::BTreeMap::new(),
            &roots,
            true,
        )?;

        let cwd = snapshot.cwd().display().to_string();
        self.store
            .set_session_environment_snapshot(
                session_id,
                snapshot.environment_id(),
                Some(&cwd),
                snapshot.env(),
            )
            .map_err(|error| {
                crate::environment::EnvironmentError::new(
                    codes::ENVIRONMENT_STORE_FAILED,
                    error.to_string(),
                )
            })?;
        self.subagent_manager
            .set_session_environment(session_id, snapshot.clone(), roots)
            .await;
        Ok(Some(snapshot))
    }

    pub async fn restore_session_environment(
        &self,
        session_id: &str,
    ) -> Result<Option<crate::environment::RestoreOutcome>, crate::environment::EnvironmentError>
    {
        use crate::environment::{EnvironmentSnapshot, WorkspaceRoots, codes};

        let Some(stored) = self
            .store
            .get_session_environment_snapshot(session_id)
            .map_err(|error| {
                crate::environment::EnvironmentError::new(
                    codes::ENVIRONMENT_STORE_FAILED,
                    error.to_string(),
                )
            })?
        else {
            return Ok(None);
        };
        let declaration = self.environment_declaration().await?;
        let Some(root) = stored
            .binding
            .cwd
            .clone()
            .or_else(|| self.session_root_path(session_id))
        else {
            self.subagent_manager
                .clear_session_environment(session_id)
                .await;
            return Ok(None);
        };
        let roots = WorkspaceRoots::new(root, Vec::new())?;
        let outcome = EnvironmentSnapshot::restore(
            &declaration,
            &stored.binding,
            Some(stored.env),
            &roots,
            true,
        )?;
        self.subagent_manager
            .set_session_environment(session_id, outcome.snapshot.clone(), roots)
            .await;
        Ok(Some(outcome))
    }

    fn session_state_store(&self, session_id: &str) -> Option<Arc<crate::storage::StateStore>> {
        let workspace = self.session_workspace(session_id);
        let ws_ref = workspace
            .as_ref()
            .map(|workspace| std::path::PathBuf::from(&workspace.root))
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        crate::storage::StateStore::for_session(&ws_ref, session_id)
            .ok()
            .map(Arc::new)
    }

    pub fn session_goal_snapshot(&self, session_id: &str) -> Option<Value> {
        let store = self.session_state_store(session_id)?;
        store.goal_snapshot()
    }

    pub fn edit_session_goal_objective(
        &self,
        session_id: &str,
        objective: &str,
    ) -> Option<Result<Value, String>> {
        let store = self.session_state_store(session_id)?;
        let outcome = store
            .apply_write_and_persist(
                "goal",
                &serde_json::json!({ "action": "objective", "objective": objective }),
            )
            .map_err(|error| error.to_string());
        match outcome {
            Ok(outcome) => Some(Ok(outcome.response["goal"].clone())),
            Err(error) => Some(Err(error)),
        }
    }

    pub async fn pending_goal_continuation(
        &self,
        session_id: &str,
        step_capped: bool,
    ) -> Option<String> {
        let store = self.session_state_store(session_id)?;
        let goal = store.goal_context()?;
        if !goal.status.is_active() {
            return None;
        }
        let tokens_spent = goal
            .token_budget
            .is_some_and(|budget| goal.tokens_used >= budget);
        let turns_spent = goal
            .turn_budget
            .is_some_and(|budget| goal.turns_used >= budget);
        let time_spent = goal
            .wall_clock_budget_ms
            .is_some_and(|budget| goal.wall_clock_ms >= budget);
        if tokens_spent || turns_spent || time_spent {
            return None;
        }
        Some(crate::goal::continuation_prompt(step_capped))
    }

    fn project_local_roots(&self, session_id: &str) -> Vec<String> {
        let Some(workspace) = self.session_workspace(session_id) else {
            return Vec::new();
        };
        match crate::project_local_config::read_additional_dirs(std::path::Path::new(
            &workspace.root,
        )) {
            Ok(dirs) => dirs
                .into_iter()
                .map(|dir| dir.to_string_lossy().into_owned())
                .collect(),
            Err(error) => {
                tracing::warn!("ignoring the project-local config of a workspace: {error}");
                Vec::new()
            }
        }
    }

    fn session_callbacks(
        &self,
        session_id: &str,
        ws_ref: &std::path::Path,
    ) -> Arc<dyn HostCallbacks> {
        let host_callbacks: Arc<dyn HostCallbacks> = match self.host_factory() {
            Some(factory) => factory(session_id),
            None => {
                if let Some(mgr) = self.interaction_manager() {
                    Arc::new(
                        ServerHost::with_interaction(mgr, session_id.to_string())
                            .with_oauth(self.oauth_manager()),
                    )
                } else {
                    Arc::new(ServerHost::standalone().with_oauth(self.oauth_manager()))
                }
            }
        };
        match crate::storage::StateStore::for_workspace(ws_ref) {
            Ok(workspace_store) => {
                let workspace_store = Arc::new(workspace_store);
                let session_store = crate::storage::StateStore::for_session(ws_ref, session_id)
                    .ok()
                    .map(Arc::new)
                    .unwrap_or_else(|| workspace_store.clone());
                Arc::new(crate::callbacks::StateStoreCallbacks {
                    inner: host_callbacks,
                    store: session_store,
                    cron_store: workspace_store,
                })
            }
            Err(_) => host_callbacks,
        }
    }

    pub async fn session_skills(&self, session_id: &str) -> Vec<crate::skills::SkillDescriptor> {
        let spec = self.session_spec(session_id).await;
        crate::skills::SkillScanRoots {
            root: spec.workspace_root.as_deref().map(std::path::PathBuf::from),
            extra_dirs: spec.skill_dirs.clone(),
            plugin_dirs: spec.plugin_skill_dirs.clone(),
            merge_all_available_skills: spec.merge_all_available_skills,
        }
        .catalog()
    }

    pub async fn render_session_skill_prompt(
        &self,
        session_id: &str,
        name: &str,
        args: &str,
    ) -> Result<crate::tools::skill::UserSlashSkillPrompt, crate::tools::skill::SkillPromptError>
    {
        let spec = self.session_spec(session_id).await;
        let scan = crate::tools::skill::SkillScan {
            root: spec.workspace_root.as_deref().map(std::path::Path::new),
            extra_dirs: &spec.skill_dirs,
            plugin_dirs: &spec.plugin_skill_dirs,
            merge_all_available_skills: spec.merge_all_available_skills,
        };
        crate::tools::skill::render_user_slash_skill_prompt(&scan, name, args, session_id)
    }

    fn token_counting(
        &self,
        session_id: &str,
    ) -> crate::session::token_counting::TokenCountingState {
        self.store
            .get_state("metadata", session_id)
            .ok()
            .flatten()
            .and_then(|metadata| metadata.get("token_counting").cloned())
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default()
    }

    fn save_token_counting(
        &self,
        session_id: &str,
        state: &crate::session::token_counting::TokenCountingState,
    ) {
        let mut metadata = self
            .store
            .get_state("metadata", session_id)
            .ok()
            .flatten()
            .filter(|value| value.is_object())
            .unwrap_or_else(|| serde_json::json!({}));
        if let Some(object) = metadata.as_object_mut() {
            object.insert(
                "token_counting".into(),
                serde_json::to_value(state).unwrap_or_default(),
            );
        }
        if let Err(error) = self.store.put_session_metadata(session_id, &metadata) {
            tracing::warn!(%session_id, %error, "could not persist the token-counting state");
        }
    }

    fn micro_compaction_state(
        &self,
        session_id: &str,
    ) -> crate::compaction::micro::MicroCompactionState {
        self.store
            .get_state("metadata", session_id)
            .ok()
            .flatten()
            .and_then(|metadata| metadata.get("micro_compaction").cloned())
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default()
    }

    fn save_micro_compaction_state(
        &self,
        session_id: &str,
        state: &crate::compaction::micro::MicroCompactionState,
    ) {
        let mut metadata = self
            .store
            .get_state("metadata", session_id)
            .ok()
            .flatten()
            .filter(|value| value.is_object())
            .unwrap_or_else(|| serde_json::json!({}));
        if let Some(object) = metadata.as_object_mut() {
            object.insert(
                "micro_compaction".into(),
                serde_json::to_value(state).unwrap_or_default(),
            );
        }
        if let Err(error) = self.store.put_session_metadata(session_id, &metadata) {
            tracing::warn!(%session_id, %error, "could not persist the micro-compaction cutoff");
        }
    }

    fn record_token_counting(
        &self,
        session_id: &str,
        last_request: Option<&crate::turn_loop::types::RequestMeasurement>,
        tool_schema_tokens: u32,
    ) {
        let history = self
            .store
            .load_session_history(session_id)
            .unwrap_or_default();
        let mut state = self.token_counting(session_id);

        state.tool_schema_tokens = u64::from(tool_schema_tokens);

        let anchor = last_request
            .filter(|request| !request.spliced_after && request.usage.measured_context_tokens() > 0)
            .and_then(|request| {
                Some((
                    request.input_messages.checked_sub(1)?,
                    request.usage.measured_context_tokens(),
                ))
            })
            .filter(|(length, _)| *length <= history.len());
        match anchor {
            Some((length, tokens)) => state.measured(length, tokens),
            None => state.record_turn(&history, self.token_counting_strategy()),
        }
        self.save_token_counting(session_id, &state);
    }

    pub fn session_context_tokens(&self, session_id: &str) -> u64 {
        let history = self
            .store
            .load_session_history(session_id)
            .unwrap_or_default();
        self.token_counting(session_id)
            .status_size(&history, self.token_counting_strategy())
    }

    pub fn record_context_truncation(&self, session_id: &str) {
        let history = self
            .store
            .load_session_history(session_id)
            .unwrap_or_default();
        let mut state = self.token_counting(session_id);
        state.truncate(&history, history.len());
        self.save_token_counting(session_id, &state);

        let mut micro = self.micro_compaction_state(session_id);
        micro.clamp(history.len());
        self.save_micro_compaction_state(session_id, &micro);

        if let Some(cell) = self
            .media_recovery
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(session_id)
            .cloned()
        {
            cell.lock()
                .unwrap_or_else(|e| e.into_inner())
                .advance_revision();
        }
    }

    pub fn record_context_rebase(&self, session_id: &str) {
        self.record_context_rebase_with_summary(session_id, None);
    }

    pub fn record_context_rebase_with_summary(
        &self,
        session_id: &str,
        summary_output_tokens: Option<u32>,
    ) {
        let history = self
            .store
            .load_session_history(session_id)
            .unwrap_or_default();
        let tokens = rebased_tokens(&history, summary_output_tokens);
        let mut state = self.token_counting(session_id);
        state.rebase(history.len(), tokens, false);
        self.save_token_counting(session_id, &state);

        let mut micro = self.micro_compaction_state(session_id);
        micro.reset();
        self.save_micro_compaction_state(session_id, &micro);

        self.reset_cache_baseline(session_id);
    }

    pub fn observe_cache_observations(
        &self,
        session_id: &str,
        turn_id: &str,
        agent_id: &str,
        observations: &[crate::session::cache_observation::CacheObservation],
        resets: &[usize],
        probe_request: Option<&crate::session::cache_observation::CacheProbeRequest>,
    ) {
        if observations.is_empty() && probe_request.is_none() {
            return;
        }
        let callbacks = self.session_callbacks_for(session_id);
        let mut states = self.cache_states.lock().unwrap_or_else(|e| e.into_inner());
        let state = states.entry(session_id.to_string()).or_default();
        for (index, observation) in observations.iter().enumerate() {
            if resets.contains(&index) {
                state.tracker.reset();
            }
            state.last = Some(observation.clone());
            let now = chrono::Utc::now().timestamp_millis();
            let Some(breakage) =
                state
                    .tracker
                    .observe(&observation.usage(), Some(observation), now)
            else {
                continue;
            };
            tracing::warn!(
                %session_id,
                %turn_id,
                step = index + 1,
                cause = breakage.cause.as_str(),
                previous_cache_read = breakage.previous.input_cache_read,
                current_cache_read = breakage.current.input_cache_read,
                "prompt cache read collapsed"
            );
            callbacks.telemetry(breakage.telemetry_payload(Some(turn_id), Some(index as u32 + 1)));
            state.breaks.push_back(breakage);
            while state.breaks.len() > CACHE_BREAK_RETENTION {
                state.breaks.pop_front();
            }
        }
        if let Some(request) = probe_request {
            state.requests.insert(agent_id.to_string(), request.clone());
        }
    }

    pub fn cache_observation(&self, session_id: &str) -> serde_json::Value {
        let states = self.cache_states.lock().unwrap_or_else(|e| e.into_inner());
        let Some(state) = states.get(session_id) else {
            return serde_json::json!({
                "breaks": [],
                "last": null,
                "baseline": null,
                "requests": {},
            });
        };
        serde_json::json!({
            "breaks": state.breaks.iter().collect::<Vec<_>>(),
            "last": state.last,
            "baseline": state.tracker.baseline(),
            "requests": state.requests,
        })
    }

    fn reset_cache_baseline(&self, session_id: &str) {
        let mut states = self.cache_states.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(state) = states.get_mut(session_id) {
            state.tracker.reset();
            state.last = None;
            state.requests.clear();
        }
        drop(states);
        self.subagent_manager
            .bump_probe_generation(crate::callbacks::MAIN_AGENT_ID);
    }

    fn cache_probe_request(
        &self,
        session_id: &str,
    ) -> Option<crate::session::cache_observation::CacheProbeRequest> {
        let states = self.cache_states.lock().unwrap_or_else(|e| e.into_inner());
        states
            .get(session_id)
            .and_then(|state| state.requests.get(crate::callbacks::MAIN_AGENT_ID))
            .cloned()
    }

    pub async fn session_llm(&self, session_id: &str) -> Option<Arc<dyn LLM>> {
        let spec = self.session_spec(session_id).await;
        let callbacks = self.session_callbacks_for(session_id);
        let sections = self.plugin_contributions().sections;
        crate::pipeline::build_llm_for_spec(&spec, &callbacks, sections.as_deref()).ok()
    }

    pub fn session_callbacks_for(&self, session_id: &str) -> Arc<dyn HostCallbacks> {
        let ws_root = match self.store.get_session(session_id) {
            Ok(Some(s)) if s.workspace_id.is_some() => {
                let wid = s.workspace_id.unwrap();
                self.store
                    .get_workspace(&wid)
                    .ok()
                    .flatten()
                    .map(|w| std::path::PathBuf::from(w.root))
            }
            _ => None,
        };
        let ws_ref = ws_root.as_deref().unwrap_or(std::path::Path::new("."));
        self.session_callbacks(session_id, ws_ref)
    }

    pub async fn compact_session(
        &self,
        session_id: &str,
        instruction: Option<&str>,
    ) -> Result<crate::session::sqlite_store::CompactionReport, EngineError> {
        self.hub
            .bus_for(session_id)
            .publish(&crate::events::EngineEvent::Custom(serde_json::json!({
                "type": "compaction.started",
                "sessionId": session_id,
                "instruction": instruction,
            })));
        let llm = self.session_llm(session_id).await.ok_or_else(|| {
            EngineError::NoModel(
                "no model is bound to this session; compact requires a model to write the summary"
                    .into(),
            )
        })?;
        let history = self
            .store
            .load_session_history(session_id)
            .map_err(EngineError::Store)?;
        let config = crate::compaction::CompactionConfig {
            max_attempts: self.compaction_max_attempts(),
            ..crate::compaction::CompactionConfig::default()
        };
        let count = crate::compaction::compute_compact_count_manual(&history, &config);
        let started = std::time::Instant::now();
        let mut summary_usage = None;
        let mut summary_retry_count = 0;
        let mut summary_dropped_count = 0;

        let probe_baseline = self.cache_probe_request(session_id);
        let mut summary_probe = None;
        let summary = if count == 0 {
            None
        } else {
            let configured_window = self
                .store
                .get_state("metadata", session_id)
                .ok()
                .flatten()
                .and_then(|metadata| {
                    metadata
                        .get("max_context_tokens")
                        .and_then(|value| value.as_u64())
                        .and_then(|value| u32::try_from(value).ok())
                });
            let effective_window = crate::compaction::effective_max_tokens_for(
                crate::callbacks::MAIN_AGENT_ID,
                llm.model_name(),
                configured_window,
            );
            let emit_request = |event: serde_json::Value| {
                self.hub
                    .bus_for(session_id)
                    .publish(&crate::events::EngineEvent::Custom(event));
            };
            let summary = crate::compaction::summarize_with_llm_budgeted(
                &history[1..count as usize],
                llm.as_ref(),
                instruction,
                None,
                config.max_attempts,
                effective_window,
                crate::callbacks::MAIN_AGENT_ID,
                probe_baseline.as_ref(),
                None,
                Some(&emit_request),
            )
            .await
            .map_err(|error| EngineError::Turn(error.to_string()))?;
            summary_usage = summary.usage.clone();
            summary_retry_count = summary.retry_count;
            summary_dropped_count = summary.dropped_count;
            summary_probe = summary.cache_probe.clone();
            Some(crate::compaction::post_process_summary(
                summary.summary,
                self.session_todo_summary(session_id),
            ))
        };
        let duration_ms = started.elapsed().as_millis() as u64;
        let summary_text = summary.clone();
        let (report, compacted_messages) = self
            .store
            .compact_session_with_summary(session_id, instruction, summary)
            .map_err(EngineError::Turn)?;
        let summary_output_tokens = summary_usage
            .as_ref()
            .map(|usage| u32::try_from(usage.output))
            .transpose()
            .map_err(|error| EngineError::Turn(error.to_string()))?;

        if let Some(usage) = &summary_usage
            && let Err(e) = crate::session::usage::record_compaction(
                &self.store,
                session_id,
                llm.model_name(),
                usage,
            )
        {
            tracing::warn!(%session_id, %e, "could not persist compaction usage");
        }
        if report.removed > 0 {
            tracing::debug!(%session_id, removed = report.removed, "manual compaction folded the history");
        }

        if count > 0 {
            let callbacks = self.session_callbacks_for(session_id);

            if let Some(probe) = summary_probe.as_ref() {
                callbacks.telemetry(probe.telemetry_payload());
            }
            callbacks.telemetry(
                crate::compaction::CompactionReport {
                    summary: String::new(),
                    compacted_count: report.compacted_count,
                    dropped_count: summary_dropped_count,
                    tokens_before: report.tokens_before,
                    tokens_after: report.tokens_after,
                    usage: summary_usage,
                    retry_count: summary_retry_count,
                    duration_ms,
                    cache_probe: None,
                }
                .finished_payload(
                    "manual",
                    1,
                    llm.thinking_effort().unwrap_or("none"),
                    None,
                ),
            );
        }

        let mut result = serde_json::json!({
            "summary": summary_text.unwrap_or_default(),
            "compactedCount": report.compacted_count,
            "tokensBefore": report.tokens_before,
            "tokensAfter": report.tokens_after,
            "keptUserMessageCount": report.kept_user_message_count,
        });
        if summary_dropped_count > 0 {
            result["droppedCount"] = serde_json::json!(summary_dropped_count);
        }
        if report.removed > 0 {
            self.hub
                .bus_for(session_id)
                .publish(&crate::events::EngineEvent::Custom(serde_json::json!({
                    "type": "compaction.completed",
                    "sessionId": session_id,
                    "result": result,
                    "compactedCount": report.compacted_count,
                    "removed": report.removed,
                    "tokensBefore": report.tokens_before,
                    "tokensAfter": report.tokens_after,
                    "keptUserMessageCount": report.kept_user_message_count,
                    "messages": compacted_messages,
                })));
            self.record_context_rebase_with_summary(session_id, summary_output_tokens);
        }
        self.hub
            .bus_for(session_id)
            .publish(&crate::events::EngineEvent::Custom(serde_json::json!({
                "type": "event.session.history_compacted",
                "before_seq": serde_json::Value::Null,
                "reason": "manual",
                "summary_message_id": serde_json::Value::Null,
            })));

        self.publish_status_updated(session_id).await;
        Ok(report)
    }

    pub async fn task_runner(&self) -> Option<Arc<crate::storage::TaskRunner>> {
        self.subagent_manager.get_task_runner().await
    }

    pub async fn take_task_notification_prompt(
        &self,
        session_id: &str,
    ) -> Option<(String, serde_json::Value)> {
        let runner = self.task_runner().await?;
        crate::server::prompt_queue::take_task_wake_prompt(&runner, session_id)
    }

    pub async fn session_mcp_entries(
        &self,
        session_id: &str,
    ) -> Vec<crate::mcp::manager::McpServerEntry> {
        match self.mcp_manager_for(session_id) {
            Some(manager) => manager.server_entries().await,
            None => Vec::new(),
        }
    }

    pub async fn background_tasks(&self) -> Vec<serde_json::Value> {
        match self.subagent_manager.get_task_runner().await {
            Some(runner) => runner.list(),
            None => Vec::new(),
        }
    }

    pub fn session_todos(&self, session_id: &str) -> Vec<crate::tools::todo_item::TodoItem> {
        let ws_root = match self.store.get_session(session_id) {
            Ok(Some(s)) if s.workspace_id.is_some() => {
                let wid = s.workspace_id.unwrap();
                self.store
                    .get_workspace(&wid)
                    .ok()
                    .flatten()
                    .map(|w| std::path::PathBuf::from(w.root))
            }
            _ => None,
        };
        let ws_ref = ws_root.as_deref().unwrap_or(std::path::Path::new("."));
        let read_todo_items =
            |store: crate::storage::StateStore| match store.read_domain_state("todo") {
                ReadOutcome::Found(value) => Some(value),
                ReadOutcome::Absent => None,
                ReadOutcome::Unreadable(cause) => {
                    tracing::warn!(%cause, "todo state is unreadable; this summary shows no items");
                    None
                }
            };
        let session_domain = crate::storage::StateStore::for_session(ws_ref, session_id)
            .ok()
            .and_then(read_todo_items);
        let raw = session_domain.or_else(|| {
            crate::storage::StateStore::for_workspace(ws_ref)
                .ok()
                .and_then(read_todo_items)
        });
        crate::tools::todo_item::read_todo_items(raw.as_ref().unwrap_or(&serde_json::Value::Null))
    }

    pub fn session_todo_summary(&self, session_id: &str) -> Option<String> {
        let ws_root = match self.store.get_session(session_id) {
            Ok(Some(s)) if s.workspace_id.is_some() => {
                let wid = s.workspace_id.unwrap();
                self.store
                    .get_workspace(&wid)
                    .ok()
                    .flatten()
                    .map(|w| std::path::PathBuf::from(w.root))
            }
            _ => None,
        };
        let ws_ref = ws_root.as_deref().unwrap_or(std::path::Path::new("."));
        let read_todo_items =
            |store: crate::storage::StateStore| match store.read_domain_state("todo") {
                ReadOutcome::Found(value) => Some(value),
                ReadOutcome::Absent => None,
                ReadOutcome::Unreadable(cause) => {
                    tracing::warn!(%cause, "todo state is unreadable; this summary shows no items");
                    None
                }
            };
        let session_domain = crate::storage::StateStore::for_session(ws_ref, session_id)
            .ok()
            .and_then(read_todo_items);
        let raw = session_domain.or_else(|| {
            crate::storage::StateStore::for_workspace(ws_ref)
                .ok()
                .and_then(read_todo_items)
        });
        crate::compaction::todo_list_for_summary(raw.as_ref())
    }

    pub async fn llm_for_model(&self, model: &str) -> Option<Arc<dyn LLM>> {
        let native = self.resolved_native_llm(model).await?;
        let mut spec = clone_spec(&self.spec);
        spec.model_name = model.to_string();
        spec.native_llm = Some(native);

        spec.providers.clear();
        let callbacks: Arc<dyn HostCallbacks> =
            Arc::new(ServerHost::standalone().with_oauth(self.oauth_manager()));
        let sections = self.plugin_contributions().sections;
        crate::pipeline::build_llm_for_spec(&spec, &callbacks, sections.as_deref()).ok()
    }

    fn apply_prompt_metadata(&self, session_id: &str, prompt: &str, origin: Option<&Value>) {
        let client_metadata = origin.and_then(|origin| origin.get("clientMetadata"));
        let Some(text) =
            crate::session::title::prompt_metadata_text_from_parts(prompt, client_metadata)
        else {
            return;
        };
        let title = crate::session::title::title_from_prompt_metadata_text(&text);
        match self.store.apply_prompt_metadata(session_id, &text, &title) {
            Ok(applied) => {
                let current_title = self
                    .store
                    .get_session(session_id)
                    .ok()
                    .flatten()
                    .and_then(|session| session.title);
                let patch = if applied {
                    serde_json::json!({
                        "title": title,
                        "lastPrompt": text,
                        "isCustomTitle": false,
                    })
                } else {
                    serde_json::json!({ "lastPrompt": text })
                };
                self.hub
                    .bus_for(session_id)
                    .publish(&crate::events::EngineEvent::Custom(serde_json::json!({
                        "type": "session.meta.updated",
                        "agentId": crate::callbacks::MAIN_AGENT_ID,
                        "sessionId": session_id,
                        "title": current_title,
                        "patch": patch,
                    })));
            }
            Err(error) => {
                tracing::warn!(%session_id, %error, "failed to persist prompt metadata");
            }
        }
    }

    pub async fn run_turn(
        &self,
        session_id: &str,
        turn_number: u32,
        history: Vec<LLMMessage>,
        prompt: &str,
    ) -> Result<TurnReport, EngineError> {
        self.run_turn_with_media(session_id, turn_number, history, prompt, Vec::new(), None)
            .await
    }

    pub async fn run_turn_with_media(
        &self,
        session_id: &str,
        turn_number: u32,
        history: Vec<LLMMessage>,
        prompt: &str,
        media: Vec<ContentPart>,
        origin: Option<Value>,
    ) -> Result<TurnReport, EngineError> {
        let mut history = history;

        let spec = self.session_spec(session_id).await;

        match self.restore_session_environment(session_id).await {
            Ok(Some(outcome)) => {
                for drift in &outcome.drift {
                    tracing::warn!(
                        session_id,
                        ?drift,
                        "session environment drifted from its declaration; keeping the frozen binding"
                    );
                }
            }
            Ok(None) => {}
            Err(error) => return Err(EngineError::Environment(error.to_string())),
        }

        if let Some(root) = self.session_root_path(session_id) {
            if let Some(reminder) = self.observe_agents_md(session_id, &root) {
                history.push(crate::injection::injection_message_named(
                    reminder,
                    "agents_md_change",
                ));
            }
        }

        for (reminder, variant) in self
            .observe_catalog_changes(session_id, &spec, &spec.system_prompt)
            .await
        {
            history.push(crate::injection::injection_message_named(reminder, variant));
        }
        let ws_root = match self.store.get_session(session_id) {
            Ok(Some(s)) if s.workspace_id.is_some() => {
                let wid = s.workspace_id.unwrap();
                self.store
                    .get_workspace(&wid)
                    .ok()
                    .flatten()
                    .map(|w| std::path::PathBuf::from(w.root))
            }
            _ => None,
        };
        let ws_ref = ws_root.as_deref().unwrap_or(std::path::Path::new("."));

        self.apply_thinking_guard_setting().await;
        let host_callbacks = self.session_callbacks(session_id, ws_ref);

        let message_callbacks = Arc::new(
            crate::server::message_events::MessageCallbacks::with_step_tracker(
                host_callbacks,
                session_id,
                self.hub.clone(),
                turn_number,
                prompt,
                &media,
                origin.clone(),
                None,
            ),
        );
        let host_callbacks: Arc<dyn HostCallbacks> = message_callbacks.clone();

        self.ensure_mcp_status_bridge(session_id);
        let contributions = self.plugin_contributions();
        let pipeline = build_engine_pipeline(
            &spec,
            host_callbacks,
            PipelineHost {
                subagent_manager: self.subagent_manager.clone(),

                plugin_sections: contributions.sections,
                plugin_hooks: contributions.hooks,

                task_runner: self.subagent_manager.get_task_runner().await,
                parent_cancel: None,
                parent_cancel_slot: None,
                steer_slot: Some(self.steer_slot(session_id)),
                mcp_manager: self.mcp_manager_for(session_id),

                event_bus: Some(self.hub.bus_for(session_id)),

                task_event_sink: None,
            },
        )
        .await
        .map_err(|error| EngineError::NoModel(error.message))?;

        if let Some(toolset) = pipeline.toolset.as_ref() {
            let _ = self.bind_tower_wake(toolset, session_id);
        }

        pipeline.callbacks.set_file_history(
            self.store.clone(),
            session_id.to_string(),
            turn_number as usize,
        );

        pipeline.callbacks.set_session_store(self.store.clone());
        pipeline.callbacks.set_tool_select_persist_listener({
            let persisted_heads = self.hub.persisted_heads();
            std::sync::Arc::new(move |session_id, seq| persisted_heads.note(session_id, seq))
        });

        if let Some(config) = self.micro_compaction_config().await {
            let now = chrono::Utc::now().timestamp_millis();
            let last_assistant_at = self.last_assistant_at_for(session_id);

            let context_tokens =
                self.token_counting(session_id)
                    .status_size(&history, self.token_counting_strategy()) as usize;
            let outcome_gate = crate::compaction::micro::detect(
                last_assistant_at,
                now,
                context_tokens,
                self.model_context_window(session_id).await,
                &config,
            );

            let mut micro = self.micro_compaction_state(session_id);
            let previous_cutoff = micro.cutoff;
            let advanced = outcome_gate.triggered()
                && micro.advance(history.len(), config.keep_recent_messages);
            if advanced {
                self.save_micro_compaction_state(session_id, &micro);
                self.hub
                    .bus_for(session_id)
                    .publish(&crate::events::EngineEvent::Custom(serde_json::json!({
                        "type": "micro_compaction.apply",
                        "sessionId": session_id,
                        "cutoff": micro.cutoff,
                    })));
            }
            if micro.cutoff > 0 {
                let message_count = history.len();
                let tokens_before = crate::compaction::estimate_messages_tokens(&history) as usize;
                let outcome = crate::compaction::micro::apply_micro_compaction_at(
                    &history,
                    micro.cutoff,
                    &config,
                );
                if outcome.changed {
                    history = outcome.messages;
                }

                if advanced {
                    let tokens_after =
                        crate::compaction::estimate_messages_tokens(&history) as usize;
                    let truncated_before: usize =
                        outcome.replaced.iter().map(|item| item.tokens_before).sum();
                    let truncated_after: usize =
                        outcome.replaced.iter().map(|item| item.tokens_after).sum();
                    let cache_age_ms =
                        last_assistant_at.map(|last| now.saturating_sub(last).max(0));
                    pipeline.callbacks.emit_event(serde_json::json!({
                        "event": "micro_compaction_finished",
                        "keep_recent_messages": config.keep_recent_messages,
                        "min_content_tokens": config.min_content_tokens,
                        "cache_missed_threshold_ms": config.cache_missed_threshold_ms,
                        "truncated_marker": config.truncated_marker,
                        "min_context_usage_ratio": config.min_context_usage_ratio,
                        "truncated_tool_result_count": outcome.replaced.len(),
                        "truncated_tool_result_tokens_before": truncated_before,
                        "truncated_tool_result_tokens_after": truncated_after,
                        "tokens_before": tokens_before,
                        "tokens_after": tokens_after,
                        "previous_cutoff": previous_cutoff,
                        "cutoff": micro.cutoff,
                        "message_count": message_count,
                        "cache_age_ms": cache_age_ms,
                        "thinking_effort": pipeline
                            .llm
                            .thinking_effort()
                            .unwrap_or("none"),
                    }));
                }
            }
        }

        self.execute(
            pipeline.llm.as_ref(),
            &pipeline.callbacks,
            pipeline.hook_guard.clone(),
            session_id,
            turn_number,
            history,
            prompt,
            media,
            pipeline.secondary_llm.clone(),
            pipeline.toolset.clone(),
            origin,
            Some(message_callbacks),
        )
        .await
    }

    pub async fn run_turn_on(
        &self,
        llm: &dyn LLM,
        session_id: &str,
        turn_number: u32,
        history: Vec<LLMMessage>,
        prompt: &str,
    ) -> Result<TurnReport, EngineError> {
        let ws_ref = self
            .session_workspace(session_id)
            .map(|workspace| std::path::PathBuf::from(&workspace.root))
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let callbacks = self.session_callbacks(session_id, &ws_ref);
        let message_callbacks = Arc::new(crate::server::message_events::MessageCallbacks::new(
            callbacks,
            session_id,
            self.hub.clone(),
            turn_number,
            prompt,
        ));
        let callbacks: Arc<dyn HostCallbacks> = message_callbacks.clone();
        self.execute(
            llm,
            &callbacks,
            None,
            session_id,
            turn_number,
            history,
            prompt,
            Vec::new(),
            None,
            None,
            None,
            Some(message_callbacks),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute(
        &self,
        llm: &dyn LLM,
        callbacks: &Arc<dyn HostCallbacks>,
        hook_guard: Option<Arc<crate::tools::external_hooks::HookGuard>>,
        session_id: &str,
        turn_number: u32,
        history: Vec<LLMMessage>,
        prompt: &str,
        media: Vec<ContentPart>,
        secondary_llm: Option<Arc<dyn LLM>>,
        toolset: Option<Arc<crate::tools::NativeToolset>>,
        origin: Option<Value>,
        message_callbacks: Option<Arc<crate::server::message_events::MessageCallbacks>>,
    ) -> Result<TurnReport, EngineError> {
        let _occupancy = crate::session::lock::SESSION_LOCKS
            .hold_guard(session_id)
            .map_err(|error| EngineError::Occupied(error.to_string()))?;

        if self.journal_moved(session_id) {
            self.observe_journal_head(session_id);
            crate::session::lock::SESSION_LOCKS.release(session_id);
            return Err(EngineError::Stale(format!(
                "Session \"{session_id}\" was updated by another client; reload it before continuing"
            )));
        }
        let turn_id = format!("turn-{}", fastrand::u64(..));

        let turn_model = llm.model_name().to_string();
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let mut turns = self
                .active_turns
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            turns.insert(session_id.to_string(), Arc::clone(&cancel));
        }
        self.active_turn_numbers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(session_id.to_string(), turn_number);
        self.publish_work_changed(session_id, true, None);

        if let Err(error) = self.store.set_last_turn_reason(session_id, None) {
            tracing::warn!(%session_id, %error, "failed to clear last_turn_reason at turn start");
        }

        self.apply_prompt_metadata(session_id, prompt, origin.as_ref());
        self.publish_status_updated(session_id).await;

        *self
            .steer_slot(session_id)
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(crate::subagent::types::ParentCancel::new());

        let activity = self.activity_registry.tracker(session_id);
        activity.turn_started(turn_number);

        if let Some(log) = self.session_log(session_id) {
            log.log(
                crate::session::session_log::LogLevel::Info,
                "turn started",
                &[
                    ("turn", &turn_number.to_string()),
                    ("model", &self.spec.model_name),
                ],
            );
        }

        let undo_anchor = origin
            .as_ref()
            .is_none_or(crate::session::origin_is_undo_anchor);
        if undo_anchor && let Err(error) = callbacks.checkpoint_state().await {
            tracing::warn!(%session_id, %error, "state checkpoint failed; undo cannot restore this turn's state");
        }

        if let Some(callbacks) = message_callbacks.as_ref() {
            callbacks.announce_prompt();
        }

        let callbacks: Arc<dyn HostCallbacks> = Arc::new(
            crate::session::SteerQueueCallbacks::new(
                callbacks.clone(),
                self.steer_queue(session_id),
                None,
            )
            .with_steer_slot(Some(self.steer_slot(session_id))),
        );

        let callbacks: Arc<dyn HostCallbacks> =
            Arc::new(crate::server::activity::ActivityCallbacks {
                inner: callbacks.clone(),
                tracker: activity.clone(),
            });
        struct ActiveGuard<'a> {
            engine: &'a ServerEngine,
            session_id: String,
        }
        impl<'a> Drop for ActiveGuard<'a> {
            fn drop(&mut self) {
                let mut turns = self
                    .engine
                    .active_turns
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                turns.remove(&self.session_id);
                drop(turns);
                self.engine
                    .active_turn_numbers
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&self.session_id);
                self.engine.release_turn_steer_state(&self.session_id);
            }
        }
        let _guard = ActiveGuard {
            engine: self,
            session_id: session_id.to_string(),
        };

        let mut messages = history;
        let user_message = LLMMessage {
            role: kimi_core::types::Role::User,
            content: prompt.to_string(),
            blocks: media,
            tool_calls: Vec::new(),
            tool_call_id: None,

            prompt_id: None,

            origin: origin.clone(),
            tools: Vec::new(),
        };
        messages.push(user_message.clone());
        let input_len = messages.len();

        let goal = callbacks.goal().await.ok().flatten();

        let context_window = self
            .model_context_window(session_id)
            .await
            .and_then(|value| u32::try_from(value).ok());

        let previous_turn_aborted = self.take_last_turn_aborted(session_id);
        let input = RunTurnInput {
            llm_request_byte_budget: self
                .spec
                .llm_request_byte_budget
                .unwrap_or(crate::config::DEFAULT_LLM_REQUEST_BYTE_BUDGET),
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            max_attempts: self.max_attempts,
            turn_id: turn_id.clone(),
            llm,
            messages,
            tools: &[],
            tool_defs: vec![],
            max_steps: self.max_steps,
            max_context_tokens: context_window,
            compaction_max_attempts: self.compaction_max_attempts,
            permission_mode: self
                .spec
                .policy_snapshot
                .as_ref()
                .map(|snapshot| snapshot.mode),
            goal,
            cancellation: Some(cancel),
            hook_guard,
            media: Some(&self.media),
            media_dropped: Some(self.media_dropped_for(session_id)),
            media_recovery: Some(self.media_recovery_for(session_id)),

            toolset,
            previous_turn_aborted,
        };

        let observations = std::sync::Arc::new(std::sync::Mutex::new(
            crate::turn_loop::types::TurnObservations::default(),
        ));

        let turn = match run_turn_continued_with_observations(
            input,
            &callbacks,
            Some(observations.clone()),
        )
        .await
        {
            Ok(result) => Ok(result),
            Err(error) => Err(error.to_string()),
        };

        self.observe_journal_head(session_id);

        if let Some(manager) = self.interaction_manager() {
            manager.cancel_turn(session_id, &turn_id, "turn_ended");
        }
        let result = match turn {
            Ok(result) => {
                let reason = work_turn_reason(&result.stop_reason);
                if let Some(log) = self.session_log(session_id) {
                    log.log(
                        crate::session::session_log::LogLevel::Info,
                        "turn ended",
                        &[("turn", &turn_number.to_string()), ("reason", reason)],
                    );
                }
                self.publish_turn_ended(session_id, turn_number, reason);

                self.publish_work_changed(
                    session_id,
                    self.has_background_work(session_id),
                    Some(reason),
                );

                if let Err(e) = self.store.set_last_turn_reason(session_id, Some(reason)) {
                    tracing::warn!(%session_id, %e, "could not persist the turn outcome");
                }

                self.observe_cache_observations(
                    session_id,
                    &turn_id,
                    crate::callbacks::MAIN_AGENT_ID,
                    &result.cache_observations,
                    &result.cache_observation_resets,
                    result.cache_probe_requests.last(),
                );
                result
            }
            Err(error) => {
                if let Some(log) = self.session_log(session_id) {
                    log.log(
                        crate::session::session_log::LogLevel::Error,
                        "turn failed",
                        &[
                            ("turn", &turn_number.to_string()),
                            ("error", error.as_str()),
                        ],
                    );
                }

                if let Ok(partial) = observations.lock() {
                    self.observe_cache_observations(
                        session_id,
                        &turn_id,
                        crate::callbacks::MAIN_AGENT_ID,
                        &partial.cache_observations,
                        &partial.cache_observation_resets,
                        partial.cache_probe_requests.last(),
                    );
                }
                self.publish_turn_ended(session_id, turn_number, "failed");
                self.publish_work_changed(
                    session_id,
                    self.has_background_work(session_id),
                    Some("failed"),
                );
                if let Err(e) = self.store.set_last_turn_reason(session_id, Some("failed")) {
                    tracing::warn!(%session_id, %e, "could not persist the turn outcome");
                }
                self.publish_status_updated(session_id).await;
                activity.interrupted(
                    crate::server::activity::InterruptReason::Error,
                    Some(error.clone()),
                );
                crate::session::goal_turn::settle_goal_after_turn(
                    callbacks.as_ref(),
                    crate::session::goal_turn::GoalTurnEnd::Failed { error: &error },
                )
                .await;
                return Err(EngineError::Turn(error));
            }
        };

        let mut transcript =
            Vec::with_capacity(1 + result.messages.len().saturating_sub(input_len));
        transcript.push(user_message);
        transcript.extend(result.messages.iter().skip(1 + input_len).cloned());
        self.store
            .save_turn(
                session_id,
                &turn_id,
                turn_number,
                &transcript,
                Some(&result.usage),
                origin.as_ref(),
            )
            .map_err(EngineError::Store)?;

        self.record_token_counting(
            session_id,
            result.last_request_usage.as_ref(),
            result.tool_schema_tokens,
        );

        if let Err(e) =
            crate::session::usage::record_turn(&self.store, session_id, &turn_model, &result.usage)
        {
            tracing::warn!(%session_id, %e, "could not persist turn usage");
        }

        if let Some(compaction_usage) = &result.compaction_usage
            && let Err(e) = crate::session::usage::record_compaction(
                &self.store,
                session_id,
                &turn_model,
                compaction_usage,
            )
        {
            tracing::warn!(%session_id, %e, "could not persist compaction usage");
        }

        crate::session::goal_turn::settle_goal_after_turn(
            callbacks.as_ref(),
            crate::session::goal_turn::GoalTurnEnd::Ran {
                stop: &result.stop_reason,
                output_tokens: result.usage.output.into(),
            },
        )
        .await;

        self.stamp_last_assistant_at(session_id);

        self.publish_status_updated(session_id).await;

        let context_tokens = self.session_context_tokens(session_id);
        self.hub
            .bus_for(session_id)
            .publish(&crate::events::EngineEvent::Custom(serde_json::json!({
                "type": "event.session.usage_updated",
                "usage": {
                    "input_tokens": result.usage.input_other,
                    "output_tokens": result.usage.output,
                    "cache_read_tokens": result.usage.input_cache_read,
                    "cache_creation_tokens": result.usage.input_cache_creation,
                    "context_tokens": context_tokens,
                    "turn_count": turn_number,
                },
                "delta": {
                    "input_tokens": result.usage.input_other,
                    "output_tokens": result.usage.output,
                    "cache_read_tokens": result.usage.input_cache_read,
                    "cache_creation_tokens": result.usage.input_cache_creation,
                },
            })));
        activity.turn_ended(work_turn_reason(&result.stop_reason));

        let reply = transcript
            .iter()
            .rev()
            .find(|message| message.role == "assistant")
            .map(|message| message.content.clone())
            .unwrap_or_default();

        self.spawn_memory_filing(
            llm.system_prompt(),
            &transcript,
            prompt,
            &reply,
            secondary_llm,
        )
        .await;

        Ok(TurnReport {
            turn_id,
            reply,
            stop_reason: format!("{:?}", result.stop_reason),
            steps: result.steps,
            usage: result.usage,
            events_emitted: result.events_emitted,
            llm_transport: result.llm_transport,
            native_tool_calls: result.native_tool_calls,
            context_window,
            gate_blocked: result.gate_blocked,
        })
    }
}

fn work_turn_reason(reason: &crate::turn_loop::types::LoopTurnStopReason) -> &'static str {
    use crate::turn_loop::types::LoopTurnStopReason::*;
    match reason {
        Aborted => "cancelled",
        MaxSteps | Filtered => "failed",
        EndTurn | MaxTokens | Paused | Unknown | BudgetLimited | RepeatBreaker => "completed",
    }
}

fn rebased_tokens(
    history: &[crate::turn_loop::types::LLMMessage],
    summary_output_tokens: Option<u32>,
) -> u64 {
    let estimate = crate::session::token_counting::estimate_messages(history);
    let Some(measured) = summary_output_tokens else {
        return estimate;
    };
    let Some(summary) = history
        .iter()
        .find(|message| crate::compaction::is_compaction_summary_message(message))
    else {
        return estimate;
    };
    let summary_estimate = crate::session::token_counting::estimate_message(summary);
    estimate.saturating_sub(summary_estimate) + u64::from(measured)
}

fn apply_session_overrides(spec: &mut PipelineSpec, profile: &serde_json::Value) {
    if let Some(model) = profile
        .get("model")
        .and_then(|v| v.as_str())
        .filter(|model| !model.is_empty())
    {
        spec.model_name = model.to_string();
        if let Some(native) = spec.native_llm.as_mut() {
            native.model = model.to_string();
        }
    }
    if let Some(effort) = profile
        .get("thinking")
        .and_then(|v| v.as_str())
        .filter(|effort| !effort.is_empty())
        && let Some(native) = spec.native_llm.as_mut()
    {
        let thinking_on = effort != "off" && effort != "none";
        let is_kimi_anthropic = native.protocol == "anthropic"
            && native
                .provider_type
                .as_deref()
                .is_some_and(|value| value.eq_ignore_ascii_case("kimi"));
        if is_kimi_anthropic {
            native.thinking_budget = None;
            native.reasoning_effort = Some(if thinking_on {
                effort.to_string()
            } else {
                "off".to_string()
            });
        } else if native.protocol == "anthropic" {
            if native.adaptive_thinking == Some(true) {
                native.thinking_budget = None;
                native.reasoning_effort = Some(if thinking_on {
                    effort.to_string()
                } else {
                    "off".to_string()
                });
            } else {
                native.reasoning_effort = None;
                native.thinking_budget = if thinking_on {
                    Some(match effort {
                        "low" => 1024,
                        "medium" => 4096,
                        "high" | "on" => 32000,
                        other => other
                            .parse::<u32>()
                            .ok()
                            .filter(|v| *v > 0)
                            .unwrap_or(32000),
                    })
                } else {
                    None
                };
            }
        } else {
            native.thinking_budget = None;
            native.reasoning_effort = thinking_on.then(|| effort.to_string());
        }
        if !thinking_on {
            native.thinking_keep = None;
        }
    }
    let disabled: Vec<String> = profile
        .get("disabled_tools")
        .and_then(|v| v.as_array())
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    if !disabled.is_empty() {
        let snapshot = spec
            .policy_snapshot
            .get_or_insert_with(crate::permission::PolicySnapshot::default);
        match snapshot.tools_filter.as_mut() {
            Some(filter) => {
                for name in disabled {
                    if !filter.disabled.contains(&name) {
                        filter.disabled.push(name);
                    }
                }
            }
            None => {
                snapshot.tools_filter = Some(crate::tools::tool_policy::ToolsFilter {
                    enabled: Vec::new(),
                    disabled,
                });
            }
        }
    }
}

fn clone_spec(spec: &PipelineSpec) -> PipelineSpec {
    PipelineSpec {
        notify_user_enabled: true,
        system_prompt: spec.system_prompt.clone(),
        model_name: spec.model_name.clone(),
        providers: spec
            .providers
            .iter()
            .map(|provider| crate::pipeline::PipelineProvider {
                name: provider.name.clone(),
                system_prompt: provider.system_prompt.clone(),
                model: provider.model.clone(),
                native: provider.native.clone(),
            })
            .collect(),
        native_llm: spec.native_llm.clone(),
        workspace_root: spec.workspace_root.clone(),
        native_tools: spec.native_tools,
        extra_roots: spec.extra_roots.clone(),
        rust_self_contained: spec.rust_self_contained,
        shell_path: spec.shell_path.clone(),
        policy_snapshot: spec.policy_snapshot.clone(),
        github_token: spec.github_token.clone(),
        github_base_url: spec.github_base_url.clone(),
        subagent_timeout_ms: spec.subagent_timeout_ms,
        agent_tool_veto: spec.agent_tool_veto.clone(),
        tools_veto: spec.tools_veto.clone(),
        todo_tool_veto: spec.todo_tool_veto.clone(),
        tower_worktree_root: spec.tower_worktree_root.clone(),
        tower_enabled: spec.tower_enabled,
        tool_select: spec.tool_select,
        sandbox_mode: spec.sandbox_mode,
        sandbox_policy: spec.sandbox_policy.clone(),
        secondary_model: spec.secondary_model.clone(),
        caller_agent_id: spec.caller_agent_id.clone(),
        session_id: spec.session_id.clone(),
        image_read_byte_budget: spec.image_read_byte_budget,
        llm_request_byte_budget: spec.llm_request_byte_budget,
        image_max_edge_px: spec.image_max_edge_px,
        model_capabilities: spec.model_capabilities.clone(),
        skill_dirs: spec.skill_dirs.clone(),
        plugin_skill_dirs: spec.plugin_skill_dirs.clone(),
        merge_all_available_skills: spec.merge_all_available_skills,
        background: spec.background,
    }
}

fn agents_md_change_reminder(changes: &[crate::environment::AgentsMdChange]) -> Option<String> {
    use crate::environment::AgentsMdChange;

    let mut stale = Vec::new();
    let mut fresh = Vec::new();
    for change in changes {
        match change {
            AgentsMdChange::Modified { path } => stale.push(format!("- {}", path.display())),
            AgentsMdChange::Removed { path } => {
                stale.push(format!("- {} (deleted)", path.display()));
            }
            AgentsMdChange::BecameUnreadable { path, .. } => {
                stale.push(format!("- {} (unreadable)", path.display()));
            }
            AgentsMdChange::Added { path } | AgentsMdChange::BecameReadable { path } => {
                fresh.push(format!("- {}", path.display()));
            }
        }
    }
    let mut sections = Vec::new();
    if !stale.is_empty() {
        sections.push(format!(
            "The AGENTS.md instruction file(s) below changed on disk after they were injected into the system prompt:\n{}\nRead the current file(s) and follow the latest contents; the copies injected in the system prompt are stale.",
            stale.join("\n")
        ));
    }
    if !fresh.is_empty() {
        sections.push(format!(
            "The following AGENTS.md file(s) exist in your workspace but were not included in your system prompt:\n{}\nRead them before making changes in those directories.",
            fresh.join("\n")
        ));
    }
    (!sections.is_empty()).then(|| sections.join("\n\n"))
}

#[cfg(test)]
mod tests;
