use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};

use parking_lot::Mutex;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use crate::_base::di::instantiation::{
    IInstantiationService, IInstantiationServiceExt, ServicesAccessorExt, create_decorator,
};
use crate::_base::di::lifecycle::{DisposableRef, IDisposable, to_disposable};
use crate::_base::log::log::ILogger;
use crate::_base::state::state_registry::StateKey;
use crate::agent::agents_md_reminder::agents_md_reminder_service::AgentAgentsMdReminderService;
use crate::agent::change_notifier::change_notifier::{BeliefsSeed, IAgentChangeNotifierService};
use crate::agent::environment_binding::agent_environment::IAgentEnvironmentService;
use crate::agent::r#loop::config_section::LoopControl;
use crate::agent::scope_context::scope_context::IAgentScopeContext;
use crate::agent::state::agent_state::IAgentStateService;
use crate::agent::tool_executor::tool_executor::IAgentToolRegistryService;
use crate::agent::tool_registry::tool_contribution::agent_tool_contributions;
use crate::app::agent_identity::agent_identity::IAgentIdentity;
use crate::app::agent_profile_catalog::agent_profile_catalog::{
    AgentProfileContext, DEFAULT_AGENT_PROFILE_NAME, SystemPromptRenderResult,
};
use crate::app::agent_profile_catalog::builtin_agent_profile_loader::IBuiltinAgentProfileLoader;
use crate::app::agent_profile_catalog::model_adaptations::load_curated_adaptation;
use crate::app::agent_profile_catalog::profile_shared::{
    AgentProfileCaller, render_agent_profile_prompt, root_delegation_extras, subagent_allowlist_for,
    without_delegating_targets,
};
use crate::app::bootstrap::bootstrap::IBootstrapService;
use crate::app::config::config::IConfigService;
use crate::app::event::event2::{Event2, Event2Class};
use crate::app::kosong_config::config_section::{
    MODEL_OVERRIDES_SECTION, ModelOverrides, THINKING_SECTION, ThinkingConfig,
};
use crate::app::kosong_config::discovery::IProviderDiscoveryService;
use crate::app::plugin::plugin::IPluginService;
use crate::app::telemetry::telemetry::ITelemetryService;
use crate::environment::environment::EnvironmentWorkspaceRoots;
use crate::environment::environment_workspace_view::EnvironmentWorkspaceView;
use crate::features::associativeMemory::service::AssociativeMemoryService;
use crate::features::gitMemory::gitMemoryService::IGitMemoryService;
use crate::features::notify::sessionNotify::ISessionNotify;
use crate::features::notify::tools::notifyUser::notifyUser::NOTIFY_USER_TOOL_NAME;
use crate::features::plan::tools::exitPlanMode::exitPlanMode::EXIT_PLAN_MODE_TOOL_NAME;
use crate::features::skill::session::skillCatalog::ISessionSkillCatalog;
use crate::features::todo::todoItem::TODO_LIST_TOOL_NAME;
use crate::llm_adapter::contract::capability::{ModelCapability, UNKNOWN_CAPABILITY};
use crate::llm_adapter::model::catalog::{IModelCatalog, Model};
use crate::llm_adapter::model::model_requester::ModelRequestParams;
use crate::llm_adapter::model::model_types::ThinkingDefaults;
use crate::llm_adapter::model::thinking::{
    drives_thinking_through_traits, model_supports_thinking_effort, resolve_forced_thinking_effort,
    resolve_thinking_effort_for_model, resolve_thinking_keep, requires_strict_thinking_validation,
};
use crate::llm_adapter::protocol::protocol::{IProtocolAdapterRegistry, Protocol};
use crate::rpc::types::BoxFuture;
use crate::session::agent_lifecycle::agent_lifecycle::MAIN_AGENT_ID;
use crate::session::session_agent_profile_catalog::session_agent_profile_catalog::ISessionAgentProfileCatalog;
use crate::session::session_context::session_context::ISessionContext;
use crate::session::session_instructions::instructions_provider::ISessionInstructionsProvider;
use crate::session::session_tool_policy::session_tool_policy::ISessionToolPolicy;
use crate::session::session_tool_policy_gate::session_tool_policy_gate::ISessionToolPolicyGate;
use crate::session::workspace_context::workspace_context::ISessionWorkspaceContext;
use crate::state::event_dispatcher::IEventDispatcher;
use crate::tools::tool_policy::{
    GlobalToolsPolicy, ToolActivationPolicy, ToolPolicyLayers, ToolSource,
    describe_inactive_tool_pattern, find_inactive_tool_patterns, is_tool_active_composed,
    literal_tool_names,
};
use crate::wire::record::WireAsyncError;
use kimi_core::primitives::errors::Error2;

use super::context::{
    LoadedAgentsMd, PrepareSystemPromptContextOptions, ProfileContextDeps,
    extract_agents_md_paths_from_system_prompt, prepare_system_prompt_context, split_agents_md_files,
};
use super::profile::{
    AgentConfigData, ApplyProfileOptions, BindAgentInput, ProfileBindingSnapshot, ProfileData,
    ProfileError, ProfileErrors, ProfileModelContext, ProfileServiceOptions, ProfileSetModelResult,
    ProfileUpdateData, ResolvedAgentProfile, SystemPromptContext, ThinkingEffort,
    MODEL_NOT_CONFIGURED,
};
use super::profile_ops::{
    ActiveToolsState, ConfigUpdate, ConfigUpdatePayload, ProfileBind, ProfileBindPayload,
    ProfileModelState, ToolsResetActiveTools, ToolsResetActiveToolsPayload, ToolsSetActiveTools,
    ToolsSetActiveToolsPayload, WarningIssued, WarningIssuedPayload,
    profile_active_tools_replayable_key, profile_replayable_key,
};

pub const PLUGIN_SECTIONS_MAX_BYTES: usize = 64 * 1024;

const TOOLS_SECTION: &str = "tools";

static AGENT_STATUS_UPDATED_CLASS: LazyLock<Event2Class> = LazyLock::new(|| Event2Class {
    type_: "agent.status.updated".to_string(),
    durable: false,
    observable: true,
    agent_domain: true,
    schema: None,
});

pub fn profile_active_tool_names_overlay_key() -> StateKey<Value> {
    StateKey::new("profile.activeToolNamesOverlay", || Value::Null)
}

pub fn profile_agents_md_warning_key() -> StateKey<Value> {
    StateKey::new("profile.agentsMdWarning", || Value::Null)
}

pub fn profile_emitted_thinking_effort_warnings_key() -> StateKey<Value> {
    StateKey::new("profile.emittedThinkingEffortWarnings", || json!([]))
}

pub fn profile_emitted_tool_pattern_warnings_key() -> StateKey<Value> {
    StateKey::new("profile.emittedToolPatternWarnings", || json!([]))
}

pub fn profile_emitted_plugin_budget_warnings_key() -> StateKey<Value> {
    StateKey::new("profile.emittedPluginBudgetWarnings", || json!([]))
}

#[derive(Default)]
struct ProfileOptions {
    emit_status_updated: Option<Arc<dyn Fn() + Send + Sync>>,
}

struct ThinkingState {
    effective: ThinkingEffort,
    #[allow(dead_code)]
    forced: Option<ThinkingEffort>,
}

#[derive(Default)]
struct PromptBuildOptions {
    additional_dirs: Option<Vec<String>>,
    pending_model_alias: Option<String>,
}

struct DelegationCaller {
    profile_name: Option<String>,
    subagents: Option<Vec<String>>,
}

impl AgentProfileCaller for DelegationCaller {
    fn caller_profile_name(&self) -> Option<&str> {
        self.profile_name.as_deref()
    }

    fn caller_subagents(&self) -> Option<&[String]> {
        self.subagents.as_deref()
    }
}

