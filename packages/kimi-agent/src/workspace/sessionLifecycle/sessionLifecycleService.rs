use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::future::{BoxFuture, Shared};
use futures_util::{FutureExt, StreamExt};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};

use crate::_base::di::instantiation::IInstantiationService;
use crate::_base::di::lifecycle::{IDisposable, to_disposable};
use crate::_base::di::scope::{ISessionScopeHandle, LifecycleScope, create_scoped_child_handle};
use crate::_base::log::log::ILogService;
use crate::_base::log::log_service::drain_log_closes;
use crate::_base::utils::paths::join;
use crate::agent::r#loop::r#loop::IAgentLoopService;
use crate::app::bootstrap::bootstrap::IBootstrapService;
use crate::app::config::config::IConfigService;
use crate::app::environment::environment::IEnvironmentService;
use crate::app::event::event::IEventService;
use crate::app::flag::flag::IFlagService;
use crate::app::session_index::session_index::{
    CHILD_SESSION_KIND, CHILD_SESSION_KIND_KEY, ISessionIndex, ISessionIndexMirror,
    PARENT_SESSION_ID_KEY,
};
use crate::app::session_index::session_index_source::{
    SessionSummaryInput, build_session_summary,
};
use crate::app::telemetry::telemetry::ITelemetryService;
use crate::app::telemetry::telemetry_service::bind_telemetry_scope;
use crate::environment::environment::{EnvironmentBinding, EnvironmentLease, LOCAL_ENVIRONMENT_ID};
use crate::environment::environment_registry::EnvironmentError;
use crate::features::fileHistory::fileHistory::IAgentFileHistoryService;
use crate::features::fileHistory::fileHistoryRetention::{
    drop_file_history_session, touch_forked_file_history,
};
use crate::features::fileHistory::fileHistory::FILE_HISTORY_BLOB_PREFIX;
use crate::features::notify::session_notify::ISessionNotify;
use crate::features::plan::config_section::DEFAULT_PLAN_MODE_SECTION;
use crate::features::plan::plan::IAgentPlanService;
use crate::features::skill::catalog::skill_source::PLUGIN_SKILL_SOURCE_ID;
use crate::features::skill::session::skill_catalog_data::ISessionSkillCatalogData;
use crate::llm_adapter::model::model::IModelService;
use crate::llm_adapter::provider::provider::IProviderService;
use crate::os::interface::host_file_system::{HostDirEntry, IHostFileSystem};
use crate::persistence::interface::append_log_store::{
    AppendLogReadOptions, AppendLogTruncation, IAppendLogStore,
};
use crate::persistence::interface::atomic_document_store::IAtomicDocumentStore;
use crate::persistence::interface::session_lock::ISessionLockService;
use crate::persistence::interface::storage::IFileSystemStorageService;
use crate::program::program::ProgramWorkdir;
use crate::session::agent_lifecycle::agent_lifecycle::{
    IAgentLifecycleService, MAIN_AGENT_ID,
};
use crate::session::agent_lifecycle::main_agent::ensure_main_agent;
use crate::session::agent_lifecycle::subagent_metadata::labels_from_agent_meta;
use crate::session::mcp::ephemeral_mcp_servers::session_ephemeral_mcp_servers_seed;
use crate::session::mcp::session_mcp_handle::ISessionMcpHandle;
use crate::session::session_agent_profile_catalog::agent_profile_catalog_seed::session_agent_profile_catalog_seed;
use crate::session::session_context::session_context::{
    ISessionContext, session_context_seed,
};
use crate::session::session_instructions::instructions_provider::ISessionInstructionsProvider;
use crate::session::session_metadata::session_metadata::{
    AgentMeta, ISessionMetadata, SESSION_META_VERSION, SessionMeta,
};
use crate::session::session_metadata::session_metadata_service::{
    drain_session_metadata_writes, encode_session_meta, to_epoch_ms,
};
use crate::session::session_tool_policy::session_tool_policy::ISessionToolPolicy;
use crate::session::workspace_context::workspace_roots::resolve_effective_workspace_roots;
use crate::session::workspace_info::workspace_info::ISessionWorkspaceInfo;
use crate::session::workdir::workdir::ISessionWorkdir;
use crate::state::event_dispatcher::IEventDispatcher;
use crate::wire::record::{
    AGENT_WIRE_RECORD_KEY, WireAsyncError, WireRecord, create_wire_metadata_record,
};
use crate::wire::repair::{WireJournalRepairServices, repair_wire_journal};
use crate::wire::tree::fork::flatten_chain;
use crate::workspace::workspace_agent_profile_loader::explicit_agent_profile_loader::IExplicitAgentProfileLoader;
use crate::workspace::workspace_agent_profile_loader::extra_agent_profile_loader::IExtraAgentProfileLoader;
use crate::workspace::workspace_agent_profile_loader::plugin_agent_profile_loader::IPluginAgentProfileLoader;
use crate::workspace::workspace_agent_profile_loader::user_agent_profile_loader::IUserAgentProfileLoader;
use crate::workspace::workspace_context::workspace_context::IWorkspaceContext;
use crate::workspace::workspace_mcp::workspace_mcp::IWorkspaceMcpService;
use kimi_core::primitives::errors::{Error2, Error2Options};

use super::internal::addressing::{agent_scope_of, session_dir_of, session_scope_of};
use super::internal::fork_turn_slice::*;
use super::session_lifecycle::*;
use super::session_lifecycle_events::{
    SessionArchived, SessionArchivedPayload, SessionDeleted, SessionDeletedPayload,
    SessionRestored, SessionRestoredPayload,
};

const SESSION_CREATE_RELOAD_SKILL_SOURCES: &[&str] = &["user", "explicit", "extra", PLUGIN_SKILL_SOURCE_ID];

#[derive(Debug, Clone)]
struct SharedLifecycleError(Arc<String>);

impl SharedLifecycleError {
    fn new(error: WireAsyncError) -> Self {
        Self(Arc::new(error.to_string()))
    }

    fn boxed(self) -> WireAsyncError {
        Box::new(self)
    }
}

impl fmt::Display for SharedLifecycleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.as_str())
    }
}

impl Error for SharedLifecycleError {}

type ResumeFuture = Shared<
    BoxFuture<'static, Result<Option<Arc<ISessionScopeHandle>>, SharedLifecycleError>>,
>;

pub struct SessionLifecycleServices {
    pub instantiation: Arc<dyn IInstantiationService>,
    pub workspace_context: Arc<dyn IWorkspaceContext>,
    pub bootstrap: Arc<dyn IBootstrapService>,
    pub config: Arc<dyn IConfigService>,
    pub index: Arc<dyn ISessionIndex>,
    pub index_mirror: Arc<dyn ISessionIndexMirror>,
    pub append_log_store: Arc<dyn IAppendLogStore>,
    pub session_locks: Arc<dyn ISessionLockService>,
    pub docs: Arc<dyn IAtomicDocumentStore>,
    pub storage: Arc<dyn IFileSystemStorageService>,
    pub log: Arc<dyn ILogService>,
    pub host_fs: Arc<dyn IHostFileSystem>,
    pub event: Arc<dyn IEventService>,
    pub telemetry: Arc<dyn ITelemetryService>,
    pub flags: Arc<dyn IFlagService>,
    pub extra_agent_profile_loader: Arc<dyn IExtraAgentProfileLoader>,
    pub explicit_agent_profile_loader: Arc<dyn IExplicitAgentProfileLoader>,
    pub user_agent_profile_loader: Arc<dyn IUserAgentProfileLoader>,
    pub plugin_agent_profile_loader: Arc<dyn IPluginAgentProfileLoader>,
    pub workspace_mcp: Arc<dyn IWorkspaceMcpService>,
    pub models: Arc<dyn IModelService>,
    pub providers: Arc<dyn IProviderService>,
    pub environments: Arc<dyn IEnvironmentService>,
    pub acquire_workdir: Arc<
        dyn Fn(String, String, bool) -> BoxFuture<'static, Result<ProgramWorkdir, WireAsyncError>>
            + Send
            + Sync,
    >,
}

