use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Completed,
    ToolCalls,
    Truncated,
    Filtered,
    Paused,
    Error,
    Other,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FinishInfo {
    pub finish_reason: Option<FinishReason>,
    pub raw_finish_reason: Option<String>,
}

pub const NO_FINISH: FinishInfo = FinishInfo {
    finish_reason: None,
    raw_finish_reason: None,
};
