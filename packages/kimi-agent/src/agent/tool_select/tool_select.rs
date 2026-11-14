use std::collections::BTreeMap;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};

use crate::agent::context_memory::types::ContextMessage;
pub use kimi_core::types::ToolDescription as Tool;
use crate::tools::tool_retriever::ToolSearchHit;
use crate::wire::record::WireAsyncError;
use tokio_util::sync::CancellationToken;

pub use crate::tools::select_tools::{
    NO_LOADABLE_TOOLS_MESSAGE, SELECT_TOOLS_DESCRIPTION, SELECT_TOOLS_PROVIDE_NAMES_OR_QUERY,
    SELECT_TOOLS_TOOL_NAME, SELECT_TOOLS_UNAVAILABLE, STOP_RETRY_THRESHOLD, SelectionFailureTracker,
    ToolRegistration, evaluate_load_tools, execute_select_tools, inactive_loaded_tool_output,
    missing_refusal, missing_tool_output, not_loaded_tool_output, render_loadable_tools_announcement,
    search_output, select_tools_schema, selection_environment_fingerprint,
    selection_environment_for_tools, text_result, unavailable_refusal,
};
pub use crate::tools::tool_select_ops::{ToolSelectOrigin, ToolSelectTool};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSelectCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dynamically_loaded_tools: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_use: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShapedToolEntry {
    #[serde(flatten)]
    pub tool: ToolSelectTool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred: Option<bool>,
}

pub use crate::tools::tool_select_state::ToolSelectAnnouncementDisclosure;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSelectAnnouncement {
    pub content: String,
    pub disclosure: ToolSelectAnnouncementDisclosure,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoadToolsResult {
    pub to_load: Vec<String>,
    pub already_available: Vec<String>,
    pub already_callable: Vec<String>,
    pub stale_loaded: Vec<String>,
    pub policy_denied: Vec<String>,
    pub unknown: Vec<String>,
    pub escalated: BTreeMap<String, usize>,
    pub consecutive_failures: usize,
    pub suggestions: BTreeMap<String, Vec<String>>,
    pub loadable: Vec<String>,
}

pub trait IAgentToolSelectService: Send + Sync {
    fn feature_enabled(&self) -> bool;

    fn enabled(&self, capabilities: Option<ToolSelectCapabilities>) -> bool;

    fn prepare_for_request(
        self: &Arc<Self>,
        signal: Option<CancellationToken>,
    ) -> BoxFuture<'static, Result<(), WireAsyncError>>;

    fn default_tools(&self, disclosure: Option<bool>) -> Vec<Tool>;

    fn shape_tools(
        &self,
        entries: Vec<ToolSelectTool>,
        disclosure: Option<bool>,
    ) -> Vec<ShapedToolEntry>;

    fn shape_history(
        &self,
        messages: Vec<ContextMessage>,
        disclosure: Option<bool>,
        tools: Option<Vec<Tool>>,
    ) -> Vec<ContextMessage>;

    fn load(&self, names: &[String]) -> LoadToolsResult;

    fn search(&self, query: &str, limit: Option<usize>) -> Vec<ToolSearchHit>;

    fn drain_pending_tool_schemas(&self) -> Option<Vec<Tool>>;

    fn loadable_tools_announcement(
        &self,
        last_disclosure: Option<ToolSelectAnnouncementDisclosure>,
    ) -> Option<ToolSelectAnnouncement>;
}

