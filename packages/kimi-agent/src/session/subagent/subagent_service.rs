use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use kimi_core::primitives::errors::domains::agent_lifecycle::{
    AGENT_NOT_FOUND, AGENT_TYPE_NOT_ALLOWED,
};
use kimi_core::primitives::errors::domains::profile::{MODEL_NOT_CONFIGURED, PROFILE_UNKNOWN};
use kimi_core::primitives::errors::{Error2, Error2Options};
use parking_lot::Mutex;
use serde_json::{Map, Value};

use crate::_base::di::lifecycle::{IDisposable, to_disposable};
use crate::_base::di::scope::{IAgentScopeHandle, ScopeActivation, register_scoped_service};
use crate::_base::log::log::ILogService;
use crate::agent::agent_context::agent_context::AgentContext;
use crate::agent::environment_binding::agent_environment::IAgentEnvironmentService;
use crate::agent::environment_binding::environment_binding::IAgentEnvironmentBindingService;
use crate::agent::permission_mode::permission_mode::IAgentPermissionModeService;
use crate::agent::profile::profile::{BindAgentInput, IAgentProfileService};
use crate::agent::scope_context::scope_context::agent_context_of;
use crate::agent::user_tool::user_tool::IAgentUserToolService;
use crate::app::agent_profile_catalog::agent_profile_catalog::AgentProfilePromptPrefixContext;
use crate::app::agent_profile_catalog::profile_shared::{
    subagent_allowlist_for, subagent_type_not_allowed_message, without_delegating_targets,
};
use crate::app::agent_profile_catalog::prompt_prefix::apply_profile_prompt_prefix;
use crate::app::config::config::IConfigService;
use crate::app::scopes::LifecycleScope;
use crate::environment::environment::Environment;
use crate::environment::environment_workspace_view::EnvironmentWorkspaceView;
use crate::features::reminder::reminder_service::IAgentReminderService;
use crate::hooks::{Hooks, create_hooks};
use crate::llm_adapter::model::catalog::{IModelCatalog, Model};
use crate::session::agent_lifecycle::agent_lifecycle::{
    CreateAgentOptions, ForkAgentOptions, IAgentLifecycleService,
};
use crate::session::agent_lifecycle::subagent_metadata::{
    SubagentLabelOptions, assert_subagent_depth_allowed, subagent_labels,
};
use crate::session::session_agent_profile_catalog::session_agent_profile_catalog::ISessionAgentProfileCatalog;
use crate::session::session_context::session_context::ISessionContext;
use crate::session::session_metadata::session_metadata::ISessionMetadata;
use crate::wire::record::WireAsyncError;

use super::config_section::{
    SubagentBinding, SubagentModelSource, resolve_subagent_binding, resolve_subagent_thinking,
    wrap_subagent_model_error,
};
use super::run_agent_turn::{RunAgentTurnOptions, run_agent_turn};
use super::spawn::{
    DEFAULT_PROFILE_NAME, FORK_CONTEXT_NOTICE, SpawnSubagentOptions, SpawnedSubagent,
    SubagentSpawnPlan, SubagentSpawnPlanInput,
};
use super::subagent::{
    AgentRunHandle, AgentRunRequest, AgentTaskHooks, AgentTaskStopHookContext,
    ISessionSubagentService, RunAgentOptions,
};

pub struct SessionSubagentService {
    agent_lifecycle: Arc<dyn IAgentLifecycleService>,
    catalog: Arc<dyn ISessionAgentProfileCatalog>,
    config_service: Arc<dyn IConfigService>,
    model_catalog: Arc<dyn IModelCatalog>,
    session_context: Arc<dyn ISessionContext>,
    log: Arc<dyn ILogService>,
    session_metadata: Arc<dyn ISessionMetadata>,
    hooks: Arc<Hooks<AgentTaskHooks>>,
    stop_listeners: Arc<Mutex<Vec<(u64, Arc<dyn Fn(AgentTaskStopHookContext) + Send + Sync>)>>>,
    next_listener_id: AtomicU64,
}

