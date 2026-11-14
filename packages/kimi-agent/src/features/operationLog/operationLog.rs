use serde::{Deserialize, Serialize};

pub const OPERATION_LOG_RECORD_VERSION: u32 = 1;
pub const OPERATION_LOG_MAX_OPERATIONS: u64 = 1000;
pub const OPERATION_LOG_MAX_AGE_MS: u64 = 30 * 24 * 60 * 60 * 1000;
pub const OPERATION_LOG_DEFAULT_MAX_TOTAL_BYTES: u64 = 1024 * 1024 * 1024;

pub const OPERATION_LOG_DEFAULT_RETENTION: OperationLogRetention = OperationLogRetention {
    max_operations: OPERATION_LOG_MAX_OPERATIONS,
    max_age_ms: OPERATION_LOG_MAX_AGE_MS,
    max_total_bytes: OPERATION_LOG_DEFAULT_MAX_TOTAL_BYTES,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum OperationGitHead {
    Branch {
        #[serde(rename = "ref")]
        ref_name: String,
        target: String,
    },
    Detached {
        target: String,
    },
    Unborn {
        #[serde(rename = "ref")]
        ref_name: String,
    },
}

impl PartialEq for OperationGitHead {
    fn eq(&self, other: &Self) -> bool {
        self.equals(other)
    }
}

impl Eq for OperationGitHead {}

impl OperationGitHead {
    pub fn equals(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Branch {
                    ref_name: left_ref,
                    target: left_target,
                },
                Self::Branch {
                    ref_name: right_ref,
                    target: right_target,
                },
            ) => left_ref == right_ref && left_target == right_target,
            (Self::Detached { target: left }, Self::Detached { target: right }) => left == right,
            (Self::Unborn { ref_name: left }, Self::Unborn { ref_name: right }) => left == right,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationView {
    pub tree: String,
    pub head: OperationGitHead,
    pub refs: std::collections::BTreeMap<String, String>,
    pub index_tree: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unfinished_operation: Option<OperationViewUnfinished>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OperationViewUnfinished {
    Merge,
    Rebase,
}

impl OperationView {
    pub fn equals(&self, other: &Self) -> bool {
        self.tree == other.tree
            && self.index_tree == other.index_tree
            && self.unfinished_operation == other.unfinished_operation
            && self.head.equals(&other.head)
            && refs_equal(&self.refs, &other.refs)
    }
}

fn refs_equal(
    left: &std::collections::BTreeMap<String, String>,
    right: &std::collections::BTreeMap<String, String>,
) -> bool {
    left.len() == right.len()
        && left.iter().zip(right.iter()).all(
            |((left_name, left_target), (right_name, right_target))| {
                left_name == right_name && left_target == right_target
            },
        )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OperationKind {
    Capture,
    Undo,
    Redo,
    Restore,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationOrigin {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationRecordContent {
    pub version: u32,
    pub parents: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_view_id: Option<String>,
    pub view_id: String,
    pub kind: OperationKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_id: Option<String>,
    pub at: String,
    pub description: String,
    pub nonce: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<OperationOrigin>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationRecord {
    pub id: String,
    #[serde(flatten)]
    pub content: OperationRecordContent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredOperationView {
    pub version: u32,
    #[serde(flatten)]
    pub view: OperationView,
}

impl StoredOperationView {
    pub fn new(view: OperationView) -> Self {
        Self {
            version: OPERATION_LOG_RECORD_VERSION,
            view,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationLogHead {
    pub operation_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undo_target_id: Option<String>,
    pub redo_stack: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationLogRetention {
    pub max_operations: u64,
    pub max_age_ms: u64,
    pub max_total_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationActionInput {
    pub at: String,
    pub description: String,
    pub origin: Option<OperationOrigin>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OperationLogErrorCode {
    Busy,
    Corrupt,
    Invalid,
    NotFound,
    NothingToUndo,
    NothingToRedo,
    RecoveryRequired,
    RetentionExceeded,
    StagedChanges,
    UnfinishedOperation,
    ViewMismatch,
}

impl OperationLogErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Busy => "busy",
            Self::Corrupt => "corrupt",
            Self::Invalid => "invalid",
            Self::NotFound => "not-found",
            Self::NothingToUndo => "nothing-to-undo",
            Self::NothingToRedo => "nothing-to-redo",
            Self::RecoveryRequired => "recovery-required",
            Self::RetentionExceeded => "retention-exceeded",
            Self::StagedChanges => "staged-changes",
            Self::UnfinishedOperation => "unfinished-operation",
            Self::ViewMismatch => "view-mismatch",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationLogErrorLimit {
    Operations,
    Age,
    Bytes,
}

impl OperationLogErrorLimit {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Operations => "operations",
            Self::Age => "age",
            Self::Bytes => "bytes",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationLogErrorOperation {
    Merge,
    Rebase,
}

impl OperationLogErrorOperation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::Rebase => "rebase",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationLogErrorDetails {
    pub limit: Option<OperationLogErrorLimit>,
    pub maximum: Option<u64>,
    pub operation: Option<OperationLogErrorOperation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationLogError {
    pub code: OperationLogErrorCode,
    pub message: String,
    pub details: Option<OperationLogErrorDetails>,
}

impl OperationLogError {
    pub fn new(code: OperationLogErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: None,
        }
    }

    pub fn with_details(
        code: OperationLogErrorCode,
        message: impl Into<String>,
        details: OperationLogErrorDetails,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            details: Some(details),
        }
    }
}

impl std::fmt::Display for OperationLogError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for OperationLogError {}

pub fn operation_git_heads_equal(left: &OperationGitHead, right: &OperationGitHead) -> bool {
    match (left, right) {
        (
            OperationGitHead::Branch {
                ref_name: left_ref,
                target: left_target,
            },
            OperationGitHead::Branch {
                ref_name: right_ref,
                target: right_target,
            },
        ) => left_ref == right_ref && left_target == right_target,
        (OperationGitHead::Detached { target: left }, OperationGitHead::Detached { target: right }) => {
            left == right
        }
        (
            OperationGitHead::Unborn { ref_name: left },
            OperationGitHead::Unborn { ref_name: right },
        ) => left == right,
        _ => false,
    }
}

pub fn operation_refs_equal(
    left: &std::collections::BTreeMap<String, String>,
    right: &std::collections::BTreeMap<String, String>,
) -> bool {
    left.len() == right.len()
        && left.iter().zip(right.iter()).all(
            |((left_name, left_target), (right_name, right_target))| {
                left_name == right_name && left_target == right_target
            },
        )
}

pub fn operation_views_equal(left: &OperationView, right: &OperationView) -> bool {
    left.tree == right.tree
        && left.index_tree == right.index_tree
        && left.unfinished_operation == right.unfinished_operation
        && operation_git_heads_equal(&left.head, &right.head)
        && operation_refs_equal(&left.refs, &right.refs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(tree: &str) -> OperationView {
        OperationView {
            tree: tree.into(),
            head: OperationGitHead::Branch {
                ref_name: "main".into(),
                target: "abc123".into(),
            },
            refs: std::collections::BTreeMap::new(),
            index_tree: "index".into(),
            unfinished_operation: None,
        }
    }

    #[test]
    fn branch_heads_compare_by_ref_and_target() {
        assert!(
            OperationGitHead::Branch {
                ref_name: "main".into(),
                target: "abc".into()
            }
            .equals(&OperationGitHead::Branch {
                ref_name: "main".into(),
                target: "abc".into()
            })
        );
        assert!(
            !OperationGitHead::Branch {
                ref_name: "main".into(),
                target: "abc".into()
            }
            .equals(&OperationGitHead::Branch {
                ref_name: "dev".into(),
                target: "abc".into()
            })
        );
        assert!(
            !OperationGitHead::Detached {
                target: "abc".into()
            }
            .equals(&OperationGitHead::Unborn {
                ref_name: "main".into()
            })
        );
    }

    #[test]
    fn views_compare_every_field() {
        let mut refs = std::collections::BTreeMap::new();
        refs.insert("a".to_string(), "1".to_string());
        let mut other_refs = refs.clone();
        other_refs.insert("b".to_string(), "2".to_string());

        let mut left = view("tree");
        left.refs = refs;
        let mut right = view("tree");
        right.refs = other_refs;
        assert!(!left.equals(&right));

        right.refs = left.refs.clone();
        right.tree = "changed".into();
        assert!(!left.equals(&right));

        let mut unfinished = view("tree");
        unfinished.refs = left.refs.clone();
        unfinished.unfinished_operation = Some(OperationViewUnfinished::Merge);
        assert!(!left.equals(&unfinished));
    }

    #[test]
    fn error_codes_keep_the_wire_vocabulary() {
        assert_eq!(
            OperationLogErrorCode::NothingToUndo.as_str(),
            "nothing-to-undo"
        );
        assert_eq!(
            OperationLogErrorCode::ViewMismatch.as_str(),
            "view-mismatch"
        );
        assert_eq!(OperationLogErrorLimit::Operations.as_str(), "operations");
        assert_eq!(OperationLogErrorOperation::Rebase.as_str(), "rebase");
        let error =
            OperationLogError::new(OperationLogErrorCode::NothingToRedo, "redo stack is empty");
        assert_eq!(error.to_string(), "nothing-to-redo: redo stack is empty");
    }

    #[test]
    fn default_retention_matches_v2() {
        assert_eq!(OPERATION_LOG_DEFAULT_RETENTION.max_operations, 1000);
        assert_eq!(
            OPERATION_LOG_DEFAULT_RETENTION.max_age_ms,
            30 * 24 * 60 * 60 * 1000
        );
        assert_eq!(
            OPERATION_LOG_DEFAULT_RETENTION.max_total_bytes,
            1024 * 1024 * 1024
        );
    }
}
