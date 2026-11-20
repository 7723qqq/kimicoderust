use crate::_base::di::instantiation::ServicesAccessorExt;
use crate::agent::context_memory::context_memory::I_AGENT_CONTEXT_MEMORY_SERVICE;
use crate::agent::environment_binding::environment_binding::I_AGENT_ENVIRONMENT_BINDING_SERVICE;
use crate::agent::full_compaction::full_compaction::I_AGENT_FULL_COMPACTION_SERVICE;
use crate::agent::r#loop::r#loop::{I_AGENT_LOOP_SERVICE, LoopCancelTarget, LoopState};
use crate::agent::mcp::mcp::I_AGENT_MCP_SERVICE;
use crate::agent::permission_mode::permission_mode::I_AGENT_PERMISSION_MODE_SERVICE;
use crate::agent::permission_rules::permission_rules::I_AGENT_PERMISSION_RULES_SERVICE;
use crate::agent::profile::profile::I_AGENT_PROFILE_SERVICE;
use crate::agent::task::task::I_AGENT_TASK_SERVICE;
use crate::agent::tool_activation::tool_activation::I_AGENT_TOOL_ACTIVATION_SERVICE;
use crate::state::event_dispatcher::I_EVENT_DISPATCHER;
use crate::wire::wire::I_WIRE_SERVICE;
use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::future::{BoxFuture, Shared};
use futures_util::FutureExt;
use kimi_core::primitives::errors::{Error2, Error2Options};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};

use crate::_base::di::instantiation::{IInstantiationService, create_decorator};
use crate::agent::blob::agent_blob_service::I_AGENT_BLOB_SERVICE;
use crate::agent::scope_context::scope_context::I_AGENT_SCOPE_CONTEXT;
use crate::app::telemetry::telemetry::I_TELEMETRY_SERVICE;
use crate::_base::di::lifecycle::{DisposableRef, IDisposable, to_disposable};
use crate::_base::di::scope::{
    IAgentScopeHandle, LifecycleScope, ScopeActivation, ScopeSeed, create_scoped_child_handle,
    register_scoped_service, seed_instance, ScopeOptions,
};
use crate::_base::errors::unexpected_error::on_unexpected_error;
use crate::_base::log::log::ILogService;
use crate::_base::utils::abort::{AbortError, AbortSignal, abort_error};
use crate::_base::utils::paths::join;
use crate::agent::agent_context::agent_context::AgentContext;
use crate::agent::blob::agent_blob_service::IAgentBlobService;
use crate::agent::blob::agent_blob_service_impl::AgentBlobServiceImpl;
use crate::agent::context_memory::context_memory::IAgentContextMemoryService;
use crate::agent::context_memory::open_tool_exchange::close_trailing_open_tool_exchange;
use crate::agent::environment_binding::environment_binding::{
    IAgentEnvironmentBindingSeed, IAgentEnvironmentBindingService,
};
use crate::agent::environment_binding::environment_binding_ops::EnvironmentSetBinding;
use crate::agent::full_compaction::full_compaction::IAgentFullCompactionService;
use crate::agent::interaction::interaction_wiring::{
    attach_interaction_agent, cancel_interactions_for_turn, detach_interaction_agent,
};
use crate::agent::r#loop::r#loop::IAgentLoopService;
use crate::agent::registrations::ensure_agent_services_registered;
use crate::agent::r#loop::machine::engine::{MACHINE_LOOP_MODEL, MachineEngineAttachRef};
use crate::agent::r#loop::turn_ops::TurnEnded;
use crate::agent::mcp::mcp::IAgentMcpService;
use crate::agent::permission_mode::config_section::DEFAULT_PERMISSION_MODE_SECTION;
use crate::agent::permission_mode::permission_mode::IAgentPermissionModeService;
use crate::agent::permission_mode::permission_mode_ops::permission_mode_configured_key;
use crate::agent::permission_policy::types::PermissionMode;
use crate::agent::permission_rules::permission_rules::IAgentPermissionRulesService;
use crate::agent::profile::profile::IAgentProfileService;
use crate::agent::profile::profile_ops::{ProfileModelState, profile_replayable_key};
use crate::agent::scope_context::scope_context::{
    IAgentScopeContext, agent_context_of, make_agent_scope_context,
};
use crate::agent::state::agent_state::I_AGENT_STATE_SERVICE;
use crate::agent::task::task::IAgentTaskService;
use crate::agent::tool_activation::tool_activation::IAgentToolActivationService;
use crate::app::bootstrap::bootstrap::IBootstrapService;
use crate::app::config::config::IConfigService;
use crate::app::event::event_bus::ISessionEventBus;
use crate::app::telemetry::telemetry::ITelemetryService;
use crate::app::telemetry::telemetry_service::bind_telemetry_scope;
use crate::environment::environment::{EnvironmentBinding, LOCAL_ENVIRONMENT_ID};
use crate::features::tower::tower::has_pinned_permission_mode;
use crate::human::agent::machine::{
    AgentEmitted, AgentInput, AgentMachineSelf, AgentScopeHandle, ScopeFactoryOutput,
    create_agent_machine,
};
use crate::human::interaction::facade::interactions;
use crate::human::llm::requester::requester::LlmRequestConfig;
use crate::human::session::machine::{
    AgentActorRef, SessionEmitted, SessionEvent, SessionInput, SessionMachineContext,
    create_session_machine, session_emitted_of,
};
use crate::human::xstate2::{XEvent, create_actor, set_root_actor_error_reporter, wait_for};
use crate::persistence::interface::append_log_store::IAppendLogStore;
use crate::persistence::interface::blob_store::IBlobStore;
use crate::persistence::interface::session_lock::ISessionLockService;
use crate::persistence::interface::storage::IFileSystemStorageService;
use crate::session::session_context::session_context::ISessionContext;
use crate::session::session_metadata::session_metadata::{
    AgentMeta, AgentType, ISessionMetadata,
};
use crate::session::workdir::workdir::ISessionWorkdir;
use crate::state::event_dispatcher::IEventDispatcher;
use crate::wire::record::WireAsyncError;
use crate::wire::wire::IWireService;
use crate::wire::wire_service::WireService;

