use super::snapshot::{SnapshotRecord, SnapshotSkipReason, SnapshotStore};
use super::snapshotList::{
    SNAPSHOT_LIST_TOOL_NAME, parse_snapshot_list_input, snapshot_list_input_schema,
};
use crate::rpc::types::BoxFuture;
use crate::tools::{err_result, ok_result};
use crate::turn_loop::types::{ExecutableToolResult, ToolInfo};

pub const SNAPSHOT_LIST_EMPTY_OUTPUT: &str = "No recorded versions of";
pub const SNAPSHOT_LIST_DESCRIPTION: &str = r#"List the versions this session recorded for a file before it was overwritten.

Write and Edit keep a record of the content they replace. This tool reads that record for one path and answers who replaced it, when, and in which session or subagent: each line carries the time, the tool that wrote, the size, a short content hash, and the agent that did it. Versions come newest first; index 1 is the most recent overwrite.

It never returns the recorded content. To put a version back, the user runs `kimi snapshot restore <path> --previous` (or `--hash <sha|id>` for a specific one).
"#;

const HASH_PREFIX: usize = 12;

fn owner_of(record: &SnapshotRecord) -> String {
    let mut parts = Vec::new();
    if let Some(session) = record.session_id.as_deref() {
        parts.push(format!("session={session}"));
    }
    if let Some(agent) = record.agent_id.as_deref() {
        parts.push(format!("agent={agent}"));
    }
    parts.join(" ")
}

fn content_of(record: &SnapshotRecord) -> String {
    if record.hash.is_empty() {
        return match record.skipped {
            None => "no content".to_string(),
            Some(reason) => format!("skipped:{}", skip_reason_str(reason)),
        };
    }
    record.hash.chars().take(HASH_PREFIX).collect()
}

fn skip_reason_str(reason: SnapshotSkipReason) -> &'static str {
    match reason {
        SnapshotSkipReason::MaxSize => "max-size",
        SnapshotSkipReason::Unreadable => "unreadable",
    }
}

pub fn format_snapshot_version(record: &SnapshotRecord, index: usize) -> String {
    let owner = owner_of(record);
    let head = format!(
        "{}. {}  {}  {} B  {}",
        index + 1,
        record.at,
        record.tool,
        record.size,
        content_of(record)
    );
    if owner.is_empty() {
        head
    } else {
        format!("{head}  {owner}")
    }
}

pub fn render_snapshot_list(path: &str, records: &[SnapshotRecord]) -> ExecutableToolResult {
    if records.is_empty() {
        return err_result(format!("{SNAPSHOT_LIST_EMPTY_OUTPUT} {path}."));
    }
    let mut lines = vec![format!(
        "{} recorded version(s) of {}, newest first",
        records.len(),
        path
    )];
    for (index, record) in records.iter().enumerate() {
        lines.push(format_snapshot_version(record, index));
    }
    ok_result(lines.join("\n"))
}

pub fn snapshot_list_tool_def() -> ToolInfo {
    ToolInfo {
        name: SNAPSHOT_LIST_TOOL_NAME.into(),
        description: SNAPSHOT_LIST_DESCRIPTION.into(),
        input_schema: snapshot_list_input_schema(),
    }
}

