//! Wire protocol migrations (v2 `wire/migration/*`).
//!
//! v2's journal carries a leading `metadata` record; this engine stamps every
//! row with the protocol version that wrote it. The migration discipline is the
//! same either way: a reader that understands only the current record shape
//! must upgrade older rows step by step, and a version with no step must fail
//! loudly (`wire.migration_missing`) instead of being read as if it were
//! current.
//!
//! The record split follows the storage: v2 puts `type` beside the payload,
//! this engine keeps `type` in the `event_type` column and the rest in the
//! `payload` JSON column, so every migration here takes `(event_type, payload)`
//! and may rewrite either half.

use serde_json::{Map, Value};

use super::{WIRE_PROTOCOL_VERSION, compare_wire_versions};

/// v2 `WireErrors.codes.WIRE_MIGRATION_MISSING` (`wire/errors.ts:8`).
pub const WIRE_MIGRATION_MISSING: &str = "wire.migration_missing";

/// v2 `WireErrors.codes.WIRE_CYCLE` (`wire/errors.ts:9`).
pub const WIRE_CYCLE: &str = "wire.cycle";

/// A log version the reader cannot bring forward.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WireMigrationError {
    /// No migration starts at the version the log was written with, so the
    /// reader cannot reach the current shape (v2 `resolveWireMigrations`).
    /// Reading anyway would reinterpret old fields as current ones.
    #[error("Missing wire migration for version {version}")]
    Missing { version: String },
    /// The chain loops: a step's target is a version already visited (v2
    /// `WIRE_CYCLE`). A malformed chain must terminate, not spin.
    #[error("Wire migration chain cycles at version {version}")]
    Cycle { version: String },
}

impl WireMigrationError {
    /// The v2 error code the failure maps to.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Missing { .. } => WIRE_MIGRATION_MISSING,
            Self::Cycle { .. } => WIRE_CYCLE,
        }
    }
}

/// One step of the chain (`sourceVersion` → `targetVersion`).
#[derive(Debug)]
pub struct WireMigration {
    pub source_version: &'static str,
    pub target_version: &'static str,
    migrate: fn(&mut String, &mut Value),
}

impl WireMigration {
    /// Apply this step to one record in place.
    pub fn migrate_record(&self, event_type: &mut String, payload: &mut Value) {
        (self.migrate)(event_type, payload);
    }
}

/// The full chain, oldest step first (v2 `MIGRATIONS`).
pub const MIGRATIONS: [WireMigration; 5] = [
    WireMigration {
        source_version: "1.0",
        target_version: "1.1",
        migrate: migrate_v1_0_to_v1_1,
    },
    WireMigration {
        source_version: "1.1",
        target_version: "1.2",
        migrate: migrate_v1_1_to_v1_2,
    },
    WireMigration {
        source_version: "1.2",
        target_version: "1.3",
        migrate: migrate_v1_2_to_v1_3,
    },
    WireMigration {
        source_version: "1.3",
        target_version: "1.4",
        migrate: migrate_v1_3_to_v1_4,
    },
    WireMigration {
        source_version: "1.4",
        target_version: "1.5",
        migrate: migrate_v1_4_to_v1_5,
    },
];

/// The steps needed to lift `read_version` to the current protocol version.
///
/// A log written by a newer engine needs none (its reader gate has already
/// refused it), a log at the current version needs none, and any gap in the
/// chain is `wire.migration_missing` rather than a silent partial upgrade.
pub fn resolve_wire_migrations(
    read_version: &str,
) -> Result<Vec<&'static WireMigration>, WireMigrationError> {
    resolve_chain(&MIGRATIONS, read_version)
}

