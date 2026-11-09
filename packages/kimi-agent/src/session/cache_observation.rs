//! Prompt-cache observability (v2 `agent/usage/cacheObservation.ts`,
//! `cacheBreakService.ts`, `cacheProbeService.ts`).
//!
//! Three related records, all derived from the usage a provider reports:
//!
//! - [`CacheObservation`]: what the last turn-sourced request looked like —
//!   model, protocol, effort, the request projection, the history it carried
//!   and the cache buckets the provider charged. v2 stores it as
//!   `llmRequesterLastTurnRequest.observation` on every request.
//! - [`CacheBreakTracker`]: v2 `AgentCacheBreakService.observe`. It keeps the
//!   previous step's snapshot and reports a break when the prompt cache read
//!   collapses (`< 95%` of the previous read and by more than 2000 tokens),
//!   classified into model / effort / projection / prefix resets. A compaction
//!   resets the baseline.
//! - [`CacheProbeRequest`] / [`build_cache_probe`]: v2 `prompt_cache_probe`
//!   telemetry. It compares a fork's first request (or a compaction request)
//!   with the parent agent's last request, so a forked conversation's cache
//!   reuse can be attributed.
//!
//! Nothing here is a wire type: the tracker's output is only logged, kept for
//! an engine read entry, and emitted through the existing host telemetry
//! channel (`cache_break_detected`, exactly v2's `track2` event). v2 also
//! carries the observation on `TurnStepCompleted.cache`; Rust's step event is
//! a wire payload and deliberately keeps that field out.

use serde::{Deserialize, Serialize};

use crate::rpc::types::TokenUsage;

/// v2 `CACHE_BREAK_MIN_DROP_TOKENS`.
pub const CACHE_BREAK_MIN_DROP_TOKENS: u64 = 2000;
/// v2 `CACHE_BREAK_DROP_RATIO`.
pub const CACHE_BREAK_DROP_RATIO: f64 = 0.95;

/// The request one provider response answered (v2 `CacheObservation`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheObservation {
    pub model_alias: String,
    #[serde(default)]
    pub protocol: Option<String>,
    #[serde(default)]
    pub provider_type: Option<String>,
    /// Messages the request carried (`llm.request.messageCount`).
    pub message_count: usize,
    /// Fingerprint of the request's message list.
    pub history_hash: String,
    /// The media projection in effect (`llm.request.projection`), when a
    /// degrade/strip/strict round applied.
    #[serde(default)]
    pub projection: Option<String>,
    #[serde(default)]
    pub thinking_effort: Option<String>,
    /// Every input bucket (`inputTotal`: uncached + cache read + cache
    /// creation), v2 `buildCacheObservation`.
    #[serde(default)]
    pub input_tokens: Option<u64>,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub cache_creation: u64,
    #[serde(default)]
    pub output_tokens: u64,
}

/// The inputs of [`build_cache_observation`] (v2 `CacheObservationInput`).
pub struct CacheObservationInput<'a> {
    pub model_alias: &'a str,
    pub protocol: Option<&'a str>,
    pub provider_type: Option<&'a str>,
    pub message_count: usize,
    pub history_hash: &'a str,
    pub projection: Option<&'a str>,
    pub thinking_effort: Option<&'a str>,
    pub usage: Option<&'a TokenUsage>,
}

/// v2 `inputTotal`: every input bucket, cached or not.
pub fn input_total(usage: &TokenUsage) -> u64 {
    u64::from(usage.input_tokens)
        + u64::from(usage.input_cache_read)
        + u64::from(usage.input_cache_creation)
}

impl CacheObservation {
    /// The measured usage the observation was built from: the uncached input
    /// bucket is the total minus the cache buckets. Used by the engine's break
    /// tracker, which receives observations rather than raw usages.
    pub fn usage(&self) -> TokenUsage {
        let total = self.input_tokens.unwrap_or(0);
        let cache_read = self.cache_read.min(u64::from(u32::MAX)) as u32;
        let cache_creation = self.cache_creation.min(u64::from(u32::MAX)) as u32;
        TokenUsage {
            input_tokens: total
                .saturating_sub(self.cache_read + self.cache_creation)
                .min(u64::from(u32::MAX)) as u32,
            output_tokens: self.output_tokens.min(u64::from(u32::MAX)) as u32,
            total_tokens: 0,
            input_cache_read: cache_read,
            input_cache_creation: cache_creation,
        }
    }
}

