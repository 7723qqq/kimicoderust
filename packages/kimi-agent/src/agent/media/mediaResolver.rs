use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::_base::utils::abort::AbortSignal;
use crate::llm_adapter::contract::message::Message;
use crate::llm_adapter::model::model_requester::ModelRequester;
use crate::wire::record::WireAsyncError;

#[async_trait]
pub trait IAgentMediaResolverService: Send + Sync {
    async fn resolve(
        &self,
        messages: &[Message],
        requester: Arc<dyn ModelRequester>,
        signal: Option<AbortSignal>,
    ) -> Result<Vec<Message>, WireAsyncError>;

    async fn display_paths(
        &self,
        messages: &[Message],
    ) -> Result<HashMap<String, String>, WireAsyncError>;
}