/// [`resolve_wire_migrations`] against an explicit chain, so the malformed
/// chain cases (missing step, duplicate step, cycle) can be exercised without
/// mutating the compiled-in table.
fn resolve_chain<'a>(
    chain: &'a [WireMigration],
    read_version: &str,
) -> Result<Vec<&'a WireMigration>, WireMigrationError> {
    if compare_wire_versions(read_version, WIRE_PROTOCOL_VERSION) >= 0 {
        return Ok(Vec::new());
    }

    let mut migrations = Vec::new();
    let mut version = read_version.to_string();
    let mut visited = vec![version.clone()];
    while compare_wire_versions(&version, WIRE_PROTOCOL_VERSION) < 0 {
        // v2 `findMigration` takes the first step whose source matches
        // (`wire/migration/migration.ts:104-108`).
        let Some(migration) = chain
            .iter()
            .find(|migration| migration.source_version == version)
        else {
            return Err(WireMigrationError::Missing { version });
        };
        let target = migration.target_version.to_string();
        if visited.contains(&target) {
            return Err(WireMigrationError::Cycle { version });
        }
        visited.push(target.clone());
        migrations.push(migration);
        version = target;
    }
    Ok(migrations)
}

/// Bring one record to the current shape.
///
/// `read_version` is the row's own stamp. `None` means the row predates the
/// version column; the table has only ever had one shape, so a NULL is this
/// engine's current version, not a very old record.
pub fn migrate_wire_record(
    event_type: &mut String,
    payload: &mut Value,
    read_version: Option<&str>,
) -> Result<(), WireMigrationError> {
    let Some(read_version) = read_version else {
        return Ok(());
    };
    let migrations = resolve_wire_migrations(read_version)?;
    for migration in migrations {
        migration.migrate_record(event_type, payload);
    }
    Ok(())
}

/// v1.0 → v1.1: flatten `context.append_message` tool calls from
/// `{type, function: {name, arguments}}` to `{type, name, arguments}`
/// (v2 `migration/v1.1.ts`).
fn migrate_v1_0_to_v1_1(event_type: &mut String, payload: &mut Value) {
    if event_type != "context.append_message" {
        return;
    }
    let Some(message) = payload.get_mut("message").and_then(Value::as_object_mut) else {
        return;
    };
    let Some(tool_calls) = message.get_mut("toolCalls").and_then(Value::as_array_mut) else {
        return;
    };
    for call in tool_calls {
        let Some(call) = call.as_object_mut() else {
            continue;
        };
        let Some(function) = call.remove("function").and_then(|f| f.as_object().cloned()) else {
            continue;
        };
        if let Some(name) = function.get("name") {
            call.insert("name".to_string(), name.clone());
        }
        if let Some(arguments) = function.get("arguments") {
            call.insert("arguments".to_string(), arguments.clone());
        }
    }
}

/// Legacy approval action → the pattern the rule would have stored
/// (v2 `v1.2.ts` `LEGACY_SESSION_APPROVAL_ACTION_TO_PATTERN`).
const LEGACY_SESSION_APPROVAL_ACTION_TO_PATTERN: [(&str, &str); 5] = [
    ("run command", "Bash"),
    ("stop background task", "TaskStop"),
    ("edit file", "Write"),
    ("edit file outside of working directory", "Write"),
    ("write file", "Write"),
];

/// Actions whose approval was never convertible into a session rule
/// (v2 `LEGACY_SESSION_APPROVAL_UNRESTORABLE_ACTIONS`).
const LEGACY_SESSION_APPROVAL_UNRESTORABLE_ACTIONS: [&str; 2] =
    ["run command in plan mode", "run background command"];

