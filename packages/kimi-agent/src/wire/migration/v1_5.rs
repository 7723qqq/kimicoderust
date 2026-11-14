use serde_json::Value;

use super::migration::{WireMigration, WireMigrationRecord};

pub struct MigrateV14ToV15;

pub static MIGRATE_V1_4_TO_V1_5: MigrateV14ToV15 = MigrateV14ToV15;

impl WireMigration for MigrateV14ToV15 {
    fn source_version(&self) -> &'static str {
        "1.4"
    }

    fn target_version(&self) -> &'static str {
        "1.5"
    }

    fn migrate_record(&self, mut record: WireMigrationRecord) -> WireMigrationRecord {
        if !advances_active_interval(&record)
            || record.contains_key("wallClockResumedAt")
            || !record.get("time").is_some_and(Value::is_number)
        {
            return record;
        }
        record.insert(
            "wallClockResumedAt".to_string(),
            record.get("time").unwrap().clone(),
        );
        record
    }
}

fn advances_active_interval(record: &WireMigrationRecord) -> bool {
    record.get("type").and_then(Value::as_str) == Some("goal.create")
        || (record.get("type").and_then(Value::as_str) == Some("goal.update")
            && (record.get("status").and_then(Value::as_str) == Some("active")
                || (!record.contains_key("status")
                    && record
                        .get("wallClockMs")
                        .is_some_and(Value::is_number))))
}
