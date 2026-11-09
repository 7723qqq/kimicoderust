use serde_json::{Map, Value};

use crate::tool_input_display::{TodoListItem, ToolInputDisplay};
use crate::tools::agent_tool::agent_call_display;

fn text<'a>(args: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

pub fn tool_input_display(name: &str, args: &Value) -> Option<ToolInputDisplay> {
    let args = args.as_object()?;
    let normalized = name.to_ascii_lowercase().replace('_', "");
    let display = match normalized.as_str() {
        "bash" => ToolInputDisplay::Command {
            command: text(args, "command")?.to_string(),
            cwd: text(args, "cwd").map(str::to_string),
            description: text(args, "description").map(str::to_string),
            language: Some("bash".to_string()),
        },

        "powershell" => ToolInputDisplay::Command {
            command: text(args, "command")?.to_string(),
            cwd: text(args, "cwd").map(str::to_string),
            description: text(args, "description").map(str::to_string),
            language: None,
        },
        "read" => ToolInputDisplay::FileIo {
            operation: "read".to_string(),
            path: text(args, "path")?.to_string(),
            detail: None,
            content: None,
            before: None,
            after: None,
        },
        "write" => ToolInputDisplay::FileIo {
            operation: "write".to_string(),
            path: text(args, "path")?.to_string(),
            detail: None,
            content: text(args, "content").map(str::to_string),
            before: None,
            after: None,
        },
        "edit" => ToolInputDisplay::FileIo {
            operation: "edit".to_string(),
            path: text(args, "path")?.to_string(),
            detail: None,
            content: None,
            before: Some(text(args, "old_string")?.to_string()),
            after: Some(text(args, "new_string")?.to_string()),
        },

        "glob" => {
            let mut detail = format!("pattern: {}", text(args, "pattern")?);
            if let Some(path) = text(args, "path") {
                detail.push_str(&format!(", path: {path}"));
            }
            if args.get("include_ignored").and_then(Value::as_bool) == Some(true) {
                detail.push_str(", include_ignored: true");
            }
            ToolInputDisplay::FileIo {
                operation: "glob".to_string(),
                path: text(args, "path").unwrap_or(".").to_string(),
                detail: Some(detail),
                content: None,
                before: None,
                after: None,
            }
        }

        "grep" => ToolInputDisplay::FileIo {
            operation: "grep".to_string(),
            path: text(args, "path").unwrap_or(".").to_string(),
            detail: None,
            content: None,
            before: None,
            after: None,
        },
        "fetchurl" => ToolInputDisplay::UrlFetch {
            url: text(args, "url")?.to_string(),
            method: None,
        },
        "websearch" => ToolInputDisplay::Search {
            query: text(args, "query")?.to_string(),
            scope: None,
        },
        "skill" => ToolInputDisplay::SkillCall {
            skill_name: text(args, "skill")?.to_string(),
            args: text(args, "args").map(str::to_string),
        },
        "todolist" => {
            let items = args
                .get("todos")
                .and_then(Value::as_array)
                .map(|todos| {
                    todos
                        .iter()
                        .filter_map(|item| {
                            Some(TodoListItem {
                                title: item["title"].as_str()?.to_string(),
                                status: item["status"].as_str()?.to_string(),
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            ToolInputDisplay::TodoList { items }
        }
        "agent" | "agentswarm" => {
            return agent_call_display(name, &Value::Object(args.clone()))
                .and_then(|value| serde_json::from_value(value).ok());
        }
        _ => return None,
    };
    Some(display)
}

fn preview(value: &str, limit: usize) -> String {
    let units: Vec<u16> = value.encode_utf16().collect();
    if units.len() > limit {
        format!("{}…", String::from_utf16_lossy(&units[..limit]))
    } else {
        value.to_string()
    }
}

fn rendered_number(value: &Value) -> Option<String> {
    if let Some(integer) = value.as_i64() {
        return Some(integer.to_string());
    }
    value.as_f64().map(|number| number.to_string())
}

fn budget_label(value: &Value, unit: &str) -> Option<String> {
    let amount = rendered_number(value)?;
    let singular = unit.strip_suffix('s').unwrap_or(unit);
    Some(format!(
        "{amount} {}",
        if amount == "1" { singular } else { unit }
    ))
}

fn question_label(questions: &[Value]) -> String {
    let first = questions
        .first()
        .and_then(|question| question.get("question"))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    let label = if first.is_empty() {
        "Ask user question"
    } else {
        first
    };
    if questions.len() <= 1 {
        label.to_string()
    } else {
        format!("{label} (+{} more)", questions.len() - 1)
    }
}

pub fn tool_call_description(
    name: &str,
    args: &Value,
    workspace_dir: Option<&str>,
) -> Option<String> {
    let args = args.as_object()?;
    let normalized = name.to_ascii_lowercase().replace('_', "");
    let description = match normalized.as_str() {
        "read" => format!("Reading {}", text(args, "path")?),
        "write" => format!("Writing {}", text(args, "path")?),
        "edit" => format!("Editing {}", text(args, "path")?),
        "grep" => format!(
            "Searching for '{}' in {}",
            text(args, "pattern")?,
            text(args, "path").or(workspace_dir)?
        ),
        "glob" => format!("Searching {}", text(args, "pattern")?),
        "websearch" => format!("Searching: {}", preview(text(args, "query")?, 40)),
        "fetchurl" => format!("Fetching: {}", preview(text(args, "url")?, 50)),
        "bash" | "powershell" => {
            let preview = preview(text(args, "command")?, 50);
            if args.get("run_in_background").and_then(Value::as_bool) == Some(true) {
                format!("Starting background: {preview}")
            } else {
                format!("Running: {preview}")
            }
        }
        "taskstop" => format!("Stopping task {}", text(args, "task_id")?),
        "taskoutput" => format!("Reading output of task {}", text(args, "task_id")?),
        "tasklist" => "Listing background tasks".to_string(),
        "agent" => {
            let profile = text(args, "subagent_type")
                .unwrap_or(crate::tools::agent_tool::DEFAULT_PROFILE_NAME);
            let prefix = if args.get("run_in_background").and_then(Value::as_bool) == Some(true) {
                "Launching background"
            } else {
                "Launching"
            };
            format!("{prefix} {profile} agent: {}", text(args, "description")?)
        }
        "agentswarm" => format!("Launching agent swarm: {}", text(args, "description")?),
        "selecttools" => {
            if let Some(query) = text(args, "query") {
                format!("Searching tools for \"{query}\"")
            } else {
                let names = args.get("names")?.as_array()?;
                let names = names.iter().filter_map(Value::as_str).collect::<Vec<_>>();
                format!("Loading {}", names.join(", "))
            }
        }
        "skill" => format!("Invoke skill {}", text(args, "skill")?),
        "knowledge" => format!("Knowledge: {}", text(args, "action")?),
        "askuserquestion" => {
            if args.get("background").and_then(Value::as_bool) == Some(true) {
                let questions = args.get("questions")?.as_array()?;
                format!(
                    "Starting background question: {}",
                    question_label(questions)
                )
            } else {
                "Asking user questions".to_string()
            }
        }
        "exitplanmode" => "Presenting plan and exiting plan mode".to_string(),
        "enterspecmode" => "Requesting to enter spec mode".to_string(),
        "exitspecmode" => "Presenting spec and exiting spec mode".to_string(),
        "creategoal" => "Creating a goal".to_string(),
        "getgoal" => "Reading the current goal".to_string(),
        "updategoal" => format!("Setting goal status: {}", text(args, "status")?),
        "setgoalbudget" => format!(
            "Setting goal budget: {}",
            budget_label(args.get("value")?, text(args, "unit")?)?
        ),
        "cronlist" => "Listing scheduled cron jobs".to_string(),
        "crondelete" => format!("Deleting cron {}", text(args, "id")?),
        "lsp" => format!(
            "Running LSP {} at {}:{}:{}",
            text(args, "operation")?,
            text(args, "file_path")?,
            args.get("line")?,
            args.get("character")?
        ),
        "runcode" => "Running code in an isolated subprocess".to_string(),
        "repomap" => "Reading the repository map".to_string(),
        "sessionquery" => format!("Session query: {}", text(args, "operation")?),
        "memory" => crate::i18n::LocalizedText::with_params(
            "engine.tools.memoryAction",
            crate::i18n::i18n_params!["action" => text(args, "action")?],
        )
        .render(),
        "workflow" => format!("Workflow: {}", text(args, "operation")?),
        "notifyuser" => "Notifying the user".to_string(),
        "towerinit" => "Initializing tower workspace".to_string(),
        "towerplan" => format!(
            "Planning {} tower mission(s)",
            args.get("missions")?.as_array()?.len()
        ),
        "towerspawn" => format!(
            "Spawning tower {} \"{}\"",
            text(args, "kind")?,
            text(args, "name")?
        ),
        "towersend" => format!(
            "Sending tower message to {}: {}",
            text(args, "to")?,
            text(args, "subject")?
        ),
        "towerinbox" => "Reading tower inbox".to_string(),
        "towerfinding" => format!(
            "Filing tower {} finding: {}",
            text(args, "type")?,
            text(args, "title")?
        ),
        "towerreview" => format!(
            "Submitting tower review for {}: {}",
            text(args, "target")?,
            text(args, "status")?
        ),
        "towermerge" => format!("Merging tower branch: {}", text(args, "branch")?),
        "towerrebase" => format!(
            "Rebasing tower mission onto base: {}",
            text(args, "mission")?
        ),
        "towerteardown" => {
            let suffix = if args.get("dry_run").and_then(Value::as_bool) == Some(true) {
                " (dry run)"
            } else if args.get("force").and_then(Value::as_bool) == Some(true) {
                " (force)"
            } else {
                ""
            };
            format!("Tearing down tower workspace{suffix}")
        }
        "towerstatus" => "Reading tower status".to_string(),
        "towercomplete" => "Completing tower mission and storing its notification".to_string(),
        _ => return None,
    };
    Some(description)
}

#[cfg(test)]
mod tests {
    use super::{tool_call_description, tool_input_display};
    use serde_json::{Value, json};

    fn wire(name: &str, args: Value) -> Option<Value> {
        tool_input_display(name, &args).map(|display| serde_json::to_value(display).unwrap())
    }

    #[test]
    fn shell_tools_carry_command_and_language() {
        assert_eq!(
            wire("Bash", json!({ "command": "ls", "description": "list" })),
            Some(json!({
                "kind": "command",
                "command": "ls",
                "description": "list",
                "language": "bash",
            }))
        );
        assert_eq!(
            wire("PowerShell", json!({ "command": "Get-ChildItem" })),
            Some(json!({ "kind": "command", "command": "Get-ChildItem" }))
        );
    }

    #[test]
    fn file_tools_use_file_io() {
        assert_eq!(
            wire("Read", json!({ "path": "src/lib.rs" })),
            Some(json!({ "kind": "file_io", "operation": "read", "path": "src/lib.rs" }))
        );
        assert_eq!(
            wire(
                "Edit",
                json!({ "path": "a.rs", "old_string": "x", "new_string": "y" })
            ),
            Some(json!({
                "kind": "file_io",
                "operation": "edit",
                "path": "a.rs",
                "before": "x",
                "after": "y",
            }))
        );
        assert_eq!(
            wire("Write", json!({ "path": "a.rs", "content": "body" })),
            Some(json!({
                "kind": "file_io",
                "operation": "write",
                "path": "a.rs",
                "content": "body",
            }))
        );
    }

    #[test]
    fn glob_and_grep_name_the_search_root() {
        assert_eq!(
            wire("Glob", json!({ "pattern": "*.rs", "path": "src" })),
            Some(json!({
                "kind": "file_io",
                "operation": "glob",
                "path": "src",
                "detail": "pattern: *.rs, path: src",
            }))
        );
        assert_eq!(
            wire("Glob", json!({ "pattern": "*.rs" })),
            Some(json!({
                "kind": "file_io",
                "operation": "glob",
                "path": ".",
                "detail": "pattern: *.rs",
            }))
        );
        assert_eq!(
            wire("Grep", json!({ "pattern": "fn main" })),
            Some(json!({ "kind": "file_io", "operation": "grep", "path": "." }))
        );
    }

    #[test]
    fn web_and_skill_tools() {
        assert_eq!(
            wire("FetchURL", json!({ "url": "https://example.com" })),
            Some(json!({ "kind": "url_fetch", "url": "https://example.com" }))
        );
        assert_eq!(
            wire("WebSearch", json!({ "query": "rust" })),
            Some(json!({ "kind": "search", "query": "rust" }))
        );
        assert_eq!(
            wire("Skill", json!({ "skill": "commit" })),
            Some(json!({ "kind": "skill_call", "skill_name": "commit" }))
        );
    }

    #[test]
    fn todo_list_projects_title_and_status() {
        assert_eq!(
            wire(
                "TodoList",
                json!({ "todos": [{ "title": "one", "status": "pending" }] })
            ),
            Some(json!({
                "kind": "todo_list",
                "items": [{ "title": "one", "status": "pending" }],
            }))
        );
    }

    #[test]
    fn agent_tools_delegate_and_unknown_tools_have_no_display() {
        assert_eq!(
            wire("Agent", json!({ "prompt": "do it" })).map(|value| value["kind"].clone()),
            Some(json!("agent_call"))
        );
        assert!(tool_input_display("ExitPlanMode", &json!({})).is_none());
        assert!(tool_input_display("Read", &json!("not an object")).is_none());
    }

    #[test]
    fn call_descriptions_match_the_v2_action_labels() {
        assert_eq!(
            tool_call_description("Read", &json!({ "path": "src/lib.rs" }), None).as_deref(),
            Some("Reading src/lib.rs")
        );
        assert_eq!(
            tool_call_description("Edit", &json!({ "path": "a.rs" }), None).as_deref(),
            Some("Editing a.rs")
        );
        assert_eq!(
            tool_call_description("Grep", &json!({ "pattern": "fn main" }), Some("/w")).as_deref(),
            Some("Searching for 'fn main' in /w")
        );
        assert_eq!(
            tool_call_description(
                "Grep",
                &json!({ "pattern": "x", "path": "src" }),
                Some("/w")
            )
            .as_deref(),
            Some("Searching for 'x' in src")
        );
        assert_eq!(
            tool_call_description("Bash", &json!({ "command": "ls" }), None).as_deref(),
            Some("Running: ls")
        );
        assert_eq!(
            tool_call_description(
                "Bash",
                &json!({ "command": "x".repeat(51), "run_in_background": true }),
                None
            )
            .unwrap(),
            format!("Starting background: {}…", "x".repeat(50))
        );
        assert_eq!(
            tool_call_description(
                "Agent",
                &json!({ "description": "explore", "subagent_type": "Explore" }),
                None
            )
            .as_deref(),
            Some("Launching Explore agent: explore")
        );
        assert_eq!(
            tool_call_description("TaskStop", &json!({ "task_id": "t1" }), None).as_deref(),
            Some("Stopping task t1")
        );
        assert_eq!(
            tool_call_description(
                "AskUserQuestion",
                &json!({ "questions": [{ "question": "Pick" }] }),
                None
            )
            .as_deref(),
            Some("Asking user questions")
        );
        assert_eq!(
            tool_call_description(
                "AskUserQuestion",
                &json!({ "background": true, "questions": [{ "question": "Pick" }, { "question": "Two" }] }),
                None
            )
            .as_deref(),
            Some("Starting background question: Pick (+1 more)")
        );
        assert_eq!(
            tool_call_description(
                "SetGoalBudget",
                &json!({ "value": 1, "unit": "turns" }),
                None
            )
            .as_deref(),
            Some("Setting goal budget: 1 turn")
        );
        assert_eq!(
            tool_call_description("SelectTools", &json!({ "names": ["Read", "Grep"] }), None)
                .as_deref(),
            Some("Loading Read, Grep")
        );
        assert_eq!(
            tool_call_description("TowerTeardown", &json!({ "force": true }), None).as_deref(),
            Some("Tearing down tower workspace (force)")
        );
        assert_eq!(
            tool_call_description("LSP", &json!({ "operation": "goToDefinition", "file_path": "a.rs", "line": 3, "character": 7 }), None)
                .as_deref(),
            Some("Running LSP goToDefinition at a.rs:3:7")
        );
        assert_eq!(
            tool_call_description("Memory", &json!({ "action": "read" }), None).as_deref(),
            Some("Memory: read")
        );
        assert!(tool_call_description("TaskWait", &json!({ "timeout": 5 }), None).is_none());
        assert!(tool_call_description("TodoList", &json!({ "todos": [] }), None).is_none());
        assert!(tool_call_description("mcp__server__tool", &json!({}), None).is_none());
        assert!(tool_call_description("Read", &json!("nope"), None).is_none());
    }
}
