//! Durable per-session token usage, ported from v2's usage agent model
//! (`agent-core-v2/src/session/usage/usageAgentModel.ts`).
//!
//! v2 keeps `byModel` in a durable agent model fed by `UsageRecord` events,
//! derives `total` as the sum of every model bucket, and tracks the current
//! turn separately. The Rust engine has no agent-state layer, so the buckets
//! live in the session store's state domain (`domain = "usage"`, `key = the
//! session id`), which is the same seam `agent_config` and `metadata` use.
//!
//! The wire shape is v2's `UsageStatus` in camelCase (`byModel`, `total`,
//! `currentTurn`), with each bucket carrying the four `TokenUsage` counters
//! (`inputOther`, `output`, `inputCacheRead`, `inputCacheCreation`).

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::rpc::types::TokenUsage;
use crate::session::sqlite_store::SqliteSessionStore;

/// The state domain the buckets live in (`agent_config` / `metadata` precedent).
const USAGE_DOMAIN: &str = "usage";

/// A persisted usage document that parses as JSON but not as this module's
/// shape. Reported rather than defaulted: a default written back over the row
/// would reset the session's cumulative counters.
#[derive(Debug)]
struct UsageShapeError {
    session_id: String,
    source: serde_json::Error,
}

impl fmt::Display for UsageShapeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the usage state of session {} does not have the expected shape: {}",
            self.session_id, self.source
        )
    }
}

impl std::error::Error for UsageShapeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// One model's (or the session's) four token counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageCounters {
    #[serde(default)]
    pub input_other: i64,
    #[serde(default)]
    pub output: i64,
    #[serde(default)]
    pub input_cache_read: i64,
    #[serde(default)]
    pub input_cache_creation: i64,
}

impl UsageCounters {
    fn from_token_usage(usage: &TokenUsage) -> Self {
        Self {
            input_other: i64::from(usage.input_tokens),
            output: i64::from(usage.output_tokens),
            input_cache_read: i64::from(usage.input_cache_read),
            input_cache_creation: i64::from(usage.input_cache_creation),
        }
    }

    fn add(&mut self, other: Self) {
        self.input_other += other.input_other;
        self.output += other.output;
        self.input_cache_read += other.input_cache_read;
        self.input_cache_creation += other.input_cache_creation;
    }

    fn is_zero(self) -> bool {
        self.input_other == 0
            && self.output == 0
            && self.input_cache_read == 0
            && self.input_cache_creation == 0
    }
}

/// The persisted half: model buckets plus the most recent turn's counters.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersistedUsage {
    #[serde(default)]
    by_model: BTreeMap<String, UsageCounters>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    current_turn: Option<UsageCounters>,
    /// The slice of `by_model` spent on compaction requests. A separate bucket
    /// keeps `byModel` billable in full while per-turn statistics can subtract
    /// the compaction work instead of diluting it into chat turns.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    compaction_by_model: BTreeMap<String, UsageCounters>,
}

/// v2 `UsageStatus`: the model buckets, their sum, and the current turn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageStatus {
    pub by_model: BTreeMap<String, UsageCounters>,
    pub total: UsageCounters,
    pub current_turn: Option<UsageCounters>,
    /// The compaction share of [`Self::by_model`] / [`Self::total`].
    pub compaction_by_model: BTreeMap<String, UsageCounters>,
    pub compaction_total: UsageCounters,
}

impl UsageStatus {
    fn from_persisted(persisted: PersistedUsage) -> Self {
        let mut total = UsageCounters::default();
        for counters in persisted.by_model.values() {
            total.add(*counters);
        }
        let mut compaction_total = UsageCounters::default();
        for counters in persisted.compaction_by_model.values() {
            compaction_total.add(*counters);
        }
        Self {
            by_model: persisted.by_model,
            total,
            current_turn: persisted.current_turn,
            compaction_by_model: persisted.compaction_by_model,
            compaction_total,
        }
    }