use super::agent_lifecycle::{
    AgentListFilter, AgentScopeCreatedEvent, CreateAgentOptions, ForkAgentOptions,
    I_AGENT_LIFECYCLE_SERVICE, IAgentLifecycleService, MAIN_AGENT_ID,
};
use super::errors::{AGENT_ALREADY_EXISTS, AGENT_NOT_FOUND};
use super::managed_agent::ManagedAgent;
use super::subagent_metadata::with_subagent_profile;

static NEXT_AGENT_ID: AtomicI64 = AtomicI64::new(0);

pub const REMOVE_PROMPT_QUIESCE_TIMEOUT_MS: u64 = 3_000;
pub const REMOVE_PROMPT_QUIESCE_POLL_MS: u64 = 10;

#[derive(Debug, Clone)]
struct SharedAgentError(Arc<String>);

impl SharedAgentError {
    fn new(error: WireAsyncError) -> Self {
        Self(Arc::new(error.to_string()))
    }

    fn boxed(self) -> WireAsyncError {
        Box::new(self)
    }
}

impl fmt::Display for SharedAgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.as_str())
    }
}

impl Error for SharedAgentError {}

type AgentFuture = Shared<BoxFuture<'static, Result<Arc<AgentContext>, SharedAgentError>>>;

struct ListenerRegistry<T> {
    next_id: AtomicU64,
    listeners: Mutex<Vec<(u64, Arc<dyn Fn(T) + Send + Sync>)>>,
}

impl<T> Default for ListenerRegistry<T> {
    fn default() -> Self {
        Self {
            next_id: AtomicU64::new(0),
            listeners: Mutex::new(Vec::new()),
        }
    }
}

impl<T: Clone + 'static> ListenerRegistry<T> {
    fn subscribe_in(
        self: &Arc<Self>,
        listener: Arc<dyn Fn(T) + Send + Sync>,
    ) -> DisposableRef {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.listeners.lock().push((id, listener));
        let this = Arc::downgrade(self);
        to_disposable(move || {
            if let Some(this) = this.upgrade() {
                this.listeners.lock().retain(|(candidate, _)| *candidate != id);
            }
        })
    }

    fn fire(&self, event: T) {
        let listeners = self.listeners.lock().clone();
        for (_, listener) in listeners {
            listener(event.clone());
        }
    }
}

pub struct AgentLifecycleService {
    instantiation: Arc<dyn IInstantiationService>,
    ctx: Arc<dyn ISessionContext>,
    workdir: Arc<dyn ISessionWorkdir>,
    session_metadata: Arc<dyn ISessionMetadata>,
    bootstrap: Arc<dyn IBootstrapService>,
    config: Arc<dyn IConfigService>,
    telemetry: Arc<dyn ITelemetryService>,
    bus: Arc<dyn ISessionEventBus>,
    append_log_store: Arc<dyn IAppendLogStore>,
    session_locks: Arc<dyn ISessionLockService>,
    blob_store: Arc<dyn IBlobStore>,
    storage: Arc<dyn IFileSystemStorageService>,
    logger: Arc<dyn ILogService>,
    roster: Arc<Mutex<HashMap<String, Arc<ManagedAgent>>>>,
    creating: Arc<Mutex<HashMap<String, AgentFuture>>>,
    next_lifecycle_generation: AtomicU64,
    session_actor: crate::human::session::machine::AgentActorRef,
    did_create: Arc<ListenerRegistry<Arc<AgentContext>>>,
    did_create_scope: Arc<ListenerRegistry<AgentScopeCreatedEvent>>,
    will_close: Arc<ListenerRegistry<Arc<AgentContext>>>,
    did_close: Arc<ListenerRegistry<Arc<AgentContext>>>,
    subscriptions: Mutex<Vec<DisposableRef>>,
}

impl AgentLifecycleService {
    pub fn new(
        instantiation: Arc<dyn IInstantiationService>,
        ctx: Arc<dyn ISessionContext>,
        workdir: Arc<dyn ISessionWorkdir>,
        session_metadata: Arc<dyn ISessionMetadata>,
        bootstrap: Arc<dyn IBootstrapService>,
        config: Arc<dyn IConfigService>,
        telemetry: Arc<dyn ITelemetryService>,
        bus: Arc<dyn ISessionEventBus>,
        append_log_store: Arc<dyn IAppendLogStore>,
        session_locks: Arc<dyn ISessionLockService>,
        blob_store: Arc<dyn IBlobStore>,
        storage: Arc<dyn IFileSystemStorageService>,
        logger: Arc<dyn ILogService>,
    ) -> Arc<Self> {
        let reporter = logger.clone();
        set_root_actor_error_reporter(Arc::new(move |error| {
            reporter.error("root actor stopped on aborted operation", Some(error));
        }));
        let session_actor = create_actor(
            create_session_machine().machine().clone(),
            Arc::new(SessionInput {
                request: LlmRequestConfig { model: MACHINE_LOOP_MODEL.clone(), ..Default::default() },
            }),
        );
        session_actor.start();
        let this = Arc::new(Self {
            instantiation,
            ctx,
            workdir,
            session_metadata,
            bootstrap,
            config,
            telemetry,
            bus,
            append_log_store,
            session_locks,
            blob_store,
            storage,
            logger,
            roster: Arc::new(Mutex::new(HashMap::new())),
            creating: Arc::new(Mutex::new(HashMap::new())),
            next_lifecycle_generation: AtomicU64::new(0),
            session_actor,
            did_create: Arc::new(ListenerRegistry::default()),
            did_create_scope: Arc::new(ListenerRegistry::default()),
            will_close: Arc::new(ListenerRegistry::default()),
            did_close: Arc::new(ListenerRegistry::default()),
            subscriptions: Mutex::new(Vec::new()),
        });
        let weak = Arc::downgrade(&this);
        let session_subscription = this.session_actor.on(
            "agent.restarted",
            Arc::new(move |event: &XEvent| {
                let Some(SessionEmitted::AgentRestarted { agent_id, ref_ }) =
                    session_emitted_of(event)
                else {
                    return;
                };
                if let Some(this) = weak.upgrade()
                    && let Some(managed) = this.roster.lock().get(&agent_id)
                {
                    *managed.ref_.lock() = Some(ref_);
                }
            }),
        );
        this.subscriptions
            .lock()
            .push(Arc::new(SessionSubscription(session_subscription)));
        let session_id = this.ctx.session_id();
        this.subscriptions.lock().push(this.bus.subscribe_turn_ended(
            Arc::new(move |event: TurnEnded| {
                cancel_interactions_for_turn(&event.agent_id, &session_id, event.turn_id);
            }),
        ));
        let session_id = this.ctx.session_id();
        let subscription = this.did_close.subscribe_in(Arc::new(move |context| {
            detach_interaction_agent(&context.agent_id, &session_id);
        }));
        this.subscriptions.lock().push(subscription);
        this
    }

