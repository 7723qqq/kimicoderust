use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::wire::record::WireAsyncError;

pub use super::git_service::GitService;
pub use super::work_tree::GitWorkTree;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FsGitStatus {
    Clean,
    Modified,
    Added,
    Deleted,
    Renamed,
    Untracked,
    Ignored,
    Conflicted,
}

pub static FS_GIT_STATUS_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "enum": ["clean", "modified", "added", "deleted", "renamed", "untracked", "ignored", "conflicted"]
    })
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FsPullRequestState {
    Open,
    Merged,
    Closed,
    Draft,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsPullRequest {
    pub number: u64,
    pub state: FsPullRequestState,
    pub url: String,
}

pub static FS_PULL_REQUEST_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "required": ["number", "state", "url"],
        "properties": {
            "number": {"type": "integer", "exclusiveMinimum": 0},
            "state": {"enum": ["open", "merged", "closed", "draft"]},
            "url": {"type": "string", "format": "uri"}
        }
    })
});

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsGitStatusRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_pull_request: Option<bool>,
}

pub static FS_GIT_STATUS_REQUEST_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "properties": {
            "root": {"type": "string", "minLength": 1},
            "paths": {"type": "array", "items": {"type": "string", "minLength": 1}},
            "include_pull_request": {"type": "boolean"}
        }
    })
});

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsPullRequestRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
}

pub static FS_PULL_REQUEST_REQUEST_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "properties": {"root": {"type": "string", "minLength": 1}}
    })
});

#[derive(Debug, Clone)]
pub struct GitStatusOptions {
    pub path_filter: Option<HashSet<String>>,
    pub scope_to_cwd: bool,
    pub include_pull_request: bool,
}

impl Default for GitStatusOptions {
    fn default() -> Self {
        Self {
            path_filter: None,
            scope_to_cwd: false,
            include_pull_request: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsGitStatusResponse {
    pub branch: String,
    pub ahead: u64,
    pub behind: u64,
    pub entries: HashMap<String, FsGitStatus>,
    pub additions: u64,
    pub deletions: u64,
    pub pull_request: Option<FsPullRequest>,
}

pub static FS_GIT_STATUS_RESPONSE_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "required": ["branch", "ahead", "behind", "entries", "additions", "deletions", "pullRequest"],
        "properties": {
            "branch": {"type": "string"},
            "ahead": {"type": "integer", "minimum": 0},
            "behind": {"type": "integer", "minimum": 0},
            "entries": {"type": "object", "additionalProperties": FS_GIT_STATUS_SCHEMA.clone()},
            "additions": {"type": "integer", "minimum": 0},
            "deletions": {"type": "integer", "minimum": 0},
            "pullRequest": {"anyOf": [FS_PULL_REQUEST_SCHEMA.clone(), {"type": "null"}]}
        }
    })
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsBatchError {
    pub code: i64,
    pub msg: String,
}

pub static FS_BATCH_ERROR_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "required": ["code", "msg"],
        "properties": {"code": {"type": "integer"}, "msg": {"type": "string"}}
    })
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsGitStatusBatchRequest {
    pub roots: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_pull_request: Option<bool>,
}

pub static FS_GIT_STATUS_BATCH_REQUEST_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "required": ["roots"],
        "properties": {
            "roots": {"type": "array", "items": {"type": "string", "minLength": 1}, "minItems": 1, "maxItems": 32},
            "include_pull_request": {"type": "boolean"}
        }
    })
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsGitStatusBatchResponse {
    pub results: HashMap<String, FsGitStatusResponse>,
    pub errors: HashMap<String, FsBatchError>,
}

pub static FS_GIT_STATUS_BATCH_RESPONSE_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "required": ["results", "errors"],
        "properties": {
            "results": {"type": "object", "additionalProperties": FS_GIT_STATUS_RESPONSE_SCHEMA.clone()},
            "errors": {"type": "object", "additionalProperties": FS_BATCH_ERROR_SCHEMA.clone()}
        }
    })
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsPullRequestBatchRequest {
    pub roots: Vec<String>,
}

pub static FS_PULL_REQUEST_BATCH_REQUEST_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "required": ["roots"],
        "properties": {
            "roots": {"type": "array", "items": {"type": "string", "minLength": 1}, "minItems": 1, "maxItems": 32}
        }
    })
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsPullRequestBatchResponse {
    pub results: HashMap<String, Option<FsPullRequest>>,
    pub errors: HashMap<String, FsBatchError>,
}

pub static FS_PULL_REQUEST_BATCH_RESPONSE_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "required": ["results", "errors"],
        "properties": {
            "results": {"type": "object", "additionalProperties": {"anyOf": [FS_PULL_REQUEST_SCHEMA.clone(), {"type": "null"}]}},
            "errors": {"type": "object", "additionalProperties": FS_BATCH_ERROR_SCHEMA.clone()}
        }
    })
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsDiffRequest {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
}

pub static FS_DIFF_REQUEST_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "required": ["path"],
        "properties": {
            "path": {"type": "string", "minLength": 1},
            "root": {"type": "string", "minLength": 1}
        }
    })
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsDiffResponse {
    pub path: String,
    pub diff: String,
    pub truncated: bool,
}

pub static FS_DIFF_RESPONSE_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "required": ["path", "diff", "truncated"],
        "properties": {
            "path": {"type": "string"},
            "diff": {"type": "string"},
            "truncated": {"type": "boolean"}
        }
    })
});

#[async_trait]
pub trait IGitService: Send + Sync {
    async fn status(
        &self,
        cwd: &str,
        options: Option<GitStatusOptions>,
    ) -> Result<FsGitStatusResponse, WireAsyncError>;

    async fn pull_request(&self, cwd: &str) -> Result<Option<FsPullRequest>, WireAsyncError>;

    async fn diff(
        &self,
        cwd: &str,
        rel_path: &str,
        abs_path: &str,
    ) -> Result<FsDiffResponse, WireAsyncError>;

    async fn find_work_tree(&self, cwd: &str) -> Result<Option<GitWorkTree>, WireAsyncError>;
}
