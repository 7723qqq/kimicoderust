use std::error::Error;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    String(String),
    Number(serde_json::Number),
}

#[derive(Debug, Clone, PartialEq)]
pub struct JsonRpcRequest {
    pub id: RequestId,
    pub method: String,
    pub params: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct JsonRpcNotification {
    pub method: String,
    pub params: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct JsonRpcResponse {
    pub id: RequestId,
    pub result: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcErrorBody {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct JsonRpcErrorResponse {
    pub id: RequestId,
    pub error: JsonRpcErrorBody,
}

#[derive(Debug, Clone, PartialEq)]
pub enum JsonRpcMessage {
    Request(JsonRpcRequest),
    Notification(JsonRpcNotification),
    Response(JsonRpcResponse),
    ErrorResponse(JsonRpcErrorResponse),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolViolationError {
    message: String,
}

impl ProtocolViolationError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ProtocolViolationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for ProtocolViolationError {}

pub fn classify_message(value: Value) -> Result<JsonRpcMessage, ProtocolViolationError> {
    let Some(value) = value.as_object() else {
        return Err(ProtocolViolationError::new("message must be a JSON object"));
    };
    if value.contains_key("method") {
        let method = value
            .get("method")
            .and_then(Value::as_str)
            .ok_or_else(|| ProtocolViolationError::new("method must be a string"))?
            .to_string();
        let params = value.get("params").cloned();
        if value.contains_key("id") {
            return Ok(JsonRpcMessage::Request(JsonRpcRequest {
                id: require_request_id(value.get("id").unwrap())?,
                method,
                params,
            }));
        }
        return Ok(JsonRpcMessage::Notification(JsonRpcNotification {
            method,
            params,
        }));
    }
    if value.contains_key("result") {
        let id = value
            .get("id")
            .ok_or_else(|| ProtocolViolationError::new("response must carry an id"))?;
        return Ok(JsonRpcMessage::Response(JsonRpcResponse {
            id: require_request_id(id)?,
            result: value.get("result").cloned().unwrap_or(Value::Null),
        }));
    }
    let id = value
        .get("id")
        .ok_or_else(|| ProtocolViolationError::new("error response must carry an id"))?;
    let error = value.get("error").and_then(Value::as_object);
    let code = error
        .and_then(|error| error.get("code"))
        .and_then(Value::as_i64);
    let message = error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str);
    let (Some(code), Some(message)) = (code, message) else {
        return Err(ProtocolViolationError::new(
            "error must carry a numeric code and a message",
        ));
    };
    Ok(JsonRpcMessage::ErrorResponse(JsonRpcErrorResponse {
        id: require_request_id(id)?,
        error: JsonRpcErrorBody {
            code,
            message: message.to_string(),
            data: error
                .and_then(|error| error.get("data"))
                .cloned(),
        },
    }))
}

pub fn is_request(message: &JsonRpcMessage) -> bool {
    matches!(message, JsonRpcMessage::Request(_))
}

pub fn is_notification(message: &JsonRpcMessage) -> bool {
    matches!(message, JsonRpcMessage::Notification(_))
}

pub fn is_response(message: &JsonRpcMessage) -> bool {
    matches!(message, JsonRpcMessage::Response(_))
}

pub fn is_error_response(message: &JsonRpcMessage) -> bool {
    matches!(message, JsonRpcMessage::ErrorResponse(_))
}

fn require_request_id(value: &Value) -> Result<RequestId, ProtocolViolationError> {
    match value {
        Value::String(value) => Ok(RequestId::String(value.clone())),
        Value::Number(value) => Ok(RequestId::Number(value.clone())),
        _ => Err(ProtocolViolationError::new("id must be a string or number")),
    }
}