struct LifecycleListeners<T> {
    next_id: u64,
    listeners: HashMap<u64, Arc<dyn Fn(T) + Send + Sync>>,
}

impl<T> Default for LifecycleListeners<T> {
    fn default() -> Self {
        Self {
            next_id: 0,
            listeners: HashMap::new(),
        }
    }
}

struct LifecycleEvents {
    will_create: Mutex<LifecycleListeners<SessionWillCreateEvent>>,
    did_create: Mutex<LifecycleListeners<LifecycleWaitEvent<SessionCreatedEvent>>>,
    will_close: Mutex<LifecycleListeners<LifecycleWaitEvent<SessionWillCloseEvent>>>,
    did_close: Mutex<LifecycleListeners<SessionClosedEvent>>,
    did_archive: Mutex<LifecycleListeners<SessionArchivedEvent>>,
    did_fork: Mutex<LifecycleListeners<SessionForkedEvent>>,
}

struct LifecycleState {
    sessions: HashMap<String, Arc<ISessionScopeHandle>>,
    resuming: HashMap<String, ResumeFuture>,
    resume_failures: HashMap<String, SharedLifecycleError>,
}

pub struct SessionLifecycleService {
    services: Arc<SessionLifecycleServices>,
    state: Arc<Mutex<LifecycleState>>,
    events: Arc<LifecycleEvents>,
    _on_dispose: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl SessionLifecycleService {
    pub fn new(services: SessionLifecycleServices, on_dispose: Option<Box<dyn FnOnce() + Send>>) -> Self {
        Self {
            services: Arc::new(services),
            state: Arc::new(Mutex::new(LifecycleState {
                sessions: HashMap::new(),
                resuming: HashMap::new(),
                resume_failures: HashMap::new(),
            })),
            events: Arc::new(LifecycleEvents {
                will_create: Mutex::new(LifecycleListeners::default()),
                did_create: Mutex::new(LifecycleListeners::default()),
                will_close: Mutex::new(LifecycleListeners::default()),
                did_close: Mutex::new(LifecycleListeners::default()),
                did_archive: Mutex::new(LifecycleListeners::default()),
                did_fork: Mutex::new(LifecycleListeners::default()),
            }),
            _on_dispose: Mutex::new(on_dispose),
        }
    }

    fn workspace_id(&self) -> String {
        self.services.workspace_context.workspace_id()
    }

    fn handler_scope(&self) -> String {
        self.services.workspace_context.persistence_scope()
    }

    pub async fn create(&self, opts: CreateSessionOptions) -> Result<Arc<ISessionScopeHandle>, WireAsyncError> {
        let session_id = opts.session_id.clone().unwrap_or_else(create_session_id);
        let environment = session_environment(opts.environment_id.as_deref(), opts.environment_cwd.as_deref())?;
        let work_dir = self.session_work_dir(&opts.work_dir, environment.as_ref());
        let workdir = self.open_workdir(environment.as_ref(), &work_dir).await?;
        let _ = workdir
            .skills
            .reload_sources(&SESSION_CREATE_RELOAD_SKILL_SOURCES.iter().map(|source| source.to_string()).collect::<Vec<_>>())
            .await;
        let handle = self.materialize_session(&opts, &session_id, environment.clone(), workdir).await?;
        let result = async {
            if let Some(environment) = &environment {
                let metadata = handle.accessor().get::<ISessionMetadata>();
                if environment.environment_id == LOCAL_ENVIRONMENT_ID {
                    metadata.update_environment_cwd(&environment.cwd, false).await?;
                } else {
                    metadata.update_environment(&environment.environment_id, &environment.cwd, &work_dir, false).await?;
                }
            }
            let agents = handle.accessor().get::<IAgentLifecycleService>();
            let main = if opts.main_agent_binding.is_some() || opts.ensure_main_agent || environment.is_some() {
                Some(agents.create(Some(crate::session::agent_lifecycle::agent_lifecycle::CreateAgentOptions {
                    agent_id: Some(MAIN_AGENT_ID.to_string()),
                    binding: opts.main_agent_binding.clone(),
                    environment_id: environment.as_ref().map(|environment| environment.environment_id.clone()),
                    environment_cwd: environment.as_ref().map(|environment| environment.cwd.clone()),
                    ..Default::default()
                })).await?)
            } else {
                None
            };
            if self.services.config.get(DEFAULT_PLAN_MODE_SECTION).and_then(|value| value.as_bool()) == Some(true) {
                let plan_agent = match main {
                    Some(agent) => agent,
                    None => ensure_main_agent(&handle).await?,
                };
                let plan_handle = agents.handle_of(&plan_agent.agent_id).ok_or_else(|| -> WireAsyncError {
                    Box::new(Error2::new("agent.not_found", "Main agent was not found"))
                })?;
                plan_handle.accessor().get::<IAgentPlanService>().enter().await?;
            }
            self.append_session_index_entry(&session_id, &work_dir).await
        }
        .await;
        if let Err(error) = result {
            let session_dir = handle.accessor().get::<ISessionContext>().session_dir();
            self.state.lock().sessions.remove(&session_id);
            let _ = self.drain_agents(&handle).await;
            handle.dispose();
            let _ = self.services.host_fs.remove(&session_dir).await;
            return Err(error);
        }
        self.announce_created(SessionCreatedEvent {
            session_id,
            handle: handle.clone(),
            source: SessionCreateSource::Startup,
        }).await?;
        Ok(handle)
    }

    async fn materialize_session(
        &self,
        opts: &CreateSessionOptions,
        session_id: &str,
        environment: Option<EnvironmentBinding>,
        workdir: ProgramWorkdir,
    ) -> Result<Arc<ISessionScopeHandle>, WireAsyncError> {
        let workspace_id = self.workspace_id();
        let session_scope = session_scope_of(&self.handler_scope(), session_id);
        let session_dir = session_dir_of(&self.services.bootstrap.home_dir(), &self.handler_scope(), session_id);
        let dirs = workdir.dirs.clone();
        let ready_result = async {
            let (config, models, providers) = tokio::join!(
                self.services.config.ready(),
                self.services.models.ready(),
                self.services.providers.ready()
            );
            config?;
            models?;
            providers?;
            dirs.ready().await?;
            dirs.merge_additional_dirs(
                environment.as_ref().map(|environment| environment.cwd.as_str()).unwrap_or(&opts.work_dir),
                opts.additional_dirs.as_deref().unwrap_or(&[]),
            ).await
        }
        .await;
        if let Err(error) = ready_result {
            workdir.dispose();
            return Err(error);
        }
        let ctx = SessionContextData {
            session_id: session_id.to_string(),
            workspace_id: workspace_id.clone(),
            session_dir,
            meta_scope: session_scope.clone(),
            cwd: environment
                .as_ref()
                .filter(|environment| environment.environment_id == LOCAL_ENVIRONMENT_ID)
                .map(|environment| environment.cwd.clone())
                .unwrap_or_else(|| opts.work_dir.clone()),
            scope: session_scope.clone(),
        };
        let telemetry_binding = bind_telemetry_scope(
            self.services.telemetry.clone(),
            serde_json::Map::from_iter([("session_id".to_string(), session_id.into())]),
        );
        let scope_listeners = Arc::new(Mutex::new(LifecycleListeners::<Arc<ISessionScopeHandle>>::default()));
        let will_create_listeners = scope_listeners.clone();
        let container_scope = session_scope.clone();
        let seeds = session_context_seed(ctx.clone());
        let configure = move |container: &dyn crate::_base::di::scope::IScopeContainer| {
            container.anchor_kernel_entry(Box::new(|| telemetry_binding.dispose()), "telemetry:session-context");
            let workdir_dispose = workdir.clone();
            container.anchor_kernel_entry(Box::new(move || workdir_dispose.dispose()), "sessionLifecycle:workdir");
            let read_container = container.clone_handle();
            let contribute_container = container.clone_handle();
            let dispose_container = container.clone_handle();
            self.fire_will_create(SessionWillCreateEvent {
                session_id: session_id.to_string(),
                on_did_create_scope: Arc::new(move |listener| {
                    add_listener(&will_create_listeners, listener)
                }),
                read_seed: Arc::new(move |id| read_container.invoke_function(id)),
                contribute_seed: Arc::new(move |id, value| contribute_container.provide(id, value)),
                on_session_dispose: Arc::new(move |dispose| {
                    dispose_container.anchor_kernel_entry(dispose, "sessionLifecycle:willCreateParticipant");
                }),
            });
            let _ = container_scope;
        };
        let handle = match create_scoped_child_handle(
            self.services.instantiation.clone(),
            LifecycleScope::Session,
            session_id,
            seeds,
            configure,
        ) {
            Ok(handle) => handle,
            Err(error) => {
                workdir.dispose();
                return Err(error);
            }
        };
        fire_listeners(&scope_listeners, handle.clone());
        let ready_result = async {
            handle.accessor().get::<ISessionMetadata>().ready().await?;
            handle.accessor().get::<ISessionNotify>().ready().await?;
            handle.accessor().get::<ISessionToolPolicy>().ready().await?;
            let (profiles, extra, explicit, user, plugin) = tokio::join!(
                workdir.agent_profiles.ready(),
                self.services.extra_agent_profile_loader.ready(),
                self.services.explicit_agent_profile_loader.ready(),
                self.services.user_agent_profile_loader.ready(),
                self.services.plugin_agent_profile_loader.ready()
            );
            profiles?;
            extra?;
            explicit?;
            user?;
            plugin?;
            Ok(())
        }
        .await;
        if let Err(error) = ready_result {
            handle.dispose();
            let _ = self.services.explicit_agent_profile_loader.reload().await;
            return Err(error);
        }
        self.state.lock().sessions.insert(session_id.to_string(), handle.clone());
        Ok(handle)
    }

    fn session_work_dir(&self, fallback_cwd: &str, environment: Option<&EnvironmentBinding>) -> String {
        if environment.is_some_and(|environment| environment.environment_id != LOCAL_ENVIRONMENT_ID) {
            return fallback_cwd.to_string();
        }
        let primary = self.services.workspace_context.meta().root;
        resolve_effective_workspace_roots(
            &self.workspace_id(),
            fallback_cwd,
            environment,
            &HashMap::from([(self.workspace_id(), primary)]),
            &[],
        ).work_dir
    }

    async fn open_workdir(&self, environment: Option<&EnvironmentBinding>, work_dir: &str) -> Result<ProgramWorkdir, WireAsyncError> {
        let Some(environment) = environment else {
            return (self.services.acquire_workdir)(LOCAL_ENVIRONMENT_ID.to_string(), work_dir.to_string(), true).await;
        };
        let environment_id = environment.environment_id.clone();
        let cwd = environment.cwd.clone();
        let lease = match self.services.environments.acquire_when_ready(&environment_id, &["fs"]).await {
            Ok(lease) => lease,
            Err(error) if error.downcast_ref::<EnvironmentError>().is_some() => return Err(error),
            Err(error) => {
                return Err(Box::new(EnvironmentError::new(
                    "environment.unavailable",
                    format!("failed to connect environment {environment_id}: {error}"),
                    Some(error),
                )));
            }
        };
        let stat_result = async {
            let stat = lease.environment.fs.as_ref().unwrap().stat(&cwd).await.map_err(|error| {
                Box::new(EnvironmentError::new(
                    "environment.invalid_cwd",
                    format!("cwd {cwd} is not readable on environment {environment_id}: {error}"),
                    None,
                )) as WireAsyncError
            })?;
            if !stat.is_directory {
                return Err(Box::new(EnvironmentError::new(
                    "environment.invalid_cwd",
                    format!("cwd {cwd} is not a directory on environment {environment_id}"),
                    None,
                )) as WireAsyncError);
            }
            Ok(())
        }
        .await;
        drop(lease);
        stat_result?;
        (self.services.acquire_workdir)(environment_id, cwd, false).await
    }

    async fn append_session_index_entry(&self, session_id: &str, work_dir: &str) -> Result<(), WireAsyncError> {
        let session_dir = session_dir_of(&self.services.bootstrap.home_dir(), &self.handler_scope(), session_id);
        self.services.append_log_store.append("", "session_index.jsonl", json!({
            "sessionId": session_id,
            "sessionDir": session_dir,
            "workDir": work_dir,
        }), None)?;
        self.services.append_log_store.flush().await
    }

    async fn announce_created(&self, event: SessionCreatedEvent) -> Result<(), WireAsyncError> {
        fire_wait_listeners(&self.events.did_create, event.clone()).await?;
        let resumed = event.source != SessionCreateSource::Startup;
        let mut flags = self.services.flags.exposed_ids();
        flags.sort();
        let mut properties = Map::new();
        properties.insert("resumed".to_string(), resumed.into());
        properties.insert("experimental_flags".to_string(), flags.join(",").into());
        event.handle.accessor().get::<ITelemetryService>().track2("session_started", Some(properties));
        Ok(())
    }

    pub fn get(&self, session_id: &str) -> Option<Arc<ISessionScopeHandle>> {
        let state = self.state.lock();
        if state.resuming.contains_key(session_id) {
            return None;
        }
        state.sessions.get(session_id).cloned()
    }

    pub fn resume(&self, session_id: &str, opts: Option<ResumeSessionOptions>) -> BoxFuture<'static, Result<Option<Arc<ISessionScopeHandle>>, WireAsyncError>> {
        self.resume_with_source(session_id, opts, SessionCreateSource::Resume)
    }

