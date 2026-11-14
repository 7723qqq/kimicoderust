use std::sync::Arc;

use futures_util::future::BoxFuture;
use kimi_core::primitives::errors::Error2;
use serde_json::Map;

use crate::_base::di::instantiation::ServicesAccessor;
use crate::_base::di::lifecycle::{DisposableRef, DisposableStore};
use crate::_base::di::scope::ISessionScopeHandle;
use crate::app::session_index::session_index::ISessionIndex;
use crate::app::telemetry::telemetry::ITelemetryService;
use crate::app::workspace_lifecycle::workspace_lifecycle::{
    IWorkspaceLifecycleService, IWorkspaceScopeHandle, WorkspaceRef,
};
use crate::session::session_context::session_context::ISessionContext;
use crate::wire::record::WireAsyncError;
use crate::workspace::session_lifecycle::session_lifecycle::{
    ISessionLifecycleService, ResumeSessionOptions,
};
use crate::workspace::workspace_instance::workspace_instance_manager::{
    IWorkspaceInstanceManager, WorkspaceInstanceLease, WorkspaceInstanceRef,
};

use super::session_manager::{ISessionManager, SessionLifecycleEvents};

pub async fn handler_for_session(
    accessor: &dyn ServicesAccessor,
    session_id: &str,
) -> Result<Option<Arc<dyn IWorkspaceScopeHandle>>, WireAsyncError> {
    let Some(summary) = accessor.get::<ISessionIndex>().get(session_id).await? else {
        return Ok(None);
    };
    let result = accessor
        .get::<IWorkspaceLifecycleService>()
        .handler_for(WorkspaceRef::WorkspaceId {
            workspace_id: summary.workspace_id,
            root: summary.cwd,
        })
        .await;
    match result {
        Ok(handler) => Ok(Some(handler)),
        Err(error)
            if error.downcast_ref::<Error2>().is_some_and(|error| {
                error.code().as_str() == "workspace.not_found"
            }) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

pub fn live_handler_for_session(
    accessor: &dyn ServicesAccessor,
    session_id: &str,
) -> Option<Arc<dyn IWorkspaceScopeHandle>> {
    for handler in accessor.get::<IWorkspaceLifecycleService>().handlers().list() {
        if handler
            .accessor()
            .get::<ISessionLifecycleService>()
            .get(session_id)
            .is_some()
        {
            return Some(handler);
        }
    }
    None
}

pub fn follow_workspace_handlers(
    accessor: &dyn ServicesAccessor,
    follow: Arc<
        dyn Fn(Arc<dyn ISessionLifecycleService>) -> DisposableRef + Send + Sync,
    >,
) -> DisposableRef {
    let lifecycle = accessor.get::<IWorkspaceLifecycleService>();
    let store = DisposableStore::new();
    for handler in lifecycle.handlers().list() {
        store.add(follow(handler.accessor().get::<ISessionLifecycleService>()));
    }
    let followed_store = store.clone();
    store.add(Arc::from(lifecycle.on_did_materialize_handler(Arc::new(
        move |handler| {
            if !followed_store.is_disposed() {
                followed_store.add(follow(
                    handler.accessor().get::<ISessionLifecycleService>(),
                ));
            }
        },
    ))));
    Arc::new(store)
}

pub async fn program_for_session(
    accessor: &dyn ServicesAccessor,
    session_id: &str,
) -> Result<Option<WorkspaceInstanceLease>, WireAsyncError> {
    let manager = accessor.get::<ISessionManager>();
    if let Some(live) = manager.get(session_id) {
        let context = live.accessor().get::<ISessionContext>();
        return accessor
            .get::<IWorkspaceInstanceManager>()
            .acquire(WorkspaceInstanceRef::WorkspaceId {
                workspace_id: context.workspace_id(),
                root: Some(context.cwd()),
            })
            .await
            .map(Some);
    }
    let Some(summary) = accessor.get::<ISessionIndex>().get(session_id).await? else {
        return Ok(None);
    };
    accessor
        .get::<IWorkspaceInstanceManager>()
        .acquire(WorkspaceInstanceRef::WorkspaceId {
            workspace_id: summary.workspace_id,
            root: summary.cwd,
        })
        .await
        .map(Some)
}

pub async fn resume_session_by_id(
    accessor: &dyn ServicesAccessor,
    session_id: &str,
    options: Option<ResumeSessionOptions>,
) -> Result<Option<Arc<ISessionScopeHandle>>, WireAsyncError> {
    let result = accessor
        .get::<ISessionManager>()
        .resume(session_id, options)
        .await;
    if let Err(error) = &result {
        let reason = error
            .downcast_ref::<Error2>()
            .map(|error| error.code().as_str())
            .unwrap_or("Error");
        let properties = Map::from_iter([(
            "reason".to_string(),
            reason.to_string().into(),
        )]);
        accessor
            .get::<ITelemetryService>()
            .with_context(Map::from_iter([(
                "session_id".to_string(),
                session_id.to_string().into(),
            )]))
            .track2("session_load_failed", Some(properties));
    }
    result
}

pub fn get_live_session_by_id(
    accessor: &dyn ServicesAccessor,
    session_id: &str,
) -> Option<Arc<ISessionScopeHandle>> {
    accessor.get::<ISessionManager>().get(session_id)
}

pub async fn close_session_by_id(
    accessor: &dyn ServicesAccessor,
    session_id: &str,
) -> Result<(), WireAsyncError> {
    accessor.get::<ISessionManager>().close(session_id).await
}

pub type SessionReloadedCallback = Arc<
    dyn Fn(
            Arc<ISessionScopeHandle>,
        ) -> BoxFuture<'static, Result<(), WireAsyncError>>
        + Send
        + Sync,
>;

#[derive(Clone, Default)]
pub struct StaleReloadOptions {
    pub retry: Option<bool>,
    pub on_reloaded: Option<SessionReloadedCallback>,
}

pub async fn reload_on_stale<T>(
    accessor: &dyn ServicesAccessor,
    session_id: &str,
    run: Arc<
        dyn Fn() -> BoxFuture<'static, Result<T, WireAsyncError>> + Send + Sync,
    >,
    options: Option<StaleReloadOptions>,
) -> Result<T, WireAsyncError>
where
    T: Send + 'static,
{
    let error = match run().await {
        Ok(value) => return Ok(value),
        Err(error) => error,
    };
    if !error.downcast_ref::<Error2>().is_some_and(|error| {
        error.code().as_str() == "session.stale"
    }) {
        return Err(error);
    }
    close_session_by_id(accessor, session_id).await?;
    let Some(session) = resume_session_by_id(accessor, session_id, None).await? else {
        return Err(error);
    };
    if let Some(on_reloaded) = options.as_ref().and_then(|options| options.on_reloaded.as_ref()) {
        on_reloaded(session).await?;
    }
    if options.as_ref().and_then(|options| options.retry) == Some(false) {
        return Err(error);
    }
    run().await
}

pub fn follow_session_lifecycles(
    accessor: &dyn ServicesAccessor,
    follow: Arc<dyn Fn(Arc<dyn SessionLifecycleEvents>) -> DisposableRef + Send + Sync>,
) -> DisposableRef {
    let manager = accessor.get::<ISessionManager>();
    let events: Arc<dyn SessionLifecycleEvents> = manager;
    follow(events)
}
