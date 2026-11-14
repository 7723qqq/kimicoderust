use std::io;
use std::sync::Arc;

use serde_json::{Map, Value, json};

use crate::human::llm::message::ContentPart;
use crate::human::tool::tool::{ToolDefinition, define_tool};
use crate::human::tool_select::state::{SELECT_TOOLS_TOOL_NAME, ToolSelectState};
use crate::wire::record::WireAsyncError;

const DESCRIPTION: &str = "Load one or more tools by name so you can call them. All available tool names are listed in the <tools_added>/<tools_removed> announcements in the system context — fold them in order to get the current list. Pass the exact name(s) you need; their full definitions become available immediately, so you can call them directly in your next tool call.";

fn result(text: String, is_error: Option<bool>) -> crate::human::tool::executor::ToolResult {
    crate::human::tool::executor::ToolResult {
        content: vec![ContentPart::Text { text, meta: None }],
        is_error,
    }
}

pub fn create_select_tools_tool(
    state: Arc<ToolSelectState>,
) -> Result<ToolDefinition, WireAsyncError> {
    let parameters = json!({
        "type": "object",
        "properties": {
            "names": {
                "type": "array",
                "description": "Exact tool names to load, taken from the latest announced tool list.",
                "items": { "type": "string" },
                "minItems": 1
            }
        },
        "required": ["names"],
        "additionalProperties": false
    });
    define_tool(ToolDefinition {
        name: SELECT_TOOLS_TOOL_NAME.to_string(),
        description: DESCRIPTION.to_string(),
        parameters: parameters.as_object().cloned().unwrap_or_else(Map::new),
        deferred: None,
        execute: Arc::new(move |input| {
            let state = state.clone();
            Box::pin(async move {
                if !state.enabled() {
                    return Ok(result(
                        "select_tools is not available for the current model.".to_string(),
                        Some(true),
                    ));
                }
                let raw = input.tool_call.arguments.as_deref().unwrap_or("{}");
                let args = serde_json::from_str::<Value>(raw)?;
                if args.is_null() {
                    return Err(Box::new(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Cannot read properties of null (reading 'names')",
                    )) as WireAsyncError);
                }
                let names = args
                    .get("names")
                    .and_then(Value::as_array)
                    .map(|names| {
                        names
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                if names.is_empty() {
                    return Ok(result(
                        "Provide at least one tool name in names.".to_string(),
                        Some(true),
                    ));
                }
                let loaded = state.load(&names);
                let mut lines = Vec::new();
                if !loaded.to_load.is_empty() {
                    lines.push(format!("Loaded: {}", loaded.to_load.join(", ")));
                }
                if !loaded.already_available.is_empty() {
                    lines.push(format!(
                        "Already available: {}",
                        loaded.already_available.join(", ")
                    ));
                }
                for name in &loaded.unknown {
                    lines.push(format!(
                        "Unknown tool: {name}. Pick from the latest announced tools list."
                    ));
                }
                let is_error = loaded.to_load.is_empty() && loaded.already_available.is_empty();
                Ok(result(
                    lines.join("\n"),
                    if is_error { Some(true) } else { None },
                ))
            })
        }),
    })
}

pub fn defer_tool(
    tool: &ToolDefinition,
    state: Arc<ToolSelectState>,
) -> Result<ToolDefinition, WireAsyncError> {
    let mut deferred = tool.clone();
    deferred.deferred = Some(true);
    let original = tool.clone();
    let name = tool.name.clone();
    deferred.execute = Arc::new(move |input| {
        let state = state.clone();
        let original = original.clone();
        let name = name.clone();
        Box::pin(async move {
            if !state.is_loaded(&name) {
                return Ok(result(
                    format!(
                        "Tool \"{name}\" is available but not loaded. Call select_tools with [\"{name}\"] first, then call the tool."
                    ),
                    Some(true),
                ));
            }
            original.execute(input).await
        })
    });
    define_tool(deferred)
}
