use std::any::Any;
use std::collections::HashSet;
use std::error::Error;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};

use futures_util::FutureExt;
use kimi_core::primitives::errors::domains::agent_lifecycle::{
    AGENT_ALREADY_RUNNING, AGENT_MAX_TOKENS_EXCEEDED, AGENT_NOT_A_SUBAGENT, AGENT_NOT_FOUND,
    AGENT_NOT_OWNED, AGENT_NO_FINAL_MESSAGE,
};
use kimi_core::primitives::errors::domains::agent_loop::LOOP_MAX_STEPS_EXCEEDED;
use kimi_core::primitives::errors::domains::protocol::{
    PROVIDER_API_ERROR, PROVIDER_AUTH_ERROR, PROVIDER_CONNECTION_ERROR, PROVIDER_FILTERED,
    PROVIDER_OVERLOADED, PROVIDER_RATE_LIMIT,
};
use kimi_core::primitives::errors::domains::task::TASK_LIMIT_EXCEEDED;
use kimi_core::primitives::errors::{Error2, Error2Options};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};

use super::agent::{
    BACKGROUND_AGENT_UNAVAILABLE, DEFAULT_PROFILE_NAME, ISubagentTool, RESUMED_LABEL,
    RESUME_WITH_TYPE_UNAVAILABLE, SUBAGENT_STOPPED_MESSAGE, SubagentToolInput,
    USER_INTERRUPTED_SUBAGENT_MESSAGE,
};
use super::subagent_task::{
    NO_PARTIAL_OUTPUT, SharedSubagentError, SubagentCompletion, SubagentHandle, SubagentTask,
};
use crate::_base::di::instantiation::IInstantiationService;
use crate::_base::di::scope::IAgentScopeHandle;
use crate::_base::errors::errors::SharedError;
use crate::_base::log::log::ILogService;
use crate::_base::utils::abort::{
    AbortController, AbortSignal, DeadlineAbortSignal, abort_error, create_deadline_abort_signal,
    is_abort_error, is_user_cancellation, link_abort_signal, user_cancellation_reason,
};
use crate::_base::utils::error_message::error_message;
use crate::agent::permission_mode::permission_mode::IAgentPermissionModeService;
use crate::agent::profile::profile::IAgentProfileService;
use crate::agent::r#loop::r#loop::{IAgentLoopService, LoopState, Turn};
use crate::agent::scope_context::scope_context::agent_context_of;
use crate::agent::task::task::{
    ForegroundTaskReleaseReason, IAgentTaskService, RegisterAgentTaskOptions,
};
use crate::agent::task::types::{AgentTaskInfo, AgentTaskStatus};
use crate::agent::tool_activation::tool_availability::{AgentEnvironment, is_tool_available};
use crate::agent::tool_dedupe::tool_dedupe::REPEAT_BREAKER_STOP_REASON;
use crate::agent::tool_executor::tool_executor::{
    ExecutableToolContext, ExecutableToolResult, IAgentToolRegistryService, RunnableToolExecution,
    ToolExecution,
};
use crate::agent::tool_registry::tool_contribution::{
    AgentToolContribution, AgentToolContributionOptions,
};
use crate::agent::tool_policy::evaluate::{
    ToolActivationPolicy, ToolSource as PolicyToolSource, is_mcp_tool_name, is_tool_active,
    resolve_active_tool_names,
};
use crate::agent::tool_policy::tool_policy::IAgentToolPolicyService;
use crate::agent::tool_select::flag::TOOL_SELECT_FLAG_ID;
use crate::agent::tool_select::tool_select::SELECT_TOOLS_TOOL_NAME;
use crate::agent::tools::read_media_file::read_media_file::READ_MEDIA_FILE_TOOL_NAME;
use crate::app::agent_profile_catalog::agent_profile_catalog::AgentProfile;
use crate::app::agent_profile_catalog::profile_shared::{
    subagent_allowlist_for, without_delegating_targets,
};
use crate::app::config::config::IConfigService;
use crate::app::flag::flag::IFlagService;
use crate::features::tower::tower::has_pinned_permission_mode;
use crate::llm_adapter::contract::capability::ModelCapability;
use crate::llm_adapter::model::catalog::IModelCatalog;
use crate::session::agent_lifecycle::agent_lifecycle::IAgentLifecycleService;
use crate::session::agent_lifecycle::create_awaiting_close::create_agent_awaiting_close;
use crate::session::agent_lifecycle::subagent_metadata::{
    SubagentLabelOptions, is_subagent_meta, labels_from_agent_meta, subagent_labels,
    subagent_parent_agent_id, subagent_profile_name,
};
use crate::session::session_agent_profile_catalog::session_agent_profile_catalog::ISessionAgentProfileCatalog;
use crate::session::session_metadata::session_metadata::{AgentMeta, ISessionMetadata};
use crate::session::subagent::backend::backend_compat::{BackendCompatInputLike, backend_incompatibility};
use crate::session::subagent::backend::config_section::{
    BACKENDS_EXPERIMENTAL_UNAVAILABLE, build_subagent_backend_descriptions,
    exposes_subagent_backends, strip_subagent_backend_parameter,
};
use crate::session::subagent::backend::subagent_backend::{
    ISubagentBackendService, SubagentBackendName, SubagentBackendResult, SubagentBackendRun,
    SubagentBackendStartRequest, SubagentBackendStopReason,
};
use crate::session::subagent::config_section::{
    build_subagent_model_descriptions, exposes_subagent_model_choice,
    format_subagent_timeout_description, resolve_subagent_model_pool, resolve_subagent_timeout_ms,
    strip_subagent_fork_parameter, strip_subagent_model_parameter,
};
use crate::session::subagent::flag::SUBAGENT_FORK_FLAG_ID;
use crate::session::subagent::run_agent_turn::{classify_turn_result, no_final_message_error};
use crate::session::subagent::mirror_agent_run::{
    AgentRunSpawnedMeta, MirrorAgentRunOptions, SubagentStarted, SubagentStartedPayload,
    emit_agent_run_spawned, mirror_agent_run,
};
use crate::session::subagent::spawn::{
    FORK_EXPERIMENTAL_UNAVAILABLE, ForkCompatibilityArgs, SpawnCallerProfile, SpawnSubagentOptions,
    SubagentSpawnPlanInput, fork_incompatibility,
};
use crate::session::subagent::subagent::{
    AgentRunRequest, ISessionSubagentService, RunAgentOptions, SharedAgentRunError,
};
use crate::session::workspace_context::workspace_context::ISessionWorkspaceContext;
use crate::state::event_dispatcher::IEventDispatcher;
use crate::tool::input_schema::to_input_json_schema;
use crate::tool::rule_match::matches_glob_rule_subject;
use crate::tool::tool_contract::ToolSource;
use crate::tool::tool_input_display::ToolInputDisplay;
use crate::wire::record::WireAsyncError;

static SUBAGENT_TOOL_PARAMETERS: LazyLock<Map<String, Value>> = LazyLock::new(|| {
    to_input_json_schema(super::agent::subagent_tool_input_schema())
        .as_object()
        .expect("subagent input schema is an object")
        .clone()
});

static SUBAGENT_TOOL_PARAMETERS_NO_MODEL: LazyLock<Map<String, Value>> =
    LazyLock::new(|| strip_subagent_model_parameter(&SUBAGENT_TOOL_PARAMETERS));

const AGENT_DESCRIPTION_BASE: &str = r#"Launch a subagent to handle a task in its own context.

