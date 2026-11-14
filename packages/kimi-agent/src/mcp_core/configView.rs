use serde::{Deserialize, Serialize};

use super::config_schema::{McpServerConfig, McpTransport};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpServerConfigView {
    #[serde(flatten)]
    pub config: McpServerConfig,
    #[serde(rename = "envKeys", default, skip_serializing_if = "Option::is_none")]
    pub env_keys: Option<Vec<String>>,
    #[serde(rename = "headerKeys", default, skip_serializing_if = "Option::is_none")]
    pub header_keys: Option<Vec<String>>,
}

pub fn to_mcp_server_config_view(config: McpServerConfig) -> McpServerConfigView {
    let mut config = config;
    let env_keys = if config.transport == McpTransport::Stdio {
        config.env.take().map(|env| {
            let mut keys = env.keys().cloned().collect::<Vec<_>>();
            keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            keys
        })
    } else {
        None
    };
    let header_keys = if config.transport != McpTransport::Stdio {
        config.headers.take().map(|headers| {
            let mut keys = headers.keys().cloned().collect::<Vec<_>>();
            keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            keys
        })
    } else {
        None
    };
    if config.transport == McpTransport::Stdio {
        config.headers = None;
    } else {
        config.env = None;
    }
    McpServerConfigView {
        config,
        env_keys,
        header_keys,
    }
}
