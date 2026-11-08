use super::super::*;
use super::common::*;
use super::*;

/// v2's listing is allowlist-filtered: without a catalog the default
/// delegation set stands in, and the builtin `agent` is never spawnable.
#[test]
fn agent_def_lists_only_spawnable_types() {
    let def = agent_tool_def(None);
    let listing = def
        .description
        .split("Available agent types:")
        .nth(1)
        .expect("the description advertises types");
    for name in ["coder", "explore", "plan"] {
        assert!(listing.contains(&format!("- {name}:")), "missing {name}");
    }
    assert!(
        !listing.contains("- agent:"),
        "the builtin agent is not spawnable: {listing}"
    );
}

#[test]
fn agent_def_requires_explicit_subagent_authorization() {
    let description = agent_tool_def(None).description;
    for expected in [
        "Do not launch a subagent unless the user explicitly asks for one.",
        "Explicit instructions in AGENTS.md or a skill",
        "a multi-agent mode the user turned on (such as swarm or tower mode)",
        "a large or multi-part task, or a request for depth, thoroughness, research, or investigation, is not a request to launch",
        "The agent type descriptions below only help you choose a type once launching is authorized; they never authorize it.",
        "Resuming a subagent to continue already-authorized work needs no new request.",
    ] {
        assert!(
            description.contains(expected),
            "missing explicit-authorization rule {expected:?}:\n{description}"
        );
    }
    assert!(!description.contains("When NOT to use Agent:"));
    assert!(!description.contains("Delegating also keeps the bulk"));
}

/// v2 `buildProfileDescriptions` joins `description` and `whenToUse` in the
/// Agent tool listing. These are the three builtin profiles' upstream texts,
/// not a shortened paraphrase.
#[test]
fn agent_def_includes_builtin_when_to_use_text() {
    let description = agent_tool_def(None).description;
    for expected in [
        "Use this agent for non-trivial software engineering work that may require reading files, editing code, running commands, and returning a compact but technically complete summary to the parent agent.",
        "Fast agent specialized for exploring codebases. Use this when you need to quickly find files by patterns (e.g. \"src/**/*.yaml\"), search code for keywords (e.g. \"database connection\"), or answer questions about the codebase (e.g. \"how does the auth module work?\"). When calling this agent, specify the desired thoroughness level: \"quick\" for basic searches, \"medium\" for moderate exploration, or \"thorough\" for comprehensive analysis across multiple locations and naming conventions. Use this agent for any read-only exploration that will clearly require more than 3 search queries. Prefer launching multiple explore agents concurrently when investigating independent questions.",
        "Use this agent when the parent agent needs a step-by-step implementation plan, key file identification, and architectural trade-off analysis before code changes are made.",
    ] {
        assert!(
            description.contains(expected),
            "missing builtin whenToUse text {expected:?}:\n{description}"
        );
    }
}

#[test]
fn agent_def_disables_background_when_any_task_tool_is_unavailable() {
    const DISABLED: &str = "Background agent execution is disabled for this agent. Do not set `run_in_background=true` — any call that sets it is rejected before the subagent launches. Run every subagent in the foreground and wait for its result.";
    for missing in ["TaskList", "TaskOutput", "TaskStop"] {
        let def = agent_tool_def_filtered(None, &|name| name != missing, None);
        assert!(
            def.description.contains(DISABLED),
            "{missing} is unavailable, so background execution must be disabled:\n{}",
            def.description
        );
        assert!(
            !def.description.contains(
                "When `run_in_background=true`, the subagent runs detached from this turn."
            ),
            "{missing} is unavailable, but the enabled copy is still advertised"
        );
    }

    let enabled = agent_tool_def_filtered(None, &|_| true, None);
    assert!(
        enabled
            .description
            .contains("When `run_in_background=true`, the subagent runs detached from this turn.")
    );
    assert!(!enabled.description.contains(DISABLED));
}

#[test]
fn system_prompt_requires_explicit_subagent_authorization() {
    assert!(
        crate::prompt::builder::SYSTEM_PROMPT_TEMPLATE.contains(
            "Do not launch subagents with Agent unless the user, AGENTS.md, or a skill asks for them, or the user turned on a multi-agent mode such as swarm or tower mode."
        )
    );
}