Do not launch a subagent unless the user explicitly asks for one, including delegation, parallel agents, or a named agent type. Explicit instructions in AGENTS.md or a skill, and user-enabled multi-agent modes, also authorize launching. Large or multi-part tasks and requests for depth, thoroughness, research, or investigation do not; do that work yourself. Agent type descriptions below guide selection only after authorization. Resuming already-authorized work needs no new request. Each subagent starts cold and re-derives context, costing time and tokens.

Writing the prompt:
- Brief it like a colleague who just walked into the room: state the goal, list what you already know, hand over the specifics.
- Lookups: put exact paths or commands you already know in the prompt.
- Investigations (figure out X, find why Y): give the question, not prescribed steps — fixed steps become dead weight when the premise is wrong.
- Do not delegate understanding. If the task hinges on a file path or line number, find it yourself first and write it into the prompt.

Usage notes:
- When the task continues earlier work a subagent already did, prefer resuming that agent (pass its `resume` id) over spawning a fresh instance — the resumed agent keeps its prior context.
- A subagent's result is only visible to you, not to the user. When the user needs to see what a subagent produced, summarize the relevant parts yourself in your own reply.

When NOT to use Agent: skip delegation for trivial work you can do directly — reading a file whose path you already know, searching a small known set of files, or any task that takes only a step or two. Delegation has a context-handoff cost; it pays off only when the task is substantial enough to outweigh it.

Once a subagent is running, leave that scope to it: do not redo its searches or reads in parallel, and do not abandon it midway and finish the job manually. Both undo the context savings the delegation was meant to buy.
"#;

const AGENT_BACKGROUND_DESCRIPTION: &str = "With `run_in_background=true`, the subagent runs detached and its result arrives automatically in a later turn as a synthetic user-role message — for an agent task it states that the subagent finished and points at the output file, so read that file to get the result. Continue with other work or respond to the user, and never fabricate or predict the result. Use it only when you have other work to do meanwhile. When your next step needs the result, run the subagent in the foreground instead of launching it in the background and waiting on it.\n";
const AGENT_BACKGROUND_DISABLED_DESCRIPTION: &str = "Background agent execution is disabled for this agent. Do not set `run_in_background=true` — any call that sets it is rejected before the subagent launches. Run every subagent in the foreground and wait for its result.";
const AGENT_FORK_DESCRIPTION: &str = "Context forking: pass `fork: true` to start with a snapshot of your completed conversation history, inheriting your current agent type, model, and tool set.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolReference {
    pub name: String,
    pub source: ToolSource,
}

pub struct SubagentToolDependencies {
    pub agent_lifecycle: Arc<dyn IAgentLifecycleService>,
    pub subagents: Arc<dyn ISessionSubagentService>,
    pub catalog: Arc<dyn ISessionAgentProfileCatalog>,
    pub caller_agent_id: String,
    pub instantiation_service: Arc<dyn IInstantiationService + Send + Sync>,
    pub environment: Arc<dyn AgentEnvironment + Send + Sync>,
    pub tasks: Arc<dyn IAgentTaskService>,
    pub profile: Arc<dyn IAgentProfileService>,
    pub model_catalog: Arc<dyn IModelCatalog>,
    pub tool_policy: Arc<dyn IAgentToolPolicyService>,
    pub tool_registry: Arc<dyn IAgentToolRegistryService>,
    pub permission_mode: Arc<dyn IAgentPermissionModeService>,
    pub session_metadata: Arc<dyn ISessionMetadata>,
    pub log: Arc<dyn ILogService>,
    pub config: Arc<dyn IConfigService>,
    pub flags: Arc<dyn IFlagService>,
    pub backends: Arc<dyn ISubagentBackendService>,
    pub workspace_ctx: Arc<dyn ISessionWorkspaceContext>,
    pub contributions: Vec<AgentToolContribution>,
}

pub struct SubagentTool {
    agent_lifecycle: Arc<dyn IAgentLifecycleService>,
    subagents: Arc<dyn ISessionSubagentService>,
    catalog: Arc<dyn ISessionAgentProfileCatalog>,
    caller_agent_id: String,
    instantiation_service: Arc<dyn IInstantiationService + Send + Sync>,
    environment: Arc<dyn AgentEnvironment + Send + Sync>,
    tasks: Arc<dyn IAgentTaskService>,
    profile: Arc<dyn IAgentProfileService>,
    model_catalog: Arc<dyn IModelCatalog>,
    tool_policy: Arc<dyn IAgentToolPolicyService>,
    tool_registry: Arc<dyn IAgentToolRegistryService>,
    permission_mode: Arc<dyn IAgentPermissionModeService>,
    session_metadata: Arc<dyn ISessionMetadata>,
    log: Arc<dyn ILogService>,
    config: Arc<dyn IConfigService>,
    flags: Arc<dyn IFlagService>,
    backends: Arc<dyn ISubagentBackendService>,
    workspace_ctx: Arc<dyn ISessionWorkspaceContext>,
    contributions: Vec<AgentToolContribution>,
    catalog_ready: AtomicBool,
    frozen_catalog_profiles: Mutex<Option<Vec<Arc<AgentProfile>>>>,
}

impl ISubagentTool for SubagentTool {
    fn invalidate_profile_descriptions(&self) {
        SubagentTool::invalidate_profile_descriptions(self);
    }
}

impl SubagentTool {
    pub fn new(dependencies: SubagentToolDependencies) -> Arc<Self> {
        let SubagentToolDependencies {
            agent_lifecycle,
            subagents,
            catalog,
            caller_agent_id,
            instantiation_service,
            environment,
            tasks,
            profile,
            model_catalog,
            tool_policy,
            tool_registry,
            permission_mode,
            session_metadata,
            log,
            config,
            flags,
            backends,
            workspace_ctx,
            contributions,
        } = dependencies;
        let this = Arc::new(Self {
            agent_lifecycle,
            subagents,
            catalog: catalog.clone(),
            caller_agent_id,
            instantiation_service,
            environment,
            tasks,
            profile,
            model_catalog,
            tool_policy,
            tool_registry,
            permission_mode,
            session_metadata,
            log,
            config,
            flags,
            backends,
            workspace_ctx,
            contributions,
            catalog_ready: AtomicBool::new(false),
            frozen_catalog_profiles: Mutex::new(None),
        });
        let weak = Arc::downgrade(&this);
        tokio::spawn(async move {
            if catalog.ready().await.is_ok()
                && let Some(this) = weak.upgrade()
            {
                this.catalog_ready.store(true, Ordering::SeqCst);
            }
        });
        this
    }

