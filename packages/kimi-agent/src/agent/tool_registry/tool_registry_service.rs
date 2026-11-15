use std::collections::BTreeMap;
use std::sync::Arc;

use once_cell::sync::Lazy;
use parking_lot::Mutex;

use crate::_base::di::descriptors::{Constructor, DynamicValue, SyncDescriptor};
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
use crate::_base::di::scope::{ScopeActivation, register_scoped_service};
use crate::agent::tool_executor::tool_executor::{
    ExecutableTool, IAgentToolRegistryService, ToolInfo, ToolInfoOrigin,
};
use crate::app::scopes::LifecycleScope;
use crate::tools::registry::{
    RegistrationStatus, ToolDefinition, ToolDisclosure, ToolSource, version_signature,
};

pub static I_AGENT_TOOL_REGISTRY_SERVICE: Lazy<
    ServiceIdentifier<Arc<dyn IAgentToolRegistryService>>,
> = Lazy::new(|| create_decorator("agentToolRegistryService"));

#[derive(Debug, Clone, Default)]
pub struct ToolRegistrationOptions {
    pub source: Option<ToolSource>,
    pub disclosure: Option<ToolDisclosure>,
}

pub struct ToolRegistration {
    pub status: RegistrationStatus,
    dispose: Box<dyn Fn() + Send + Sync>,
}

impl ToolRegistration {
    pub fn dispose(&self) {
        (self.dispose)();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolReference {
    pub name: String,
    pub source: ToolSource,
}

#[derive(Clone)]
struct ToolEntry {
    id: u64,
    tool: Arc<dyn ExecutableTool>,
    source: ToolSource,
    disclosure: Option<ToolDisclosure>,
    info: ToolInfo,
}

#[derive(Default)]
struct RegistryState {
    tools: BTreeMap<String, ToolEntry>,
    pending: BTreeMap<String, ToolEntry>,
    revision: u64,
    next_id: u64,
}

impl RegistryState {
    fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn unregister(&mut self, name: &str) {
        self.pending.remove(name);
        if self.tools.remove(name).is_some() {
            self.revision += 1;
        }
    }
}

fn create_entry(
    id: u64,
    tool: Arc<dyn ExecutableTool>,
    source: ToolSource,
    disclosure: Option<ToolDisclosure>,
) -> ToolEntry {
    let info = ToolInfo {
        name: tool.name().to_string(),
        description: tool.description().to_string(),
        parameters: tool.parameters(),
        source,
        disclosure,
        origin: ToolInfoOrigin::Runtime,
    };
    ToolEntry {
        id,
        tool,
        source,
        disclosure,
        info,
    }
}

fn live_info(entry: &ToolEntry) -> ToolInfo {
    ToolInfo {
        name: entry.tool.name().to_string(),
        description: entry.tool.description().to_string(),
        parameters: entry.tool.parameters(),
        source: entry.source,
        disclosure: entry.disclosure,
        origin: ToolInfoOrigin::Runtime,
    }
}

fn info_version_signature(info: &ToolInfo) -> String {
    version_signature(&ToolDefinition {
        name: info.name.clone(),
        description: info.description.clone(),
        input_schema: serde_json::Value::Object(info.parameters.clone()),
        source: info.source,
        disclosure: info.disclosure.unwrap_or_default(),
    })
}

pub struct AgentToolRegistryService {
    state: Arc<Mutex<RegistryState>>,
}

impl Default for AgentToolRegistryService {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentToolRegistryService {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(RegistryState::default())),
        }
    }

