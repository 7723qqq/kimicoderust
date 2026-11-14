use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::session_index::SessionSummary;

pub const SESSION_INDEX_MANIFEST: &str = "sessionIndex";
pub const SESSION_INDEX_SCHEMA_VERSION: u64 = 3;
pub const PARENT_INDEX_NAME: &str = "byParent";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionWorkspaceCounts {
    pub active: i64,
    pub archived: i64,
}

pub fn session_collection(generation: u64) -> String {
    format!("session:g{generation}")
}

pub fn session_counters_collection(generation: u64) -> String {
    format!("sessionCounters:g{generation}")
}

pub fn recency_column(generation: u64) -> String {
    format!("g{generation}:updatedAt")
}

pub fn with_recency_field(generation: u64, mut summary: SessionSummary) -> SessionSummary {
    summary.extra.insert(
        recency_column(generation),
        Value::Number(summary.updated_at.into()),
    );
    summary
}

pub fn strip_recency_field(generation: u64, mut summary: SessionSummary) -> SessionSummary {
    summary.extra.remove(&recency_column(generation));
    summary
}