impl SessionSubagentService {
    pub fn new(
        agent_lifecycle: Arc<dyn IAgentLifecycleService>,
        catalog: Arc<dyn ISessionAgentProfileCatalog>,
        config_service: Arc<dyn IConfigService>,
        model_catalog: Arc<dyn IModelCatalog>,
        session_context: Arc<dyn ISessionContext>,
        log: Arc<dyn ILogService>,
        session_metadata: Arc<dyn ISessionMetadata>,
    ) -> Self {
        Self {
            agent_lifecycle,
            catalog,
            config_service,
            model_catalog,
            session_context,
            log,
            session_metadata,
            hooks: create_hooks(&["onWillStartAgentTask"]),
            stop_listeners: Arc::new(Mutex::new(Vec::new())),
            next_listener_id: AtomicU64::new(0),
        }
    }

    pub fn run(
        &self,
        agent: Arc<AgentContext>,
        request: AgentRunRequest,
        opts: RunAgentOptions,
    ) -> Result<impl std::future::Future<Output = Result<AgentRunHandle, WireAsyncError>>, WireAsyncError> {
        let handle = self.require_caller(&agent.agent_id)?;
        Ok(run_agent_turn(
            &handle,
            request,
            RunAgentTurnOptions {
                signal: opts.signal,
                on_ready: opts.on_ready,
            },
        ))
    }

    pub async fn plan_spawn(
        &self,
        input: SubagentSpawnPlanInput,
    ) -> Result<SubagentSpawnPlan, WireAsyncError> {
        let caller = self.require_caller(&input.caller_agent_id)?;
        let fork = input.fork == Some(true);
        self.catalog.ready().await?;
        let own = caller.accessor().get::<IAgentProfileService>().data();
        let requested = input
            .profile_name
            .as_deref()
            .filter(|profile_name| !profile_name.is_empty());
        let requested_profile_name = requested
            .map(ToString::to_string)
            .or_else(|| {
                fork.then(|| {
                    own.profile_name
                        .clone()
                        .unwrap_or_else(|| DEFAULT_PROFILE_NAME.to_string())
                })
            })
            .unwrap_or_else(|| DEFAULT_PROFILE_NAME.to_string());
        let mut allowlist = subagent_allowlist_for(&self.catalog, &own, None);
        if allowlist.is_some() && own.subagents.is_none() {
            allowlist = allowlist.map(|allowlist| {
                without_delegating_targets(&self.catalog, &allowlist)
            });
        }
        if !fork
            && let Some(allowlist) = allowlist.as_ref()
            && !allowlist.contains(&requested_profile_name)
        {
            let mut details = Map::new();
            details.insert(
                "profileName".to_string(),
                Value::String(requested_profile_name.clone()),
            );
            details.insert("allowlist".to_string(), serde_json::to_value(allowlist).unwrap());
            return Err(Box::new(Error2::with_options(
                AGENT_TYPE_NOT_ALLOWED,
                subagent_type_not_allowed_message(&requested_profile_name, allowlist),
                Error2Options {
                    details: Some(details),
                    ..Default::default()
                },
            )));
        }
        let profile = self.catalog.get(&requested_profile_name);
        if !fork && profile.is_none() {
            return Err(Box::new(Error2::with_options(
                PROFILE_UNKNOWN,
                format!("Unknown agent type: \"{requested_profile_name}\""),
                Error2Options {
                    details: Some(Map::from_iter([(
                        "profileName".to_string(),
                        Value::String(requested_profile_name.clone()),
                    )])),
                    ..Default::default()
                },
            )));
        }
        let Some(own_model) = own.model_alias.clone() else {
            return Err(agent_error(
                MODEL_NOT_CONFIGURED,
                "Caller agent has no model bound".to_string(),
                &input.caller_agent_id,
            ));
        };
        let binding = if fork {
            SubagentBinding {
                model: own_model,
                thinking: Some(own.thinking_level.clone()),
                model_source: SubagentModelSource::Inherited,
            }
        } else {
            resolve_subagent_binding(
                self.config_service.as_ref(),
                (&own_model, &own.thinking_level),
                input.model.as_deref(),
            )?
        };
        let model = match self.model_catalog.get(&binding.model) {
            Ok(model) => model,
            Err(error) => {
                return Err(wrap_subagent_model_error(
                    error,
                    &binding.model,
                    Some(&own_model),
                ));
            }
        };
        Ok(SubagentSpawnPlan {
            profile_name: profile
                .map(|profile| profile.name.clone())
                .unwrap_or(requested_profile_name),
            model: binding.model,
            model_source: Some(binding.model_source),
            thinking: resolve_subagent_thinking(
                self.config_service.as_ref(),
                Some(&model),
                binding.thinking.as_deref(),
            ),
            fork,
        })
    }