    pub fn name(&self) -> &'static str {
        "Agent"
    }

    pub fn side_effect(&self) -> &'static str {
        "read"
    }

    pub fn repo_mutation(&self) -> &'static str {
        "none"
    }

    pub fn parameters(&self) -> Map<String, Value> {
        let mut parameters = if exposes_subagent_model_choice(self.config.as_ref()) {
            SUBAGENT_TOOL_PARAMETERS.clone()
        } else {
            SUBAGENT_TOOL_PARAMETERS_NO_MODEL.clone()
        };
        if !self.flags.enabled(SUBAGENT_FORK_FLAG_ID) {
            parameters = strip_subagent_fork_parameter(&parameters);
        }
        if !exposes_subagent_backends(self.flags.as_ref()) {
            parameters = strip_subagent_backend_parameter(&parameters);
        }
        parameters
    }

    pub fn description(&self) -> String {
        let background_description = if self.can_run_in_background() {
            AGENT_BACKGROUND_DESCRIPTION
        } else {
            AGENT_BACKGROUND_DISABLED_DESCRIPTION
        };
        let mut description = format!("{AGENT_DESCRIPTION_BASE}\n\n{background_description}");
        if self.flags.enabled(SUBAGENT_FORK_FLAG_ID) {
            description.push_str(&format!("\n\n{AGENT_FORK_DESCRIPTION}"));
        }
        let own = self.profile.data();
        let catalog_profiles = self.catalog_profiles();
        let allowlist = self.effective_allowlist(&own);
        let profiles = catalog_profiles
            .into_iter()
            .filter(|profile| {
                allowlist
                    .as_ref()
                    .is_none_or(|allowlist| allowlist.contains(&profile.name))
            })
            .collect::<Vec<_>>();
        let known_tools = self.available_tool_references();
        let available = known_tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<HashSet<_>>();
        let profiles = profiles
            .into_iter()
            .map(|profile| {
                let mut profile = profile.as_ref().clone();
                profile.tools = profile.tools.map(|tools| {
                    tools
                        .into_iter()
                        .filter(|name| is_tool_name_pattern(name) || available.contains(name.as_str()))
                        .collect()
                });
                profile
            })
            .collect::<Vec<_>>();
        let type_lines = build_profile_descriptions(
            &profiles,
            &known_tools,
            &|profile, name, source| {
                self.tool_policy.is_tool_active_for_profile(
                    &ToolActivationPolicy {
                        tools: profile.tools.clone(),
                        disallowed_tools: profile.disallowed_tools.clone(),
                    },
                    name,
                    Some(source),
                )
            },
        );
        if !type_lines.is_empty() {
            description.push_str(&format!(
                "\n\nAvailable agent types (pass via subagent_type):\n{type_lines}"
            ));
        }
        if let Some(model_lines) = build_subagent_model_descriptions(self.config.as_ref()) {
            description.push_str(&format!("\n\n{model_lines}"));
        }
        if exposes_subagent_backends(self.flags.as_ref()) {
            description.push_str(&format!(
                "\n\n{}",
                build_subagent_backend_descriptions(&self.backends.list())
            ));
        }
        description
    }

    fn can_run_in_background(&self) -> bool {
        ["TaskList", "TaskOutput", "TaskStop"]
            .into_iter()
            .all(|name| self.tool_policy.is_tool_active(name, None))
    }

    fn catalog_profiles(&self) -> Vec<Arc<AgentProfile>> {
        if let Some(profiles) = self.frozen_catalog_profiles.lock().clone() {
            return profiles;
        }
        let profiles = self.catalog.list();
        if self.catalog_ready.load(Ordering::SeqCst) {
            *self.frozen_catalog_profiles.lock() = Some(profiles.clone());
        }
        profiles
    }

    /// Clear the frozen catalog-profile descriptions cache so the next listing
    /// re-reads the freshly refreshed profile catalog (v2
    /// `invalidateProfileDescriptions`). The plugin refresh that calls this is a
    /// host/seam concern; the cache invalidation itself lives in the engine.
    pub fn invalidate_profile_descriptions(&self) {
        *self.frozen_catalog_profiles.lock() = None;
    }

    fn effective_allowlist(&self, own: &crate::agent::profile::profile::ProfileData) -> Option<Vec<String>> {
        let allowlist = subagent_allowlist_for(self.catalog.as_ref(), own, None);
        if allowlist.is_none() || own.subagents.is_some() {
            return allowlist;
        }
        allowlist.map(|allowlist| without_delegating_targets(self.catalog.as_ref(), &allowlist))
    }

    fn available_tool_references(&self) -> Vec<ToolReference> {
        let mut refs = self
            .tool_registry
            .list()
            .into_iter()
            .map(|tool| ToolReference {
                name: tool.name,
                source: tool.source,
            })
            .collect::<Vec<_>>();
        self.instantiation_service.invoke_function(|_| {
            for contribution in &self.contributions {
                let options = &contribution.options;
                if refs
                    .iter()
                    .find(|tool| tool.name == options.name)
                    .is_some_and(|tool| is_user_reference_source(tool.source))
                {
                    continue;
                }
                if is_tool_available(options, self.environment.as_ref()) {
                    upsert_reference(
                        &mut refs,
                        ToolReference {
                            name: options.name.clone(),
                            source: options.resolved_source(),
                        },
                    );
                } else {
                    refs.retain(|tool| tool.name != options.name);
                }
            }
        });
        if refs
            .iter()
            .find(|tool| tool.name == READ_MEDIA_FILE_TOOL_NAME)
            .is_none_or(|tool| !is_user_reference_source(tool.source))
        {
            if self.any_subagent_model_supports(&|capabilities| {
                capabilities.image_in || capabilities.video_in
            }) {
                upsert_reference(
                    &mut refs,
                    ToolReference {
                        name: READ_MEDIA_FILE_TOOL_NAME.to_string(),
                        source: ToolSource::Builtin,
                    },
                );
            } else {
                refs.retain(|tool| tool.name != READ_MEDIA_FILE_TOOL_NAME);
            }
        }
        if !self.flags.enabled(TOOL_SELECT_FLAG_ID)
            || !self.any_subagent_model_supports(&|capabilities| {
                capabilities.dynamically_loaded_tools == Some(true) && capabilities.tool_use
            })
        {
            refs.retain(|tool| tool.name != SELECT_TOOLS_TOOL_NAME);
        }
        refs
    }

    fn any_subagent_model_supports(
        &self,
        supports: &dyn Fn(&ModelCapability) -> bool,
    ) -> bool {
        let Some(pool) = resolve_subagent_model_pool(self.config.as_ref()) else {
            return supports(&self.profile.get_model_capabilities());
        };
        for alias in pool.models.keys() {
            if let Ok(model) = self.model_catalog.get(alias)
                && supports(&model.capabilities)
            {
                return true;
            }
        }
        false
    }

    pub async fn resolve_execution(
        self: &Arc<Self>,
        args: SubagentToolInput,
    ) -> Result<ToolExecution, WireAsyncError> {
        let requested_profile_name = args
            .subagent_type
            .as_ref()
            .filter(|name| !name.is_empty())
            .cloned();
        let resume_agent_id = args.resume.as_ref().map(|agent_id| agent_id.trim().to_string());
        let is_resume = resume_agent_id
            .as_ref()
            .is_some_and(|agent_id| !agent_id.is_empty());
        if is_resume && requested_profile_name.is_some() {
            return Ok(ToolExecution::Error(tool_error(
                RESUME_WITH_TYPE_UNAVAILABLE,
            )));
        }
        if args.fork == Some(true) {
            if !self.flags.enabled(SUBAGENT_FORK_FLAG_ID) {
                return Ok(ToolExecution::Error(tool_error(
                    FORK_EXPERIMENTAL_UNAVAILABLE,
                )));
            }
            if let Some(error) = self.fork_error(&args) {
                return Ok(ToolExecution::Error(tool_error(error)));
            }
        }
        let backend_check = self.check_backend_args(&args);
        if let Some(error) = backend_check.error {
            return Ok(ToolExecution::Error(tool_error(error)));
        }
        let profile_name_for_display = if let Some(name) = backend_check.name {
            name.as_str().to_string()
        } else if is_resume {
            self.resume_profile_name(resume_agent_id.as_deref().unwrap())
                .await?
                .unwrap_or_else(|| RESUMED_LABEL.to_string())
        } else if let Some(name) = requested_profile_name {
            name
        } else if args.fork == Some(true) {
            self.profile
                .data()
                .base
                .profile_name
                .unwrap_or_else(|| DEFAULT_PROFILE_NAME.to_string())
        } else {
            DEFAULT_PROFILE_NAME.to_string()
        };
        let prefix = if args.run_in_background == Some(true) {
            "Launching background"
        } else {
            "Launching"
        };
        let display_name = profile_name_for_display.clone();
        let tool = self.clone();
        let execute_args = args.clone();
        Ok(ToolExecution::Runnable(RunnableToolExecution {
            accesses: Some(Vec::new()),
            display: Some(ToolInputDisplay::AgentCall {
                agent_name: profile_name_for_display.clone(),
                prompt: args.prompt.clone(),
                background: args.run_in_background,
            }),
            description: Some(format!(
                "{prefix} {profile_name_for_display} agent: {}",
                args.description
            )),
            stop_batch_after_this: None,
            approval_rule: self.name().to_string(),
            matches_rule: Some(Arc::new(move |rule_args| {
                matches_glob_rule_subject(rule_args, &display_name)
            })),
            execute: Arc::new(move |context| {
                let tool = tool.clone();
                let args = execute_args.clone();
                Box::pin(async move { Ok(tool.execution(args, context).await) })
            }),
        }))
    }

    fn fork_error(&self, args: &SubagentToolInput) -> Option<&'static str> {
        let own = self.profile.data();
        fork_incompatibility(
            &ForkCompatibilityArgs {
                resume: args.resume.clone(),
                subagent_type: args.subagent_type.clone(),
                model: args.model.clone(),
            },
            &SpawnCallerProfile {
                profile_name: own.base.profile_name,
                model_alias: own.base.model_alias,
            },
        )
    }

    fn check_backend_args(&self, args: &SubagentToolInput) -> BackendCheck {
        let requested = args.backend.as_ref().map(|name| name.trim().to_string());
        if requested.as_ref().is_none_or(String::is_empty) {
            return BackendCheck::default();
        }
        let requested = requested.unwrap();
        if !exposes_subagent_backends(self.flags.as_ref()) {
            return BackendCheck {
                name: None,
                error: Some(BACKENDS_EXPERIMENTAL_UNAVAILABLE.to_string()),
            };
        }
        if let Some(error) = backend_incompatibility(&BackendCompatInputLike {
            backend: args.backend.clone(),
            fork: args.fork,
            resume: args.resume.clone(),
            subagent_type: args.subagent_type.clone(),
            model: args.model.clone(),
            run_in_background: args.run_in_background,
        }) {
            return BackendCheck {
                name: None,
                error: Some(error.to_string()),
            };
        }
        let backend = SubagentBackendName::parse(&requested)
            .and_then(|name| self.backends.get_backend(name));
        let Some(backend) = backend else {
            let available = self
                .backends
                .list()
                .iter()
                .map(|entry| entry.name().as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return BackendCheck {
                name: None,
                error: Some(format!(
                    "Unknown backend \"{requested}\". Available backends: {available}."
                )),
            };
        };
        BackendCheck {
            name: Some(backend.name()),
            error: None,
        }
    }

    async fn resume_profile_name(&self, agent_id: &str) -> Result<Option<String>, WireAsyncError> {
        if let Some(target) = self.agent_lifecycle.handle_of(agent_id) {
            return Ok(target.accessor().get::<IAgentProfileService>().data().base.profile_name);
        }
        let mut agents = self.session_metadata.read().await?.agents.unwrap_or_default();
        Ok(subagent_profile_name(agents.remove(agent_id).as_ref()))
    }

    async fn launch(
        &self,
        args: &SubagentToolInput,
        tool_call_id: &str,
        controller: Arc<AbortController>,
    ) -> Result<SubagentHandle, WireAsyncError> {
        let requester = self.agent_lifecycle.handle_of(&self.caller_agent_id);
        let Some(requester) = requester else {
            return Err(coded_error(
                AGENT_NOT_FOUND,
                format!("Caller agent \"{}\" does not exist", self.caller_agent_id),
                [("agentId", json!(self.caller_agent_id))],
            ));
        };
        let resume_agent_id = args.resume.as_ref().map(|agent_id| agent_id.trim().to_string());
        let is_resume = resume_agent_id
            .as_ref()
            .is_some_and(|agent_id| !agent_id.is_empty());
        let agent_id;
        let profile_name;
        let display_model;
        let mut display_model_source = None;
        let mut prompt_text = args.prompt.clone();
        if is_resume {
            let target = self
                .resolve_resume_target(resume_agent_id.as_deref().unwrap(), &controller.signal())
                .await?;
            agent_id = target.id().to_string();
            let resumed = target.accessor().get::<IAgentProfileService>().data();
            profile_name = resumed
                .base
                .profile_name
                .unwrap_or_else(|| RESUMED_LABEL.to_string());
            display_model = resumed.base.model_alias;
        } else {
            let plan = self
                .subagents
                .plan_spawn(SubagentSpawnPlanInput {
                    caller_agent_id: self.caller_agent_id.clone(),
                    profile_name: args.subagent_type.clone(),
                    model: args.model.clone(),
                    fork: Some(args.fork == Some(true)),
                })
                .await?;
            let spawned = self
                .subagents
                .spawn(SpawnSubagentOptions {
                    caller_agent_id: self.caller_agent_id.clone(),
                    plan,
                    labels: Some(subagent_labels(
                        &self.caller_agent_id,
                        SubagentLabelOptions::default(),
                    )),
                    prompt: args.prompt.clone(),
                })
                .await?;
            agent_id = spawned.agent_id;
            profile_name = spawned.profile_name;
            display_model = Some(spawned.model);
            display_model_source = spawned.model_source;
            prompt_text = spawned.prompt_text;
        }
        let Some(target) = self.agent_lifecycle.handle_of(&agent_id) else {
            return Err(format!("Agent \"{agent_id}\" does not exist").into());
        };
        let run = self
            .subagents
            .run(
                agent_context_of(&target),
                AgentRunRequest::Prompt {
                    prompt: prompt_text.clone(),
                    helper: None,
                },
                RunAgentOptions {
                    signal: controller.signal(),
                    on_ready: None,
                },
            )
            .await?;
        let partial_output = run.partial_output.clone();
        let turn = run.turn.clone();
        let mirror_controller = controller.clone();
        let mirrored = mirror_agent_run(
            &requester,
            run,
            MirrorAgentRunOptions {
                profile_name: profile_name.clone(),
                prompt: Some(prompt_text),
                suppress_rate_limit_failure_event: None,
                signal: controller.signal(),
                cancel: Some(Arc::new(move |reason| {
                    mirror_controller.abort(reason.map(|error| Arc::from(error) as SharedError));
                })),
                defer_started: Some(true),
                terminalize: None,
            },
        );
        let completion_partial_output = partial_output.clone();
        let completion_signal = controller.signal();
        let thinking_effort = self
            .agent_lifecycle
            .handle_of(&agent_id)
            .map(|handle| {
                handle
                    .accessor()
                    .get::<IAgentProfileService>()
                    .get_effective_thinking_level()
            });
        let completion = async move {
            mirrored
                .await
                .map(|result| SubagentCompletion {
                    result: result.summary,
                    usage: result.usage,
                    stop_reason: result.stop_reason,
                })
                .map_err(|error| {
                    recover_mirror_error(
                        error,
                        &turn,
                        &completion_partial_output,
                        &completion_signal,
                    )
                })
        }
        .boxed()
        .shared();
        Ok(SubagentHandle {
            agent_id,
            profile_name,
            parent_tool_call_id: Some(tool_call_id.to_string()),
            model: display_model,
            model_source: display_model_source,
            thinking_effort,
            completion,
            partial_output: Some(partial_output),
        })
    }

    async fn resolve_resume_target(
        &self,
        agent_id: &str,
        signal: &AbortSignal,
    ) -> Result<Arc<IAgentScopeHandle>, WireAsyncError> {
        let meta = self
            .session_metadata
            .read()
            .await?
            .agents
            .unwrap_or_default()
            .remove(agent_id);
        let live = self.agent_lifecycle.handle_of(agent_id);
        if meta.is_none() && live.is_none() {
            return Err(coded_error(
                AGENT_NOT_FOUND,
                format!("Agent instance \"{agent_id}\" does not exist"),
                [("agentId", json!(agent_id))],
            ));
        }
        if meta.is_none() || !is_subagent_meta(meta.as_ref()) {
            return Err(coded_error(
                AGENT_NOT_A_SUBAGENT,
                format!("Agent instance \"{agent_id}\" is not a subagent"),
                [("agentId", json!(agent_id))],
            ));
        }
        if subagent_parent_agent_id(meta.as_ref()).as_deref() != Some(self.caller_agent_id.as_str()) {
            return Err(coded_error(
                AGENT_NOT_OWNED,
                format!("Agent instance \"{agent_id}\" does not belong to this parent agent"),
                [
                    ("agentId", json!(agent_id)),
                    ("callerAgentId", json!(self.caller_agent_id)),
                ],
            ));
        }
        let target = match live {
            Some(target) => target,
            None => {
                self.rebuild_subagent(agent_id, &meta.unwrap(), signal)
                    .await?
            }
        };
        if target
            .accessor()
            .get::<IAgentLoopService>()
            .snapshot()
            .state
            == LoopState::Running
        {
            return Err(coded_error(
                AGENT_ALREADY_RUNNING,
                format!("Agent instance \"{agent_id}\" is already running and cannot run concurrently"),
                [("agentId", json!(agent_id))],
            ));
        }
        Ok(target)
    }

    async fn rebuild_subagent(
        &self,
        agent_id: &str,
        meta: &AgentMeta,
        signal: &AbortSignal,
    ) -> Result<Arc<IAgentScopeHandle>, WireAsyncError> {
        create_agent_awaiting_close(
            &self.agent_lifecycle,
            crate::session::agent_lifecycle::agent_lifecycle::CreateAgentOptions {
                agent_id: Some(agent_id.to_string()),
                labels: labels_from_agent_meta(meta),
                forked_from: meta.forked_from.clone(),
                ..Default::default()
            },
            Some(signal),
        )
        .await?;
        let Some(rebuilt) = self.agent_lifecycle.handle_of(agent_id) else {
            return Err(coded_error(
                AGENT_NOT_FOUND,
                format!("Agent instance \"{agent_id}\" does not exist"),
                [("agentId", json!(agent_id))],
            ));
        };
        let profile_name = rebuilt
            .accessor()
            .get::<IAgentProfileService>()
            .data()
            .base
            .profile_name;
        if !has_pinned_permission_mode(profile_name.as_deref()) {
            rebuilt
                .accessor()
                .get::<IAgentPermissionModeService>()
                .set_mode(self.permission_mode.mode());
        }
        self.log.info(
            "subagent rebuilt for resume",
            Some(json!({"agentId": agent_id, "callerAgentId": self.caller_agent_id})),
        );
        Ok(rebuilt)
    }

    async fn execution_backend(
        &self,
        args: &SubagentToolInput,
        backend_name: SubagentBackendName,
        signal: &AbortSignal,
    ) -> Result<ExecutableToolResult, WireAsyncError> {
        let timeout_ms = resolve_subagent_timeout_ms(self.config.as_ref());
        let deadline = Some(create_deadline_abort_signal(signal, timeout_ms as i64));
        let effective_signal = deadline
            .as_ref()
            .map(|deadline| deadline.signal.clone())
            .unwrap_or_else(|| signal.clone());
        let result = async {
            let run = self
                .backends
                .start(
                    backend_name,
                    SubagentBackendStartRequest {
                        prompt: args.prompt.clone(),
                        cwd: self.workspace_ctx.work_dir(),
                        signal: effective_signal.clone(),
                    },
                )
                .await?;
            let SubagentBackendRun { id, result, dispose } = run;
            let outcome = async {
                let result = result.await?;
                if result.stop_reason == SubagentBackendStopReason::Completed {
                    return Ok(tool_result(
                        format_backend_agent_success(backend_name.as_str(), &id, &result.output),
                        false,
                    ));
                }
                let message = backend_stop_message(
                    result.stop_reason,
                    signal,
                    deadline.as_ref(),
                    timeout_ms,
                );
                Ok(tool_result(
                    format_backend_agent_failure(backend_name.as_str(), &id, &result, &message),
                    true,
                ))
            }
            .await;
            let disposed = dispose().await;
            match disposed {
                Err(error) => Err(error),
                Ok(()) => outcome,
            }
        }
        .await;
        if let Some(deadline) = deadline {
            deadline.clear();
        }
        result
    }

    async fn execution(
        self: &Arc<Self>,
        args: SubagentToolInput,
        context: ExecutableToolContext,
    ) -> ExecutableToolResult {
        let signal = context.signal.clone();
        let result = async {
            throw_if_aborted(&signal)?;
            let run_in_background = args.run_in_background == Some(true);
            let requested_profile_name = args
                .subagent_type
                .as_ref()
                .filter(|name| !name.is_empty())
                .cloned();
            let resume_agent_id = args.resume.as_ref().map(|agent_id| agent_id.trim().to_string());
            let is_resume = resume_agent_id
                .as_ref()
                .is_some_and(|agent_id| !agent_id.is_empty());
            if is_resume && requested_profile_name.is_some() {
                return Ok(tool_error(RESUME_WITH_TYPE_UNAVAILABLE));
            }
            if args.fork == Some(true) {
                if !self.flags.enabled(SUBAGENT_FORK_FLAG_ID) {
                    return Ok(tool_error(FORK_EXPERIMENTAL_UNAVAILABLE));
                }
                if let Some(error) = self.fork_error(&args) {
                    return Ok(tool_error(error));
                }
            }
            let backend_check = self.check_backend_args(&args);
            if let Some(error) = backend_check.error {
                return Ok(tool_error(error));
            }
            if let Some(name) = backend_check.name {
                return self.execution_backend(&args, name, &signal).await;
            }
            let allow_background = self.can_run_in_background();
            if run_in_background && !allow_background {
                return Ok(tool_error(BACKGROUND_AGENT_UNAVAILABLE));
            }
            let timeout_ms = resolve_subagent_timeout_ms(self.config.as_ref());
            let controller = Arc::new(AbortController::new());
            let abort_link = (!run_in_background)
                .then(|| link_abort_signal(&signal, controller.clone()));
            let handle = match self.launch(&args, &context.tool_call_id, controller.clone()).await {
                Ok(handle) => handle,
                Err(error) => {
                    remove_abort_link(abort_link);
                    self.log.warn(
                        "subagent launch failed",
                        Some(json!({
                            "toolCallId": context.tool_call_id,
                            "runInBackground": run_in_background,
                            "operation": if is_resume { "resume" } else { "spawn" },
                            "subagentType": requested_profile_name.unwrap_or_else(|| DEFAULT_PROFILE_NAME.to_string()),
                            "resumeAgentId": if is_resume { resume_agent_id } else { None },
                            "error": error.to_string(),
                        })),
                    );
                    return Err(error);
                }
            };
            let task = SubagentTask::new(handle.clone(), args.description.clone(), controller.clone());
            let task = if run_in_background {
                task
            } else {
                task.with_parent_signal(signal.clone())
            };
            let task_id = match register_agent_task(
                self.tasks.as_ref(),
                Arc::new(task),
                RegisterAgentTaskOptions {
                    detached: Some(run_in_background),
                    detachable: None,
                    timeout_ms: Some(timeout_ms),
                    detach_timeout_ms: None,
                    auto_background_on_timeout: None,
                    signal: (!run_in_background).then(|| signal.clone()),
                },
            ) {
                Ok(task_id) => task_id,
                Err(error) => {
                    controller.abort(Some(Arc::new(abort_error())));
                    let completion = handle.completion.clone();
                    tokio::spawn(async move {
                        let _ = completion.await;
                    });
                    remove_abort_link(abort_link);
                    self.log.warn(
                        "background agent task registration failed",
                        Some(json!({
                            "toolCallId": context.tool_call_id,
                            "agentId": handle.agent_id,
                            "subagentType": handle.profile_name,
                            "error": error.to_string(),
                        })),
                    );
                    let message = if error
                        .downcast_ref::<Error2>()
                        .is_some_and(|error| error.code().as_str() == TASK_LIMIT_EXCEEDED)
                    {
                        "Too many background tasks are already running.".to_string()
                    } else {
                        error.to_string()
                    };
                    return Ok(tool_error(message));
                }
            };
            remove_abort_link(abort_link);
            if let Some(requester) = self.agent_lifecycle.handle_of(&self.caller_agent_id) {
                emit_agent_run_spawned(
                    &requester,
                    &handle.agent_id,
                    AgentRunSpawnedMeta {
                        profile_name: handle.profile_name.clone(),
                        parent_tool_call_id: Some(context.tool_call_id.clone()),
                        parent_tool_call_uuid: None,
                        description: Some(args.description.clone()),
                        swarm_index: None,
                        run_in_background: Some(run_in_background),
                        fork: Some(args.fork == Some(true)),
                        model: handle.model.clone(),
                        model_source: handle.model_source.clone(),
                        task_id: Some(task_id.clone()),
                    },
                );
                let dispatcher = requester.accessor().get::<IEventDispatcher>();
                let agent_id = handle.agent_id.clone();
                tokio::spawn(async move {
                    let _ = dispatcher
                        .dispatch(
                            SubagentStarted::new(SubagentStartedPayload {
                                subagent_id: agent_id,
                            })
                            .into(),
                        )
                        .await;
                });
            }
            if run_in_background {
                return Ok(tool_result(
                    format_background_agent_result(
                        &task_id,
                        &handle,
                        &args.description,
                        allow_background,
                        false,
                    ),
                    false,
                ));
            }
            let release = self.tasks.wait_for_foreground_release(&task_id).await?;
            let result = if release == Some(ForegroundTaskReleaseReason::Detached) {
                tool_result(
                    format_background_agent_result(
                        &task_id,
                        &handle,
                        &args.description,
                        allow_background,
                        true,
                    ),
                    false,
                )
            } else {
                self.format_foreground_result(&task_id, &handle, timeout_ms).await?
            };
            Ok(result)
        }
        .await;
        match result {
            Ok(result) => result,
            Err(error) => tool_error(format!(
                "subagent error: {}",
                launch_error_message(error.as_ref(), &signal)
            )),
        }
    }

    async fn format_foreground_result(
        &self,
        task_id: &str,
        handle: &SubagentHandle,
        timeout_ms: u64,
    ) -> Result<ExecutableToolResult, WireAsyncError> {
        let info = self.tasks.get_task(task_id);
        let stop_code = match &info {
            Some(AgentTaskInfo::Agent(info)) => info.stop_code.clone(),
            _ => None,
        };
        if info.as_ref().is_some_and(|info| info.status() == AgentTaskStatus::Completed) {
            return Ok(tool_result(
                format_foreground_agent_success(
                    handle,
                    &self.tasks.read_output(task_id, None).await?,
                    stop_code.as_deref(),
                ),
                false,
            ));
        }
        let timed_out = info
            .as_ref()
            .is_some_and(|info| info.status() == AgentTaskStatus::TimedOut);
        let message = if timed_out {
            format!(
                "Agent timed out after {}.",
                format_subagent_timeout_description(timeout_ms)
            )
        } else {
            format_subagent_stopped_message(info.as_ref().and_then(|info| info.stop_reason()))
        };
        let reason = failure_stop_reason(info.as_ref(), stop_code.as_deref());
        Ok(tool_error(format_foreground_agent_failure(
            handle,
            &message,
            reason,
            &self.tasks.read_output(task_id, None).await?,
        )))
    }
}