struct AgentProfileServiceInner {
    dispatcher: Arc<dyn IEventDispatcher>,
    telemetry: Arc<dyn ITelemetryService>,
    config: Arc<dyn IConfigService>,
    model_catalog: Arc<dyn IModelCatalog>,
    protocol_adapters: Arc<dyn IProtocolAdapterRegistry>,
    environment: Arc<dyn IAgentEnvironmentService>,
    session_context: Arc<dyn ISessionContext>,
    bootstrap: Arc<dyn IBootstrapService>,
    notify: Arc<dyn ISessionNotify>,
    workspace: Arc<dyn ISessionWorkspaceContext>,
    catalog: Arc<dyn ISessionAgentProfileCatalog>,
    skill_catalog: Arc<dyn ISessionSkillCatalog>,
    instructions: Arc<dyn ISessionInstructionsProvider>,
    session_tool_policy: Arc<dyn ISessionToolPolicy>,
    tool_policy_gate: Arc<dyn ISessionToolPolicyGate>,
    tool_registry: Arc<dyn IAgentToolRegistryService>,
    builtin_profiles: Arc<dyn IBuiltinAgentProfileLoader>,
    scope_context: Arc<dyn IAgentScopeContext>,
    states: Arc<dyn IAgentStateService>,
    plugins: Arc<dyn IPluginService>,
    identity: Arc<dyn IAgentIdentity>,
    agents_md_reminder: Arc<AgentAgentsMdReminderService>,
    discovery: Arc<dyn IProviderDiscoveryService>,
    instantiation: Arc<dyn IInstantiationService>,
    log: Arc<dyn ILogger>,
    git_memory: Arc<dyn IGitMemoryService>,
    associative_memory: Arc<AssociativeMemoryService>,
    options: Mutex<ProfileOptions>,
    active_profile: Mutex<Option<ResolvedAgentProfile>>,
    registered: AtomicBool,
    frozen_skill_listing: Mutex<Option<String>>,
    frozen_plugin_sections: Mutex<Option<String>>,
    model_adaptation_cache: Mutex<HashMap<String, Option<String>>>,
    subscriptions: Mutex<Vec<DisposableRef>>,
}

impl Drop for AgentProfileServiceInner {
    fn drop(&mut self) {
        for subscription in self.subscriptions.get_mut().drain(..) {
            subscription.dispose();
        }
    }
}

#[derive(Clone)]
pub struct AgentProfileService {
    inner: Arc<AgentProfileServiceInner>,
}

impl AgentProfileService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        dispatcher: Arc<dyn IEventDispatcher>,
        telemetry: Arc<dyn ITelemetryService>,
        config: Arc<dyn IConfigService>,
        model_catalog: Arc<dyn IModelCatalog>,
        protocol_adapters: Arc<dyn IProtocolAdapterRegistry>,
        environment: Arc<dyn IAgentEnvironmentService>,
        session_context: Arc<dyn ISessionContext>,
        bootstrap: Arc<dyn IBootstrapService>,
        notify: Arc<dyn ISessionNotify>,
        workspace: Arc<dyn ISessionWorkspaceContext>,
        catalog: Arc<dyn ISessionAgentProfileCatalog>,
        skill_catalog: Arc<dyn ISessionSkillCatalog>,
        instructions: Arc<dyn ISessionInstructionsProvider>,
        session_tool_policy: Arc<dyn ISessionToolPolicy>,
        tool_policy_gate: Arc<dyn ISessionToolPolicyGate>,
        tool_registry: Arc<dyn IAgentToolRegistryService>,
        builtin_profiles: Arc<dyn IBuiltinAgentProfileLoader>,
        scope_context: Arc<dyn IAgentScopeContext>,
        states: Arc<dyn IAgentStateService>,
        plugins: Arc<dyn IPluginService>,
        identity: Arc<dyn IAgentIdentity>,
        agents_md_reminder: Arc<AgentAgentsMdReminderService>,
        discovery: Arc<dyn IProviderDiscoveryService>,
        instantiation: Arc<dyn IInstantiationService>,
        log: Arc<dyn ILogger>,
        git_memory: Arc<dyn IGitMemoryService>,
        associative_memory: Arc<AssociativeMemoryService>,
    ) -> Self {
        let inner = Arc::new(AgentProfileServiceInner {
            dispatcher,
            telemetry,
            config,
            model_catalog,
            protocol_adapters,
            environment,
            session_context,
            bootstrap,
            notify,
            workspace,
            catalog,
            skill_catalog,
            instructions,
            session_tool_policy,
            tool_policy_gate,
            tool_registry,
            builtin_profiles,
            scope_context,
            states,
            plugins,
            identity,
            agents_md_reminder,
            discovery,
            instantiation,
            log,
            git_memory,
            associative_memory,
            options: Mutex::new(ProfileOptions::default()),
            active_profile: Mutex::new(None),
            registered: AtomicBool::new(false),
            frozen_skill_listing: Mutex::new(None),
            frozen_plugin_sections: Mutex::new(None),
            model_adaptation_cache: Mutex::new(HashMap::new()),
            subscriptions: Mutex::new(Vec::new()),
        });
        inner.register();
        inner.install_subscriptions();
        Self { inner }
    }
}

impl AgentProfileServiceInner {
    fn agent_id(&self) -> String {
        self.scope_context.agent_id()
    }

    fn register(&self) {
        if self.registered.swap(true, Ordering::AcqRel) {
            return;
        }
        let _ = self
            .states
            .contribute_state(profile_replayable_key().state_key());
        let _ = self
            .states
            .contribute_state(profile_active_tools_replayable_key().state_key());
        let _ = self
            .states
            .contribute_state(profile_active_tool_names_overlay_key());
        let _ = self.states.contribute_state(profile_agents_md_warning_key());
        let _ = self
            .states
            .contribute_state(profile_emitted_thinking_effort_warnings_key());
        let _ = self
            .states
            .contribute_state(profile_emitted_tool_pattern_warnings_key());
        let _ = self
            .states
            .contribute_state(profile_emitted_plugin_budget_warnings_key());
    }

    fn install_subscriptions(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        if let Ok(registration) = self.dispatcher.hooks().on_did_restore.register(
            "profile",
            Arc::new(move |_ctx, next| {
                let weak = weak.clone();
                Box::pin(async move {
                    let Some(this) = weak.upgrade() else {
                        return next.proceed(None).await;
                    };
                    let replacement = this
                        .model_alias()
                        .and_then(|alias| this.discovery.retired_model_replacement(&alias));
                    if let Some(replacement) = replacement {
                        let _ = this.set_model_inner(replacement).await;
                    }
                    this.sync_telemetry_model_context(this.model_alias());
                    if let Err(error) = this.refresh_bound_subagent_allowlist().await {
                        panic!("{error}");
                    }
                    next.proceed(None).await
                })
            }),
            Default::default(),
        ) {
            self.subscriptions
                .lock()
                .push(to_disposable(move || registration.dispose()));
        }

        let weak = Arc::downgrade(self);
        let subscription = self.catalog.on_did_change(Arc::new(move |_| {
            let weak = weak.clone();
            tokio::spawn(async move {
                if let Some(this) = weak.upgrade() {
                    let _ = this.refresh_bound_subagent_allowlist().await;
                }
            });
        }));
        self.subscriptions.lock().push(Arc::from(subscription));

        let weak = Arc::downgrade(self);
        let subscription = self.config.on_did_section_change(Box::new(move |event| {
            if event.domain == TOOLS_SECTION
                && let Some(this) = weak.upgrade()
            {
                this.publish_tool_pattern_warnings(None);
            }
        }));
        self.subscriptions.lock().push(Arc::from(subscription));

        let weak = Arc::downgrade(self);
        let dispose = self.discovery.on_did_retire_model(Arc::new(move |event| {
            let Some(this) = weak.upgrade() else {
                return;
            };
            if this.model_alias().as_deref() != Some(event.from.as_str()) {
                return;
            }
            let weak = Arc::downgrade(&this);
            tokio::spawn(async move {
                if let Some(this) = weak.upgrade() {
                    let _ = this.set_model_inner(event.to).await;
                }
            });
        }));
        self.subscriptions.lock().push(to_disposable(dispose));
    }