    fn resume_with_source(&self, session_id: &str, opts: Option<ResumeSessionOptions>, source: SessionCreateSource) -> BoxFuture<'static, Result<Option<Arc<ISessionScopeHandle>>, WireAsyncError>> {
        if let Some(inflight) = self.state.lock().resuming.get(session_id).cloned() {
            return async move { inflight.await.map_err(SharedLifecycleError::boxed) }.boxed();
        }
        if let Some(live) = self.state.lock().sessions.get(session_id).cloned() {
            return async move { Ok(Some(live)) }.boxed();
        }
        self.state.lock().resume_failures.remove(session_id);
        let this = self.clone_handle();
        let session_id = session_id.to_string();
        let future = async move {
            let result = this.do_resume(&session_id, source, opts).await;
            if let Err(error) = &result {
                let reason = error_code(error.as_ref()).unwrap_or("unknown");
                let mut properties = Map::new();
                properties.insert("reason".to_string(), reason.into());
                this.services.telemetry.with_context(serde_json::Map::from_iter([
                    ("session_id".to_string(), session_id.clone().into()),
                ])).track2("session_load_failed", Some(properties));
                this.state.lock().resume_failures.insert(
                    session_id.clone(),
                    SharedLifecycleError::new(error.to_string().into()),
                );
            }
            this.state.lock().resuming.remove(&session_id);
            result.map_err(SharedLifecycleError::new)
        }
        .boxed()
        .shared();
        self.state.lock().resuming.insert(session_id, future.clone());
        tokio::spawn(future.clone());
        async move { future.await.map_err(SharedLifecycleError::boxed) }.boxed()
    }

    pub async fn when_resume_settled(&self, session_id: &str) -> Result<(), WireAsyncError> {
        let inflight = self.state.lock().resuming.get(session_id).cloned();
        if let Some(inflight) = inflight {
            let _ = inflight.await;
        }
        if let Some(error) = self.state.lock().resume_failures.get(session_id).cloned() {
            return Err(error.boxed());
        }
        Ok(())
    }

    async fn do_resume(&self, session_id: &str, source: SessionCreateSource, opts: Option<ResumeSessionOptions>) -> Result<Option<Arc<ISessionScopeHandle>>, WireAsyncError> {
        if let Some(live) = self.state.lock().sessions.get(session_id).cloned() {
            return Ok(Some(live));
        }
        let summary = self.services.index.get(session_id).await?;
        let Some(summary) = summary.filter(|summary| summary.workspace_id == self.workspace_id()) else {
            return Ok(None);
        };
        let persisted = self.read_meta_from_disk(session_id).await?;
        let environment = session_environment(
            persisted.as_ref().and_then(|meta| meta.environment_id.as_deref()),
            persisted.as_ref().and_then(|meta| meta.environment_cwd.as_deref()),
        )?;
        let fallback_cwd = persisted.as_ref().and_then(|meta| meta.cwd.clone())
            .or(summary.cwd)
            .unwrap_or_else(|| self.services.workspace_context.meta().root);
        let work_dir = self.session_work_dir(&fallback_cwd, environment.as_ref());
        let create_opts = CreateSessionOptions {
            session_id: Some(session_id.to_string()),
            work_dir: work_dir.clone(),
            additional_dirs: opts.as_ref().and_then(|opts| opts.additional_dirs.clone()),
            main_agent_binding: None,
            ensure_main_agent: false,
            environment_id: environment.as_ref().map(|environment| environment.environment_id.clone()),
            environment_cwd: environment.as_ref().map(|environment| environment.cwd.clone()),
            mcp_servers: opts.and_then(|opts| opts.mcp_servers),
        };
        let workdir = self.open_workdir(environment.as_ref(), &work_dir).await?;
        let handle = self.materialize_session(&create_opts, session_id, environment, workdir).await?;
        let result = async {
            let agents = handle.accessor().get::<IAgentLifecycleService>();
            if agents.get(MAIN_AGENT_ID).is_none() {
                agents.create(Some(crate::session::agent_lifecycle::agent_lifecycle::CreateAgentOptions {
                    agent_id: Some(MAIN_AGENT_ID.to_string()),
                    binding: None,
                    environment_id: None,
                    environment_cwd: None,
                    ..Default::default()
                })).await?;
            }
            self.announce_created(SessionCreatedEvent {
                session_id: session_id.to_string(),
                handle: handle.clone(),
                source,
            }).await
        }
        .await;
        if let Err(error) = result {
            self.state.lock().sessions.remove(session_id);
            handle.dispose();
            return Err(error);
        }
        Ok(Some(handle))
    }

    pub fn list(&self) -> Vec<Arc<ISessionScopeHandle>> {
        let state = self.state.lock();
        state.sessions.iter().filter(|(id, _)| !state.resuming.contains_key(*id)).map(|(_, handle)| handle.clone()).collect()
    }

    pub async fn close(&self, session_id: &str) -> Result<(), WireAsyncError> {
        self.close_with_reason(session_id, SessionCloseReason::Exit).await
    }

    async fn close_with_reason(&self, session_id: &str, reason: SessionCloseReason) -> Result<(), WireAsyncError> {
        let handle = self.state.lock().sessions.get(session_id).cloned();
        let Some(handle) = handle else {
            return Ok(());
        };
        self.announce_will_close(SessionWillCloseEvent {
            session_id: session_id.to_string(),
            handle: handle.clone(),
            reason,
        }).await?;
        self.state.lock().sessions.remove(session_id);
        self.drain_agents(&handle).await?;
        self.services.append_log_store.drain_retirements().await?;
        drain_session_metadata_writes().await?;
        self.services.index_mirror.drain().await?;
        handle.dispose();
        self.services.environments.drain_session(session_id).await?;
        drain_log_closes().await?;
        fire_listeners(&self.events.did_close, SessionClosedEvent { session_id: session_id.to_string() });
        let mut properties = Map::new();
        properties.insert("reason".to_string(), format!("{reason:?}").to_lowercase().into());
        self.services.telemetry.with_context(serde_json::Map::from_iter([
            ("session_id".to_string(), session_id.into()),
        ])).track2("session_ended", Some(properties));
        Ok(())
    }

    pub async fn archive(&self, session_id: &str) -> Result<(), WireAsyncError> {
        let handle = self.state.lock().sessions.get(session_id).cloned();
        let Some(handle) = handle else {
            return Ok(());
        };
        let lease = self.services.session_locks.lease(session_id);
        let result = async {
            let meta = handle.accessor().get::<ISessionMetadata>();
            meta.set_archived(true).await?;
            self.drain_agents(&handle).await?;
            self.services.append_log_store.drain_retirements().await?;
            self.services.event.publish(SessionArchived::new(SessionArchivedPayload {
                session_id: session_id.to_string(),
                workspace_id: self.workspace_id(),
            }));
            self.announce_will_close(SessionWillCloseEvent {
                session_id: session_id.to_string(),
                handle: handle.clone(),
                reason: SessionCloseReason::Archive,
            }).await?;
            self.state.lock().sessions.remove(session_id);
            drain_session_metadata_writes().await?;
            self.services.index_mirror.drain().await?;
            handle.dispose();
            self.services.environments.drain_session(session_id).await?;
            drain_log_closes().await?;
            fire_listeners(&self.events.did_archive, SessionArchivedEvent { session_id: session_id.to_string() });
            let mut properties = Map::new();
            properties.insert("reason".to_string(), "archive".into());
            self.services.telemetry.with_context(serde_json::Map::from_iter([
                ("session_id".to_string(), session_id.into()),
            ])).track2("session_ended", Some(properties));
            Ok(())
        }
        .await;
        lease.release();
        result
    }

    pub async fn restore(&self, session_id: &str, opts: Option<ResumeSessionOptions>) -> Result<Option<Arc<ISessionScopeHandle>>, WireAsyncError> {
        let handle = self.resume(session_id, opts).await?;
        let Some(handle) = handle else {
            return Ok(None);
        };
        let metadata = handle.accessor().get::<ISessionMetadata>();
        let was_archived = metadata.read().await?.archived;
        metadata.set_archived(false).await?;
        if was_archived {
            self.services.event.publish(SessionRestored::new(SessionRestoredPayload {
                session_id: session_id.to_string(),
                workspace_id: self.workspace_id(),
            }));
        }
        Ok(Some(handle))
    }

    pub async fn delete(&self, session_id: &str) -> Result<(), WireAsyncError> {
        let inflight = self.state.lock().resuming.get(session_id).cloned();
        if let Some(inflight) = inflight {
            let _ = inflight.await;
        }
        let handle = self.state.lock().sessions.get(session_id).cloned();
        let summary = self.services.index.get(session_id).await?;
        let persisted_here = summary.as_ref().is_some_and(|summary| summary.workspace_id == self.workspace_id());
        if handle.is_none() && !persisted_here {
            return Err(Box::new(Error2::new("session.not_found", format!("session {session_id} does not exist"))));
        }
        let lease = self.services.session_locks.lease(session_id);
        let result = async {
            if handle.is_some() {
                self.close(session_id).await?;
            }
            self.services.host_fs.remove(&session_dir_of(&self.services.bootstrap.home_dir(), &self.handler_scope(), session_id)).await?;
            self.services.index.remove(session_id).await?;
            drop_file_history_session(self.services.docs.as_ref(), &self.workspace_id(), session_id).await?;
            self.services.append_log_store.append("", "session_index.jsonl", json!({ "sessionId": session_id, "deleted": true }), None)?;
            self.services.append_log_store.flush().await?;
            self.services.event.publish(SessionDeleted::new(SessionDeletedPayload {
                session_id: session_id.to_string(),
                workspace_id: self.workspace_id(),
            }));
            Ok(())
        }
        .await;
        lease.release();
        result
    }

    async fn announce_will_close(&self, event: SessionWillCloseEvent) -> Result<(), WireAsyncError> {
        fire_wait_listeners(&self.events.will_close, event).await
    }

    async fn drain_agents(&self, handle: &Arc<ISessionScopeHandle>) -> Result<(), WireAsyncError> {
        let lifecycle = handle.accessor().get::<IAgentLifecycleService>();
        for agent in lifecycle.list() {
            lifecycle.remove(&agent).await?;
        }
        Ok(())
    }

    pub async fn fork(&self, opts: ForkSessionOptions) -> Result<SessionMeta, WireAsyncError> {
        let source_id = opts.source_session_id.clone();
        let source_handle = self.state.lock().sessions.get(&source_id).cloned();
        let index_summary = self.services.index.get(&source_id).await?;
        if (source_handle.is_none() && index_summary.is_none())
            || index_summary.as_ref().is_some_and(|summary| summary.workspace_id != self.workspace_id())
        {
            return Err(Box::new(Error2::new("session.not_found", format!("session {source_id} does not exist"))));
        }
        assert_fork_turn_index(opts.turn_index)?;
        let source_lease = self.services.session_locks.lease(&source_id);
        let mut target_session_dir = None;
        let result = self.fork_inner(&opts, source_handle.clone(), &mut target_session_dir).await;
        if result.is_err() {
            if let Some(target_dir) = target_session_dir {
                let _ = self.services.host_fs.remove(&target_dir).await;
            }
        }
        source_lease.release();
        result
    }

    async fn fork_inner(
        &self,
        opts: &ForkSessionOptions,
        source_handle: Option<Arc<ISessionScopeHandle>>,
        target_session_dir: &mut Option<String>,
    ) -> Result<SessionMeta, WireAsyncError> {
        let source_id = opts.source_session_id.clone();
        let mut quiescence_holds = Vec::new();
        let mut history_snapshot_agent_ids = HashSet::new();
        let mut active_turn_ids = HashMap::new();
        let mut queued_prompt_ids = HashMap::new();
        if let Some(source_handle) = &source_handle {
            let agents = source_handle.accessor().get::<IAgentLifecycleService>();
            let agent_handles = agents.list().into_iter().filter_map(|agent| agents.handle_of(&agent.agent_id)).collect::<Vec<_>>();
            for agent_handle in agent_handles {
                let loop_ = agent_handle.accessor().get::<IAgentLoopService>();
                let snapshot = loop_.snapshot();
                let active_turn_id = snapshot.active_turn_id.or_else(|| snapshot.turn.as_ref().map(|turn| turn.turn_id));
                let requires_history_snapshot = active_turn_id.is_some() || snapshot.has_pending_requests;
                let hold = if requires_history_snapshot {
                    loop_.try_acquire_history_snapshot()
                } else {
                    loop_.try_acquire_quiescence()
                };
                let Some(hold) = hold else {
                    let mut details = Map::new();
                    details.insert("sessionId".to_string(), source_id.clone().into());
                    details.insert("agentId".to_string(), agent_handle.id().into());
                    return Err(Box::new(Error2::with_options(
                        "session.fork_active_turn",
                        format!("Session \"{source_id}\" cannot be forked while another session history operation is in progress"),
                        Error2Options { details: Some(details), ..Default::default() },
                    )));
                };
                quiescence_holds.push(hold);
                if requires_history_snapshot {
                    history_snapshot_agent_ids.insert(agent_handle.id());
                    queued_prompt_ids.insert(agent_handle.id(), snapshot.queue.iter().filter_map(|item| {
                        item.meta.as_ref().and_then(|meta| meta.get("promptId")).and_then(Value::as_str).map(str::to_string)
                    }).collect::<Vec<_>>());
                }
                if let Some(active_turn_id) = active_turn_id {
                    active_turn_ids.insert(agent_handle.id(), active_turn_id);
                }
            }
            for agent_handle in agents.list().into_iter().filter_map(|agent| agents.handle_of(&agent.agent_id)) {
                agent_handle.accessor().get::<IEventDispatcher>().flush().await?;
            }
            self.services.append_log_store.flush().await?;
        }
        drain_session_metadata_writes().await?;
        if let Some(source_handle) = &source_handle {
            let agents = source_handle.accessor().get::<IAgentLifecycleService>();
            for agent in agents.list() {
                if let Some(handle) = agents.handle_of(&agent.agent_id) {
                    handle.accessor().get::<IAgentFileHistoryService>().settled().await;
                }
            }
        }
        let source_meta = match &source_handle {
            Some(handle) => Some(handle.accessor().get::<ISessionMetadata>().read().await?),
            None => self.read_meta_from_disk(&source_id).await?,
        };
        let target_id = opts.new_session_id.clone().unwrap_or_else(create_session_id);
        if self.state.lock().sessions.contains_key(&target_id) || self.services.index.get(&target_id).await?.is_some() {
            return Err(Box::new(Error2::new("session.already_exists", format!("Session \"{target_id}\" already exists"))));
        }
        let running_fork = !history_snapshot_agent_ids.is_empty();
        let main_records = if opts.turn_index.is_some() || running_fork {
            Some(flatten_chain(&self.read_source_wire_records(source_handle.as_ref(), &source_id, MAIN_AGENT_ID).await?)?)
        } else {
            None
        };
        let completed_main_records = match (main_records.clone(), running_fork) {
            (Some(records), true) => Some(slice_records_before_pending_work(
                &records,
                active_turn_ids.get(MAIN_AGENT_ID).copied(),
                queued_prompt_ids.get(MAIN_AGENT_ID).map(Vec::as_slice).unwrap_or(&[]),
            )?),
            (records, _) => records,
        };
        let turn_slice = if let Some(turn_index) = opts.turn_index {
            Some(slice_main_records_at_turn(
                completed_main_records.as_ref().unwrap(),
                &source_id,
                turn_index as i64,
            )?)
        } else if running_fork {
            Some(slice_main_records_for_running_fork(
                main_records.as_ref().unwrap(),
                &source_id,
                active_turn_ids.get(MAIN_AGENT_ID).copied(),
                queued_prompt_ids.get(MAIN_AGENT_ID).map(Vec::as_slice).unwrap_or(&[]),
            )?)
        } else {
            None
        };
        let target_dir = session_dir_of(&self.services.bootstrap.home_dir(), &self.handler_scope(), &target_id);
        *target_session_dir = Some(target_dir.clone());
        self.copy_session_files(
            &session_dir_of(&self.services.bootstrap.home_dir(), &self.handler_scope(), &source_id),
            &target_dir,
            turn_slice.is_some(),
        ).await?;
        let source_agents = source_meta.as_ref().map(|meta| meta.agents.clone()).unwrap_or_default();
        let agent_ids = source_agents.keys().cloned().collect::<Vec<_>>();
        let mut retained_agent_ids = agent_ids.clone();
        if let Some(turn_slice) = &turn_slice {
            let mut retained = Vec::new();
            for agent_id in &agent_ids {
                let sliced_records = if agent_id == MAIN_AGENT_ID {
                    turn_slice.records.clone()
                } else {
                    let records = flatten_chain(&self.read_source_wire_records(source_handle.as_ref(), &source_id, agent_id).await?)?;
                    let records = slice_records_before_pending_work(
                        &records,
                        active_turn_ids.get(agent_id).copied(),
                        queued_prompt_ids.get(agent_id).map(Vec::as_slice).unwrap_or(&[]),
                    )?;
                    let records = slice_subagent_records_at_time(&records, turn_slice.cutoff_time);
                    if records.is_empty() {
                        continue;
                    }
                    records
                };
                self.copy_agent_wire(CopyAgentWireInput {
                    source_handle: source_handle.clone(),
                    source_session_id: source_id.clone(),
                    agent_id: agent_id.clone(),
                    target_session_id: target_id.clone(),
                    records: Some(sliced_records),
                }).await?;
                retained.push(agent_id.clone());
            }
            retained_agent_ids = retained;
            self.prune_truncated_fork_files(&target_dir, &agent_ids, &retained_agent_ids).await?;
        } else {
            for agent_id in &agent_ids {
                self.append_forked_marker(&target_id, agent_id).await?;
            }
            self.services.append_log_store.flush().await?;
            touch_forked_file_history(
                self.services.docs.as_ref(),
                self.services.host_fs.as_ref(),
                &self.workspace_id(),
                &target_dir,
                &target_id,
            ).await?;
        }
        let title = opts.title.clone().unwrap_or_else(|| {
            format!("Fork: {}", source_meta.as_ref().and_then(|meta| meta.title.clone()).filter(|title| !title.is_empty()).unwrap_or_else(|| source_id.clone()))
        });
        let now = chrono::Utc::now().timestamp_millis();
        let mut agents = HashMap::new();
        for agent_id in &retained_agent_ids {
            let source_agent = source_agents.get(agent_id).unwrap();
            agents.insert(agent_id.clone(), AgentMeta {
                homedir: join(&self.services.bootstrap.home_dir(), &[&agent_scope_of(&session_scope_of(&self.handler_scope(), &target_id), agent_id)]),
                type_: if agent_id == MAIN_AGENT_ID { "main".to_string() } else { "sub".to_string() },
                parent_agent_id: (agent_id != MAIN_AGENT_ID).then(|| MAIN_AGENT_ID.to_string()),
                forked_from: source_agent.forked_from.clone(),
                labels: labels_from_agent_meta(source_agent),
                ..source_agent.clone()
            });
        }
        let host_cwd = source_handle.as_ref().map(|handle| handle.accessor().get::<ISessionContext>().cwd())
            .or_else(|| source_meta.as_ref().and_then(|meta| meta.cwd.clone()))
            .unwrap_or_else(|| self.services.workspace_context.cwd());
        let meta = SessionMeta {
            id: target_id.clone(),
            version: SESSION_META_VERSION,
            cwd: Some(host_cwd.clone()),
            created_at: now,
            updated_at: source_meta.as_ref().map(|meta| to_epoch_ms(&meta.updated_at)).filter(|time| *time != 0).unwrap_or(now),
            archived: false,
            archived_at: None,
            title,
            title_kind: if opts.title.is_some() { Some("custom".to_string()) } else { source_meta.as_ref().and_then(|meta| meta.title_kind.clone()) },
            forked_from: Some(source_id.clone()),
            agents,
            environment_id: source_meta.as_ref().and_then(|meta| meta.environment_id.clone()),
            environment_cwd: source_meta.as_ref().and_then(|meta| meta.environment_cwd.clone()),
            custom: fork_custom_metadata(source_meta.as_ref().and_then(|meta| meta.custom.clone()), opts.metadata.clone()),
            last_prompt: if turn_slice.is_some() { turn_slice.as_ref().and_then(|slice| slice.last_prompt.clone()) } else { source_meta.as_ref().and_then(|meta| meta.last_prompt.clone()) },
            last_turn_reason: if turn_slice.is_some() { turn_slice.as_ref().and_then(|slice| slice.last_turn_reason.map(|reason| reason.as_str().to_string())) } else { source_meta.as_ref().and_then(|meta| meta.last_turn_reason.clone()) },
            ..Default::default()
        };
        self.services.docs.set(&session_scope_of(&self.handler_scope(), &target_id), "state.json", encode_session_meta(&meta)).await?;
        self.services.index_mirror.record(build_session_summary(SessionSummaryInput {
            id: target_id.clone(),
            workspace_id: self.workspace_id(),
            cwd: Some(host_cwd),
            environment_id: meta.environment_id.clone(),
            environment_cwd: meta.environment_cwd.clone(),
            title: meta.title.clone(),
            last_prompt: meta.last_prompt.clone(),
            created_at: now,
            updated_at: meta.updated_at,
            archived: false,
            archived_at: None,
            custom: meta.custom.clone(),
            last_turn_reason: meta.last_turn_reason.clone(),
        }));
        self.append_session_index_entry(&target_id, &self.services.workspace_context.cwd()).await?;
        fire_listeners(&self.events.did_fork, SessionForkedEvent {
            source_session_id: source_id,
            session_id: target_id,
        });
        drop(quiescence_holds);
        Ok(meta)
    }

    pub async fn create_child(&self, opts: CreateChildSessionOptions) -> Result<SessionMeta, WireAsyncError> {
        let title = match opts.title {
            Some(title) => title,
            None => format!("Child: {}", self.resolve_source_title(&opts.source_session_id).await?.unwrap_or_else(|| opts.source_session_id.clone())),
        };
        let mut metadata = opts.metadata.unwrap_or_default();
        metadata.insert(PARENT_SESSION_ID_KEY.to_string(), opts.source_session_id.clone().into());
        metadata.insert(CHILD_SESSION_KIND_KEY.to_string(), CHILD_SESSION_KIND.into());
        self.fork(ForkSessionOptions {
            source_session_id: opts.source_session_id,
            new_session_id: opts.new_session_id,
            title: Some(title),
            metadata: Some(metadata),
            turn_index: None,
        }).await
    }

    async fn resolve_source_title(&self, source_id: &str) -> Result<Option<String>, WireAsyncError> {
        if let Some(live) = self.state.lock().sessions.get(source_id).cloned() {
            return Ok(live.accessor().get::<ISessionMetadata>().read().await?.title);
        }
        Ok(self.services.index.get(source_id).await?.and_then(|summary| summary.title))
    }

    async fn copy_agent_wire(&self, input: CopyAgentWireInput) -> Result<(), WireAsyncError> {
        let mut records = match input.records {
            Some(records) => records,
            None => self.read_source_wire_records(input.source_handle.as_ref(), &input.source_session_id, &input.agent_id).await?,
        };
        if records.is_empty() {
            records.push(create_wire_metadata_record(None));
        } else if records.first().and_then(|record| record.get("type")).and_then(Value::as_str) != Some("metadata") {
            records.insert(0, create_wire_metadata_record(None));
        }
        records.push(forked_record(&input.agent_id));
        let values = records.into_iter().map(Value::Object).collect::<Vec<_>>();
        self.services.append_log_store.rewrite(
            &agent_scope_of(&session_scope_of(&self.handler_scope(), &input.target_session_id), &input.agent_id),
            AGENT_WIRE_RECORD_KEY,
            &values,
            None,
        ).await
    }

    async fn append_forked_marker(&self, target_session_id: &str, agent_id: &str) -> Result<(), WireAsyncError> {
        let scope = agent_scope_of(&session_scope_of(&self.handler_scope(), target_session_id), agent_id);
        let tolerate = Some(AppendLogReadOptions { on_truncate: Some(Arc::new(|_| {})) });
        let first = {
            let source = self.services.append_log_store.read(&scope, AGENT_WIRE_RECORD_KEY, tolerate.clone());
            futures_util::pin_mut!(source);
            source.next().await.transpose()?
        };
        if first.is_none() {
            self.services.append_log_store.append(&scope, AGENT_WIRE_RECORD_KEY, Value::Object(create_wire_metadata_record(None)), None)?;
            self.services.append_log_store.append(&scope, AGENT_WIRE_RECORD_KEY, Value::Object(forked_record(agent_id)), None)?;
            return Ok(());
        }
        if first.as_ref().and_then(|record| record.get("type")).and_then(Value::as_str) == Some("metadata") {
            self.services.append_log_store.append(&scope, AGENT_WIRE_RECORD_KEY, Value::Object(forked_record(agent_id)), None)?;
            return Ok(());
        }
        let mut records = vec![Value::Object(create_wire_metadata_record(None))];
        let source = self.services.append_log_store.read(&scope, AGENT_WIRE_RECORD_KEY, tolerate);
        futures_util::pin_mut!(source);
        while let Some(record) = source.next().await {
            records.push(record?);
        }
        records.push(Value::Object(forked_record(agent_id)));
        self.services.append_log_store.rewrite(&scope, AGENT_WIRE_RECORD_KEY, &records, None).await
    }

    async fn read_source_wire_records(&self, source_handle: Option<&Arc<ISessionScopeHandle>>, source_session_id: &str, agent_id: &str) -> Result<Vec<WireRecord>, WireAsyncError> {
        if let Some(source_handle) = source_handle {
            if let Some(agent_handle) = source_handle.accessor().get::<IAgentLifecycleService>().handle_of(agent_id) {
                agent_handle.accessor().get::<IEventDispatcher>().flush().await?;
            }
        }
        let scope = agent_scope_of(&session_scope_of(&self.handler_scope(), source_session_id), agent_id);
        let truncation = Arc::new(Mutex::new(None::<AppendLogTruncation>));
        let truncation_sink = truncation.clone();
        let source = self.services.append_log_store.read(&scope, AGENT_WIRE_RECORD_KEY, Some(AppendLogReadOptions {
            on_truncate: Some(Arc::new(move |info| *truncation_sink.lock() = Some(info))),
        }));
        futures_util::pin_mut!(source);
        let mut records = Vec::new();
        while let Some(record) = source.next().await {
            let record = record?;
            if let Some(record) = record.as_object() {
                records.push(record.clone());
            }
        }
        if let Some(truncation) = truncation.lock().take() {
            let values = records.iter().cloned().map(Value::Object).collect::<Vec<_>>();
            repair_wire_journal(&WireJournalRepairServices {
                append_log: self.services.append_log_store.clone(),
                storage: self.services.storage.clone(),
                log: self.services.log.clone(),
                telemetry: self.services.telemetry.clone(),
            }, &scope, AGENT_WIRE_RECORD_KEY, &values, &truncation).await;
        }
        Ok(records)
    }

    async fn prune_truncated_fork_files(&self, target_session_dir: &str, agent_ids: &[String], retained_agent_ids: &[String]) -> Result<(), WireAsyncError> {
        let retained = retained_agent_ids.iter().collect::<HashSet<_>>();
        for agent_id in agent_ids {
            if !retained.contains(agent_id) {
                self.services.host_fs.remove(&join(target_session_dir, &["agents", agent_id])).await?;
            }
        }
        for agent_id in retained_agent_ids {
            let agent_dir = join(target_session_dir, &["agents", agent_id]);
            self.services.host_fs.remove(&join(&agent_dir, &["tasks"])).await?;
            self.services.host_fs.remove(&join(&agent_dir, &["cron"])).await?;
            self.services.host_fs.remove(&join(&agent_dir, &[FILE_HISTORY_BLOB_PREFIX])).await?;
        }
        Ok(())
    }

    async fn copy_session_files(&self, source_dir: &str, target_dir: &str, exclude_wire: bool) -> Result<(), WireAsyncError> {
        let entries = match self.services.host_fs.readdir(source_dir).await {
            Ok(entries) => entries,
            Err(error) if is_missing_file_error(&error) => return Ok(()),
            Err(error) => return Err(error),
        };
        self.copy_session_dir_entries(source_dir, target_dir, entries, "", exclude_wire).await
    }

    fn copy_session_dir_entries<'a>(
        &'a self,
        source_dir: &'a str,
        target_dir: &'a str,
        entries: Vec<HostDirEntry>,
        rel_base: &'a str,
        exclude_wire: bool,
    ) -> BoxFuture<'a, Result<(), WireAsyncError>> {
        async move {
            for entry in entries {
                let rel = if rel_base.is_empty() { entry.name.clone() } else { format!("{rel_base}/{}", entry.name) };
                if matches!(rel.as_str(), "state.json" | "logs" | "upcoming-goals.json")
                    || (exclude_wire && entry.name == AGENT_WIRE_RECORD_KEY)
                    || entry.is_symbolic_link
                {
                    continue;
                }
                let source_path = join(source_dir, &[&entry.name]);
                let target_path = join(target_dir, &[&entry.name]);
                if entry.is_directory {
                    let children = match self.services.host_fs.readdir(&source_path).await {
                        Ok(children) => children,
                        Err(error) if is_missing_file_error(&error) => continue,
                        Err(error) => return Err(error),
                    };
                    self.services.host_fs.mkdir(&target_path, true).await?;
                    self.copy_session_dir_entries(&source_path, &target_path, children, &rel, exclude_wire).await?;
                } else if entry.is_file {
                    let data = self.services.host_fs.read_bytes(&source_path, u64::MAX, None).await?;
                    self.services.host_fs.mkdir(target_dir, true).await?;
                    self.services.host_fs.write_bytes(&target_path, &data).await?;
                }
            }
            Ok(())
        }
        .boxed()
    }

    async fn read_meta_from_disk(&self, session_id: &str) -> Result<Option<SessionMeta>, WireAsyncError> {
        let raw = self.services.docs.get(&session_scope_of(&self.handler_scope(), session_id), "state.json").await?;
        Ok(raw.map(|raw| crate::session::session_metadata::session_metadata_service::normalize_session_meta(raw, session_id)))
    }

    fn clone_handle(&self) -> Self {
        Self {
            services: self.services.clone(),
            state: self.state.clone(),
            events: self.events.clone(),
            _on_dispose: Mutex::new(None),
        }
    }

    fn fire_will_create(&self, event: SessionWillCreateEvent) {
        fire_listeners(&self.events.will_create, event);
    }

    pub fn on_will_create_session(&self, listener: Arc<dyn Fn(SessionWillCreateEvent) + Send + Sync>) -> Box<dyn IDisposable> {
        add_listener(&self.events.will_create, listener)
    }

    pub fn on_did_create_session(&self, listener: Arc<dyn Fn(LifecycleWaitEvent<SessionCreatedEvent>) + Send + Sync>) -> Box<dyn IDisposable> {
        add_listener(&self.events.did_create, listener)
    }

    pub fn on_will_close_session(&self, listener: Arc<dyn Fn(LifecycleWaitEvent<SessionWillCloseEvent>) + Send + Sync>) -> Box<dyn IDisposable> {
        add_listener(&self.events.will_close, listener)
    }

    pub fn on_did_close_session(&self, listener: Arc<dyn Fn(SessionClosedEvent) + Send + Sync>) -> Box<dyn IDisposable> {
        add_listener(&self.events.did_close, listener)
    }

    pub fn on_did_archive_session(&self, listener: Arc<dyn Fn(SessionArchivedEvent) + Send + Sync>) -> Box<dyn IDisposable> {
        add_listener(&self.events.did_archive, listener)
    }

    pub fn on_did_fork_session(&self, listener: Arc<dyn Fn(SessionForkedEvent) + Send + Sync>) -> Box<dyn IDisposable> {
        add_listener(&self.events.did_fork, listener)
    }
}

