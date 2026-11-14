use serde_json::{Map, Value};

use super::migration::{WireMigration, WireMigrationRecord};

type TimedWireMigrationRecord = WireMigrationRecord;
type V13GoalCreateRecord = TimedWireMigrationRecord;
type V13GoalUpdateRecord = TimedWireMigrationRecord;
type V13GoalAccountUsageRecord = TimedWireMigrationRecord;
type V13GoalContinuationRecord = TimedWireMigrationRecord;
type V13GoalClearRecord = TimedWireMigrationRecord;

pub struct MigrateV13ToV14;

pub static MIGRATE_V1_3_TO_V1_4: MigrateV13ToV14 = MigrateV13ToV14;

impl WireMigration for MigrateV13ToV14 {
    fn source_version(&self) -> &'static str {
        "1.3"
    }

    fn target_version(&self) -> &'static str {
        "1.4"
    }

    fn migrate_record(&self, record: WireMigrationRecord) -> WireMigrationRecord {
        match record.get("type").and_then(Value::as_str) {
            Some("goal.create") => migrate_goal_create(record),
            Some("goal.update") => migrate_goal_update(record),
            Some("goal.account_usage") => migrate_goal_account_usage(record),
            Some("goal.continuation") => migrate_goal_continuation(record),
            Some("goal.clear") => migrate_goal_clear(record),
            _ => record,
        }
    }
}

fn migrate_goal_create(record: V13GoalCreateRecord) -> WireMigrationRecord {
    let mut output = required_record("goal.create", &record, &["goalId", "objective"]);
    copy_optional(
        &record,
        &mut output,
        &["completionCriterion", "time"],
    );
    output
}

fn migrate_goal_update(record: V13GoalUpdateRecord) -> WireMigrationRecord {
    let mut output = required_record("goal.update", &record, &["goalId", "status"]);
    copy_optional(
        &record,
        &mut output,
        &[
            "reason",
            "turnsUsed",
            "tokensUsed",
            "wallClockMs",
            "actor",
            "time",
        ],
    );
    output
}

fn migrate_goal_account_usage(record: V13GoalAccountUsageRecord) -> WireMigrationRecord {
    let mut output = required_record("goal.update", &record, &["goalId"]);
    copy_optional(
        &record,
        &mut output,
        &["tokensUsed", "wallClockMs", "time"],
    );
    output
}

fn migrate_goal_continuation(record: V13GoalContinuationRecord) -> WireMigrationRecord {
    let mut output = required_record("goal.update", &record, &["goalId"]);
    copy_optional(&record, &mut output, &["turnsUsed", "time"]);
    output
}

fn migrate_goal_clear(record: V13GoalClearRecord) -> WireMigrationRecord {
    let mut output = required_record("goal.clear", &record, &["goalId"]);
    copy_optional(&record, &mut output, &["time"]);
    output
}

fn required_record(
    record_type: &str,
    record: &WireMigrationRecord,
    required: &[&str],
) -> WireMigrationRecord {
    let mut output = Map::new();
    output.insert("type".to_string(), record_type.into());
    for key in required {
        if let Some(value) = record.get(*key) {
            output.insert((*key).to_string(), value.clone());
        }
    }
    output
}

fn copy_optional(
    record: &WireMigrationRecord,
    output: &mut WireMigrationRecord,
    keys: &[&str],
) {
    for key in keys {
        if let Some(value) = record.get(*key) {
            output.insert((*key).to_string(), value.clone());
        }
    }
}
