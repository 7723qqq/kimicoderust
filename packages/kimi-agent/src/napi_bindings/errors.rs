//! Structured errors on the napi boundary.
//!
//! napi-rs gives the JS side an `Error` with a message string and nothing
//! else: a domain code and its `details` would be flattened into text, so a
//! host could only distinguish "host unreachable" from "the user said no" by
//! matching strings. The envelope below keeps the message human-readable and
//! appends a machine-readable payload after [`MARKER`]; a host that has not
//! adopted the format still shows the prefix unchanged.
//!
//! The node-sdk side must split on the marker to read `code`/`details`; the
//! engine-side wiring already emits them at every export that has a domain
//! code to report.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Separates the human-readable message from the machine-readable envelope.
pub(crate) const MARKER: &str = "\n[kimi-error] ";

/// The machine-readable half of a boundary error.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ErrorEnvelope {
    /// Stable domain code (`session.unknown`, `session.release_timeout`, …).
    pub code: String,
    /// The same text that precedes [`MARKER`].
    pub message: String,
    /// Extra context; omitted when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

/// Render `message` plus its code and details as one boundary string.
pub(crate) fn encode(message: &str, code: &str, details: Option<Value>) -> String {
    let envelope = ErrorEnvelope {
        code: code.to_string(),
        message: message.to_string(),
        details,
    };
    match serde_json::to_string(&envelope) {
        Ok(json) => format!("{message}{MARKER}{json}"),
        // Serialization cannot fail for this shape; fall back to the message
        // rather than losing the error entirely.
        Err(_) => message.to_string(),
    }
}

/// Read back an envelope previously produced by [`encode`].
pub(crate) fn decode(message: &str) -> Option<ErrorEnvelope> {
    let (_, json) = message.rsplit_once(MARKER)?;
    serde_json::from_str(json).ok()
}

/// A structured `napi::Error`.
pub(crate) fn napi_error(
    code: &str,
    message: impl Into<String>,
    details: Option<Value>,
) -> napi::Error {
    let message = message.into();
    napi::Error::from_reason(encode(&message, code, details))
}

/// Wrap a message that does not already carry an envelope. A message that
/// already has one is passed through, so re-wrapping at an outer layer cannot
/// overwrite a more specific code.
pub(crate) fn napi_error_message(
    code: &str,
    message: String,
    details: Option<Value>,
) -> napi::Error {
    if decode(&message).is_some() {
        napi::Error::from_reason(message)
    } else {
        napi_error(code, message, details)
    }
}

/// Normalize an error string coming *from* the host.
///
/// A host may send a JSON object (`{"code", "message", "details"}`) instead of
/// a plain message; it is re-encoded so the code survives the rest of the
/// pipeline. Plain messages and already-encoded strings pass through
/// unchanged.
pub(crate) fn normalize_host_error(message: String) -> String {
    if decode(&message).is_some() {
        return message;
    }
    match serde_json::from_str::<ErrorEnvelope>(&message) {
        Ok(envelope) => encode(&envelope.message, &envelope.code, envelope.details),
        Err(_) => message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_envelope_round_trips_and_keeps_the_message_readable() {
        let raw = encode(
            "unknown session: s1",
            "session.unknown",
            Some(json!({"sessionId": "s1"})),
        );
        assert!(raw.starts_with("unknown session: s1"));
        let decoded = decode(&raw).expect("envelope");
        assert_eq!(decoded.code, "session.unknown");
        assert_eq!(decoded.message, "unknown session: s1");
        assert_eq!(decoded.details, Some(json!({"sessionId": "s1"})));
    }

    #[test]
    fn a_plain_message_has_no_envelope() {
        assert!(decode("something went wrong").is_none());
        assert!(decode("").is_none());
    }

    #[test]
    fn wrapping_does_not_replace_an_existing_code() {
        let existing = encode("boom", "engine.turn_failed", None);
        let error = napi_error_message("session.release_timeout", existing.clone(), None);
        assert_eq!(error.reason, existing);
    }

    #[test]
    fn a_host_json_error_keeps_its_code() {
        let normalized = normalize_host_error(
            serde_json::json!({
                "code": "host.unreachable",
                "message": "the host went away",
                "details": {"attempt": 2},
            })
            .to_string(),
        );
        let decoded = decode(&normalized).expect("envelope");
        assert_eq!(decoded.code, "host.unreachable");
        assert_eq!(decoded.message, "the host went away");
        assert_eq!(decoded.details, Some(json!({"attempt": 2})));
    }

    #[test]
    fn a_plain_host_error_passes_through() {
        assert_eq!(normalize_host_error("User denied".into()), "User denied");
    }
}
