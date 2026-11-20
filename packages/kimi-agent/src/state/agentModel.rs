use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use indexmap::IndexMap;
use parking_lot::Mutex;
use serde_json::Value;

use crate::_base::di::collection::{CollectionDescriptor, CollectionOptions, collection};
use crate::state::state::ReplayableStateKey;
use crate::agent::agent_context::agent_context::AgentContext;
use crate::app::event::event2::{Event2, Event2Class, register_event2_class};
use crate::wire::record::WireAsyncError;

use super::errors::{StateError, STATE_DUPLICATE_FOLD};
use super::state::FoldContext;

#[async_trait]
pub trait DomainResourceRuntime: Send + Sync {
    async fn dispose(&self) -> Result<(), WireAsyncError> {
        Ok(())
    }
    fn abort(&self, _reason: Option<WireAsyncError>) {}
}

#[async_trait]
pub trait AgentModelBridge: Send + Sync {
    async fn dispatch(&self, event: Event2) -> Result<(), WireAsyncError>;
    fn read_legacy(&self, key: &Arc<ReplayableStateKey>) -> Value;
    fn initial_state(&self) -> Value;
}

pub struct AgentModelContext {
    pub agent: Arc<AgentContext>,
    pub bridge: Arc<dyn AgentModelBridge>,
}

#[derive(Clone)]
struct ModelWindow {
    draft: Value,
    replaced: bool,
    replacement: Option<Value>,
}

#[derive(Clone)]
pub struct AgentModel {
    committed_state: Value,
    window: Option<ModelWindow>,
    appliers: HashMap<usize, Arc<dyn Fn(&Event2) + Send + Sync>>,
    sealed: bool,
    pub on_undo_handler: Option<Arc<dyn Fn(u64) + Send + Sync>>,
    pub agent: Arc<AgentContext>,
    bridge: Arc<dyn AgentModelBridge>,
}

impl AgentModel {
    pub fn new(context: AgentModelContext) -> Self {
        let committed_state = context.bridge.initial_state();
        Self {
            committed_state,
            window: None,
            appliers: HashMap::new(),
            sealed: false,
            on_undo_handler: None,
            agent: context.agent,
            bridge: context.bridge,
        }
    }

    pub fn state(&self) -> &Value {
        self.window
            .as_ref()
            .map(|window| &window.draft)
            .unwrap_or(&self.committed_state)
    }

    pub fn set_state(&mut self, value: Value) -> Result<(), StateError> {
        let Some(window) = &mut self.window else {
            return Err(StateError::new(
                "internal",
                "Model can only replace state inside an applier",
            ));
        };
        window.replaced = true;
        window.replacement = Some(value);
        Ok(())
    }

    pub fn on(
        &mut self,
        cls: &Arc<Event2Class>,
        applier: Arc<dyn Fn(&Event2) + Send + Sync>,
    ) -> Result<(), StateError> {
        if self.sealed {
            return Err(StateError::new(
                "internal",
                "Model cannot register appliers after construction",
            ));
        }
        let address = Arc::as_ptr(cls) as usize;
        if self.appliers.contains_key(&address) {
            return Err(StateError::new(
                "internal",
                format!("Model already applies event '{}'", cls.type_),
            ));
        }
        self.appliers.insert(address, applier);
        Ok(())
    }

    pub async fn emit(&self, event: Event2) -> Result<(), WireAsyncError> {
        self.bridge.dispatch(event).await
    }

    pub fn read_legacy(&self, key: &Arc<ReplayableStateKey>) -> Value {
        self.bridge.read_legacy(key)
    }

    pub fn on_undo(&mut self, count: u64) {
        if let Some(handler) = &self.on_undo_handler {
            handler(count);
        }
    }

    pub fn seal(&mut self) {
        self.sealed = true;
    }

    pub fn appliers_table(&self) -> &HashMap<usize, Arc<dyn Fn(&Event2) + Send + Sync>> {
        &self.appliers
    }

    pub fn state_value(&self) -> Value {
        self.committed_state.clone()
    }

    pub fn commit_state(&mut self, next: Value) {
        self.committed_state = next;
    }

    pub fn enter_window(&mut self, draft: Value) {
        self.window = Some(ModelWindow {
            draft,
            replaced: false,
            replacement: None,
        });
    }

    pub fn exit_window(&mut self) -> (bool, Option<Value>) {
        let window = self.window.take();
        (
            window.as_ref().is_some_and(|window| window.replaced),
            window.and_then(|window| window.replacement),
        )
    }
}

#[async_trait]
impl DomainResourceRuntime for AgentModel {}

