use std::convert::Infallible;
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub type Result<T, E> = std::result::Result<T, E>;

pub fn ok<T>(value: T) -> Result<T, Infallible> {
    Ok(value)
}

pub fn err<E>(error: E) -> Result<Infallible, E> {
    Err(error)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoreError {
    pub code: String,
    pub message: String,
}

impl StoreError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for StoreError {}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchHeader {
    pub version: u8,
    pub tree: String,
    pub branch: String,
    pub created_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_seq: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Payload {
    Inline {
        kind: String,
        size: usize,
        data: Value,
    },
    Offloaded {
        kind: String,
        size: usize,
        #[serde(rename = "ref")]
        reference: String,
    },
}

impl Payload {
    pub fn kind(&self) -> &str {
        match self {
            Self::Inline { kind, .. } | Self::Offloaded { kind, .. } => kind,
        }
    }

    pub fn size(&self) -> usize {
        match self {
            Self::Inline { size, .. } | Self::Offloaded { size, .. } => *size,
        }
    }
}

pub fn is_offloaded_payload(payload: &Payload) -> bool {
    matches!(payload, Payload::Offloaded { .. })
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EntryLine {
    pub kind: String,
    pub seq: u64,
    pub ts: u64,
    #[serde(rename = "type")]
    pub type_: String,
    pub payload: Payload,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CorruptionKind {
    Missing,
    Syntax,
    Schema,
    SeqGap,
    ParentRef,
    Header,
    BlobMissing,
    BlobCrc,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CorruptionReport {
    pub tree: String,
    pub branch: String,
    pub seq: Option<u64>,
    pub line: usize,
    pub kind: CorruptionKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
    pub detail: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BranchRef {
    pub branch: String,
    pub seq: u64,
}

pub trait Subscriber: Send + Sync {
    fn on_append(&self, _tree: &str, _branch: &str, _entry: &EntryLine) {}
    fn on_corruption(&self, _report: &CorruptionReport) {}
}

#[derive(Clone)]
pub struct SubscriberRegistration {
    pub prefix: String,
    pub subscriber: Arc<dyn Subscriber>,
}

#[derive(Clone, Default)]
pub struct TreeStoreOptions {
    pub offload_threshold: Option<usize>,
    pub fsync: Option<bool>,
    pub subscribers: Vec<SubscriberRegistration>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AppendInput {
    #[serde(rename = "type")]
    pub type_: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

pub const DEFAULT_OFFLOAD_THRESHOLD: usize = 64 * 1024;