fn recover_mirror_error(
    error: WireAsyncError,
    turn: &Arc<Turn>,
    partial_output: &Arc<dyn Fn() -> String + Send + Sync>,
    signal: &AbortSignal,
) -> SharedSubagentError {
    let message = error.to_string();
    if let Some(reason) = signal
        .reason()
        .filter(|reason| reason.to_string() == message)
    {
        return SharedSubagentError::from_shared(reason);
    }
    let fallback = SharedSubagentError::new(error);
    let Some(turn_result) = turn.result.clone().now_or_never() else {
        return fallback;
    };
    match classify_turn_result(turn_result) {
        Err(coded) if coded.to_string() == message => SharedSubagentError::new(coded),
        Ok(completed) => {
            if !partial_output().trim().is_empty() {
                return fallback;
            }
            let stop_reason = completed.stop_reason;
            if no_final_message_error(stop_reason.as_deref()) != message {
                return fallback;
            }
            let details = stop_reason.map(|stop_reason| {
                Map::from_iter([(
                    "stopReason".to_string(),
                    Value::String(stop_reason),
                )])
            });
            SharedSubagentError::new(Box::new(Error2::with_options(
                AGENT_NO_FINAL_MESSAGE,
                message,
                Error2Options {
                    details,
                    ..Default::default()
                },
            )))
        }
        _ => fallback,
    }
}