/// The value a model definition constructs (v2 `AgentModelInstanceOf<D>`).
///
/// v2 types the instance by the definition, so `AgentSpace.use` hands callers the
/// subclass the definition declares. The Rust port erases it at the definition
/// boundary; this trait keeps the erased value reachable as its base model while
/// leaving the concrete type available for a typed `use`.
pub trait AgentModelInstance: Send + Sync {
    fn base(&self) -> &AgentModel;
    fn base_mut(&mut self) -> &mut AgentModel;
    fn as_any(&self) -> &dyn std::any::Any;
}

impl AgentModelInstance for AgentModel {
    fn base(&self) -> &AgentModel {
        self
    }

    fn base_mut(&mut self) -> &mut AgentModel {
        self
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[derive(Clone)]
pub struct AgentModelStateSpec {
    pub initial: Arc<dyn Fn() -> Value + Send + Sync>,
    pub schema: Value,
}

#[derive(Clone)]
pub struct AgentModelDefinition {
    pub id: String,
    pub model: Arc<dyn Fn(AgentModelContext) -> Box<dyn AgentModelInstance> + Send + Sync>,
    pub state: AgentModelStateSpec,
    pub events: Vec<Arc<Event2Class>>,
    pub undoable: bool,
}

pub struct AgentModelDefinitionInput {
    pub id: String,
    pub model: Arc<dyn Fn(AgentModelContext) -> Box<dyn AgentModelInstance> + Send + Sync>,
    pub state: AgentModelStateSpec,
    pub events: Vec<Arc<Event2Class>>,
    pub undoable: Option<bool>,
}

static AGENT_MODEL_DEFINITIONS: LazyLock<Mutex<IndexMap<String, Arc<AgentModelDefinition>>>> =
    LazyLock::new(|| Mutex::new(IndexMap::new()));

pub fn define_agent_model(input: AgentModelDefinitionInput) -> Result<Arc<AgentModelDefinition>, StateError> {
    if AGENT_MODEL_DEFINITIONS.lock().contains_key(&input.id) {
        return Err(StateError::new(
            "internal",
            format!("Agent model '{}' is already defined", input.id),
        ));
    }
    for cls in &input.events {
        if !cls.durable {
            return Err(StateError::new(
                "internal",
                format!("Agent model '{}' cannot apply non-durable event '{}'", input.id, cls.type_),
            ));
        }
        register_event2_class(cls).map_err(|error| StateError::new("internal", error))?;
    }
    let definition = Arc::new(AgentModelDefinition {
        id: input.id,
        model: input.model,
        state: input.state,
        events: input.events,
        undoable: input.undoable.unwrap_or(false),
    });
    AGENT_MODEL_DEFINITIONS
        .lock()
        .insert(definition.id.clone(), definition.clone());
    Ok(definition)
}

pub fn agent_model_definitions() -> Vec<Arc<AgentModelDefinition>> {
    AGENT_MODEL_DEFINITIONS.lock().values().cloned().collect()
}

pub fn agent_model_contribution() -> CollectionDescriptor<Arc<AgentModelDefinition>> {
    collection::<Arc<AgentModelDefinition>>(
        "agent-model",
        CollectionOptions::validating(|value: &Arc<AgentModelDefinition>, existing: &[Arc<AgentModelDefinition>]| {
            if existing.iter().any(|definition| definition.id == value.id) {
                return Err(format!(
                    "Agent model '{}' already has an active provider",
                    value.id
                ));
            }
            Ok(())
        }),
    )
}

#[derive(Clone)]
pub struct SessionModelDefinition {
    pub id: String,
    pub state: AgentModelStateSpec,
    pub events: Vec<Arc<Event2Class>>,
    pub undoable: bool,
}

pub fn session_model_contribution() -> CollectionDescriptor<SessionModelDefinition> {
    collection::<SessionModelDefinition>(
        "session-model",
        CollectionOptions::validating(|value: &SessionModelDefinition, existing: &[SessionModelDefinition]| {
            if existing.iter().any(|definition| definition.id == value.id) {
                return Err(format!(
                    "Session model '{}' already has an active provider",
                    value.id
                ));
            }
            Ok(())
        }),
    )
}

pub fn duplicate_fold_error(name: &str, event_type: &str) -> StateError {
    StateError::with_options(
        STATE_DUPLICATE_FOLD,
        format!("State '{name}' already folds event '{event_type}'"),
        kimi_core::primitives::errors::Error2Options {
            details: Some(serde_json::Map::from_iter([
                ("state".to_string(), name.into()),
                ("type".to_string(), event_type.into()),
            ])),
            ..Default::default()
        },
    )
}
