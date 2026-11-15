use serde_json::Value;

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

pub fn parse_tool_arguments(raw: Option<&str>) -> ParsedToolCallArguments {
    parse_tool_call_arguments(raw.map(|raw| Value::String(raw.to_string())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_or_empty_payload_is_a_valid_no_args_call() {
        for parsed in [parse_tool_arguments(None), parse_tool_arguments(Some(""))] {
            assert_eq!(parsed.data, serde_json::json!({}));
            assert!(!parsed.parse_failed);
            assert_eq!(parsed.error, None);
        }
    }

    #[test]
    fn a_valid_payload_decodes_unchanged() {
        let parsed = parse_tool_arguments(Some(r#"{"path":"a.txt"}"#));
        assert_eq!(parsed.data, serde_json::json!({ "path": "a.txt" }));
        assert!(!parsed.parse_failed);
        assert_eq!(parsed.error, None);
    }

    #[test]
    fn an_unparseable_payload_reports_why_and_uses_empty_arguments() {
        let parsed = parse_tool_arguments(Some(r#"{"command": "ech"#));
        assert_eq!(parsed.data, serde_json::json!({}));
        assert!(parsed.parse_failed);
        assert!(parsed.error.is_some_and(|error| !error.is_empty()));
    }

    #[test]
    fn a_non_string_payload_passes_through() {
        let parsed = parse_tool_call_arguments(Some(serde_json::json!([1, 2])));
        assert_eq!(parsed.data, serde_json::json!([1, 2]));
        assert!(!parsed.parse_failed);
        assert_eq!(parsed.error, None);
    }
}
