use serde::{Deserialize, Serialize};

pub use crate::compaction::*;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionResult {
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_summary: Option<String>,
    pub compacted_count: usize,
    pub tokens_before: u64,
    pub tokens_after: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kept_user_message_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kept_head_user_message_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dropped_count: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompactionSource {
    Manual,
    Auto,
}

impl CompactionSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Auto => "auto",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionBeginData {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction: Option<String>,
    pub source: CompactionSource,
}
