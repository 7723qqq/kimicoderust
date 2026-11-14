use std::collections::HashSet;
use std::sync::Arc;

use indexmap::IndexSet;
use parking_lot::Mutex;

use crate::human::llm::capability::ModelCapability;
use crate::human::llm::message::ToolDescription;
use crate::human::tool::tool::ToolDefinition;

pub const SELECT_TOOLS_TOOL_NAME: &str = "select_tools";

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LoadToolsResult {
    pub to_load: Vec<String>,
    pub already_available: Vec<String>,
    pub unknown: Vec<String>,
}

struct State {
    pending: IndexSet<String>,
    landed: IndexSet<String>,
    announced: IndexSet<String>,
    pending_announcement: Option<AnnouncementChanges>,
}

#[derive(Clone)]
struct AnnouncementChanges {
    added: Vec<String>,
    removed: Vec<String>,
}

pub struct ToolSelectState {
    loadable: Arc<dyn Fn() -> Vec<ToolDefinition> + Send + Sync>,
    enabled_callback: Arc<dyn Fn() -> bool + Send + Sync>,
    state: Mutex<State>,
}

impl ToolSelectState {
    pub fn enabled(&self) -> bool {
        (self.enabled_callback)()
    }

    pub fn is_loadable(&self, name: &str) -> bool {
        self.loadable_names().contains(name)
    }

    pub fn is_loaded(&self, name: &str) -> bool {
        let state = self.state.lock();
        state.pending.contains(name) || state.landed.contains(name)
    }

    pub fn load(&self, names: &[String]) -> LoadToolsResult {
        let loadable = self.loadable_names();
        let mut result = LoadToolsResult::default();
        let unique = names.iter().cloned().collect::<IndexSet<_>>();
        let mut state = self.state.lock();
        for name in unique {
            if state.pending.contains(&name) || state.landed.contains(&name) {
                result.already_available.push(name);
            } else if loadable.contains(&name) {
                result.to_load.push(name);
            } else {
                result.unknown.push(name);
            }
        }
        for name in &result.to_load {
            state.pending.insert(name.clone());
        }
        result
    }

    pub fn pending_schemas(&self) -> Vec<ToolDescription> {
        let mut names = self
            .state
            .lock()
            .pending
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        names.sort();
        names
            .into_iter()
            .filter_map(|name| self.schema_of(&name))
            .collect()
    }

    pub fn announcement(&self) -> Option<String> {
        if !self.enabled() {
            return None;
        }
        let mut names = self
            .loadable
            .as_ref()()
            .into_iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>();
        names.sort();
        let names_set = names.iter().cloned().collect::<HashSet<_>>();
        let mut state = self.state.lock();
        let added = names
            .iter()
            .filter(|name| !state.announced.contains(*name))
            .cloned()
            .collect::<Vec<_>>();
        let mut removed = state
            .announced
            .iter()
            .filter(|name| !names_set.contains(*name))
            .cloned()
            .collect::<Vec<_>>();
        removed.sort();
        if added.is_empty() && removed.is_empty() {
            return None;
        }
        state.pending_announcement = Some(AnnouncementChanges {
            added: added.clone(),
            removed: removed.clone(),
        });
        Some(render_loadable_tools_announcement(&added, &removed))
    }

    pub fn mark_schemas_landed(&self) {
        let mut state = self.state.lock();
        let pending = std::mem::take(&mut state.pending);
        state.landed.extend(pending);
    }

    pub fn mark_announced(&self) {
        let mut state = self.state.lock();
        let Some(changes) = state.pending_announcement.take() else {
            return;
        };
        for name in changes.added {
            state.announced.insert(name);
        }
        for name in changes.removed {
            state.announced.shift_remove(&name);
        }
    }

    pub fn reset(&self) {
        *self.state.lock() = State {
            pending: IndexSet::new(),
            landed: IndexSet::new(),
            announced: IndexSet::new(),
            pending_announcement: None,
        };
    }

    fn loadable_names(&self) -> HashSet<String> {
        (self.loadable)()
            .into_iter()
            .map(|tool| tool.name)
            .collect()
    }

    fn schema_of(&self, name: &str) -> Option<ToolDescription> {
        (self.loadable)()
            .into_iter()
            .find(|tool| tool.name == name)
            .map(|tool| ToolDescription {
                name: tool.name,
                description: tool.description,
                parameters: tool.parameters,
                deferred: None,
            })
    }
}

pub struct CreateToolSelectStateOptions {
    pub loadable: Arc<dyn Fn() -> Vec<ToolDefinition> + Send + Sync>,
    pub enabled: Arc<dyn Fn() -> bool + Send + Sync>,
}

pub fn is_tool_select_enabled(capability: &ModelCapability) -> bool {
    capability.dynamically_loaded_tools == Some(true) && capability.tool_use
}

pub fn render_loadable_tools_announcement(added: &[String], removed: &[String]) -> String {
    let mut sections = Vec::new();
    if !added.is_empty() {
        sections.push(format!("<tools_added>\n{}\n</tools_added>", added.join("\n")));
    }
    if !removed.is_empty() {
        sections.push(format!(
            "<tools_removed>\n{}\n</tools_removed>",
            removed.join("\n")
        ));
    }
    sections.push(
        "Use the select_tools tool with exact names to load full tool definitions before calling them. Names listed as removed are no longer loadable — do not select them. Fold all announcements in this conversation in order to get the current list."
            .to_string(),
    );
    sections.join("\n\n")
}

pub fn create_tool_select_state(options: CreateToolSelectStateOptions) -> ToolSelectState {
    ToolSelectState {
        loadable: options.loadable,
        enabled_callback: options.enabled,
        state: Mutex::new(State {
            pending: IndexSet::new(),
            landed: IndexSet::new(),
            announced: IndexSet::new(),
            pending_announcement: None,
        }),
    }
}
