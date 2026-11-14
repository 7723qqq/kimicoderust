use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::rpc::types::BoxFuture;
use crate::tool_input_display::ToolInputDisplay;

pub const SNAPSHOT_RECORD_VERSION: u32 = 1;
pub const SNAPSHOT_MAX_BYTES: u64 = 4 * 1024 * 1024;
pub const SNAPSHOT_DIR_NAME: &str = "snapshots";

pub const SNAPSHOT_PRUNE_MAX_PER_PATH: usize = 100;
pub const SNAPSHOT_PRUNE_MAX_BYTES_PER_PATH: u64 = 64 * 1024 * 1024;
pub const SNAPSHOT_PRUNE_MAX_AGE_MS: i64 = 30 * 24 * 60 * 60 * 1000;
pub const SNAPSHOT_PRUNE_MIN_OBJECT_AGE_MS: i64 = 60 * 1000;
pub const SNAPSHOT_PRUNE_INTERVAL_MS: i64 = 60 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SnapshotWriteMode {
    Overwrite,
    Edit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SnapshotSkipReason {
    MaxSize,
    Unreadable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotTarget {
    pub path: String,
    pub tool: String,
    pub mode: SnapshotWriteMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotRecord {
    pub version: u32,
    pub id: String,
    pub path: String,
    pub hash: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime_ms: Option<i64>,
    pub at: String,
    pub tool: String,
    pub mode: SnapshotWriteMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<SnapshotSkipReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_size: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotRecordInput {
    pub path: String,
    pub at: String,
    pub tool: String,
    pub mode: SnapshotWriteMode,
    pub size: u64,
    pub bytes: Option<Vec<u8>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<SnapshotSkipReason>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotAgentMark {
    pub version: u32,
    pub agent_id: String,
    pub session_id: String,
    pub at: String,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swarm_index: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotAgentMarkInput {
    pub agent_id: String,
    pub session_id: String,
    pub at: String,
    pub cwd: String,
    pub parent_agent_id: Option<String>,
    pub profile_name: Option<String>,
    pub swarm_index: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotPruneOptions {
    pub now_ms: i64,
    pub max_per_path: usize,
    pub max_bytes_per_path: u64,
    pub max_age_ms: i64,
    pub min_object_age_ms: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SnapshotPruneResult {
    pub records_removed: usize,
    pub objects_removed: usize,
    pub agent_marks_removed: usize,
}

pub trait SnapshotStore: Send + Sync {
    fn record<'a>(
        &'a self,
        input: SnapshotRecordInput,
    ) -> BoxFuture<'a, Result<SnapshotRecord, String>>;

    fn complete<'a>(
        &'a self,
        path: String,
        id: String,
        after_hash: String,
        after_size: u64,
    ) -> BoxFuture<'a, Result<bool, String>>;

    fn remove<'a>(
        &'a self,
        path: String,
        id: String,
    ) -> BoxFuture<'a, Result<bool, String>>;

    fn list<'a>(&'a self, path: String) -> BoxFuture<'a, Result<Vec<SnapshotRecord>, String>>;

    fn content<'a>(&'a self, hash: String) -> BoxFuture<'a, Result<Option<Vec<u8>>, String>>;

    fn mark_agent<'a>(
        &'a self,
        input: SnapshotAgentMarkInput,
    ) -> BoxFuture<'a, Result<(), String>>;

    fn agents<'a>(
        &'a self,
        session_id: String,
    ) -> BoxFuture<'a, Result<Vec<SnapshotAgentMark>, String>>;

    fn prune<'a>(
        &'a self,
        options: SnapshotPruneOptions,
    ) -> BoxFuture<'a, Result<SnapshotPruneResult, String>>;
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SnapshotChoice {
    pub hash: Option<String>,
    pub previous: bool,
}

pub fn is_snapshot_record(value: &Value) -> bool {
    let Some(record) = value.as_object() else {
        return false;
    };
    record.get("id").is_some_and(Value::is_string)
        && record.get("path").is_some_and(Value::is_string)
        && record.get("hash").is_some_and(Value::is_string)
        && record.get("size").is_some_and(Value::is_number)
        && record.get("at").is_some_and(Value::is_string)
        && record.get("tool").is_some_and(Value::is_string)
        && matches!(record.get("mode"), Some(Value::String(mode)) if mode == "overwrite" || mode == "edit")
}

pub fn is_snapshot_agent_mark(value: &Value) -> bool {
    let Some(mark) = value.as_object() else {
        return false;
    };
    mark.get("agentId").is_some_and(Value::is_string)
        && mark.get("sessionId").is_some_and(Value::is_string)
        && mark.get("at").is_some_and(Value::is_string)
        && mark.get("cwd").is_some_and(Value::is_string)
}

pub fn resolve_snapshot_target(
    display: Option<&ToolInputDisplay>,
    args: &Value,
) -> Option<SnapshotTarget> {
    let display = display?;
    let ToolInputDisplay::FileIo { operation, path, .. } = display else {
        return None;
    };
    if operation == "edit" {
        return Some(SnapshotTarget {
            path: path.clone(),
            tool: operation.clone(),
            mode: SnapshotWriteMode::Edit,
        });
    }
    if operation != "write" {
        return None;
    }
    if args.get("mode").and_then(Value::as_str) == Some("append") {
        return None;
    }
    Some(SnapshotTarget {
        path: path.clone(),
        tool: operation.clone(),
        mode: SnapshotWriteMode::Overwrite,
    })
}

pub fn pick_snapshot<'a>(
    records: &'a [SnapshotRecord],
    choice: &SnapshotChoice,
) -> Option<&'a SnapshotRecord> {
    if let Some(wanted) = choice.hash.as_deref() {
        if let Some(exact) = records
            .iter()
            .find(|record| record.hash == wanted || record.id == wanted)
        {
            return Some(exact);
        }
        let matches: Vec<&SnapshotRecord> = records
            .iter()
            .filter(|record| record.hash.starts_with(wanted) || record.id.starts_with(wanted))
            .collect();
        return if matches.len() == 1 {
            matches.into_iter().next()
        } else {
            None
        };
    }
    if choice.previous {
        return records.iter().find(|record| !record.hash.is_empty());
    }
    records
        .iter()
        .find(|record| !record.hash.is_empty())
        .or_else(|| records.first())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_input_display::ToolInputDisplay;

    fn file_io(operation: &str, path: &str) -> ToolInputDisplay {
        ToolInputDisplay::FileIo {
            operation: operation.to_string(),
            path: path.to_string(),
            detail: None,
            content: None,
            before: None,
            after: None,
        }
    }

    fn record(hash: &str) -> SnapshotRecord {
        SnapshotRecord {
            version: SNAPSHOT_RECORD_VERSION,
            id: format!("id-{hash}"),
            path: "src/lib.rs".into(),
            hash: hash.into(),
            size: 10,
            mtime_ms: None,
            at: "2026-01-01T00:00:00.000Z".into(),
            tool: "write".into(),
            mode: SnapshotWriteMode::Overwrite,
            session_id: None,
            agent_id: None,
            previous_id: None,
            skipped: None,
            after_hash: None,
            after_size: None,
        }
    }

    #[test]
    fn target_resolution_matches_v2() {
        assert_eq!(
            resolve_snapshot_target(Some(&file_io("edit", "/w/a.rs")), &serde_json::json!({})),
            Some(SnapshotTarget {
                path: "/w/a.rs".into(),
                tool: "edit".into(),
                mode: SnapshotWriteMode::Edit,
            })
        );
        assert_eq!(
            resolve_snapshot_target(Some(&file_io("write", "/w/a.rs")), &serde_json::json!({})),
            Some(SnapshotTarget {
                path: "/w/a.rs".into(),
                tool: "write".into(),
                mode: SnapshotWriteMode::Overwrite,
            })
        );
        assert!(resolve_snapshot_target(
            Some(&file_io("write", "/w/a.rs")),
            &serde_json::json!({ "mode": "append" })
        )
        .is_none());
        assert!(resolve_snapshot_target(Some(&file_io("read", "/w/a.rs")), &serde_json::json!({}))
            .is_none());
        assert!(resolve_snapshot_target(None, &serde_json::json!({})).is_none());
        assert!(resolve_snapshot_target(
            Some(&ToolInputDisplay::Command {
                command: "ls".into(),
                cwd: None,
                description: None,
                language: None,
            }),
            &serde_json::json!({})
        )
        .is_none());
    }

    #[test]
    fn picking_matches_v2() {
        let records = vec![
            record(""),
            record("abcdef123456"),
            record("abcdef987654"),
        ];
        assert_eq!(
            pick_snapshot(&records, &SnapshotChoice::default()).map(|r| r.hash.as_str()),
            Some("abcdef123456"),
            "the first record with content wins"
        );
        assert_eq!(
            pick_snapshot(
                &records,
                &SnapshotChoice {
                    hash: Some("abcdef987654".into()),
                    previous: false,
                }
            )
            .map(|r| r.hash.as_str()),
            Some("abcdef987654"),
            "exact hash"
        );
        assert_eq!(
            pick_snapshot(
                &records,
                &SnapshotChoice {
                    hash: Some("id-abcdef123456".into()),
                    previous: false,
                }
            )
            .map(|r| r.hash.as_str()),
            Some("abcdef123456"),
            "exact id"
        );
        assert_eq!(
            pick_snapshot(
                &records,
                &SnapshotChoice {
                    hash: Some("abcdef9".into()),
                    previous: false,
                }
            )
            .map(|r| r.hash.as_str()),
            Some("abcdef987654"),
            "a unique prefix resolves"
        );
        assert!(
            pick_snapshot(
                &records,
                &SnapshotChoice {
                    hash: Some("abcdef".into()),
                    previous: false,
                }
            )
            .is_none(),
            "an ambiguous prefix does not"
        );
        assert_eq!(
            pick_snapshot(
                &records,
                &SnapshotChoice {
                    hash: None,
                    previous: true,
                }
            )
            .map(|r| r.hash.as_str()),
            Some("abcdef123456"),
            "previous takes the first non-empty hash"
        );
        let only_empty = vec![record("")];
        assert_eq!(
            pick_snapshot(&only_empty, &SnapshotChoice::default()).map(|r| r.hash.as_str()),
            Some(""),
            "a skipped-only list still returns its record"
        );
    }

    #[test]
    fn prune_constants_match_v2() {
        assert_eq!(SNAPSHOT_PRUNE_MAX_PER_PATH, 100);
        assert_eq!(SNAPSHOT_PRUNE_MAX_BYTES_PER_PATH, 64 * 1024 * 1024);
        assert_eq!(SNAPSHOT_PRUNE_MAX_AGE_MS, 30 * 24 * 60 * 60 * 1000);
        assert_eq!(SNAPSHOT_PRUNE_MIN_OBJECT_AGE_MS, 60 * 1000);
        assert_eq!(SNAPSHOT_PRUNE_INTERVAL_MS, 60 * 60 * 1000);
    }
}