#[derive(Clone)]
struct SessionContextData {
    session_id: String,
    workspace_id: String,
    session_dir: String,
    meta_scope: String,
    cwd: String,
    scope: String,
}

impl ISessionContext for SessionContextData {
    fn session_id(&self) -> String {
        self.session_id.clone()
    }

    fn workspace_id(&self) -> String {
        self.workspace_id.clone()
    }

    fn session_dir(&self) -> String {
        self.session_dir.clone()
    }

    fn meta_scope(&self) -> String {
        self.meta_scope.clone()
    }

    fn cwd(&self) -> String {
        self.cwd.clone()
    }

    fn scope(&self, sub_key: Option<&str>) -> String {
        match sub_key.filter(|sub_key| !sub_key.is_empty()) {
            Some(sub_key) => format!("{}/{}", self.scope, sub_key),
            None => self.scope.clone(),
        }
    }
}

struct CopyAgentWireInput {
    source_handle: Option<Arc<ISessionScopeHandle>>,
    source_session_id: String,
    agent_id: String,
    target_session_id: String,
    records: Option<Vec<WireRecord>>,
}

#[async_trait]
impl ISessionLifecycleService for SessionLifecycleService {
    fn on_will_create_session(&self, listener: Arc<dyn Fn(SessionWillCreateEvent) + Send + Sync>) -> Box<dyn IDisposable> {
        SessionLifecycleService::on_will_create_session(self, listener)
    }

