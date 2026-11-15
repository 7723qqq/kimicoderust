use std::sync::LazyLock;
use std::sync::Arc;
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
use crate::wire::record::WireLineRange;

use super::loop_event_fold::LoopRecordedEvent;
use super::types::ContextMessage;

#[derive(Debug, Clone)]
pub struct ContextCompactionInput {
    pub summary: String,
    pub context_summary: Option<String>,
    pub in_flight_turn: Option<Vec<ContextMessage>>,
    pub compacted_count: usize,
    pub tokens_before: u64,
    pub tokens_after: Option<u64>,
    pub summary_output_tokens: Option<u64>,
    pub request_overhead_tokens: Option<u64>,
    pub kept_user_message_count: Option<usize>,
    pub kept_head_user_message_count: Option<usize>,
    pub dropped_count: Option<usize>,
    pub wire_lines: Option<WireLineRange>,
}

#[derive(Debug, Clone)]
pub struct ContextCompactionResult {
    pub summary: String,
    pub context_summary: String,
    pub compacted_count: usize,
    pub tokens_before: u64,
    pub tokens_after: u64,
    pub kept_user_message_count: usize,
    pub kept_head_user_message_count: Option<usize>,
    pub appended_injection_count: usize,
    pub dropped_count: Option<usize>,
}

pub trait IAgentContextMemoryService: Send + Sync {
    fn get(&self) -> Vec<ContextMessage>;
    fn prefix_revision(&self) -> u64;
    fn append(&self, messages: &[ContextMessage]);
    fn append_loop_event(&self, event: LoopRecordedEvent);
    fn publish_trailing_removal(&self, previous: &[ContextMessage]) -> bool;
    fn clear(&self);
    fn apply_compaction(&self, input: ContextCompactionInput) -> ContextCompactionResult;
}


pub static I_AGENT_CONTEXT_MEMORY_SERVICE: LazyLock<ServiceIdentifier<Arc<dyn IAgentContextMemoryService>>> =
    LazyLock::new(|| create_decorator("agentContextMemoryService"));