    /// Fold one finished turn into the model's bucket (v2
    /// `UsageAgentModel.record`): the bucket and the total grow, and the turn
    /// becomes `currentTurn`. v2 records every turn, including a zero-usage
    /// one — the bucket entry and `currentTurn` must still be created, so a
    /// caller can tell "a turn ran" from "no turn ran".
    pub fn record(&mut self, model: &str, usage: &TokenUsage) {
        let counters = UsageCounters::from_token_usage(usage);
        self.by_model
            .entry(model.to_string())
            .or_default()
            .add(counters);
        self.total.add(counters);
        self.current_turn = Some(counters);
    }

    /// Fold a compaction request into the model's bucket while tagging it as
    /// compaction work. It counts toward `by_model` / `total` (v2 records every
    /// request there), but not toward `currentTurn` — a compaction is not a
    /// chat turn.
    pub fn record_compaction(&mut self, model: &str, usage: &TokenUsage) {
        let counters = UsageCounters::from_token_usage(usage);
        self.by_model
            .entry(model.to_string())
            .or_default()
            .add(counters);
        self.total.add(counters);
        self.compaction_by_model
            .entry(model.to_string())
            .or_default()
            .add(counters);
        self.compaction_total.add(counters);
    }

    /// Rebuild a status from the wire snapshot (`byModel` + `currentTurn`;
    /// `total` is re-derived so a stale serialized total cannot disagree with
    /// the buckets).
    pub fn from_json(value: &Value) -> Self {
        let by_model = value
            .get("byModel")
            .cloned()
            .and_then(|by_model| {
                serde_json::from_value::<BTreeMap<String, UsageCounters>>(by_model).ok()
            })
            .unwrap_or_default();
        let current_turn = value
            .get("currentTurn")
            .cloned()
            .and_then(|current| serde_json::from_value::<UsageCounters>(current).ok());
        let compaction_by_model = value
            .get("compactionByModel")
            .cloned()
            .and_then(|by_model| {
                serde_json::from_value::<BTreeMap<String, UsageCounters>>(by_model).ok()
            })
            .unwrap_or_default();
        let mut total = UsageCounters::default();
        for counters in by_model.values() {
            total.add(*counters);
        }
        let mut compaction_total = UsageCounters::default();
        for counters in compaction_by_model.values() {
            compaction_total.add(*counters);
        }
        Self {
            by_model,
            total,
            current_turn,
            compaction_by_model,
            compaction_total,
        }
    }

    /// Parse the JSON snapshot the host persists across handle builds.
    pub fn from_json_str(raw: &str) -> Self {
        serde_json::from_str::<Value>(raw)
            .map(|value| Self::from_json(&value))
            .unwrap_or_default()
    }

    /// The `AgentStatusUpdated.usage` payload, or `None` when nothing has been
    /// recorded (v2's status is empty until the first `UsageRecord`).
    pub fn to_json(&self) -> Option<Value> {
        if self.by_model.is_empty() && self.current_turn.is_none() {
            return None;
        }
        let mut object = serde_json::Map::new();
        if !self.by_model.is_empty() {
            object.insert("byModel".into(), serde_json::to_value(&self.by_model).ok()?);
        }
        if !self.total.is_zero() || !self.by_model.is_empty() {
            object.insert("total".into(), serde_json::to_value(self.total).ok()?);
        }
        if let Some(current_turn) = self.current_turn {
            object.insert(
                "currentTurn".into(),
                serde_json::to_value(current_turn).ok()?,
            );
        }
        if !self.compaction_by_model.is_empty() {
            object.insert(
                "compactionByModel".into(),
                serde_json::to_value(&self.compaction_by_model).ok()?,
            );
            object.insert(
                "compactionTotal".into(),
                serde_json::to_value(self.compaction_total).ok()?,
            );
        }
        Some(Value::Object(object))
    }
}

/// Read the persisted buckets. Only an absent row is empty (`None`); a failed
/// read is returned to the caller, because a default here would be written back
/// over the row and reset the session's cumulative counters.
fn load(store: &SqliteSessionStore, session_id: &str) -> Result<PersistedUsage, rusqlite::Error> {
    let Some(value) = store.get_state(USAGE_DOMAIN, session_id)? else {
        return Ok(PersistedUsage::default());
    };
    serde_json::from_value(value).map_err(|source| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(UsageShapeError {
                session_id: session_id.to_string(),
                source,
            }),
        )
    })
}

