use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::future::BoxFuture;
use once_cell::sync::Lazy;

use crate::_base::di::descriptors::DynamicValue;
use crate::_base::di::instantiation::{ServiceIdentifier, ServicesAccessor, create_decorator};
use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::_base::di::scope::ISessionScopeHandle;
use crate::agent::profile::profile::BindAgentInput;
use crate::app::session_index::session_index::SessionSummary;
use crate::mcp_core::config_schema::McpServerConfig;
use crate::session::session_metadata::session_metadata::SessionMeta;
use crate::wire::record::WireAsyncError;
use crate::workspace::session_lifecycle::session_lifecycle::{
    CreateChildSessionOptions, CreateSessionOptions, ForkSessionOptions, LifecycleWaitEvent,
    ResumeSessionOptions, SessionArchivedEvent, SessionClosedEvent, SessionCreatedEvent,
    SessionForkedEvent, SessionWillCloseEvent, SessionWillCreateEvent,
};

#[derive(Clone)]
pub struct CreateManagedSessionOptions {
    pub session_id: Option<String>,
    pub work_dir: String,
    pub additional_dirs: Option<Vec<String>>,
    pub main_agent_binding: Option<BindAgentInput>,
    pub ensure_main_agent: bool,
    pub environment_id: Option<String>,
    pub environment_cwd: Option<String>,
    pub mcp_servers: Option<HashMap<String, McpServerConfig>>,
    pub workspace_id: Option<String>,
}

impl CreateManagedSessionOptions {
    pub fn into_create_options(self) -> CreateSessionOptions {
        self.into()
    }
}

impl From<CreateManagedSessionOptions> for CreateSessionOptions {
    fn from(options: CreateManagedSessionOptions) -> Self {
        Self {
            session_id: options.session_id,
            work_dir: options.work_dir,
            additional_dirs: options.additional_dirs,
            main_agent_binding: options.main_agent_binding,
            ensure_main_agent: options.ensure_main_agent,
            environment_id: options.environment_id,
            environment_cwd: options.environment_cwd,
            mcp_servers: options.mcp_servers,
        }
    }
}

impl From<CreateSessionOptions> for CreateManagedSessionOptions {
    fn from(options: CreateSessionOptions) -> Self {
        Self {
            session_id: options.session_id,
            work_dir: options.work_dir,
            additional_dirs: options.additional_dirs,
            main_agent_binding: options.main_agent_binding,
            ensure_main_agent: options.ensure_main_agent,
            environment_id: options.environment_id,
            environment_cwd: options.environment_cwd,
            mcp_servers: options.mcp_servers,
            workspace_id: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreationEnvironment {
    pub workspace_id: String,
    pub environment_id: Option<String>,
    pub environment_cwd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionWillDeleteEvent {
    pub session_id: String,
}

#[async_trait]
pub trait UnguardedSessionLifecycle: Send + Sync {
    async fn archive(&self) -> Result<(), WireAsyncError>;
    async fn restore(
        &self,
        options: Option<ResumeSessionOptions>,
    ) -> Result<Option<Arc<ISessionScopeHandle>>, WireAsyncError>;
}

pub type SessionLifecycleOutput = Box<dyn Any + Send>;
pub type SessionLifecycleWork = Box<
    dyn FnOnce(
            Arc<dyn UnguardedSessionLifecycle>,
        ) -> BoxFuture<'static, Result<SessionLifecycleOutput, WireAsyncError>>
        + Send,
>;

pub trait SessionLifecycleEvents: Send + Sync {
    fn on_did_close_session(
        &self,
        listener: Arc<dyn Fn(SessionClosedEvent) + Send + Sync>,
    ) -> DisposableRef;
    fn on_did_archive_session(
        &self,
        listener: Arc<dyn Fn(SessionArchivedEvent) + Send + Sync>,
    ) -> DisposableRef;
}

#[async_trait]
pub trait ISessionManager: SessionLifecycleEvents {
    fn on_will_create_session(
        &self,
        listener: Arc<dyn Fn(SessionWillCreateEvent) + Send + Sync>,
    ) -> DisposableRef;
    fn on_did_create_session(
        &self,
        listener: Arc<dyn Fn(LifecycleWaitEvent<SessionCreatedEvent>) + Send + Sync>,
    ) -> DisposableRef;
    fn on_will_close_session(
        &self,
        listener: Arc<dyn Fn(LifecycleWaitEvent<SessionWillCloseEvent>) + Send + Sync>,
    ) -> DisposableRef;
    fn on_will_delete_session(
        &self,
        listener: Arc<dyn Fn(LifecycleWaitEvent<SessionWillDeleteEvent>) + Send + Sync>,
    ) -> DisposableRef;
    fn on_did_fork_session(
        &self,
        listener: Arc<dyn Fn(SessionForkedEvent) + Send + Sync>,
    ) -> DisposableRef;
    async fn resolve_creation_environment(
        &self,
        options: CreateManagedSessionOptions,
    ) -> Result<CreationEnvironment, WireAsyncError>;
    async fn create(
        &self,
        options: CreateManagedSessionOptions,
    ) -> Result<Arc<ISessionScopeHandle>, WireAsyncError>;
    async fn resume(
        &self,
        session_id: &str,
        options: Option<ResumeSessionOptions>,
    ) -> Result<Option<Arc<ISessionScopeHandle>>, WireAsyncError>;
    fn get(&self, session_id: &str) -> Option<Arc<ISessionScopeHandle>>;
    async fn status(&self, session_id: &str) -> Result<Option<SessionSummary>, WireAsyncError>;
    async fn when_resume_settled(&self, session_id: &str) -> Result<(), WireAsyncError>;
    fn with_lifecycle_serialization(
        &self,
        session_id: &str,
        work: SessionLifecycleWork,
    ) -> BoxFuture<'static, Result<SessionLifecycleOutput, WireAsyncError>>;
    fn list(&self) -> Vec<Arc<ISessionScopeHandle>>;
    async fn close(&self, session_id: &str) -> Result<(), WireAsyncError>;
    async fn close_if_idle(
        &self,
        session_id: &str,
        idle_timeout_ms: i64,
    ) -> Result<bool, WireAsyncError>;
    async fn archive(&self, session_id: &str) -> Result<(), WireAsyncError>;
    async fn restore(
        &self,
        session_id: &str,
        options: Option<ResumeSessionOptions>,
    ) -> Result<Option<Arc<ISessionScopeHandle>>, WireAsyncError>;
    async fn delete(&self, session_id: &str) -> Result<(), WireAsyncError>;
    async fn fork(&self, options: ForkSessionOptions) -> Result<SessionMeta, WireAsyncError>;
    async fn create_child(
        &self,
        options: CreateChildSessionOptions,
    ) -> Result<SessionMeta, WireAsyncError>;
}

pub static I_SESSION_MANAGER: Lazy<ServiceIdentifier<Arc<dyn ISessionManager>>> =
    Lazy::new(|| create_decorator("sessionManager"));

pub type SessionManagerAccessor = dyn ServicesAccessor;
