use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use parking_lot::Mutex;
use serde_json::Value;

use crate::agent::agent_context::AgentContext;
use crate::state::state::ReplayableStateKey;
use crate::app::event::event2::{Event2Class, register_event2_class};
use crate::rpc::types::BoxFuture;

pub trait DomainResourceRuntime: Send + Sync {
    fn dispose(&self) -> BoxFuture<'static, ()>;

    fn abort(&self, _reason: Option<Value>) {}
}

pub trait AgentModelBridge: Send + Sync {
    fn dispatch(&self, event: Value) -> BoxFuture<'static, Result<(), String>>;

    fn read_legacy(&self, key: &Arc<ReplayableStateKey>) -> Value;
}

pub struct AgentModelContext {
    pub agent: AgentContext,
    pub bridge: Arc<dyn AgentModelBridge>,
    pub initial: Value,
}

pub struct FoldContext {
    pub emit: Arc<dyn Fn(Value) + Send + Sync>,
}

struct ModelWindow {
    draft: Value,
    ctx: FoldContext,
    replaced: bool,
    replacement: Option<Value>,
}

pub struct AgentModel {
    committed_state: Value,
    window: Option<ModelWindow>,
    appliers: HashMap<String, Arc<dyn Fn(Value) + Send + Sync>>,
    sealed: bool,
    bridge: Arc<dyn AgentModelBridge>,
}

impl AgentModel {
    pub fn new(context: AgentModelContext) -> Self {
        Self {
            committed_state: context.initial,
            window: None,
            appliers: HashMap::new(),
            sealed: false,
            bridge: context.bridge,
        }
    }

    pub fn state(&self) -> &Value {
        match &self.window {
            Some(window) => &window.draft,
            None => &self.committed_state,
        }
    }

    pub fn set_state(&mut self, value: Value) -> Result<(), String> {
        let Some(window) = self.window.as_mut() else {
            return Err("Model state can only be replaced inside an applier".into());
        };
        window.replaced = true;
        window.replacement = Some(value);
        Ok(())
    }

    pub fn on(
        &mut self,
        cls: &Event2Class,
        applier: Arc<dyn Fn(Value) + Send + Sync>,
    ) -> Result<(), String> {
        if self.sealed {
            return Err("Model cannot register appliers after construction".into());
        }
        if self.appliers.contains_key(&cls.type_) {
            return Err(format!("Model already applies event '{}'", cls.type_));
        }
        self.appliers.insert(cls.type_.clone(), applier);
        Ok(())
    }

    pub async fn emit(&self, event: Value) -> Result<(), String> {
        match &self.window {
            Some(window) => {
                (window.ctx.emit)(event);
                Ok(())
            }
            None => self.bridge.dispatch(event).await,
        }
    }

    pub fn read_legacy(&self, key: &Arc<ReplayableStateKey>) -> Value {
        self.bridge.read_legacy(key)
    }

    pub fn on_undo(&self, _count: u64) {}

    pub fn seal(&mut self) {
        self.sealed = true;
    }

    pub fn appliers_table(&self) -> &HashMap<String, Arc<dyn Fn(Value) + Send + Sync>> {
        &self.appliers
    }

    pub fn committed(&self) -> &Value {
        &self.committed_state
    }

    pub fn commit_state(&mut self, next: Value) {
        self.committed_state = next;
    }

    pub fn enter_window(&mut self, draft: Value, ctx: FoldContext) {
        self.window = Some(ModelWindow {
            draft,
            ctx,
            replaced: false,
            replacement: None,
        });
    }

    pub fn exit_window(&mut self) -> (bool, Option<Value>) {
        let window = self.window.take();
        match window {
            Some(window) => (window.replaced, window.replacement),
            None => (false, None),
        }
    }
}

impl DomainResourceRuntime for AgentModel {
    fn dispose(&self) -> BoxFuture<'static, ()> {
        Box::pin(async {})
    }
}

#[derive(Clone)]
pub struct AgentModelStateSpec {
    pub initial: Arc<dyn Fn() -> Value + Send + Sync>,
}

#[derive(Clone)]
pub struct AgentModelDefinition {
    pub id: String,
    pub model: Arc<dyn Fn(AgentModelContext) -> AgentModel + Send + Sync>,
    pub state: AgentModelStateSpec,
    pub events: Vec<Arc<Event2Class>>,
    pub undoable: bool,
}

static AGENT_MODEL_DEFINITIONS: LazyLock<Mutex<HashMap<String, AgentModelDefinition>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn define_agent_model(input: AgentModelDefinition) -> Result<AgentModelDefinition, String> {
    let mut definitions = AGENT_MODEL_DEFINITIONS.lock();
    if definitions.contains_key(&input.id) {
        return Err(format!("Agent model '{}' is already defined", input.id));
    }
    for cls in &input.events {
        if !cls.durable {
            return Err(format!(
                "Agent model '{}' cannot apply non-durable event '{}'",
                input.id, cls.type_
            ));
        }
        register_event2_class(cls);
    }
    definitions.insert(input.id.clone(), input.clone());
    Ok(input)
}

pub fn agent_model_definitions() -> Vec<AgentModelDefinition> {
    AGENT_MODEL_DEFINITIONS.lock().values().cloned().collect()
}

pub fn validate_applier_coverage(
    definition: &AgentModelDefinition,
    model: &AgentModel,
) -> Result<(), String> {
    let registered = model.appliers_table();
    for cls in &definition.events {
        if !registered.contains_key(&cls.type_) {
            return Err(format!(
                "Agent model '{}' does not apply declared event '{}'",
                definition.id, cls.type_
            ));
        }
    }
    for type_ in registered.keys() {
        if !definition.events.iter().any(|cls| &cls.type_ == type_) {
            return Err(format!(
                "Agent model '{}' applies undeclared event '{}'",
                definition.id, type_
            ));
        }
    }
    Ok(())
}
