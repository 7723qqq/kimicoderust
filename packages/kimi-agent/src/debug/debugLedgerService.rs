use std::sync::Arc;

use crate::_base::di::instantiation_service::InstantiationService;

use super::debug_ledger::{DebugLedgerNode, DebugUnit, IDebugLedgerService};
use super::scope_tree::scope_container_label;

pub struct DebugLedgerService {
    root: Arc<InstantiationService>,
}

impl DebugLedgerService {
    pub fn new(root: Arc<InstantiationService>) -> Self {
        Self { root }
    }

    fn node(&self, container: &Arc<InstantiationService>, path: String) -> DebugLedgerNode {
        DebugLedgerNode {
            path: path.clone(),
            label: scope_container_label(container),
            units: join_units(container),
            ledger: container.ledger().entries(),
            children: container
                .children()
                .iter()
                .map(|child| self.node(child, format!("{path}/{}", scope_container_label(child))))
                .collect(),
        }
    }
}

impl IDebugLedgerService for DebugLedgerService {
    fn tree(&self) -> DebugLedgerNode {
        self.node(&self.root, scope_container_label(&self.root))
    }
}

fn join_units(container: &InstantiationService) -> Vec<DebugUnit> {
    let states = container
        .cascade()
        .units_snapshot()
        .into_iter()
        .map(|unit| (unit.token.clone(), unit))
        .collect::<std::collections::HashMap<_, _>>();
    container
        .services_snapshot()
        .into_iter()
        .map(|(token, uid)| {
            let unit = states.get(&token);
            DebugUnit {
                token,
                uid,
                state: unit.map(|unit| unit.state),
                error: unit.and_then(|unit| unit.error.clone()),
                ever_active: unit.map(|unit| unit.ever_active),
                in_flight: unit.map(|unit| unit.in_flight),
            }
        })
        .collect()
}