    pub async fn create(
        self: &Arc<Self>,
        opts: Option<CreateAgentOptions>,
    ) -> Result<Arc<AgentContext>, WireAsyncError> {
        let opts = opts.unwrap_or_default();
        if let Some(agent_id) = opts.agent_id.as_deref() {
            let inflight = self.creating.lock().get(agent_id).cloned();
            if let Some(inflight) = inflight {
                return inflight.await.map_err(SharedAgentError::boxed);
            }
            if let Some(existing) = self.roster.lock().get(agent_id)
                && !existing.closing.load(Ordering::Acquire)
            {
                return Ok(existing.context.clone());
            }
        }
        let agent_id = match opts.agent_id.clone() {
            Some(agent_id) => agent_id,
            None => self.next_available_agent_id().await?,
        };
        let this = self.clone();
        let create_opts = opts.clone();
        let create_id = agent_id.clone();
        let future = async move {
            this.do_create(create_id, create_opts)
                .await
                .map_err(SharedAgentError::new)
        }
        .boxed()
        .shared();
        self.creating.lock().insert(agent_id.clone(), future.clone());
        let result = future.await.map_err(SharedAgentError::boxed);
        self.creating.lock().remove(&agent_id);
        result
    }

    async fn next_available_agent_id(&self) -> Result<String, WireAsyncError> {
        let mut max_suffix = -1_i64;
        let mut consider = |id: &str| {
            if let Some(suffix) = id.strip_prefix("agent-")
                && let Ok(suffix) = suffix.parse::<i64>()
            {
                max_suffix = max_suffix.max(suffix);
            }
        };
        for id in self.roster.lock().keys() {
            consider(id);
        }
        let persisted = self.session_metadata.read().await?.agents.unwrap_or_default();
        for id in persisted.keys() {
            consider(id);
        }
        let candidate = (max_suffix + 1).max(NEXT_AGENT_ID.load(Ordering::Relaxed));
        NEXT_AGENT_ID.store(candidate + 1, Ordering::Relaxed);
        Ok(format!("agent-{candidate}"))
    }