/// The catalog-fed listing advertises a discovered type exactly when the
/// gate admits it (v2's filtered profile listing).
#[tokio::test]
async fn agent_def_lists_discovered_types_the_gate_admits() {
    let recorder = Arc::new(EventRecorder::new());
    let llm = Arc::new(RecordingPromptLlm::new(vec![]));
    let manager = manager_with_callbacks(llm, recorder).await;
    manager
        .register_discovered_definition(crate::subagent::types::SubagentDefinition {
            name: "reviewer".into(),
            description: "Reviewer subagent".into(),
            system_prompt: "You review.".into(),
            tools: vec![],
            disallowed_tools: vec![],
            when_to_use: None,
            prompt_prefix: None,
            summary_policy: None,
            model: None,
            subagents: None,
        })
        .await;
    let catalog = manager
        .spawn_catalog(crate::callbacks::MAIN_AGENT_ID)
        .await
        .expect("a catalog for the main agent");
    let def = agent_tool_def_filtered(None, &|_| true, Some(&catalog));
    assert!(
        def.description.contains("- reviewer: Reviewer subagent"),
        "{}",
        def.description
    );
    assert!(!def.description.contains("- agent:"), "{}", def.description);
}

#[tokio::test]
async fn agent_def_renders_discovered_when_to_use_only_when_present() {
    let recorder = Arc::new(EventRecorder::new());
    let llm = Arc::new(RecordingPromptLlm::new(vec![]));
    let manager = manager_with_callbacks(llm, recorder).await;
    let profiles: Vec<crate::rpc::types::SubagentProfileWire> =
        serde_json::from_value(serde_json::json!([
            {
                "name": "reviewer",
                "description": "Reviewer subagent",
                "system_prompt": "You review.",
                "when_to_use": "Use for code review."
            },
            {
                "name": "plain",
                "description": "Plain subagent",
                "system_prompt": "You assist."
            }
        ]))
        .expect("profile wires deserialize");
    manager.register_profile_snapshot(&profiles).await;

    let catalog = manager
        .spawn_catalog(crate::callbacks::MAIN_AGENT_ID)
        .await
        .expect("a catalog for the main agent");
    let description = agent_tool_def_filtered(None, &|_| true, Some(&catalog)).description;

    assert!(
        description.contains("- reviewer: Reviewer subagent Use for code review.\n  Tools: all"),
        "discovered whenToUse must follow description:\n{description}"
    );
    assert!(
        description.contains("- plain: Plain subagent\n  Tools: all"),
        "a missing whenToUse must not add a separator or placeholder:\n{description}"
    );
}

/// v2 #4152: a fork inherits the caller's profile when `subagent_type`
/// is omitted, and an explicit different type is refused against the
/// caller's REAL profile — the main agent's `agent`, not the spawn
/// default the stand-in once compared against.
#[tokio::test]
async fn fork_inherits_the_callers_profile() {
    let recorder = Arc::new(EventRecorder::new());
    let session_llm = Arc::new(RecordingPromptLlm::new(vec!["fork answer".into()]));
    let manager = manager_with_callbacks(session_llm.clone(), recorder.clone()).await;

    let refused = execute_agent(
        &manager,
        None,
        &serde_json::json!({ "subagent_type": "coder", "prompt": "task", "fork": true }),
        None,
        None,
        None,
        None,
    )
    .await
    .expect("typed forks resolve natively");
    assert!(refused.is_error);
    assert!(
        refused
            .content
            .contains("Cannot set a different subagent_type"),
        "{}",
        refused.content
    );

    let spawned = execute_agent(
        &manager,
        None,
        &serde_json::json!({ "prompt": "task", "fork": true }),
        None,
        None,
        None,
        None,
    )
    .await
    .expect("an untyped fork runs natively");
    assert!(!spawned.is_error, "{}", spawned.content);
    assert!(
        spawned.content.contains("actual_subagent_type: agent"),
        "the fork must inherit the main agent's profile: {}",
        spawned.content
    );
}

