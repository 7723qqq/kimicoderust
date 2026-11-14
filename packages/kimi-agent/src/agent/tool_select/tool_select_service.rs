use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use futures_util::future::BoxFuture;
use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::_base::di::lifecycle::DisposableRef;
use crate::agent::context_memory::context_memory::IAgentContextMemoryService;
use crate::agent::context_memory::types::ContextMessage;
use crate::agent::profile::profile::AgentProfileService;
use crate::agent::state::agent_state::{ReplayableStateMeta, StateKey};
use crate::agent::state::agent_state_service::{AgentStateService, StateRegistration};
use crate::agent::tool_executor::tool_executor::{
    IAgentToolExecutorService, IAgentToolRegistryService, ToolInfo,
};
use crate::app::flag::flag::IFlagService;
use crate::app::telemetry::telemetry::ITelemetryService;
use crate::features::reminder::reminder_service::ReminderService;
use crate::features::reminder::types::ReminderNotification;
use crate::session::mcp::session_mcp_handle::ISessionMcpHandle;
use crate::state::event_dispatcher::IEventDispatcher;
use crate::tool::tool_contract::{ToolDisclosure, ToolSource};
use crate::tools::tool_retriever::{ToolSearchEntry, ToolSearchHit, rank_tools_by_query};
use crate::wire::record::WireAsyncError;

use super::dynamic_tools::*;
use super::flag::TOOL_SELECT_FLAG_ID;
use super::tool_select::{
    IAgentToolSelectService, LoadToolsResult, ShapedToolEntry, Tool, ToolSelectAnnouncement,
    ToolSelectCapabilities,
};
use crate::tools::select_tools::{
    STOP_RETRY_THRESHOLD, inactive_loaded_tool_output, not_loaded_tool_output,
};

pub use crate::tools::tool_select_ops::*;

const RETRIEVAL_MIN_TOOLS: usize = 20;
const RETRIEVAL_TOP_K: usize = 10;

pub trait ToolSelectPolicy: Send + Sync {
    fn is_tool_denied(&self, name: &str, source: ToolSource) -> bool;
    fn is_tool_active(&self, name: &str, source: ToolSource) -> bool;
    fn is_tool_active_for_disclosure(&self, name: &str, source: ToolSource) -> bool;
    fn policy_fingerprint(&self) -> String;
}

pub fn tool_select_state_key() -> StateKey {
    StateKey::new("toolSelect", Arc::new(|| {
        serde_json::to_value(ToolSelectModelState::default())
            .expect("tool select state is serializable")
    }))
    .snapshot_excluded(true)
    .replayable(ReplayableStateMeta::new(true, false))
}

pub fn tool_select_pending_loaded_key() -> StateKey {
    StateKey::new("toolSelect.pendingLoaded", Arc::new(|| json!([])))
}

#[derive(Default)]
struct SelectionFailures {
    attempts: HashMap<String, usize>,
    consecutive: usize,
    environment: Option<String>,
}

pub struct AgentToolSelectService {
    tool_registry: Arc<dyn IAgentToolRegistryService>,
    profile: Arc<dyn AgentProfileService>,
    tool_policy: Arc<dyn ToolSelectPolicy>,
    context: Arc<dyn IAgentContextMemoryService>,
    flags: Arc<dyn IFlagService>,
    states: Arc<AgentStateService>,
    telemetry: Arc<dyn ITelemetryService>,
    dispatcher: Arc<dyn IEventDispatcher>,
    agent_id: String,
    mcp_handle: Arc<dyn ISessionMcpHandle>,
    reminder: Arc<dyn ReminderService>,
    reconcile_lock: Arc<tokio::sync::Mutex<()>>,
    notified_updates: Arc<Mutex<HashSet<String>>>,
    failures: Arc<Mutex<SelectionFailures>>,
    registrations: Mutex<Vec<DisposableRef>>,
    _state_registrations: Vec<StateRegistration>,
}

