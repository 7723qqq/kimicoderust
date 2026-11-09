
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LlmTiming {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_token_latency_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_duration_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_build_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_first_token_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_decode_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_consume_ms: Option<i64>,
}

impl LlmTiming {
    pub fn from_marks(
        started_ms: i64,
        sent_ms: Option<i64>,
        first_event_ms: Option<i64>,
        ended_ms: i64,
    ) -> Self {
        let first_event_ms = first_event_ms.filter(|at| *at >= started_ms);
        let sent_ms = sent_ms.filter(|at| *at >= started_ms);
        Self {
            first_token_latency_ms: first_event_ms.map(|at| at - started_ms),
            stream_duration_ms: first_event_ms.map(|at| (ended_ms - at).max(0)),
            request_build_ms: sent_ms.map(|at| at - started_ms),
            server_first_token_ms: match (sent_ms, first_event_ms) {
                (Some(sent), Some(first)) if first >= sent => Some(first - sent),
                _ => None,
            },
            server_decode_ms: None,
            client_consume_ms: None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.first_token_latency_ms.is_none()
            && self.stream_duration_ms.is_none()
            && self.request_build_ms.is_none()
            && self.server_first_token_ms.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_marks_applies_v2_formulas() {
        let timing = LlmTiming::from_marks(100, Some(140), Some(300), 900);
        assert_eq!(timing.first_token_latency_ms, Some(200));
        assert_eq!(timing.stream_duration_ms, Some(600));
        assert_eq!(timing.request_build_ms, Some(40));
        assert_eq!(timing.server_first_token_ms, Some(160));
        assert_eq!(timing.server_decode_ms, None);
        assert_eq!(timing.client_consume_ms, None);
    }

    #[test]
    fn test_from_marks_without_a_send_mark_has_no_build_split() {
        // A request that failed before reaching the wire still has a first
        // event only if the stream produced one; without the send mark the
        // build/server split does not exist (v2 leaves both undefined).
        let timing = LlmTiming::from_marks(0, None, Some(500), 800);
        assert_eq!(timing.first_token_latency_ms, Some(500));
        assert_eq!(timing.stream_duration_ms, Some(300));
        assert_eq!(timing.request_build_ms, None);
        assert_eq!(timing.server_first_token_ms, None);
    }

    #[test]
    fn test_from_marks_without_a_first_event() {
        let timing = LlmTiming::from_marks(0, Some(10), None, 50);
        assert_eq!(timing.first_token_latency_ms, None);
        assert_eq!(timing.stream_duration_ms, None);
        assert_eq!(timing.request_build_ms, Some(10));
        assert_eq!(timing.server_first_token_ms, None);
        assert!(timing.is_empty() || timing.request_build_ms.is_some());
    }

    #[test]
    fn test_from_marks_clamps_at_zero() {
        // A clock that moved backwards must not render a negative latency.
        let timing = LlmTiming::from_marks(500, Some(400), Some(300), 200);
        assert_eq!(timing.first_token_latency_ms, None);
        assert_eq!(timing.stream_duration_ms, None);
        assert_eq!(timing.request_build_ms, None);
        assert_eq!(timing.server_first_token_ms, None);
    }

    #[test]
    fn test_serde_camel_case_skips_none() {
        let timing = LlmTiming::from_marks(0, Some(10), Some(110), 210);
        let json = serde_json::to_value(&timing).unwrap();
        assert_eq!(json["firstTokenLatencyMs"], 110);
        assert_eq!(json["streamDurationMs"], 100);
        assert_eq!(json["requestBuildMs"], 10);
        assert_eq!(json["serverFirstTokenMs"], 100);
        assert!(json.get("serverDecodeMs").is_none());
        assert!(json.get("clientConsumeMs").is_none());
    }
}