/// v2 #4152: a pool-bound caller's fork binds the SAME pool entry — the
/// session model is not the caller's model and never serves the fork.
#[tokio::test]
async fn a_pool_bound_callers_fork_binds_the_same_pool_entry() {
    let recorder = Arc::new(EventRecorder::new());
    let session_llm = Arc::new(RecordingPromptLlm::new(vec!["session answer".into()]));
    let strong_llm = Arc::new(RecordingPromptLlm::new(vec!["pool answer".into()]));
    let manager = manager_with_callbacks(session_llm.clone(), recorder.clone()).await;
    let pool = pool_runtime(strong_llm.clone());

    let parent = execute_agent(
        &manager,
        None,
        &serde_json::json!({ "subagent_type": "coder", "prompt": "parent task", "model": "strong" }),
        None,
        None,
        None,
        Some(&pool),
    )
    .await
    .expect("the pool-bound caller runs natively");
    assert!(!parent.is_error, "{}", parent.content);
    let parent_id = parent
        .content
        .lines()
        .next()
        .expect("the success text carries the id")
        .trim_start_matches("agent_id: ")
        .to_string();
    assert_eq!(strong_llm.call_count(), 1);

    let nested_manager = manager.clone();
    let fork = crate::tools::CALLER_AGENT_ID
        .scope(parent_id, async move {
            execute_agent(
                &nested_manager,
                None,
                &serde_json::json!({ "prompt": "fork task", "fork": true }),
                None,
                None,
                None,
                Some(&pool),
            )
            .await
            .expect("the nested fork runs natively")
        })
        .await;
    assert!(!fork.is_error, "{}", fork.content);
    assert!(
        fork.content.contains("actual_subagent_type: coder"),
        "the fork inherits the subagent caller's own type: {}",
        fork.content
    );
    assert_eq!(
        strong_llm.call_count(),
        2,
        "the fork reused the caller's pool entry"
    );
    assert_eq!(
        session_llm.call_count(),
        0,
        "the session model never served"
    );
}

#[test]
fn agent_def_lists_each_type_tool_scope() {
    // v2 `buildSubagentTypeDescriptions` + `resolveActiveToolNames`: the
    // model picks a subagent by what it is allowed to do, so every
    // advertised type carries its tool scope (MCP globs included).
    let def = agent_tool_def(None);
    let listing = def
        .description
        .split("Available agent types:")
        .nth(1)
        .expect("the description advertises the available types");
    for line in listing.lines() {
        if !line.starts_with("- ") {
            continue;
        }
        let name = line
            .trim_start_matches("- ")
            .split(':')
            .next()
            .unwrap_or_default()
            .trim()
            .to_string();
        let catalog = crate::prompt::profiles::ProfileCatalog::with_builtins();
        let profile = catalog
            .list()
            .iter()
            .find(|p| p.name == name)
            .unwrap_or_else(|| panic!("advertised type {name} has no profile"));
        let expected = format!("  Tools: {}", profile_tools_listing(profile, None));
        assert!(
            def.description.contains(&format!("- {name}")),
            "type {name} must be advertised"
        );
        assert!(
            def.description.contains(&expected),
            "type {name} must advertise its tool scope: {expected}\n{}",
            def.description
        );
    }
    // The built-in `explore` profile advertises its read-only scope.
    let explore_scope = listing
        .lines()
        .skip_while(|line| !line.starts_with("- explore"))
        .nth(1)
        .unwrap_or_default()
        .to_string();
    assert!(explore_scope.starts_with("  Tools: "), "{explore_scope}");
}

#[test]
fn profile_tools_listing_matches_v2_wording() {
    let profile = crate::prompt::profiles::AgentProfile {
        name: "t".into(),
        description: "d".into(),
        role_additional: String::new(),
        when_to_use: None,
        tools: vec!["Read".into(), "mcp__*".into()],
        subagents: None,
    };
    assert_eq!(profile_tools_listing(&profile, None), "Read, mcp__*");
    let unrestricted = crate::prompt::profiles::AgentProfile {
        name: "t".into(),
        description: "d".into(),
        role_additional: String::new(),
        when_to_use: None,
        tools: vec![],
        subagents: None,
    };
    assert_eq!(profile_tools_listing(&unrestricted, None), "all");
}

#[test]
fn profile_tools_listing_drops_names_the_session_cannot_call() {
    let profile = crate::prompt::profiles::AgentProfile {
        name: "t".into(),
        description: "d".into(),
        role_additional: String::new(),
        when_to_use: None,
        tools: vec!["Read".into(), "Bash".into(), "mcp__github__*".into()],
        subagents: None,
    };
    // The live table holds Read and an MCP tool from another server:
    // Bash is globally disabled and no connected server matches
    // `mcp__github__*`, so both leave the listing.
    let available = ["Read".to_string(), "mcp__linear__get_issue".to_string()];
    let is_available = |pattern: &str| {
        available
            .iter()
            .any(|name| crate::tools::tool_policy::matches_profile_tool_pattern(pattern, name))
    };
    assert_eq!(profile_tools_listing(&profile, Some(&is_available)), "Read");
    // Nothing available renders as `none`, the empty-allowlist wording.
    let nothing = |_: &str| false;
    assert_eq!(profile_tools_listing(&profile, Some(&nothing)), "none");
}