    fn do_create(
        self: &Arc<Self>,
        agent_id: String,
        opts: CreateAgentOptions,
    ) -> BoxFuture<'static, Result<Arc<AgentContext>, WireAsyncError>> {
        if session_context(&self.session_actor)
            .agents
            .contains_key(&agent_id)
        {
            return Box::pin(std::future::ready(Err(agent_error(
                AGENT_ALREADY_EXISTS,
                format!("Agent \"{agent_id}\" already exists"),
                &agent_id,
            ))));
        }
        let this = self.clone();
        let scope_agent_id = agent_id.clone();
        let input = AgentInput {
            request: LlmRequestConfig { model: MACHINE_LOOP_MODEL.clone(), ..Default::default() },
            store: None,
            session: Some(json!({
                "sessionId": self.ctx.session_id(),
                "workspaceId": self.ctx.workspace_id(),
            })),
            scope_factory: Arc::new(move |self_, signal| {
                this.build_agent_scope(scope_agent_id.clone(), opts.clone(), self_, crate::_base::utils::abort::AbortSignal::from_cancellation_token(signal))
            }),
        };
        self.session_actor.send(
            SessionEvent::AgentCreate {
                agent_id: Some(agent_id.clone()),
                logic: create_agent_machine(Default::default()),
                input,
            }
            .into(),
        );
        let context = session_context(&self.session_actor);
        let Some(entry) = context.agents.get(&agent_id) else {
            return Box::pin(std::future::ready(Err(format!(
                "Agent \"{agent_id}\" was not spawned by the session actor"
            )
            .into())));
        };
        let ref_ = entry.ref_.clone();
        if let Some(managed) = self.roster.lock().get(&agent_id) {
            *managed.ref_.lock() = Some(ref_.clone());
        }
        self.await_linked(agent_id, ref_)
    }

    fn await_linked(
        self: &Arc<Self>,
        agent_id: String,
        ref_: AgentActorRef,
    ) -> BoxFuture<'static, Result<Arc<AgentContext>, WireAsyncError>> {
        let this = self.clone();
        Box::pin(async move {
            let failure = Arc::new(Mutex::new(None::<String>));
            let failure_slot = failure.clone();
            let subscription = AgentMachineSelf::new(ref_.clone()).on(
                "agent.failed",
                Arc::new(move |event| {
                    let AgentEmitted::AgentFailed { error } = event else {
                        return;
                    };
                    *failure_slot.lock() = Some(error.to_string());
                }),
            );
            let waited = wait_for(&ref_, |snapshot| snapshot.value != "linking").await;
            subscription.unsubscribe();
            if let Some(error) = failure.lock().clone() {
                return Err(error.into());
            }
            if let Err(error) = waited {
                return Err(error);
            }
            let managed = this.roster.lock().get(&agent_id).cloned();
            match managed {
                Some(managed) => Ok(managed.context.clone()),
                None => Err(Box::new(AbortError::new(format!(
                    "Agent \"{agent_id}\" linking was cancelled"
                ))) as crate::wire::record::WireAsyncError),
            }
        })
    }

    fn build_agent_scope(
        self: &Arc<Self>,
        agent_id: String,
        opts: CreateAgentOptions,
        self_: AgentMachineSelf,
        signal: AbortSignal,
    ) -> BoxFuture<'static, Result<ScopeFactoryOutput, WireAsyncError>> {
        let this = self.clone();
        Box::pin(async move {
            let agent_scope = this.ctx.scope(Some(&format!("agents/{agent_id}")));
            let agent_homedir = join(&this.bootstrap.home_dir(), &[agent_scope.clone()]);
            let generation = this.next_lifecycle_generation.fetch_add(1, Ordering::AcqRel) + 1;
            let scope_context = make_agent_scope_context(
                agent_id.clone(),
                agent_scope.clone(),
                opts.forked_from.clone(),
                generation as u32,
            );
            let agent = scope_context.agent_context.clone();
            let event_bus = this.bus.clone();
            event_bus.activate_agent(agent.clone());
            let telemetry_binding = bind_telemetry_scope(
                this.telemetry.clone(),
                json!({ "agent_id": agent_id, "mode": "agent" }),
            );
            let blob_view = Arc::new(AgentBlobServiceImpl::new(
                this.blob_store.clone(),
                agent_scope,
            ));
            let wire = Arc::new(WireService::new(
                scope_context.clone(),
                this.append_log_store.clone(),
                blob_view.clone(),
                this.storage.clone(),
                this.logger.clone(),
                telemetry_binding.telemetry.clone(),
                this.session_locks.clone(),
                this.ctx.clone(),
            ));
            let finalizer_armed = Arc::new(AtomicBool::new(false));
            let armed = finalizer_armed.clone();
            let telemetry_dispose = telemetry_binding.clone();
            let wire_dispose = wire.clone();
            let event_dispose = event_bus.clone();
            let agent_dispose = agent.clone();
            let mut seeds = seed_instance(
                &I_AGENT_SCOPE_CONTEXT,
                Arc::new(scope_context.clone()),
            );
            seeds.extend(seed_instance(
                &I_TELEMETRY_SERVICE,
                Arc::new(telemetry_binding.telemetry.clone()),
            ));
            let env_binding_id = create_decorator::<Arc<dyn IAgentEnvironmentBindingSeed>>(
                "agentEnvironmentBindingSeed",
            );
            let env_binding_seed: Arc<dyn IAgentEnvironmentBindingSeed> =
                Arc::new(environment_binding_seed(&this, &opts));
            seeds.extend(seed_instance(&env_binding_id, Arc::new(env_binding_seed)));
            seeds.extend(seed_instance(&I_AGENT_BLOB_SERVICE, Arc::new(blob_view)));
            seeds.extend(seed_instance(&I_WIRE_SERVICE, Arc::new(wire.clone())));
            ensure_agent_services_registered()
                .map_err(|error| -> WireAsyncError { error.into() })?;
            let parent = this
                .instantiation
                .concrete()
                .ok_or_else(|| -> WireAsyncError { "agent lifecycle requires a concrete instantiation service".into() })?;
            let handle = create_scoped_child_handle(
                parent,
                LifecycleScope::Agent.as_str().to_string(),
                agent_id.clone(),
                ScopeOptions {
                    seeds: Some(seeds),
                    configure_container: Some(Arc::new(move |container| {
                        let telemetry_dispose = telemetry_dispose.clone();
                        container.anchor_kernel_entry(
                            Arc::new(move |_reason| {
                                telemetry_dispose.dispose();
                                None
                            }),
                            "telemetry:agent-context",
                        );
                        let wire_dispose = wire_dispose.clone();
                        container.anchor_kernel_entry(
                            Arc::new(move |_reason| {
                                wire_dispose.dispose();
                                None
                            }),
                            "wire-view-dispose",
                        );
                        let event_dispose = event_dispose.clone();
                        let agent_dispose = agent_dispose.clone();
                        container.anchor_kernel_finalizer(Arc::new(move |_reason| {
                            event_dispose.deactivate_agent(agent_dispose.clone());
                            None
                        }), "agent:event-deactivate");
                        armed.store(true, Ordering::Release);
                        Ok(())
                    })),
                    ..Default::default()
                },
            );
            let handle = match handle {
                Ok(handle) => handle,
                Err(error) => {
                    wire.dispose();
                    telemetry_binding.dispose();
                    event_bus.deactivate_agent(agent.clone());
                    return Err(error.into());
                }
            };
            let dispose_handle = handle.clone();
            signal.add_event_listener(
                "abort",
                Arc::new(move || {
                    let handle = dispose_handle.clone();
                    tokio::spawn(async move {
                        let _ = handle.dispose().await;
                    });
                }),
                true,
            );
            let managed = match this.roster_adopt(handle.clone()) {
                Ok(_) => this.roster.lock().get(&agent_id).cloned(),
                Err(error) => {
                    let _ = handle.dispose().await;
                    return Err(error);
                }
            };
            let result = this
                .initialize_agent_scope(
                    agent_id.clone(),
                    opts,
                    agent.clone(),
                    handle.clone(),
                    agent_homedir,
                    self_,
                )
                .await;
            match result {
                Ok(bundle) => {
                    if let Some(managed) = managed {
                        *managed.bundle.lock() = Some(bundle.clone());
                    }
                    let dispose = handle.clone();
                    Ok(ScopeFactoryOutput {
                        handle: Some(AgentScopeHandle::new(move || {
                            let handle = dispose.clone();
                            Box::pin(async move { Ok(handle.dispose().await) }) as BoxFuture<'static, Result<(), WireAsyncError>>
                        })),
                        store: bundle.store.clone(),
                        turn_logic: bundle.turn_logic.clone(),
                        tool_logic: bundle.tool_logic.clone(),
                        tools: bundle.tools.clone(),
                        request: bundle.request.clone(),
                        prompt_gate: bundle.prompt_gate.clone(),
                    })
                }
                Err(error) => {
                    this.telemetry.track2(
                        "agent_create_failed",
                        Some(
                            json!({
                                "agent_id": agent_id,
                                "stage": "bootstrap",
                                "error_type": error_name(&error),
                            })
                            .as_object()
                            .cloned()
                            .unwrap_or_default(),
                        ),
                    );
                    if let Some(managed) = managed {
                        managed.closing.store(true, Ordering::Release);
                        this.roster.lock().remove(&agent_id);
                        managed.kill_space();
                        let _ = managed.handle.dispose().await;
                    } else {
                        let _ = handle.dispose().await;
                    }
                    if !finalizer_armed.load(Ordering::Acquire) {
                        event_bus.deactivate_agent(agent.clone());
                    }
                    this.did_close.fire(agent);
                    Err(error)
                }
            }
        })
    }

    async fn initialize_agent_scope(
        &self,
        agent_id: String,
        opts: CreateAgentOptions,
        agent: Arc<AgentContext>,
        handle: Arc<IAgentScopeHandle>,
        agent_homedir: String,
        self_: AgentMachineSelf,
    ) -> Result<crate::agent::r#loop::machine::engine::MachineEngineAttachBundle, WireAsyncError> {
        handle.accessor().get(&I_WIRE_SERVICE).seal().await?;
        let labels = with_subagent_profile(
            opts.labels.clone(),
            (agent_id != MAIN_AGENT_ID)
                .then(|| opts.binding.as_ref().map(|binding| binding.profile.clone()))
                .flatten(),
        );
        let meta = AgentMeta {
            homedir: Some(agent_homedir),
            type_: Some(if agent_id == MAIN_AGENT_ID {
                AgentType::Main
            } else {
                AgentType::Sub
            }),
            parent_agent_id: (agent_id != MAIN_AGENT_ID)
                .then_some(Some(MAIN_AGENT_ID.to_string())),
            forked_from: opts.forked_from.clone(),
            labels,
            swarm_item: None,
        };
        self.session_metadata.register_agent(&agent_id, meta.clone()).await?;
        self.did_create.fire(agent.clone());
        self.did_create_scope.fire(AgentScopeCreatedEvent {
            context: agent.clone(),
            handle: handle.clone(),
            meta: Some(meta),
        });
        if let Some(environment_id) = opts.environment_id.as_deref()
            && environment_id != LOCAL_ENVIRONMENT_ID
        {
            handle
                .accessor()
                .get(&I_EVENT_DISPATCHER)
                .dispatch(EnvironmentSetBinding {
                    agent_id: agent_id.clone(),
                    environment_id: environment_id.to_string(),
                    cwd: opts.environment_cwd.clone(),
                }.to_event2())
                .await?;
        }
        handle.accessor().get(&I_AGENT_PROFILE_SERVICE);
        handle.accessor().get(&I_AGENT_PERMISSION_RULES_SERVICE);
        handle.accessor().get(&I_AGENT_PERMISSION_MODE_SERVICE);
        handle.accessor().get(&I_AGENT_MCP_SERVICE);
        handle.accessor().get(&I_EVENT_DISPATCHER).restore().await?;
        attach_interaction_agent(
            &agent_id,
            &self.ctx.session_id(),
            handle.accessor().get(&I_EVENT_DISPATCHER),
        );
        self.bind_bootstrap(&handle, &opts).await?;
        handle.accessor().get(&I_AGENT_TOOL_ACTIVATION_SERVICE).activate().await;
        let loop_ = handle.accessor().get(&I_AGENT_LOOP_SERVICE);
        let bundle = loop_.build_attach_bundle();
        loop_.attach_engine(self_ as MachineEngineAttachRef, bundle.clone());
        Ok(bundle)
    }

    async fn bind_bootstrap(
        &self,
        handle: &Arc<IAgentScopeHandle>,
        opts: &CreateAgentOptions,
    ) -> Result<(), WireAsyncError> {
        if let Some(binding) = opts.binding.clone() {
            handle.accessor().get(&I_AGENT_PROFILE_SERVICE).bind(binding).await?;
            handle.accessor().get(&I_AGENT_MCP_SERVICE).register();
        }
        let permission_mode = self
            .config
            .get(DEFAULT_PERMISSION_MODE_SECTION)
            .and_then(|value| serde_json::from_value::<PermissionMode>(value).ok());
        let restored = handle
            .accessor()
            .get(&I_AGENT_STATE_SERVICE)
            .get(&permission_mode_configured_key().state_key())
            .as_bool()
            .unwrap_or(false);
        if let Some(permission_mode) = permission_mode
            && !restored
        {
            handle
                .accessor()
                .get(&I_AGENT_PERMISSION_MODE_SERVICE)
                .set_mode(permission_mode);
        }
        Ok(())
    }

    pub async fn fork(
        self: &Arc<Self>,
        source_context: Arc<AgentContext>,
        opts: Option<ForkAgentOptions>,
    ) -> Result<Arc<AgentContext>, WireAsyncError> {
        let source_managed = self.managed_for(&source_context).ok_or_else(|| {
            agent_error(
                AGENT_NOT_FOUND,
                format!("Source agent \"{}\" does not exist", source_context.agent_id),
                &source_context.agent_id,
            )
        })?;
        if let Some(agent_id) = opts.as_ref().and_then(|opts| opts.agent_id.as_deref())
            && self.get(agent_id).is_some()
        {
            return Err(agent_error(
                AGENT_ALREADY_EXISTS,
                format!("Agent \"{agent_id}\" already exists"),
                agent_id,
            ));
        }
        let source = source_managed.handle.clone();
        let source_data = source.accessor().get(&I_AGENT_PROFILE_SERVICE).data();
        let source_binding = source
            .accessor()
            .get(&I_AGENT_ENVIRONMENT_BINDING_SERVICE)
            .current();
        let override_ = opts.as_ref().and_then(|opts| opts.binding.clone());
        let labels = with_subagent_profile(
            opts.as_ref().and_then(|opts| opts.labels.clone()),
            override_
                .as_ref()
                .and_then(|binding| binding.profile.clone())
                .or_else(|| source_data.base.profile_name.clone()),
        );
        let child_context = self
            .create(Some(CreateAgentOptions {
                agent_id: opts.as_ref().and_then(|opts| opts.agent_id.clone()),
                binding: None,
                environment_id: Some(source_binding.environment_id),
                environment_cwd: source_binding.cwd,
                forked_from: Some(source.id().to_string()),
                labels,
            }))
            .await?;
        let child = self.require_managed(&child_context)?.handle.clone();
        let child_profile = child.accessor().get(&I_AGENT_PROFILE_SERVICE);
        if let Some(profile) = override_.as_ref().and_then(|binding| binding.profile.clone()) {
            child_profile
                .bind(crate::agent::profile::profile::BindAgentInput {
                    profile,
                    model: override_
                        .as_ref()
                        .and_then(|binding| binding.model.clone())
                        .or(source_data.base.model_alias.clone()),
                    thinking: override_
                        .as_ref()
                        .and_then(|binding| binding.thinking.clone())
                        .or_else(|| Some(source_data.base.thinking_level.clone())),
                    strict_thinking: None,
                })
                .await?;
        } else {
            child_profile.apply_binding_snapshot(
                crate::agent::profile::profile_service::profile_binding_snapshot(source_data.clone()),
            );
            if let Some(model) = override_.as_ref().and_then(|binding| binding.model.clone()) {
                child_profile.set_model(&model).await?;
            }
            if let Some(thinking) = override_.as_ref().and_then(|binding| binding.thinking.clone()) {
                child_profile.set_thinking(&thinking);
            }
        }
        let source_messages = source.accessor().get(&I_AGENT_CONTEXT_MEMORY_SERVICE).get();
        if !source_messages.is_empty() {
            child
                .accessor()
                .get(&I_AGENT_CONTEXT_MEMORY_SERVICE)
                .append(&close_trailing_open_tool_exchange(&source_messages));
        }
        Ok(child_context)
    }

    pub fn get(&self, agent_id: &str) -> Option<Arc<AgentContext>> {
        self.roster.lock().get(agent_id).and_then(|managed| {
            (!managed.closing.load(Ordering::Acquire) && managed.active.load(Ordering::Acquire))
                .then(|| managed.context.clone())
        })
    }

    pub fn list(&self, filter: Option<AgentListFilter>) -> Vec<Arc<AgentContext>> {
        let prefix = filter.and_then(|filter| filter.prefix);
        self.roster
            .lock()
            .values()
            .filter(|managed| {
                managed.active.load(Ordering::Acquire) && !managed.closing.load(Ordering::Acquire)
            })
            .map(|managed| managed.context.clone())
            .filter(|context| {
                prefix
                    .as_deref()
                    .is_none_or(|prefix| context.agent_id.starts_with(prefix))
            })
            .collect()
    }

    pub fn broadcast_permission_mode(&self, mode: PermissionMode) {
        for managed in self.roster.lock().values() {
            if managed.closing.load(Ordering::Acquire) || !managed.active.load(Ordering::Acquire) {
                continue;
            }
            let handle = managed.handle.clone();
            let profile = serde_json::from_value::<ProfileModelState>(
                handle
                    .accessor()
                    .get(&I_AGENT_STATE_SERVICE)
                    .get(&profile_replayable_key().state_key()),
            )
            .unwrap_or_else(|error| panic!("{error}"));
            if has_pinned_permission_mode(profile.profile_name.as_deref()) {
                continue;
            }
            handle
                .accessor()
                .get(&I_AGENT_PERMISSION_MODE_SERVICE)
                .set_mode(mode);
        }
    }

    pub fn handle_of(&self, agent_id: &str) -> Option<Arc<IAgentScopeHandle>> {
        self.roster.lock().get(agent_id).and_then(|managed| {
            (!managed.closing.load(Ordering::Acquire) && managed.active.load(Ordering::Acquire))
                .then(|| managed.handle.clone())
        })
    }

    pub fn adopt(
        self: &Arc<Self>,
        handle: Arc<IAgentScopeHandle>,
    ) -> Result<Arc<AgentContext>, WireAsyncError> {
        let agent = agent_context_of(&handle);
        let agent_id = agent.agent_id.clone();
        if let Some(existing) = self.roster.lock().get(&agent_id) {
            if !existing.closing.load(Ordering::Acquire)
                && Arc::ptr_eq(&existing.context, &agent)
            {
                return Ok(existing.context.clone());
            }
            if !existing.closing.load(Ordering::Acquire) {
                return Err(format!(
                    "Agent \"{agent_id}\" is already managed by a different context"
                )
                .into());
            }
        }
        let this = self.clone();
        let scope_agent = agent.clone();
        let scope_handle = handle.clone();
        self.session_actor.send(
            SessionEvent::AgentCreate {
                agent_id: Some(agent_id.clone()),
                logic: create_agent_machine(Default::default()),
                input: AgentInput {
                    request: LlmRequestConfig { model: MACHINE_LOOP_MODEL.clone(), ..Default::default() },
                    store: None,
                    session: Some(json!({
                        "sessionId": self.ctx.session_id(),
                        "workspaceId": self.ctx.workspace_id(),
                    })),
                    scope_factory: Arc::new(move |self_, signal| {
                        this.adopt_agent_scope(
                            scope_agent.clone(),
                            scope_handle.clone(),
                            self_,
                            crate::_base::utils::abort::AbortSignal::from_cancellation_token(signal),
                        )
                    }),
                },
            }
            .into(),
        );
        if let Some(managed) = self.roster.lock().get(&agent_id)
            && let Some(entry) = session_context(&self.session_actor).agents.get(&agent_id)
        {
            *managed.ref_.lock() = Some(entry.ref_.clone());
        }
        Ok(agent)
    }

    fn adopt_agent_scope(
        self: &Arc<Self>,
        agent: Arc<AgentContext>,
        handle: Arc<IAgentScopeHandle>,
        self_: AgentMachineSelf,
        signal: AbortSignal,
    ) -> BoxFuture<'static, Result<ScopeFactoryOutput, WireAsyncError>> {
        let this = self.clone();
        Box::pin(async move {
            let result = async {
                this.roster_adopt(handle.clone())?;
                let managed = this.roster.lock().get(&agent.agent_id).cloned();
                let dispose_handle = handle.clone();
                signal.add_event_listener(
                    "abort",
                    Arc::new(move || {
                        let handle = dispose_handle.clone();
                        tokio::spawn(async move {
                            let _ = handle.dispose().await;
                        });
                    }),
                    true,
                );
                let loop_ = handle.accessor().get(&I_AGENT_LOOP_SERVICE);
                let bundle = loop_.build_attach_bundle();
                loop_.attach_engine(self_ as MachineEngineAttachRef, bundle.clone());
                if let Some(managed) = managed {
                    *managed.bundle.lock() = Some(bundle.clone());
                }
                this.did_create.fire(agent.clone());
                this.did_create_scope.fire(AgentScopeCreatedEvent {
                    context: agent.clone(),
                    handle: handle.clone(),
                    meta: None,
                });
                attach_interaction_agent(
                    &agent.agent_id,
                    &this.ctx.session_id(),
                    handle.accessor().get(&I_EVENT_DISPATCHER),
                );
                Ok(bundle)
            }
            .await;
            match result {
                Ok(bundle) => {
                    let dispose = handle.clone();
                    Ok(ScopeFactoryOutput {
                        handle: Some(AgentScopeHandle::new(move || {
                            let handle = dispose.clone();
                            Box::pin(async move { Ok(handle.dispose().await) }) as BoxFuture<'static, Result<(), WireAsyncError>>
                        })),
                        store: bundle.store.clone(),
                        turn_logic: bundle.turn_logic.clone(),
                        tool_logic: bundle.tool_logic.clone(),
                        tools: bundle.tools.clone(),
                        request: bundle.request.clone(),
                        prompt_gate: bundle.prompt_gate.clone(),
                    })
                }
                Err(error) => {
                    if let Some(managed) = this.roster.lock().get(&agent.agent_id).cloned()
                        && Arc::ptr_eq(&managed.context, &agent)
                    {
                        managed.closing.store(true, Ordering::Release);
                        this.roster.lock().remove(&agent.agent_id);
                        managed.kill_space();
                    }
                    Err(error)
                }
            }
        })
    }

    fn roster_adopt(
        &self,
        handle: Arc<IAgentScopeHandle>,
    ) -> Result<Arc<AgentContext>, WireAsyncError> {
        let agent = agent_context_of(&handle);
        if let Some(existing) = self.roster.lock().get(&agent.agent_id) {
            if !existing.closing.load(Ordering::Acquire)
                && Arc::ptr_eq(&existing.context, &agent)
            {
                return Ok(existing.context.clone());
            }
            if !existing.closing.load(Ordering::Acquire) {
                return Err(format!(
                    "Agent \"{}\" is already managed by a different context",
                    agent.agent_id
                )
                .into());
            }
        }
        let managed = Arc::new(ManagedAgent::new(agent.clone(), handle));
        managed.active.store(true, Ordering::Release);
        self.roster.lock().insert(agent.agent_id.clone(), managed);
        Ok(agent)
    }

    pub async fn remove(&self, agent: Arc<AgentContext>) -> Result<(), WireAsyncError> {
        let managed = self.roster.lock().get(&agent.agent_id).cloned();
        let Some(managed) = managed else {
            return Ok(());
        };
        if !Arc::ptr_eq(&managed.context, &agent)
            || managed.closing.swap(true, Ordering::AcqRel)
        {
            return Ok(());
        }
        self.remove_managed(agent, managed).await
    }

    async fn remove_managed(
        &self,
        agent: Arc<AgentContext>,
        managed: Arc<ManagedAgent>,
    ) -> Result<(), WireAsyncError> {
        self.will_close.fire(agent.clone());
        let handle = managed.handle.clone();
        handle
            .accessor()
            .get(&I_AGENT_TASK_SERVICE)
            .suppress_all_terminal_notifications()
            .await?;
        let loop_ = handle.accessor().get(&I_AGENT_LOOP_SERVICE);
        let compacting = handle.accessor().get(&I_AGENT_FULL_COMPACTION_SERVICE).compacting();
        let compaction_settled = async {
            if let Some(compacting) = compacting.clone() {
                let _ = compacting.promise.clone().await;
            }
        }
        .boxed()
        .shared();
        let reason: crate::_base::errors::errors::SharedError =
            Arc::new(AbortError::new("Agent removed"));
        if let Some(compacting) = compacting.as_ref()
            && !compacting.abort_controller.signal().aborted()
        {
            compacting.abort_controller.abort(Some(reason.clone()));
        }
        let prompt_idle_deadline = tokio::time::Instant::now()
            + Duration::from_millis(REMOVE_PROMPT_QUIESCE_TIMEOUT_MS);
        let mut release_quiescence = None::<DisposableRef>;
        loop {
            let snapshot = loop_.snapshot();
            for prompt_id in snapshot
                .queue
                .iter()
                .filter_map(|item| item.meta.as_ref().and_then(|meta| meta.prompt_id.clone()))
            {
                loop_.cancel(
                    Some(LoopCancelTarget {
                        turn_id: None,
                        prompt_id: Some(prompt_id.clone()),
                    }),
                    Some(reason.clone()),
                );
            }
            loop_.cancel(None, Some(reason.clone()));
            let remaining = prompt_idle_deadline.saturating_duration_since(tokio::time::Instant::now());
            let settled = tokio::time::timeout(remaining, async {
                let _ = tokio::join!(loop_.settled(), compaction_settled.clone());
            })
            .await
            .is_ok();
            if !settled {
                break;
            }
            let snapshot = loop_.snapshot();
            let idle = snapshot.state == LoopState::Idle && snapshot.queue.is_empty();
            if idle {
                if let Some(guard) = loop_.try_acquire_quiescence() {
                    release_quiescence = Some(guard);
                    break;
                }
            }
            if tokio::time::Instant::now() >= prompt_idle_deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(REMOVE_PROMPT_QUIESCE_POLL_MS)).await;
        }
        let stop_error = handle
            .accessor()
            .get(&I_AGENT_TASK_SERVICE)
            .stop_all_on_exit("Session closed".to_string())
            .await
            .err();
        let cleanup = async {
            if let Err(error) = handle.accessor().get(&I_EVENT_DISPATCHER).flush().await {
                on_unexpected_error(error);
            }
            managed.kill_space();
            if managed.ref_.lock().is_some() {
                self.session_actor.send(
                    SessionEvent::AgentStop {
                        agent_id: agent.agent_id.clone(),
                    }
                    .into(),
                );
                let ref_ = managed.ref_.lock().clone().unwrap();
                wait_for(ref_.as_ref(), |snapshot| snapshot.status == "done").await?;
            } else {
                managed.handle.dispose().await;
            }
            Ok::<(), WireAsyncError>(())
        }
        .await;
        drop(release_quiescence);
        cleanup?;
        if self
            .roster
            .lock()
            .get(&agent.agent_id)
            .is_some_and(|current| Arc::ptr_eq(current, &managed))
        {
            self.roster.lock().remove(&agent.agent_id);
        }
        self.did_close.fire(agent);
        if let Some(error) = stop_error {
            return Err(error);
        }
        Ok(())
    }

    fn managed_for(&self, agent: &Arc<AgentContext>) -> Option<Arc<ManagedAgent>> {
        self.roster.lock().get(&agent.agent_id).and_then(|managed| {
            (Arc::ptr_eq(&managed.context, agent) && !managed.closing.load(Ordering::Acquire))
                .then(|| managed.clone())
        })
    }

    fn require_managed(
        &self,
        agent: &Arc<AgentContext>,
    ) -> Result<Arc<ManagedAgent>, WireAsyncError> {
        self.managed_for(agent).ok_or_else(|| {
            format!(
                "Agent {}:{} is not a lifecycle-issued context",
                agent.agent_id, agent.generation
            )
            .into()
        })
    }
}

