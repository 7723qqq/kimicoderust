use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSource {
    Builtin,
    Mcp,
    Host,
    Plugin,
}

impl ToolSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Mcp => "mcp",
            Self::Host => "host",
            Self::Plugin => "plugin",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolDisclosure {
    #[default]
    Inline,
    Deferred,
}

impl ToolDisclosure {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inline => "inline",
            Self::Deferred => "deferred",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSideEffect {
    Read,
    Write,
    Shell,
    Execute,
}

impl ToolSideEffect {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Shell => "shell",
            Self::Execute => "execute",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoMutation {
    Possible,
    None,
}

impl RepoMutation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Possible => "possible",
            Self::None => "none",
        }
    }
}

pub const MCP_OAUTH_AUTHORIZATION_URL_TOOL_UPDATE: &str = "mcp.oauth.authorization_url";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub source: ToolSource,
    pub disclosure: ToolDisclosure,
}

pub(crate) fn canonical_value(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(canonical_value).collect()),
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by_key(|(left, _)| *left);
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, item)| (key.clone(), canonical_value(item)))
                    .collect(),
            )
        }
        other => other.clone(),
    }
}

pub fn schema_signature(tool: &ToolDefinition) -> String {
    let parameters = if tool.input_schema.is_null() {
        serde_json::json!({ "type": "object", "properties": {} })
    } else {
        tool.input_schema.clone()
    };
    serde_json::to_string(&serde_json::json!([
        tool.name,
        tool.description,
        canonical_value(&parameters),
    ]))
    .unwrap_or_default()
}

pub fn version_signature(tool: &ToolDefinition) -> String {
    serde_json::to_string(&serde_json::json!([
        schema_signature(tool),
        tool.source.as_str(),
        tool.disclosure.as_str(),
    ]))
    .unwrap_or_default()
}

pub use crate::turn_loop::types::{
    ExecutableToolResult, FileOperation as ToolFileAccessOperation, ToolFileAccess,
    ToolResourceAccess, ToolResultSpill,
};
pub use kimi_core::types::ToolDelivery;

pub struct ToolAccesses;

impl ToolAccesses {
    pub fn none() -> Vec<ToolResourceAccess> {
        Vec::new()
    }

    pub fn all() -> Vec<ToolResourceAccess> {
        vec![ToolResourceAccess::All]
    }

    pub fn file(
        operation: ToolFileAccessOperation,
        path: &str,
        recursive: Option<bool>,
    ) -> Vec<ToolResourceAccess> {
        vec![ToolResourceAccess::File(ToolFileAccess {
            operation,
            path: path.to_string(),
            recursive: recursive.unwrap_or(false),
        })]
    }

    pub fn read_file(path: &str) -> Vec<ToolResourceAccess> {
        Self::file(ToolFileAccessOperation::Read, path, None)
    }

    pub fn read_tree(path: &str) -> Vec<ToolResourceAccess> {
        Self::file(ToolFileAccessOperation::Read, path, Some(true))
    }

    pub fn write_file(path: &str) -> Vec<ToolResourceAccess> {
        Self::file(ToolFileAccessOperation::Write, path, None)
    }

    pub fn write_tree(path: &str) -> Vec<ToolResourceAccess> {
        Self::file(ToolFileAccessOperation::Write, path, Some(true))
    }

    pub fn read_write_file(path: &str) -> Vec<ToolResourceAccess> {
        Self::file(ToolFileAccessOperation::ReadWrite, path, None)
    }

    pub fn read_write_tree(path: &str) -> Vec<ToolResourceAccess> {
        Self::file(ToolFileAccessOperation::ReadWrite, path, Some(true))
    }

    pub fn search_tree(path: &str) -> Vec<ToolResourceAccess> {
        Self::file(ToolFileAccessOperation::Search, path, Some(true))
    }

    pub fn conflict(left: &[ToolResourceAccess], right: &[ToolResourceAccess]) -> bool {
        left.iter()
            .any(|left_access| right.iter().any(|right_access| left_access.conflicts_with(right_access)))
    }
}

pub fn is_mcp_tool_name(name: &str) -> bool {
    name.starts_with("mcp__")
}

pub const DEFAULT_TOOL_RESULT_MAX_CHARS: usize = 50_000;
pub const DEFAULT_TOOL_RESULT_MAX_RETAINED_CHARS: usize = 10_000_000;