#[test]
fn agent_tool_def_filtered_renders_the_live_table() {
    // With only Read on the table, no MCP glob survives in any built-in
    // profile's listing — a glob is advertised only when some connected
    // tool matches it.
    let available = ["Read".to_string()];
    let def = agent_tool_def_filtered(
        None,
        &|pattern: &str| {
            available
                .iter()
                .any(|name| crate::tools::tool_policy::matches_profile_tool_pattern(pattern, name))
        },
        None,
    );
    let listing = def
        .description
        .split("Available agent types:")
        .nth(1)
        .expect("the description advertises the available types");
    for line in listing.lines() {
        if line.starts_with("  Tools: ") {
            assert!(
                !line.contains("mcp__"),
                "no MCP pattern without a connected MCP tool: {line}"
            );
        }
    }
    // The unfiltered form keeps the static allowlist verbatim.
    let static_def = agent_tool_def(None);
    assert!(static_def.description.contains("mcp__"));
}

/// v2 #3878: the Agent tool once advertised tool names that no registry
/// entry backed, so the model was told it could call something that would
/// come back "unknown tool". The fork builds its listing and its execution
/// gate from the same `ToolPolicyFilter`, which makes that structurally
/// impossible — this pins it, because a listing and a gate that drift apart
/// fail silently.
#[test]
fn every_advertised_builtin_tool_is_resolvable() {
    let catalog = crate::prompt::profiles::ProfileCatalog::with_builtins();
    for profile in catalog.list() {
        // `tools` empty means "all tools", so there is no name to check.
        for name in &profile.tools {
            // `mcp__*` and similar are prefix patterns the filter owns, not
            // concrete tool names; `is_native_tool_name` covers the concrete
            // ones plus MCP and GitHub families.
            if name.contains('*') {
                continue;
            }
            assert!(
                crate::tools::is_native_tool_name(name),
                "profile {:?} advertises {name:?}, which no native tool backs",
                profile.name
            );
            assert!(
                crate::subagent::manager::ToolPolicyFilter::from_allowlist(&profile.tools)
                    .allows(name),
                "profile {:?} advertises {name:?} but its own gate rejects it",
                profile.name
            );
        }
    }
}

/// v2 #4418 `exposesSubagentModelChoice`: only a pool with a real choice
/// advertises the listing and the `model` parameter — a single entry
/// pins every spawn and hides both.
#[test]
fn agent_def_advertises_the_pool_only_when_a_choice_is_offered() {
    fn entry(alias: &str, hint: &str) -> SecondaryModelEntry {
        SecondaryModelEntry {
            alias: alias.into(),
            hint: hint.into(),
            llm: Default::default(),
        }
    }
    fn pool(aliases: &[&str]) -> SecondaryModelRuntime {
        let llms = aliases
            .iter()
            .map(|alias| {
                (
                    alias.to_string(),
                    Arc::new(RecordingPromptLlm::new(vec![]))
                        as Arc<dyn crate::turn_loop::types::LLM>,
                )
            })
            .collect();
        SecondaryModelRuntime::new(
            SecondaryModelPool {
                default_model: aliases[0].into(),
                caller_model_alias: None,
                models: aliases.iter().map(|alias| entry(alias, "hint")).collect(),
            },
            llms,
        )
    }

    let offered = agent_tool_def(Some(&pool(&["strong", "fast"])));
    assert!(
        offered
            .description
            .contains("Available models (pass via model):")
    );
    assert!(
        offered
            .input_schema
            .get("properties")
            .and_then(|properties| properties.get("model"))
            .is_some()
    );

    let pinned = agent_tool_def(Some(&pool(&["strong"])));
    assert!(
        !pinned.description.contains("Available models"),
        "{}",
        pinned.description
    );
    assert!(
        pinned
            .input_schema
            .get("properties")
            .and_then(|properties| properties.get("model"))
            .is_none()
    );

    let bare = agent_tool_def(None);
    assert!(!bare.description.contains("Available models"));
    assert!(
        bare.input_schema
            .get("properties")
            .and_then(|properties| properties.get("model"))
            .is_none()
    );
}