impl AgentToolSelectService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tool_registry: Arc<dyn IAgentToolRegistryService>,
        profile: Arc<dyn AgentProfileService>,
        tool_policy: Arc<dyn ToolSelectPolicy>,
        context: Arc<dyn IAgentContextMemoryService>,
        tool_executor: Arc<dyn IAgentToolExecutorService>,
        flags: Arc<dyn IFlagService>,
        states: Arc<AgentStateService>,
        telemetry: Arc<dyn ITelemetryService>,
        dispatcher: Arc<dyn IEventDispatcher>,
        agent_id: String,
        mcp_handle: Arc<dyn ISessionMcpHandle>,
        reminder: Arc<dyn ReminderService>,
    ) -> Result<Arc<Self>, crate::agent::state::agent_state::StateError> {
        let state_registrations = vec![
            states.contribute_state(tool_select_state_key())?,
            states.contribute_state(tool_select_pending_loaded_key())?,
        ];
        let this = Arc::new(Self {
            tool_registry,
            profile,
            tool_policy,
            context,
            flags,
            states,
            telemetry,
            dispatcher,
            agent_id,
            mcp_handle,
            reminder,
            reconcile_lock: Arc::new(tokio::sync::Mutex::new(())),
            notified_updates: Arc::new(Mutex::new(HashSet::new())),
            failures: Arc::new(Mutex::new(SelectionFailures::default())),
            registrations: Mutex::new(Vec::new()),
            _state_registrations: state_registrations,
        });
        let unavailable = Arc::downgrade(&this);
        let missing = Arc::downgrade(&this);
        this.registrations.lock().extend([
            tool_executor.register_unavailable_tool_describer(Arc::new(move |name| {
                unavailable
                    .upgrade()
                    .and_then(|this| this.describe_unavailable_tool(name))
            })),
            tool_executor.register_missing_tool_describer(Arc::new(move |name| {
                missing.upgrade().and_then(|this| this.describe_missing_tool(name))
            })),
        ]);
        Ok(this)
    }

    fn pending_loaded(&self) -> BTreeSet<String> {
        self.states
            .get(&tool_select_pending_loaded_key())
            .ok()
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default()
    }

    fn set_pending_loaded(&self, pending: BTreeSet<String>) {
        if let Ok(value) = serde_json::to_value(pending) {
            let _ = self.states.set(&tool_select_pending_loaded_key(), value);
        }
    }

    pub fn clear_pending_loaded(&self) {
        self.set_pending_loaded(BTreeSet::new());
    }

    pub fn drop_pending_loaded_not_landed(&self) {
        let pending = self.pending_loaded();
        if pending.is_empty() {
            return;
        }
        let landed = collect_loaded_dynamic_tools(&self.context.get(), &self.current_tools());
        self.set_pending_loaded(
            pending
                .into_iter()
                .filter(|name| landed.contains_key(name))
                .collect(),
        );
    }

    fn model_state(&self) -> ToolSelectModelState {
        self.states
            .get(&tool_select_state_key())
            .ok()
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default()
    }

    fn set_model_state(&self, state: ToolSelectModelState) {
        if let Ok(value) = serde_json::to_value(state) {
            let _ = self.states.set(&tool_select_state_key(), value);
        }
    }

    fn current_tools(&self) -> Vec<ToolSelectTool> {
        let state = self.model_state();
        if state.initial.is_some() {
            state.current
        } else {
            self.active_tools(ToolSelectOrigin::Initial)
        }
    }

    fn initial_tools(&self) -> Vec<ToolSelectTool> {
        self.model_state()
            .initial
            .unwrap_or_else(|| self.active_tools(ToolSelectOrigin::Initial))
    }

    fn active_tools(&self, origin: ToolSelectOrigin) -> Vec<ToolSelectTool> {
        normalize_tools(self.tool_registry.list(), origin)
    }

    fn live_tools(&self, origin: ToolSelectOrigin) -> Vec<ToolSelectTool> {
        normalize_tools(
            self.tool_registry
                .list_live()
                .unwrap_or_else(|| self.tool_registry.list()),
            origin,
        )
    }

    async fn reconcile(&self, signal: Option<CancellationToken>) -> Result<(), WireAsyncError> {
        throw_if_cancelled(&signal)?;
        let state = self.model_state();
        let live = self.live_tools(ToolSelectOrigin::Initial);
        if state.initial.is_none() {
            let active = self.active_tools(ToolSelectOrigin::Initial);
            self.dispatcher
                .dispatch(
                    ToolSelectInitialized {
                        agent_id: self.agent_id.clone(),
                        tools: active.clone(),
                    }
                    .into(),
                )
                .await?;
            let mut next = self.model_state();
            next.initialize(active);
            self.set_model_state(next);
            self.notify_updated_tools(live, signal)?;
            return Ok(());
        }
        let current = state.current;
        let current_names = current
            .iter()
            .map(|tool| tool.name.clone())
            .collect::<HashSet<_>>();
        let active = self.active_tools(ToolSelectOrigin::Initial);
        let active_names = active
            .iter()
            .map(|tool| tool.name.clone())
            .collect::<HashSet<_>>();
        let removed = current
            .iter()
            .filter(|tool| !active_names.contains(&tool.name))
            .cloned()
            .collect::<Vec<_>>();
        let added = active
            .iter()
            .filter(|tool| !current_names.contains(&tool.name))
            .map(|tool| ToolSelectTool {
                origin: ToolSelectOrigin::Runtime,
                ..tool.clone()
            })
            .collect::<Vec<_>>();
        if !added.is_empty() || !removed.is_empty() {
            throw_if_cancelled(&signal)?;
            self.dispatcher
                .dispatch(
                    ToolSelectToolsChanged {
                        agent_id: self.agent_id.clone(),
                        added: added.clone(),
                        removed: removed.clone(),
                    }
                    .into(),
                )
                .await?;
            let mut next = self.model_state();
            next.change(added, removed);
            self.set_model_state(next);
        }
        self.notify_updated_tools(live, signal)
    }

    fn notify_updated_tools(
        &self,
        live: Vec<ToolSelectTool>,
        signal: Option<CancellationToken>,
    ) -> Result<(), WireAsyncError> {
        let live = live
            .into_iter()
            .map(|tool| (tool.name.clone(), tool))
            .collect::<HashMap<_, _>>();
        for active in self.model_state().current {
            let Some(next) = live.get(&active.name) else {
                continue;
            };
            let signature = tool_select_version_signature(next);
            if signature == tool_select_version_signature(&active) {
                continue;
            }
            if self.notified_updates.lock().contains(&signature) {
                continue;
            }
            throw_if_cancelled(&signal)?;
            self.notified_updates.lock().insert(signature);
            self.reminder.notify(
                &format!(
                    "Tool \"{}\" has an update, but the update is not applied in this session. Continue using the currently registered version.",
                    active.name
                ),
                &ReminderNotification {
                    variant: "tool_update_pending".to_string(),
                    owner_prompt_id: None,
                },
            );
        }
        Ok(())
    }

    fn capable_tools(&self, include_runtime: bool) -> Vec<ToolSelectTool> {
        let initial = self.initial_tools();
        let initial_names = initial
            .iter()
            .map(|tool| tool.name.clone())
            .collect::<HashSet<_>>();
        let current = if include_runtime {
            self.current_tools()
        } else {
            self.live_tools(ToolSelectOrigin::Initial)
        };
        let current_by_name = current
            .iter()
            .map(|tool| (tool.name.clone(), tool.clone()))
            .collect::<HashMap<_, _>>();
        let mut tools = initial
            .iter()
            .map(|tool| {
                current_by_name
                    .get(&tool.name)
                    .cloned()
                    .unwrap_or_else(|| tool.clone())
            })
            .collect::<Vec<_>>();
        tools.extend(
            current
                .into_iter()
                .filter(|tool| {
                    include_runtime
                        && tool.origin == ToolSelectOrigin::Runtime
                        && !initial_names.contains(&tool.name)
                }),
        );
        tools.sort_by(|left, right| left.name.cmp(&right.name));
        tools
    }

    fn shape_tools_for_disclosure(
        &self,
        entries: Vec<ToolSelectTool>,
        disclosure: bool,
    ) -> Vec<ShapedToolEntry> {
        let active = self.active_entries(entries, disclosure);
        if !disclosure {
            return active
                .into_iter()
                .map(|tool| ShapedToolEntry {
                    tool,
                    deferred: None,
                })
                .collect();
        }
        let loaded = collect_loaded_dynamic_tools(&self.context.get(), &self.current_tools());
        let pending = self.pending_loaded();
        let mut shaped = Vec::new();
        for entry in active {
            if entry.name == SELECT_TOOLS_TOOL_NAME || !self.is_dynamically_loadable(&entry) {
                shaped.push(ShapedToolEntry {
                    tool: entry,
                    deferred: None,
                });
                continue;
            }
            if !loaded.contains_key(&entry.name) && !pending.contains(&entry.name) {
                continue;
            }
            shaped.push(ShapedToolEntry {
                tool: entry,
                deferred: Some(true),
            });
        }
        shaped
    }

    fn active_entries(
        &self,
        entries: Vec<ToolSelectTool>,
        disclosure: bool,
    ) -> Vec<ToolSelectTool> {
        entries
            .into_iter()
            .filter(|entry| {
                let source = tool_source(entry.source);
                let active = self.tool_policy.is_tool_active(&entry.name, source)
                    || (disclosure
                        && entry.name == SELECT_TOOLS_TOOL_NAME
                        && self
                            .tool_policy
                            .is_tool_active_for_disclosure(&entry.name, source));
                active && (disclosure || entry.name != SELECT_TOOLS_TOOL_NAME)
            })
            .collect()
    }

    fn reset_failures_if_tools_changed(&self, registry_infos: &[ToolSelectTool]) {
        let policy = self.tool_policy.policy_fingerprint();
        let mut sorted_infos = registry_infos.iter().collect::<Vec<_>>();
        sorted_infos.sort_by(|left, right| left.name.cmp(&right.name));
        let tools = sorted_infos
            .into_iter()
            .map(|tool| {
                json!([
                    tool.name,
                    tool.description,
                    tool.parameters,
                    tool.source,
                    tool.disclosure,
                    tool.origin,
                ])
            })
            .collect::<Vec<_>>();
        let environment = json!([policy, tools]).to_string();
        let mut failures = self.failures.lock();
        if failures.environment.as_deref() == Some(environment.as_str()) {
            return;
        }
        failures.environment = Some(environment);
        failures.consecutive = 0;
        failures.attempts.clear();
    }

    fn loaded_tool_names(&self) -> BTreeSet<String> {
        let mut names = collect_loaded_dynamic_tools(&self.context.get(), &self.current_tools())
            .into_keys()
            .collect::<BTreeSet<_>>();
        names.extend(self.pending_loaded());
        names
    }

    fn historically_loaded_tool_names(&self) -> BTreeSet<String> {
        let mut names = BTreeSet::new();
        for message in self.context.get() {
            if !is_dynamic_tool_schema_message(&message) {
                continue;
            }
            for tool in message.tools {
                names.insert(tool.name);
            }
        }
        names.extend(self.pending_loaded());
        names
    }

    fn active_loaded_tool_names(&self) -> BTreeSet<String> {
        self.loaded_tool_names()
            .into_iter()
            .filter(|name| self.is_loaded_tool_active(name))
            .collect()
    }

    fn loadable_entries(&self) -> Vec<ToolSelectTool> {
        self.current_tools()
            .into_iter()
            .filter(|tool| {
                self.is_dynamically_loadable(tool)
                    && self
                        .tool_policy
                        .is_tool_active(&tool.name, tool_source(tool.source))
            })
            .collect()
    }

    fn loadable_tool_names(&self) -> BTreeSet<String> {
        self.loadable_entries()
            .into_iter()
            .map(|tool| tool.name)
            .collect()
    }

    fn is_dynamically_loadable(&self, info: &ToolSelectTool) -> bool {
        if info.disclosure == Some(ToolDisclosure::Deferred) {
            return true;
        }
        info.origin == ToolSelectOrigin::Runtime
            && !self.initial_tools().iter().any(|tool| {
                tool.name == info.name && tool.disclosure != Some(ToolDisclosure::Deferred)
            })
    }

    fn is_static_callable(&self, name: &str, registry_infos: &[ToolSelectTool]) -> bool {
        let Some(info) = registry_infos.iter().find(|info| info.name == name) else {
            return false;
        };
        !self.is_dynamically_loadable(info) && self.is_tool_active_for_selection(info)
    }

    fn is_tool_active_for_selection(&self, info: &ToolSelectTool) -> bool {
        if info.name == SELECT_TOOLS_TOOL_NAME {
            self.tool_policy
                .is_tool_active_for_disclosure(&info.name, tool_source(info.source))
        } else {
            self.tool_policy
                .is_tool_active(&info.name, tool_source(info.source))
        }
    }

    fn is_loaded_tool_active(&self, name: &str) -> bool {
        let current = self.current_tools();
        let Some(info) = current.iter().find(|info| info.name == name) else {
            return is_mcp_tool_name(name)
                && self.tool_policy.is_tool_active(name, ToolSource::Mcp);
        };
        self.tool_policy
            .is_tool_active(name, tool_source(info.source))
            && (!self.is_dynamically_loadable(info) || self.loaded_tool_names().contains(name))
    }

    fn is_inactive_loaded_tool(&self, name: &str) -> bool {
        self.historically_loaded_tool_names().contains(name) && !self.is_loaded_tool_active(name)
    }

    fn should_intercept(&self, name: &str) -> bool {
        if !self.enabled(None) {
            return false;
        }
        let Some(info) = self.current_tools().into_iter().find(|info| info.name == name) else {
            return false;
        };
        if !self.is_dynamically_loadable(&info) || !self.loadable_tool_names().contains(name) {
            return false;
        }
        !self.active_loaded_tool_names().contains(name)
    }

    fn describe_unavailable_tool(&self, name: &str) -> Option<String> {
        if self.is_inactive_loaded_tool(name) {
            return Some(inactive_loaded_tool_output(name));
        }
        if !self.should_intercept(name) {
            return None;
        }
        Some(not_loaded_tool_output(name))
    }

    fn describe_missing_tool(&self, name: &str) -> Option<String> {
        if self.tool_registry.resolve(name).is_some()
            || !self.historically_loaded_tool_names().contains(name)
        {
            return None;
        }
        if is_mcp_tool_name(name) {
            return Some(format!(
                "Tool \"{name}\" was loaded but its MCP server is currently disconnected. It may become available again when the server reconnects; do not retry immediately."
            ));
        }
        Some(format!(
            "Tool \"{name}\" was loaded but is no longer registered. Do not retry it unless it becomes available again."
        ))
    }

    fn retrieval_announcement(
        &self,
        entries: Vec<ToolSelectTool>,
        revision: usize,
    ) -> Option<ToolSelectAnnouncement> {
        let history = self.context.get();
        let loaded = self.active_loaded_tool_names();
        let candidates = entries
            .iter()
            .filter(|entry| !loaded.contains(&entry.name))
            .cloned()
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return None;
        }
        let query = latest_user_query(&history).unwrap_or_default();
        let search_entries = candidates.iter().map(search_entry).collect::<Vec<_>>();
        let ranked = rank_tools_by_query(&search_entries, &query, RETRIEVAL_TOP_K);
        let selected = if ranked.is_empty() {
            search_entries
                .iter()
                .take(RETRIEVAL_TOP_K)
                .map(|entry| ToolSearchHit {
                    name: entry.name.clone(),
                    description: entry.description.clone(),
                    score: 0.0,
                })
                .collect::<Vec<_>>()
        } else {
            ranked
        };
        let content = render_retrieved_tools_announcement(&selected, candidates.len());
        if last_retrieved_announcement_text(&history).as_deref() == Some(content.as_str()) {
            return None;
        }
        let matched = !query.trim().is_empty() && selected.iter().any(|hit| hit.score > 0.0);
        self.telemetry.track2(
            "tool_retrieval",
            Some(Map::from_iter([
                ("mode".to_string(), json!("announce")),
                ("catalog_size".to_string(), json!(entries.len())),
                ("candidate_count".to_string(), json!(candidates.len())),
                ("selected_count".to_string(), json!(selected.len())),
                ("query_chars".to_string(), json!(query.encode_utf16().count())),
                ("matched".to_string(), json!(matched)),
            ])),
        );
        Some(ToolSelectAnnouncement {
            content,
            disclosure: ToolSelectAnnouncementDisclosure::Snapshot {
                revision,
                tools: entries,
            },
        })
    }

    fn legacy_announcement(&self, revision: usize) -> Option<ToolSelectAnnouncement> {
        let state = self.model_state();
        let history = self.context.get();
        let announced = fold_announced_tool_names(&history);
        let loadable = self.loadable_entries();
        let loadable_names = loadable
            .iter()
            .map(|tool| tool.name.clone())
            .collect::<HashSet<_>>();
        let added = loadable
            .iter()
            .filter(|tool| !announced.contains(&tool.name))
            .cloned()
            .collect::<Vec<_>>();
        let known = state
            .current
            .iter()
            .chain(state.initial.iter().flatten())
            .chain(
                state
                    .changes
                    .iter()
                    .flat_map(|change| change.added.iter().chain(change.removed.iter())),
            )
            .cloned()
            .collect::<Vec<_>>();
        let mut removed_names = announced
            .into_iter()
            .filter(|name| !loadable_names.contains(name))
            .collect::<Vec<_>>();
        removed_names.sort();
        let removed = removed_names
            .iter()
            .filter_map(|name| known.iter().find(|tool| tool.name == *name).cloned())
            .collect::<Vec<_>>();
        if !added.is_empty() || !removed.is_empty() {
            return Some(render_delta_announcement(
                added,
                removed,
                !loadable.is_empty(),
                revision,
            ));
        }
        if !loadable.is_empty() {
            return None;
        }
        if history.iter().rev().find(|message| is_loadable_tools_announcement(message)).is_some_and(
            |message| message.content.contains(NO_LOADABLE_TOOLS_MESSAGE),
        ) {
            return None;
        }
        Some(render_snapshot_announcement(Vec::new(), revision))
    }

    fn shape_active_history(
        &self,
        messages: Vec<ContextMessage>,
        tools: Vec<Tool>,
    ) -> Vec<ContextMessage> {
        let inline_names = tools
            .iter()
            .filter(|tool| tool.deferred != Some(true))
            .map(|tool| tool.name.clone())
            .collect::<HashSet<_>>();
        messages
            .into_iter()
            .filter_map(|message| self.shape_active_message(message, &inline_names))
            .collect()
    }

    fn shape_active_message(
        &self,
        mut message: ContextMessage,
        inline_names: &HashSet<String>,
    ) -> Option<ContextMessage> {
        if message.role != "system" || message.tools.is_empty() {
            return Some(message);
        }
        let current = self.current_tools();
        let loaded = collect_loaded_dynamic_tools(&self.context.get(), &current);
        let mut kept = Vec::new();
        for tool in &message.tools {
            let info = current.iter().find(|info| info.name == tool.name);
            let active = match info {
                Some(info) => self
                    .tool_policy
                    .is_tool_active(&info.name, tool_source(info.source)),
                None => {
                    is_mcp_tool_name(&tool.name)
                        && self.tool_policy.is_tool_active(&tool.name, ToolSource::Mcp)
                }
            };
            let loaded_schema = loaded.get(&tool.name);
            let keep = !inline_names.contains(&tool.name)
                && if is_mcp_tool_name(&tool.name) {
                    self.tool_policy.is_tool_active(&tool.name, ToolSource::Mcp)
                        && loaded_schema.is_none_or(|loaded| {
                            tool_description_schema_signature(loaded) == tool_description_schema_signature(tool)
                        })
                } else {
                    active
                        && loaded_schema.is_some_and(|loaded| {
                            tool_description_schema_signature(loaded) == tool_description_schema_signature(tool)
                        })
                };
            if keep {
                kept.push(tool.clone());
            }
        }
        if kept.len() == message.tools.len() {
            return Some(message);
        }
        if !kept.is_empty() {
            message.tools = kept;
            return Some(message);
        }
        message.tools.clear();
        if message.content.is_empty() && message.tool_calls.is_empty() {
            None
        } else {
            Some(message)
        }
    }

    fn schema_of(&self, name: &str) -> Option<Tool> {
        self.current_tools()
            .into_iter()
            .find(|tool| tool.name == name)
            .map(|tool| Tool {
                name: tool.name,
                description: tool.description,
                parameters: tool.parameters,
                deferred: None,
            })
    }
}

