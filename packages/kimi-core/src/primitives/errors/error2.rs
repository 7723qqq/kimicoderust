
use std::error::Error;
use std::fmt;

use serde_json::{Map, Value};

use super::codes::{ErrorCode, NOT_IMPLEMENTED_CODE};

pub type ErrorDetails = Map<String, Value>;

#[derive(Default)]
pub struct Error2Options {
    pub details: Option<ErrorDetails>,
    pub cause: Option<Box<dyn Error + Send + Sync>>,
    pub name: Option<String>,
}

#[derive(Debug)]
pub struct Error2 {
    code: ErrorCode,
    message: String,
    name: Option<String>,
    details: Option<ErrorDetails>,
    retryable: Option<bool>,
    status_code: Option<i64>,
    cause: Option<Box<dyn Error + Send + Sync>>,
}

impl Error2 {
    pub fn new(code: impl Into<ErrorCode>, message: impl Into<String>) -> Self {
        Self::with_options(code, message, Error2Options::default())
    }

    pub fn with_options(
        code: impl Into<ErrorCode>,
        message: impl Into<String>,
        options: Error2Options,
    ) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            name: options.name,
            details: options.details,
            retryable: None,
            status_code: None,
            cause: options.cause,
        }
    }

    pub fn code(&self) -> &ErrorCode {
        &self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn name(&self) -> &str {
        self.name.as_deref().unwrap_or("Error2")
    }

    pub fn details(&self) -> Option<&ErrorDetails> {
        self.details.as_ref()
    }

    pub fn retryable(&self) -> Option<bool> {
        self.retryable
    }

    pub fn status_code(&self) -> Option<i64> {
        self.status_code
    }

    pub fn cause(&self) -> Option<&(dyn Error + Send + Sync)> {
        self.cause.as_deref()
    }

    pub fn with_details(mut self, details: ErrorDetails) -> Self {
        self.details = Some(details);
        self
    }

    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    pub fn with_retryable(mut self, retryable: bool) -> Self {
        self.retryable = Some(retryable);
        self
    }

    pub fn with_status_code(mut self, status_code: i64) -> Self {
        self.status_code = Some(status_code);
        self
    }

    pub fn with_cause(mut self, cause: Box<dyn Error + Send + Sync>) -> Self {
        self.cause = Some(cause);
        self
    }
}

impl fmt::Display for Error2 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for Error2 {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        let cause = self.cause.as_deref()?;
        Some(cause)
    }
}

pub fn is_error2(error: &(dyn Error + 'static)) -> bool {
    error.is::<Error2>()
}

pub fn unwrap_error_cause<'a>(error: &'a (dyn Error + 'static)) -> &'a (dyn Error + 'static) {
    let mut current = error;
    loop {
        let Some(coded) = current.downcast_ref::<Error2>() else {
            return current;
        };
        let Some(cause) = coded.source() else {
            return current;
        };
        current = cause;
    }
}

pub fn not_implemented(feature: Option<&str>) -> Error2 {
    let message = match feature {
        Some(feature) => format!("Not implemented: {feature}"),
        None => "Not implemented".to_string(),
    };
    Error2::new(NOT_IMPLEMENTED_CODE, message).with_name("NotImplementedError")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::primitives::errors::{error_info, is_error_code};

    #[test]
    fn a_coded_error_keeps_its_code_and_default_name() {
        let error = Error2::new("internal", "boom");
        assert_eq!(error.code().as_str(), "internal");
        assert_eq!(error.message(), "boom");
        assert_eq!(error.name(), "Error2");
        assert_eq!(error.to_string(), "boom");
    }

    #[test]
    fn not_implemented_matches_the_reference_shape() {
        let error = not_implemented(Some("streaming"));
        assert_eq!(error.code().as_str(), "not_implemented");
        assert_eq!(error.message(), "Not implemented: streaming");
        assert_eq!(error.name(), "NotImplementedError");
        assert!(is_error_code(error.code().as_str()));
        assert_eq!(error.retryable(), None);
        assert!(!error_info(error.code().as_str()).retryable);

        assert_eq!(not_implemented(None).message(), "Not implemented");
    }

    #[test]
    fn unwrap_walks_only_error2_links() {
        let inner = Error2::new("internal", "inner");
        let outer = Error2::new("internal", "outer").with_cause(Box::new(inner));
        assert_eq!(unwrap_error_cause(&outer).to_string(), "inner");

        let plain = std::io::Error::other("plain");
        assert_eq!(unwrap_error_cause(&plain).to_string(), "plain");
    }

    #[test]
    fn source_exposes_the_cause() {
        let error =
            Error2::new("internal", "outer").with_cause(Box::new(Error2::new("internal", "inner")));
        assert_eq!(
            error.source().map(ToString::to_string).as_deref(),
            Some("inner")
        );
    }
}