/// Fold one finished turn's usage into the session's model bucket and refresh
/// `currentTurn`. v2 records zero-usage turns too, so the bucket entry and
/// `currentTurn` are created even when every counter is zero — "a turn ran"
/// stays observable.
pub fn record_turn(
    store: &SqliteSessionStore,
    session_id: &str,
    model: &str,
    usage: &TokenUsage,
) -> Result<(), rusqlite::Error> {
    let counters = UsageCounters::from_token_usage(usage);
    let mut persisted = load(store, session_id)?;
    persisted
        .by_model
        .entry(model.to_string())
        .or_default()
        .add(counters);
    persisted.current_turn = Some(counters);
    let value = serde_json::to_value(&persisted)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    store.put_state(USAGE_DOMAIN, session_id, &value)
}

/// Fold one compaction request's usage into the session's model bucket. It
/// grows `byModel` / `total` (the billable totals) and the compaction slice,
/// but leaves `currentTurn` alone — a compaction must not look like a chat
/// turn to per-turn statistics.
pub fn record_compaction(
    store: &SqliteSessionStore,
    session_id: &str,
    model: &str,
    usage: &TokenUsage,
) -> Result<(), rusqlite::Error> {
    let counters = UsageCounters::from_token_usage(usage);
    let mut persisted = load(store, session_id)?;
    persisted
        .by_model
        .entry(model.to_string())
        .or_default()
        .add(counters);
    persisted
        .compaction_by_model
        .entry(model.to_string())
        .or_default()
        .add(counters);
    let value = serde_json::to_value(&persisted)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    store.put_state(USAGE_DOMAIN, session_id, &value)
}

