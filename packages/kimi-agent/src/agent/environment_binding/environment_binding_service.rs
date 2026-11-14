use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::json;

use crate::_base::di::lifecycle::IDisposable;
use crate::_base::event::{Emitter, Event};
use crate::_base::log::log::ILogService;
use crate::agent::profile::context::{ProfileContextDeps, load_agents_md_detailed};
use crate::agent::scope_context::scope_context::IAgentScopeContext;
use crate::agent::state::agent_state_v2::IAgentStateService;
use crate::app::bootstrap::bootstrap::IBootstrapService;
use crate::app::environment::environment::EnvironmentResolver;
use crate::environment::environment::{EnvironmentBinding, EnvironmentCapability, LOCAL_ENVIRONMENT_ID};
use crate::environment::environment_registry::EnvironmentError;
use crate::features::reminder::reminder_service::ReminderService;
use crate::features::reminder::types::ReminderNotification;
use crate::hooks::HookRegistration;
use crate::os::interface::host_environment::HostEnvironmentInfo;
use crate::session::agent_lifecycle::agent_lifecycle::MAIN_AGENT_ID;
use crate::session::session_context::session_context::ISessionContext;
use crate::session::workspace_context::workspace_context::ISessionWorkspaceContext;
use crate::session::workspace_context::workspace_roots::{
    EffectiveWorkspaceRoots, resolve_effective_workspace_roots,
};
use crate::session::workspace_info::workspace_info::ISessionWorkspaceInfo;
use crate::state::event_dispatcher::IEventDispatcher;
use crate::wire::record::WireAsyncError;

use super::environment_binding::{IAgentEnvironmentBindingSeed, IAgentEnvironmentBindingService};
use super::environment_binding_ops::{
    EnvironmentSetBinding, environment_binding_replayable_key,
};

pub const ENVIRONMENT_BINDING_REMINDER_VARIANT: &str = "environment_binding";
pub const PROJECT_CONTEXT_REMINDER_VARIANT: &str = "project_context";

