//! Agent Profile definitions and catalog for Kimi Agent.
//!
//! Provides builtin roles (agent, coder, explore, plan) with their
//! tool allowlists, role prefixes, descriptions, and handoff contracts.

pub const TASK_AGENT_ROLE_PREFIX: &str = "You are now running as a subagent. All the `user` messages are sent by the main agent. \
The main agent cannot see your context, it can only see your last message when you finish the task. \
You must treat the parent agent as your caller. Do not directly ask the end user questions. \
If something is unclear, explain the ambiguity in your final summary to the parent agent.";

pub const CODER_ROLE_SUFFIX: &str = "Your final message is the entire handoff — the parent sees nothing else from your run. \
Make it technically complete: what you changed and why, the path of every file you touched, \
how you verified the change (tests or commands run, with results), and anything left undone \
or worth follow-up. A final message of only a sentence or two is treated as too brief and \
sent back to you for expansion, costing an extra turn.";

pub const EXPLORE_ROLE_SUFFIX: &str = "Fast agent specialized for exploring codebases. Use this when you need to quickly find files by patterns, \
search code for keywords, or answer questions about the codebase. Read-only actions only.";

pub const PLAN_ROLE_SUFFIX: &str = "Read-only implementation planning and architecture design. Use this agent when the parent agent \
needs a step-by-step implementation plan, key file identification, and architectural trade-off analysis \
before code changes are made.";

/// Represents an agent persona/profile configuration.
#[derive(Debug, Clone)]
pub struct AgentProfile {
    pub name: String,
    pub description: String,
    /// v2 `AgentProfile.whenToUse`, rendered only in the Agent tool's type
    /// listing (never appended to the profile's system prompt).
    pub when_to_use: Option<String>,
    pub role_additional: String,
    pub tools: Vec<String>,
    /// The subagent types this profile may itself spawn (v2
    /// `AgentProfile.subagents`; the builtin `agent` declares
    /// `['coder', 'explore', 'plan']`, every other builtin declares none).
    pub subagents: Option<Vec<String>>,
}

/// Catalog holding registered AgentProfiles.
#[derive(Debug, Clone)]
pub struct ProfileCatalog {
    profiles: Vec<AgentProfile>,
}

impl Default for ProfileCatalog {
    fn default() -> Self {
        Self::with_builtins()
    }
}

impl ProfileCatalog {
    /// Create a catalog pre-populated with standard Kimi Code builtin profiles.
    pub fn with_builtins() -> Self {
        let agent = AgentProfile {
            name: "agent".into(),
            description: "Default interactive agent".into(),
            when_to_use: None,
            role_additional: String::new(),
            tools: vec![
                "Read".into(),
                "Write".into(),
                "Edit".into(),
                "Grep".into(),
                "Glob".into(),
                "Bash".into(),
                "TaskList".into(),
                "TaskOutput".into(),
                "TaskStop".into(),
                "WaitFor".into(),
                "CronCreate".into(),
                "CronList".into(),
                "CronDelete".into(),
                "TodoList".into(),
                "Skill".into(),
                "WebSearch".into(),
                "Agent".into(),
                "AgentSwarm".into(),
                "FetchURL".into(),
                "AskUserQuestion".into(),
                "EnterPlanMode".into(),
                "ExitPlanMode".into(),
                "CreateGoal".into(),
                "GetGoal".into(),
                "SetGoalBudget".into(),
                "UpdateGoal".into(),
                "mcp__*".into(),
            ],
            // v2 `session/agentLifecycle/profile/profiles.ts:91`: the main
            // agent's declared delegation set.
            subagents: Some(vec!["coder".into(), "explore".into(), "plan".into()]),
        };

        let coder = AgentProfile {
            name: "coder".into(),
            description: "General software engineering agent — modifying code with verification"
                .into(),
            when_to_use: Some(
                "Use this agent for non-trivial software engineering work that may require reading files, editing code, running commands, and returning a compact but technically complete summary to the parent agent."
                    .into(),
            ),
            role_additional: format!("{}\n\n{}", TASK_AGENT_ROLE_PREFIX, CODER_ROLE_SUFFIX),
            tools: vec![
                "NotifyUser".into(),
                "Bash".into(),
                "CronCreate".into(),
                "CronDelete".into(),
                "CronList".into(),
                "Edit".into(),
                "EnterPlanMode".into(),
                "ExitPlanMode".into(),
                "Glob".into(),
                "Grep".into(),
                "Read".into(),
                "Skill".into(),
                "TaskList".into(),
                "TaskOutput".into(),
                "TaskStop".into(),
                "TodoList".into(),
                "WaitFor".into(),
                "WebSearch".into(),
                "FetchURL".into(),
                "Write".into(),
                "mcp__*".into(),
            ],
            subagents: None,
        };

        let explore = AgentProfile {
            name: "explore".into(),
            description: "Fast codebase exploration with prompt-enforced read-only behavior".into(),
            when_to_use: Some(
                concat!(
                    "Fast agent specialized for exploring codebases. Use this when you need to quickly find files by patterns (e.g. \"src/**/*.yaml\"), search code for keywords (e.g. \"database connection\"), or answer questions about the codebase (e.g. \"how does the auth module work?\"). ",
                    "When calling this agent, specify the desired thoroughness level: \"quick\" for basic searches, \"medium\" for moderate exploration, or \"thorough\" for comprehensive analysis across multiple locations and naming conventions. ",
                    "Use this agent for any read-only exploration that will clearly require more than 3 search queries. Prefer launching multiple explore agents concurrently when investigating independent questions."
                )
                .into(),
            ),
            role_additional: format!("{}\n\n{}", TASK_AGENT_ROLE_PREFIX, EXPLORE_ROLE_SUFFIX),
            tools: vec![
                "NotifyUser".into(),
                "Bash".into(),
                "Read".into(),
                "Glob".into(),
                "Grep".into(),
                "WebSearch".into(),
                "FetchURL".into(),
            ],
            subagents: None,
        };

        let plan = AgentProfile {
            name: "plan".into(),
            description: "Read-only implementation planning and architecture design".into(),
            when_to_use: Some(
                "Use this agent when the parent agent needs a step-by-step implementation plan, key file identification, and architectural trade-off analysis before code changes are made."
                    .into(),
            ),
            role_additional: format!("{}\n\n{}", TASK_AGENT_ROLE_PREFIX, PLAN_ROLE_SUFFIX),
            tools: vec![
                "Read".into(),
                "Glob".into(),
                "Grep".into(),
                "WebSearch".into(),
                "FetchURL".into(),
            ],
            subagents: None,
        };

        Self {
            profiles: vec![agent, coder, explore, plan],
        }
    }