/// v2 gates the fork guidance and the `fork` parameter on the same flag: the
/// guidance comes from `agent-fork.md` and the parameter from
/// `stripSubagentForkParameter`. The fork sentence used to be baked into
/// `AGENT_TOOL_DESCRIPTION`, so a flag-off session still advertised a feature it
/// had removed from the schema.
#[test]
fn agent_def_gates_fork_guidance_on_the_flag() {
    use crate::subagent::fork::test_override::Scoped;

    {
        let _off = Scoped::set(false);
        let def = agent_tool_def(None);
        assert!(
            !def.description.contains("Context forking"),
            "flag off must not advertise the guidance: {}",
            def.description
        );
        assert!(
            def.input_schema
                .get("properties")
                .and_then(|properties| properties.get("fork"))
                .is_none(),
            "flag off hides the parameter"
        );
    }

    let _on = Scoped::set(true);
    let def = agent_tool_def(None);
    assert!(
        def.description.contains(
            "Context forking: pass `fork: true` to start with a snapshot of your completed \
             conversation history, inheriting your current agent type, model, and tool set."
        ),
        "flag on advertises the v2 sentence: {}",
        def.description
    );
    assert_eq!(
        def.input_schema
            .get("properties")
            .and_then(|properties| properties.get("fork"))
            .and_then(|fork| fork.get("description"))
            .and_then(|description| description.as_str()),
        Some(
            "Start with a snapshot of your completed conversation history, inheriting your \
             current agent type, model, and tool set."
        ),
        "the parameter description is v2's, verbatim"
    );
}

/// v2 declares `NotifyUser` in both builtin subagent profiles (`CODER_TOOLS` /
/// `EXPLORE_TOOLS`) and lets the runtime availability filter drop it from the
/// listing when the host cannot show updates. The fork left it out of the
/// declarations entirely, so even a capable host's subagents could never use it.
#[test]
fn subagent_profiles_declare_notify_user_and_filter_it_at_runtime() {
    let unavailable = agent_tool_def_filtered(None, &|name| name != "NotifyUser", None);
    assert!(
        !unavailable.description.contains("NotifyUser"),
        "an unavailable tool is filtered out of the listing: {}",
        unavailable.description
    );

    let available = agent_tool_def_filtered(None, &|_| true, None);
    assert!(
        available.description.contains("NotifyUser"),
        "a capable host keeps it: {}",
        available.description
    );
}

/// The depth refusal reaches the model as a tool error. Returning `None`
/// instead would hand the same request to the host fallback, which runs it
/// outside the engine's depth accounting.
#[tokio::test]
async fn the_agent_tool_refuses_a_spawn_at_the_depth_cap() {
    let llm = Arc::new(RecordingPromptLlm::new(vec!["unused".into()]));
    let manager = manager_with(llm.clone()).await;
    manager
        .register_discovered_definition(crate::subagent::types::SubagentDefinition {
            name: "research".into(),
            description: "d".into(),
            system_prompt: "You research.".into(),
            tools: vec![],
            disallowed_tools: vec![],
            when_to_use: None,
            prompt_prefix: None,
            summary_policy: None,
            model: None,
            subagents: Some(vec!["research".into()]),
        })
        .await;

    let mut caller = crate::callbacks::MAIN_AGENT_ID.to_string();
    for _ in 0..crate::subagent::types::MAX_SUBAGENT_DEPTH {
        caller = manager
            .spawn_child(&caller, "research", "worker")
            .await
            .expect("the chain is admitted up to the cap");
    }

    let result = crate::tools::CALLER_AGENT_ID
        .scope(caller, async {
            execute_agent(
                &manager,
                None,
                &serde_json::json!({ "subagent_type": "research", "prompt": "go" }),
                None,
                None,
                None,
                None,
            )
            .await
        })
        .await
        .expect("a refused spawn is a native outcome, not a host fallback");

    assert!(result.is_error, "the depth refusal must be an error result");
    assert!(
        result.content.contains("delegation depth exceeded"),
        "v2 SUBAGENT_DEPTH_EXCEEDED text: {}",
        result.content
    );
    assert_eq!(llm.call_count(), 0, "nothing may run past the refusal");
}
