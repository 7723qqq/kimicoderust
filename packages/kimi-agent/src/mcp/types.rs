//! Protocol types for Model Context Protocol (MCP) wire communication.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpTool {
    pub name: String,
    pub description: Option<String>,
    #[serde(rename = "inputSchema", default)]
    pub input_schema: Value,
}

/// MCP content block (per the MCP spec `CallToolResult.content[]`):
/// text carries the string text; image / audio carry a base64 payload with
/// a mime type; resource carries a `resource` blob (uri + optional
/// text/mime). The engine preserves every variant — v2's
/// `convertMCPContentBlock` maps them to model `ContentPart`s, but our
/// `ExecutableToolResult.content: String` can't carry media yet, so non-text
/// blocks fall back to a descriptive notice in the tool output (data is
/// truncated to a preview, never silently dropped).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpContent {
    #[serde(rename = "type")]
    pub content_type: String,
    pub text: Option<String>,
    #[serde(default)]
    pub uri: Option<String>,
    #[serde(rename = "mimeType", default)]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub data: Option<String>,
    #[serde(default)]
    pub resource: Option<Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpToolCallResult {
    pub content: Vec<McpContent>,
    #[serde(rename = "isError", default)]
    pub is_error: bool,
    #[serde(
        rename = "structuredContent",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub structured_content: Option<Value>,
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

pub fn to_mcp_tool_result(result: Value) -> Result<McpToolCallResult, super::errors::McpError> {
    if let Some(object) = result.as_object() {
        if let Some(content) = object.get("content").and_then(Value::as_array) {
            let content =
                serde_json::from_value(Value::Array(content.clone())).map_err(|error| {
                    super::errors::McpError::transport(format!(
                        "Failed to parse tool call result: {error}"
                    ))
                })?;
            return Ok(McpToolCallResult {
                content,
                is_error: object.get("isError").and_then(Value::as_bool) == Some(true),
                structured_content: object.get("structuredContent").cloned(),
                meta: object
                    .get("_meta")
                    .filter(|value| value.is_object())
                    .cloned(),
            });
        }
        if let Some(legacy) = object.get("toolResult") {
            let text = match legacy {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            return Ok(McpToolCallResult {
                content: vec![McpContent {
                    content_type: "text".to_string(),
                    text: Some(text),
                    ..McpContent::default()
                }],
                ..McpToolCallResult::default()
            });
        }
    }
    Ok(McpToolCallResult::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_mcp_tool_deserialization_and_serialization() {
        let raw = json!({
            "name": "read_file",
            "description": "Read content of a file",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" }
                },
                "required": ["path"]
            }
        });

        let tool: McpTool = serde_json::from_value(raw).expect("deserialization failed");
        assert_eq!(tool.name, "read_file");
        assert_eq!(tool.description.as_deref(), Some("Read content of a file"));
        assert_eq!(tool.input_schema["type"], "object");
        assert_eq!(tool.input_schema["properties"]["path"]["type"], "string");
        assert_eq!(tool.input_schema["required"][0], "path");

        // Re-serialize and verify wire key names match MCP protocol specification
        let serialized = serde_json::to_value(&tool).expect("serialization failed");
        assert_eq!(serialized["name"], "read_file");
        assert_eq!(serialized["description"], "Read content of a file");
        assert_eq!(serialized["inputSchema"]["type"], "object");
        assert!(serialized.get("input_schema").is_none());

        // Default inputSchema and optional description when omitted
        let minimal = json!({ "name": "minimal_tool" });
        let min_tool: McpTool = serde_json::from_value(minimal).expect("minimal tool failed");
        assert_eq!(min_tool.name, "minimal_tool");
        assert_eq!(min_tool.description, None);
        assert_eq!(min_tool.input_schema, Value::Null);
    }

    #[test]
    fn test_mcp_content_wire_mapping() {
        let raw_text = json!({
            "type": "text",
            "text": "File content here"
        });
        let content: McpContent = serde_json::from_value(raw_text).expect("deserialization failed");
        assert_eq!(content.content_type, "text");
        assert_eq!(content.text.as_deref(), Some("File content here"));

        let serialized = serde_json::to_value(&content).expect("serialization failed");
        assert_eq!(serialized["type"], "text");
        assert!(serialized.get("content_type").is_none());
        assert_eq!(serialized["text"], "File content here");

        // Non-text content without text field
        let raw_binary = json!({ "type": "image" });
        let bin_content: McpContent =
            serde_json::from_value(raw_binary).expect("deserialization failed");
        assert_eq!(bin_content.content_type, "image");
        assert_eq!(bin_content.text, None);
    }

    #[test]
    fn test_mcp_tool_call_result_defaults_and_roundtrip() {
        let raw = json!({
            "content": [
                { "type": "text", "text": "Execution successful" }
            ],
            "isError": false
        });
        let result: McpToolCallResult =
            serde_json::from_value(raw).expect("deserialization failed");
        assert!(!result.is_error);
        assert_eq!(result.content.len(), 1);
        assert_eq!(result.content[0].content_type, "text");
        assert_eq!(
            result.content[0].text.as_deref(),
            Some("Execution successful")
        );

        // When isError is omitted, default to false per MCP spec
        let raw_default = json!({
            "content": []
        });
        let default_res: McpToolCallResult =
            serde_json::from_value(raw_default).expect("default failed");
        assert!(!default_res.is_error);
        assert!(default_res.content.is_empty());

        // Error result with isError = true
        let raw_err = json!({
            "content": [
                { "type": "text", "text": "File not found" }
            ],
            "isError": true
        });
        let err_res: McpToolCallResult =
            serde_json::from_value(raw_err).expect("error result failed");
        assert!(err_res.is_error);
        assert_eq!(err_res.content[0].text.as_deref(), Some("File not found"));

        let serialized = serde_json::to_value(&err_res).expect("serialization failed");
        assert_eq!(serialized["isError"], true);
        assert!(serialized.get("is_error").is_none());

        // Structured content and meta preservation (#3654)
        let raw_structured = json!({
            "content": [{ "type": "text", "text": "Found rows" }],
            "isError": false,
            "structuredContent": { "rows": [{ "id": 1 }] },
            "_meta": { "source": "db" }
        });
        let structured_res: McpToolCallResult =
            serde_json::from_value(raw_structured).expect("structured parse failed");
        assert_eq!(
            structured_res.structured_content,
            Some(json!({ "rows": [{ "id": 1 }] }))
        );
        assert_eq!(structured_res.meta, Some(json!({ "source": "db" })));
    }

    #[test]
    fn test_to_mcp_tool_result_supports_legacy_and_empty_shapes() {
        let legacy = to_mcp_tool_result(json!({ "toolResult": { "answer": 42 } })).unwrap();
        assert_eq!(legacy.content.len(), 1);
        assert_eq!(legacy.content[0].content_type, "text");
        assert_eq!(legacy.content[0].text.as_deref(), Some("{\"answer\":42}"));
        assert!(!legacy.is_error);

        let legacy_text = to_mcp_tool_result(json!({ "toolResult": "plain" })).unwrap();
        assert_eq!(legacy_text.content[0].text.as_deref(), Some("plain"));
        assert!(
            to_mcp_tool_result(json!({ "unexpected": true }))
                .unwrap()
                .content
                .is_empty()
        );
        assert!(to_mcp_tool_result(Value::Null).unwrap().content.is_empty());
    }

    #[test]
    fn test_to_mcp_tool_result_uses_v2_boolean_and_meta_filters() {
        let result = to_mcp_tool_result(json!({
            "content": [],
            "isError": "true",
            "structuredContent": [1, 2],
            "_meta": "not-an-object"
        }))
        .unwrap();
        assert!(!result.is_error);
        assert_eq!(result.structured_content, Some(json!([1, 2])));
        assert_eq!(result.meta, None);
    }

    #[test]
    fn test_mcp_content_uses_the_v2_mime_type_key() {
        let content: McpContent = serde_json::from_value(json!({
            "type": "image",
            "mimeType": "image/png",
            "data": "AA=="
        }))
        .unwrap();
        assert_eq!(content.mime_type.as_deref(), Some("image/png"));
        let wire = serde_json::to_value(content).unwrap();
        assert_eq!(wire["mimeType"], "image/png");
        assert!(wire.get("mime_type").is_none());
    }

    #[test]
    fn test_resource_link_preserves_the_top_level_uri() {
        let result = to_mcp_tool_result(json!({
            "content": [{
                "type": "resource_link",
                "uri": "https://example.com/image.png",
                "mimeType": "image/png"
            }]
        }))
        .unwrap();
        assert_eq!(
            result.content[0].uri.as_deref(),
            Some("https://example.com/image.png")
        );
        let wire = serde_json::to_value(&result.content[0]).unwrap();
        assert_eq!(wire["uri"], "https://example.com/image.png");
    }
}