    fn on_did_create_session(&self, listener: Arc<dyn Fn(LifecycleWaitEvent<SessionCreatedEvent>) + Send + Sync>) -> Box<dyn IDisposable> {
        SessionLifecycleService::on_did_create_session(self, listener)
    }

    fn on_will_close_session(&self, listener: Arc<dyn Fn(LifecycleWaitEvent<SessionWillCloseEvent>) + Send + Sync>) -> Box<dyn IDisposable> {
        SessionLifecycleService::on_will_close_session(self, listener)
    }

    fn on_did_close_session(&self, listener: Arc<dyn Fn(SessionClosedEvent) + Send + Sync>) -> Box<dyn IDisposable> {
        SessionLifecycleService::on_did_close_session(self, listener)
    }

    fn on_did_archive_session(&self, listener: Arc<dyn Fn(SessionArchivedEvent) + Send + Sync>) -> Box<dyn IDisposable> {
        SessionLifecycleService::on_did_archive_session(self, listener)
    }

    fn on_did_fork_session(&self, listener: Arc<dyn Fn(SessionForkedEvent) + Send + Sync>) -> Box<dyn IDisposable> {
        SessionLifecycleService::on_did_fork_session(self, listener)
    }

    async fn create(&self, opts: CreateSessionOptions) -> Result<Arc<ISessionScopeHandle>, WireAsyncError> {
        SessionLifecycleService::create(self, opts).await
    }

