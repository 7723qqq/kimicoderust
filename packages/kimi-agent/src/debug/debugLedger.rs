use crate::_base::di::cascade_engine::UnitState;
use crate::_base::lifecycle::ledger::LedgerEntryInfo;

#[derive(Clone)]
pub struct DebugUnit {
    pub token: String,
    pub uid: u64,
    pub state: Option<UnitState>,
    pub error: Option<String>,
    pub ever_active: Option<bool>,
    pub in_flight: Option<bool>,
}

#[derive(Clone)]
pub struct DebugLedgerNode {
    pub path: String,
    pub label: String,
    pub units: Vec<DebugUnit>,
    pub ledger: Vec<LedgerEntryInfo>,
    pub children: Vec<DebugLedgerNode>,
}

pub trait IDebugLedgerService: Send + Sync {
    fn tree(&self) -> DebugLedgerNode;
}