    fn config_value<T: DeserializeOwned>(&self, domain: &str) -> Option<T> {
        self.config.get(domain).map(|value| {
            serde_json::from_value(value).unwrap_or_else(|error| {
                panic!("invalid configuration section {domain}: {error}")
            })
        })
    }

    fn profile_state(&self) -> ProfileModelState {
        serde_json::from_value(
            self.states
                .get(&profile_replayable_key().state_key()),
        )
        .unwrap_or_else(|error| panic!("{error}"))
    }

    fn model_alias(&self) -> Option<String> {
        self.profile_state().model_alias
    }

    fn profile_name(&self) -> Option<String> {
        self.profile_state().profile_name
    }

    fn system_prompt(&self) -> String {
        self.profile_state().system_prompt
    }

    fn active_tool_names_overlay(&self) -> Option<Vec<String>> {
        serde_json::from_value(
            self.states
                .get(&profile_active_tool_names_overlay_key()),
        )
        .unwrap_or_else(|error| panic!("{error}"))
    }

    fn set_active_tool_names_overlay(&self, value: Option<Vec<String>>) {
        self.states.set(
            &profile_active_tool_names_overlay_key(),
            serde_json::to_value(value).unwrap_or_else(|error| panic!("{error}")),
        );
    }

    fn active_tool_names(&self) -> ActiveToolsState {
        self.active_tool_names_overlay().or_else(|| {
            serde_json::from_value(
                self.states
                    .get(&profile_active_tools_replayable_key().state_key()),
            )
            .unwrap_or_else(|error| panic!("{error}"))
        })
    }

    fn agents_md_warning(&self) -> Option<String> {
        serde_json::from_value(self.states.get(&profile_agents_md_warning_key()))
            .unwrap_or_else(|error| panic!("{error}"))
    }

    fn set_agents_md_warning(&self, value: Option<String>) {
        self.states.set(
            &profile_agents_md_warning_key(),
            serde_json::to_value(value).unwrap_or_else(|error| panic!("{error}")),
        );
    }

    fn configure(&self, options: ProfileServiceOptions) {
        if let Some(callback) = options.emit_status_updated {
            self.options.lock().emit_status_updated = Some(Arc::from(callback));
        }
    }

    fn update(&self, changed: ProfileUpdateData) {
        if let Some(profile_name) = changed.profile_name.as_deref() {
            let mut active = self.active_profile.lock();
            if active
                .as_ref()
                .is_some_and(|profile| profile.name != profile_name)
            {
                *active = None;
            }
        }
        let has_config_changes = changed.model_alias.is_some()
            || changed.profile_name.is_some()
            || changed.thinking_level.is_some()
            || changed.system_prompt.is_some()
            || changed.environment_disclosure.is_some()
            || changed.agents_md_paths.is_some()
            || changed.disallowed_tools.is_some();
        if has_config_changes {
            let payload = self.resolve_config_payload(&changed);
            self.dispatch_detached(ConfigUpdate::new(payload).into());
            self.after_config_dispatch(&changed);
        }
        if let Some(names) = changed.active_tool_names {
            self.set_active_tools(Some(names));
        }
    }

    fn apply_binding_snapshot(&self, snapshot: ProfileBindingSnapshot) {
        *self.active_profile.lock() = None;
        self.set_active_tool_names_overlay(None);
        let agents_md_paths = snapshot.agents_md_paths.clone().unwrap_or_else(|| {
            extract_agents_md_paths_from_system_prompt(&snapshot.system_prompt)
        });
        self.dispatch_detached(
            ProfileBind::new(ProfileBindPayload {
                agent_id: self.agent_id(),
                model_alias: snapshot.model_alias.clone(),
                profile_name: snapshot.profile_name.clone(),
                thinking_effort: snapshot.thinking_level.clone(),
                system_prompt: snapshot.system_prompt.clone(),
                environment_disclosure: snapshot.environment_disclosure.clone(),
                render_generation: snapshot.render_generation,
                agents_md_paths: Some(agents_md_paths.clone()),
                active_tool_names: snapshot.active_tool_names.clone(),
                disallowed_tools: snapshot.disallowed_tools.clone().unwrap_or_default(),
                subagents: snapshot.subagents.clone(),
            })
            .into(),
        );
        self.after_config_dispatch(&ProfileUpdateData {
            model_alias: snapshot.model_alias,
            profile_name: snapshot.profile_name,
            thinking_level: Some(snapshot.thinking_level),
            system_prompt: Some(snapshot.system_prompt),
            environment_disclosure: snapshot.environment_disclosure,
            agents_md_paths: Some(agents_md_paths.clone()),
            disallowed_tools: Some(snapshot.disallowed_tools.unwrap_or_default()),
            active_tool_names: None,
        });
        self.agents_md_reminder.seed_injected(
            &agents_md_paths,
            Path::new(&self.workspace.work_dir()),
        );
    }

    fn bind_subagents(
        &self,
        profile_name: Option<String>,
        subagents: Option<Vec<String>>,
    ) -> Option<Vec<String>> {
        if self.agent_id() != MAIN_AGENT_ID {
            return subagents;
        }
        let caller = DelegationCaller {
            profile_name,
            subagents: subagents.clone(),
        };
        let profiles = self.catalog.list();
        let extras = root_delegation_extras(self.catalog.as_ref(), &caller, &profiles);
        let allowlist = subagent_allowlist_for(self.catalog.as_ref(), &caller, extras.as_deref());
        let Some(allowlist) = allowlist else {
            return subagents;
        };
        if subagents.is_some() {
            return Some(allowlist);
        }
        Some(without_delegating_targets(
            self.catalog.as_ref(),
            &allowlist,
        ))
    }

    async fn refresh_bound_subagent_allowlist(&self) -> Result<(), ProfileError> {
        if self.agent_id() != MAIN_AGENT_ID || self.profile_name().is_none() {
            return Ok(());
        }
        self.catalog.ready().await.map_err(into_profile_error)?;
        let snapshot = self.data();
        let subagents = self.bind_subagents(
            snapshot.base.profile_name.clone(),
            snapshot.subagents.clone(),
        );
        if subagents != snapshot.subagents {
            let mut snapshot = snapshot;
            snapshot.subagents = subagents;
            self.apply_binding_snapshot(profile_binding_snapshot(snapshot));
        }
        Ok(())
    }

