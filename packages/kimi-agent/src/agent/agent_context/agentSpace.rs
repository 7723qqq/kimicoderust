use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use async_trait::async_trait;
use futures_util::FutureExt;
use parking_lot::Mutex;
use serde_json::Value;

use crate::agent::agent_context::AgentContext;
use crate::app::event::event2::Event2;
use crate::state::agent_model::{
    AgentModel, AgentModelBridge, AgentModelContext, AgentModelDefinition, AgentModelInstance,
    DomainResourceRuntime,
};
use crate::wire::record::WireAsyncError;
use crate::state::state::ReplayableStateKey;

pub trait AgentSpaceHost: Send + Sync {
    fn is_active_model_definition(&self, definition: &AgentModelDefinition) -> bool;

    fn register_model(&self, definition: &AgentModelDefinition, model: &AgentModel);

    fn dispatch_model_event(
        &self,
        event: Event2,
    ) -> crate::rpc::types::BoxFuture<'static, Result<(), WireAsyncError>>;

    fn read_legacy_state(&self, key: &Arc<ReplayableStateKey>) -> Value;
}

struct SpaceBridge {
    host: Arc<dyn AgentSpaceHost>,
}

#[async_trait]
impl AgentModelBridge for SpaceBridge {
    async fn dispatch(&self, event: Event2) -> Result<(), WireAsyncError> {
        self.host.dispatch_model_event(event).await
    }

    fn read_legacy(&self, key: &Arc<ReplayableStateKey>) -> Value {
        self.host.read_legacy_state(key)
    }

    fn initial_state(&self) -> Value {
        Value::Null
    }
}

pub struct ModelEntry {
    pub definition: AgentModelDefinition,
    pub model: Box<dyn AgentModelInstance>,
    pub leases: AtomicU32,
    pub retired: AtomicBool,
    pub disposed: AtomicBool,
}

pub struct AgentSpaceImpl {
    agent_id: String,
    instances: Mutex<HashMap<String, Arc<ModelEntry>>>,
    host: Mutex<Option<Arc<dyn AgentSpaceHost>>>,
    context: Mutex<Option<Arc<AgentContext>>>,
    dead: AtomicBool,
}

impl AgentSpaceImpl {
    pub fn new(agent_id: impl Into<String>) -> Self {
        Self {
            agent_id: agent_id.into(),
            instances: Mutex::new(HashMap::new()),
            host: Mutex::new(None),
            context: Mutex::new(None),
            dead: AtomicBool::new(false),
        }
    }

    pub fn bind_context(&self, context: Arc<AgentContext>) {
        *self.context.lock() = Some(context);
    }

    pub fn attach_host(&self, host: Arc<dyn AgentSpaceHost>) {
        *self.host.lock() = Some(host);
    }