    pub fn register(
        &self,
        tool: Arc<dyn ExecutableTool>,
        options: ToolRegistrationOptions,
    ) -> ToolRegistration {
        let source = options.source.unwrap_or(ToolSource::Builtin);
        let name = tool.name().to_string();
        let mut state = self.state.lock();
        let id = state.next_id();
        let candidate = create_entry(id, tool, source, options.disclosure);
        let status = match state.tools.get(&name).cloned() {
            None => {
                state.pending.remove(&name);
                state.tools.insert(name.clone(), candidate);
                RegistrationStatus::Active
            }
            Some(current)
                if info_version_signature(&current.info) == info_version_signature(&candidate.info) =>
            {
                let replacement = ToolEntry {
                    info: current.info,
                    ..candidate
                };
                state.pending.remove(&name);
                state.tools.insert(name.clone(), replacement);
                RegistrationStatus::Active
            }
            Some(_) => {
                state.pending.insert(name.clone(), candidate);
                RegistrationStatus::Pending
            }
        };
        state.revision += 1;
        drop(state);
        let state = self.state.clone();
        ToolRegistration {
            status,
            dispose: Box::new(move || {
                let mut state = state.lock();
                match status {
                    RegistrationStatus::Pending => {
                        if state.pending.get(&name).map(|entry| entry.id) == Some(id) {
                            state.pending.remove(&name);
                            state.revision += 1;
                        }
                    }
                    RegistrationStatus::Active => {
                        if state.tools.get(&name).map(|entry| entry.id) == Some(id) {
                            state.unregister(&name);
                        }
                    }
                }
            }),
        }
    }

    pub fn list(&self) -> Vec<ToolInfo> {
        let state = self.state.lock();
        state
            .tools
            .values()
            .map(|entry| {
                if state.pending.contains_key(entry.tool.name()) {
                    entry.info.clone()
                } else {
                    live_info(entry)
                }
            })
            .collect()
    }

    pub fn list_live(&self) -> Vec<ToolInfo> {
        let state = self.state.lock();
        let mut live: BTreeMap<String, ToolInfo> = state
            .tools
            .iter()
            .map(|(name, entry)| (name.clone(), live_info(entry)))
            .collect();
        for (name, entry) in &state.pending {
            live.insert(name.clone(), entry.info.clone());
        }
        live.into_values().collect()
    }

    pub fn list_references(&self) -> Vec<ToolReference> {
        let state = self.state.lock();
        state
            .tools
            .iter()
            .map(|(name, entry)| ToolReference {
                name: name.clone(),
                source: entry.source,
            })
            .collect()
    }

    pub fn resolve(&self, name: &str) -> Option<Arc<dyn ExecutableTool>> {
        self.state
            .lock()
            .tools
            .get(name)
            .map(|entry| entry.tool.clone())
    }

    pub fn revision(&self) -> u64 {
        self.state.lock().revision
    }
}

impl IAgentToolRegistryService for AgentToolRegistryService {
    fn resolve(&self, name: &str) -> Option<Arc<dyn ExecutableTool>> {
        AgentToolRegistryService::resolve(self, name)
    }

    fn list(&self) -> Vec<ToolInfo> {
        AgentToolRegistryService::list(self)
    }

    fn list_live(&self) -> Option<Vec<ToolInfo>> {
        Some(AgentToolRegistryService::list_live(self))
    }

    fn revision(&self) -> u64 {
        AgentToolRegistryService::revision(self)
    }
}

static TOOL_REGISTRY_SERVICE_REGISTRATION: Lazy<Result<(), String>> = Lazy::new(|| {
    let ctor: Constructor<DynamicValue> = Arc::new(|_| {
        let service: Arc<dyn IAgentToolRegistryService> =
            Arc::new(AgentToolRegistryService::new());
        Arc::new(service) as DynamicValue
    });
    register_scoped_service(
        LifecycleScope::Agent.as_str().to_string(),
        create_decorator("agentToolRegistryService"),
        SyncDescriptor::new(ctor, Vec::new()),
        ScopeActivation::OnScopeCreated,
        "toolRegistry",
    )
    .map_err(|error| error.to_string())
});

pub fn ensure_agent_tool_registry_service_registered() -> Result<(), String> {
    Lazy::force(&TOOL_REGISTRY_SERVICE_REGISTRATION).clone()
}