pub fn build_cache_observation(input: CacheObservationInput<'_>) -> CacheObservation {
    CacheObservation {
        model_alias: input.model_alias.to_string(),
        protocol: input.protocol.map(str::to_string),
        provider_type: input.provider_type.map(str::to_string),
        message_count: input.message_count,
        history_hash: input.history_hash.to_string(),
        projection: input.projection.map(str::to_string),
        thinking_effort: input.thinking_effort.map(str::to_string),
        input_tokens: input.usage.map(input_total),
        cache_read: input
            .usage
            .map_or(0, |usage| u64::from(usage.input_cache_read)),
        cache_creation: input
            .usage
            .map_or(0, |usage| u64::from(usage.input_cache_creation)),
        output_tokens: input
            .usage
            .map_or(0, |usage| u64::from(usage.output_tokens)),
    }
}

/// Why a cache break happened (v2 `CacheBreakCause`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheBreakCause {
    ModelSwitch,
    EffortSwitch,
    ProjectionChange,
    PrefixReset,
    Unexplained,
}

impl CacheBreakCause {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ModelSwitch => "model_switch",
            Self::EffortSwitch => "effort_switch",
            Self::ProjectionChange => "projection_change",
            Self::PrefixReset => "prefix_reset",
            Self::Unexplained => "unexplained",
        }
    }
}

/// One step's cache state (v2 `CacheBreakSnapshot`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheBreakSnapshot {
    pub model: String,
    pub effort: String,
    #[serde(default)]
    pub projection: Option<String>,
    #[serde(default)]
    pub message_count: Option<usize>,
    /// The non-cached input bucket (v2 `usage.inputOther`).
    pub input_other: u64,
    pub input_cache_read: u64,
    pub input_cache_creation: u64,
    pub output: u64,
    /// Epoch millis of the observation.
    pub at: i64,
}

/// A detected break (v2 `CacheBreakObservation`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheBreakObservation {
    pub cause: CacheBreakCause,
    pub previous: CacheBreakSnapshot,
    pub current: CacheBreakSnapshot,
    pub cache_read_drop_ratio: f64,
    pub interval_ms: i64,
}

impl CacheBreakObservation {
    /// The `cache_break_detected` telemetry body (v2 `CacheBreakDetectedEvent`),
    /// field for field.
    pub fn telemetry_payload(&self, turn_id: Option<&str>, step: Option<u32>) -> serde_json::Value {
        serde_json::json!({
            "type": "cache_break_detected",
            "turn_id": turn_id,
            "step_no": step,
            "cause": self.cause.as_str(),
            "prev_model": self.previous.model,
            "curr_model": self.current.model,
            "prev_effort": self.previous.effort,
            "curr_effort": self.current.effort,
            "prev_projection": self.previous.projection,
            "curr_projection": self.current.projection,
            "prev_message_count": self.previous.message_count,
            "curr_message_count": self.current.message_count,
            "prev_input_cache_read": self.previous.input_cache_read,
            "curr_input_cache_read": self.current.input_cache_read,
            "prev_input_other": self.previous.input_other,
            "curr_input_other": self.current.input_other,
            "prev_output": self.previous.output,
            "curr_output": self.current.output,
            "prev_input_cache_creation": self.previous.input_cache_creation,
            "curr_input_cache_creation": self.current.input_cache_creation,
            "cache_read_drop_ratio": self.cache_read_drop_ratio,
            "interval_ms": self.interval_ms,
        })
    }
}

/// Per-agent cache-break baseline (v2 `AgentCacheBreakService`).
#[derive(Debug, Clone, Default)]
pub struct CacheBreakTracker {
    baseline: Option<CacheBreakSnapshot>,
}