    fn get(&self, session_id: &str) -> Option<Arc<ISessionScopeHandle>> {
        SessionLifecycleService::get(self, session_id)
    }

    fn list(&self) -> Vec<Arc<ISessionScopeHandle>> {
        SessionLifecycleService::list(self)
    }

    async fn resume(&self, session_id: &str, opts: Option<ResumeSessionOptions>) -> Result<Option<Arc<ISessionScopeHandle>>, WireAsyncError> {
        SessionLifecycleService::resume(self, session_id, opts).await
    }

    async fn close(&self, session_id: &str) -> Result<(), WireAsyncError> {
        SessionLifecycleService::close(self, session_id).await
    }

    async fn archive(&self, session_id: &str) -> Result<(), WireAsyncError> {
        SessionLifecycleService::archive(self, session_id).await
    }

    async fn restore(&self, session_id: &str, opts: Option<ResumeSessionOptions>) -> Result<Option<Arc<ISessionScopeHandle>>, WireAsyncError> {
        SessionLifecycleService::restore(self, session_id, opts).await
    }

    async fn delete(&self, session_id: &str) -> Result<(), WireAsyncError> {
        SessionLifecycleService::delete(self, session_id).await
    }

    async fn fork(&self, opts: ForkSessionOptions) -> Result<SessionMeta, WireAsyncError> {
        SessionLifecycleService::fork(self, opts).await
    }

