use serde_json::Value;

use kimi_core::types::ArgsParseError;

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedToolCallArguments {
    pub data: Value,
    pub parse_failed: bool,
    pub error: Option<String>,
}

pub fn parse_tool_call_arguments(raw: Option<Value>) -> ParsedToolCallArguments {
    let empty = || ParsedToolCallArguments {
        data: serde_json::json!({}),
        parse_failed: false,
        error: None,
    };
    match raw {
        None | Some(Value::Null) => empty(),
        Some(Value::String(raw)) if raw.is_empty() => empty(),
        Some(Value::String(raw)) => match serde_json::from_str(&raw) {
            Ok(data) => ParsedToolCallArguments {
                data,
                parse_failed: false,
                error: None,
            },
            Err(error) => ParsedToolCallArguments {
                data: serde_json::json!({}),
                parse_failed: true,
                error: Some(error.to_string()),
            },
        },
        Some(data) => ParsedToolCallArguments {
            data,
            parse_failed: false,
            error: None,
        },
    }
}

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
        let (arguments, error) = parse_tool_arguments(Some(r#"{"command": "ech"#));
        assert_eq!(arguments, serde_json::json!({}));
        let error = error.expect("a truncated payload must report why");
        assert!(!error.error.is_empty(), "the reason must name the failure");
    }

    #[test]
    fn an_unparseable_payload_keeps_the_text_it_failed_on() {
        let payload = r#"{"command": "ech"#;
        let (_, error) = parse_tool_arguments(Some(payload));
        assert_eq!(error.expect("broken payload").raw, payload);
    }
}
