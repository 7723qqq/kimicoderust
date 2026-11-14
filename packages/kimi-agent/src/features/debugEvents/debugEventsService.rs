use std::collections::{BTreeMap, HashSet};

use super::debugEvents::{
    DebugEventBusSnapshot, DebugEventEntryKind, DebugEventSubscription, DebugEventSubscriptions,
    IDebugEventsService,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugLedgerEntry {
    pub label: String,
    pub kind: DebugEventEntryKind,
    pub children: Option<Vec<DebugLedgerEntry>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugEventUnit {
    pub identity: usize,
    pub token: String,
    pub uid: Option<u64>,
    pub entries: Option<Vec<DebugLedgerEntry>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugEventBusCounts {
    pub identity: usize,
    pub all: u64,
    pub per_type: BTreeMap<String, u64>,
    pub per_agent: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugEventContainer {
    pub scope_path: String,
    pub units: Vec<DebugEventUnit>,
    pub event_bus: Option<DebugEventBusCounts>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DebugEventRoot {
    pub containers: Vec<DebugEventContainer>,
    pub global_listeners: Option<u64>,
}

pub struct DebugEventsService {
    root: DebugEventRoot,
}

impl DebugEventsService {
    pub fn new(root: DebugEventRoot) -> Self {
        Self { root }
    }

    pub fn subscriptions(&self) -> DebugEventSubscriptions {
        let mut subscriptions = Vec::new();
        let mut buses = Vec::new();
        let mut seen_units = HashSet::new();
        let mut seen_buses = HashSet::new();
        for container in &self.root.containers {
            for unit in &container.units {
                if !seen_units.insert(unit.identity) {
                    continue;
                }
                if let Some(entries) = &unit.entries {
                    collect_event_entries(
                        entries,
                        &mut subscriptions,
                        &container.scope_path,
                        &unit.token,
                        unit.uid,
                    );
                }
            }
            if let Some(bus) = &container.event_bus
                && seen_buses.insert(bus.identity)
            {
                buses.push(DebugEventBusSnapshot {
                    scope_path: container.scope_path.clone(),
                    all: bus.all,
                    per_type: bus.per_type.clone(),
                    per_agent: bus.per_agent.clone(),
                });
            }
        }
        DebugEventSubscriptions {
            subscriptions,
            buses,
            global_listeners: self.root.global_listeners,
        }
    }
}

impl IDebugEventsService for DebugEventsService {
    fn subscriptions(&self) -> DebugEventSubscriptions {
        Self::subscriptions(self)
    }
}

fn collect_event_entries(
    entries: &[DebugLedgerEntry],
    out: &mut Vec<DebugEventSubscription>,
    scope_path: &str,
    unit: &str,
    uid: Option<u64>,
) {
    for entry in entries {
        if is_event_subscription_label(&entry.label) {
            out.push(DebugEventSubscription {
                scope_path: scope_path.to_string(),
                unit: unit.to_string(),
                uid,
                label: entry.label.clone(),
                kind: entry.kind,
            });
        }
        if let Some(children) = &entry.children {
            collect_event_entries(children, out, scope_path, unit, uid);
        }
    }
}

fn is_event_subscription_label(label: &str) -> bool {
    label.starts_with("on:") || label == "disposable:EventSubscription"
}
