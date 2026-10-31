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
    let (name, args) = parse_slash(text)?;
    BUILTIN_COMMANDS
        .iter()
        .find(|command| command.name == name)
        .map(|command| (command, args))
}

/// The generic slash shape `<name> [args]` (the name may contain `:` — the
/// `skill:` prefix is part of it), or `None` for non-slash input and names
/// with a path separator. Mirrors v2 `parseSlashInput`.
pub fn parse_slash(text: &str) -> Option<(&str, String)> {
    let rest = text.trim_start().strip_prefix('/')?;
    let trimmed = rest.trim_start();
    if trimmed.is_empty() {
        return None;
    }
    let split = trimmed.find(char::is_whitespace);
    let name = split.map_or(trimmed, |index| &trimmed[..index]);
    if name.is_empty() || name.contains('/') {
        return None;
    }
    let args = split
        .map(|index| trimmed[index..].trim().to_string())
        .unwrap_or_default();
    Some((name, args))
}

/// One skill projected into the slash palette: the command name is what the
/// client sends, `skill` the engine skill it activates.
pub struct SkillCommand {
    pub name: String,
    pub description: String,
    pub skill: String,
}

/// Project a session's skill catalog into slash commands (v2
/// `buildAcpSkillSlashCommands`): user-activatable skills only, builtin-source
/// and sub-skills under the bare name, everything else under `skill:<name>`,
/// builtins first and then by name. A command whose name collides with an ACP
/// builtin is dropped — the locally-executed builtin must always win — and a
/// skill restricted to UI scopes is not advertised here.
pub fn skill_commands(skills: &[crate::skills::SkillDescriptor]) -> Vec<SkillCommand> {
    let reserved: std::collections::HashSet<&str> = BUILTIN_COMMANDS
        .iter()
        .map(|command| command.name)
        .collect();
    let mut sorted: Vec<&crate::skills::SkillDescriptor> = skills.iter().collect();
    sorted.sort_by(|a, b| {
        (a.source != "builtin")
            .cmp(&(b.source != "builtin"))
            .then_with(|| a.name.cmp(&b.name))
    });
    let mut used = std::collections::HashSet::new();
    let mut commands = Vec::new();
    for skill in sorted {
        if !crate::tools::skill::is_user_activatable_skill_type(skill.skill_type.as_deref()) {
            continue;
        }
        if skill.scopes.is_some() {
            continue;
        }
        let name = if skill.source == "builtin" || skill.is_sub_skill == Some(true) {
            skill.name.clone()
        } else {
            format!("skill:{}", skill.name)
        };
        if reserved.contains(name.as_str()) || !used.insert(name.clone()) {
            continue;
        }
        commands.push(SkillCommand {
            name,
            description: skill.description.clone(),
            skill: skill.name.clone(),
        });
    }
    commands
}

/// The local notice for a skill activation the engine refused: an unknown
/// name answers exactly like any other unknown command, and a skill whose
/// type is not user-activatable says so instead of silently doing nothing.
pub fn skill_error_text(name: &str, error: &crate::tools::skill::SkillPromptError) -> String {
    match error {
        crate::tools::skill::SkillPromptError::NotFound => {
            format!("Unknown ACP command: /{name}. Use /help to see available commands.")
        }
        crate::tools::skill::SkillPromptError::TypeUnsupported(skill) => {
            format!("Skill \"{skill}\" cannot be activated from a slash command.")
        }
    }
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

#[cfg(test)]
mod skill_command_tests {
    use super::*;
    use crate::skills::SkillDescriptor;

    fn descriptor(name: &str, source: &str, skill_type: Option<&str>) -> SkillDescriptor {
        SkillDescriptor {
            name: name.into(),
            description: format!("{name} description"),
            source: source.into(),
            path: format!("/skills/{name}/SKILL.md"),
            disable_model_invocation: false,
            scopes: None,
            is_sub_skill: None,
            skill_type: skill_type.map(str::to_string),
        }
    }

    /// v2 `buildAcpSkillSlashCommands`: builtin-source and sub-skills keep the
    /// bare name, project skills take the `skill:` prefix, builtins come
    /// first by name, and an ACP builtin name is never shadowed.
    #[test]
    fn skill_commands_project_and_order_the_catalog() {
        let mut sub = descriptor("parent.child", "project", None);
        sub.is_sub_skill = Some(true);
        let skills = vec![
            descriptor("mine", "project", None),
            descriptor("helper", "builtin", None),
            sub,
            descriptor("compact", "project", None),
            descriptor("ref-only", "project", Some("reference")),
        ];
        let commands = skill_commands(&skills);
        let names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
        // v2 sorts by skill name (builtin source first): compact, mine,
        // parent.child. `skill:compact` does not collide with the builtin
        // `compact` — only a bare `compact` command name would be dropped.
        assert_eq!(
            names,
            vec!["helper", "skill:compact", "skill:mine", "parent.child"]
        );
        assert_eq!(commands[0].skill, "helper");
        assert_eq!(commands[2].skill, "mine");
    }

    /// A skill restricted to a UI scope is not advertised over ACP (v2 skips
    /// `scopes !== undefined`), and a type outside the user-activatable set is
    /// not either.
    #[test]
    fn skill_commands_skip_scoped_and_non_activatable_skills() {
        let mut scoped = descriptor("tui-only", "project", None);
        scoped.scopes = Some(vec!["tui".into()]);
        let skills = vec![scoped, descriptor("flow-ok", "project", Some("flow"))];
        let commands = skill_commands(&skills);
        let names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["skill:flow-ok"]);
    }

    /// The generic slash parser keeps `skill:` names intact and rejects
    /// path-like names, matching v2 `parseSlashInput`.
    #[test]
    fn parse_slash_handles_prefixed_names_and_args() {
        assert_eq!(
            parse_slash("/skill:demo hello there"),
            Some(("skill:demo", "hello there".to_string()))
        );
        assert_eq!(parse_slash("/status"), Some(("status", String::new())));
        assert_eq!(parse_slash("/a/b"), None);
        assert_eq!(parse_slash("plain"), None);
        assert_eq!(parse_slash("/"), None);
    }
}