/// Read the session's usage status. Sessions written before this module existed
/// have per-turn usage in the `turns` table but no buckets; reconstruct them
/// against `fallback_model` (the session's configured model) so an upgraded
/// session does not report zero forever. The buckets win once they exist.
pub fn status(store: &SqliteSessionStore, session_id: &str, fallback_model: &str) -> UsageStatus {
    // Read-only path: a failed read must not rewrite anything, so falling back
    // to the empty snapshot here loses nothing (unlike `record_turn`).
    let persisted = load(store, session_id).unwrap_or_default();
    if !persisted.by_model.is_empty() || persisted.current_turn.is_some() {
        return UsageStatus::from_persisted(persisted);
    }

    let mut total = UsageCounters::default();
    let mut current_turn = None;
    if let Ok(turns) = store.list_turns(session_id) {
        for turn in &turns {
            if let Some(usage) = &turn.usage {
                let counters = UsageCounters::from_token_usage(usage);
                total.add(counters);
                current_turn = Some(counters);
            }
        }
    }
    let mut by_model = BTreeMap::new();
    if !total.is_zero() {
        by_model.insert(fallback_model.to_string(), total);
    }
    UsageStatus {
        by_model,
        total,
        current_turn,
        compaction_by_model: BTreeMap::new(),
        compaction_total: UsageCounters::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn_loop::types::LLMMessage;

    fn usage(input: u32, output: u32) -> TokenUsage {
        TokenUsage {
            input_tokens: input,
            output_tokens: output,
            total_tokens: input + output,
            ..Default::default()
        }
    }

    #[test]
    fn record_turn_groups_by_model_and_tracks_the_current_turn() {
        let store = SqliteSessionStore::in_memory().unwrap();
        store.create_session("sess-usage", None).unwrap();

        record_turn(&store, "sess-usage", "model-a", &usage(100, 10)).unwrap();
        record_turn(&store, "sess-usage", "model-a", &usage(50, 5)).unwrap();
        record_turn(&store, "sess-usage", "model-b", &usage(7, 3)).unwrap();

        let status = status(&store, "sess-usage", "fallback");
        assert_eq!(status.by_model["model-a"].input_other, 150);
        assert_eq!(status.by_model["model-a"].output, 15);
        assert_eq!(status.by_model["model-b"].input_other, 7);
        assert_eq!(status.total.input_other, 157);
        assert_eq!(status.total.output, 18);
        assert_eq!(
            status.current_turn,
            Some(UsageCounters {
                input_other: 7,
                output: 3,
                input_cache_read: 0,
                input_cache_creation: 0,
            })
        );
    }

    #[test]
    fn record_accumulates_the_bucket_and_sets_the_current_turn() {
        let mut status = UsageStatus::default();
        status.record("model-a", &usage(10, 1));
        status.record("model-a", &usage(5, 2));
        status.record("model-b", &usage(1, 1));
        assert_eq!(status.by_model["model-a"].input_other, 15);
        assert_eq!(status.by_model["model-b"].output, 1);
        assert_eq!(status.total.input_other, 16);
        assert_eq!(status.total.output, 4);
        assert_eq!(status.current_turn.map(|turn| turn.input_other), Some(1));
    }

    #[test]
    fn a_zero_usage_turn_still_creates_the_bucket_and_current_turn() {
        // v2 `UsageAgentModel.record` runs for every turn, zero usage included:
        // `byModel[model]` exists and `currentTurn` is set. Skipping zero hid
        // "a turn ran with no billable usage" from the status readers.
        let store = SqliteSessionStore::in_memory().unwrap();
        store.create_session("sess-zero", None).unwrap();
        record_turn(&store, "sess-zero", "model-a", &usage(0, 0)).unwrap();

        let status = status(&store, "sess-zero", "fallback");
        assert!(status.by_model.contains_key("model-a"));
        assert_eq!(status.total, UsageCounters::default());
        assert_eq!(status.current_turn, Some(UsageCounters::default()));

        let mut in_memory = UsageStatus::default();
        in_memory.record("model-a", &usage(0, 0));
        assert_eq!(in_memory.current_turn, Some(UsageCounters::default()));
        assert!(in_memory.by_model.contains_key("model-a"));
    }

    #[test]
    fn compaction_usage_is_billed_but_kept_apart_from_the_turn() {
        let store = SqliteSessionStore::in_memory().unwrap();
        store.create_session("sess-compaction", None).unwrap();

        record_turn(&store, "sess-compaction", "model-a", &usage(100, 10)).unwrap();
        record_compaction(&store, "sess-compaction", "model-a", &usage(40, 4)).unwrap();

        let status = status(&store, "sess-compaction", "fallback");
        assert_eq!(
            status.by_model["model-a"].input_other, 140,
            "compaction is part of the billable model bucket"
        );
        assert_eq!(status.total.input_other, 140);
        assert_eq!(status.total.output, 14);
        assert_eq!(
            status.compaction_by_model["model-a"].input_other, 40,
            "the compaction slice is recorded separately"
        );
        assert_eq!(status.compaction_total.output, 4);
        assert_eq!(
            status.current_turn.map(|turn| turn.input_other),
            Some(100),
            "a compaction must not overwrite the current chat turn"
        );

        let value = status.to_json().expect("non-empty status");
        assert_eq!(value["compactionByModel"]["model-a"]["inputOther"], 40);
        assert_eq!(value["compactionTotal"]["output"], 4);

        let round_trip = UsageStatus::from_json_str(&value.to_string());
        assert_eq!(round_trip.compaction_total.input_other, 40);
        assert_eq!(round_trip.total.input_other, 140);
    }

    #[test]
    fn from_json_rebuilds_the_buckets_and_ignores_a_stale_total() {
        let snapshot = serde_json::json!({
            "byModel": {
                "model-a": {"inputOther": 10, "output": 2, "inputCacheRead": 3, "inputCacheCreation": 4}
            },
            "total": {"inputOther": 999, "output": 999, "inputCacheRead": 999, "inputCacheCreation": 999},
            "currentTurn": {"inputOther": 6, "output": 1, "inputCacheRead": 0, "inputCacheCreation": 0}
        });
        let status = UsageStatus::from_json(&snapshot);
        assert_eq!(status.by_model["model-a"].input_other, 10);
        assert_eq!(status.total.input_other, 10);
        assert_eq!(status.current_turn.map(|turn| turn.input_other), Some(6));
        let round_trip = UsageStatus::from_json_str(&snapshot.to_string());
        assert_eq!(round_trip.total.output, 2);
    }

    #[test]
    fn a_failed_read_is_not_written_back_and_keeps_the_accumulated_counters() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("sessions.db");
        let store = SqliteSessionStore::open(&db_path).unwrap();
        store.create_session("sess-read-error", None).unwrap();
        record_turn(&store, "sess-read-error", "model-a", &usage(100, 10)).unwrap();

        let original = {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.query_row(
                "SELECT value FROM state_entries WHERE domain = 'usage' AND key = 'sess-read-error'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
        };

        // Blob storage makes the typed `value` read fail while the row stays
        // writable — exactly the "read errors, write succeeds" case that used
        // to reset the counters and overwrite the row.
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute(
                "UPDATE state_entries SET value = X'7B7D' WHERE domain = 'usage' AND key = 'sess-read-error'",
                [],
            )
            .unwrap();
        }

        let error = record_turn(&store, "sess-read-error", "model-a", &usage(5, 1))
            .expect_err("a failed read must abort before the write-back");
        assert!(
            error.to_string().contains("Invalid column type"),
            "the read error must surface unchanged: {error}"
        );

        let (kind, raw) = {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.query_row(
                "SELECT typeof(value), hex(value) FROM state_entries WHERE domain = 'usage' AND key = 'sess-read-error'",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .unwrap()
        };
        assert_eq!(
            (kind.as_str(), raw.as_str()),
            ("blob", "7B7D"),
            "a failed read must not overwrite the row with a zeroed snapshot"
        );

        // Restoring the intact payload proves the counter survived the failed
        // turn: the write-back never ran.
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute(
                "UPDATE state_entries SET value = ?1 WHERE domain = 'usage' AND key = 'sess-read-error'",
                rusqlite::params![original],
            )
            .unwrap();
        }
        assert_eq!(
            status(&store, "sess-read-error", "model-a")
                .total
                .input_other,
            100,
            "the failed turn must not reset the accumulated counters"
        );

        // A successful read resumes accumulation instead of starting over.
        record_turn(&store, "sess-read-error", "model-a", &usage(5, 1)).unwrap();
        assert_eq!(
            status(&store, "sess-read-error", "model-a")
                .total
                .input_other,
            105
        );
    }

    #[test]
    fn status_reconstructs_legacy_sessions_from_the_turn_table() {
        let store = SqliteSessionStore::in_memory().unwrap();
        store.create_session("sess-legacy", None).unwrap();
        store
            .save_turn(
                "sess-legacy",
                "turn-1",
                1,
                &[LLMMessage::user("hi")],
                Some(&usage(20, 2)),
                None,
            )
            .unwrap();
        store
            .save_turn(
                "sess-legacy",
                "turn-2",
                2,
                &[LLMMessage::user("again")],
                Some(&usage(30, 3)),
                None,
            )
            .unwrap();

        let status = status(&store, "sess-legacy", "configured-model");
        assert_eq!(status.by_model["configured-model"].input_other, 50);
        assert_eq!(status.total.input_other, 50);
        assert_eq!(
            status.current_turn.map(|c| c.input_other),
            Some(30),
            "the last recorded turn is the current turn"
        );
    }

    #[test]
    fn status_json_uses_the_v2_camel_case_shape() {
        let mut status = UsageStatus::default();
        status.by_model.insert(
            "model-a".into(),
            UsageCounters {
                input_other: 1,
                output: 2,
                input_cache_read: 3,
                input_cache_creation: 4,
            },
        );
        status.total = status.by_model["model-a"];
        status.current_turn = Some(status.total);
        let value = status.to_json().expect("non-empty status");
        assert_eq!(value["byModel"]["model-a"]["inputOther"], 1);
        assert_eq!(value["byModel"]["model-a"]["inputCacheRead"], 3);
        assert_eq!(value["total"]["output"], 2);
        assert_eq!(value["currentTurn"]["inputCacheCreation"], 4);
    }
}
