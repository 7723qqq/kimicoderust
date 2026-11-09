
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct LlmError {
    message: String,
    retry_after: Option<Duration>,
    status_code: Option<u16>,
    trace_id: Option<String>,
}

impl LlmError {
    pub fn http(status: u16, brief: &str, retry_after: Option<Duration>) -> Self {
        let suffix = retry_after.map_or(String::new(), |wait| {
            format!(" (retry-after {}s)", wait.as_secs())
        });
        Self {
            message: format!("llm http status {status}: {brief}{suffix}"),
            retry_after,
            status_code: Some(status),
            trace_id: None,
        }
    }

    #[must_use]
    pub fn typed(message: &str, status_code: Option<u16>) -> Self {
        Self {
            message: message.to_string(),
            retry_after: None,
            status_code,
            trace_id: None,
        }
    }

    #[must_use]
    pub fn attempts_exhausted(attempts: u32, message: &str, status_code: Option<u16>) -> Self {
        Self {
            message: format!("LLM call failed after {attempts} attempts: {message}"),
            retry_after: None,
            status_code,
            trace_id: None,
        }
    }

    #[must_use]
    pub fn with_trace_id(mut self, trace_id: Option<&str>) -> Self {
        let Some(id) = trace_id.map(str::trim).filter(|id| !id.is_empty()) else {
            return self;
        };
        self.message = format!("{} [trace {id}]", self.message);
        self.trace_id = Some(id.to_string());
        self
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn retry_after(&self) -> Option<Duration> {
        self.retry_after
    }

    pub fn status_code(&self) -> Option<u16> {
        self.status_code
    }

    pub fn trace_id(&self) -> Option<&str> {
        self.trace_id.as_deref()
    }
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for LlmError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_the_same_text_the_bare_string_used_to() {
        let without = LlmError::http(500, "internal server error", None);
        assert_eq!(
            without.to_string(),
            "llm http status 500: internal server error"
        );
        assert_eq!(without.status_code(), Some(500));
        assert_eq!(without.retry_after(), None);

        let with = LlmError::http(429, "slow down", Some(Duration::from_secs(7)));
        assert_eq!(
            with.to_string(),
            "llm http status 429: slow down (retry-after 7s)"
        );
        assert_eq!(with.retry_after(), Some(Duration::from_secs(7)));
    }

    #[test]
    fn keeps_sub_second_precision_the_suffix_loses() {
        let err = LlmError::http(429, "slow down", Some(Duration::from_millis(1500)));
        assert_eq!(err.retry_after(), Some(Duration::from_millis(1500)));
        // The diagnostic suffix still reads as whole seconds.
        assert_eq!(
            err.to_string(),
            "llm http status 429: slow down (retry-after 1s)"
        );
    }

    #[test]
    fn renders_a_string_the_classifiers_still_read() {
        let err = LlmError::http(413, "request too large", None);
        assert_eq!(
            llm_http_status(&err.to_string()),
            Some(413)
        );
    }

    #[test]
    fn a_trace_id_is_carried_and_rendered() {
        let err = LlmError::http(500, "upstream broke", None).with_trace_id(Some("abc-123"));
        assert_eq!(err.trace_id(), Some("abc-123"));
        assert_eq!(
            err.to_string(),
            "llm http status 500: upstream broke [trace abc-123]"
        );
        // The typed channel survives the same suffix the retry layer parses
        // around: a status classifier must still find the code.
        assert_eq!(
            llm_http_status(&err.to_string()),
            Some(500)
        );
    }

    #[test]
    fn no_trace_id_leaves_the_message_untouched() {
        let plain = LlmError::http(429, "slow down", Some(Duration::from_secs(7)));
        let expected = "llm http status 429: slow down (retry-after 7s)";
        assert_eq!(plain.to_string(), expected);
        for absent in [None, Some(""), Some("   ")] {
            let err = LlmError::http(429, "slow down", Some(Duration::from_secs(7)))
                .with_trace_id(absent);
            assert_eq!(err.to_string(), expected, "absent: {absent:?}");
            assert_eq!(err.trace_id(), None);
        }
    }

    #[test]
    fn a_trace_id_is_trimmed() {
        let err = LlmError::http(503, "busy", None).with_trace_id(Some("  t-9  "));
        assert_eq!(err.trace_id(), Some("t-9"));
        assert!(err.to_string().ends_with("[trace t-9]"));
    }
}

pub fn llm_http_status(error: &str) -> Option<u16> {
    let rest = error.strip_prefix("llm http status ")?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}
