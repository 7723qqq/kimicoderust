
use std::error::Error;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::codes::{ErrorCode, INTERNAL_CODE, error_info, is_error_code};
use super::error2::{Error2, Error2Options, ErrorDetails};

pub const MAX_CAUSE_DEPTH: usize = 8;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorPayload {
    pub code: ErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<ErrorDetails>,
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<Box<ErrorPayload>>,
}

pub type KimiErrorPayload = ErrorPayload;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PayloadOptions {
    pub details: Option<ErrorDetails>,
    pub name: Option<String>,
}

pub fn make_error_payload(
    code: impl Into<ErrorCode>,
    message: impl Into<String>,
    options: PayloadOptions,
) -> ErrorPayload {
    let code = code.into();
    ErrorPayload {
        retryable: error_info(code.as_str()).retryable,
        code,
        message: message.into(),
        name: options.name,
        details: options.details,
        cause: None,
    }
}

pub fn is_coded_error(error: &(dyn Error + 'static)) -> bool {
    error
        .downcast_ref::<Error2>()
        .is_some_and(|coded| is_error_code(coded.code().as_str()))
}

pub fn to_error_payload(error: &(dyn Error + 'static)) -> ErrorPayload {
    to_error_payload_at_depth(error, 0)
}

pub use to_error_payload as to_kimi_error_payload;

fn to_error_payload_at_depth(error: &(dyn Error + 'static), depth: usize) -> ErrorPayload {
    let payload = to_shallow_error_payload(error);
    if depth >= MAX_CAUSE_DEPTH {
        return payload;
    }
    let Some(cause) = error.source() else {
        return payload;
    };
    ErrorPayload {
        cause: Some(Box::new(to_error_payload_at_depth(cause, depth + 1))),
        ..payload
    }
}

fn to_shallow_error_payload(error: &(dyn Error + 'static)) -> ErrorPayload {
    if let Some(coded) = error.downcast_ref::<Error2>() {
        if is_error_code(coded.code().as_str()) {
            return ErrorPayload {
                code: coded.code().clone(),
                message: coded.message().to_string(),
                name: Some(coded.name().to_string()),
                details: details_for_error_payload(coded),
                retryable: coded
                    .retryable()
                    .unwrap_or_else(|| error_info(coded.code().as_str()).retryable),
                cause: None,
            };
        }
    }
    // The reference reads `error.message`; `Display` is that value in Rust.
    make_error_payload(INTERNAL_CODE, error.to_string(), PayloadOptions::default())
}

fn details_for_error_payload(error: &Error2) -> Option<ErrorDetails> {
    let details = error.details()?;
    if !details.contains_key("statusCode") {
        return Some(details.clone());
    }
    if let Some(direct) = error.status_code() {
        if details.get("statusCode").and_then(Value::as_i64) == Some(direct) {
            return Some(details.clone());
        }
    }
    let mut remaining = details.clone();
    remaining.remove("statusCode");
    Some(remaining)
}

pub fn from_error_payload(payload: &ErrorPayload) -> Error2 {
    let error = Error2::with_options(
        payload.code.clone(),
        payload.message.clone(),
        Error2Options {
            name: payload.name.clone(),
            details: payload.details.clone(),
            cause: payload
                .cause
                .as_deref()
                .map(|cause| Box::new(from_error_payload(cause)) as Box<dyn Error + Send + Sync>),
        },
    )
    .with_retryable(payload.retryable);

    let status_code = payload
        .details
        .as_ref()
        .and_then(|details| details.get("statusCode"))
        .and_then(Value::as_i64);
    match (status_code, payload.name.as_deref()) {
        (Some(status_code), Some(name)) if is_api_error_name(name) => {
            error.with_status_code(status_code)
        }
        _ => error,
    }
}

fn is_api_error_name(name: &str) -> bool {
    matches!(
        name,
        "APIStatusError"
            | "APIContextOverflowError"
            | "APIRequestTooLargeError"
            | "APIProviderRateLimitError"
            | "APIProviderQuotaExhaustedError"
            | "APIProviderOverloadedError"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::primitives::errors::not_implemented;
    use serde_json::json;

    fn details(value: Value) -> ErrorDetails {
        match value {
            Value::Object(map) => map,
            _ => panic!("details must be an object"),
        }
    }

    #[test]
    fn make_error_payload_takes_retryable_from_the_registry() {
        let payload = make_error_payload("internal", "boom", PayloadOptions::default());
        assert_eq!(payload.code.as_str(), "internal");
        assert_eq!(payload.message, "boom");
        assert_eq!(payload.retryable, false);
        assert_eq!(payload.name, None);
        assert_eq!(payload.details, None);
    }

    #[test]
    fn a_coded_error_keeps_code_name_details_and_retryable() {
        let error = Error2::new("internal", "boom")
            .with_name("MyError")
            .with_details(details(json!({"attempt": 2})))
            .with_retryable(true);
        let payload = to_error_payload(&error);
        assert_eq!(payload.code.as_str(), "internal");
        assert_eq!(payload.message, "boom");
        assert_eq!(payload.name.as_deref(), Some("MyError"));
        assert_eq!(payload.details, Some(details(json!({"attempt": 2}))));
        // An explicit retryable wins over the registry (`serialize.ts:73`).
        assert!(payload.retryable);
    }

    #[test]
    fn retryable_falls_back_to_the_registry() {
        let error = Error2::new("internal", "boom");
        assert!(!to_error_payload(&error).retryable);
    }

    #[test]
    fn an_unregistered_code_is_reported_as_internal() {
        let error = Error2::new("not.a.registered.code", "boom");
        assert!(!is_coded_error(&error));
        let payload = to_error_payload(&error);
        assert_eq!(payload.code.as_str(), "internal");
        assert_eq!(payload.message, "boom");
    }

    #[test]
    fn a_plain_error_is_reported_as_internal() {
        let error = std::io::Error::other("disk full");
        let payload = to_error_payload(&error);
        assert_eq!(payload.code.as_str(), "internal");
        assert_eq!(payload.message, "disk full");
        assert_eq!(payload.name, None);
        assert!(!payload.retryable);
    }

    #[test]
    fn the_cause_chain_is_nested_and_capped() {
        let mut error = Error2::new("internal", "0");
        for depth in 1..12 {
            error = Error2::new("internal", depth.to_string()).with_cause(Box::new(error));
        }
        let payload = to_error_payload(&error);
        let mut levels = 1;
        let mut current = &payload;
        while let Some(cause) = current.cause.as_deref() {
            levels += 1;
            current = cause;
        }
        // Depth 0 through 8 inclusive (`serialize.ts:23,56`).
        assert_eq!(levels, MAX_CAUSE_DEPTH + 1);
    }

    #[test]
    fn a_duplicate_status_code_survives_and_a_stale_one_is_stripped() {
        let kept = Error2::new("internal", "boom")
            .with_status_code(429)
            .with_details(details(json!({"statusCode": 429, "requestId": "r"})));
        assert_eq!(
            details_for_error_payload(&kept),
            Some(details(json!({"statusCode": 429, "requestId": "r"})))
        );

        let stripped = Error2::new("internal", "boom")
            .with_details(details(json!({"statusCode": 429, "requestId": "r"})));
        assert_eq!(
            details_for_error_payload(&stripped),
            Some(details(json!({"requestId": "r"})))
        );

        let mismatched = Error2::new("internal", "boom")
            .with_status_code(500)
            .with_details(details(json!({"statusCode": 429})));
        assert_eq!(
            details_for_error_payload(&mismatched),
            Some(details(json!({})))
        );
    }

    #[test]
    fn a_payload_round_trips_including_retryable_and_cause() {
        let original = Error2::new("internal", "outer")
            .with_name("Outer")
            .with_cause(Box::new(not_implemented(Some("x"))));
        let payload = to_error_payload(&original);
        let restored = from_error_payload(&payload);
        assert_eq!(restored.code().as_str(), "internal");
        assert_eq!(restored.message(), "outer");
        assert_eq!(restored.name(), "Outer");
        assert_eq!(restored.retryable(), Some(false));
        let cause = restored.cause().expect("cause");
        assert_eq!(cause.to_string(), "Not implemented: x");
    }

    #[test]
    fn an_api_status_error_restores_its_status_code() {
        let payload = ErrorPayload {
            code: "provider.api_error".into(),
            message: "bad request".into(),
            name: Some("APIStatusError".into()),
            details: Some(details(json!({"statusCode": 400}))),
            retryable: false,
            cause: None,
        };
        let error = from_error_payload(&payload);
        assert_eq!(error.status_code(), Some(400));

        let other_name = ErrorPayload {
            name: Some("SomethingElse".into()),
            ..payload
        };
        assert_eq!(from_error_payload(&other_name).status_code(), None);
    }

    #[test]
    fn the_serialized_shape_omits_absent_details_and_cause() {
        let payload = to_error_payload(&Error2::new("internal", "boom"));
        let json = serde_json::to_value(&payload).expect("serialize");
        assert_eq!(
            json,
            json!({
                "code": "internal",
                "message": "boom",
                // `Error2` sets `name` to 'Error2' in the reference too
                // (`errors.ts:43`), so a coded error always carries it.
                "name": "Error2",
                "retryable": false,
            })
        );
    }
}