impl IAgentToolSelectService for AgentToolSelectService {
    fn feature_enabled(&self) -> bool {
        self.flags.enabled(TOOL_SELECT_FLAG_ID)
    }

    fn enabled(&self, capabilities: Option<ToolSelectCapabilities>) -> bool {
        let capabilities = capabilities.unwrap_or_else(|| {
            let capabilities = self.profile.get_model_capabilities();
            ToolSelectCapabilities {
                dynamically_loaded_tools: capabilities.dynamically_loaded_tools,
                tool_use: Some(capabilities.tool_use),
            }
        });
        capabilities.dynamically_loaded_tools == Some(true)
            && capabilities.tool_use == Some(true)
            && self.feature_enabled()
    }

    fn prepare_for_request(
        self: &Arc<Self>,
        signal: Option<CancellationToken>,
    ) -> BoxFuture<'static, Result<(), WireAsyncError>> {
        let this = self.clone();
        Box::pin(async move {
            let ready = this.mcp_handle.ready();
            throw_if_cancelled(&signal)?;
            if let Some(signal) = signal.clone() {
                tokio::select! {
                    biased;
                    result = ready => result?,
                    _ = signal.cancelled() => {
                        return Err(crate::_base::utils::abort::abort_error().into());
                    }
                }
            } else {
                ready.await?;
            }
            let work = {
                let this = this.clone();
                let work_signal = signal.clone();
                tokio::spawn(async move {
                    let _serial = this.reconcile_lock.lock().await;
                    this.reconcile(work_signal).await
                })
            };
            throw_if_cancelled(&signal)?;
            if let Some(signal) = signal {
                tokio::select! {
                    biased;
                    result = work => result??,
                    _ = signal.cancelled() => {
                        return Err(crate::_base::utils::abort::abort_error().into());
                    }
                }
            } else {
                work.await??;
            }
            Ok(())
        })
    }

    fn default_tools(&self, disclosure: Option<bool>) -> Vec<Tool> {
        let disclosure = disclosure.unwrap_or_else(|| self.enabled(None));
        self.shape_tools_for_disclosure(self.capable_tools(disclosure), disclosure)
            .into_iter()
            .map(|entry| Tool {
                name: entry.tool.name,
                description: entry.tool.description,
                parameters: entry.tool.parameters,
                deferred: entry.deferred,
            })
            .collect()
    }

    fn shape_tools(
        &self,
        entries: Vec<ToolSelectTool>,
        disclosure: Option<bool>,
    ) -> Vec<ShapedToolEntry> {
        self.shape_tools_for_disclosure(entries, disclosure.unwrap_or_else(|| self.enabled(None)))
    }

    fn shape_history(
        &self,
        messages: Vec<ContextMessage>,
        disclosure: Option<bool>,
        tools: Option<Vec<Tool>>,
    ) -> Vec<ContextMessage> {
        let disclosure = disclosure.unwrap_or_else(|| self.enabled(None));
        let tools = tools.unwrap_or_else(|| self.default_tools(Some(disclosure)));
        if !disclosure {
            return strip_dynamic_tool_context(messages);
        }
        strip_stale_retrieved_tools_announcements(self.shape_active_history(messages, tools))
    }

    fn load(&self, names: &[String]) -> LoadToolsResult {
        let current = self.current_tools();
        let loadable = self.loadable_tool_names();
        let loaded = self.loaded_tool_names();
        let historically_loaded = self.historically_loaded_tool_names();
        self.reset_failures_if_tools_changed(&current);
        let mut result = LoadToolsResult::default();
        let mut seen = HashSet::new();
        for name in names {
            if !seen.insert(name) {
                continue;
            }
            let info = current.iter().find(|entry| &entry.name == name);
            let source = info.map_or_else(
                || {
                    if is_mcp_tool_name(name) {
                        ToolSource::Mcp
                    } else {
                        ToolSource::Builtin
                    }
                },
                |info| tool_source(info.source),
            );
            let denied = self.tool_policy.is_tool_denied(name, source);
            if denied || info.is_some_and(|info| !self.is_tool_active_for_selection(info)) {
                result.policy_denied.push(name.clone());
            } else if loadable.contains(name) {
                if loaded.contains(name) {
                    result.already_available.push(name.clone());
                } else {
                    result.to_load.push(name.clone());
                }
            } else if self.is_static_callable(name, &current) {
                result.already_callable.push(name.clone());
            } else if historically_loaded.contains(name) {
                result.stale_loaded.push(name.clone());
            } else {
                result.unknown.push(name.clone());
            }
        }

        let succeeded = !result.to_load.is_empty()
            || !result.already_available.is_empty()
            || !result.already_callable.is_empty();
        let mut failures = self.failures.lock();
        failures.consecutive = if succeeded {
            0
        } else {
            failures.consecutive + 1
        };
        for name in result
            .to_load
            .iter()
            .chain(result.already_available.iter())
            .chain(result.already_callable.iter())
        {
            failures.attempts.remove(&name.to_lowercase());
        }
        let mut attempts_by_key = HashMap::new();
        for name in result
            .stale_loaded
            .iter()
            .chain(result.policy_denied.iter())
            .chain(result.unknown.iter())
        {
            let key = name.to_lowercase();
            let attempts = match attempts_by_key.get(&key).copied() {
                Some(attempts) => attempts,
                None => {
                    let attempts = failures.attempts.get(&key).copied().unwrap_or(0) + 1;
                    failures.attempts.insert(key.clone(), attempts);
                    attempts_by_key.insert(key, attempts);
                    attempts
                }
            };
            if attempts >= STOP_RETRY_THRESHOLD {
                result.escalated.insert(name.clone(), attempts);
            }
        }
        result.consecutive_failures = failures.consecutive;
        drop(failures);

        let suggestion_pool = loadable.iter().cloned().collect::<Vec<_>>();
        for name in &result.unknown {
            let candidates = suggest_tool_names(name, &suggestion_pool);
            if !candidates.is_empty() {
                result.suggestions.insert(name.clone(), candidates);
            }
        }
        if !result.to_load.is_empty() {
            let mut pending = self.pending_loaded();
            pending.extend(result.to_load.iter().cloned());
            self.set_pending_loaded(pending);
        }
        result.loadable = loadable
            .into_iter()
            .filter(|name| !loaded.contains(name))
            .collect();
        result
    }

    fn drain_pending_tool_schemas(&self) -> Option<Vec<Tool>> {
        if !self.enabled(None) {
            return None;
        }
        let pending = self.pending_loaded();
        if pending.is_empty() {
            return None;
        }
        let mut tools = Vec::new();
        for name in pending.iter() {
            if let Some(tool) = self.schema_of(name) {
                tools.push(tool);
            }
        }
        if tools.is_empty() {
            return None;
        }
        let remaining = pending
            .into_iter()
            .filter(|name| !tools.iter().any(|tool| &tool.name == name))
            .collect();
        self.set_pending_loaded(remaining);
        Some(tools)
    }

    fn search(&self, query: &str, limit: Option<usize>) -> Vec<ToolSearchHit> {
        if !self.enabled(None) {
            return Vec::new();
        }
        let entries = self.loadable_entries();
        let search_entries = entries.iter().map(search_entry).collect::<Vec<_>>();
        let hits = rank_tools_by_query(
            &search_entries,
            query,
            limit.unwrap_or(RETRIEVAL_TOP_K),
        );
        self.telemetry.track2(
            "tool_retrieval",
            Some(Map::from_iter([
                ("mode".to_string(), json!("search")),
                ("catalog_size".to_string(), json!(entries.len())),
                ("candidate_count".to_string(), json!(entries.len())),
                ("selected_count".to_string(), json!(hits.len())),
                ("query_chars".to_string(), json!(query.encode_utf16().count())),
                ("matched".to_string(), json!(!hits.is_empty())),
            ])),
        );
        hits
    }

    fn loadable_tools_announcement(
        &self,
        last_disclosure: Option<ToolSelectAnnouncementDisclosure>,
    ) -> Option<ToolSelectAnnouncement> {
        if !self.feature_enabled() {
            return None;
        }
        let state = self.model_state();
        state.initial.as_ref()?;
        self.reset_failures_if_tools_changed(&state.current);
        let revision = state.changes.len();
        let loadable_entries = self.loadable_entries();
        if loadable_entries.len() >= RETRIEVAL_MIN_TOOLS {
            return self.retrieval_announcement(loadable_entries, revision);
        }
        let last_revision = last_disclosure.as_ref().map(|disclosure| disclosure.revision());
        if last_revision.is_none_or(|last| last > revision) {
            if let Some(legacy) = self.legacy_announcement(revision) {
                return Some(legacy);
            }
            let tools = loadable_entries;
            return Some(render_snapshot_announcement(tools, revision));
        }
        for index in last_revision.unwrap()..revision {
            let change = state.changes.get(index)?;
            let added = change
                .added
                .iter()
                .filter(|tool| self.is_dynamically_loadable(tool))
                .cloned()
                .collect::<Vec<_>>();
            let removed = change
                .removed
                .iter()
                .filter(|tool| {
                    self.is_dynamically_loadable(tool)
                        || !change.added.iter().any(|next| {
                            next.name == tool.name && !self.is_dynamically_loadable(next)
                        })
                })
                .cloned()
                .collect::<Vec<_>>();
            if added.is_empty() && removed.is_empty() {
                continue;
            }
            let has_loadable = state
                .current
                .iter()
                .any(|tool| self.is_dynamically_loadable(tool));
            return Some(render_delta_announcement(
                added,
                removed,
                has_loadable,
                index + 1,
            ));
        }
        self.legacy_announcement(revision)
    }
}

