use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use futures_util::FutureExt;
use parking_lot::Mutex;
use serde_json::Value;

use crate::agent::agent_context::AgentContext;
use crate::agent::state::agent_model::{
    AgentModel, AgentModelBridge, AgentModelContext, AgentModelDefinition, DomainResourceRuntime,
    validate_applier_coverage,
};
use crate::state::state::ReplayableStateKey;

pub trait AgentSpaceHost: Send + Sync {
    fn is_active_model_definition(&self, definition: &AgentModelDefinition) -> bool;

    fn register_model(&self, definition: &AgentModelDefinition, model: &AgentModel);

    fn dispatch_model_event(
        &self,
        event: Value,
    ) -> crate::rpc::types::BoxFuture<'static, Result<(), String>>;

    fn read_legacy_state(&self, key: &Arc<ReplayableStateKey>) -> Value;
}

struct SpaceBridge {
    host: Arc<dyn AgentSpaceHost>,
}

impl AgentModelBridge for SpaceBridge {
    fn dispatch(&self, event: Value) -> crate::rpc::types::BoxFuture<'static, Result<(), String>> {
        self.host.dispatch_model_event(event)
    }

    fn read_legacy(&self, key: &Arc<ReplayableStateKey>) -> Value {
        self.host.read_legacy_state(key)
    }
}

pub struct ModelEntry {
    pub definition: AgentModelDefinition,
    pub model: AgentModel,
    pub leases: AtomicU32,
    pub retired: AtomicBool,
    pub disposed: AtomicBool,
}

pub struct AgentSpaceImpl {
    agent_id: String,
    instances: Mutex<HashMap<String, Arc<ModelEntry>>>,
    host: Mutex<Option<Arc<dyn AgentSpaceHost>>>,
    context: Mutex<Option<AgentContext>>,
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

    pub fn bind_context(&self, context: AgentContext) {
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

    pub async fn use_model<R, Fut>(
        &self,
        definition: &AgentModelDefinition,
        run: impl FnOnce(&AgentModel) -> Fut,
    ) -> Result<R, String>
    where
        Fut: std::future::Future<Output = R>,
    {
        let entry = self.ensure_model(definition)?;
        entry.leases.fetch_add(1, Ordering::SeqCst);
        let future = match catch_unwind(AssertUnwindSafe(|| run(&entry.model))) {
            Ok(future) => future,
            Err(error) => {
                self.release(&entry).await;
                resume_unwind(error);
            }
        };
        let result = AssertUnwindSafe(future).catch_unwind().await;
        self.release(&entry).await;
        match result {
            Ok(result) => Ok(result),
            Err(error) => resume_unwind(error),
        }
    }

    fn ensure_model(&self, definition: &AgentModelDefinition) -> Result<Arc<ModelEntry>, String> {
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
        let initial = (definition.state.initial)();
        let mut model = (definition.model)(AgentModelContext {
            agent: context,
            bridge,
            initial,
        });
        model.seal();
        validate_applier_coverage(definition, &model)?;
        let entry = Arc::new(ModelEntry {
            definition: definition.clone(),
            model,
            leases: AtomicU32::new(0),
            retired: AtomicBool::new(false),
            disposed: AtomicBool::new(false),
        });
        self.instances
            .lock()
            .insert(definition.id.clone(), entry.clone());
        host.register_model(definition, &entry.model);
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

    async fn release(&self, entry: &Arc<ModelEntry>) {
        let previous = entry.leases.fetch_sub(1, Ordering::SeqCst);
        if previous == 1 && entry.retired.load(Ordering::SeqCst) {
            self.dispose_entry(entry).await;
        }
    }

    async fn dispose_entry(&self, entry: &Arc<ModelEntry>) {
        if entry.disposed.swap(true, Ordering::SeqCst) {
            return;
        }
        entry.model.dispose().await;
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
