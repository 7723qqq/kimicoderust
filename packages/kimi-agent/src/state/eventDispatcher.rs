use std::sync::LazyLock;
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
use std::sync::Arc;

use async_trait::async_trait;

use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::app::event::event2::{Event2, Event2Class};
use crate::hooks::Hooks;
use crate::wire::record::WireAsyncError;

use super::state::{FoldContext, StateFold};

pub struct EventDispatcherHooks {
    pub on_did_restore: crate::hooks::OrderedHookSlot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestorePhase {
    New,
    Restoring,
    Ready,
    Failed,
}

impl RestorePhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Restoring => "restoring",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }
}

pub trait DurableAgentRuntimeParticipant: Send + Sync {
    fn id(&self) -> &str;
    fn events(&self) -> Vec<Arc<Event2Class>>;
    fn undoable(&self) -> bool;
    fn transition(&self) -> StateFold;
    fn get_state(&self) -> serde_json::Value;
    fn commit(&self, state: serde_json::Value, silent: bool);
}

#[async_trait]
pub trait DurableRuntimeParticipantHost: Send + Sync {
    fn attach(&self, participant: Arc<dyn DurableAgentRuntimeParticipant>) -> DisposableRef;
}

#[async_trait]
pub trait IEventDispatcher: DurableRuntimeParticipantHost {
    fn hooks(&self) -> &EventDispatcherHooks;
    fn restore_phase(&self) -> RestorePhase;
    async fn dispatch(&self, event: Event2) -> Result<(), WireAsyncError>;
    async fn attach_late(
        &self,
        participant: Arc<dyn DurableAgentRuntimeParticipant>,
    ) -> Result<DisposableRef, WireAsyncError>;
    async fn restore(&self) -> Result<(), WireAsyncError>;
    async fn flush(&self) -> Result<(), WireAsyncError>;
}

#[allow(dead_code)]
fn _context_marker(_: &dyn FoldContext) {}


pub static I_EVENT_DISPATCHER: LazyLock<ServiceIdentifier<Arc<dyn IEventDispatcher>>> =
    LazyLock::new(|| create_decorator("eventDispatcher"));