#[derive(Default)]
struct BackendCheck {
    name: Option<SubagentBackendName>,
    error: Option<String>,
}

fn is_user_reference_source(source: ToolSource) -> bool {
    matches!(source, ToolSource::Host | ToolSource::Plugin)
}

fn policy_source(source: ToolSource) -> PolicyToolSource {
    match source {
        ToolSource::Mcp => PolicyToolSource::Mcp,
        ToolSource::Host => PolicyToolSource::User,
        ToolSource::Builtin | ToolSource::Plugin => PolicyToolSource::Builtin,
    }
}

fn upsert_reference(refs: &mut Vec<ToolReference>, reference: ToolReference) {
    if let Some(index) = refs.iter().position(|current| current.name == reference.name) {
        refs[index] = reference;
    } else {
        refs.push(reference);
    }
}

fn is_tool_name_pattern(name: &str) -> bool {
    is_mcp_tool_name(name)
        && name
            .chars()
            .any(|character| matches!(character, '*' | '?' | '[' | ']' | '{' | '}' | '!' | '@' | '+' | '(' | ')'))
}

pub fn subagent_tool_contribution_options() -> AgentToolContributionOptions {
    AgentToolContributionOptions::new("Agent")
        .domain("subagent")
        .required_environment_capabilities(vec!["process".to_string()])
}

