use std::sync::LazyLock;
use std::sync::Arc;
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::wire::record::WireAsyncError;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TitleTurnExcerpt {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TitleDigestTurn {
    pub user: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TitleDigestExcerpt {
    pub turns: Vec<TitleDigestTurn>,
}

#[async_trait]
pub trait IAgentTitlePromptSource: Send + Sync {
    async fn first_user_prompts(&self, limit: usize) -> Result<Vec<String>, WireAsyncError>;
    async fn first_turn_excerpt(&self) -> Result<TitleTurnExcerpt, WireAsyncError>;
    async fn digest_excerpt(&self) -> Result<TitleDigestExcerpt, WireAsyncError>;
}


pub static I_AGENT_TITLE_PROMPT_SOURCE: LazyLock<ServiceIdentifier<Arc<dyn IAgentTitlePromptSource>>> =
    LazyLock::new(|| create_decorator("agentTitlePromptSource"));
