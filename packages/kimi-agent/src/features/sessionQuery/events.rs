use serde::{Deserialize, Serialize};

use super::cursor::SessionSearchCursor;
use super::types::{SessionRecord, SessionResultFilter, SessionResultRange};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionEventRecord {
    pub session_id: String,
    pub seq: u64,
    pub r#type: String,
    pub time: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum SessionEventResultFilter {
    Seq {
        #[serde(flatten)]
        range: SessionResultRange,
    },
    Time {
        #[serde(flatten)]
        range: SessionResultRange,
    },
    Type { values: Vec<String> },
    Text { text: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SessionEventSearchDocument {
    #[serde(flatten)]
    pub record: SessionEventRecord,
    pub text: String,
}

#[derive(Debug, Clone, Default)]
pub struct SessionEventSearchRequest {
    pub session_id: String,
    pub query: String,
    pub filters: Vec<SessionEventResultFilter>,
    pub limit: Option<usize>,
    pub cursor: Option<SessionSearchCursor>,
}

#[derive(Debug, Clone, Default)]
pub struct SessionSearchRequest {
    pub query: String,
    pub session_filters: Vec<SessionResultFilter>,
    pub event_filters: Vec<SessionEventResultFilter>,
    pub limit: Option<usize>,
    pub cursor: Option<SessionSearchCursor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SessionEventSearchHit {
    #[serde(flatten)]
    pub record: SessionEventRecord,
    pub snippet: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSearchHit {
    #[serde(flatten)]
    pub session: SessionRecord,
    pub best_match: SessionEventSearchHit,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSearchPage<T> {
    pub items: Vec<T>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<SessionSearchCursor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionEventSearchPage {
    pub session_id: String,
    pub items: Vec<SessionEventSearchHit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<SessionSearchCursor>,
}
