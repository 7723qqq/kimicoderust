use std::sync::Arc;

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};

use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
use crate::human::llm::message::ContentPart;
use crate::tool::tool_contract::{ToolDelivery, ToolResultSpill};

pub use crate::tools::tool_dedupe::*;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolDedupeOutput {
    Text(String),
    Parts(Vec<ContentPart>),
}

impl Default for ToolDedupeOutput {
    fn default() -> Self {
        Self::Text(String::new())
    }
}

#[derive(Debug, Clone, Default)]
pub struct ToolDedupeResult {
    pub output: ToolDedupeOutput,
    pub is_error: Option<bool>,
    pub stop_turn: Option<bool>,
    pub stop_turn_reason: Option<String>,
    pub truncated: Option<bool>,
    pub note: Option<String>,
    pub delivery: Option<ToolDelivery>,
    pub spill: Option<ToolResultSpill>,
    pub spill_exempt: Option<bool>,
    pub message: Option<String>,
}

pub type ToolDedupeSuccessResult = ToolDedupeResult;
pub type ToolDedupeErrorResult = ToolDedupeResult;

pub const REPEAT_BREAKER_STOP_REASON: &str = "repeat_breaker";

pub trait IAgentToolDedupeService: Send + Sync {}

pub static I_AGENT_TOOL_DEDUPE_SERVICE: Lazy<
    ServiceIdentifier<Arc<dyn IAgentToolDedupeService>>,
> = Lazy::new(|| create_decorator("agentToolDedupeService"));
