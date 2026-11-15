use std::error::Error;
use std::fmt;
use std::sync::Arc;

use futures_util::future::{BoxFuture, Shared};
use futures_util::FutureExt;
use parking_lot::Mutex;

use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::_base::log::log::ILogService;
use crate::app::agent_profile_catalog::agent_profile_contribution::AgentProfileContribution;
use crate::app::agent_profile_catalog::agent_profile_registry::{
    AgentProfileRegistration, IAgentProfileRegistry,
};
use crate::wire::record::WireAsyncError;

#[derive(Debug, Clone)]
struct SharedLoaderError(Arc<String>);

impl SharedLoaderError {
    fn new(error: WireAsyncError) -> Self {
        Self(Arc::new(error.to_string()))
    }

    fn boxed(self) -> WireAsyncError {
        Box::new(self)
    }
}

impl fmt::Display for SharedLoaderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.as_str())
    }
}

impl Error for SharedLoaderError {}

type LoaderFuture = Shared<BoxFuture<'static, Result<(), SharedLoaderError>>>;
pub type AgentProfileLoad = Arc<
    dyn Fn() -> BoxFuture<'static, Result<AgentProfileContribution, WireAsyncError>>
        + Send
        + Sync,
>;
pub type AgentProfileProvide = Arc<
    dyn Fn(AgentProfileRegistration) -> DisposableRef + Send + Sync,
>;

#[derive(Clone)]
pub struct AgentProfileLoaderHooks {
    pub source_id: String,
    pub priority: i64,
    pub fatal: bool,
    pub workspace_key: Option<String>,
    pub context_key: Option<String>,
    pub load: AgentProfileLoad,
}

struct AgentProfileLoaderState {
    ready: Option<LoaderFuture>,
    tail: Option<LoaderFuture>,
    contribution_handle: Option<DisposableRef>,
    is_disposed: bool,
}

pub struct AgentProfileLoaderBase {
    log: Arc<dyn ILogService>,
    registry: Option<Arc<dyn IAgentProfileRegistry>>,
    provide: AgentProfileProvide,
    hooks: AgentProfileLoaderHooks,
    state: Arc<Mutex<AgentProfileLoaderState>>,
}

impl AgentProfileLoaderBase {
    pub fn new(
        log: Arc<dyn ILogService>,
        registry: Option<Arc<dyn IAgentProfileRegistry>>,
        provide: AgentProfileProvide,
        hooks: AgentProfileLoaderHooks,
    ) -> Self {
        Self {
            log,
            registry,
            provide,
            hooks,
            state: Arc::new(Mutex::new(AgentProfileLoaderState {
                ready: None,
                tail: None,
                contribution_handle: None,
                is_disposed: false,
            })),
        }
    }

    pub fn ready(&self) -> BoxFuture<'static, Result<(), WireAsyncError>> {
        let ready = self.state.lock().ready.clone();
        async move {
            if let Some(ready) = ready {
                ready.await.map_err(SharedLoaderError::boxed)
            } else {
                Ok(())
            }
        }
        .boxed()
    }

    pub fn start(&self) {
        let ready = self.enqueue();
        self.state.lock().ready = Some(ready.clone());
        tokio::spawn(ready);
    }

    pub async fn reload(&self) -> Result<(), WireAsyncError> {
        let ready = self.enqueue();
        self.state.lock().ready = Some(ready.clone());
        tokio::spawn(ready.clone());
        ready.await.map_err(SharedLoaderError::boxed)
    }

    pub fn workspace_key(&self) -> Option<String> {
        self.hooks.workspace_key.clone()
    }

    pub fn dispose(&self) {
        let handle = {
            let mut state = self.state.lock();
            state.is_disposed = true;
            state.contribution_handle.take()
        };
        if let Some(handle) = handle {
            handle.dispose();
        }
    }

    fn enqueue(&self) -> LoaderFuture {
        let previous = self.state.lock().tail.clone();
        let this = Self {
            log: self.log.clone(),
            registry: self.registry.clone(),
            provide: self.provide.clone(),
            hooks: self.hooks.clone(),
            state: self.state.clone(),
        };
        let current = async move {
            if let Some(previous) = previous {
                let _ = previous.await;
            }
            this.load_and_contribute().await
        }
        .boxed()
        .shared();
        self.state.lock().tail = Some(current.clone());
        current
    }

    async fn load_and_contribute(&self) -> Result<(), SharedLoaderError> {
        let result = async {
            let contribution = (self.hooks.load)().await?;
            let registration = AgentProfileRegistration {
                source_id: self.hooks.source_id.clone(),
                priority: self.hooks.priority,
                workspace_key: self.hooks.workspace_key.clone(),
                context_key: self.hooks.context_key.clone(),
                contribution,
            };
            let handle = match &self.registry {
                Some(registry) => registry.register(registration),
                None => (self.provide)(registration),
            };
            let previous = {
                let mut state = self.state.lock();
                if state.is_disposed {
                    drop(state);
                    handle.dispose();
                    return Ok(());
                }
                state.contribution_handle.replace(handle)
            };
            if let Some(previous) = previous {
                previous.dispose();
            }
            Ok(())
        }
        .await;
        match result {
            Ok(()) => Ok(()),
            Err(error) if self.hooks.fatal => Err(SharedLoaderError::new(error)),
            Err(error) => {
                self.log.warn(
                    &format!(
                        "agent profile loader \"{}\" load failed: {error}",
                        self.hooks.source_id
                    ),
                    None,
                );
                Ok(())
            }
        }
    }
}
