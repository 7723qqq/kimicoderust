use std::sync::Arc;

use futures_util::future::{BoxFuture, Shared};
use serde::{Deserialize, Serialize};

use crate::_base::errors::errors::SharedError;
use crate::_base::event::Event;
use crate::_base::utils::abort::AbortController;
use crate::hooks::OrderedHookSlot;
use crate::wire::record::WireAsyncError;

pub use crate::compaction::*;

use super::types::{CompactionResult, CompactionSource};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FullCompactionInput {
    pub source: CompactionSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction: Option<String>,
}

#[derive(Clone)]
pub struct FullCompactionTask {
    pub abort_controller: Arc<AbortController>,
    pub promise: Shared<BoxFuture<'static, Result<CompactionResult, SharedError>>>,
    pub trigger: CompactionSource,
    pub token_count: u64,
    pub trace_id: Option<String>,
}

#[derive(Clone)]
pub struct FullCompactionHooks {
    pub on_will_compact: OrderedHookSlot<FullCompactionTask>,
}

impl Default for FullCompactionHooks {
    fn default() -> Self {
        Self {
            on_will_compact: OrderedHookSlot::new(),
        }
    }
}

pub trait IAgentFullCompactionService: Send + Sync {
    fn compacting(&self) -> Option<FullCompactionTask>;

    fn begin(&self, input: FullCompactionInput) -> Result<bool, WireAsyncError>;

    fn cancel(&self);

    fn hooks(&self) -> FullCompactionHooks;

    fn on_did_finish_compaction(&self) -> Event<FullCompactionTask>;
}
