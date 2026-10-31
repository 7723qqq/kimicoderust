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

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::rpc::types::TokenUsage;
use crate::session::sqlite_store::SqliteSessionStore;

/// The state domain the buckets live in (`agent_config` / `metadata` precedent).
const USAGE_DOMAIN: &str = "usage";

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
}

/// v2 `UsageStatus`: the model buckets, their sum, and the current turn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageStatus {
    pub by_model: BTreeMap<String, UsageCounters>,
    pub total: UsageCounters,
    pub current_turn: Option<UsageCounters>,
}

impl UsageStatus {
    fn from_persisted(persisted: PersistedUsage) -> Self {
        let mut total = UsageCounters::default();
        for counters in persisted.by_model.values() {
            total.add(*counters);
        }
        Self {
            by_model: persisted.by_model,
            total,
            current_turn: persisted.current_turn,
        }
    }

    /// Fold one finished turn into the model's bucket (v2
    /// `UsageAgentModel.record`): the bucket and the total grow, and the turn
    /// becomes `currentTurn`. Zero-usage turns change nothing.
    pub fn record(&mut self, model: &str, usage: &TokenUsage) {
        let counters = UsageCounters::from_token_usage(usage);
        if counters.is_zero() {
            return;
        }
        self.by_model
            .entry(model.to_string())
            .or_default()
            .add(counters);
        self.total.add(counters);
        self.current_turn = Some(counters);
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
        let mut total = UsageCounters::default();
        for counters in by_model.values() {
            total.add(*counters);
        }
        Self {
            by_model,
            total,
            current_turn,
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
        Some(Value::Object(object))
    }
}

fn load(store: &SqliteSessionStore, session_id: &str) -> PersistedUsage {
    store
        .get_state(USAGE_DOMAIN, session_id)
        .ok()
        .flatten()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default()
}

/// Fold one finished turn's usage into the session's model bucket and refresh
/// `currentTurn`. A zero-usage turn is not recorded, so an untouched session
/// keeps the empty status v2 reports.
pub fn record_turn(
    store: &SqliteSessionStore,
    session_id: &str,
    model: &str,
    usage: &TokenUsage,
) -> Result<(), rusqlite::Error> {
    let counters = UsageCounters::from_token_usage(usage);
    if counters.is_zero() {
        return Ok(());
    }
    let mut persisted = load(store, session_id);
    persisted
        .by_model
        .entry(model.to_string())
        .or_default()
        .add(counters);
    persisted.current_turn = Some(counters);
    let value = serde_json::to_value(&persisted).unwrap_or(Value::Null);
    store.put_state(USAGE_DOMAIN, session_id, &value)
}

/// Read the session's usage status. Sessions written before this module existed
/// have per-turn usage in the `turns` table but no buckets; reconstruct them
/// against `fallback_model` (the session's configured model) so an upgraded
/// session does not report zero forever. The buckets win once they exist.
pub fn status(store: &SqliteSessionStore, session_id: &str, fallback_model: &str) -> UsageStatus {
    let persisted = load(store, session_id);
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
    fn zero_usage_turns_do_not_create_an_empty_status() {
        let store = SqliteSessionStore::in_memory().unwrap();
        store.create_session("sess-zero", None).unwrap();
        record_turn(&store, "sess-zero", "model-a", &TokenUsage::default()).unwrap();
        assert!(status(&store, "sess-zero", "model-a").to_json().is_none());
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
