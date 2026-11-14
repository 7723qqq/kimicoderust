use std::error::Error;
use std::fmt;

use crate::wire::record::WireAsyncError;
use kimi_core::primitives::errors::Error2;
use kimi_core::primitives::errors::domains::config::CONFIG_INVALID;

pub use self::client_remote_config::*;
use super::client_shared::{
    McpClassifiedStartupError, McpFailureKind, classify_mcp_discovery_error,
};
use super::config_schema::{McpRemoteServerConfig, McpServerConfig, McpTransport};

pub mod client_remote_config {
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct UnauthorizedError {
        pub message: String,
    }

    impl UnauthorizedError {
        pub fn new(message: impl Into<String>) -> Self {
            Self {
                message: message.into(),
            }
        }
    }

    impl std::fmt::Display for UnauthorizedError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.message)
        }
    }

    impl std::error::Error for UnauthorizedError {}

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct SseError {
        pub code: Option<u16>,
        pub message: String,
    }

    impl SseError {
        pub fn new(code: Option<u16>, message: impl Into<String>) -> Self {
            Self {
                code,
                message: message.into(),
            }
        }
    }

    impl std::fmt::Display for SseError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.message)
        }
    }

    impl std::error::Error for SseError {}

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct StreamableHTTPError {
        pub code: Option<u16>,
        pub message: String,
    }

    impl StreamableHTTPError {
        pub fn new(code: Option<u16>, message: impl Into<String>) -> Self {
            Self {
                code,
                message: message.into(),
            }
        }
    }

    impl std::fmt::Display for StreamableHTTPError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.message)
        }
    }

    impl std::error::Error for StreamableHTTPError {}
}

pub fn build_mcp_remote_headers(
    config: &McpRemoteServerConfig,
    env_lookup: impl Fn(&str) -> Option<String>,
) -> Result<Option<std::collections::HashMap<String, String>>, WireAsyncError> {
    let mut headers = config.headers.clone().unwrap_or_default();
    if let Some(name) = &config.bearer_token_env_var {
        let token = env_lookup(name).filter(|token| !token.is_empty());
        let Some(token) = token else {
            return Err(Box::new(Error2::new(
                CONFIG_INVALID,
                format!(
                    "MCP {} bearer token env var \"{name}\" is not set or is empty",
                    config.transport.as_str().to_uppercase()
                ),
            )));
        };
        headers.retain(|key, _| key.to_lowercase() != "authorization");
        headers.insert("Authorization".to_string(), format!("Bearer {token}"));
    }
    Ok((!headers.is_empty()).then_some(headers))
}

pub fn is_remote_mcp_config(config: &McpServerConfig) -> bool {
    matches!(config.transport, McpTransport::Http | McpTransport::Sse)
}

pub fn classify_remote_startup_error(
    config: &McpRemoteServerConfig,
    error: WireAsyncError,
) -> WireAsyncError {
    if !is_auth_rejection_status(error.as_ref()) {
        return error;
    }
    let kind = if has_static_mcp_credentials(config) {
        McpFailureKind::AuthStatic
    } else {
        McpFailureKind::AuthOauth
    };
    Box::new(McpClassifiedStartupError::new(
        kind,
        error.to_string(),
        Some(error),
    ))
}

pub fn classify_remote_discovery_error(
    config: &McpRemoteServerConfig,
    error: WireAsyncError,
) -> WireAsyncError {
    if is_auth_rejection_status(error.as_ref()) {
        classify_remote_startup_error(config, error)
    } else {
        classify_mcp_discovery_error(error)
    }
}

fn has_static_mcp_credentials(config: &McpRemoteServerConfig) -> bool {
    config.bearer_token_env_var.is_some()
        || (config.headers.is_some() && config.auth.is_none())
}

fn is_auth_rejection_status(error: &(dyn Error + 'static)) -> bool {
    if error.downcast_ref::<UnauthorizedError>().is_some() {
        return true;
    }
    if let Some(error) = error.downcast_ref::<StreamableHTTPError>() {
        return matches!(error.code, Some(401 | 403));
    }
    if let Some(error) = error.downcast_ref::<SseError>() {
        return matches!(error.code, Some(401 | 403));
    }
    false
}

