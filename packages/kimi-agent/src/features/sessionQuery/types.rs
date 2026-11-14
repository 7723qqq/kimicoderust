use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRecord {
    pub id: String,
    pub workspace_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    pub live: bool,
    pub persisted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionAvailability {
    Live,
    Persisted,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct SessionResultRange {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum SessionResultFilter {
    Id { values: Vec<String> },
    Cwd { values: Vec<Option<String>> },
    CreatedAt {
        #[serde(flatten)]
        range: SessionResultRange,
    },
    Parent { values: Vec<Option<String>> },
    Availability { values: Vec<SessionAvailability> },
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SessionLineageNode {
    pub session: SessionRecord,
    pub descendants: Vec<SessionLineageNode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionLineageTrace {
    pub target: SessionRecord,
    pub ancestors: Vec<SessionRecord>,
    pub descendants: Vec<SessionLineageNode>,
    pub complete: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<SessionRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unresolved_parent_id: Option<String>,
}