    async fn bind_inner(&self, input: BindAgentInput) -> Result<(), ProfileError> {
        self.catalog.ready().await.map_err(into_profile_error)?;
        self.identity.resolved().await.map_err(into_profile_error)?;
        self.assert_bindable(&input.profile)?;
        let Some(profile) = self.catalog.get(&input.profile) else {
            let available = self
                .catalog
                .list()
                .iter()
                .map(|profile| profile.name.clone())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(ProfileError::new(
                ProfileErrors::PROFILE_UNKNOWN,
                format!(
                    "Unknown agent profile: \"{}\". Available profiles: {available}",
                    input.profile
                ),
                Some(json!({"profile": input.profile, "available": available})),
            ));
        };
        let profile = profile.as_ref().clone();
        let alias = input
            .model
            .clone()
            .or_else(|| self.config_value::<String>("defaultModel"));
        let Some(alias) = alias.filter(|alias| !alias.is_empty()) else {
            return Err(ProfileError::new(
                ProfileErrors::MODEL_NOT_CONFIGURED,
                format!(
                    "model is required to bind profile \"{}\" (no default model configured)",
                    input.profile
                ),
                None,
            ));
        };
        let model = self.model_catalog.get(&alias).map_err(into_profile_error)?;
        if input.strict_thinking == Some(true)
            && let Some(requested) = input.thinking.as_deref()
        {
            self.assert_thinking_effort_supported(requested, Some(&model), &alias)?;
        }
        self.session_tool_policy
            .ready()
            .await
            .map_err(into_profile_error)?;
        let context = self
            .build_system_prompt_context(
                &profile,
                PromptBuildOptions {
                    additional_dirs: None,
                    pending_model_alias: Some(alias.clone()),
                },
            )
            .await?;
        self.assert_bindable(&profile.name)?;
        let current_profile_name = self.profile_name();
        let rendered = render_agent_profile_prompt(&profile, &context.base);
        *self.active_profile.lock() = Some(profile.clone());
        self.cache_agents_md_warning(&context);
        let requested = input.thinking.clone().or_else(|| {
            current_profile_name.map(|_| self.thinking_level())
        });
        let thinking_level = self.resolve_thinking_effort(requested.as_deref(), Some(&model));
        self.set_active_tool_names_overlay(None);
        self.dispatcher
            .dispatch(
                ProfileBind::new(ProfileBindPayload {
                    agent_id: self.agent_id(),
                    model_alias: Some(alias.clone()),
                    profile_name: Some(profile.name.clone()),
                    thinking_effort: thinking_level.clone(),
                    system_prompt: rendered.text.clone(),
                    environment_disclosure: Some(rendered.environment.clone()),
                    render_generation: None,
                    agents_md_paths: Some(context.agents_md_paths.clone().unwrap_or_default()),
                    active_tool_names: profile.tools.clone(),
                    disallowed_tools: profile.disallowed_tools.clone().unwrap_or_default(),
                    subagents: self.bind_subagents(
                        Some(profile.name.clone()),
                        profile.subagents.clone(),
                    ),
                })
                .into(),
            )
            .await
            .map_err(into_profile_error)?;
        self.after_config_dispatch(&ProfileUpdateData {
            model_alias: Some(alias),
            profile_name: Some(profile.name.clone()),
            thinking_level: Some(thinking_level),
            system_prompt: Some(rendered.text.clone()),
            environment_disclosure: None,
            agents_md_paths: None,
            disallowed_tools: Some(profile.disallowed_tools.clone().unwrap_or_default()),
            active_tool_names: None,
        });
        self.seed_agents_md_reminder(&context, &rendered);
        self.seed_change_notifier_beliefs(&context).await?;
        self.publish_agents_md_warning();
        self.publish_tool_pattern_warnings(Some(&profile));
        Ok(())
    }

    async fn set_model_inner(&self, alias: String) -> Result<ProfileSetModelResult, ProfileError> {
        let model = self.model_catalog.get(&alias).map_err(into_profile_error)?;
        if self.profile_name().is_none() {
            self.bind_inner(BindAgentInput {
                profile: DEFAULT_AGENT_PROFILE_NAME.to_string(),
                model: Some(alias.clone()),
                thinking: None,
                strict_thinking: None,
            })
            .await?;
            self.track_model_switch(&alias);
        } else if self.model_alias().as_deref() != Some(alias.as_str()) {
            self.update(ProfileUpdateData {
                model_alias: Some(alias.clone()),
                ..Default::default()
            });
            self.track_model_switch(&alias);
        }
        Ok(ProfileSetModelResult {
            model: alias,
            provider_name: Some(model.provider_name),
        })
    }

    fn track_model_switch(&self, alias: &str) {
        let mut properties = Map::new();
        properties.insert("model".to_string(), alias.into());
        self.telemetry.track2("model_switch", Some(properties));
    }

    fn set_thinking(&self, level: &str) {
        let previous_effort = self.thinking_level();
        let model = self.try_resolve_raw_model();
        let model_alias = self.model_alias().unwrap_or_default();
        if let Err(error) = self.assert_thinking_effort_supported(
            level,
            model.as_ref(),
            &model_alias,
        ) {
            std::panic::panic_any(error);
        }
        let requested = normalize_requested_effort(level).unwrap_or_else(|| level.to_string());
        self.update(ProfileUpdateData {
            thinking_level: Some(requested.clone()),
            ..Default::default()
        });
        let effort = self.resolve_thinking_effort(Some(&requested), model.as_ref());
        if effort != previous_effort {
            let mut properties = Map::new();
            properties.insert("enabled".to_string(), (effort != "off").into());
            properties.insert("effort".to_string(), effort.clone().into());
            properties.insert("from".to_string(), previous_effort.into());
            self.telemetry.track2("thinking_toggle", Some(properties));
        }
    }

    fn assert_thinking_effort_supported(
        &self,
        requested: &str,
        model: Option<&Model>,
        model_alias: &str,
    ) -> Result<(), ProfileError> {
        let Some(normalized) = normalize_requested_effort(requested) else {
            return Ok(());
        };
        if self.supports_thinking_effort(&normalized, model) {
            return Ok(());
        }
        let efforts = model
            .and_then(|model| model.support_efforts.clone())
            .unwrap_or_default();
        let supported = if efforts.is_empty() {
            "off".to_string()
        } else {
            std::iter::once("off".to_string())
                .chain(efforts)
                .collect::<Vec<_>>()
                .join(", ")
        };
        Err(ProfileError::new(
            ProfileErrors::MODEL_CONFIG_INVALID,
            format!(
                "Thinking effort \"{requested}\" is not supported by model \"{model_alias}\". Supported efforts: {supported}."
            ),
            None,
        ))
    }

    fn use_profile_inner(
        &self,
        profile: &ResolvedAgentProfile,
        context: SystemPromptContext,
    ) -> SystemPromptRenderResult {
        *self.active_profile.lock() = Some(profile.clone());
        let rendered = render_agent_profile_prompt(profile, &context.base);
        self.update(ProfileUpdateData {
            profile_name: Some(profile.name.clone()),
            system_prompt: Some(rendered.text.clone()),
            environment_disclosure: Some(rendered.environment.clone()),
            agents_md_paths: Some(context.agents_md_paths.clone().unwrap_or_default()),
            disallowed_tools: Some(profile.disallowed_tools.clone().unwrap_or_default()),
            ..Default::default()
        });
        self.set_active_tools(profile.tools.clone());
        rendered
    }

    async fn apply_profile_inner(
        &self,
        profile: &ResolvedAgentProfile,
        options: Option<ApplyProfileOptions>,
    ) -> Result<(), ProfileError> {
        let context = self
            .build_system_prompt_context(
                profile,
                PromptBuildOptions {
                    additional_dirs: options.and_then(|options| options.additional_dirs),
                    pending_model_alias: None,
                },
            )
            .await?;
        let rendered = self.use_profile_inner(profile, context.clone());
        self.warn_about_dropped_vars(&rendered, &profile.name);
        self.seed_agents_md_reminder(&context, &rendered);
        self.seed_change_notifier_beliefs(&context).await?;
        self.cache_agents_md_warning(&context);
        self.publish_agents_md_warning();
        self.publish_tool_pattern_warnings(Some(profile));
        Ok(())
    }

    fn warn_about_dropped_vars(&self, rendered: &SystemPromptRenderResult, profile_name: &str) {
        let Some(dropped) = rendered.dropped_vars.as_ref().filter(|dropped| !dropped.is_empty())
        else {
            return;
        };
        self.log.warn(
            &format!(
                "system prompt template for profile \"{profile_name}\" ignored supplied variables: {}",
                dropped.join(", ")
            ),
            None,
        );
    }

