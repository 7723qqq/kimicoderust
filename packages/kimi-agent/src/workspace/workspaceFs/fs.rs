use std::collections::HashMap;
use std::pin::Pin;
use std::sync::LazyLock;

use async_trait::async_trait;
use futures_util::Stream;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::_base::utils::abort::AbortSignal;
use crate::wire::record::WireAsyncError;

pub use crate::app::git::git::{
    FS_DIFF_REQUEST_SCHEMA as fsDiffRequestSchema, FS_DIFF_RESPONSE_SCHEMA as fsDiffResponseSchema,
    FS_GIT_STATUS_BATCH_REQUEST_SCHEMA as fsGitStatusBatchRequestSchema,
    FS_GIT_STATUS_BATCH_RESPONSE_SCHEMA as fsGitStatusBatchResponseSchema,
    FS_GIT_STATUS_REQUEST_SCHEMA as fsGitStatusRequestSchema,
    FS_GIT_STATUS_RESPONSE_SCHEMA as fsGitStatusResponseSchema,
    FS_PULL_REQUEST_BATCH_REQUEST_SCHEMA as fsPullRequestBatchRequestSchema,
    FS_PULL_REQUEST_BATCH_RESPONSE_SCHEMA as fsPullRequestBatchResponseSchema,
    FS_PULL_REQUEST_REQUEST_SCHEMA as fsPullRequestRequestSchema,
    FS_PULL_REQUEST_SCHEMA as fsPullRequestSchema, FsBatchError, FsDiffRequest, FsDiffResponse,
    FsGitStatusBatchRequest, FsGitStatusBatchResponse, FsGitStatusRequest, FsGitStatusResponse,
    FsPullRequest, FsPullRequestBatchRequest, FsPullRequestBatchResponse, FsPullRequestRequest,
};
use crate::app::git::git::FsGitStatus;

