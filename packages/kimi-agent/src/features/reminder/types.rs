use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct ContextInjectionContext<D = Value> {
    pub injected_positions: Vec<usize>,
    pub last_injected_at: Option<usize>,
    pub last_injection: Option<crate::agent::context_memory::types::ContextMessage>,
    pub last_disclosure: Option<D>,
    pub is_new_turn: bool,
    pub estimated_tokens: Option<usize>,
    pub signal: Option<CancellationToken>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextInjectionMessage {
    pub role: crate::human::llm::message::Role,
    pub content: Vec<kimi_core::types::ContentPart>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<kimi_core::types::ToolDescription>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ContextInjectionContent {
    Text(String),
    Parts(Vec<kimi_core::types::ContentPart>),
    Message(ContextInjectionMessage),
}

#[derive(Debug, Clone)]
pub struct ContextInjectionResult<D = Value> {
    pub content: ContextInjectionContent,
    pub disclosure: Option<D>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReminderNotification {
    pub variant: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_prompt_id: Option<String>,
}
