use serde::Deserialize;

pub const SNAPSHOT_LIST_TOOL_NAME: &str = "SnapshotList";

pub const SNAPSHOT_LIST_PATH_DESCRIPTION: &str = "File path whose recorded versions to list, as written in the tool call or absolute. The newest version comes first.";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SnapshotListInput {
    pub path: String,
}

pub fn parse_snapshot_list_input(value: &serde_json::Value) -> Result<SnapshotListInput, String> {
    let input: SnapshotListInput = serde_json::from_value(value.clone())
        .map_err(|error| format!("failed to parse SnapshotList input: {error}"))?;
    if input.path.is_empty() {
        return Err("Invalid SnapshotList arguments: `path` must not be empty.".into());
    }
    Ok(input)
}

pub fn snapshot_list_input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "required": ["path"],
        "properties": {
            "path": {
                "type": "string",
                "minLength": 1,
                "description": SNAPSHOT_LIST_PATH_DESCRIPTION,
            }
        },
        "additionalProperties": false
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_schema_matches_v2() {
        let schema = snapshot_list_input_schema();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["required"][0], "path");
        assert_eq!(schema["properties"]["path"]["minLength"], 1);
        assert_eq!(
            schema["properties"]["path"]["description"],
            SNAPSHOT_LIST_PATH_DESCRIPTION
        );
    }

    #[test]
    fn input_parsing_accepts_a_path_and_rejects_empty_or_missing() {
        assert_eq!(
            parse_snapshot_list_input(&serde_json::json!({ "path": "src/lib.rs" }))
                .expect("a path is accepted")
                .path,
            "src/lib.rs"
        );
        assert!(parse_snapshot_list_input(&serde_json::json!({ "path": "" })).is_err());
        assert!(parse_snapshot_list_input(&serde_json::json!({})).is_err());
        assert!(parse_snapshot_list_input(&serde_json::json!({ "path": 1 })).is_err());
        assert!(parse_snapshot_list_input(
            &serde_json::json!({ "path": "a", "extra": 1 })
        )
        .is_err());
    }
}
