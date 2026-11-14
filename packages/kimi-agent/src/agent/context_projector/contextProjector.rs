use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use crate::_base::di::descriptors::DynamicValue;
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
use crate::agent::context_memory::types::ContextMessage;
use crate::llm_adapter::contract::message::Message;
use crate::wire::record::WireAsyncError;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MediaStripSnapshot {
    pub(crate) keys: HashSet<String>,
}

pub const MEDIA_DEGRADE_KEEP_RECENT: usize = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaProjection {
    DegradeCount(usize),
    Strip(MediaStripSnapshot),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionStructure {
    Strict,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectionPolicy {
    pub structure: Option<ProjectionStructure>,
    pub media: Option<MediaProjection>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaEvictionPlan {
    MediaDegraded {
        degrade_count: usize,
        estimated_bytes: u64,
        evicted_media_count: usize,
    },
    MediaStripped {
        estimated_bytes: u64,
        evicted_media_count: usize,
    },
}

impl MediaEvictionPlan {
    pub fn projection(&self) -> &'static str {
        match self {
            Self::MediaDegraded { .. } => "media-degraded",
            Self::MediaStripped { .. } => "media-stripped",
        }
    }
}

pub trait IAgentContextProjectorService: Send + Sync {
    fn project(
        &self,
        messages: &[ContextMessage],
        policy: Option<&ProjectionPolicy>,
        media_paths: Option<&HashMap<String, String>>,
    ) -> Result<Vec<Message>, WireAsyncError>;

    fn capture_media_strip_snapshot(
        &self,
        messages: &[ContextMessage],
    ) -> Result<MediaStripSnapshot, WireAsyncError>;

    fn capture_media_degrade_count(
        &self,
        messages: &[ContextMessage],
    ) -> Result<usize, WireAsyncError>;

    fn plan_media_eviction(
        &self,
        messages: &[Message],
        estimated_bytes: u64,
        budget_bytes: u64,
    ) -> Option<MediaEvictionPlan>;
}

pub static I_AGENT_CONTEXT_PROJECTOR_SERVICE: LazyLock<ServiceIdentifier<DynamicValue>> =
    LazyLock::new(|| create_decorator("agentContextProjectorService"));
