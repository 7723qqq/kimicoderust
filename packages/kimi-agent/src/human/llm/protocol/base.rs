use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::human::llm::capability::ModelCapability;
use crate::human::llm::model::LlmModel;
use crate::human::llm::protocol::connection::ProviderConnection;
use crate::human::llm::requester::requester::{LlmErrorClassifier, LlmRequester};
use crate::human::log::log::LlmLogger;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ProtocolName {
    #[serde(rename = "openai")]
    OpenAI,
    #[serde(rename = "openai_responses")]
    OpenAIResponses,
    #[serde(rename = "anthropic")]
    Anthropic,
    #[serde(rename = "google-genai")]
    GoogleGenAI,
    #[serde(rename = "antigravity")]
    Antigravity,
}

#[derive(Debug, Clone)]
pub struct TraitContext {
    pub model: LlmModel,
}

pub struct ProtocolRequesterOptions<TTrait> {
    pub connection: Option<ProviderConnection>,
    pub trait_: Option<TTrait>,
    pub classify_error: Option<LlmErrorClassifier>,
    pub log: Option<LlmLogger>,
}

impl<TTrait> Default for ProtocolRequesterOptions<TTrait> {
    fn default() -> Self {
        Self {
            connection: None,
            trait_: None,
            classify_error: None,
            log: None,
        }
    }
}

pub trait ProtocolBase<TTrait>: Send + Sync {
    fn capability(&self, _model_name: &str) -> Option<ModelCapability> {
        None
    }

    fn create_requester(
        &self,
        options: ProtocolRequesterOptions<TTrait>,
    ) -> Result<Arc<dyn LlmRequester>, crate::wire::record::WireAsyncError>;
}
