use std::sync::LazyLock;
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::_base::di::scope::ISessionScopeHandle;
use crate::agent::profile::profile::BindAgentInput;
use crate::mcp_core::config_schema::McpServerConfig;
use crate::session::session_metadata::session_metadata::SessionMeta;
use crate::wire::record::WireAsyncError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionCreateSource {
    Startup,
    Resume,
    Reclaimed,
    Fork,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionCloseReason {
    Exit,
    Reclaim,
    Archive,
}

#[derive(Clone)]
pub struct CreateSessionOptions {
    pub session_id: Option<String>,
    pub work_dir: String,
    pub additional_dirs: Option<Vec<String>>,
    pub main_agent_binding: Option<BindAgentInput>,
    pub ensure_main_agent: bool,
    pub environment_id: Option<String>,
    pub environment_cwd: Option<String>,
    pub mcp_servers: Option<HashMap<String, McpServerConfig>>,
}

#[derive(Clone, Default)]
pub struct ForkSessionOptions {
    pub source_session_id: String,
    pub new_session_id: Option<String>,
    pub title: Option<String>,
    pub metadata: Option<HashMap<String, Value>>,
    pub turn_index: Option<f64>,
}

#[derive(Clone, Default)]
pub struct ResumeSessionOptions {
    pub additional_dirs: Option<Vec<String>>,
    pub mcp_servers: Option<HashMap<String, McpServerConfig>>,
}

#[derive(Clone, Default)]
pub struct CreateChildSessionOptions {
    pub source_session_id: String,
    pub new_session_id: Option<String>,
    pub title: Option<String>,
    pub metadata: Option<HashMap<String, Value>>,
}

#[derive(Clone)]
pub struct SessionCreatedEvent {
    pub session_id: String,
    pub handle: Arc<ISessionScopeHandle>,
    pub source: SessionCreateSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionClosedEvent {
    pub session_id: String,
}

#[derive(Clone)]
pub struct SessionWillCloseEvent {
    pub session_id: String,
    pub handle: Arc<ISessionScopeHandle>,
    pub reason: SessionCloseReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionArchivedEvent {
    pub session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionForkedEvent {
    pub source_session_id: String,
    pub session_id: String,
}

pub type LifecycleWaitUntil = Arc<
    dyn Fn(BoxFuture<'static, Result<(), WireAsyncError>>) + Send + Sync,
>;

#[derive(Clone)]
pub struct LifecycleWaitEvent<T> {
    pub event: T,
    pub wait_until: LifecycleWaitUntil,
}

#[derive(Clone)]
pub struct SessionWillCreateEvent {
    pub session_id: String,
    pub on_did_create_scope: Arc<
        dyn Fn(Arc<dyn Fn(Arc<ISessionScopeHandle>) + Send + Sync>) -> DisposableRef
            + Send
            + Sync,
    >,
    pub read_seed: Arc<dyn Fn(&str) -> Value + Send + Sync>,
    pub contribute_seed: Arc<dyn Fn(&str, Value) + Send + Sync>,
    pub on_session_dispose: Arc<dyn Fn(Box<dyn FnOnce() + Send>) + Send + Sync>,
}

#[async_trait]
pub trait ISessionLifecycleService: Send + Sync {
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
    fn on_did_close_session(
        &self,
        listener: Arc<dyn Fn(SessionClosedEvent) + Send + Sync>,
    ) -> DisposableRef;
    fn on_did_archive_session(
        &self,
        listener: Arc<dyn Fn(SessionArchivedEvent) + Send + Sync>,
    ) -> DisposableRef;
    fn on_did_fork_session(
        &self,
        listener: Arc<dyn Fn(SessionForkedEvent) + Send + Sync>,
    ) -> DisposableRef;
    async fn create(
        &self,
        opts: CreateSessionOptions,
    ) -> Result<Arc<ISessionScopeHandle>, WireAsyncError>;
    fn get(&self, session_id: &str) -> Option<Arc<ISessionScopeHandle>>;
    fn list(&self) -> Vec<Arc<ISessionScopeHandle>>;
    async fn resume(
        &self,
        session_id: &str,
        opts: Option<ResumeSessionOptions>,
    ) -> Result<Option<Arc<ISessionScopeHandle>>, WireAsyncError>;
    async fn close(&self, session_id: &str) -> Result<(), WireAsyncError>;
    async fn archive(&self, session_id: &str) -> Result<(), WireAsyncError>;
    async fn restore(
        &self,
        session_id: &str,
        opts: Option<ResumeSessionOptions>,
    ) -> Result<Option<Arc<ISessionScopeHandle>>, WireAsyncError>;
    async fn delete(&self, session_id: &str) -> Result<(), WireAsyncError>;
    async fn fork(&self, opts: ForkSessionOptions) -> Result<SessionMeta, WireAsyncError>;
    async fn create_child(&self, opts: CreateChildSessionOptions) -> Result<SessionMeta, WireAsyncError>;
}


pub static I_SESSION_LIFECYCLE_SERVICE: LazyLock<ServiceIdentifier<Arc<dyn ISessionLifecycleService>>> =
    LazyLock::new(|| create_decorator("sessionLifecycleService"));
