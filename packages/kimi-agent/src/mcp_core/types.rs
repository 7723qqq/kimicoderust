use std::collections::HashMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::_base::utils::abort::AbortSignal;
use crate::mcp_core::errors::MCP_STARTUP_FAILED;
use crate::wire::record::WireAsyncError;
use kimi_core::primitives::errors::Error2;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MCPEmbeddedResourceContents {
    pub uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MCPContentBlock {
    #[serde(rename = "type")]
    pub type_: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<MCPEmbeddedResourceContents>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MCPToolResult {
    pub content: Vec<MCPContentBlock>,
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MCPToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[async_trait]
pub trait MCPClient: Send + Sync {
    async fn list_tools(&self) -> Result<Vec<MCPToolDefinition>, WireAsyncError>;
    async fn call_tool(
        &self,
        name: &str,
        args: HashMap<String, Value>,
        signal: Option<AbortSignal>,
    ) -> Result<MCPToolResult, WireAsyncError>;
    async fn ping(&self, signal: Option<AbortSignal>) -> Result<(), WireAsyncError>;
}

pub fn assert_mcp_input_schema(
    tool_name: &str,
    input_schema: &Value,
) -> Result<Map<String, Value>, WireAsyncError> {
    input_schema.as_object().cloned().ok_or_else(|| {
        Box::new(Error2::new(
            MCP_STARTUP_FAILED,
            format!("Invalid inputSchema for MCP tool \"{tool_name}\": schema must be a JSON object"),
        )) as WireAsyncError
    })
}