impl CacheBreakTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// v2 `observe`: fold one step's usage in and report a break. No cache
    /// observation (the request did not come from the turn loop) or an
    /// all-zero usage (the provider reported nothing measurable) produces no
    /// baseline update and no output.
    pub fn observe(
        &mut self,
        usage: &TokenUsage,
        cache: Option<&CacheObservation>,
        now_ms: i64,
    ) -> Option<CacheBreakObservation> {
        let cache = cache?;
        if is_unmeasured(usage) {
            return None;
        }
        let current = CacheBreakSnapshot {
            model: cache.model_alias.clone(),
            effort: cache.thinking_effort.clone().unwrap_or_default(),
            projection: cache.projection.clone(),
            message_count: Some(cache.message_count),
            input_other: u64::from(usage.input_tokens),
            input_cache_read: u64::from(usage.input_cache_read),
            input_cache_creation: u64::from(usage.input_cache_creation),
            output: u64::from(usage.output_tokens),
            at: now_ms,
        };
        let previous = self.baseline.replace(current.clone())?;
        let previous_read = previous.input_cache_read;
        let current_read = current.input_cache_read;
        if current_read as f64 >= previous_read as f64 * CACHE_BREAK_DROP_RATIO {
            return None;
        }
        if previous_read.saturating_sub(current_read) <= CACHE_BREAK_MIN_DROP_TOKENS {
            // v2 computes the difference on `number`s; a previous read below
            // the minimum can never produce a drop above it, and the
            // saturating subtraction keeps the comparison exact at zero.
            return None;
        }
        let cause = classify_cache_break(&previous, &current);
        Some(CacheBreakObservation {
            cause,
            cache_read_drop_ratio: (previous_read - current_read) as f64 / previous_read as f64,
            interval_ms: now_ms - previous.at,
            previous,
            current,
        })
    }

    /// v2 resets the baseline when a compaction completes: the transcript was
    /// replaced, so the pre-compaction read is not comparable.
    pub fn reset(&mut self) {
        self.baseline = None;
    }

    pub fn baseline(&self) -> Option<&CacheBreakSnapshot> {
        self.baseline.as_ref()
    }
}

/// v2 `isUnmeasured`: the provider reported nothing at all.
pub fn is_unmeasured(usage: &TokenUsage) -> bool {
    usage.input_tokens == 0
        && usage.output_tokens == 0
        && usage.input_cache_read == 0
        && usage.input_cache_creation == 0
}

/// v2 `classifyCacheBreak`, in the same precedence order.
pub fn classify_cache_break(
    previous: &CacheBreakSnapshot,
    current: &CacheBreakSnapshot,
) -> CacheBreakCause {
    if previous.model != current.model {
        return CacheBreakCause::ModelSwitch;
    }
    if previous.effort != current.effort {
        return CacheBreakCause::EffortSwitch;
    }
    if previous.projection != current.projection {
        return CacheBreakCause::ProjectionChange;
    }
    if let (Some(previous_count), Some(current_count)) =
        (previous.message_count, current.message_count)
        && current_count < previous_count
    {
        return CacheBreakCause::PrefixReset;
    }
    CacheBreakCause::Unexplained
}

/// The per-request snapshot `prompt_cache_probe` compares (v2
/// `AgentLLMTurnRequestSnapshot`, restricted to the probe's fields).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheProbeRequest {
    pub model_alias: String,
    pub tools_hash: String,
    pub system_prompt_hash: String,
    pub message_count: usize,
    pub history_hash: String,
    #[serde(default)]
    pub input_tokens: Option<u64>,
    pub started_at: i64,
    pub finished_at: i64,
}

/// Which request the probe is attached to (v2 `promptCacheProbe.source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheProbeSource {
    Fork,
    Compaction,
}

/// The `prompt_cache_probe` telemetry body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheProbeObservation {
    pub source: CacheProbeSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    pub input_tokens: u64,
    pub input_cache_read: u64,
    pub input_cache_creation: u64,
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_match: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools_match: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_match: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_prefix_match: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_request_gap_ms: Option<i64>,
}