    pub async fn spawn(
        &self,
        opts: SpawnSubagentOptions,
    ) -> Result<SpawnedSubagent, WireAsyncError> {
        let caller = self.require_caller(&opts.caller_agent_id)?;
        let parent_meta = self
            .session_metadata
            .read()
            .await?
            .agents
            .unwrap_or_default()
            .remove(&opts.caller_agent_id);
        let depth = assert_subagent_depth_allowed(parent_meta.as_ref())?;
        let mut labels = opts.labels.clone().unwrap_or_default();
        labels.extend(subagent_labels(
            &opts.caller_agent_id,
            SubagentLabelOptions {
                swarm_item: None,
                depth: Some(depth),
            },
        ));
        let plan = opts.plan.clone();
        let caller_binding = caller
            .accessor()
            .get::<IAgentEnvironmentBindingService>()
            .current();
        let lease = if plan.fork {
            None
        } else {
            Some(
                caller
                    .accessor()
                    .get::<IAgentEnvironmentService>()
                    .acquire_when_ready(&["process"])
                    .await?,
            )
        };
        let result = self
            .spawn_with_lease(&caller, &opts, &plan, labels, &caller_binding, lease.as_ref())
            .await;
        if let Some(lease) = lease {
            lease.dispose();
        }
        result
    }

    async fn spawn_with_lease(
        &self,
        caller: &Arc<IAgentScopeHandle>,
        opts: &SpawnSubagentOptions,
        plan: &SubagentSpawnPlan,
        labels: HashMap<String, String>,
        caller_binding: &crate::environment::environment::EnvironmentBinding,
        lease: Option<&crate::environment::environment::EnvironmentLease>,
    ) -> Result<SpawnedSubagent, WireAsyncError> {
        let created = match async {
            if plan.fork {
                let forked = self
                    .agent_lifecycle
                    .fork(
                        agent_context_of(caller),
                        Some(ForkAgentOptions {
                            labels: Some(labels.clone()),
                            ..Default::default()
                        }),
                    )
                    .await?;
                let created = self.agent_lifecycle.handle_of(&forked.agent_id).unwrap();
                created
                    .accessor()
                    .get::<IAgentReminderService>()
                    .notify(FORK_CONTEXT_NOTICE, Some(json_variant("fork_context")));
                Ok::<_, WireAsyncError>(created)
            } else {
                let created_context = self
                    .agent_lifecycle
                    .create(Some(CreateAgentOptions {
                        binding: Some(BindAgentInput {
                            profile: plan.profile_name.clone(),
                            model: Some(plan.model.clone()),
                            thinking: plan.thinking.clone(),
                            strict_thinking: None,
                        }),
                        labels: Some(labels.clone()),
                        environment_id: Some(caller_binding.environment_id.clone()),
                        environment_cwd: caller_binding.cwd.clone(),
                        ..Default::default()
                    }))
                    .await?;
                Ok::<_, WireAsyncError>(
                    self.agent_lifecycle.handle_of(&created_context.agent_id).unwrap(),
                )
            }
        }
        .await
        {
            Ok(created) => created,
            Err(error) => {
                let caller_model = caller
                    .accessor()
                    .get::<IAgentProfileService>()
                    .data()
                    .model_alias;
                return Err(wrap_subagent_model_error(
                    error,
                    &plan.model,
                    caller_model.as_deref(),
                ));
            }
        };
        let permission_mode = caller.accessor().get::<IAgentPermissionModeService>().mode();
        created
            .accessor()
            .get::<IAgentPermissionModeService>()
            .set_mode(permission_mode);
        let created_user_tools = created.accessor().get::<IAgentUserToolService>();
        let caller_user_tools = caller.accessor().get::<IAgentUserToolService>();
        if plan.fork {
            let active_tool_names = created
                .accessor()
                .get::<IAgentProfileService>()
                .get_active_tool_names();
            created_user_tools.inherit_user_tools(&caller_user_tools, active_tool_names);
        } else {
            created_user_tools.inherit_user_tools(&caller_user_tools, None);
        }
        let prompt_text = if plan.fork {
            opts.prompt.clone()
        } else {
            self.apply_prompt_prefix(
                &plan.profile_name,
                &opts.prompt,
                &lease.unwrap().environment,
                caller_binding.cwd.as_deref(),
            )
            .await?
        };
        Ok(SpawnedSubagent {
            agent_id: created.id().to_string(),
            profile_name: plan.profile_name.clone(),
            model: plan.model.clone(),
            model_source: plan.model_source.clone(),
            prompt_text,
        })
    }

