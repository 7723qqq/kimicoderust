use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::codes::{CORE_ERRORS, ErrorCode, error_info, is_error_code};
use super::errors::{Error2, Error2Options, SharedError};

const MAX_CAUSE_DEPTH: usize = 8;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ErrorPayload {
    pub code: ErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<HashMap<String, serde_json::Value>>,
    pub retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause: Option<Box<ErrorPayload>>,
}

pub type KimiErrorPayload = ErrorPayload;

#[derive(Clone, Debug)]
pub struct CodedErrorShape {
    pub code: ErrorCode,
    pub message: String,
    pub name: Option<String>,
    pub details: Option<HashMap<String, serde_json::Value>>,
    pub retryable: Option<bool>,
    pub status_code: Option<f64>,
    pub cause: Option<Box<UnknownError>>,
}

#[derive(Clone)]
pub enum UnknownError {
    Error(SharedError),
    String(String),
    Json(serde_json::Value),
}

impl fmt::Debug for UnknownError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Error(error) => formatter.write_str(&error.to_string()),
            Self::String(error) => formatter.write_str(error),
            Self::Json(error) => fmt::Debug::fmt(error, formatter),
        }
    }
}

pub fn coded_error_shape(error: &UnknownError) -> Option<CodedErrorShape> {
    match error {
        UnknownError::Error(error) => {
            if let Some(restored) = error.downcast_ref::<RestoredError2>() {
                if !is_error_code(&restored.error.code) {
                    return None;
                }
                return Some(CodedErrorShape {
                    code: restored.error.code.clone(),
                    message: restored.error.message.clone(),
                    name: Some(restored.error.name.clone()),
                    details: restored.error.details.clone(),
                    retryable: Some(restored.retryable),
                    status_code: restored.status_code,
                    cause: restored
                        .error
                        .cause
                        .clone()
                        .map(|cause| Box::new(UnknownError::Error(cause))),
                });
            }
            let error = error.downcast_ref::<Error2>().cloned()?;
            if !is_error_code(&error.code) {
                return None;
            }
            Some(CodedErrorShape {
                code: error.code.clone(),
                message: error.message.clone(),
                name: Some(error.name.clone()),
                details: error.details.clone(),
                retryable: None,
                status_code: None,
                cause: error
                    .cause
                    .clone()
                    .map(|cause| Box::new(UnknownError::Error(cause))),
            })
        }
        UnknownError::Json(serde_json::Value::Object(object)) => {
            let code = object.get("code")?.as_str()?.to_string();
            if !is_error_code(&code) {
                return None;
            }
            Some(CodedErrorShape {
                code,
                message: object
                    .get("message")
                    .map(js_string)
                    .unwrap_or_else(|| "undefined".to_string()),
                name: object
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
                details: object.get("details").and_then(|details| {
                    serde_json::from_value::<HashMap<String, serde_json::Value>>(details.clone()).ok()
                }),
                retryable: object
                    .get("retryable")
                    .and_then(serde_json::Value::as_bool),
                status_code: object
                    .get("statusCode")
                    .and_then(serde_json::Value::as_f64),
                cause: object
                    .get("cause")
                    .cloned()
                    .map(|cause| Box::new(UnknownError::Json(cause))),
            })
        }
        _ => None,
    }
}

pub fn is_coded_error(error: &UnknownError) -> bool {
    coded_error_shape(error).is_some()
}

pub fn make_error_payload(
    code: ErrorCode,
    message: impl Into<String>,
    details: Option<HashMap<String, serde_json::Value>>,
    name: Option<String>,
) -> ErrorPayload {
    ErrorPayload {
        retryable: error_info(&code).retryable,
        code,
        message: message.into(),
        name,
        details,
        cause: None,
    }
}

pub fn to_error_payload(error: &UnknownError) -> ErrorPayload {
    to_error_payload_at_depth(error, 0)
}