/// v2 `AgentCacheProbeService.probe` (fork) and `llmRequesterService`'s
/// compaction arm. `parent` is the fork's captured baseline (or the
/// compaction request's last-turn baseline), and is only passed when the
/// caller established that it is still comparable (the fork's parent context
/// did not change under it, and a compaction baseline was captured before the
/// request). v2 leaves every comparison field `undefined` when there is no
/// baseline or the model switched; serialization skips those fields instead of
/// writing `null`.
///
/// The fork probe deliberately has no `history_prefix_match` and no
/// `model_match`: v2's `cacheProbeService` payload only carries tool/system
/// comparisons for a fork, while the compaction arm is the one that reports a
/// model match and a history-prefix check.
pub fn build_cache_probe(
    source: CacheProbeSource,
    turn_id: Option<&str>,
    provider_type: Option<&str>,
    protocol: Option<&str>,
    usage: Option<&TokenUsage>,
    own: &CacheProbeRequest,
    parent: Option<&CacheProbeRequest>,
    history_prefix_match: Option<bool>,
) -> CacheProbeObservation {
    let compared_parent = parent.filter(|parent| parent.model_alias == own.model_alias);
    CacheProbeObservation {
        source,
        turn_id: turn_id.map(str::to_string),
        provider_type: provider_type.map(str::to_string),
        protocol: protocol.map(str::to_string),
        input_tokens: usage.map_or(0, input_total),
        input_cache_read: usage.map_or(0, |usage| u64::from(usage.input_cache_read)),
        input_cache_creation: usage.map_or(0, |usage| u64::from(usage.input_cache_creation)),
        output_tokens: usage.map_or(0, |usage| u64::from(usage.output_tokens)),
        // v2's compaction arm reports `model_match` whenever a baseline exists;
        // the fork arm reports neither.
        model_match: match source {
            CacheProbeSource::Compaction => {
                parent.map(|parent| parent.model_alias == own.model_alias)
            }
            CacheProbeSource::Fork => None,
        },
        tools_match: compared_parent.map(|parent| parent.tools_hash == own.tools_hash),
        system_prompt_match: compared_parent
            .map(|parent| parent.system_prompt_hash == own.system_prompt_hash),
        // Only the compaction arm prefixes the request's history with the
        // baseline snapshot (v2 `historyPrefixMatch`). v2 writes
        // `compared && ...`, so a baseline whose model switched reports a
        // definite `false` rather than leaving the field undefined.
        history_prefix_match: match source {
            CacheProbeSource::Compaction => parent.and_then(|parent| {
                if parent.model_alias == own.model_alias {
                    history_prefix_match
                } else {
                    Some(false)
                }
            }),
            CacheProbeSource::Fork => None,
        },
        parent_input_tokens: compared_parent.and_then(|parent| parent.input_tokens),
        parent_request_gap_ms: compared_parent.map(|parent| own.started_at - parent.finished_at),
    }
}

/// v2 compaction `historyPrefixMatch`: the baseline's message list must still
/// be the head of the compaction request's messages, so a request that dropped
/// history during pre-shrinking is not reported as a clean cache probe.
pub fn compaction_history_prefix_matches(
    request_messages: &[crate::turn_loop::types::LLMMessage],
    baseline: &CacheProbeRequest,
) -> bool {
    if baseline.message_count > request_messages.len() {
        return false;
    }
    let prefix =
        serde_json::to_string(&request_messages[..baseline.message_count]).unwrap_or_default();
    fingerprint(&prefix) == baseline.history_hash
}

impl CacheProbeObservation {
    pub fn telemetry_payload(&self) -> serde_json::Value {
        let mut value = serde_json::to_value(self).unwrap_or_else(|_| serde_json::json!({}));
        if let Some(object) = value.as_object_mut() {
            object.insert("type".into(), serde_json::json!("prompt_cache_probe"));
        }
        value
    }
}

