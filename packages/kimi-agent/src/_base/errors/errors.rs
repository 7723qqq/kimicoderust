use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use super::codes::{CORE_ERRORS, ErrorCode};

#[derive(Clone, Debug)]
pub struct ExpectedError {
    pub message: String,
    pub is_expected: bool,
}

impl ExpectedError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            is_expected: true,
        }
    }
}

impl fmt::Display for ExpectedError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ExpectedError {}

#[derive(Clone, Debug)]
pub struct ErrorNoTelemetry {
    pub message: String,
    pub stack: Option<String>,
}

impl ErrorNoTelemetry {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            stack: None,
        }
    }

    pub fn from_error(error: &dyn Error) -> Self {
        Self {
            message: error.to_string(),
            stack: Some(std::backtrace::Backtrace::capture().to_string()),
        }
    }

    pub fn is_error_no_telemetry(error: &dyn Error) -> bool {
        error.is::<Self>()
    }
}

impl fmt::Display for ErrorNoTelemetry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ErrorNoTelemetry {}

#[derive(Clone, Debug)]
pub struct BugIndicatingError {
    pub message: String,
}

impl BugIndicatingError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn from_option(message: Option<String>) -> Self {
        Self::new(message.unwrap_or_else(|| "An unexpected bug occurred.".to_string()))
    }
}

impl Default for BugIndicatingError {
    fn default() -> Self {
        Self::from_option(None)
    }
}

impl fmt::Display for BugIndicatingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for BugIndicatingError {}

pub type SharedError = Arc<dyn Error + Send + Sync>;

#[derive(Default)]
pub struct Error2Options {
    pub details: Option<HashMap<String, serde_json::Value>>,
    pub cause: Option<SharedError>,
    pub name: Option<String>,
}

pub struct Error2 {
    pub code: ErrorCode,
    pub message: String,
    pub details: Option<HashMap<String, serde_json::Value>>,
    pub cause: Option<SharedError>,
    pub name: String,
}

impl Error2 {
    pub fn new(code: ErrorCode, message: impl Into<String>, options: Error2Options) -> Self {
        Self {
            code,
            message: message.into(),
            details: options.details,
            cause: options.cause,
            name: options.name.unwrap_or_else(|| "Error2".to_string()),
        }
    }
}

impl fmt::Debug for Error2 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct(&self.name)
            .field("code", &self.code)
            .field("message", &self.message)
            .field("details", &self.details)
            .finish_non_exhaustive()
    }
}

impl fmt::Display for Error2 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for Error2 {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.cause
            .as_deref()
            .map(|cause| cause as &(dyn Error + 'static))
    }
}

pub fn is_error2(error: &dyn Error) -> bool {
    error.is::<Error2>()
}

pub fn unwrap_error_cause(mut error: SharedError) -> SharedError {
    loop {
        match error.downcast::<Error2>() {
            Ok(current) => {
                if let Some(cause) = current.cause.clone() {
                    error = cause;
                } else {
                    return current;
                }
            }
            Err(current) => return current,
        }
    }
}

pub struct NotImplementedError(Error2);

impl NotImplementedError {
    pub fn new(feature: Option<&str>) -> Self {
        let message = feature
            .filter(|feature| !feature.is_empty())
            .map(|feature| format!("Not implemented: {feature}"))
            .unwrap_or_else(|| "Not implemented".to_string());
        Self(Error2::new(
            CORE_ERRORS.codes.get("NOT_IMPLEMENTED").unwrap().clone(),
            message,
            Error2Options {
                name: Some("NotImplementedError".to_string()),
                ..Error2Options::default()
            },
        ))
    }
}

impl fmt::Debug for NotImplementedError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, formatter)
    }
}

impl fmt::Display for NotImplementedError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, formatter)
    }
}

impl Error for NotImplementedError {}