impl Drop for AgentLifecycleService {
    fn drop(&mut self) {
        self.session_actor.stop();
        interactions().purge_session(&self.ctx.session_id());
    }
}

#[async_trait]
impl IAgentLifecycleService for AgentLifecycleService {
    fn on_did_create(
        &self,
        listener: Arc<dyn Fn(Arc<AgentContext>) + Send + Sync>,
    ) -> DisposableRef {
        self.did_create.subscribe_in(listener)
    }

    fn on_did_create_scope(
        &self,
        listener: Arc<dyn Fn(AgentScopeCreatedEvent) + Send + Sync>,
    ) -> DisposableRef {
        self.did_create_scope.subscribe_in(listener)
    }

    fn on_will_close(
        &self,
        listener: Arc<dyn Fn(Arc<AgentContext>) + Send + Sync>,
    ) -> DisposableRef {
        self.will_close.subscribe_in(listener)
    }

    fn on_did_close(
        &self,
        listener: Arc<dyn Fn(Arc<AgentContext>) + Send + Sync>,
    ) -> DisposableRef {
        self.did_close.subscribe_in(listener)
    }

    async fn create(
        self: Arc<Self>,
        opts: Option<CreateAgentOptions>,
    ) -> Result<Arc<AgentContext>, WireAsyncError> {
        AgentLifecycleService::create(&self, opts).await
    }