/// v1.1 → v1.2: give an old session-wide approval its missing
/// `sessionApprovalRule`, leaving unrestorable actions without one
/// (v2 `migration/v1.2.ts`).
fn migrate_v1_1_to_v1_2(event_type: &mut String, payload: &mut Value) {
    if event_type != "permission.record_approval_result" {
        return;
    }
    let Some(record) = payload.as_object_mut() else {
        return;
    };
    let result = record.get("result").and_then(Value::as_object);
    let approved = result
        .and_then(|result| result.get("decision"))
        .and_then(Value::as_str)
        == Some("approved");
    let session_scope = result
        .and_then(|result| result.get("scope"))
        .and_then(Value::as_str)
        == Some("session");
    if !approved || !session_scope {
        return;
    }
    if record.contains_key("sessionApprovalRule") {
        return;
    }

    let action = record
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if LEGACY_SESSION_APPROVAL_UNRESTORABLE_ACTIONS.contains(&action) {
        return;
    }
    let mapped = LEGACY_SESSION_APPROVAL_ACTION_TO_PATTERN
        .iter()
        .find(|(legacy, _)| *legacy == action)
        .map(|(_, pattern)| *pattern);
    let pattern = mapped.or_else(|| record.get("toolName").and_then(Value::as_str));
    let Some(pattern) = pattern else {
        return;
    };
    record.insert(
        "sessionApprovalRule".to_string(),
        Value::String(pattern.to_string()),
    );
}

/// v1.2 → v1.3: identity. The version exists to hold the slot in the chain
/// (v2 `migration/v1.3.ts`).
fn migrate_v1_2_to_v1_3(_event_type: &mut String, _payload: &mut Value) {}

/// v1.3 → v1.4: rewrite `goal.*` records to their whitelisted field set, and
/// fold the two usage records into `goal.update` (v2 `migration/v1.4.ts`).
fn migrate_v1_3_to_v1_4(event_type: &mut String, payload: &mut Value) {
    let Some(record) = payload.as_object() else {
        return;
    };
    let kept: Vec<&str> = match event_type.as_str() {
        "goal.create" => vec!["goalId", "objective", "completionCriterion", "time"],
        "goal.update" => vec![
            "goalId",
            "status",
            "reason",
            "turnsUsed",
            "tokensUsed",
            "wallClockMs",
            "actor",
            "time",
        ],
        "goal.account_usage" => {
            *event_type = "goal.update".to_string();
            vec!["goalId", "tokensUsed", "wallClockMs", "time"]
        }
        "goal.continuation" => {
            *event_type = "goal.update".to_string();
            vec!["goalId", "turnsUsed", "time"]
        }
        "goal.clear" => vec!["goalId", "time"],
        _ => return,
    };

    let mut rewritten = Map::new();
    for key in kept {
        if let Some(value) = record.get(key) {
            rewritten.insert(key.to_string(), value.clone());
        }
    }
    *payload = Value::Object(rewritten);
}