    fn seed_agents_md_reminder(
        &self,
        context: &SystemPromptContext,
        rendered: &SystemPromptRenderResult,
    ) {
        if rendered
            .dropped_vars
            .as_ref()
            .is_some_and(|dropped| dropped.iter().any(|name| name == "agents_md"))
        {
            return;
        }
        let cwd = context
            .base
            .cwd
            .clone()
            .unwrap_or_else(|| self.session_context.cwd());
        self.agents_md_reminder.seed_injected(
            &context.agents_md_paths.clone().unwrap_or_default(),
            Path::new(&cwd),
        );
    }

    async fn seed_change_notifier_beliefs(
        &self,
        context: &SystemPromptContext,
    ) -> Result<(), ProfileError> {
        let seed = BeliefsSeed {
            agents_md: context.base.agents_md.clone().unwrap_or_default(),
            agents_md_paths: context.agents_md_paths.clone().unwrap_or_default(),
            skills_listing: context.base.skills.clone().unwrap_or_default(),
        };
        let future = self.instantiation.invoke_function(|accessor| {
            let service: Arc<dyn IAgentChangeNotifierService> =
                accessor.get(&create_decorator("agentChangeNotifierService"));
            service.seed_beliefs(seed)
        });
        future.await.map_err(into_profile_error)
    }

    fn data(&self) -> ProfileData {
        let model = self.try_resolve_raw_model();
        let state = self.profile_state();
        ProfileData {
            base: AgentConfigData {
                model_alias: state.model_alias.clone(),
                model_capabilities: model
                    .map(|model| model.capabilities)
                    .unwrap_or(UNKNOWN_CAPABILITY),
                profile_name: state.profile_name.clone(),
                thinking_level: self.thinking_level(),
                system_prompt: state.system_prompt.clone(),
            },
            agents_md_paths: state.agents_md_paths.clone(),
            active_tool_names: self.active_tool_names(),
            disallowed_tools: Some(state.disallowed_tools.clone().unwrap_or_default()),
            subagents: state.subagents.clone(),
            environment_disclosure: state.environment_disclosure.clone(),
            render_generation: Some(state.render_generation),
        }
    }

    fn resolve_model_context(&self) -> ProfileModelContext {
        let alias = self.required_model_alias();
        let model = self
            .model_catalog
            .get(&alias)
            .unwrap_or_else(|error| std::panic::panic_any(error));
        let loop_control = self.config_value::<LoopControl>("loopControl");
        ProfileModelContext {
            model_alias: alias,
            model_capabilities: model.capabilities.clone(),
            max_output_size: model.max_output_size,
            always_thinking: model.always_thinking.filter(|always| *always),
            thinking_level: self.resolve_thinking_state(Some(&model)).effective,
            reserved_context_size: loop_control
                .as_ref()
                .and_then(|control| control.reserved_context_size),
            compaction_trigger_ratio: loop_control
                .as_ref()
                .and_then(|control| control.compaction_trigger_ratio),
            compaction_max_attempts: loop_control
                .and_then(|control| control.compaction_max_attempts),
        }
    }

    fn resolve_request_params(&self) -> crate::llm_adapter::model::model_requester::ModelRequestParams {
        let model = self.try_resolve_raw_model();
        let thinking = self.resolve_thinking_state(model.as_ref());
        let thinking_config = self.config_value::<ThinkingConfig>(THINKING_SECTION);
        let overrides = self.config_value::<ModelOverrides>(MODEL_OVERRIDES_SECTION);
        let sampling = overrides.as_ref().and_then(|overrides| {
            (overrides.temperature.is_some() || overrides.top_p.is_some()).then(|| {
                crate::llm_adapter::model::model_requester::SamplingOptions {
                    temperature: overrides.temperature,
                    top_p: overrides.top_p,
                }
            })
        });
        crate::llm_adapter::model::model_requester::ModelRequestParams {
            cache_key: Some(self.session_context.session_id()),
            sampling,
            thinking_effort: Some(thinking.effective.clone()),
            thinking_keep: resolve_thinking_keep(
                overrides
                    .as_ref()
                    .and_then(|overrides| overrides.thinking_keep.as_deref()),
                thinking_config
                    .as_ref()
                    .and_then(|config| config.keep.as_deref()),
                &thinking.effective,
            ),
            ..Default::default()
        }
    }

    fn thinking_level(&self) -> ThinkingEffort {
        let stored = self.profile_state().thinking_level;
        if stored == "off" && self.always_thinking_model() {
            return self.resolve_thinking_effort(Some(&stored), self.try_resolve_raw_model().as_ref());
        }
        stored
    }

    fn resolve_thinking_state(&self, model: Option<&Model>) -> ThinkingState {
        let base = self.thinking_level();
        let forced = resolve_forced_thinking_effort(
            self.config_value::<ThinkingConfig>(THINKING_SECTION)
                .and_then(|config| config.forced_effort)
                .as_deref(),
            &base,
            drives_thinking_through_traits(model.and_then(|model| model.provider_type.as_deref())),
        );
        ThinkingState {
            effective: forced.clone().unwrap_or(base),
            forced,
        }
    }

    fn strict_thinking_validation(&self, model: Option<&Model>) -> bool {
        model.is_some_and(|model| {
            requires_strict_thinking_validation(
                self.protocol_adapters.as_ref(),
                model.protocol,
                model.provider_type.as_deref(),
            )
        })
    }

    fn resolve_thinking_effort(
        &self,
        requested: Option<&str>,
        model: Option<&Model>,
    ) -> ThinkingEffort {
        let config = self.config_value::<ThinkingConfig>(THINKING_SECTION);
        let defaults = config.map(|config| ThinkingDefaults {
            enabled: config.enabled,
            effort: config.effort,
        });
        resolve_thinking_effort_for_model(
            requested,
            defaults.as_ref(),
            model,
            self.strict_thinking_validation(model),
        )
    }

    fn supports_thinking_effort(&self, effort: &str, model: Option<&Model>) -> bool {
        match model {
            Some(model) => model_supports_thinking_effort(
                effort,
                model,
                self.strict_thinking_validation(Some(model)),
            ),
            None => true,
        }
    }

    fn always_thinking_model(&self) -> bool {
        self.try_resolve_raw_model()
            .and_then(|model| model.always_thinking)
            == Some(true)
    }

    fn try_resolve_raw_model(&self) -> Option<Model> {
        self.resolve_model_for_thinking(self.model_alias())
    }

    fn resolve_model_for_thinking(&self, alias: Option<String>) -> Option<Model> {
        let alias = alias?;
        self.model_catalog.get(&alias).ok()
    }

    fn required_model_alias(&self) -> String {
        self.model_alias().unwrap_or_else(|| {
            std::panic::panic_any(Error2::new(MODEL_NOT_CONFIGURED, "Model not set"))
        })
    }

    fn assert_bindable(&self, requested: &str) -> Result<(), ProfileError> {
        let current = self.profile_name();
        if let Some(current) = current.filter(|current| current != requested) {
            return Err(ProfileError::new(
                ProfileErrors::PROFILE_ALREADY_BOUND,
                format!(
                    "agent is already bound to profile \"{current}\"; cannot switch to \"{requested}\" in this session"
                ),
                Some(json!({"current": current, "requested": requested})),
            ));
        }
        Ok(())
    }