fn project_context_reminder_text(
    binding: &EnvironmentBinding,
    cwd: &str,
    paths: &[String],
) -> String {
    let paths = paths
        .iter()
        .map(|path| format!("- {path}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "The active project context is now \"{}\" at working directory {cwd}. Previous working directories, AGENTS.md instructions, and environment details no longer apply. The AGENTS.md file(s) below apply to this working directory:\n{paths}\nRead them before making changes in this working directory.",
        binding.environment_id
    )
}

fn environment_reminder_text(
    binding: &EnvironmentBinding,
    environment: &HostEnvironmentInfo,
    fallback_cwd: &str,
) -> String {
    format!(
        "The active environment is now \"{}\": {} {} {}, shell {} ({}), working directory {}. Tool calls execute in this environment.",
        binding.environment_id,
        environment.os_kind,
        environment.os_version,
        environment.os_arch,
        environment.shell_name,
        environment.shell_path,
        binding.cwd.as_deref().unwrap_or(fallback_cwd),
    )
}

struct AgentEnvironmentBindingServiceInner {
    scope_context: Arc<dyn IAgentScopeContext>,
    state: Arc<dyn IAgentStateService>,
    seed: Arc<dyn IAgentEnvironmentBindingSeed>,
    session: Arc<dyn ISessionContext>,
    workspace_context: Arc<dyn ISessionWorkspaceContext>,
    workspace_info: Arc<dyn ISessionWorkspaceInfo>,
    resolver: Arc<dyn EnvironmentResolver>,
    dispatcher: Arc<dyn IEventDispatcher>,
    reminder: Arc<dyn ReminderService>,
    log: Arc<dyn ILogService>,
    bootstrap: Arc<dyn IBootstrapService>,
    change_emitter: Emitter<EnvironmentBinding>,
    restore_hook: Mutex<Option<HookRegistration<()>>>,
}

impl AgentEnvironmentBindingServiceInner {
    fn current(&self) -> EnvironmentBinding {
        let replayed = self
            .state
            .get(&environment_binding_replayable_key().state_key());
        if replayed.is_null() {
            return self.seed.binding().clone();
        }
        serde_json::from_value(replayed).unwrap_or_else(|error| panic!("{error}"))
    }

    fn is_seed_round_trip(&self, binding: &EnvironmentBinding) -> bool {
        let seed = self.seed.binding();
        binding.environment_id != LOCAL_ENVIRONMENT_ID
            && binding.environment_id == seed.environment_id
            && binding.cwd == seed.cwd
    }

    fn apply_session_work_dir(&self, binding: &EnvironmentBinding) {
        if self.scope_context.agent_id() != MAIN_AGENT_ID {
            return;
        }
        self.workspace_context
            .set_work_dir(self.effective_roots(binding, None).work_dir);
    }

    fn effective_roots(
        &self,
        binding: &EnvironmentBinding,
        host: Option<&HostEnvironmentInfo>,
    ) -> EffectiveWorkspaceRoots {
        let resolved_host = match host {
            Some(host) => Some(host.clone()),
            None
                if binding.cwd.is_none()
                    && binding.environment_id != LOCAL_ENVIRONMENT_ID =>
            {
                self.resolver
                    .inspect(binding)
                    .ok()
                    .and_then(|environment| environment.host())
            }
            None => None,
        };
        resolve_effective_workspace_roots(
            self.session.as_ref(),
            Some(binding),
            self.workspace_info.as_ref(),
            resolved_host.as_ref(),
        )
    }

    fn bind(
        self: &Arc<Self>,
        environment_id: &str,
        cwd: Option<String>,
    ) -> Result<EnvironmentBinding, WireAsyncError> {
        let binding = EnvironmentBinding {
            environment_id: environment_id.to_string(),
            cwd,
        };
        let lease = self.resolver.acquire(&binding, &[])?;
        lease.dispose();
        Ok(self.commit(binding))
    }

    fn commit(self: &Arc<Self>, binding: EnvironmentBinding) -> EnvironmentBinding {
        let previous = self.current();
        if binding == previous {
            return previous;
        }
        let next = EnvironmentBinding {
            environment_id: binding.environment_id,
            cwd: binding.cwd,
        };
        let dispatcher = self.dispatcher.clone();
        let event = EnvironmentSetBinding {
            agent_id: self.scope_context.agent_id(),
            environment_id: next.environment_id.clone(),
            cwd: next.cwd.clone(),
        };
        tokio::spawn(async move {
            let _ = dispatcher.dispatch(event.into()).await;
        });
        self.state.set(
            &environment_binding_replayable_key().state_key(),
            serde_json::to_value(next.clone()).unwrap_or_else(|error| panic!("{error}")),
        );
        self.apply_session_work_dir(&next);
        self.emit_project_context_reminder(next.clone());
        self.change_emitter.fire(next.clone());
        next
    }

    fn emit_environment_reminder(&self, binding: &EnvironmentBinding) {
        if self.scope_context.agent_id() != MAIN_AGENT_ID {
            return;
        }
        let Ok(environment) = self.resolver.inspect(binding) else {
            return;
        };
        let Some(environment) = environment.host() else {
            return;
        };
        let fallback_cwd = self.effective_roots(binding, Some(&environment)).work_dir;
        self.reminder.notify(
            &environment_reminder_text(binding, &environment, &fallback_cwd),
            &ReminderNotification {
                variant: ENVIRONMENT_BINDING_REMINDER_VARIANT.to_string(),
                owner_prompt_id: None,
            },
        );
    }

    fn emit_project_context_reminder(self: &Arc<Self>, binding: EnvironmentBinding) {
        if self.scope_context.agent_id() != MAIN_AGENT_ID {
            return;
        }
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let Some(this) = weak.upgrade() else {
                return;
            };
            if let Err(error) = this.probe_project_context(&binding).await {
                this.log.warn(
                    &format!(
                        "project context probe for environment {} failed",
                        binding.environment_id
                    ),
                    Some(json!({"error": error.to_string()})),
                );
            }
        });
    }

    async fn probe_project_context(
        &self,
        binding: &EnvironmentBinding,
    ) -> Result<(), WireAsyncError> {
        let lease = match self.resolver.acquire(binding, &[EnvironmentCapability::Fs]) {
            Ok(lease) => lease,
            Err(error) if error.downcast_ref::<EnvironmentError>().is_some() => return Ok(()),
            Err(error) => return Err(error),
        };
        let result = async {
            let fs = lease.environment.fs();
            let Some(host) = lease.environment.host() else {
                return Ok(());
            };
            let cwd = self.effective_roots(binding, Some(&host)).work_dir;
            let brand_home = self.bootstrap.home_dir();
            let loaded = load_agents_md_detailed(
                ProfileContextDeps {
                    fs: fs.as_ref(),
                    home_dir: &host.home_dir,
                },
                &cwd,
                Some(&brand_home),
            )
            .await?;
            if loaded.paths.is_empty() {
                return Ok(());
            }
            self.reminder.notify(
                &project_context_reminder_text(binding, &cwd, &loaded.paths),
                &ReminderNotification {
                    variant: PROJECT_CONTEXT_REMINDER_VARIANT.to_string(),
                    owner_prompt_id: None,
                },
            );
            Ok(())
        }
        .await;
        lease.dispose();
        result
    }

    fn dispose(&self) {
        if let Some(registration) = self.restore_hook.lock().take() {
            registration.dispose();
        }
        self.change_emitter.dispose();
    }
}