    pub fn notify_agent_task_stopped(&self, context: AgentTaskStopHookContext) {
        let listeners = self.stop_listeners.lock().clone();
        for (_, listener) in listeners {
            listener(context.clone());
        }
    }

    async fn apply_prompt_prefix(
        &self,
        profile_name: &str,
        prompt: &str,
        environment: &Environment,
        cwd: Option<&str>,
    ) -> Result<String, WireAsyncError> {
        let Some(profile) = self.catalog.get(profile_name) else {
            return Ok(prompt.to_string());
        };
        if profile.prompt_prefix.is_none() {
            return Ok(prompt.to_string());
        }
        let view = EnvironmentWorkspaceView::new(
            environment.clone(),
            cwd.map(ToString::to_string)
                .unwrap_or_else(|| self.session_context.cwd()),
        );
        apply_profile_prompt_prefix(
            &profile,
            prompt,
            AgentProfilePromptPrefixContext {
                cwd: view.work_dir().to_string(),
                process: environment.process.clone().unwrap(),
                log: Some(self.log.clone()),
            },
        )
        .await
    }

    fn require_caller(&self, agent_id: &str) -> Result<Arc<IAgentScopeHandle>, WireAsyncError> {
        self.agent_lifecycle.handle_of(agent_id).ok_or_else(|| {
            agent_error(
                AGENT_NOT_FOUND,
                format!("Caller agent \"{agent_id}\" does not exist"),
                agent_id,
            )
        })
    }
}

#[async_trait]
impl ISessionSubagentService for SessionSubagentService {
    fn hooks(&self) -> Arc<Hooks<AgentTaskHooks>> {
        self.hooks.clone()
    }

    fn on_did_stop_agent_task(
        &self,
        listener: Arc<dyn Fn(AgentTaskStopHookContext) + Send + Sync>,
    ) -> Box<dyn IDisposable> {
        let id = self.next_listener_id.fetch_add(1, Ordering::Relaxed);
        self.stop_listeners.lock().push((id, listener));
        let listeners = Arc::downgrade(&self.stop_listeners);
        to_disposable(move || {
            if let Some(listeners) = listeners.upgrade() {
                listeners.lock().retain(|(candidate, _)| *candidate != id);
            }
        })
    }

    async fn run(
        &self,
        agent: Arc<AgentContext>,
        request: AgentRunRequest,
        opts: RunAgentOptions,
    ) -> Result<AgentRunHandle, WireAsyncError> {
        let handle = self.require_caller(&agent.agent_id)?;
        run_agent_turn(
            &handle,
            request,
            RunAgentTurnOptions {
                signal: opts.signal,
                on_ready: opts.on_ready,
            },
        )
        .await
    }

    async fn plan_spawn(
        &self,
        input: SubagentSpawnPlanInput,
    ) -> Result<SubagentSpawnPlan, WireAsyncError> {
        SessionSubagentService::plan_spawn(self, input).await
    }

    async fn spawn(
        &self,
        opts: SpawnSubagentOptions,
    ) -> Result<SpawnedSubagent, WireAsyncError> {
        SessionSubagentService::spawn(self, opts).await
    }

    fn notify_agent_task_stopped(&self, context: AgentTaskStopHookContext) {
        SessionSubagentService::notify_agent_task_stopped(self, context)
    }
}

fn agent_error(code: &'static str, message: String, agent_id: &str) -> WireAsyncError {
    Box::new(Error2::with_options(
        code,
        message,
        Error2Options {
            details: Some(Map::from_iter([(
                "agentId".to_string(),
                Value::String(agent_id.to_string()),
            )])),
            ..Default::default()
        },
    ))
}

fn json_variant(variant: &str) -> Value {
    Value::Object(Map::from_iter([(
        "variant".to_string(),
        Value::String(variant.to_string()),
    )]))
}

pub fn register_subagent_service() {
    register_scoped_service(
        LifecycleScope::Session,
        ScopeActivation::OnScopeCreated,
        "subagent",
    );
}