    async fn create_child(&self, opts: CreateChildSessionOptions) -> Result<SessionMeta, WireAsyncError> {
        SessionLifecycleService::create_child(self, opts).await
    }
}

fn session_environment(environment_id: Option<&str>, cwd: Option<&str>) -> Result<Option<EnvironmentBinding>, WireAsyncError> {
    if environment_id.is_none() || environment_id == Some(LOCAL_ENVIRONMENT_ID) {
        return Ok(cwd.map(|cwd| EnvironmentBinding {
            environment_id: LOCAL_ENVIRONMENT_ID.to_string(),
            cwd: cwd.to_string(),
        }));
    }
    let environment_id = environment_id.unwrap();
    let Some(cwd) = cwd else {
        return Err(Box::new(EnvironmentError::new(
            "environment.invalid_cwd",
            format!("environment {environment_id} requires a cwd"),
            None,
        )));
    };
    Ok(Some(EnvironmentBinding {
        environment_id: environment_id.to_string(),
        cwd: cwd.to_string(),
    }))
}

fn create_session_id() -> String {
    format!("session_{}", uuid::Uuid::new_v4())
}

fn forked_record(agent_id: &str) -> WireRecord {
    json!({
        "type": "forked",
        "agentId": agent_id,
        "time": chrono::Utc::now().timestamp_millis(),
    })
    .as_object()
    .cloned()
    .unwrap()
}