    async fn fork(
        self: Arc<Self>,
        source: Arc<AgentContext>,
        opts: Option<ForkAgentOptions>,
    ) -> Result<Arc<AgentContext>, WireAsyncError> {
        AgentLifecycleService::fork(&self, source, opts).await
    }

    fn get(&self, agent_id: &str) -> Option<Arc<AgentContext>> {
        AgentLifecycleService::get(self, agent_id)
    }

    fn list(&self, filter: Option<AgentListFilter>) -> Vec<Arc<AgentContext>> {
        AgentLifecycleService::list(self, filter)
    }

    fn broadcast_permission_mode(&self, mode: PermissionMode) {
        AgentLifecycleService::broadcast_permission_mode(self, mode)
    }

    async fn remove(&self, agent: Arc<AgentContext>) -> Result<(), WireAsyncError> {
        AgentLifecycleService::remove(self, agent).await
    }

    fn handle_of(&self, agent_id: &str) -> Option<Arc<IAgentScopeHandle>> {
        AgentLifecycleService::handle_of(self, agent_id)
    }

    fn adopt(
        self: Arc<Self>,
        handle: Arc<IAgentScopeHandle>,
    ) -> Result<Arc<AgentContext>, WireAsyncError> {
        AgentLifecycleService::adopt(&self, handle)
    }
}