    fn resolve_config_payload(&self, changed: &ProfileUpdateData) -> ConfigUpdatePayload {
        let mut payload = ConfigUpdatePayload {
            agent_id: self.agent_id(),
            model_alias: changed.model_alias.clone(),
            profile_name: changed.profile_name.clone(),
            thinking_effort: None,
            thinking_level: None,
            system_prompt: None,
            environment_disclosure: None,
            render_generation: None,
            agents_md_paths: changed.agents_md_paths.clone(),
            disallowed_tools: changed.disallowed_tools.clone(),
        };
        if changed.thinking_level.is_some() || changed.model_alias.is_some() {
            let model = self.resolve_model_for_thinking(
                changed
                    .model_alias
                    .clone()
                    .or_else(|| self.model_alias()),
            );
            let requested = changed.thinking_level.clone().or_else(|| {
                self.model_alias().map(|_| self.thinking_level())
            });
            payload.thinking_effort = Some(
                self.resolve_thinking_effort(requested.as_deref(), model.as_ref()),
            );
        }
        if let Some(system_prompt) = changed.system_prompt.clone() {
            payload.system_prompt = Some(system_prompt);
            if changed.environment_disclosure.is_some() {
                payload.environment_disclosure = changed.environment_disclosure.clone();
            }
        }
        payload
    }

    fn after_config_dispatch(&self, changed: &ProfileUpdateData) {
        if let Some(alias) = changed.model_alias.clone() {
            self.sync_telemetry_model_context(Some(alias));
        }
        if changed.model_alias.is_some() || changed.thinking_level.is_some() {
            self.warn_about_anthropic_thinking_effort();
        }
        self.emit_status_updated(
            changed.model_alias.is_some() || changed.thinking_level.is_some(),
        );
    }

    fn sync_telemetry_model_context(&self, model_alias: Option<String>) {
        let Some(model_alias) = model_alias else {
            return;
        };
        let model = self.resolve_model_for_thinking(Some(model_alias.clone()));
        let mut context = Map::new();
        context.insert("model".to_string(), model_alias.into());
        if let Some(provider_type) = model.as_ref().and_then(|model| {
            model
                .provider_type
                .clone()
                .or_else(|| Some(model.protocol.as_str().to_string()))
        }) {
            context.insert("provider_type".to_string(), provider_type.into());
        }
        if let Some(protocol) = model.map(|model| model.protocol.as_str().to_string()) {
            context.insert("protocol".to_string(), protocol.into());
        }
        self.telemetry.set_context(context);
    }

    fn warn_about_anthropic_thinking_effort(&self) {
        let Some(model) = self.try_resolve_raw_model() else {
            return;
        };
        if model.protocol != Protocol::Anthropic {
            return;
        }
        let effort = self.resolve_thinking_state(Some(&model)).effective;
        if effort == "on" || effort == "off" {
            return;
        }
        let efforts = model
            .support_efforts
            .clone()
            .unwrap_or_default()
            .into_iter()
            .filter(|effort| !effort.is_empty())
            .collect::<Vec<_>>();
        if efforts.is_empty() || efforts.contains(&effort) {
            return;
        }
        let known_efforts = efforts.join(",");
        let code = "anthropic-thinking-effort-not-listed";
        let message = format!(
            "Thinking effort \"{effort}\" is not listed for model \"{}\" (known: {}). The configured value will be sent unchanged to the Anthropic-compatible backend.",
            model.name,
            efforts.join(", ")
        );
        let key = [code, &model.id, &model.name, &effort, &known_efforts].join("\0");
        if !self.insert_emitted_warning(
            &profile_emitted_thinking_effort_warnings_key(),
            key,
        ) {
            return;
        }
        self.dispatch_detached(
            WarningIssued::new(WarningIssuedPayload {
                agent_id: self.agent_id(),
                code: Some(code.to_string()),
                message,
            })
            .into(),
        );
    }

    fn insert_emitted_warning(&self, key: &StateKey<Value>, value: String) -> bool {
        let mut emitted: HashSet<String> =
            serde_json::from_value(self.states.get(key)).unwrap_or_else(|error| panic!("{error}"));
        let inserted = emitted.insert(value);
        if inserted {
            self.states.set(
                key,
                serde_json::to_value(emitted).unwrap_or_else(|error| panic!("{error}")),
            );
        }
        inserted
    }

    fn set_active_tools(&self, names: Option<Vec<String>>) {
        self.set_active_tool_names_overlay(None);
        match names {
            Some(names) => self.dispatch_detached(
                ToolsSetActiveTools::new(ToolsSetActiveToolsPayload {
                    agent_id: self.agent_id(),
                    names,
                })
                .into(),
            ),
            None => self.dispatch_detached(
                ToolsResetActiveTools::new(ToolsResetActiveToolsPayload {
                    agent_id: self.agent_id(),
                })
                .into(),
            ),
        }
    }

    fn emit_status_updated(&self, include_thinking_effort: bool) {
        let custom = self.options.lock().emit_status_updated.clone();
        if let Some(custom) = custom {
            custom();
            return;
        }
        let Some(model_alias) = self.model_alias() else {
            return;
        };
        let capabilities = self
            .try_resolve_raw_model()
            .map(|model| model.capabilities);
        let max_context_tokens = capabilities.map(|capabilities| {
            capabilities
                .max_input_tokens
                .unwrap_or(capabilities.max_context_tokens)
        });
        let mut payload = Map::new();
        payload.insert("agentId".to_string(), self.agent_id().into());
        payload.insert("model".to_string(), model_alias.into());
        if include_thinking_effort {
            payload.insert(
                "thinkingEffort".to_string(),
                self.resolve_thinking_state(self.try_resolve_raw_model().as_ref())
                    .effective
                    .into(),
            );
        }
        if let Some(max_context_tokens) = max_context_tokens.filter(|tokens| *tokens > 0) {
            payload.insert("maxContextTokens".to_string(), max_context_tokens.into());
        }
        self.dispatch_detached(Event2::new(
            &AGENT_STATUS_UPDATED_CLASS,
            Value::Object(payload),
            None,
        ));
    }

    async fn refresh_active_tools_inner(&self) -> Result<(), ProfileError> {
        if self.active_tool_names_overlay().is_some() {
            return Ok(());
        }
        let Some(name) = self.profile_name() else {
            return Ok(());
        };
        self.catalog.ready().await.map_err(into_profile_error)?;
        let Some(profile) = self.catalog.get(&name) else {
            return Ok(());
        };
        let Some(tools) = profile.tools.clone() else {
            return Ok(());
        };
        let current: ActiveToolsState = serde_json::from_value(
            self.states
                .get(&profile_active_tools_replayable_key().state_key()),
        )
        .unwrap_or_else(|error| panic!("{error}"));
        if current.as_deref() == Some(tools.as_slice()) {
            return Ok(());
        }
        self.set_active_tools(Some(tools));
        Ok(())
    }

    fn cache_agents_md_warning(&self, context: &SystemPromptContext) {
        self.set_agents_md_warning(context.agents_md_warning.clone());
    }

    fn publish_agents_md_warning(&self) {
        let Some(warning) = self.agents_md_warning() else {
            return;
        };
        self.dispatch_detached(
            WarningIssued::new(WarningIssuedPayload {
                agent_id: self.agent_id(),
                message: warning,
                code: Some("agents-md-oversized".to_string()),
            })
            .into(),
        );
    }

