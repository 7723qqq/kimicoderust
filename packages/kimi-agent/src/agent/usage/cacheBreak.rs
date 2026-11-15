pub use crate::session::cache_observation::{
    CacheBreakCause, CacheBreakObservation, CacheBreakSnapshot,
};

use kimi_core::types::TokenUsage;

use super::cache_observation::CacheObservation;

#[derive(Debug, Clone)]
pub struct CacheBreakObserveInput {
    pub usage: TokenUsage,
    pub cache: Option<CacheObservation>,
    pub turn_id: Option<String>,
    pub step: Option<u32>,
    pub now: Option<i64>,
}

pub trait IAgentCacheBreakService: Send + Sync {
    fn observe(&self, input: CacheBreakObserveInput) -> Option<CacheBreakObservation>;
    fn reset(&self);
}