pub fn build_profile_descriptions(
    profiles: &[AgentProfile],
    tools: &[ToolReference],
    is_tool_active_for_profile: &dyn Fn(&AgentProfile, &str, ToolSource) -> bool,
) -> String {
    profiles
        .iter()
        .map(|profile| {
            let details = [profile.description.as_ref(), profile.when_to_use.as_ref()]
                .into_iter()
                .flatten()
                .filter(|part| !part.is_empty())
                .cloned()
                .collect::<Vec<_>>();
            let header = if details.is_empty() {
                format!("- {}", profile.name)
            } else {
                format!("- {}: {}", profile.name, details.join(" "))
            };
            let policy = ToolActivationPolicy {
                tools: profile.tools.clone(),
                disallowed_tools: profile.disallowed_tools.clone(),
            };
            let active_tools = resolve_active_tool_names(&policy);
            let externally_restricted = tools.iter().any(|tool| {
                is_tool_active(&policy, &tool.name, policy_source(tool.source))
                    && !is_tool_active_for_profile(profile, &tool.name, tool.source)
            });
            if externally_restricted || active_tools.is_none() {
                let effective_tools = tools
                    .iter()
                    .filter(|tool| {
                        is_tool_active_for_profile(profile, &tool.name, tool.source)
                    })
                    .map(|tool| tool.name.clone())
                    .collect::<Vec<_>>();
                if effective_tools.is_empty() {
                    return format!("{header}\n  Tools: none");
                }
                return format!("{header}\n  Tools: {}", effective_tools.join(", "));
            }
            let active_tools = active_tools.unwrap();
            if active_tools.is_empty() {
                format!("{header}\n  Tools: none")
            } else {
                format!("{header}\n  Tools: {}", active_tools.join(", "))
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentStopReason {
    Completed,
    RepeatBreaker,
    MaxTokens,
    MaxSteps,
    Filtered,
    ProviderError,
    NoFinalMessage,
    Cancelled,
    Stopped,
    TimedOut,
    Error,
}

impl SubagentStopReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::RepeatBreaker => "repeat_breaker",
            Self::MaxTokens => "max_tokens",
            Self::MaxSteps => "max_steps",
            Self::Filtered => "filtered",
            Self::ProviderError => "provider_error",
            Self::NoFinalMessage => "no_final_message",
            Self::Cancelled => "cancelled",
            Self::Stopped => "stopped",
            Self::TimedOut => "timed_out",
            Self::Error => "error",
        }
    }
}

const REASON_MAX_CHARS: usize = 2000;
const REPEAT_BREAKER_NOTICE: &str = "notice: The subagent was stopped by the repeat breaker after issuing the same tool call repeatedly. The summary below is its handoff, not a finished result.";
const RESUME_NEXT_STEP: &str = "next_step: Resume to continue where it stopped, or take over the task yourself; if neither works, report the failure to the user.";

pub fn resume_hint(agent_id: &str, prompt: &str) -> String {
    format!("resume_hint: Continue with Agent(resume=\"{agent_id}\", prompt=\"{prompt}\"). Use agent_id only; do not set subagent_type. The subagent retains its prior context; redo any unfinished tool call if its result was lost.")
}

pub fn next_step(reason: SubagentStopReason) -> Option<&'static str> {
    match reason {
        SubagentStopReason::Completed => None,
        SubagentStopReason::RepeatBreaker => Some("next_step: The subagent was stuck on one tool call. If you resume it, change the instructions or supply the missing input; otherwise continue the work yourself."),
        SubagentStopReason::Cancelled => Some("next_step: The user stopped this subagent. Do not restart it unless the user asks."),
        SubagentStopReason::Filtered => Some("next_step: Resuming is unlikely to help; rephrase or split the task before trying again."),
        SubagentStopReason::MaxTokens
        | SubagentStopReason::MaxSteps
        | SubagentStopReason::ProviderError
        | SubagentStopReason::NoFinalMessage
        | SubagentStopReason::Stopped
        | SubagentStopReason::TimedOut
        | SubagentStopReason::Error => Some(RESUME_NEXT_STEP),
    }
}

