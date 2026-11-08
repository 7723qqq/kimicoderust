//! Decoding a provider's raw tool-call argument payload.
//!
//! v2 `parseToolCallArguments` (`agent-core-v2/src/tool/tool-args-parse.ts`) is
//! the reference. An absent, `null` or empty payload is a valid no-args call
//! (`{}`) and never a failure; a payload that is not valid JSON keeps its call
//! and records why, so the turn loop can answer that call with an error result.
//!
//! Both alternatives are wrong here. Dropping the call — what the streaming
//! transports used to do — leaves the model with no result for a call it
//! issued, so it cannot tell a parse failure from a lost message. Executing a
//! fabricated `{}` — what the non-streaming OpenAI path used to do — runs
//! `Bash` / `Write` with no inputs at all.

use serde_json::Value;

use crate::turn_loop::types::ArgsParseError;

/// Decode a provider's JSON-string argument payload.
///
/// Returns the arguments the call carries and, when the payload did not parse,
/// the failure with the payload it came from. The caller keeps the call either
/// way: the assistant message must keep its `tool_use` or a strict provider
/// rejects the transcript, and the model needs to read what went wrong.
///
/// The failure keeps `raw` because decoding collapses every bad payload to
/// `{}` — see [`ArgsParseError`] — and the dedupe key distinguishes them by the
/// text the provider actually sent.
pub fn parse_tool_arguments(raw: Option<&str>) -> (Value, Option<ArgsParseError>) {
    match raw {
        None => (serde_json::json!({}), None),
        Some("") => (serde_json::json!({}), None),
        Some(raw) => match serde_json::from_str(raw) {
            Ok(value) => (value, None),
            Err(error) => (
                serde_json::json!({}),
                Some(ArgsParseError {
                    raw: raw.to_string(),
                    error: error.to_string(),
                }),
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_or_empty_payload_is_a_valid_no_args_call() {
        assert_eq!(parse_tool_arguments(None), (serde_json::json!({}), None));
        assert_eq!(
            parse_tool_arguments(Some("")),
            (serde_json::json!({}), None)
        );
    }

    #[test]
    fn a_valid_payload_decodes_unchanged() {
        let (arguments, error) = parse_tool_arguments(Some(r#"{"path":"a.txt"}"#));
        assert_eq!(arguments, serde_json::json!({ "path": "a.txt" }));
        assert!(error.is_none(), "a payload that parses is never a failure");
    }

    #[test]
    fn an_unparseable_payload_reports_why_and_carries_no_arguments() {
        // v2 `parseToolCallArguments`: `{}` plus the failure. Keeping the call
        // is the caller's job — the decoder only says what went wrong.
        let (arguments, error) = parse_tool_arguments(Some(r#"{"command": "ech"#));
        assert_eq!(arguments, serde_json::json!({}));
        let error = error.expect("a truncated payload must report why");
        assert!(!error.error.is_empty(), "the reason must name the failure");
    }

    #[test]
    fn an_unparseable_payload_keeps_the_text_it_failed_on() {
        // v2 `registerSkipped` keys the dedupe on the raw payload, so the
        // decoder has to hand it back — the `{}` it returns is identical for
        // every broken payload.
        let payload = r#"{"command": "ech"#;
        let (_, error) = parse_tool_arguments(Some(payload));
        assert_eq!(error.expect("broken payload").raw, payload);
    }
}
