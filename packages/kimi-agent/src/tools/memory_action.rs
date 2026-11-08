//! v2 `app/memory/tools/memoryTool.ts:38-59, :114-134`: the single `Memory`
//! tool's action vocabulary, input schema and unknown-action refusal.
//!
//! This is the first piece of the tool itself. It is deliberately standalone:
//! the seven handlers are ported one at a time, and the six `memory_*` tool
//! definitions stay in place until every handler has a counterpart.

use serde_json::{Value, json};

use crate::i18n::{LocalizedText, i18n_params};

/// The seven actions v2's `MemoryActionSchema` accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryAction {
    Search,
    Read,
    Write,
    List,
    Delete,
    Log,
    Revert,
}

impl MemoryAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Search => "search",
            Self::Read => "read",
            Self::Write => "write",
            Self::List => "list",
            Self::Delete => "delete",
            Self::Log => "log",
            Self::Revert => "revert",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "search" => Self::Search,
            "read" => Self::Read,
            "write" => Self::Write,
            "list" => Self::List,
            "delete" => Self::Delete,
            "log" => Self::Log,
            "revert" => Self::Revert,
            _ => return None,
        })
    }
}

/// v2 `MemoryToolInputSchema` rendered as JSON Schema. The field descriptions
/// are v2's `.describe()` text, verbatim.
pub fn memory_input_schema() -> Value {
    json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ["search", "read", "write", "list", "delete", "log", "revert"],
                "description": "The memory operation to perform. Required."
            },
            "query": {
                "type": "string",
                "description": "Search query (for `search` action)."
            },
            "path": {
                "type": "string",
                "description": "Memory file path (for `read`, `write`, `delete`). Can include or omit `.md` extension. For `read`/`delete`, this is the full relative path (e.g. `global/auth.md`). For `write`, this is the filename within the chosen scope."
            },
            "scope": {
                "type": "string",
                "enum": ["global", "project", "session"],
                "description": "Memory scope (for `search`, `write`, `list`). `search` defaults to every scope of every project; `write` defaults to `project`; `list` defaults to all scopes."
            },
            "content": {
                "type": "string",
                "description": "Markdown content (for `write` action)."
            },
            "sha": {
                "type": "string",
                "description": "Memory repository commit sha (for `revert` action)."
            }
        },
        "required": ["action"],
        "additionalProperties": false
    })
}

/// The display description v2 shows on the approval card
/// (`memoryTool.ts:93`, `toolsV2.memory.memoryAction`). The engine names the
/// same key under its own `engine.tools.*` namespace, as `check:engine-i18n`
/// requires.
pub fn action_display(action: MemoryAction) -> String {
    LocalizedText::with_params(
        "engine.tools.memoryAction",
        i18n_params!["action" => action.as_str()],
    )
    .render()
}

/// v2's `default` arm (`memoryTool.ts:129-133`): an action outside the enum is
/// an error result for the model, not a dropped call.
pub fn unknown_action_message(action: &str) -> String {
    LocalizedText::with_params(
        "engine.tools.unknownMemoryAction",
        i18n_params!["action" => action],
    )
    .render()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_seven_actions_round_trip() {
        for name in [
            "search", "read", "write", "list", "delete", "log", "revert",
        ] {
            let action = MemoryAction::parse(name).expect("v2 action parses");
            assert_eq!(action.as_str(), name);
        }
    }

    #[test]
    fn an_unknown_action_is_refused_not_dropped() {
        assert_eq!(MemoryAction::parse("append"), None);
        let message = unknown_action_message("append");
        assert!(message.contains("append"), "{message}");
    }

    #[test]
    fn the_schema_names_every_action_and_requires_action() {
        let schema = memory_input_schema();
        let variants: Vec<&str> = schema["properties"]["action"]["enum"]
            .as_array()
            .expect("enum")
            .iter()
            .map(|value| value.as_str().expect("string"))
            .collect();
        assert_eq!(
            variants,
            ["search", "read", "write", "list", "delete", "log", "revert"]
        );
        assert_eq!(schema["required"][0], "action");
        assert_eq!(schema["additionalProperties"], false);
        for field in ["query", "path", "scope", "content", "sha"] {
            assert!(schema["properties"][field].is_object(), "missing {field}");
        }
    }

    #[test]
    fn the_action_display_names_the_action() {
        let text = action_display(MemoryAction::Write);
        assert!(text.contains("write"), "{text}");
    }
}