fn fork_custom_metadata(source: Option<HashMap<String, Value>>, input: Option<HashMap<String, Value>>) -> Option<HashMap<String, Value>> {
    let mut merged = without_goal(source);
    merged.extend(without_goal(input));
    (!merged.is_empty()).then_some(merged)
}

fn without_goal(value: Option<HashMap<String, Value>>) -> HashMap<String, Value> {
    let mut value = value.unwrap_or_default();
    value.remove("goal");
    value
}

fn is_missing_file_error(error: &WireAsyncError) -> bool {
    error
        .downcast_ref::<crate::os::interface::host_fs_errors::HostFsError>()
        .and_then(|error| error.errno())
        == Some("ENOENT")
}

fn error_code<'a>(error: &'a (dyn Error + 'static)) -> Option<&'a str> {
    error.downcast_ref::<Error2>().map(|error| error.code().as_str())
}

fn add_listener<T: 'static>(
    listeners: &Arc<Mutex<LifecycleListeners<T>>>,
    listener: Arc<dyn Fn(T) + Send + Sync>,
) -> Box<dyn IDisposable> {
    let id = {
        let mut listeners = listeners.lock();
        let id = listeners.next_id;
        listeners.next_id += 1;
        listeners.listeners.insert(id, listener);
        id
    };
    let listeners = listeners.clone();
    to_disposable(move || {
        listeners.lock().listeners.remove(&id);
    })
}

fn fire_listeners<T: Clone>(listeners: &Arc<Mutex<LifecycleListeners<T>>>, event: T) {
    let snapshot = listeners.lock().listeners.values().cloned().collect::<Vec<_>>();
    for listener in snapshot {
        listener(event.clone());
    }
}

async fn fire_wait_listeners<T: Clone + 'static>(
    listeners: &Arc<Mutex<LifecycleListeners<LifecycleWaitEvent<T>>>>,
    event: T,
) -> Result<(), WireAsyncError> {
    let pending = Arc::new(Mutex::new(Vec::new()));
    let pending_sink = pending.clone();
    let event = LifecycleWaitEvent {
        event,
        wait_until: Arc::new(move |future| pending_sink.lock().push(future)),
    };
    fire_listeners(listeners, event);
    for result in futures_util::future::join_all(pending.lock().drain(..).collect::<Vec<_>>()).await {
        result?;
    }
    Ok(())
}