/// v1.4 → v1.5: stamp `wallClockResumedAt = time` on the records that advance
/// an active goal interval (v2 `migration/v1.5.ts`).
fn migrate_v1_4_to_v1_5(event_type: &mut String, payload: &mut Value) {
    let Some(record) = payload.as_object_mut() else {
        return;
    };
    let advances = event_type == "goal.create"
        || (event_type == "goal.update"
            && (record.get("status").and_then(Value::as_str) == Some("active")
                || (record.get("status").is_none()
                    && record.get("wallClockMs").is_some_and(Value::is_number))));
    if !advances {
        return;
    }
    if record.contains_key("wallClockResumedAt") {
        return;
    }
    let Some(time) = record.get("time").filter(|time| time.is_number()).cloned() else {
        return;
    };
    record.insert("wallClockResumedAt".to_string(), time);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn migrate(event_type: &str, payload: Value, version: &str) -> (String, Value) {
        let mut event_type = event_type.to_string();
        let mut payload = payload;
        migrate_wire_record(&mut event_type, &mut payload, Some(version)).unwrap();
        (event_type, payload)
    }

    #[test]
    fn a_current_version_needs_no_migration() {
        assert!(
            resolve_wire_migrations(WIRE_PROTOCOL_VERSION)
                .unwrap()
                .is_empty()
        );
        assert!(resolve_wire_migrations("1.9").unwrap().is_empty());
    }

    #[test]
    fn the_chain_runs_from_the_oldest_version_in_order() {
        let sources: Vec<&str> = resolve_wire_migrations("1.0")
            .unwrap()
            .iter()
            .map(|migration| migration.source_version)
            .collect();
        assert_eq!(sources, vec!["1.0", "1.1", "1.2", "1.3", "1.4"]);
    }

    #[test]
    fn a_version_without_a_step_is_reported_as_wire_migration_missing() {
        let error = resolve_wire_migrations("0.9").unwrap_err();
        assert_eq!(error.code(), WIRE_MIGRATION_MISSING);
        assert_eq!(error.to_string(), "Missing wire migration for version 0.9");
    }

    #[test]
    fn a_missing_version_aborts_the_record_migration() {
        let mut event_type = "message.user".to_string();
        let mut payload = json!({ "content": "x" });
        let error = migrate_wire_record(&mut event_type, &mut payload, Some("0.9")).unwrap_err();
        assert_eq!(error.code(), WIRE_MIGRATION_MISSING);
    }

    #[test]
    fn a_null_version_is_read_as_the_current_shape() {
        let mut event_type = "goal.create".to_string();
        let mut payload = json!({ "goalId": "g", "objective": "o", "stray": 1 });
        migrate_wire_record(&mut event_type, &mut payload, None).unwrap();
        assert_eq!(
            payload,
            json!({ "goalId": "g", "objective": "o", "stray": 1 })
        );
    }

    #[test]
    fn v1_1_flattens_the_tool_call_function_envelope() {
        let (_, payload) = migrate(
            "context.append_message",
            json!({
                "message": {
                    "toolCalls": [
                        { "type": "function", "id": "c1", "function": { "name": "Read", "arguments": "{}" } }
                    ]
                }
            }),
            "1.0",
        );
        assert_eq!(
            payload["message"]["toolCalls"][0],
            json!({ "type": "function", "id": "c1", "name": "Read", "arguments": "{}" })
        );
    }

    #[test]
    fn v1_1_leaves_other_record_types_alone() {
        let original = json!({ "message": { "toolCalls": [{ "function": { "name": "Read" } }] } });
        let (_, payload) = migrate("message.user", original.clone(), "1.0");
        assert_eq!(payload, original);
    }

    #[test]
    fn v1_2_restores_a_session_approval_rule_from_the_legacy_action() {
        let (_, payload) = migrate(
            "permission.record_approval_result",
            json!({
                "action": "run command",
                "toolName": "Bash",
                "result": { "decision": "approved", "scope": "session" }
            }),
            "1.1",
        );
        assert_eq!(payload["sessionApprovalRule"], json!("Bash"));
    }

    #[test]
    fn v1_2_falls_back_to_the_tool_name_and_leaves_unrestorable_actions_alone() {
        let (_, payload) = migrate(
            "permission.record_approval_result",
            json!({
                "action": "a tool",
                "toolName": "Glob",
                "result": { "decision": "approved", "scope": "session" }
            }),
            "1.1",
        );
        assert_eq!(payload["sessionApprovalRule"], json!("Glob"));

        let (_, payload) = migrate(
            "permission.record_approval_result",
            json!({
                "action": "run background command",
                "toolName": "Bash",
                "result": { "decision": "approved", "scope": "session" }
            }),
            "1.1",
        );
        assert!(payload.get("sessionApprovalRule").is_none());
    }

    #[test]
    fn v1_2_skips_a_one_shot_approval() {
        let (_, payload) = migrate(
            "permission.record_approval_result",
            json!({
                "action": "run command",
                "toolName": "Bash",
                "result": { "decision": "approved" }
            }),
            "1.1",
        );
        assert!(payload.get("sessionApprovalRule").is_none());
    }

    #[test]
    fn v1_4_whitelists_goal_fields_and_folds_usage_records_into_goal_update() {
        let (_, payload) = migrate(
            "goal.create",
            json!({ "goalId": "g1", "objective": "o", "completionCriterion": "c", "time": 5, "stray": true }),
            "1.3",
        );
        assert_eq!(
            payload,
            json!({
                "goalId": "g1",
                "objective": "o",
                "completionCriterion": "c",
                "time": 5,
                "wallClockResumedAt": 5
            })
        );

        let (event_type, payload) = migrate(
            "goal.account_usage",
            json!({ "goalId": "g1", "tokensUsed": 10, "wallClockMs": 20, "time": 5, "stray": true }),
            "1.3",
        );
        assert_eq!(event_type, "goal.update");
        assert_eq!(
            payload,
            json!({
                "goalId": "g1",
                "tokensUsed": 10,
                "wallClockMs": 20,
                "time": 5,
                "wallClockResumedAt": 5
            })
        );

        let (event_type, payload) = migrate(
            "goal.continuation",
            json!({ "goalId": "g1", "turnsUsed": 2, "time": 5 }),
            "1.3",
        );
        assert_eq!(event_type, "goal.update");
        assert_eq!(
            payload,
            json!({ "goalId": "g1", "turnsUsed": 2, "time": 5 })
        );

        let (_, payload) = migrate("goal.clear", json!({ "goalId": "g1", "time": 5 }), "1.3");
        assert_eq!(payload, json!({ "goalId": "g1", "time": 5 }));
    }

    #[test]
    fn v1_4_leaves_non_goal_records_untouched() {
        let original = json!({ "content": "hi", "stray": true });
        let (_, payload) = migrate("message.user", original.clone(), "1.3");
        assert_eq!(payload, original);
    }

    #[test]
    fn v1_5_stamps_the_wall_clock_start_only_where_an_active_interval_advances() {
        let (_, payload) = migrate(
            "goal.create",
            json!({ "goalId": "g1", "objective": "o", "time": 7 }),
            "1.4",
        );
        assert_eq!(payload["wallClockResumedAt"], json!(7));

        let (_, payload) = migrate(
            "goal.update",
            json!({ "goalId": "g1", "status": "active", "time": 7 }),
            "1.4",
        );
        assert_eq!(payload["wallClockResumedAt"], json!(7));

        let (_, payload) = migrate(
            "goal.update",
            json!({ "goalId": "g1", "wallClockMs": 30, "time": 7 }),
            "1.4",
        );
        assert_eq!(payload["wallClockResumedAt"], json!(7));

        let (_, payload) = migrate(
            "goal.update",
            json!({ "goalId": "g1", "status": "paused", "time": 7 }),
            "1.4",
        );
        assert!(payload.get("wallClockResumedAt").is_none());
    }

    #[test]
    fn v1_5_keeps_an_existing_stamp_and_ignores_a_non_numeric_time() {
        let (_, payload) = migrate(
            "goal.create",
            json!({ "goalId": "g1", "time": 7, "wallClockResumedAt": 3 }),
            "1.4",
        );
        assert_eq!(payload["wallClockResumedAt"], json!(3));

        let (_, payload) = migrate("goal.create", json!({ "goalId": "g1", "time": "7" }), "1.4");
        assert!(payload.get("wallClockResumedAt").is_none());
    }

    #[test]
    fn a_cyclic_step_is_reported_and_the_chain_terminates() {
        let chain = [WireMigration {
            source_version: "1.0",
            target_version: "1.0",
            migrate: migrate_v1_0_to_v1_1,
        }];
        let error = resolve_chain(&chain, "1.0").unwrap_err();
        assert_eq!(error.code(), WIRE_CYCLE);
    }

    #[test]
    fn every_step_is_reachable_from_its_predecessor() {
        for pair in MIGRATIONS.windows(2) {
            assert_eq!(pair[0].target_version, pair[1].source_version);
        }
        assert_eq!(
            MIGRATIONS.last().unwrap().target_version,
            WIRE_PROTOCOL_VERSION
        );
    }
}
