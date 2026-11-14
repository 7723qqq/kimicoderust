use std::error::Error;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::Stream;

use crate::_base::di::lifecycle::IDisposable;

use super::journal::IAgentJournal;
use super::record::{RecordDehydrator, WireAsyncError, WireRecord};

pub use super::tree::fork::{ForkLineError, ForkLineFailure};
use super::tree::tree::WireLine;

#[derive(Debug, Clone, PartialEq)]
pub struct WireRestoreChains {
    pub restorable: Vec<WireRecord>,
    pub journal: Vec<WireRecord>,
}

#[async_trait]
pub trait IWireService: IAgentJournal {
    async fn seal(&self) -> Result<(), WireAsyncError>;
    fn append_record(&self, record: WireRecord, dehydrate: Option<RecordDehydrator>);
    fn on_did_append_record(
        &self,
        listener: Arc<dyn Fn(&WireRecord) + Send + Sync>,
    ) -> Box<dyn IDisposable>;
    fn read_journal(
        &self,
    ) -> Pin<Box<dyn Stream<Item = Result<WireRecord, WireAsyncError>> + Send + '_>>;
    fn read_restorable(
        &self,
    ) -> Pin<Box<dyn Stream<Item = Result<WireRecord, WireAsyncError>> + Send + '_>>;
    async fn read_restore_chains(&self) -> Result<WireRestoreChains, WireAsyncError>;
    fn read_human_chain(&self) -> Result<Vec<WireLine>, Box<dyn Error + Send + Sync>>;
    async fn flush(&self) -> Result<(), WireAsyncError>;
    async fn drain_persisted(&self) -> Result<(), WireAsyncError>;
    fn line_count(&self) -> u64;
    fn last_context_clear_line(&self) -> Option<u64>;
    fn journal_path(&self) -> Option<String>;
    fn externally_modified(&self) -> bool;
}
