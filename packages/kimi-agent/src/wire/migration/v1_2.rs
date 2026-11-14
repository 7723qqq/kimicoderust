use serde_json::{Map, Value};

use super::migration::{WireMigration, WireMigrationRecord};

type V11ApprovalResult = Map<String, Value>;
type V11ApprovalResultRecord = WireMigrationRecord;

const LEGACY_SESSION_APPROVAL_ACTION_TO_PATTERN: &[(&str, &str)] = &[
    ("run command", "Bash"),
    ("stop background task", "TaskStop"),
    ("edit file", "Write"),
    ("edit file outside of working directory", "Write"),
    ("write file", "Write"),
];

const LEGACY_SESSION_APPROVAL_UNRESTORABLE_ACTIONS: &[&str] =
    &["run command in plan mode", "run background command"];

pub struct MigrateV11ToV12;

pub static MIGRATE_V1_1_TO_V1_2: MigrateV11ToV12 = MigrateV11ToV12;

impl WireMigration for MigrateV11ToV12 {
    fn source_version(&self) -> &'static str {
        "1.1"
    }

    fn target_version(&self) -> &'static str {
        "1.2"
    }

    fn migrate_record(&self, mut record: WireMigrationRecord) -> WireMigrationRecord {
        if record.get("type").and_then(Value::as_str)
            != Some("permission.record_approval_result")
        {
            return record;
        }
        let approval_record: &V11ApprovalResultRecord = &record;
        let result: &V11ApprovalResult = approval_record
            .get("result")
            .and_then(Value::as_object)
            .unwrap();
        if result.get("decision").and_then(Value::as_str) != Some("approved")
            || result.get("scope").and_then(Value::as_str) != Some("session")
        {
            return record;
        }
        if record.contains_key("sessionApprovalRule") {
            return record;
        }
        let action = record.get("action").and_then(Value::as_str);
        let pattern = if LEGACY_SESSION_APPROVAL_UNRESTORABLE_ACTIONS
            .iter()
            .any(|candidate| Some(*candidate) == action)
        {
            None
        } else if let Some(pattern) = LEGACY_SESSION_APPROVAL_ACTION_TO_PATTERN
            .iter()
            .find_map(|(candidate, pattern)| (Some(*candidate) == action).then_some(*pattern))
        {
            Some(Value::String(pattern.to_string()))
        } else {
            record.get("toolName").cloned()
        };
        let Some(pattern) = pattern else {
            return record;
        };
        record.insert("sessionApprovalRule".to_string(), pattern);
        record
    }
}
