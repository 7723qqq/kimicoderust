use serde_json::Value;

use crate::tool_input_display::ToolInputDisplay;
use crate::turn_loop::types::ToolAccesses;

pub struct ToolCallResolution {
    pub accesses: ToolAccesses,
    pub display: Option<ToolInputDisplay>,
    pub description: Option<String>,
    pub error: Option<String>,
}

pub fn resolve_tool_execution(
    tool_name: &str,
    args: &Value,
    workspace_dir: Option<&str>,
) -> ToolCallResolution {
    if let Some(error) = resolution_failure(tool_name, args) {
        return ToolCallResolution {
            accesses: Vec::new(),
            display: None,
            description: None,
            error: Some(error),
        };
    }
    ToolCallResolution {
        accesses: crate::turn_loop::tool_scheduler::infer_tool_accesses(tool_name, args),
        display: crate::tools::tool_display::tool_input_display(tool_name, args),
        description: crate::tools::tool_display::tool_call_description(
            tool_name,
            args,
            workspace_dir,
        ),
        error: None,
    }
}

fn resolution_failure(tool_name: &str, args: &Value) -> Option<String> {
    if !tool_name.eq_ignore_ascii_case("read") {
        return None;
    }
    let path = args.get("path").and_then(Value::as_str);
    if path.is_none_or(str::is_empty) {
        return Some("File path cannot be empty.".to_string());
    }
    let has_column = args
        .get("column_offset")
        .is_some_and(|value| !value.is_null());
    if has_column
        && args
            .get("line_offset")
            .and_then(Value::as_i64)
            .unwrap_or(1)
            < 0
    {
        return Some(
            "column_offset is only supported for forward reads. Use a positive line_offset or the forward Next Read arguments."
                .to_string(),
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn_loop::types::{FileOperation, ToolResourceAccess};

    #[test]
    fn an_empty_read_path_fails_resolution() {
        let resolution = resolve_tool_execution("Read", &serde_json::json!({ "path": "" }), None);
        assert_eq!(
            resolution.error.as_deref(),
            Some("File path cannot be empty.")
        );
        assert!(resolution.accesses.is_empty());
        assert!(resolution.display.is_none());
        assert!(resolution.description.is_none());
    }

    #[test]
    fn a_missing_read_path_fails_resolution() {
        let resolution = resolve_tool_execution("read", &serde_json::json!({}), None);
        assert_eq!(
            resolution.error.as_deref(),
            Some("File path cannot be empty.")
        );
    }

    #[test]
    fn a_backward_read_with_a_column_offset_fails_resolution() {
        let resolution = resolve_tool_execution(
            "Read",
            &serde_json::json!({ "path": "a.txt", "line_offset": -5, "column_offset": 3 }),
            None,
        );
        assert_eq!(
            resolution.error.as_deref(),
            Some(
                "column_offset is only supported for forward reads. Use a positive line_offset or the forward Next Read arguments."
            )
        );
        assert!(resolution.accesses.is_empty());
    }

    #[test]
    fn a_forward_read_with_a_column_offset_resolves_runnable() {
        let resolution = resolve_tool_execution(
            "Read",
            &serde_json::json!({ "path": "a.txt", "line_offset": 5, "column_offset": 3 }),
            Some("/workspace"),
        );
        assert!(resolution.error.is_none());
        assert_eq!(resolution.description.as_deref(), Some("Reading a.txt"));
        assert!(resolution.display.is_some());
        assert_eq!(resolution.accesses.len(), 1);
        match &resolution.accesses[0] {
            ToolResourceAccess::File(file) => {
                assert_eq!(file.operation, FileOperation::Read);
                assert_eq!(file.path, "a.txt");
            }
            other => panic!("unexpected access: {other:?}"),
        }
    }

    #[test]
    fn a_read_without_a_column_offset_allows_a_backward_range() {
        let resolution = resolve_tool_execution(
            "Read",
            &serde_json::json!({ "path": "a.txt", "line_offset": -5 }),
            Some("/workspace"),
        );
        assert!(resolution.error.is_none());
        assert!(resolution.description.is_some());
    }

    #[test]
    fn non_read_tools_never_fail_resolution() {
        let resolution = resolve_tool_execution("Bash", &serde_json::json!({ "command": "" }), None);
        assert!(resolution.error.is_none());
        assert_eq!(resolution.accesses.len(), 1);
    }
}