/// A stable, deterministic fingerprint over a request's serialized shape.
/// v2 uses `fingerprint` for the same purpose: hashes are only ever compared
/// with other hashes produced here, so the algorithm need not match v2's.
pub fn fingerprint(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// The media projection label v2 records on `llm.request.projection`.
pub fn projection_label(
    structure_strict: bool,
    media_degraded: bool,
    media_stripped: bool,
) -> Option<&'static str> {
    match (structure_strict, media_degraded, media_stripped) {
        (false, false, false) => None,
        (true, false, false) => Some("strict"),
        (false, false, true) => Some("media-stripped"),
        (false, true, false) => Some("media-degraded"),
        (true, false, true) => Some("strict-media-stripped"),
        (true, true, false) => Some("strict-media-degraded"),
        // A later strip supersedes a degrade within the same turn.
        (false, true, true) => Some("media-stripped"),
        (true, true, true) => Some("strict-media-stripped"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input_other: u32, cache_read: u32, cache_creation: u32, output: u32) -> TokenUsage {
        TokenUsage {
            input_tokens: input_other,
            output_tokens: output,
            total_tokens: input_other + cache_read + cache_creation + output,
            input_cache_read: cache_read,
            input_cache_creation: cache_creation,
        }
    }

    fn observation(model: &str, effort: Option<&str>, message_count: usize) -> CacheObservation {
        build_cache_observation(CacheObservationInput {
            model_alias: model,
            protocol: Some("anthropic"),
            provider_type: Some("anthropic"),
            message_count,
            history_hash: "h",
            projection: None,
            thinking_effort: effort,
            usage: None,
        })
    }

    #[test]
    fn an_observation_carries_every_usage_bucket() {
        let measured = usage(100, 900, 30, 40);
        let observation = build_cache_observation(CacheObservationInput {
            model_alias: "kimi",
            protocol: Some("openai"),
            provider_type: Some("openai"),
            message_count: 7,
            history_hash: "abc",
            projection: Some("strict"),
            thinking_effort: Some("high"),
            usage: Some(&measured),
        });
        assert_eq!(observation.input_tokens, Some(1030));
        assert_eq!(observation.cache_read, 900);
        assert_eq!(observation.cache_creation, 30);
        assert_eq!(observation.output_tokens, 40);
        assert_eq!(observation.message_count, 7);
        assert_eq!(observation.projection.as_deref(), Some("strict"));
    }

    /// A measured cache collapse is reported once, classified, and becomes the
    /// next baseline.
    #[test]
    fn a_cache_read_collapse_is_recorded() {
        let mut tracker = CacheBreakTracker::new();
        let first = observation("kimi", Some("high"), 4);
        assert!(
            tracker
                .observe(&usage(10, 10_000, 0, 10), Some(&first), 1_000)
                .is_none(),
            "the first observation only seeds the baseline"
        );
        let second = observation("kimi", Some("high"), 6);
        let breakage = tracker
            .observe(&usage(5_000, 2_000, 0, 10), Some(&second), 2_500)
            .expect("an 8000-token drop must be reported");
        assert_eq!(breakage.cause, CacheBreakCause::Unexplained);
        assert_eq!(breakage.interval_ms, 1_500);
        assert!((breakage.cache_read_drop_ratio - 0.8).abs() < f64::EPSILON);
        assert_eq!(
            tracker.baseline().map(|snapshot| snapshot.input_cache_read),
            Some(2_000)
        );
    }

    /// A small dip (below the 5%/2000-token thresholds) stays silent.
    #[test]
    fn a_small_cache_read_dip_is_not_noise() {
        let mut tracker = CacheBreakTracker::new();
        let first = observation("kimi", None, 4);
        tracker.observe(&usage(10, 10_000, 0, 10), Some(&first), 0);
        let second = observation("kimi", None, 5);
        assert!(
            tracker
                .observe(&usage(10, 9_800, 0, 10), Some(&second), 1_000)
                .is_none()
        );
        assert_eq!(
            tracker.baseline().map(|snapshot| snapshot.input_cache_read),
            Some(9_800),
            "the silent dip still becomes the next reference"
        );
    }

    /// The absolute floor applies even when the ratio is exceeded.
    #[test]
    fn the_absolute_drop_floor_is_enforced() {
        let mut tracker = CacheBreakTracker::new();
        let first = observation("kimi", None, 4);
        tracker.observe(&usage(0, 2_000, 0, 1), Some(&first), 0);
        let second = observation("kimi", None, 5);
        assert!(
            tracker
                .observe(&usage(0, 0, 0, 1), Some(&second), 1)
                .is_none(),
            "2000 -> 0 is exactly 2000, and v2 requires a drop strictly greater"
        );
        tracker.reset();
        let first = observation("kimi", None, 4);
        tracker.observe(&usage(0, 2_001, 0, 1), Some(&first), 0);
        let second = observation("kimi", None, 5);
        assert!(
            tracker
                .observe(&usage(0, 0, 0, 1), Some(&second), 1)
                .is_some(),
            "2001 -> 0 clears both thresholds"
        );
    }

    /// Nothing is recorded without a cache observation or without measured
    /// usage, and such a step does not disturb the baseline.
    #[test]
    fn unobserved_and_unmeasured_steps_do_not_touch_the_baseline() {
        let mut tracker = CacheBreakTracker::new();
        let first = observation("kimi", None, 4);
        tracker.observe(&usage(1, 10_000, 0, 1), Some(&first), 0);
        assert!(
            tracker.observe(&usage(1, 0, 0, 1), None, 1).is_none(),
            "no observation, no probe"
        );
        let second = observation("kimi", None, 5);
        assert!(
            tracker
                .observe(&usage(0, 0, 0, 0), Some(&second), 2)
                .is_none(),
            "an unmeasured step (provider reported nothing) is skipped"
        );
        assert_eq!(
            tracker.baseline().map(|snapshot| snapshot.input_cache_read),
            Some(10_000),
            "the skipped steps must not move the baseline"
        );
    }

    #[test]
    fn breaks_are_classified_in_v2_precedence_order() {
        let base = CacheBreakSnapshot {
            model: "a".into(),
            effort: "high".into(),
            projection: None,
            message_count: Some(4),
            input_other: 0,
            input_cache_read: 10_000,
            input_cache_creation: 0,
            output: 0,
            at: 0,
        };
        let mut current = base.clone();
        current.model = "b".into();
        current.effort = "low".into();
        current.message_count = Some(1);
        assert_eq!(
            classify_cache_break(&base, &current),
            CacheBreakCause::ModelSwitch
        );
        current.model = "a".into();
        assert_eq!(
            classify_cache_break(&base, &current),
            CacheBreakCause::EffortSwitch
        );
        current.effort = "high".into();
        current.projection = Some("media-stripped".into());
        assert_eq!(
            classify_cache_break(&base, &current),
            CacheBreakCause::ProjectionChange
        );
        current.projection = None;
        assert_eq!(
            classify_cache_break(&base, &current),
            CacheBreakCause::PrefixReset
        );
        current.message_count = Some(9);
        assert_eq!(
            classify_cache_break(&base, &current),
            CacheBreakCause::Unexplained
        );
    }

    #[test]
    fn a_reset_drops_the_baseline() {
        let mut tracker = CacheBreakTracker::new();
        let first = observation("kimi", None, 4);
        tracker.observe(&usage(1, 10_000, 0, 1), Some(&first), 0);
        tracker.reset();
        assert!(tracker.baseline().is_none());
    }

    #[test]
    fn a_fork_probe_compares_the_parent_request() {
        let parent = CacheProbeRequest {
            model_alias: "kimi".into(),
            tools_hash: "tools".into(),
            system_prompt_hash: "prompt".into(),
            message_count: 3,
            history_hash: "hist".into(),
            input_tokens: Some(900),
            started_at: 100,
            finished_at: 200,
        };
        let own = CacheProbeRequest {
            model_alias: "kimi".into(),
            tools_hash: "tools".into(),
            system_prompt_hash: "other".into(),
            message_count: 9,
            history_hash: "hist".into(),
            input_tokens: Some(1_000),
            started_at: 250,
            finished_at: 300,
        };
        let measured = usage(5, 800, 0, 10);
        let probe = build_cache_probe(
            CacheProbeSource::Fork,
            Some("turn-1"),
            Some("anthropic"),
            Some("anthropic"),
            Some(&measured),
            &own,
            Some(&parent),
            None,
        );
        assert_eq!(probe.tools_match, Some(true));
        assert_eq!(probe.system_prompt_match, Some(false));
        assert_eq!(
            probe.history_prefix_match, None,
            "v2's fork probe never reports a history prefix"
        );
        assert_eq!(
            probe.model_match, None,
            "v2's fork probe never reports model_match"
        );
        assert_eq!(probe.parent_input_tokens, Some(900));
        assert_eq!(probe.parent_request_gap_ms, Some(50));
        assert_eq!(probe.input_tokens, 805);

        let mismatched = CacheProbeRequest {
            model_alias: "other".into(),
            ..parent.clone()
        };
        let probe = build_cache_probe(
            CacheProbeSource::Fork,
            None,
            None,
            None,
            Some(&measured),
            &own,
            Some(&mismatched),
            None,
        );
        assert_eq!(probe.model_match, None);
        assert_eq!(probe.tools_match, None);
        assert_eq!(probe.parent_input_tokens, None);
    }

    #[test]
    fn a_compaction_probe_reports_the_model_and_prefix_comparisons() {
        let baseline = CacheProbeRequest {
            model_alias: "kimi".into(),
            tools_hash: "tools".into(),
            system_prompt_hash: "prompt".into(),
            message_count: 2,
            history_hash: fingerprint("[]"),
            input_tokens: Some(900),
            started_at: 100,
            finished_at: 200,
        };
        let own = CacheProbeRequest {
            model_alias: "kimi".into(),
            tools_hash: fingerprint("[]"),
            system_prompt_hash: fingerprint(""),
            message_count: 3,
            history_hash: fingerprint("[1,2]"),
            input_tokens: Some(1_000),
            started_at: 250,
            finished_at: 300,
        };
        let probe = build_cache_probe(
            CacheProbeSource::Compaction,
            None,
            Some("anthropic"),
            Some("anthropic"),
            None,
            &own,
            Some(&baseline),
            Some(true),
        );
        assert_eq!(probe.model_match, Some(true));
        assert_eq!(probe.tools_match, Some(false));
        assert_eq!(probe.system_prompt_match, Some(false));
        assert_eq!(probe.history_prefix_match, Some(true));
        assert_eq!(probe.parent_input_tokens, Some(900));
        assert_eq!(probe.parent_request_gap_ms, Some(50));

        // No baseline still emits the probe, with every comparison undefined.
        let probe = build_cache_probe(
            CacheProbeSource::Compaction,
            Some("turn-7"),
            None,
            None,
            None,
            &own,
            None,
            None,
        );
        assert_eq!(probe.model_match, None);
        assert_eq!(probe.tools_match, None);
        assert_eq!(probe.history_prefix_match, None);
        assert_eq!(probe.turn_id.as_deref(), Some("turn-7"));

        // A switched model: v2's `compared && ...` reports a model mismatch
        // and a definite prefix failure, while the parent usage comparisons
        // stay undefined.
        let switched = CacheProbeRequest {
            model_alias: "other".into(),
            ..baseline.clone()
        };
        let probe = build_cache_probe(
            CacheProbeSource::Compaction,
            None,
            None,
            None,
            None,
            &own,
            Some(&switched),
            Some(true),
        );
        assert_eq!(probe.model_match, Some(false));
        assert_eq!(probe.tools_match, None);
        assert_eq!(probe.system_prompt_match, None);
        assert_eq!(probe.history_prefix_match, Some(false));
        assert_eq!(probe.parent_input_tokens, None);
    }

    #[test]
    fn a_compaction_prefix_matches_only_the_captured_head() {
        use crate::turn_loop::types::LLMMessage;
        let messages = vec![
            LLMMessage::user("one"),
            LLMMessage::user("two"),
            LLMMessage::user("three"),
        ];
        let mut baseline = CacheProbeRequest {
            model_alias: "kimi".into(),
            tools_hash: String::new(),
            system_prompt_hash: String::new(),
            message_count: 2,
            history_hash: fingerprint(
                &serde_json::to_string(&messages[..2]).expect("serialize two messages"),
            ),
            input_tokens: None,
            started_at: 0,
            finished_at: 0,
        };
        assert!(compaction_history_prefix_matches(&messages, &baseline));
        // The same prefix with a changed message is no longer the captured one.
        let changed = vec![
            messages[0].clone(),
            LLMMessage::user("changed"),
            messages[2].clone(),
        ];
        assert!(!compaction_history_prefix_matches(&changed, &baseline));
        // A baseline longer than the request cannot be a prefix of it.
        baseline.message_count = 4;
        assert!(!compaction_history_prefix_matches(&messages, &baseline));
    }

    #[test]
    fn projections_are_labelled_like_v2() {
        assert_eq!(projection_label(false, false, false), None);
        assert_eq!(projection_label(true, false, false), Some("strict"));
        assert_eq!(projection_label(false, true, false), Some("media-degraded"));
        assert_eq!(projection_label(false, false, true), Some("media-stripped"));
        assert_eq!(
            projection_label(true, true, false),
            Some("strict-media-degraded")
        );
        assert_eq!(
            projection_label(true, false, true),
            Some("strict-media-stripped")
        );
    }

    #[test]
    fn fingerprints_are_stable_and_distinct() {
        assert_eq!(fingerprint("abc"), fingerprint("abc"));
        assert_ne!(fingerprint("abc"), fingerprint("abd"));
        assert_eq!(fingerprint("").len(), 16);
    }
}