fn normalize_tools(infos: Vec<ToolInfo>, origin: ToolSelectOrigin) -> Vec<ToolSelectTool> {
    let mut tools = infos
        .into_iter()
        .map(|info| ToolSelectTool {
            name: info.name,
            description: info.description,
            parameters: if info.parameters.is_empty() {
                json!({"type": "object", "properties": {}})
            } else {
                Value::Object(info.parameters)
            },
            source: match info.source {
                ToolSource::Builtin => ToolSelectSource::Builtin,
                ToolSource::Mcp => ToolSelectSource::Mcp,
                ToolSource::Host | ToolSource::Plugin => ToolSelectSource::User,
            },
            disclosure: info.disclosure,
            origin,
        })
        .collect::<Vec<_>>();
    tools.sort_by(|left, right| left.name.cmp(&right.name));
    tools
}

fn tool_source(source: ToolSelectSource) -> ToolSource {
    match source {
        ToolSelectSource::Builtin => ToolSource::Builtin,
        ToolSelectSource::Mcp => ToolSource::Mcp,
        ToolSelectSource::User => ToolSource::Host,
    }
}

fn search_entry(tool: &ToolSelectTool) -> ToolSearchEntry {
    ToolSearchEntry {
        name: tool.name.clone(),
        description: tool.description.clone(),
        source: match tool.source {
            ToolSelectSource::Builtin => "builtin",
            ToolSelectSource::User => "user",
            ToolSelectSource::Mcp => "mcp",
        }
        .to_string(),
        parameter_names: tool
            .parameters
            .get("properties")
            .and_then(Value::as_object)
            .map(|properties| properties.keys().cloned().take(32).collect())
            .unwrap_or_default(),
    }
}