    /// Retrieve an AgentProfile by name (case-insensitive).
    pub fn get(&self, name: &str) -> Option<&AgentProfile> {
        self.profiles
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case(name))
    }

    /// Return the default 'agent' profile.
    pub fn default_profile(&self) -> &AgentProfile {
        self.get("agent")
            .expect("default 'agent' profile must exist")
    }

    /// List all registered profiles.
    pub fn list(&self) -> &[AgentProfile] {
        &self.profiles
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_catalog_builtins_and_case_insensitive_lookup() {
        let catalog = ProfileCatalog::with_builtins();

        // Verify catalog list count and default profile
        assert_eq!(catalog.list().len(), 4);
        assert_eq!(catalog.default_profile().name, "agent");
        assert_eq!(
            catalog.default_profile().description,
            "Default interactive agent"
        );

        // Verify each profile exists with exact name and non-empty description
        let agent = catalog.get("agent").expect("agent profile must exist");
        assert_eq!(agent.name, "agent");
        assert_eq!(agent.description, "Default interactive agent");
        assert!(agent.role_additional.is_empty());

        let coder = catalog.get("coder").expect("coder profile must exist");
        assert_eq!(coder.name, "coder");
        assert_eq!(
            coder.description,
            "General software engineering agent — modifying code with verification"
        );

        let explore = catalog.get("explore").expect("explore profile must exist");
        assert_eq!(explore.name, "explore");
        assert_eq!(
            explore.description,
            "Fast codebase exploration with prompt-enforced read-only behavior"
        );

        let plan = catalog.get("plan").expect("plan profile must exist");
        assert_eq!(plan.name, "plan");
        assert_eq!(
            plan.description,
            "Read-only implementation planning and architecture design"
        );

        // Case-insensitive retrieval verification
        assert_eq!(catalog.get("AGENT").map(|p| p.name.as_str()), Some("agent"));
        assert_eq!(catalog.get("Coder").map(|p| p.name.as_str()), Some("coder"));
        assert_eq!(
            catalog.get("EXPLORE").map(|p| p.name.as_str()),
            Some("explore")
        );
        assert_eq!(catalog.get("Plan").map(|p| p.name.as_str()), Some("plan"));

        // Non-existent profile returns None
        assert!(catalog.get("unknown_role").is_none());
    }

    #[test]
    fn test_coder_profile_contract_and_tool_boundaries() {
        let catalog = ProfileCatalog::with_builtins();
        let coder = catalog.get("coder").unwrap();

        // Exact role_additional contract
        let expected_role_additional =
            format!("{}\n\n{}", TASK_AGENT_ROLE_PREFIX, CODER_ROLE_SUFFIX);
        assert_eq!(coder.role_additional, expected_role_additional);
        assert!(
            coder
                .role_additional
                .contains("You are now running as a subagent.")
        );
        assert!(
            coder
                .role_additional
                .contains("Your final message is the entire handoff")
        );

        // Allowed tools: exact 21 tools. `NotifyUser` is v2's first entry in
        // `CODER_TOOLS`; the runtime listing filters it out when the host cannot
        // show updates, but the profile must declare it.
        assert_eq!(coder.tools.len(), 21);
        let expected_tools = [
            "NotifyUser",
            "Bash",
            "CronCreate",
            "CronDelete",
            "CronList",
            "Edit",
            "EnterPlanMode",
            "ExitPlanMode",
            "Glob",
            "Grep",
            "Read",
            "Skill",
            "TaskList",
            "TaskOutput",
            "TaskStop",
            "TodoList",
            "WaitFor",
            "WebSearch",
            "FetchURL",
            "Write",
            "mcp__*",
        ];
        for tool in expected_tools {
            assert!(
                coder.tools.contains(&tool.to_string()),
                "coder must have access to {}",
                tool
            );
        }

        // Boundary enforcement: coder cannot spawn recursive subagents or manage top-level goals
        let forbidden_tools = [
            "Agent",
            "AgentSwarm",
            "CreateGoal",
            "GetGoal",
            "SetGoalBudget",
            "UpdateGoal",
        ];
        for tool in forbidden_tools {
            assert!(
                !coder.tools.contains(&tool.to_string()),
                "coder must not have access to {}",
                tool
            );
        }
    }

    #[test]
    fn test_explore_profile_is_readonly_with_shell() {
        let catalog = ProfileCatalog::with_builtins();
        let explore = catalog.get("explore").unwrap();

        // Exact role_additional contract
        let expected_role = format!("{}\n\n{}", TASK_AGENT_ROLE_PREFIX, EXPLORE_ROLE_SUFFIX);
        assert_eq!(explore.role_additional, expected_role);
        assert!(
            explore
                .role_additional
                .contains("Fast agent specialized for exploring codebases.")
        );

        // Explore tools must be strictly read-only + Bash. `NotifyUser` is in
        // v2's `EXPLORE_TOOLS`; it updates the user rather than touching files.
        let expected_tools: Vec<String> = vec![
            "NotifyUser".into(),
            "Bash".into(),
            "Read".into(),
            "Glob".into(),
            "Grep".into(),
            "WebSearch".into(),
            "FetchURL".into(),
        ];
        assert_eq!(explore.tools, expected_tools);

        // Mutating tools strictly excluded
        let mutating_tools = [
            "Write",
            "Edit",
            "TodoList",
            "CronCreate",
            "Agent",
            "CreateGoal",
        ];
        for tool in mutating_tools {
            assert!(
                !explore.tools.contains(&tool.to_string()),
                "explore profile must not contain mutating tool {}",
                tool
            );
        }
    }

    #[test]
    fn test_plan_profile_is_strictly_readonly_and_no_shell() {
        let catalog = ProfileCatalog::with_builtins();
        let plan = catalog.get("plan").unwrap();

        // Exact role_additional contract
        let expected_role = format!("{}\n\n{}", TASK_AGENT_ROLE_PREFIX, PLAN_ROLE_SUFFIX);
        assert_eq!(plan.role_additional, expected_role);
        assert!(
            plan.role_additional
                .contains("Read-only implementation planning")
        );

        // Plan tools: strictly inspection and research only — NO shell execution
        let expected_tools: Vec<String> = vec![
            "Read".into(),
            "Glob".into(),
            "Grep".into(),
            "WebSearch".into(),
            "FetchURL".into(),
        ];
        assert_eq!(plan.tools, expected_tools);
        assert!(
            !plan.tools.contains(&"Bash".to_string()),
            "plan profile must NOT have Bash tool"
        );
        assert!(
            !plan.tools.contains(&"Write".to_string()),
            "plan profile must NOT have Write tool"
        );
        assert!(
            !plan.tools.contains(&"Edit".to_string()),
            "plan profile must NOT have Edit tool"
        );
    }

    #[test]
    fn test_agent_profile_has_full_orchestration_capabilities() {
        let catalog = ProfileCatalog::with_builtins();
        let agent = catalog.get("agent").unwrap();

        assert!(
            agent.role_additional.is_empty(),
            "agent profile role_additional should be empty"
        );
        assert_eq!(agent.tools.len(), 27);

        // Agent has top-level orchestration capabilities that subagents lack
        let orchestration_tools = [
            "Agent",
            "AgentSwarm",
            "CreateGoal",
            "GetGoal",
            "SetGoalBudget",
            "UpdateGoal",
            "AskUserQuestion",
        ];
        for tool in orchestration_tools {
            assert!(
                agent.tools.contains(&tool.to_string()),
                "default agent must have {}",
                tool
            );
        }
    }
}
