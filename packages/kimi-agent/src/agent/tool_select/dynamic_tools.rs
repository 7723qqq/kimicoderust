use serde_json::Value;

pub use crate::tools::select_tools::{NO_LOADABLE_TOOLS_MESSAGE, SELECT_TOOLS_TOOL_NAME};
pub use crate::tools::tool_select_state::*;
pub use kimi_core::types::{LLMMessage, ToolDescription};

pub fn is_tool_select_announcement_disclosure(value: &Value) -> bool {
    let Some(candidate) = value.as_object() else {
        return false;
    };
    if !candidate.get("revision").is_some_and(Value::is_number) {
        return false;
    }
    match candidate.get("kind").and_then(Value::as_str) {
        Some("snapshot") => candidate.get("tools").is_some_and(Value::is_array),
        Some("delta") => {
            candidate.get("added").is_some_and(Value::is_array)
                && candidate.get("removed").is_some_and(Value::is_array)
        }
        _ => false,
    }
}

pub fn render_loadable_tools_announcement(
    added: &[String],
    removed: &[String],
    has_loadable: bool,
) -> String {
    crate::tools::select_tools::render_loadable_tools_announcement(added, removed, has_loadable)
        .unwrap_or_else(|| {
            "Use the select_tools tool with exact names to load full definitions of the announced tools before calling them. Only the announced names are loadable — tools you already have available are called directly, never passed to select_tools; plugin, skill, or category names do not work. Names listed as removed are no longer loadable — do not select them. Fold all announcements in this conversation in order to get the current list."
                .to_string()
        })
}