impl Drop for AgentEnvironmentBindingServiceInner {
    fn drop(&mut self) {
        self.dispose();
    }
}

#[derive(Clone)]
pub struct AgentEnvironmentBindingService {
    inner: Arc<AgentEnvironmentBindingServiceInner>,
}

impl AgentEnvironmentBindingService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        scope_context: Arc<dyn IAgentScopeContext>,
        state: Arc<dyn IAgentStateService>,
        seed: Arc<dyn IAgentEnvironmentBindingSeed>,
        session: Arc<dyn ISessionContext>,
        workspace_context: Arc<dyn ISessionWorkspaceContext>,
        workspace_info: Arc<dyn ISessionWorkspaceInfo>,
        resolver: Arc<dyn EnvironmentResolver>,
        dispatcher: Arc<dyn IEventDispatcher>,
        reminder: Arc<dyn ReminderService>,
        log: Arc<dyn ILogService>,
        bootstrap: Arc<dyn IBootstrapService>,
    ) -> Self {
        let _ = state.contribute_state(environment_binding_replayable_key().state_key());
        let inner = Arc::new(AgentEnvironmentBindingServiceInner {
            scope_context,
            state,
            seed,
            session,
            workspace_context,
            workspace_info,
            resolver,
            dispatcher,
            reminder,
            log,
            bootstrap,
            change_emitter: Emitter::new(),
            restore_hook: Mutex::new(None),
        });
        let weak = Arc::downgrade(&inner);
        let registration = inner.dispatcher.hooks().on_did_restore.register(
            "agent-environment-binding",
            Arc::new(move |_ctx, next| {
                let weak = weak.clone();
                Box::pin(async move {
                    let Some(this) = weak.upgrade() else {
                        return next.proceed(None).await;
                    };
                    let replayed = this
                        .state
                        .get(&environment_binding_replayable_key().state_key());
                    if replayed.is_null() {
                        let current = this.current();
                        if let Err(error) = this
                            .dispatcher
                            .dispatch(
                                EnvironmentSetBinding {
                                    agent_id: this.scope_context.agent_id(),
                                    environment_id: current.environment_id,
                                    cwd: current.cwd,
                                }
                                .into(),
                            )
                            .await
                        {
                            panic!("{error}");
                        }
                    }
                    let current = this.current();
                    this.apply_session_work_dir(&current);
                    let should_remind = match replayed.as_ref() {
                        None => current.environment_id != LOCAL_ENVIRONMENT_ID,
                        Some(replayed) => this.is_seed_round_trip(replayed),
                    };
                    if should_remind {
                        this.emit_environment_reminder(&current);
                    }
                    next.proceed(None).await
                })
            }),
            Default::default(),
        )
        .unwrap_or_else(|error| panic!("{error}"));
        *inner.restore_hook.lock() = Some(registration);
        Self { inner }
    }

    pub fn current(&self) -> EnvironmentBinding {
        self.inner.current()
    }

    pub fn on_did_change(&self) -> Event<EnvironmentBinding> {
        self.inner.change_emitter.event()
    }

    pub fn bind(
        &self,
        environment_id: &str,
        cwd: Option<String>,
    ) -> Result<EnvironmentBinding, WireAsyncError> {
        self.inner.bind(environment_id, cwd)
    }

    pub fn dispose(&self) {
        self.inner.dispose();
    }
}

impl IAgentEnvironmentBindingService for AgentEnvironmentBindingService {
    fn current(&self) -> EnvironmentBinding {
        self.inner.current()
    }

    fn on_did_change(&self) -> Event<EnvironmentBinding> {
        self.inner.change_emitter.event()
    }

    fn bind(
        &self,
        environment_id: &str,
        cwd: Option<String>,
    ) -> Result<EnvironmentBinding, WireAsyncError> {
        self.inner.bind(environment_id, cwd)
    }
}

impl IDisposable for AgentEnvironmentBindingService {
    fn dispose(&self) {
        self.inner.dispose();
    }
}
