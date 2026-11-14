const TOOL_EXCHANGE_ADJACENCY_PATTERNS: [&str; 6] = [
    "role 'tool' must be a response to a preceding message",
    "role \"tool\" must be a response to a preceding message",
    "role `tool` must be a response to a preceding message",
    "assistant message with 'tool_calls' must be followed by tool messages",
    "tool_call_ids did not have response messages",
    "insufficient tool messages following",
];

const STRUCTURAL_REQUEST_PATTERNS: [&str; 6] = [
    "text content blocks must be non-empty",
    "text content blocks must be non empty",
    "text content blocks must contain non-whitespace",
    "text content blocks must contain non whitespace",
    "first message must use the",
    "roles must alternate",
];

fn matches_shaped_patterns(lower: &str) -> bool {
    if lower.contains("multiple ") && lower.contains(" roles in a row") {
        let between = lower
            .split_once("multiple ")
            .map(|(_, rest)| rest.split(" roles in a row").next().unwrap_or(""))
            .unwrap_or("");
        if between.contains("user") || between.contains("assistant") {
            return true;
        }
    }

    if lower.contains("message at position") && lower.contains("must not be empty") {
        let rest = lower
            .split_once("message at position")
            .map(|(_, rest)| rest.trim_start())
            .unwrap_or("");
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if !digits.is_empty() {
            return true;
        }
    }
    false
}

fn is_tool_exchange_adjacency_error(status: u16, lower: &str) -> bool {
    if status != 400 && status != 422 {
        return false;
    }

    let has_use = lower.contains("tool_use");
    let has_result = lower.contains("tool_result");
    if has_use && has_result {
        return true;
    }
    if has_result && lower.contains("unexpected") {
        return true;
    }

    if let Some((before, _)) = lower.split_once("ids must be unique")
        && before.contains("tool_use")
    {
        return true;
    }

    if let Some((before, _)) = lower.split_once("not found")
        && before.contains("tool_call_id")
    {
        return true;
    }
    TOOL_EXCHANGE_ADJACENCY_PATTERNS
        .iter()
        .any(|pattern| lower.contains(pattern))
}

pub fn is_request_structure_error(status: u16, message: &str) -> bool {
    let lower = message.to_lowercase();
    if is_tool_exchange_adjacency_error(status, &lower) {
        return true;
    }
    if status != 400 && status != 422 {
        return false;
    }
    STRUCTURAL_REQUEST_PATTERNS
        .iter()
        .any(|pattern| lower.contains(pattern))
        || matches_shaped_patterns(&lower)
}

pub const STRUCTURE_STRICT_CODE: &str = "structure-strict";

pub fn is_recoverable_request_structure_error(error: &str) -> bool {
    match kimi_core::error::llm_http_status(error) {
        Some(status) => is_request_structure_error(status, error),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_the_tool_exchange_complaints_it_can_repair() {
        for message in [
            "messages: tool_use ids must be unique",
            "unexpected `tool_result` block in the request",
            "tool_use block and tool_result block are not adjacent",
            "Invalid `tool_call_id`: not found in the preceding messages",
        ] {
            assert!(
                is_recoverable_request_structure_error(&format!("llm http status 400: {message}")),
                "expected a structure verdict for {message:?}"
            );
        }
    }

    #[test]
    fn accepts_the_remaining_shape_complaints() {
        for (status, message) in [
            (400, "text content blocks must be non-empty"),
            (400, "roles must alternate between user and assistant"),
            (400, "first message must use the \"user\" role"),
            (
                400,
                "invalid_request_error: message at position 3 with role \"user\" must not be empty",
            ),
            (422, "text content blocks must contain non-whitespace"),
        ] {
            assert!(
                is_recoverable_request_structure_error(&format!(
                    "llm http status {status}: {message}"
                )),
                "expected a structure verdict for {message:?}"
            );
        }
    }

    #[test]
    fn declines_the_classes_that_have_their_own_recovery() {
        for (status, message) in [
            (413, "Request too large"),
            (400, "invalid image data"),
            (400, "prompt is too long: 250000 tokens > 200000 maximum"),
            (429, "rate limit exceeded"),
            (500, "internal server error"),
            (401, "invalid api key"),
        ] {
            assert!(
                !is_recoverable_request_structure_error(&format!(
                    "llm http status {status}: {message}"
                )),
                "{message:?} must not be answered with a strict projection"
            );
        }
    }

    #[test]
    fn declines_an_error_that_carries_no_status() {
        assert!(!is_recoverable_request_structure_error(
            "error sending request for url (https://api.example.com/v1/chat/completions)"
        ));
        assert!(!is_recoverable_request_structure_error(
            "connection reset by peer"
        ));
    }

    #[test]
    fn declines_a_bare_400_with_no_shape_complaint() {
        assert!(!is_recoverable_request_structure_error(
            "llm http status 400: invalid request"
        ));
    }
}
