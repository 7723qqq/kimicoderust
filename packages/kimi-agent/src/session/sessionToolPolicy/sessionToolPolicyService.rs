use crate::_base::state::state_registry::IStateRegistryExt;
use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use futures_util::future::{BoxFuture, Shared, join_all};
use futures_util::FutureExt;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::_base::di::lifecycle::{DisposableRef, IDisposable, to_disposable};
use crate::persistence::interface::atomic_document_store::IAtomicDocumentStore;
use crate::session::session_context::session_context::ISessionContext;
use crate::session::state::session_state::ISessionStateService;
use crate::state::state::{StateDefinition, define_state};
use crate::wire::record::WireAsyncError;

use super::session_tool_policy::{
    ISessionToolPolicy, SessionToolPolicyChangedEvent,
};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionToolPolicyState {
    pub disabled_tools: Vec<String>,
}

pub static SESSION_TOOL_POLICY_STATE_KEY: LazyLock<StateDefinition<SessionToolPolicyState>> =
    LazyLock::new(|| {
        define_state(
            "sessionToolPolicy.state",
            Arc::new(SessionToolPolicyState::default),
        )
    });

pub const STATE_KEY: &str = "state.json";

#[derive(Debug, Clone)]
struct SharedPolicyError(Arc<String>);

impl SharedPolicyError {
    fn new(error: WireAsyncError) -> Self {
        Self(Arc::new(error.to_string()))
    }

    fn boxed(self) -> WireAsyncError {
        Box::new(self)
    }
}

impl fmt::Display for SharedPolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.as_str())
    }
}

impl Error for SharedPolicyError {}

type PolicyFuture = Shared<BoxFuture<'static, Result<(), SharedPolicyError>>>;

pub struct SessionToolPolicyService {
    states: Arc<dyn ISessionStateService>,
    store: Arc<dyn IAtomicDocumentStore>,
    scope: String,
    ready: PolicyFuture,
    listeners: Arc<Mutex<Vec<(u64, Arc<dyn Fn(SessionToolPolicyChangedEvent) + Send + Sync>)>>>,
    next_listener_id: std::sync::atomic::AtomicU64,
    update_queue: Arc<Mutex<PolicyFuture>>,
}

impl SessionToolPolicyService {
    pub fn new(
        states: Arc<dyn ISessionStateService>,
        session_context: Arc<dyn ISessionContext>,
        store: Arc<dyn IAtomicDocumentStore>,
    ) -> Self {
        states.contribute_state(&SESSION_TOOL_POLICY_STATE_KEY);
        let scope = session_context.scope(Some("tool-policy"));
        let ready_states = states.clone();
        let ready_store = store.clone();
        let ready_scope = scope.clone();
        let ready = async move {
            let stored = ready_store
                .get(&ready_scope, STATE_KEY)
                .await
                .map_err(SharedPolicyError::new)?;
            if let Some(stored) = stored {
                let stored = serde_json::from_value::<SessionToolPolicyState>(stored)
                    .map_err(|error| SharedPolicyError(Arc::new(error.to_string())))?;
                ready_states.set(
                    &SESSION_TOOL_POLICY_STATE_KEY,
                    SessionToolPolicyState {
                        disabled_tools: dedup(stored.disabled_tools),
                    },
                );
            }
            Ok(())
        }
        .boxed()
        .shared();
        Self {
            states,
            store,
            scope,
            ready,
            listeners: Arc::new(Mutex::new(Vec::new())),
            next_listener_id: std::sync::atomic::AtomicU64::new(0),
            update_queue: Arc::new(Mutex::new(async { Ok(()) }.boxed().shared())),
        }
    }

    pub fn disabled_tools(&self) -> Vec<String> {
        self.states
            .get(&SESSION_TOOL_POLICY_STATE_KEY)
            .disabled_tools
    }

    pub fn set_disabled_tools(&self, names: Vec<String>) -> PolicyFuture {
        let previous = self.update_queue.lock().clone();
        let states = self.states.clone();
        let store = self.store.clone();
        let scope = self.scope.clone();
        let ready = self.ready.clone();
        let listeners = self.listeners.clone();
        let run = async move {
            let _ = previous.await;
            replace(
                states,
                store,
                scope,
                ready,
                listeners,
                names,
            )
            .await
            .map_err(SharedPolicyError::new)
        }
        .boxed()
        .shared();
        *self.update_queue.lock() = async move {
            let _ = run.clone().await;
            Ok(())
        }
        .boxed()
        .shared();
        run
    }

    async fn fire_change(&self) -> Result<(), WireAsyncError> {
        fire_listeners(self.listeners.clone()).await
    }
}

#[async_trait]
impl ISessionToolPolicy for SessionToolPolicyService {
    async fn ready(&self) -> Result<(), WireAsyncError> {
        self.ready.clone().await.map_err(SharedPolicyError::boxed)
    }

    fn on_did_change(
        &self,
        listener: Arc<dyn Fn(SessionToolPolicyChangedEvent) + Send + Sync>,
    ) -> DisposableRef {
        let id = self.next_listener_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.listeners.lock().push((id, listener));
        let listeners = Arc::downgrade(&self.listeners);
        to_disposable(move || {
            if let Some(listeners) = listeners.upgrade() {
                listeners.lock().retain(|(candidate, _)| *candidate != id);
            }
        })
    }

    fn disabled_tools(&self) -> Vec<String> {
        SessionToolPolicyService::disabled_tools(self)
    }

    async fn set_disabled_tools(&self, names: Vec<String>) -> Result<(), WireAsyncError> {
        self.set_disabled_tools(names)
            .await
            .map_err(SharedPolicyError::boxed)
    }
}

async fn replace(
    states: Arc<dyn ISessionStateService>,
    store: Arc<dyn IAtomicDocumentStore>,
    scope: String,
    ready: PolicyFuture,
    listeners: Arc<Mutex<Vec<(u64, Arc<dyn Fn(SessionToolPolicyChangedEvent) + Send + Sync>)>>>,
    names: Vec<String>,
) -> Result<(), WireAsyncError> {
    ready.await.map_err(SharedPolicyError::boxed)?;
    let disabled_tools = dedup(names);
    let current = states.get(&SESSION_TOOL_POLICY_STATE_KEY).disabled_tools;
    if disabled_tools == current {
        return Ok(());
    }
    let next_state = SessionToolPolicyState { disabled_tools };
    store
        .set(&scope, STATE_KEY, serde_json::to_value(&next_state).unwrap())
        .await?;
    states.set(&SESSION_TOOL_POLICY_STATE_KEY, next_state);
    fire_listeners(listeners).await
}

async fn fire_listeners(
    listeners: Arc<Mutex<Vec<(u64, Arc<dyn Fn(SessionToolPolicyChangedEvent) + Send + Sync>)>>>,
) -> Result<(), WireAsyncError> {
    let pending = Arc::new(Mutex::new(Vec::<BoxFuture<'static, Result<(), WireAsyncError>>>::new()));
    let callbacks = listeners.lock().clone();
    for (_, listener) in callbacks {
        let pending = pending.clone();
        listener(SessionToolPolicyChangedEvent {
            wait_until: Arc::new(move |future| {
                pending.lock().push(future);
            }),
        });
    }
    let futures = std::mem::take(&mut *pending.lock());
    for result in join_all(futures).await {
        result?;
    }
    Ok(())
}

fn dedup(names: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    names
        .into_iter()
        .filter(|name| seen.insert(name.clone()))
        .collect()
}
