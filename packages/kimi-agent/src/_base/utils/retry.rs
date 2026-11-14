use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use futures_util::FutureExt;

use super::abort::{AbortSignal, abortable};
use crate::_base::errors::errors::{Error2, SharedError};
use crate::_base::errors::serialize::{RestoredError2, UnknownError};

pub const DEFAULT_MAX_RETRY_ATTEMPTS: usize = 10;
const BASE_DELAY_MS: f64 = 500.0;
const MAX_DELAY_MS: f64 = 32_000.0;
const RETRY_FACTOR: f64 = 2.0;
const JITTER_FACTOR: f64 = 0.25;
const MAX_CAUSE_DEPTH: usize = 3;

pub fn retry_backoff_delay(attempt_index: u32) -> f64 {
    let base = (BASE_DELAY_MS * RETRY_FACTOR.powi(attempt_index as i32)).min(MAX_DELAY_MS);
    base + fastrand::f64() * JITTER_FACTOR * base
}

pub fn retry_backoff_delays(max_attempts: usize) -> Vec<f64> {
    (0..max_attempts.saturating_sub(1))
        .map(|index| retry_backoff_delay(index as u32))
        .collect()
}

pub fn read_retry_after_ms(error: &UnknownError) -> Option<f64> {
    let UnknownError::Json(serde_json::Value::Object(object)) = error else {
        return None;
    };
    object
        .get("retryAfterMs")
        .and_then(serde_json::Value::as_f64)
        .filter(|value| *value > 0.0)
}

pub trait RetrySleepTimers: Send + Sync {
    fn set_timeout(&self, callback: Box<dyn FnOnce() + Send>, timeout_ms: f64);
}

pub fn sleep_for_retry(
    delay_ms: f64,
    signal: Option<AbortSignal>,
    timers: Option<Arc<dyn RetrySleepTimers>>,
) -> BoxFuture<'static, Result<(), SharedError>> {
    if signal.as_ref().is_some_and(AbortSignal::aborted) {
        return abortable(async {}, signal.unwrap());
    }
    let sleep = async move {
        if let Some(timers) = timers {
            let (sender, receiver) = tokio::sync::oneshot::channel();
            timers.set_timeout(
                Box::new(move || {
                    let _ = sender.send(());
                }),
                delay_ms,
            );
            if receiver.await.is_err() {
                std::future::pending::<()>().await;
            }
        } else {
            tokio::time::sleep(Duration::from_secs_f64((delay_ms / 1000.0).max(0.0))).await;
        }
    };
    if let Some(signal) = signal {
        abortable(sleep, signal)
    } else {
        async move {
            sleep.await;
            Ok(())
        }
        .boxed()
    }
}

pub struct RetryErrorFields {
    pub error_name: String,
    pub error_message: String,
    pub status_code: Option<f64>,
}

pub fn retry_error_fields(error: &UnknownError) -> RetryErrorFields {
    RetryErrorFields {
        error_name: match error {
            UnknownError::Error(error) => {
                if let Some(error) = error.downcast_ref::<Error2>() {
                    error.name.clone()
                } else if let Some(error) = error.downcast_ref::<RestoredError2>() {
                    error.error.name.clone()
                } else if error.is::<super::abort::AbortError>()
                    || error.is::<super::abort::UserCancellationError>()
                {
                    "AbortError".to_string()
                } else {
                    std::any::type_name_of_val(error.as_ref()).to_string()
                }
            }
            UnknownError::String(_) => "string".to_string(),
            UnknownError::Json(serde_json::Value::String(_)) => "string".to_string(),
            UnknownError::Json(serde_json::Value::Bool(_)) => "boolean".to_string(),
            UnknownError::Json(serde_json::Value::Number(_)) => "number".to_string(),
            UnknownError::Json(_) => "object".to_string(),
        },
        error_message: message_with_cause(error),
        status_code: maybe_status_code(error),
    }
}

fn message_with_cause(error: &UnknownError) -> String {
    let message = match error {
        UnknownError::Error(error) => error.to_string(),
        UnknownError::String(error) => error.clone(),
        UnknownError::Json(error) => error.to_string(),
    };
    let parts = describe_cause_chain(error, &message);
    if parts.is_empty() {
        message
    } else if message.is_empty() {
        parts.join("; ")
    } else {
        format!("{message} ({})", parts.join("; "))
    }
}

fn describe_cause_chain(error: &UnknownError, message: &str) -> Vec<String> {
    let mut parts = Vec::new();
    match error {
        UnknownError::Error(error) => {
            let mut current = error.source();
            while parts.len() < MAX_CAUSE_DEPTH {
                let Some(cause) = current else {
                    break;
                };
                let cause_message = cause.to_string();
                if !cause_message.is_empty() {
                    let text = if let Some(coded) = cause.downcast_ref::<Error2>() {
                        if coded.code.is_empty() {
                            format!("{}: {cause_message}", coded.name)
                        } else {
                            format!("{}: {cause_message}", coded.code)
                        }
                    } else {
                        format!("{}: {cause_message}", std::any::type_name_of_val(cause))
                    };
                    if !message.contains(&cause_message) && !parts.contains(&text) {
                        parts.push(text);
                    }
                }
                current = cause.source();
            }
        }
        UnknownError::Json(value) => {
            let mut current = value.get("cause");
            while parts.len() < MAX_CAUSE_DEPTH {
                let Some(cause) = current else {
                    break;
                };
                if cause.is_null() {
                    break;
                }
                let cause_message = cause.to_string();
                if !cause_message.is_empty()
                    && !message.contains(&cause_message)
                    && !parts.contains(&cause_message)
                {
                    parts.push(cause_message);
                }
                current = cause.get("cause");
            }
        }
        UnknownError::String(_) => {}
    }
    parts
}

fn maybe_status_code(error: &UnknownError) -> Option<f64> {
    match error {
        UnknownError::Error(error) => {
            if let Some(error) = error.downcast_ref::<RestoredError2>() {
                return error.status_code;
            }
            error.downcast_ref::<Error2>().and_then(|error| {
                error
                    .details
                    .as_ref()?
                    .get("statusCode")?
                    .as_f64()
            })
        }
        UnknownError::Json(serde_json::Value::Object(object)) => object
            .get("statusCode")
            .and_then(serde_json::Value::as_f64)
            .or_else(|| {
                object
                    .get("details")?
                    .get("statusCode")?
                    .as_f64()
            }),
        _ => None,
    }
}
