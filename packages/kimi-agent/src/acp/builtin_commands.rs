//! ACP built-in slash commands (v2 `acp-server/src/builtin-commands.ts`).
//!
//! The palette is advertised through `available_commands_update`; the prompt
//! handler intercepts these commands and answers locally instead of sending
//! the text to the model. Only the ACP-owned builtins are listed — the fork
//! has no host slash-command snapshot, and advertising a command nothing can
//! execute is worse than a smaller palette.

use serde_json::{Value, json};

pub struct BuiltinCommand {
    pub name: &'static str,
    pub description: &'static str,
    pub input_hint: Option<&'static str>,
}

pub const BUILTIN_COMMANDS: &[BuiltinCommand] = &[
    BuiltinCommand {
        name: "compact",
        description: "Compact the conversation context",
        input_hint: Some("<optional custom summarization instructions>"),
    },
    BuiltinCommand {
        name: "status",
        description: "Show current session status",
        input_hint: None,
    },
    BuiltinCommand {
        name: "usage",
        description: "Show session token usage",
        input_hint: None,
    },
    BuiltinCommand {
        name: "mcp",
        description: "Show MCP server status",
        input_hint: None,
    },
    BuiltinCommand {
        name: "tasks",
        description: "List background tasks",
        input_hint: None,
    },
    BuiltinCommand {
        name: "help",
        description: "Show available ACP commands",
        input_hint: None,
    },
];

/// The ACP `AvailableCommand[]` wire shape.
pub fn palette() -> Vec<Value> {
    BUILTIN_COMMANDS
        .iter()
        .map(|command| {
            let mut entry = json!({
                "name": command.name,
                "description": command.description,
            });
            if let Some(hint) = command.input_hint {
                entry["input"] = json!({ "hint": hint });
            }
            entry
        })
        .collect()
}

/// Parse a prompt whose leading text is `/<builtin> [args]`. Non-slash input
/// and unknown command names fall through to the model.
pub fn parse(text: &str) -> Option<(&'static BuiltinCommand, String)> {
    let rest = text.trim_start().strip_prefix('/')?;
    let mut parts = rest.splitn(2, char::is_whitespace);
    let name = parts.next()?;
    if name.is_empty() || name.contains('/') {
        return None;
    }
    let args = parts.next().unwrap_or("").trim().to_string();
    BUILTIN_COMMANDS
        .iter()
        .find(|command| command.name == name)
        .map(|command| (command, args))
}

/// `/help` output: every advertised command and its description.
pub fn help_text() -> String {
    let lines: Vec<String> = BUILTIN_COMMANDS
        .iter()
        .map(|command| format!("/{} — {}", command.name, command.description))
        .collect();
    format!("Available commands:\n{}", lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_recognizes_builtins_and_keeps_args() {
        let (command, args) = parse("/compact  focus on the auth refactor").unwrap();
        assert_eq!(command.name, "compact");
        assert_eq!(args, "focus on the auth refactor");

        let (command, args) = parse("  /status").unwrap();
        assert_eq!(command.name, "status");
        assert_eq!(args, "");

        assert!(parse("plain prompt").is_none());
        assert!(parse("/unknown").is_none());
        assert!(parse("/skill:foo").is_none());
        assert!(parse("/status/extra").is_none());
        assert!(parse("/").is_none());
    }

    #[test]
    fn palette_matches_the_command_table() {
        let palette = palette();
        let names: Vec<&str> = palette
            .iter()
            .map(|entry| entry["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec!["compact", "status", "usage", "mcp", "tasks", "help"]
        );
        assert_eq!(
            palette[0]["input"]["hint"],
            "<optional custom summarization instructions>"
        );
        assert!(palette[1].get("input").is_none());
    }

    #[test]
    fn help_lists_every_command() {
        let text = help_text();
        for command in BUILTIN_COMMANDS {
            assert!(text.contains(&format!("/{} — {}", command.name, command.description)));
        }
    }
}