    fn publish_tool_pattern_warnings(&self, profile: Option<&ResolvedAgentProfile>) {
        let mut known = HashSet::new();
        for contribution in agent_tool_contributions() {
            known.insert(contribution.options.name);
        }
        let references = self
            .tool_registry
            .list_live()
            .unwrap_or_else(|| self.tool_registry.list());
        for reference in references {
            known.insert(reference.name);
        }
        for builtin in self.builtin_profiles.list() {
            let patterns = builtin
                .tools
                .unwrap_or_default()
                .into_iter()
                .chain(builtin.disallowed_tools.unwrap_or_default())
                .collect::<Vec<_>>();
            known.extend(literal_tool_names(&patterns));
        }
        let mut checks = Vec::new();
        if let Some(profile) = profile {
            checks.push((
                format!("profile \"{}\"", profile.name),
                "tools",
                profile.tools.clone(),
            ));
            checks.push((
                format!("profile \"{}\"", profile.name),
                "disallowedTools",
                profile.disallowed_tools.clone(),
            ));
        }
        let global = self.config_value::<GlobalToolsPolicy>(TOOLS_SECTION);
        checks.push((
            "the global [tools] config".to_string(),
            "enabled",
            global.as_ref().and_then(|global| global.enabled.clone()),
        ));
        checks.push((
            "the global [tools] config".to_string(),
            "disabled",
            global.and_then(|global| global.disabled),
        ));
        let is_known = |name: &str| known.contains(name);
        for (context, field, patterns) in checks {
            let Some(patterns) = patterns else {
                continue;
            };
            for issue in find_inactive_tool_patterns(&patterns, Some(&is_known)) {
                let key = format!("{context}|{field}|{}", issue.pattern);
                if !self.insert_emitted_warning(
                    &profile_emitted_tool_pattern_warnings_key(),
                    key,
                ) {
                    continue;
                }
                self.dispatch_detached(
                    WarningIssued::new(WarningIssuedPayload {
                        agent_id: self.agent_id(),
                        code: Some("tool-pattern-no-match".to_string()),
                        message: describe_inactive_tool_pattern(&context, field, &issue),
                    })
                    .into(),
                );
            }
        }
    }

    async fn build_system_prompt_context(
        &self,
        profile: &ResolvedAgentProfile,
        options: PromptBuildOptions,
    ) -> Result<SystemPromptContext, ProfileError> {
        self.notify.ready().await.map_err(into_profile_error)?;
        let preloaded_agents_md = self.workspace_instructions_snapshot().await?;
        let fs_available = self.environment.is_available(&["fs"]);
        let required: &[&str] = if fs_available { &["fs"] } else { &[] };
        let lease = self.environment.acquire(required).map_err(into_profile_error)?;
        let view = EnvironmentWorkspaceView::new(
            lease.environment.clone(),
            EnvironmentWorkspaceRoots {
                work_dir: self.workspace.work_dir(),
                additional_dirs: Some(
                    options
                        .additional_dirs
                        .unwrap_or_else(|| self.workspace.additional_dirs()),
                ),
                ..Default::default()
            },
        );
        let env = view.host.clone();
        let fs = lease.environment.fs();
        let brand_home = self.bootstrap.home_dir();
        let base_result = if fs_available {
            prepare_system_prompt_context(
                ProfileContextDeps {
                    fs: fs.as_ref(),
                    home_dir: &env.home_dir,
                },
                &view.work_dir,
                Some(&brand_home),
                Some(PrepareSystemPromptContextOptions {
                    additional_dirs: Some(view.additional_dirs.clone()),
                    preloaded_agents_md: Some(preloaded_agents_md),
                }),
            )
            .await
            .map_err(into_profile_error)
        } else {
            Ok(SystemPromptContext {
                base: AgentProfileContext::default(),
                agents_md_warning: None,
                agents_md_paths: None,
            })
        };
        lease.dispose();
        let mut context = base_result?;
        let skills = self.resolve_skill_listing().await;
        let plugin_sections = self.resolve_plugin_sections().await?;
        let model_name = self.wire_model_name_for(options.pending_model_alias);
        context.base.repo_memory = Some(self.git_memory.repo_block().await);
        context.base.memory_code = Some(self.associative_memory.code_block().await);
        context.base.model_name = model_name.clone();
        context.base.model_adaptation = self.resolve_model_adaptation(model_name).await;
        context.base.cwd = Some(view.work_dir.clone());
        context.base.os_kind = Some(env.os_kind.clone());
        context.base.shell_name = Some(env.shell_name.clone());
        context.base.shell_path = Some(env.shell_path.clone());
        context.base.skills = Some(skills);
        context.base.plugin_sections = Some(plugin_sections);
        context.base.skill_active = Some(self.is_tool_active_for_profile(profile, "Skill", ToolSource::Builtin));
        context.base.product_name = self
            .identity
            .resolved()
            .await
            .map_err(into_profile_error)?
            .display_name;
        context.base.reply_style_guide = self.bootstrap.args().reply_style_guide;
        context.base.notify_user_active = Some(
            self.notify.enabled()
                && self.is_tool_active_for_profile(
                    profile,
                    NOTIFY_USER_TOOL_NAME,
                    ToolSource::Builtin,
                ),
        );
        context.base.todo_list_active = Some(self.is_tool_active_for_profile(
            profile,
            TODO_LIST_TOOL_NAME,
            ToolSource::Builtin,
        ));
        context.base.plan_mode_available = Some(self.is_tool_active_for_profile(
            profile,
            EXIT_PLAN_MODE_TOOL_NAME,
            ToolSource::Builtin,
        ));
        Ok(context)
    }

    async fn resolve_model_adaptation(&self, wire: Option<String>) -> Option<String> {
        let wire = wire?;
        if let Some(Some(cached)) = self.model_adaptation_cache.lock().get(&wire).cloned() {
            return Some(cached);
        }
        let loaded = load_curated_adaptation(&wire, Some(self.log.clone())).await;
        self.model_adaptation_cache
            .lock()
            .insert(wire, loaded.clone());
        loaded
    }

    fn wire_model_name_for(&self, pending_model_alias: Option<String>) -> Option<String> {
        let alias = self.model_alias().or(pending_model_alias);
        let alias = alias.filter(|alias| !alias.is_empty())?;
        self.resolve_model_for_thinking(Some(alias))
            .map(|model| model.name)
    }

    async fn workspace_instructions_snapshot(&self) -> Result<LoadedAgentsMd, ProfileError> {
        self.instructions.ready().await.map_err(into_profile_error)?;
        let paths = self.instructions.agents_md_paths().unwrap_or_default();
        let content = self.instructions.agents_md().unwrap_or_default();
        Ok(LoadedAgentsMd {
            content: content.clone(),
            warning: self.instructions.agents_md_warning(),
            paths: paths.clone(),
            files: split_agents_md_files(&content, &paths),
        })
    }

    fn is_tool_active_for_profile(
        &self,
        profile: &ResolvedAgentProfile,
        name: &str,
        source: ToolSource,
    ) -> bool {
        is_tool_active_composed(
            &ToolPolicyLayers {
                workspace_disabled_tools: Some(self.tool_policy_gate.disabled_tools()),
                profile: ToolActivationPolicy {
                    tools: profile.tools.clone(),
                    disallowed_tools: profile.disallowed_tools.clone(),
                },
                global: self.config_value::<GlobalToolsPolicy>(TOOLS_SECTION),
                session_disabled_tools: Some(self.session_tool_policy.disabled_tools()),
            },
            name,
            source,
        )
    }

    async fn resolve_skill_listing(&self) -> String {
        if let Some(listing) = self.frozen_skill_listing.lock().clone() {
            return listing;
        }
        let Ok(()) = self.skill_catalog.ready().await else {
            return String::new();
        };
        let listing = self.skill_catalog.catalog().get_model_skill_listing();
        *self.frozen_skill_listing.lock() = Some(listing.clone());
        listing
    }