fn environment_binding_seed(
    service: &AgentLifecycleService,
    opts: &CreateAgentOptions,
) -> crate::agent::environment_binding::environment_binding::AgentEnvironmentBindingSeed {
    let binding = if opts.environment_id.is_some() {
        EnvironmentBinding {
            environment_id: opts.environment_id.clone().unwrap_or_default(),
            cwd: opts.environment_cwd.clone(),
        }
    } else {
        EnvironmentBinding {
            environment_id: service.workdir.environment_id(),
            cwd: service.workdir.cwd(),
        }
    };
    crate::agent::environment_binding::environment_binding::AgentEnvironmentBindingSeed { binding }
}

struct SessionSubscription(crate::human::xstate2::Subscription);

impl IDisposable for SessionSubscription {
    fn dispose(&self) {
        self.0.unsubscribe();
    }
}

fn session_context(actor: &AgentActorRef) -> SessionMachineContext {
    actor
        .get_snapshot()
        .context
        .downcast_ref::<SessionMachineContext>()
        .expect("session machine context")
        .clone()
}

fn agent_error(code: &'static str, message: String, agent_id: &str) -> WireAsyncError {
    let mut details = Map::new();
    details.insert("agentId".to_string(), Value::String(agent_id.to_string()));
    Box::new(Error2::with_options(
        code,
        message,
        Error2Options {
            details: Some(details),
            ..Default::default()
        },
    ))
}

fn error_name(error: &WireAsyncError) -> String {
    error
        .downcast_ref::<Error2>()
        .map(|error| error.name().to_string())
        .unwrap_or_else(|| "Unknown".to_string())
}