    pub fn detach_host(&self, host: &Arc<dyn AgentSpaceHost>) {
        let mut slot = self.host.lock();
        if slot
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, host))
        {
            *slot = None;
        }
    }

    pub fn use_<M, R>(
        self: &Arc<Self>,
        definition: &AgentModelDefinition,
        run: impl FnOnce(&M) -> R,
    ) -> R
    where
        M: AgentModelInstance + 'static,
    {
        let entry = match self.ensure_model(definition) {
            Ok(entry) => entry,
            Err(error) => panic!("{error}"),
        };
        let lease = ModelLease::acquire(self, &entry);
        let model = entry
            .model
            .as_any()
            .downcast_ref::<M>()
            .unwrap_or_else(|| {
                panic!(
                    "Agent model '{}' is not {}",
                    definition.id,
                    std::any::type_name::<M>()
                )
            });
        let result = catch_unwind(AssertUnwindSafe(|| run(model)));
        drop(lease);
        match result {
            Ok(value) => value,
            Err(error) => resume_unwind(error),
        }
    }

    pub async fn use_async<M, R, F>(
        self: &Arc<Self>,
        definition: &AgentModelDefinition,
        run: F,
    ) -> Result<R, String>
    where
        M: AgentModelInstance + 'static,
        F: AsyncFnOnce(&M) -> R,
    {
        let entry = self.ensure_model(definition)?;
        let lease = ModelLease::acquire(self, &entry);
        let Some(model) = entry.model.as_any().downcast_ref::<M>() else {
            return Err(format!(
                "Agent model '{}' is not {}",
                definition.id,
                std::any::type_name::<M>()
            ));
        };
        let future = match catch_unwind(AssertUnwindSafe(|| run(model))) {
            Ok(future) => future,
            Err(error) => {
                drop(lease);
                resume_unwind(error);
            }
        };
        let result = AssertUnwindSafe(future).catch_unwind().await;
        drop(lease);
        match result {
            Ok(result) => Ok(result),
            Err(error) => resume_unwind(error),
        }
    }

    pub fn ensure_model(&self, definition: &AgentModelDefinition) -> Result<Arc<ModelEntry>, String> {
        if let Some(existing) = self.instances.lock().get(&definition.id) {
            return Ok(existing.clone());
        }
        if self.dead.load(Ordering::SeqCst) {
            return Err(format!("Agent {} space is disposed", self.agent_id));
        }
        let host = self.host.lock().clone();
        let Some(host) = host else {
            return Err(format!("Agent {} space has no model host", self.agent_id));
        };
        if !host.is_active_model_definition(definition) {
            return Err(format!(
                "Model definition '{}' is unavailable",
                definition.id
            ));
        }
        let context = self.context.lock().clone();
        let Some(context) = context else {
            return Err(format!(
                "Agent {} space is not bound to a context",
                self.agent_id
            ));
        };
        let bridge = Arc::new(SpaceBridge { host: host.clone() });
        let mut model = (definition.model)(AgentModelContext {
            agent: context,
            bridge,
        });
        model.base_mut().seal();
        let entry = Arc::new(ModelEntry {
            definition: (*definition).clone(),
            model,
            leases: AtomicU32::new(0),
            retired: AtomicBool::new(false),
            disposed: AtomicBool::new(false),
        });
        self.instances
            .lock()
            .insert(definition.id.clone(), entry.clone());
        host.register_model(definition, entry.model.base());
        Ok(entry)
    }

    pub async fn retire_model(&self, definition: &AgentModelDefinition) {
        let entry = self.instances.lock().remove(&definition.id);
        let Some(entry) = entry else {
            return;
        };
        entry.retired.store(true, Ordering::SeqCst);
        if entry.leases.load(Ordering::SeqCst) == 0 {
            self.dispose_entry(&entry).await;
        }
    }

    pub async fn kill(&self) {
        if self.dead.swap(true, Ordering::SeqCst) {
            return;
        }
        let entries: Vec<Arc<ModelEntry>> = self
            .instances
            .lock()
            .drain()
            .map(|(_, entry)| entry)
            .collect();
        for entry in entries {
            entry.retired.store(true, Ordering::SeqCst);
            if entry.leases.load(Ordering::SeqCst) == 0 {
                self.dispose_entry(&entry).await;
            }
        }
    }


    async fn dispose_entry(&self, entry: &Arc<ModelEntry>) {
        if entry.disposed.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = entry.model.base().dispose().await;
    }
}

/// v2 `use` releases the lease in `finally`, so a throwing callback or an
/// ignored promise still releases it. A drop guard gives the same guarantee when
/// the Rust future is cancelled or never awaited.
struct ModelLease {
    space: Arc<AgentSpaceImpl>,
    entry: Arc<ModelEntry>,
}

impl ModelLease {
    fn acquire(space: &Arc<AgentSpaceImpl>, entry: &Arc<ModelEntry>) -> Self {
        entry.leases.fetch_add(1, Ordering::SeqCst);
        Self {
            space: space.clone(),
            entry: entry.clone(),
        }
    }
}

impl Drop for ModelLease {
    fn drop(&mut self) {
        let previous = self.entry.leases.fetch_sub(1, Ordering::SeqCst);
        if previous == 1 && self.entry.retired.load(Ordering::SeqCst) {
            let space = self.space.clone();
            let entry = self.entry.clone();
            tokio::spawn(async move { space.dispose_entry(&entry).await });
        }
    }
}

pub fn agent_space_of(agent: &AgentContext) -> Result<Arc<AgentSpaceImpl>, String> {
    agent.space.clone().ok_or_else(|| {
        format!(
            "Agent {}:{} is not a lifecycle-issued context",
            agent.agent_id, agent.generation
        )
    })
}

pub fn agent_space_or_bug(agent: &AgentContext) -> Arc<AgentSpaceImpl> {
    match agent.space.clone() {
        Some(space) => space,
        None => panic!(
            "Agent {}:{} is not a lifecycle-issued context",
            agent.agent_id, agent.generation
        ),
    }
}