    async fn resolve_plugin_sections(&self) -> Result<String, ProfileError> {
        if let Some(sections) = self.frozen_plugin_sections.lock().clone() {
            return Ok(sections);
        }
        let sections = self
            .plugins
            .enabled_system_prompts()
            .await
            .map_err(into_profile_error)?;
        let mut parts = Vec::new();
        let mut skipped = Vec::new();
        let mut total_bytes = 0usize;
        for section in sections {
            let block = format!(
                "<!-- From: plugin {} -->\n{}",
                section.plugin_id, section.content
            );
            let bytes = block.len();
            if total_bytes + bytes > PLUGIN_SECTIONS_MAX_BYTES {
                skipped.push(section.plugin_id);
                continue;
            }
            total_bytes += bytes;
            parts.push(block);
        }
        let newly_skipped = skipped
            .into_iter()
            .filter(|id| {
                !serde_json::from_value::<HashSet<String>>(
                    self.states
                        .get(&profile_emitted_plugin_budget_warnings_key()),
                )
                .unwrap_or_else(|error| panic!("{error}"))
                .contains(id)
            })
            .collect::<Vec<_>>();
        if !newly_skipped.is_empty() {
            let mut emitted: HashSet<String> = serde_json::from_value(
                self.states
                    .get(&profile_emitted_plugin_budget_warnings_key()),
            )
            .unwrap_or_else(|error| panic!("{error}"));
            emitted.extend(newly_skipped.iter().cloned());
            self.states.set(
                &profile_emitted_plugin_budget_warnings_key(),
                serde_json::to_value(emitted).unwrap_or_else(|error| panic!("{error}")),
            );
            let message = format!(
                "Plugin system-prompt contributions from {} were skipped: the aggregate {} KB budget is exhausted.",
                newly_skipped
                    .iter()
                    .map(|id| format!("\"{id}\""))
                    .collect::<Vec<_>>()
                    .join(", "),
                PLUGIN_SECTIONS_MAX_BYTES / 1024
            );
            self.dispatch_detached(
                WarningIssued::new(WarningIssuedPayload {
                    agent_id: self.agent_id(),
                    message,
                    code: Some("plugin-sections-oversized".to_string()),
                })
                .into(),
            );
        }
        let resolved = parts.join("\n\n");
        if self.plugins.has_loaded_snapshot() {
            *self.frozen_plugin_sections.lock() = Some(resolved.clone());
        }
        Ok(resolved)
    }

    fn dispatch_detached(&self, event: Event2) {
        let dispatcher = self.dispatcher.clone();
        tokio::spawn(async move {
            let _ = dispatcher.dispatch(event).await;
        });
    }
}

fn normalize_requested_effort(requested: &str) -> Option<String> {
    crate::human::llm::thinking::normalize_requested_thinking_effort(Some(requested))
}

fn into_profile_error(error: WireAsyncError) -> ProfileError {
    match error.downcast::<ProfileError>() {
        Ok(error) => *error,
        Err(error) => ProfileError::new(
            ProfileErrors::MODEL_CONFIG_INVALID,
            error.to_string(),
            None,
        ),
    }
}

fn profile_binding_snapshot(data: ProfileData) -> ProfileBindingSnapshot {
    ProfileBindingSnapshot {
        model_alias: data.base.model_alias,
        profile_name: data.base.profile_name,
        thinking_level: data.base.thinking_level,
        system_prompt: data.base.system_prompt,
        environment_disclosure: data.environment_disclosure,
        render_generation: data.render_generation,
        agents_md_paths: data.agents_md_paths,
        active_tool_names: data.active_tool_names,
        disallowed_tools: data.disallowed_tools,
        subagents: data.subagents,
    }
}

impl super::profile::AgentProfileService for AgentProfileService {
    fn register(&self) {
        self.inner.register();
    }

    fn configure(&self, options: ProfileServiceOptions) {
        self.inner.configure(options);
    }

    fn update(&self, changed: ProfileUpdateData) {
        self.inner.update(changed);
    }

    fn apply_binding_snapshot(&self, snapshot: ProfileBindingSnapshot) {
        self.inner.apply_binding_snapshot(snapshot);
    }

    fn bind(&self, input: BindAgentInput) -> BoxFuture<'static, Result<(), ProfileError>> {
        let inner = self.inner.clone();
        Box::pin(async move { inner.bind_inner(input).await })
    }

    fn set_model(
        &self,
        model: &str,
    ) -> BoxFuture<'static, Result<ProfileSetModelResult, ProfileError>> {
        let inner = self.inner.clone();
        let model = model.to_string();
        Box::pin(async move { inner.set_model_inner(model).await })
    }

    fn set_thinking(&self, level: &str) {
        self.inner.set_thinking(level);
    }

    fn republish_status(&self) {
        self.inner.emit_status_updated(true);
    }

    fn get_model(&self) -> String {
        self.inner.model_alias().unwrap_or_default()
    }

    fn use_profile(&self, profile: &ResolvedAgentProfile, context: SystemPromptContext) {
        let _ = self.inner.use_profile_inner(profile, context);
    }

    fn apply_profile(
        &self,
        profile: &ResolvedAgentProfile,
        options: Option<ApplyProfileOptions>,
    ) -> BoxFuture<'static, Result<(), ProfileError>> {
        let inner = self.inner.clone();
        let profile = profile.clone();
        Box::pin(async move { inner.apply_profile_inner(&profile, options).await })
    }

    fn get_agents_md_warning(&self) -> Option<String> {
        self.inner.agents_md_warning()
    }

    fn data(&self) -> ProfileData {
        self.inner.data()
    }

    fn get_effective_thinking_level(&self) -> ThinkingEffort {
        self.inner
            .resolve_thinking_state(self.inner.try_resolve_raw_model().as_ref())
            .effective
    }

    fn resolve_model_context(&self) -> ProfileModelContext {
        self.inner.resolve_model_context()
    }

    fn resolve_request_params(&self) -> ModelRequestParams {
        self.inner.resolve_request_params()
    }

    fn get_model_capabilities(&self) -> ModelCapability {
        self.inner
            .try_resolve_raw_model()
            .map(|model| model.capabilities)
            .unwrap_or(UNKNOWN_CAPABILITY)
    }

    fn get_model_wire_name(&self) -> Option<String> {
        self.inner.try_resolve_raw_model().map(|model| model.name)
    }

    fn get_model_provider_type(&self, alias: Option<&str>) -> Option<String> {
        let effective = alias
            .map(ToString::to_string)
            .or_else(|| self.inner.model_alias())
            .or_else(|| self.inner.config_value::<String>("defaultModel"));
        self.inner
            .resolve_model_for_thinking(effective)
            .and_then(|model| model.provider_type)
    }

    fn get_max_output_size(&self) -> Option<u32> {
        self.inner
            .try_resolve_raw_model()
            .and_then(|model| model.max_output_size)
    }

    fn has_model(&self) -> bool {
        self.inner.model_alias().is_some()
    }

    fn is_runnable(&self) -> bool {
        self.inner.profile_name().is_some() && self.inner.model_alias().is_some()
    }

    fn has_provider(&self) -> bool {
        self.inner.try_resolve_raw_model().is_some()
    }

    fn get_system_prompt(&self) -> String {
        self.inner.system_prompt()
    }

    fn get_active_tool_names(&self) -> Option<Vec<String>> {
        self.inner.active_tool_names()
    }

    fn add_active_tool(&self, name: &str) {
        let Some(mut active) = self.inner.active_tool_names() else {
            return;
        };
        if active.iter().any(|candidate| candidate == name) {
            return;
        }
        active.push(name.to_string());
        self.inner.set_active_tool_names_overlay(Some(active));
    }

    fn remove_active_tool(&self, name: &str) {
        let Some(active) = self.inner.active_tool_names() else {
            return;
        };
        if !active.iter().any(|candidate| candidate == name) {
            return;
        }
        self.inner.set_active_tool_names_overlay(Some(
            active
                .into_iter()
                .filter(|candidate| candidate != name)
                .collect(),
        ));
    }

    fn refresh_active_tools(&self) -> BoxFuture<'static, Result<(), ProfileError>> {
        let inner = self.inner.clone();
        Box::pin(async move { inner.refresh_active_tools_inner().await })
    }
}
