use crate::turn_loop::types::ExecutableToolResult;

/// The v2 default profile name (`DEFAULT_PROFILE_NAME`).
pub const DEFAULT_PROFILE_NAME: &str = "coder";

/// The default foreground timeout (v2 `DEFAULT_SUBAGENT_TIMEOUT_MS`: 2h).
pub const DEFAULT_SUBAGENT_TIMEOUT_MS: u64 = 2 * 60 * 60 * 1000;

/// The tower worker profile (`tools/tower/mod.rs`'s `TOWER_WORKER_PROFILE`):
/// the only profile `TowerSpawn` spawns, so it gates the tower resume check.
pub(super) const TOWER_WORKER_PROFILE: &str = "tower-worker";

/// The v2 stopped messages (`agent/tools/agent/agent.ts`), byte-identical.
pub(super) const SUBAGENT_STOPPED_MESSAGE: &str = "The subagent was stopped before it finished.";
pub(super) const USER_INTERRUPTED_SUBAGENT_MESSAGE: &str =
    "The subagent was stopped before it finished by user.";

/// The v2 timeout duration rendering (`formatSubagentTimeoutDescription`).
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

/// A required tool argument, validated instead of defaulted.
///
/// A missing `prompt` used to start a subagent with no task and report success,
/// so the model's mistake was invisible and the turn — plus its tokens — was
/// spent on nothing.
///
/// `description` is deliberately not validated here: the schema marks it
/// required, but it only feeds the UI label, and callers in the wild omit it.
/// Rejecting the call for a cosmetic field would break working clients for no
/// gain, so it keeps a default.
pub(super) fn required_string_arg(
    args: &serde_json::Value,
    name: &str,
) -> Result<String, ExecutableToolResult> {
    string_arg(args, name).ok_or_else(|| ExecutableToolResult {
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

/// Apply the caller's profile policy without leaking subagent types into the
/// callbacks layer. `is_available` supplies the other half (live table).
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
        delivery: None,
        stop_turn: false,
        content: BACKGROUND_AGENT_UNAVAILABLE.into(),
        is_error: true,
        note: None,
        display: None,
    }
}

const AGENT_BACKGROUND_ENABLED_DESCRIPTION: &str = "When `run_in_background=true`, the subagent runs detached from this turn. The completion arrives in a later turn as a synthetic user-role message containing its result — you do not need to poll, sleep, or check on its progress. Default to a foreground subagent when your next step needs its result.";
const AGENT_BACKGROUND_DISABLED_DESCRIPTION: &str = "Background agent execution is disabled for this agent. Do not set `run_in_background=true` — any call that sets it is rejected before the subagent launches. Run every subagent in the foreground and wait for its result.";

/// The v2 `SubagentTool` description body (condensed from
/// `agent-core-v2/src/agent/tools/agent/agent*.md`), with the profile list
/// appended at definition time.
pub(super) const AGENT_TOOL_DESCRIPTION: &str = "\
Launch a subagent to handle a task. The subagent runs as a same-process loop instance with its own context.

Do not launch a subagent unless the user explicitly asks for one. Each subagent starts cold and re-derives context you already have, which costs the user time and tokens. Asking means: the user asks for a subagent, delegation, or parallel agents, or requests one of the agent types below by name (for example \"use an explore agent\"). Explicit instructions in AGENTS.md or a skill, and a multi-agent mode the user turned on (such as swarm or tower mode), also count. Nothing else does: a large or multi-part task, or a request for depth, thoroughness, research, or investigation, is not a request to launch — do that work yourself with your own tools. The agent type descriptions below only help you choose a type once launching is authorized; they never authorize it. Resuming a subagent to continue already-authorized work needs no new request.

Writing the prompt:
- The subagent starts with zero context — it has not seen this conversation. Brief it like a colleague who just walked into the room: state the goal, list what you already know, hand over the specifics.
- Lookups (read this file, run that test): put the exact path or command in the prompt. The subagent should not have to search for things you already know.
- Investigations (figure out X, find why Y): give the question, not prescribed steps — fixed steps become dead weight when the premise is wrong.
- Do not delegate understanding. If the task hinges on a file path or line number, find it yourself first and write it into the prompt.

Usage notes:
- When the task continues earlier work a subagent already did, prefer resuming that agent (pass its `resume` id) over spawning a fresh instance — the resumed agent keeps its prior context.
- A subagent's result is only visible to you, not to the user. When the user needs to see what a subagent produced, summarize the relevant parts yourself in your own reply.
- Subagents use a fixed 2-hour timeout. If one times out, resume the same agent instead of starting over.

Once a subagent is running, leave that scope to it: do not redo its searches or reads in parallel, and do not abandon it midway and finish the job manually.";

/// The `Agent` tool definition (v2 `SubagentTool`): the subagent spawn /
/// resume surface. The optional `model` parameter is added only when a
/// `[secondary_model]` pool advertises a choice; without one the parameter is
/// not advertised, matching v2's `stripSubagentModelParameter`.
/// The v2 `resolveActiveToolNames` listing for one profile: `all` when no
/// allowlist is declared, `none` when the allowlist admits nothing, else the
/// comma-separated names (MCP globs included verbatim — `mcp__*` reads as
/// "every MCP tool").
///
/// `is_available` is the runtime availability filter: when present, a
/// declared pattern is listed only if it matches at least one tool in the
/// session's live table. MCP globs stay globs in the output — the predicate
/// answers "does any available tool match this pattern?", so `mcp__*`
/// survives exactly when some MCP tool is actually connected.
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

/// The `Tools:` line for a discovered definition (v2
/// `resolveActiveToolNames` over a file-declared allowlist): `all` when the
/// profile declares no list, `none` when nothing survives the session's
/// live table.
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

/// The runtime-filtered form of [`agent_tool_def`]: each profile's `Tools:`
/// line drops the declared patterns that match nothing in the session's live
/// tool table (a disconnected MCP server, a name the global `[tools]`
/// switch disabled, GitHub tools with no token), so the model never picks a
/// subagent for a capability it does not have right now.
///
/// `catalog` is the spawn gate's view (v2's allowlist-filtered profile
/// listing): the listed types are exactly the ones the caller may spawn,
/// including the discovered definitions. Absent, the static default
/// profile's delegation set (`coder` / `explore` / `plan`) stands in — the
/// builtin `agent` is not spawnable.
pub fn agent_tool_def_filtered(
    pool: Option<&crate::subagent::secondary::SecondaryModelRuntime>,
    is_available: &dyn Fn(&str) -> bool,
    catalog: Option<&crate::subagent::manager::SpawnCatalog>,
) -> crate::turn_loop::types::ToolInfo {
    let background_available = background_agent_tools_available(is_available);
    agent_tool_def_impl(pool, Some(is_available), catalog, background_available)
}

/// The caller-aware form: background availability also includes the caller's
/// own profile policy, not just the live table used for the target listings.
pub(crate) fn agent_tool_def_filtered_with_background(
    pool: Option<&crate::subagent::secondary::SecondaryModelRuntime>,
    is_available: &dyn Fn(&str) -> bool,
    catalog: Option<&crate::subagent::manager::SpawnCatalog>,
    background_available: bool,
) -> crate::turn_loop::types::ToolInfo {
    agent_tool_def_impl(pool, Some(is_available), catalog, background_available)
}

/// v2 `buildProfileDescriptions`: join non-empty description and whenToUse
/// with one space for both builtin and discovered profiles.
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
        // v2 `buildSubagentTypeDescriptions` / `resolveActiveToolNames`: the
        // listing advertises each type's tool scope so the model can pick a
        // subagent by what it is actually allowed to do.
        available.push_str(&format!(
            "  Tools: {}\n",
            profile_tools_listing(profile, is_available)
        ));
    }
    // The turn's discovered (host-pushed) types join the listing, filtered
    // by the same allowlist and deduplicated against builtin overrides.
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
    // v2 renders the fork guidance from `agent-fork.md`, and only while the
    // `subagent_fork` flag is on — the same gate the schema parameter uses
    // (`stripSubagentForkParameter`). The sentence is v2's, verbatim.
    let mut extra = format!("\n\n{background_description}");
    if crate::subagent::fork::subagent_fork_enabled() {
        extra.push_str(
            "\n\nContext forking: pass `fork: true` to start with a snapshot of your completed conversation history, inheriting your current agent type, model, and tool set.",
        );
    }
    description.insert_str(AGENT_TOOL_DESCRIPTION.len(), &extra);
    // v2 `buildSubagentModelDescriptions`: a pool with a single entry pins
    // every spawn, so the listing and the `model` parameter appear only when
    // a real choice exists (v2 `exposesSubagentModelChoice`).
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
                "description": "Short task description (3-5 words) for UI display. Optional: only feeds the UI label, so omitting it never fails the call."
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
        // Only `prompt` is required: `description` is cosmetic (UI label)
        // and real callers omit it, so the schema must not claim otherwise.
        "required": ["prompt"]
    });
    // v2 `stripSubagentForkParameter`: hide the parameter while the flag is
    // off. On by default here, so only an explicit opt-out hits this.
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
