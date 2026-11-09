use crate::turn_loop::types::ExecutableToolResult;

pub const DEFAULT_PROFILE_NAME: &str = "coder";

pub const DEFAULT_SUBAGENT_TIMEOUT_MS: u64 = 2 * 60 * 60 * 1000;

pub fn subagent_timeout_ms() -> u64 {
    if let Ok(raw) = std::env::var("KIMI_SUBAGENT_TIMEOUT_MS")
        && let Ok(parsed) = raw.trim().parse::<u64>()
        && parsed >= 1
    {
        return parsed;
    }
    let Ok((_config, path)) = crate::config::KimiConfig::discover() else {
        return DEFAULT_SUBAGENT_TIMEOUT_MS;
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return DEFAULT_SUBAGENT_TIMEOUT_MS;
    };
    let Ok(value) = text.parse::<toml::Value>() else {
        return DEFAULT_SUBAGENT_TIMEOUT_MS;
    };
    value
        .get("subagent")
        .and_then(|section| section.get("timeout_ms"))
        .and_then(toml::Value::as_integer)
        .filter(|ms| *ms >= 1)
        .map_or(DEFAULT_SUBAGENT_TIMEOUT_MS, |ms| ms as u64)
}

pub(super) const TOWER_WORKER_PROFILE: &str = "tower-worker";

pub(super) const SUBAGENT_STOPPED_MESSAGE: &str = "The subagent was stopped before it finished.";
pub(super) const USER_INTERRUPTED_SUBAGENT_MESSAGE: &str =
    "The subagent was stopped before it finished by user.";

pub fn format_timeout_description(ms: u64) -> String {
    const HOUR: u64 = 60 * 60 * 1000;
    const MINUTE: u64 = 60 * 1000;
    if ms.is_multiple_of(HOUR) {
        let h = ms / HOUR;
        return format!("{h} hour{}", if h == 1 { "" } else { "s" });
    }
    if ms.is_multiple_of(MINUTE) {
        let m = ms / MINUTE;
        return format!("{m} minute{}", if m == 1 { "" } else { "s" });
    }
    if ms.is_multiple_of(1000) {
        let s = ms / 1000;
        return format!("{s} second{}", if s == 1 { "" } else { "s" });
    }
    format!("{ms} ms")
}