pub fn failure_stop_reason(
    info: Option<&AgentTaskInfo>,
    stop_code: Option<&str>,
) -> SubagentStopReason {
    if info.is_some_and(|info| info.status() == AgentTaskStatus::TimedOut) {
        return SubagentStopReason::TimedOut;
    }
    if info.is_some_and(|info| info.status() == AgentTaskStatus::Killed) {
        let user_reason = user_cancellation_reason().to_string();
        return if info
            .and_then(AgentTaskInfo::stop_reason)
            .map(str::trim)
            == Some(user_reason.as_str())
        {
            SubagentStopReason::Cancelled
        } else {
            SubagentStopReason::Stopped
        };
    }
    let Some(stop_code) = stop_code else {
        return SubagentStopReason::Error;
    };
    match stop_code {
        REPEAT_BREAKER_STOP_REASON => SubagentStopReason::RepeatBreaker,
        AGENT_MAX_TOKENS_EXCEEDED => SubagentStopReason::MaxTokens,
        LOOP_MAX_STEPS_EXCEEDED => SubagentStopReason::MaxSteps,
        PROVIDER_FILTERED => SubagentStopReason::Filtered,
        PROVIDER_RATE_LIMIT
        | PROVIDER_API_ERROR
        | PROVIDER_OVERLOADED
        | PROVIDER_CONNECTION_ERROR
        | PROVIDER_AUTH_ERROR => SubagentStopReason::ProviderError,
        AGENT_NO_FINAL_MESSAGE => SubagentStopReason::NoFinalMessage,
        _ => SubagentStopReason::Error,
    }
}

pub fn truncate_reason(reason: &str) -> String {
    if reason.encode_utf16().count() <= REASON_MAX_CHARS {
        return reason.to_string();
    }
    let mut units = 0usize;
    let mut cut = reason.len();
    for (index, character) in reason.char_indices() {
        units += character.len_utf16();
        if units > REASON_MAX_CHARS {
            cut = index;
            break;
        }
    }
    format!("{}… [truncated]", &reason[..cut])
}

pub fn format_backend_agent_success(backend_name: &str, run_id: &str, output: &str) -> String {
    [
        format!("backend: {backend_name}"),
        format!("backend_run_id: {run_id}"),
        "status: completed".to_string(),
        "stop_reason: completed".to_string(),
        String::new(),
        "[summary]".to_string(),
        output.to_string(),
    ]
    .join("\n")
}

pub fn format_backend_agent_failure(
    backend_name: &str,
    run_id: &str,
    result: &SubagentBackendResult,
    message: &str,
) -> String {
    [
        format!("backend: {backend_name}"),
        format!("backend_run_id: {run_id}"),
        "status: failed".to_string(),
        format!("stop_reason: {}", backend_stop_reason_name(result.stop_reason)),
        String::new(),
        format!("subagent error: {message}"),
        String::new(),
        if result.output.is_empty() {
            NO_PARTIAL_OUTPUT.to_string()
        } else {
            result.output.clone()
        },
    ]
    .join("\n")
}

fn backend_stop_reason_name(reason: SubagentBackendStopReason) -> &'static str {
    match reason {
        SubagentBackendStopReason::Completed => "completed",
        SubagentBackendStopReason::Aborted => "aborted",
        SubagentBackendStopReason::Error => "error",
        SubagentBackendStopReason::MaxTokens => "max-tokens",
        SubagentBackendStopReason::Refusal => "refusal",
    }
}

