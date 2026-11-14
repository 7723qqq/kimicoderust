use async_trait::async_trait;
use futures_util::Stream;
use serde::{Deserialize, Serialize};

use super::record::{RecordDehydrator, WireAsyncError, WireRecord};
use super::tree::tree::BranchBase;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentJournalRef {
    pub tree: String,
    pub branch: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwitchBranchInput {
    pub turns: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_turn_id: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwitchedBranch {
    pub branch: String,
    pub base: BranchBase,
    pub edge_line: u64,
    pub fork_line: u64,
}

#[async_trait]
pub trait IAgentJournal: Send + Sync {
    fn journal_ref(&self) -> AgentJournalRef;
    fn append(&self, record: WireRecord, dehydrate: Option<RecordDehydrator>);
    fn read(
        &self,
    ) -> std::pin::Pin<
        Box<dyn Stream<Item = Result<WireRecord, WireAsyncError>> + Send + '_>,
    >;
    fn read_raw(
        &self,
    ) -> std::pin::Pin<
        Box<dyn Stream<Item = Result<WireRecord, WireAsyncError>> + Send + '_>,
    >;
    async fn switch_branch(
        &self,
        input: SwitchBranchInput,
    ) -> Result<SwitchedBranch, WireAsyncError>;
    fn branches(&self) -> Vec<String>;
    fn next_seq(&self) -> u64;
    async fn settled(&self) -> Result<(), WireAsyncError>;
}