fn render_snapshot_announcement(
    tools: Vec<ToolSelectTool>,
    revision: usize,
) -> ToolSelectAnnouncement {
    let mut names = tools.iter().map(|tool| tool.name.clone()).collect::<Vec<_>>();
    names.sort();
    let content = render_loadable_tools_announcement(&names, &[], !names.is_empty());
    ToolSelectAnnouncement {
        content,
        disclosure: ToolSelectAnnouncementDisclosure::Snapshot { revision, tools },
    }
}

fn render_delta_announcement(
    added: Vec<ToolSelectTool>,
    removed: Vec<ToolSelectTool>,
    has_loadable: bool,
    revision: usize,
) -> ToolSelectAnnouncement {
    let added_names = added.iter().map(|tool| tool.name.clone()).collect::<Vec<_>>();
    let removed_names = removed.iter().map(|tool| tool.name.clone()).collect::<Vec<_>>();
    ToolSelectAnnouncement {
        content: render_loadable_tools_announcement(&added_names, &removed_names, has_loadable),
        disclosure: ToolSelectAnnouncementDisclosure::Delta {
            revision,
            added,
            removed,
        },
    }
}

fn suggest_tool_names(name: &str, pool: &[String]) -> Vec<String> {
    let lower = name.to_lowercase();
    let mut case_fix = pool
        .iter()
        .filter(|candidate| candidate.to_lowercase() == lower)
        .cloned()
        .collect::<Vec<_>>();
    if !case_fix.is_empty() {
        case_fix.sort();
        case_fix.truncate(3);
        return case_fix;
    }
    let stripped = lower.strip_prefix("mcp__").unwrap_or(&lower);
    let mut matches = BTreeSet::new();
    if stripped.encode_utf16().count() >= 3 {
        for candidate in pool {
            if candidate.to_lowercase().contains(stripped) {
                matches.insert(candidate.clone());
            }
        }
    }
    if matches.is_empty() {
        for candidate in pool {
            let last = candidate
                .split("__")
                .next_back()
                .unwrap_or(candidate)
                .to_lowercase();
            if last.encode_utf16().count() >= 3 && lower.contains(&last) {
                matches.insert(candidate.clone());
            }
        }
    }
    matches.into_iter().take(3).collect()
}

fn is_mcp_tool_name(name: &str) -> bool {
    crate::native::tool_naming::is_mcp_tool_name(name)
}

fn throw_if_cancelled(signal: &Option<CancellationToken>) -> Result<(), WireAsyncError> {
    if signal.as_ref().is_some_and(CancellationToken::is_cancelled) {
        return Err(crate::_base::utils::abort::abort_error().into());
    }
    Ok(())
}

impl Drop for AgentToolSelectService {
    fn drop(&mut self) {
        for registration in self.registrations.lock().drain(..) {
            registration.dispose();
        }
    }
}

