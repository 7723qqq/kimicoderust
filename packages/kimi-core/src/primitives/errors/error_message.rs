use std::error::Error;

use super::Error2;

pub fn to_error_message(error: &(dyn Error + 'static), verbose: bool) -> String {
    if let Some(coded) = error.downcast_ref::<Error2>() {
        let base = format!("[{}] {}", coded.code(), coded.message());
        if verbose && let Some(details) = coded.details() {
            return format!(
                "{base} {}",
                serde_json::to_string(details).unwrap_or_default()
            );
        }
        return base;
    }
    let message = error.to_string();
    let base = if message.is_empty() {
        "Error".to_string()
    } else {
        message
    };
    if verbose && let Some(cause) = error.source() {
        return format!("{base} (caused by: {})", to_error_message(cause, false));
    }
    base
}

#[cfg(test)]
mod tests {
    use std::fmt;

    use serde_json::{Map, Value};

    use super::*;
    use crate::primitives::errors::Error2Options;

    #[derive(Debug)]
    struct PlainError {
        message: &'static str,
        cause: Option<Box<dyn Error + Send + Sync>>,
    }

    impl fmt::Display for PlainError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.message)
        }
    }

    impl Error for PlainError {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            self.cause
                .as_ref()
                .map(|cause| cause.as_ref() as &(dyn Error + 'static))
        }
    }

    #[test]
    fn coded_errors_include_the_code_and_optional_details() {
        let error = Error2::new("request.invalid", "bad request");
        assert_eq!(
            to_error_message(&error, false),
            "[request.invalid] bad request"
        );
        assert_eq!(
            to_error_message(&error, true),
            "[request.invalid] bad request"
        );

        let details = Map::from_iter([("field".to_string(), Value::from("query"))]);
        let error = Error2::with_options(
            "request.invalid",
            "bad request",
            Error2Options {
                details: Some(details),
                ..Error2Options::default()
            },
        );
        assert_eq!(
            to_error_message(&error, true),
            "[request.invalid] bad request {\"field\":\"query\"}"
        );
    }

    #[test]
    fn verbose_plain_errors_include_only_the_direct_cause_chain() {
        let inner = PlainError {
            message: "inner",
            cause: None,
        };
        let outer = PlainError {
            message: "outer",
            cause: Some(Box::new(inner)),
        };
        assert_eq!(to_error_message(&outer, false), "outer");
        assert_eq!(to_error_message(&outer, true), "outer (caused by: inner)");
    }

    #[test]
    fn an_empty_plain_error_uses_the_default_error_name() {
        let error = PlainError {
            message: "",
            cause: None,
        };
        assert_eq!(to_error_message(&error, false), "Error");
    }

    #[test]
    fn coded_errors_do_not_switch_to_the_plain_cause_branch() {
        let inner = PlainError {
            message: "inner",
            cause: None,
        };
        let error = Error2::new("internal", "outer").with_cause(Box::new(inner));
        assert_eq!(to_error_message(&error, true), "[internal] outer");
    }
}