pub async fn execute_snapshot_list(
    store: &dyn SnapshotStore,
    args: &serde_json::Value,
) -> ExecutableToolResult {
    let input = match parse_snapshot_list_input(args) {
        Ok(input) => input,
        Err(error) => return err_result(error),
    };
    match store.list(input.path.clone()).await {
        Ok(records) => render_snapshot_list(&input.path, &records),
        Err(error) => err_result(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::snapshot::snapshot::{
        SnapshotWriteMode, SNAPSHOT_RECORD_VERSION,
    };
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct ProbeStore {
        records: Mutex<HashMap<String, Vec<SnapshotRecord>>>,
        listed: Mutex<Vec<String>>,
    }

    impl SnapshotStore for ProbeStore {
        fn record<'a>(
            &'a self,
            _: super::super::snapshot::SnapshotRecordInput,
        ) -> BoxFuture<'a, Result<SnapshotRecord, String>> {
            Box::pin(async { Err("not used".into()) })
        }

        fn complete<'a>(
            &'a self,
            _: String,
            _: String,
            _: String,
            _: u64,
        ) -> BoxFuture<'a, Result<bool, String>> {
            Box::pin(async { Err("not used".into()) })
        }

        fn remove<'a>(&'a self, _: String, _: String) -> BoxFuture<'a, Result<bool, String>> {
            Box::pin(async { Err("not used".into()) })
        }

        fn list<'a>(&'a self, path: String) -> BoxFuture<'a, Result<Vec<SnapshotRecord>, String>> {
            self.listed.lock().unwrap().push(path.clone());
            let records = self.records.lock().unwrap().get(&path).cloned().unwrap_or_default();
            Box::pin(async move { Ok(records) })
        }

        fn content<'a>(&'a self, _: String) -> BoxFuture<'a, Result<Option<Vec<u8>>, String>> {
            Box::pin(async { Err("not used".into()) })
        }

        fn mark_agent<'a>(
            &'a self,
            _: super::super::snapshot::SnapshotAgentMarkInput,
        ) -> BoxFuture<'a, Result<(), String>> {
            Box::pin(async { Err("not used".into()) })
        }

        fn agents<'a>(
            &'a self,
            _: String,
        ) -> BoxFuture<'a, Result<Vec<super::super::snapshot::SnapshotAgentMark>, String>> {
            Box::pin(async { Err("not used".into()) })
        }

        fn prune<'a>(
            &'a self,
            _: super::super::snapshot::SnapshotPruneOptions,
        ) -> BoxFuture<'a, Result<super::super::snapshot::SnapshotPruneResult, String>> {
            Box::pin(async { Err("not used".into()) })
        }
    }

    fn record(hash: &str) -> SnapshotRecord {
        SnapshotRecord {
            version: SNAPSHOT_RECORD_VERSION,
            id: format!("id-{hash}"),
            path: "src/lib.rs".into(),
            hash: hash.into(),
            size: 128,
            mtime_ms: None,
            at: "2026-01-01T00:00:00.000Z".into(),
            tool: "write".into(),
            mode: SnapshotWriteMode::Overwrite,
            session_id: Some("sess-1".into()),
            agent_id: Some("main".into()),
            previous_id: None,
            skipped: None,
            after_hash: None,
            after_size: None,
        }
    }

    #[test]
    fn version_lines_match_v2_formatting() {
        let skipped = SnapshotRecord {
            hash: String::new(),
            skipped: Some(SnapshotSkipReason::MaxSize),
            ..record("abcdef1234567890")
        };
        let unowned = SnapshotRecord {
            session_id: None,
            agent_id: None,
            ..record("fedcba9876543210")
        };
        assert_eq!(
            format_snapshot_version(&record("abcdef1234567890"), 0),
            "1. 2026-01-01T00:00:00.000Z  write  128 B  abcdef123456  session=sess-1 agent=main"
        );
        assert_eq!(
            format_snapshot_version(&skipped, 1),
            "2. 2026-01-01T00:00:00.000Z  write  128 B  skipped:max-size  session=sess-1 agent=main"
        );
        assert_eq!(
            format_snapshot_version(&unowned, 2),
            "3. 2026-01-01T00:00:00.000Z  write  128 B  fedcba987654"
        );
    }

    #[tokio::test]
    async fn empty_list_is_an_error_with_the_v2_prefix() {
        let store = ProbeStore::default();
        let result = execute_snapshot_list(&store, &serde_json::json!({ "path": "src/lib.rs" })).await;
        assert!(result.is_error);
        assert_eq!(result.content, "No recorded versions of src/lib.rs.");
        assert_eq!(store.listed.lock().unwrap().as_slice(), ["src/lib.rs"]);
    }

    #[tokio::test]
    async fn a_list_renders_header_then_versions_newest_first() {
        let store = ProbeStore {
            records: Mutex::new(HashMap::from([(
                "src/lib.rs".to_string(),
                vec![record("aaaa"), record("bbbb")],
            )])),
            listed: Mutex::new(Vec::new()),
        };
        let result = execute_snapshot_list(&store, &serde_json::json!({ "path": "src/lib.rs" })).await;
        assert!(!result.is_error);
        assert_eq!(
            result.content,
            "2 recorded version(s) of src/lib.rs, newest first\n\
             1. 2026-01-01T00:00:00.000Z  write  128 B  aaaa  session=sess-1 agent=main\n\
             2. 2026-01-01T00:00:00.000Z  write  128 B  bbbb  session=sess-1 agent=main"
        );
    }

    #[tokio::test]
    async fn invalid_arguments_do_not_reach_the_store() {
        let store = ProbeStore::default();
        let result = execute_snapshot_list(&store, &serde_json::json!({})).await;
        assert!(result.is_error);
        assert!(store.listed.lock().unwrap().is_empty());
    }

    #[test]
    fn the_tool_def_is_named_like_v2() {
        assert_eq!(snapshot_list_tool_def().name, SNAPSHOT_LIST_TOOL_NAME);
    }
}