pub(super) fn string_arg(args: &serde_json::Value, name: &str) -> Option<String> {
    args.get(name)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

pub(super) fn required_string_arg(
    args: &serde_json::Value,
    name: &str,
) -> Result<String, ExecutableToolResult> {
    string_arg(args, name).ok_or_else(|| ExecutableToolResult {
        spill: None,
        parts: Vec::new(),
        delivery: None,
        stop_turn: false,
        content: format!(
            "Missing required argument `{name}`: Agent needs a non-empty `{name}` (the tool schema marks it required)."
        ),
        is_error: true,
        note: None,
        display: None,
    })
}

const BACKGROUND_REQUIRED_TOOLS: [&str; 3] = ["TaskList", "TaskOutput", "TaskStop"];
const BACKGROUND_AGENT_UNAVAILABLE: &str = "Background agent execution is not available for this agent because TaskList, TaskOutput, and TaskStop are not enabled.";

pub(crate) fn background_agent_tools_available(is_available: &dyn Fn(&str) -> bool) -> bool {
    BACKGROUND_REQUIRED_TOOLS
        .iter()
        .all(|name| is_available(name))
}

pub(crate) async fn background_agent_available_for_caller(
    manager: &std::sync::Arc<crate::subagent::SubagentManager>,
    caller_id: &str,
    is_available: &(dyn Fn(&str) -> bool + Send + Sync),
) -> bool {
    let Some(profile_name) = manager.caller_profile_name(caller_id).await else {
        return false;
    };
    let Some(profile) = manager.get_definition(&profile_name).await else {
        return false;
    };
    let policy = crate::subagent::manager::ToolPolicyFilter::from_definition(&profile);
    background_agent_tools_available(&|name| policy.allows(name) && is_available(name))
}

pub(crate) fn background_agent_unavailable() -> ExecutableToolResult {
    ExecutableToolResult {
        spill: None,
        parts: Vec::new(),
        delivery: None,
        stop_turn: false,
        content: BACKGROUND_AGENT_UNAVAILABLE.into(),
        is_error: true,
        note: None,
        display: None,
    }
}

const AGENT_BACKGROUND_ENABLED_DESCRIPTION: &str = "With `run_in_background=true`, the subagent runs detached and its result arrives automatically in a later turn as a synthetic user-role message — for an agent task it states that the subagent finished and points at the output file, so read that file to get the result. Continue with other work or respond to the user, and never fabricate or predict the result. Use it only when you have other work to do meanwhile. When your next step needs the result, run the subagent in the foreground instead of launching it in the background and waiting on it.";
const AGENT_BACKGROUND_DISABLED_DESCRIPTION: &str = "Background agent execution is disabled for this agent. Do not set `run_in_background=true` — any call that sets it is rejected before the subagent launches. Run every subagent in the foreground and wait for its result.";

pub(super) const AGENT_TOOL_DESCRIPTION: &str = "\
Launch a subagent to handle a task in its own context.

Do not launch a subagent unless the user explicitly asks for one, including delegation, parallel agents, or a named agent type. Explicit instructions in AGENTS.md or a skill, and user-enabled multi-agent modes, also authorize launching. Large or multi-part tasks and requests for depth, thoroughness, research, or investigation do not; do that work yourself. Agent type descriptions below guide selection only after authorization. Resuming already-authorized work needs no new request. Each subagent starts cold and re-derives context, costing time and tokens.

Writing the prompt:
- Brief it like a colleague who just walked into the room: state the goal, list what you already know, hand over the specifics.
- Lookups: put exact paths or commands you already know in the prompt.
- Investigations (figure out X, find why Y): give the question, not prescribed steps — fixed steps become dead weight when the premise is wrong.
- Do not delegate understanding. If the task hinges on a file path or line number, find it yourself first and write it into the prompt.

Usage notes:
- When the task continues earlier work a subagent already did, prefer resuming that agent (pass its `resume` id) over spawning a fresh instance — the resumed agent keeps its prior context.
- A subagent's result is only visible to you, not to the user. When the user needs to see what a subagent produced, summarize the relevant parts yourself in your own reply.
- Subagents use a fixed 2-hour timeout. If one times out, resume the same agent instead of starting over.

When NOT to use Agent: skip delegation for trivial work you can do directly — reading a file whose path you already know, searching a small known set of files, or any task that takes only a step or two. Delegation has a context-handoff cost; it pays off only when the task is substantial enough to outweigh it.

Once a subagent is running, leave that scope to it: do not redo its searches or reads in parallel, and do not abandon it midway and finish the job manually. Both undo the context savings the delegation was meant to buy.";

pub(super) fn profile_tools_listing(
    profile: &crate::prompt::profiles::AgentProfile,
    is_available: Option<&dyn Fn(&str) -> bool>,
) -> String {
    if profile.tools.is_empty() {
        return "all".to_string();
    }
    let active: Vec<&str> = profile
        .tools
        .iter()
        .filter(|name| {
            crate::subagent::manager::ToolPolicyFilter::from_allowlist(&profile.tools).allows(name)
        })
        .filter(|name| match is_available {
            Some(is_available) => is_available(name),
            None => true,
        })
        .map(|name| name.as_str())
        .collect();
    if active.is_empty() {
        "none".to_string()
    } else {
        active.join(", ")
    }
}

pub(super) fn definition_tools_listing(
    def: &crate::subagent::types::SubagentDefinition,
    is_available: Option<&dyn Fn(&str) -> bool>,
) -> String {
    if def.tools.is_empty() {
        return "all".to_string();
    }
    let active: Vec<&str> = def
        .tools
        .iter()
        .filter(|name| !def.disallowed_tools.contains(name))
        .filter(|name| match is_available {
            Some(is_available) => is_available(name),
            None => true,
        })
        .map(|name| name.as_str())
        .collect();
    if active.is_empty() {
        "none".to_string()
    } else {
        active.join(", ")
    }
}

pub fn agent_tool_def(
    pool: Option<&crate::subagent::secondary::SecondaryModelRuntime>,
) -> crate::turn_loop::types::ToolInfo {
    agent_tool_def_impl(pool, None, None, true)
}

pub fn agent_tool_def_filtered(
    pool: Option<&crate::subagent::secondary::SecondaryModelRuntime>,
    is_available: &dyn Fn(&str) -> bool,
    catalog: Option<&crate::subagent::manager::SpawnCatalog>,
) -> crate::turn_loop::types::ToolInfo {
    let background_available = background_agent_tools_available(is_available);
    agent_tool_def_impl(pool, Some(is_available), catalog, background_available)
}

pub(crate) fn agent_tool_def_filtered_with_background(
    pool: Option<&crate::subagent::secondary::SecondaryModelRuntime>,
    is_available: &dyn Fn(&str) -> bool,
    catalog: Option<&crate::subagent::manager::SpawnCatalog>,
    background_available: bool,
) -> crate::turn_loop::types::ToolInfo {
    agent_tool_def_impl(pool, Some(is_available), catalog, background_available)
}

fn profile_description_header(name: &str, description: &str, when_to_use: Option<&str>) -> String {
    let details: Vec<&str> = [Some(description), when_to_use]
        .into_iter()
        .flatten()
        .filter(|part| !part.is_empty())
        .collect();
    if details.is_empty() {
        format!("- {name}")
    } else {
        format!("- {name}: {}", details.join(" "))
    }
}

pub(super) fn agent_tool_def_impl(
    pool: Option<&crate::subagent::secondary::SecondaryModelRuntime>,
    is_available: Option<&dyn Fn(&str) -> bool>,
    catalog: Option<&crate::subagent::manager::SpawnCatalog>,
    background_available: bool,
) -> crate::turn_loop::types::ToolInfo {
    let builtin_catalog = crate::prompt::profiles::ProfileCatalog::with_builtins();
    let allowlist: Option<Vec<String>> = match catalog {
        Some(catalog) => catalog.allowlist.clone(),
        None => Some(
            crate::subagent::allowlist::DEFAULT_MAIN_SUBAGENTS
                .iter()
                .map(|name| name.to_string())
                .collect(),
        ),
    };
    let admitted = |name: &str| {
        allowlist
            .as_ref()
            .is_none_or(|list| list.iter().any(|allowed| allowed == name))
    };
    let mut available = String::new();
    for profile in builtin_catalog
        .list()
        .iter()
        .filter(|profile| admitted(&profile.name))
    {
        available.push_str(&profile_description_header(
            &profile.name,
            &profile.description,
            profile.when_to_use.as_deref(),
        ));
        available.push('\n');

        available.push_str(&format!(
            "  Tools: {}\n",
            profile_tools_listing(profile, is_available)
        ));
    }

    if let Some(catalog) = catalog {
        for def in catalog
            .discovered
            .iter()
            .filter(|def| admitted(&def.name))
            .filter(|def| !builtin_catalog.list().iter().any(|p| p.name == def.name))
        {
            available.push_str(&profile_description_header(
                &def.name,
                &def.description,
                def.when_to_use.as_deref(),
            ));
            available.push('\n');
            available.push_str(&format!(
                "  Tools: {}\n",
                definition_tools_listing(def, is_available)
            ));
        }
    }
    let background_description = if background_available {
        AGENT_BACKGROUND_ENABLED_DESCRIPTION
    } else {
        AGENT_BACKGROUND_DISABLED_DESCRIPTION
    };
    let mut description = format!(
        "{AGENT_TOOL_DESCRIPTION}\n\nAvailable agent types:\n{}",
        available.trim_end()
    );

    let mut extra = format!("\n\n{background_description}");
    if crate::subagent::fork::subagent_fork_enabled() {
        extra.push_str(
            "\n\nContext forking: pass `fork: true` to start with a snapshot of your completed conversation history, inheriting your current agent type, model, and tool set.",
        );
    }
    description.insert_str(AGENT_TOOL_DESCRIPTION.len(), &extra);

    if let Some(pool) = pool
        && pool.exposes_choice()
    {
        description.push_str("\n\n");
        description.push_str(&pool.description());
    }
    let mut input_schema = serde_json::json!({
        "type": "object",
        "properties": {
            "prompt": {
                "type": "string",
                "description": "Full task prompt for the subagent"
            },
            "description": {
                "type": "string",
                "description": "Short task description (3-5 words) for UI display"
            },
            "subagent_type": {
                "type": "string",
                "description": "One of the available agent types (see \"Available agent types\" in this tool description). Defaults to \"coder\" when omitted."
            },
            "resume": {
                "type": "string",
                "description": "Optional agent ID to resume instead of creating a new instance. When set, do not also pass subagent_type — the resumed agent keeps its own type, and supplying both is rejected."
            },
            "run_in_background": {
                "type": "boolean",
                "description": "If true, return immediately without waiting for completion. Prefer false unless the task can run independently and there is a clear benefit to not waiting."
            },
            "fork": {
                "type": "boolean",
                "description": "Start with a snapshot of your completed conversation history, inheriting your current agent type, model, and tool set."
            }
        },
        "required": ["prompt", "description"]
    });

    if !crate::subagent::fork::subagent_fork_enabled()
        && let Some(properties) = input_schema
            .get_mut("properties")
            .and_then(|properties| properties.as_object_mut())
    {
        properties.remove("fork");
    }
    if let Some(pool) = pool
        && pool.exposes_choice()
        && let Some(properties) = input_schema
            .get_mut("properties")
            .and_then(|properties| properties.as_object_mut())
    {
        properties.insert(
            "model".into(),
            serde_json::json!({
                "type": "string",
                "description": "Which model to run the subagent on: one of the aliases listed under \"Available models\" in this tool description. When omitted, the configured default model is used. Ignored when resuming — resumed subagents keep their own model."
            }),
        );
    }
    crate::turn_loop::types::ToolInfo {
        name: "Agent".into(),
        description,
        input_schema,
    }
}