macro_rules! schema {
    ($name:ident, $value:expr) => {
        pub static $name: LazyLock<Value> = LazyLock::new(|| $value);
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FsKind {
    File,
    Directory,
    Symlink,
}

schema!(FS_KIND_SCHEMA, json!({"enum": ["file", "directory", "symlink"]}));

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FsEntry {
    pub path: String,
    pub name: String,
    pub kind: FsKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    pub modified_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_binary: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_symlink_to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_directory: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_status: Option<FsGitStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_count: Option<u64>,
}

schema!(FS_ENTRY_SCHEMA, json!({
    "type": "object",
    "required": ["path", "name", "kind", "modified_at"],
    "properties": {
        "path": {"type": "string"}, "name": {"type": "string"},
        "kind": {"$ref": "fsKind"}, "size": {"type": "integer", "minimum": 0},
        "modified_at": {"type": "string", "format": "date-time"},
        "etag": {"type": "string"}, "mime": {"type": "string"},
        "language_id": {"type": "string"}, "is_binary": {"type": "boolean"},
        "is_symlink_to": {"type": "string"}, "is_directory": {"type": "boolean"},
        "git_status": {"$ref": "fsGitStatus"},
        "child_count": {"type": "integer", "minimum": 0}
    }
}));

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FsSearchHit {
    pub path: String,
    pub name: String,
    pub kind: FsKind,
    pub score: f64,
    pub match_positions: Vec<u64>,
}

schema!(FS_SEARCH_HIT_SCHEMA, json!({
    "type": "object", "required": ["path", "name", "kind", "score", "match_positions"],
    "properties": {
        "path": {"type": "string"}, "name": {"type": "string"}, "kind": {"$ref": "fsKind"},
        "score": {"type": "number", "minimum": 0, "maximum": 1},
        "match_positions": {"type": "array", "items": {"type": "integer", "minimum": 0}}
    }
}));

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsGrepMatch {
    pub line: u64,
    pub col: u64,
    pub text: String,
    pub before: Vec<String>,
    pub after: Vec<String>,
}

schema!(FS_GREP_MATCH_SCHEMA, json!({
    "type": "object", "required": ["line", "col", "text", "before", "after"],
    "properties": {
        "line": {"type": "integer", "minimum": 1}, "col": {"type": "integer", "minimum": 1},
        "text": {"type": "string"}, "before": {"type": "array", "items": {"type": "string"}},
        "after": {"type": "array", "items": {"type": "string"}}
    }
}));

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsGrepFileHit {
    pub path: String,
    pub matches: Vec<FsGrepMatch>,
}

schema!(FS_GREP_FILE_HIT_SCHEMA, json!({
    "type": "object", "required": ["path", "matches"],
    "properties": {"path": {"type": "string"}, "matches": {"type": "array", "items": {"$ref": "fsGrepMatch"}}}
}));

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FsListSort {
    TypeFirst,
    NameAsc,
    NameDesc,
    MtimeDesc,
    SizeDesc,
}

impl Default for FsListSort {
    fn default() -> Self {
        Self::TypeFirst
    }
}

schema!(FS_LIST_SORT_SCHEMA, json!({"enum": ["type_first", "name_asc", "name_desc", "mtime_desc", "size_desc"]}));

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsListRequest {
    #[serde(default = "default_list_path")]
    pub path: String,
    #[serde(default = "default_list_depth")]
    pub depth: u32,
    #[serde(default = "default_list_limit")]
    pub limit: usize,
    #[serde(default)]
    pub show_hidden: bool,
    #[serde(default = "default_true")]
    pub follow_gitignore: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_globs: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_ignored_globs: Option<Vec<String>>,
    #[serde(default)]
    pub sort: FsListSort,
    #[serde(default)]
    pub include_git_status: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_external_symlinks: Option<bool>,
}

impl Default for FsListRequest {
    fn default() -> Self {
        Self {
            path: default_list_path(), depth: default_list_depth(), limit: default_list_limit(),
            show_hidden: false, follow_gitignore: true, exclude_globs: None,
            allow_ignored_globs: None, sort: FsListSort::default(), include_git_status: false,
            allow_external_symlinks: None,
        }
    }
}

schema!(FS_LIST_REQUEST_SCHEMA, json!({
    "type": "object", "properties": {
        "path": {"type": "string", "default": "."}, "depth": {"type": "integer", "minimum": 1, "maximum": 10, "default": 1},
        "limit": {"type": "integer", "minimum": 1, "maximum": 1000, "default": 200},
        "show_hidden": {"type": "boolean", "default": false}, "follow_gitignore": {"type": "boolean", "default": true},
        "exclude_globs": {"type": "array", "items": {"type": "string"}},
        "allow_ignored_globs": {"type": "array", "items": {"type": "string"}},
        "sort": {"$ref": "fsListSort", "default": "type_first"},
        "include_git_status": {"type": "boolean", "default": false}, "allow_external_symlinks": {"type": "boolean"}
    }
}));

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FsListResponse {
    pub items: Vec<FsEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub children_by_path: Option<HashMap<String, Vec<FsEntry>>>,
    pub truncated: bool,
}

schema!(FS_LIST_RESPONSE_SCHEMA, json!({
    "type": "object", "required": ["items", "truncated"],
    "properties": {"items": {"type": "array", "items": {"$ref": "fsEntry"}},
        "children_by_path": {"type": "object", "additionalProperties": {"type": "array", "items": {"$ref": "fsEntry"}}},
        "truncated": {"type": "boolean"}}
}));

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FsReadEncodingRequest {
    Auto,
    #[serde(rename = "utf-8")]
    Utf8,
    Base64,
}

impl Default for FsReadEncodingRequest {
    fn default() -> Self {
        Self::Auto
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FsReadEncoding {
    #[serde(rename = "utf-8")]
    Utf8,
    Base64,
}

schema!(FS_READ_ENCODING_REQUEST_SCHEMA, json!({"enum": ["auto", "utf-8", "base64"]}));
schema!(FS_READ_ENCODING_RESPONSE_SCHEMA, json!({"enum": ["utf-8", "base64"]}));

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsReadRequest {
    pub path: String,
    #[serde(default)]
    pub offset: u64,
    #[serde(default = "default_read_length")]
    pub length: u64,
    #[serde(default)]
    pub encoding: FsReadEncodingRequest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_external_symlinks: Option<bool>,
}

schema!(FS_READ_REQUEST_SCHEMA, json!({
    "type": "object", "required": ["path"], "properties": {
        "path": {"type": "string", "minLength": 1}, "offset": {"type": "integer", "minimum": 0, "default": 0},
        "length": {"type": "integer", "minimum": 1, "maximum": 10485760, "default": 1048576},
        "encoding": {"$ref": "fsReadEncodingRequest", "default": "auto"}, "allow_external_symlinks": {"type": "boolean"}
    }
}));

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FsReadResponse {
    pub path: String,
    pub content: String,
    pub encoding: FsReadEncoding,
    pub size: u64,
    pub truncated: bool,
    pub etag: String,
    pub mime: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_count: Option<u64>,
    pub is_binary: bool,
}

schema!(FS_READ_RESPONSE_SCHEMA, json!({
    "type": "object", "required": ["path", "content", "encoding", "size", "truncated", "etag", "mime", "is_binary"],
    "properties": {
        "path": {"type": "string"}, "content": {"type": "string"}, "encoding": {"$ref": "fsReadEncodingResponse"},
        "size": {"type": "integer", "minimum": 0}, "truncated": {"type": "boolean"}, "etag": {"type": "string"},
        "mime": {"type": "string"}, "language_id": {"type": "string"},
        "line_count": {"type": "integer", "minimum": 0}, "is_binary": {"type": "boolean"}
    }
}));

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsMkdirRequest {
    pub path: String,
    #[serde(default)]
    pub recursive: bool,
}

schema!(FS_MKDIR_REQUEST_SCHEMA, json!({
    "type": "object", "required": ["path"], "properties": {"path": {"type": "string", "minLength": 1}, "recursive": {"type": "boolean", "default": false}}
}));

pub type FsMkdirResponse = FsEntry;
pub use FS_ENTRY_SCHEMA as FS_MKDIR_RESPONSE_SCHEMA;

schema!(FS_LIST_MANY_REQUEST_SCHEMA, json!({
    "type": "object", "required": ["paths"], "properties": {
        "paths": {"type": "array", "minItems": 1, "maxItems": 100, "items": {"type": "string", "minLength": 1}},
        "depth": {"type": "integer", "minimum": 1, "maximum": 10, "default": 1},
        "limit": {"type": "integer", "minimum": 1, "maximum": 1000, "default": 200},
        "show_hidden": {"type": "boolean", "default": false}, "follow_gitignore": {"type": "boolean", "default": true},
        "exclude_globs": {"type": "array", "items": {"type": "string"}},
        "sort": {"$ref": "fsListSort", "default": "type_first"}, "include_git_status": {"type": "boolean", "default": false}
    }
}));

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsListManyRequest {
    pub paths: Vec<String>,
    #[serde(default = "default_list_depth")]
    pub depth: u32,
    #[serde(default = "default_list_limit")]
    pub limit: usize,
    #[serde(default)]
    pub show_hidden: bool,
    #[serde(default = "default_true")]
    pub follow_gitignore: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_globs: Option<Vec<String>>,
    #[serde(default)]
    pub sort: FsListSort,
    #[serde(default)]
    pub include_git_status: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsListManyPartialError {
    pub code: i64,
    pub msg: String,
}

schema!(FS_LIST_MANY_PARTIAL_ERROR_SCHEMA, json!({
    "type": "object", "required": ["code", "msg"], "properties": {"code": {"type": "integer"}, "msg": {"type": "string"}}
}));

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FsListManyResponse {
    pub results: HashMap<String, Vec<FsEntry>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated_paths: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial_errors: Option<HashMap<String, FsListManyPartialError>>,
}

schema!(FS_LIST_MANY_RESPONSE_SCHEMA, json!({
    "type": "object", "required": ["results"], "properties": {
        "results": {"type": "object", "additionalProperties": {"type": "array", "items": {"$ref": "fsEntry"}}},
        "truncated_paths": {"type": "array", "items": {"type": "string"}},
        "partial_errors": {"type": "object", "additionalProperties": {"$ref": "fsListManyPartialError"}}
    }
}));

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsStatRequest {
    pub path: String,
}

schema!(FS_STAT_REQUEST_SCHEMA, json!({"type": "object", "required": ["path"], "properties": {"path": {"type": "string", "minLength": 1}}}));
pub type FsStatResponse = FsEntry;
pub use FS_ENTRY_SCHEMA as FS_STAT_RESPONSE_SCHEMA;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsStatManyRequest {
    pub paths: Vec<String>,
}

schema!(FS_STAT_MANY_REQUEST_SCHEMA, json!({
    "type": "object", "required": ["paths"], "properties": {"paths": {"type": "array", "minItems": 1, "maxItems": 1000, "items": {"type": "string", "minLength": 1}}}
}));

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FsStatManyResponse {
    pub entries: HashMap<String, Option<FsEntry>>,
}

schema!(FS_STAT_MANY_RESPONSE_SCHEMA, json!({
    "type": "object", "required": ["entries"],
    "properties": {"entries": {"type": "object", "additionalProperties": {"anyOf": [{"$ref": "fsEntry"}, {"type": "null"}]}}}
}));

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsSearchRequest {
    pub query: String,
    #[serde(default = "default_search_limit")]
    pub limit: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_globs: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_globs: Option<Vec<String>>,
    #[serde(default)]
    pub follow_gitignore: bool,
}

schema!(FS_SEARCH_REQUEST_SCHEMA, json!({
    "type": "object", "required": ["query"], "properties": {
        "query": {"type": "string"}, "limit": {"type": "integer", "minimum": 1, "maximum": 200, "default": 50},
        "include_globs": {"type": "array", "items": {"type": "string"}}, "exclude_globs": {"type": "array", "items": {"type": "string"}},
        "follow_gitignore": {"type": "boolean", "default": false}
    }
}));

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FsSearchResponse {
    pub items: Vec<FsSearchHit>,
    pub truncated: bool,
}

schema!(FS_SEARCH_RESPONSE_SCHEMA, json!({
    "type": "object", "required": ["items", "truncated"], "properties": {"items": {"type": "array", "items": {"$ref": "fsSearchHit"}}, "truncated": {"type": "boolean"}}
}));

pub type FsSuggestItem = FsSearchHit;
pub type FsSuggestResponse = FsSearchResponse;

schema!(FS_SUGGEST_ITEM_SCHEMA, json!({
    "type": "object", "required": ["path", "name", "kind", "score", "match_positions"],
    "properties": {
        "path": {"type": "string"}, "name": {"type": "string"}, "kind": {"$ref": "fsKind"},
        "score": {"type": "number", "minimum": 0, "maximum": 1},
        "match_positions": {"type": "array", "items": {"type": "integer", "minimum": 0}}
    }
}));

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsSuggestRequest {
    pub query: String,
    #[serde(default = "default_search_limit")]
    pub limit: usize,
    #[serde(default)]
    pub follow_gitignore: bool,
    #[serde(default)]
    pub show_hidden: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_globs: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_globs: Option<Vec<String>>,
}

schema!(FS_SUGGEST_REQUEST_SCHEMA, json!({
    "type": "object", "required": ["query"], "properties": {
        "query": {"type": "string"}, "limit": {"type": "integer", "minimum": 1, "maximum": 200, "default": 50},
        "follow_gitignore": {"type": "boolean", "default": false}, "show_hidden": {"type": "boolean", "default": false},
        "include_globs": {"type": "array", "items": {"type": "string"}}, "exclude_globs": {"type": "array", "items": {"type": "string"}}
    }
}));
schema!(FS_SUGGEST_RESPONSE_SCHEMA, json!({
    "type": "object", "required": ["items", "truncated"], "properties": {"items": {"type": "array", "items": {"$ref": "fsSuggestItem"}}, "truncated": {"type": "boolean"}}
}));

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FsSuggestStreamPhase {
    Preview,
    Final,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FsSuggestStreamEvent {
    pub phase: FsSuggestStreamPhase,
    pub response: FsSuggestResponse,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsGrepRequest {
    pub pattern: String,
    #[serde(default)]
    pub regex: bool,
    #[serde(default = "default_true")]
    pub case_sensitive: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_globs: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_globs: Option<Vec<String>>,
    #[serde(default = "default_true")]
    pub follow_gitignore: bool,
    #[serde(default = "default_grep_max_files")]
    pub max_files: usize,
    #[serde(default = "default_grep_max_matches_per_file")]
    pub max_matches_per_file: usize,
    #[serde(default = "default_grep_max_total_matches")]
    pub max_total_matches: usize,
    #[serde(default = "default_grep_context_lines")]
    pub context_lines: usize,
}

schema!(FS_GREP_REQUEST_SCHEMA, json!({
    "type": "object", "required": ["pattern"], "properties": {
        "pattern": {"type": "string", "minLength": 1}, "regex": {"type": "boolean", "default": false},
        "case_sensitive": {"type": "boolean", "default": true},
        "include_globs": {"type": "array", "items": {"type": "string"}}, "exclude_globs": {"type": "array", "items": {"type": "string"}},
        "follow_gitignore": {"type": "boolean", "default": true},
        "max_files": {"type": "integer", "minimum": 1, "maximum": 10000, "default": 200},
        "max_matches_per_file": {"type": "integer", "minimum": 1, "maximum": 10000, "default": 50},
        "max_total_matches": {"type": "integer", "minimum": 1, "maximum": 100000, "default": 5000},
        "context_lines": {"type": "integer", "minimum": 0, "maximum": 10, "default": 2}
    }
}));

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsGrepResponse {
    pub files: Vec<FsGrepFileHit>,
    pub files_scanned: usize,
    pub truncated: bool,
    pub elapsed_ms: u64,
}

schema!(FS_GREP_RESPONSE_SCHEMA, json!({
    "type": "object", "required": ["files", "files_scanned", "truncated", "elapsed_ms"],
    "properties": {
        "files": {"type": "array", "items": {"$ref": "fsGrepFileHit"}}, "files_scanned": {"type": "integer", "minimum": 0},
        "truncated": {"type": "boolean"}, "elapsed_ms": {"type": "integer", "minimum": 0}
    }
}));

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsPathResolved {
    pub absolute: String,
    pub relative: String,
    pub is_directory: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsDownloadResolved {
    pub absolute: String,
    pub relative: String,
    pub size: u64,
    pub etag: String,
    pub mime: String,
    pub modified_at: chrono::DateTime<chrono::Utc>,
}

#[async_trait]
pub trait IWorkspaceFsService: Send + Sync {
    async fn list(&self, req: FsListRequest) -> Result<FsListResponse, WireAsyncError>;
    async fn read(&self, req: FsReadRequest) -> Result<FsReadResponse, WireAsyncError>;
    async fn list_many(&self, req: FsListManyRequest) -> Result<FsListManyResponse, WireAsyncError>;
    async fn stat(&self, req: FsStatRequest) -> Result<FsStatResponse, WireAsyncError>;
    async fn stat_many(&self, req: FsStatManyRequest) -> Result<FsStatManyResponse, WireAsyncError>;
    async fn mkdir(&self, req: FsMkdirRequest) -> Result<FsMkdirResponse, WireAsyncError>;
    async fn search(&self, req: FsSearchRequest) -> Result<FsSearchResponse, WireAsyncError>;
    async fn suggest(
        &self,
        req: FsSuggestRequest,
        signal: Option<AbortSignal>,
    ) -> Result<FsSuggestResponse, WireAsyncError>;
    fn suggest_stream(
        &self,
        req: FsSuggestRequest,
        signal: Option<AbortSignal>,
    ) -> Option<Pin<Box<dyn Stream<Item = Result<FsSuggestStreamEvent, WireAsyncError>> + Send>>>;
    async fn grep(&self, req: FsGrepRequest) -> Result<FsGrepResponse, WireAsyncError>;
    async fn git_status(&self, req: FsGitStatusRequest) -> Result<FsGitStatusResponse, WireAsyncError>;
    async fn git_status_batch(
        &self,
        req: FsGitStatusBatchRequest,
    ) -> Result<FsGitStatusBatchResponse, WireAsyncError>;
    async fn pull_request(&self, req: FsPullRequestRequest) -> Result<Option<FsPullRequest>, WireAsyncError>;
    async fn pull_request_batch(
        &self,
        req: FsPullRequestBatchRequest,
    ) -> Result<FsPullRequestBatchResponse, WireAsyncError>;
    async fn diff(&self, req: FsDiffRequest) -> Result<FsDiffResponse, WireAsyncError>;
    async fn resolve_path(&self, rel_path: &str) -> Result<FsPathResolved, WireAsyncError>;
    async fn resolve_download(
        &self,
        rel_path: &str,
        allow_external_symlinks: bool,
    ) -> Result<FsDownloadResolved, WireAsyncError>;
}

fn default_list_path() -> String {
    ".".to_string()
}

fn default_list_depth() -> u32 {
    1
}

fn default_list_limit() -> usize {
    200
}

fn default_read_length() -> u64 {
    1_048_576
}

fn default_search_limit() -> usize {
    50
}

fn default_grep_max_files() -> usize {
    200
}

fn default_grep_max_matches_per_file() -> usize {
    50
}

fn default_grep_max_total_matches() -> usize {
    5000
}

fn default_grep_context_lines() -> usize {
    2
}

fn default_true() -> bool {
    true
}