fn to_error_payload_at_depth(error: &UnknownError, depth: usize) -> ErrorPayload {
    let (mut payload, cause) = to_shallow_error_payload(error);
    if depth >= MAX_CAUSE_DEPTH {
        return payload;
    }
    payload.cause = cause.map(|cause| Box::new(to_error_payload_at_depth(&cause, depth + 1)));
    payload
}

fn to_shallow_error_payload(error: &UnknownError) -> (ErrorPayload, Option<UnknownError>) {
    if let Some(shape) = coded_error_shape(error) {
        let details = details_for_error_payload(&shape);
        let payload = ErrorPayload {
            retryable: shape
                .retryable
                .unwrap_or_else(|| error_info(&shape.code).retryable),
            code: shape.code,
            message: shape.message,
            name: shape.name,
            details,
            cause: None,
        };
        return (payload, shape.cause.map(|cause| *cause));
    }
    match error {
        UnknownError::Error(error) => (
            make_error_payload(
                CORE_ERRORS.codes.get("INTERNAL").unwrap().clone(),
                error.to_string(),
                None,
                Some(std::any::type_name_of_val(error.as_ref()).to_string()),
            ),
            error.source().map(|_| UnknownError::String(
                error
                    .source()
                    .map(|source| source.to_string())
                    .unwrap_or_default(),
            )),
        ),
        UnknownError::String(error) => (
            make_error_payload(
                CORE_ERRORS.codes.get("INTERNAL").unwrap().clone(),
                error.clone(),
                None,
                None,
            ),
            None,
        ),
        UnknownError::Json(error) => (
            make_error_payload(
                CORE_ERRORS.codes.get("INTERNAL").unwrap().clone(),
                js_string(error),
                None,
                None,
            ),
            None,
        ),
    }
}

fn details_for_error_payload(
    error: &CodedErrorShape,
) -> Option<HashMap<String, serde_json::Value>> {
    let mut details = error.details.clone()?;
    let Some(status) = details.get("statusCode").and_then(serde_json::Value::as_f64) else {
        return Some(details);
    };
    if error.status_code == Some(status) {
        return Some(details);
    }
    details.remove("statusCode");
    Some(details)
}

pub fn to_kimi_error_payload(error: &UnknownError) -> ErrorPayload {
    to_error_payload(error)
}

pub struct RestoredError2 {
    pub error: Error2,
    pub retryable: bool,
    pub status_code: Option<f64>,
}

impl fmt::Debug for RestoredError2 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.error, formatter)
    }
}

impl fmt::Display for RestoredError2 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.error, formatter)
    }
}

impl Error for RestoredError2 {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.error.source()
    }
}

pub fn from_error_payload(payload: ErrorPayload) -> RestoredError2 {
    let status_code = payload
        .details
        .as_ref()
        .and_then(|details| details.get("statusCode"))
        .and_then(serde_json::Value::as_f64)
        .filter(|_| {
            matches!(
                payload.name.as_deref(),
                Some("APIStatusError")
                    | Some("APIContextOverflowError")
                    | Some("APIRequestTooLargeError")
                    | Some("APIProviderRateLimitError")
                    | Some("APIProviderQuotaExhaustedError")
                    | Some("APIProviderOverloadedError")
            )
        });
    let error = Error2::new(
        payload.code,
        payload.message,
        Error2Options {
            details: payload.details,
            cause: payload
                .cause
                .map(|cause| Arc::new(from_error_payload(*cause)) as SharedError),
            name: payload.name,
        },
    );
    RestoredError2 {
        error,
        retryable: payload.retryable,
        status_code,
    }
}

fn js_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(value) => value.to_string(),
        serde_json::Value::Number(value) => value.to_string(),
        serde_json::Value::String(value) => value.clone(),
        serde_json::Value::Array(values) => values
            .iter()
            .map(|value| {
                if value.is_null() {
                    String::new()
                } else {
                    js_string(value)
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        serde_json::Value::Object(_) => "[object Object]".to_string(),
    }
}
