use std::error::Error;
use std::fmt;

use serde_json::Value;

use super::messages::JsonRpcErrorBody;
use crate::wire::record::WireAsyncError;

pub const RPC_NOT_FOUND: i64 = -32004;
pub const RPC_INVALID_REQUEST: i64 = -32600;
pub const RPC_METHOD_NOT_FOUND: i64 = -32601;
pub const RPC_INVALID_PARAMS: i64 = -32602;
pub const RPC_INTERNAL_ERROR: i64 = -32603;

#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>, data: Option<Value>) -> Self {
        Self {
            code,
            message: message.into(),
            data,
        }
    }

    pub fn from_error_body(body: JsonRpcErrorBody) -> Self {
        Self::new(body.code, body.message, body.data)
    }

    pub fn to_error_body(&self) -> JsonRpcErrorBody {
        JsonRpcErrorBody {
            code: self.code,
            message: self.message.clone(),
            data: self.data.clone(),
        }
    }
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for RpcError {}

pub fn to_rpc_error(error: WireAsyncError) -> RpcError {
    if let Some(rpc) = error.downcast_ref::<RpcError>() {
        return rpc.clone();
    }
    RpcError::new(RPC_INTERNAL_ERROR, error.to_string(), None)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionClosedError {
    message: String,
}

impl ConnectionClosedError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ConnectionClosedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for ConnectionClosedError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestTimeoutError {
    pub method: String,
    pub timeout_ms: u64,
}

impl RequestTimeoutError {
    pub fn new(method: impl Into<String>, timeout_ms: u64) -> Self {
        Self {
            method: method.into(),
            timeout_ms,
        }
    }
}

impl fmt::Display for RequestTimeoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "request {} timed out after {}ms", self.method, self.timeout_ms)
    }
}

impl Error for RequestTimeoutError {}
