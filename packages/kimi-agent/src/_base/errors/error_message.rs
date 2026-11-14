use super::errors::Error2;
use super::serialize::{RestoredError2, UnknownError, coded_error_shape};

pub fn to_error_message(error: &UnknownError, verbose: bool) -> String {
    if let Some(shape) = coded_error_shape(error) {
        let base = format!("[{}] {}", shape.code, shape.message);
        return if verbose {
            shape
                .details
                .as_ref()
                .and_then(|details| serde_json::to_string(details).ok())
                .map(|details| format!("{base} {details}"))
                .unwrap_or(base)
        } else {
            base
        };
    }
    match error {
        UnknownError::Error(error) => {
            let mut base = error.to_string();
            if base.is_empty() {
                base = if let Some(error) = error.downcast_ref::<Error2>() {
                    error.name.clone()
                } else if let Some(error) = error.downcast_ref::<RestoredError2>() {
                    error.error.name.clone()
                } else {
                    std::any::type_name_of_val(error.as_ref()).to_string()
                };
            }
            if verbose {
                let shared_cause = if let Some(error) = error.downcast_ref::<Error2>() {
                    error.cause.clone()
                } else if let Some(error) = error.downcast_ref::<RestoredError2>() {
                    error.error.cause.clone()
                } else {
                    None
                };
                if let Some(cause) = shared_cause {
                    return format!(
                        "{base} (caused by: {})",
                        to_error_message(&UnknownError::Error(cause), false)
                    );
                }
                if let Some(cause) = error.source() {
                    return format!(
                        "{base} (caused by: {})",
                        to_error_message(&UnknownError::String(cause.to_string()), false)
                    );
                }
            }
            base
        }
        UnknownError::String(error) => error.clone(),
        UnknownError::Json(error) => serde_json::to_string(error)
            .unwrap_or_else(|_| js_string(error)),
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
