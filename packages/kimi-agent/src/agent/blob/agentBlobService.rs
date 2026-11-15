use std::sync::{Arc, LazyLock};

use async_trait::async_trait;

use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
use crate::human::llm::message::ContentPart;
use crate::wire::record::WireAsyncError;

pub const BLOBREF_PROTOCOL: &str = "blobref:";
pub const MISSING_MEDIA_PLACEHOLDER: &str = "[media missing]";

#[async_trait]
pub trait IAgentBlobService: Send + Sync {
    async fn offload_parts(&self, parts: &[ContentPart]) -> Result<Vec<ContentPart>, WireAsyncError>;
    async fn load_parts(&self, parts: &[ContentPart]) -> Result<Vec<ContentPart>, WireAsyncError>;
    fn is_blob_ref(&self, url: &str) -> bool;
}

pub static I_AGENT_BLOB_SERVICE: LazyLock<ServiceIdentifier<Arc<dyn IAgentBlobService>>> =
    LazyLock::new(|| create_decorator("agentBlobService"));