pub fn backend_stop_message(
    stop_reason: SubagentBackendStopReason,
    signal: &AbortSignal,
    deadline: Option<&DeadlineAbortSignal>,
    timeout_ms: u64,
) -> String {
    match stop_reason {
        SubagentBackendStopReason::Aborted => {
            if signal.reason().is_some_and(|reason| is_user_cancellation(reason.as_ref())) {
                USER_INTERRUPTED_SUBAGENT_MESSAGE.to_string()
            } else if deadline.is_some_and(|deadline| deadline.timed_out()) {
                format!(
                    "Agent timed out after {}.",
                    format_subagent_timeout_description(timeout_ms)
                )
            } else {
                SUBAGENT_STOPPED_MESSAGE.to_string()
            }
        }
        SubagentBackendStopReason::MaxTokens => {
            "The external backend stopped at its token limit.".to_string()
        }
        SubagentBackendStopReason::Refusal => {
            "The external backend refused the task.".to_string()
        }
        _ => "The external backend reported an error.".to_string(),
    }
}

pub fn format_background_agent_result(
    task_id: &str,
    handle: &SubagentHandle,
    description: &str,
    allow_background: bool,
    detached_by_user: bool,
) -> String {
    let next_step = if allow_background {
        "next_step: The completion arrives automatically in a later turn — do NOT wait, poll, or call TaskOutput on it; continue with other work or hand back to the user. (If you have nothing to do until it finishes, run such tasks in the foreground next time.)"
    } else {
        "next_step: The completion arrives automatically in a later turn."
    };
    let agent_id = &handle.agent_id;
    [
        format!("task_id: {task_id}"),
        "status: running".to_string(),
        format!("agent_id: {agent_id}"),
        format!("actual_subagent_type: {}", handle.profile_name),
        "automatic_notification: true".to_string(),
        String::new(),
        format!("description: {description}"),
        String::new(),
        if detached_by_user {
            format!("note: The user moved this subagent to the background.\n{next_step}")
        } else {
            next_step.to_string()
        },
        format!("resume_hint: To continue or recover this same subagent later, call Agent(resume=\"{agent_id}\", prompt=\"...\"). The parameter is agent_id (\"{agent_id}\"), NOT task_id (\"{task_id}\") or source_id from a later <notification>. Recovery cases: a later <notification type=\"task.lost\" | \"task.failed\" | \"task.killed\"> for this subagent — its conversation history is preserved across session restarts and resume will pick it up."),
    ]
    .join("\n")
}

pub fn format_foreground_agent_success(
    handle: &SubagentHandle,
    result: &str,
    stop_code: Option<&str>,
) -> String {
    let reason = if stop_code == Some(REPEAT_BREAKER_STOP_REASON) {
        SubagentStopReason::RepeatBreaker
    } else {
        SubagentStopReason::Completed
    };
    let mut lines = vec![
        format!("agent_id: {}", handle.agent_id),
        format!("actual_subagent_type: {}", handle.profile_name),
        "status: completed".to_string(),
        format!("stop_reason: {}", reason.as_str()),
    ];
    if reason == SubagentStopReason::RepeatBreaker {
        lines.push(REPEAT_BREAKER_NOTICE.to_string());
    }
    lines.extend([
        String::new(),
        "[summary]".to_string(),
        result.to_string(),
        String::new(),
        resume_hint(&handle.agent_id, "..."),
    ]);
    if let Some(next) = next_step(reason) {
        lines.push(next.to_string());
    }
    lines.join("\n")
}

pub fn format_foreground_agent_failure(
    handle: &SubagentHandle,
    message: &str,
    reason: SubagentStopReason,
    partial_output: &str,
) -> String {
    let mut lines = vec![
        format!("agent_id: {}", handle.agent_id),
        format!("actual_subagent_type: {}", handle.profile_name),
        "status: failed".to_string(),
        format!("stop_reason: {}", reason.as_str()),
        String::new(),
        format!("subagent error: {message}"),
        String::new(),
        if partial_output.is_empty() {
            NO_PARTIAL_OUTPUT.to_string()
        } else {
            partial_output.to_string()
        },
        String::new(),
    ];
    if reason != SubagentStopReason::Cancelled {
        lines.push(resume_hint(&handle.agent_id, "continue"));
    }
    if let Some(next) = next_step(reason) {
        lines.push(next.to_string());
    }
    lines.join("\n")
}

pub fn launch_error_message(error: &(dyn Error + 'static), signal: &AbortSignal) -> String {
    if signal.reason().is_some_and(|reason| is_user_cancellation(reason.as_ref())) {
        return USER_INTERRUPTED_SUBAGENT_MESSAGE.to_string();
    }
    let shared_abort = error
        .downcast_ref::<SharedSubagentError>()
        .is_some_and(|error| is_abort_error(error.as_error()));
    let mirrored_abort = error.is::<SharedAgentRunError>()
        && signal.reason().is_some_and(|reason| {
            is_abort_error(reason.as_ref()) && error.to_string() == reason.to_string()
        });
    if is_abort_error(error) || shared_abort || mirrored_abort {
        return format_subagent_stopped_message(
            signal.reason().map(|reason| error_message(reason)).as_deref(),
        );
    }
    error_message(error)
}

pub fn format_subagent_stopped_message(reason: Option<&str>) -> String {
    let normalized = reason.map(str::trim).filter(|reason| !reason.is_empty());
    if normalized == Some(user_cancellation_reason().to_string().as_str()) {
        return USER_INTERRUPTED_SUBAGENT_MESSAGE.to_string();
    }
    let Some(normalized) = normalized else {
        return SUBAGENT_STOPPED_MESSAGE.to_string();
    };
    format!(
        "{SUBAGENT_STOPPED_MESSAGE} Reason: {}",
        truncate_reason(normalized)
    )
}

fn throw_if_aborted(signal: &AbortSignal) -> Result<(), WireAsyncError> {
    if !signal.aborted() {
        return Ok(());
    }
    if let Some(reason) = signal.reason() {
        return Err(Box::new(SharedSubagentError::from_shared(reason)));
    }
    Err(Box::new(abort_error()))
}

fn coded_error<const N: usize>(
    code: &'static str,
    message: String,
    details: [(&'static str, Value); N],
) -> WireAsyncError {
    Box::new(Error2::with_options(
        code,
        message,
        Error2Options {
            details: Some(Map::from_iter(
                details.into_iter().map(|(name, value)| (name.to_string(), value)),
            )),
            ..Default::default()
        },
    ))
}

fn register_agent_task(
    tasks: &dyn IAgentTaskService,
    task: Arc<dyn crate::agent::task::types::AgentTask>,
    options: RegisterAgentTaskOptions,
) -> Result<String, WireAsyncError> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        tasks.register_task(task, Some(options))
    })) {
        Ok(task_id) => Ok(task_id),
        Err(payload) => Err(registration_panic_error(payload)),
    }
}

fn remove_abort_link(link: Option<Box<dyn FnOnce() + Send>>) {
    if let Some(unlink) = link {
        unlink();
    }
}

fn registration_panic_error(payload: Box<dyn Any + Send>) -> WireAsyncError {
    match payload.downcast::<WireAsyncError>() {
        Ok(error) => *error,
        Err(payload) => {
            if let Some(message) = payload.downcast_ref::<String>() {
                return message.clone().into();
            }
            if let Some(message) = payload.downcast_ref::<&'static str>() {
                return (*message).into();
            }
            "background agent task registration failed".into()
        }
    }
}

fn tool_result(output: impl Into<String>, is_error: bool) -> ExecutableToolResult {
    let output = output.into();
    ExecutableToolResult {
        output: output.into(),
        is_error: is_error.then_some(true),
        ..Default::default()
    }
}

fn tool_error(output: impl Into<String>) -> ExecutableToolResult {
    tool_result(output, true)
}
