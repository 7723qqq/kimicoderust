use std::path::Path;

use serde_json::{Value, json};

use crate::i18n::{LocalizedText, i18n_params};
use crate::tools::memory_delete;
use crate::tools::memory_history;
use crate::tools::memory_list_handler;
use crate::tools::memory_read_handler;
use crate::tools::memory_search;
use crate::tools::memory_tool::MemoryContext;
use crate::tools::memory_write_handler;
use crate::turn_loop::types::ExecutableToolResult;

pub const ASSOCIATIVE_MEMORY_FLAG_ENV: &str = "KIMI_CODE_EXPERIMENTAL_ASSOCIATIVE_MEMORY";

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

pub fn action_display(action: MemoryAction) -> String {
    LocalizedText::with_params(
        "engine.tools.memoryAction",
        i18n_params!["action" => action.as_str()],
    )
    .render()
}

pub fn unknown_action_message(action: &str) -> String {
    LocalizedText::with_params(
        "engine.tools.unknownMemoryAction",
        i18n_params!["action" => action],
    )
    .render()
}

pub fn memory_tool_enabled() -> bool {
    memory_tool_enabled_from(Some(crate::env::env_switch_default_off(
        ASSOCIATIVE_MEMORY_FLAG_ENV,
    )))
}

pub fn memory_tool_enabled_from(associative_memory_enabled: Option<bool>) -> bool {
    associative_memory_enabled != Some(true)
}

pub fn main_agent_only_message() -> String {
    LocalizedText::new("engine.tools.memory.mainAgentOnly").render()
}

pub fn unavailable_message() -> String {
    LocalizedText::new("engine.tools.memory.associativeMemoryEnabled").render()
}

fn error_result(text: String) -> ExecutableToolResult {
    ExecutableToolResult {
        spill: None,
        parts: Vec::new(),
        delivery: None,
        stop_turn: false,
        content: text,
        is_error: true,
        note: None,
        display: None,
    }
}

pub fn execute(workspace_root: &Path, args: &Value, session_id: &str) -> ExecutableToolResult {
    if !memory_tool_enabled() {
        return error_result(unavailable_message());
    }
    match MemoryContext::resolve(workspace_root) {
        Ok(context) => run(&context, args, session_id),
        Err(error) => error_result(error),
    }
}

pub fn run(context: &MemoryContext, args: &Value, session_id: &str) -> ExecutableToolResult {
    let raw = args
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let Some(action) = MemoryAction::parse(raw) else {
        return error_result(unknown_action_message(raw));
    };
    match action {
        MemoryAction::Search => memory_search::search_action(context, args, session_id),
        MemoryAction::Read => memory_read_handler::read_action(context, args),
        MemoryAction::Write => memory_write_handler::write_action(context, args, session_id),
        MemoryAction::List => memory_list_handler::list_action(context, args, session_id),
        MemoryAction::Delete => memory_delete::delete_action(context, args),
        MemoryAction::Log => memory_history::log_action(context),
        MemoryAction::Revert => memory_history::revert_action(context, args),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_seven_actions_round_trip() {
        for name in ["search", "read", "write", "list", "delete", "log", "revert"] {
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

    #[test]
    fn the_associative_memory_flag_suppresses_the_tool() {
        assert!(memory_tool_enabled_from(None));
        assert!(memory_tool_enabled_from(Some(false)));
        assert!(!memory_tool_enabled_from(Some(true)));
    }

    #[test]
    fn the_three_history_and_search_actions_reach_their_handlers() {
        let dir = tempfile::tempdir().unwrap();
        let context = MemoryContext::new(dir.path().join("memory"), dir.path());
        let search = run(&context, &json!({ "action": "search" }), "");
        assert!(search.is_error, "search without a query must refuse");
        let log = run(&context, &json!({ "action": "log" }), "");
        assert!(!log.is_error, "{}", log.content);
        let revert = run(&context, &json!({ "action": "revert" }), "");
        assert!(revert.is_error, "revert without a sha must refuse");
    }

    #[test]
    fn an_unknown_action_reaches_the_refusal_path() {
        let dir = tempfile::tempdir().unwrap();
        let context = MemoryContext::new(dir.path().join("memory"), dir.path());
        let result = run(&context, &json!({ "action": "append" }), "");
        assert!(result.is_error);
        assert!(result.content.contains("append"), "{}", result.content);
    }
}
