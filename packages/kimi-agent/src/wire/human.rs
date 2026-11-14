use std::collections::HashSet;
use std::sync::LazyLock;

use super::tree::tree::AGENT_SWITCHED_TYPE;

pub const HUMAN_AGENT_DOMAIN: &str = "agent";

pub static AGENT_WIRE_RECORD_TYPES: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    HashSet::from([
        AGENT_SWITCHED_TYPE,
        "agent.message.appended",
        "agent.turn.started",
        "agent.turn.ended",
    ])
});

const LEGACY_HUMAN_RECORD_PREFIX: &str = "human.";

pub fn is_human_record_type(record_type: &str) -> bool {
    AGENT_WIRE_RECORD_TYPES.contains(record_type)
        || record_type.starts_with(LEGACY_HUMAN_RECORD_PREFIX)
}

pub fn human_record_type(domain: &str, record_type: &str) -> String {
    format!("{domain}.{record_type}")
}

pub fn human_event_type(record_type: &str, domain: &str) -> Option<String> {
    if record_type == AGENT_SWITCHED_TYPE {
        return None;
    }
    for prefix in [
        format!("{domain}."),
        format!("{LEGACY_HUMAN_RECORD_PREFIX}{domain}."),
    ] {
        if let Some(event_type) = record_type.strip_prefix(&prefix) {
            return (!event_type.is_empty()).then(|| event_type.to_string());
        }
    }
    None
}
