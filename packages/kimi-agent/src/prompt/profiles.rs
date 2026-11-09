//! Agent Profile definitions and catalog for Kimi Agent.
//!
//! Provides builtin roles (agent, coder, explore, plan) with their
//! tool allowlists, role prefixes, descriptions, and handoff contracts, plus
//! the session-scoped merged catalog (v2
//! `session/sessionAgentProfileCatalog/sessionAgentProfileCatalogService.ts`:
//! builtin placeholders, file sources ordered by priority, `override: true`
//! required for a same-name replacement, `subagents` inheritance, and the
//! suppressed-source diagnostics exposed through `inspect`).

/// v2 `app/agentProfileCatalog/profile-shared.ts:21-23`, verbatim: the model
/// reads this to learn who its caller is.
pub const TASK_AGENT_ROLE_PREFIX: &str = "You are a subagent; the main agent is your caller and all `user` messages come from it. \
The main agent sees only your final message. Do not ask the end user questions; explain ambiguities in your final handoff.";

/// v2 `session/agentLifecycle/profile/profiles.ts:87-93`: the coder handoff
/// contract.
pub const CODER_ROLE_SUFFIX: &str = "Your final message is the entire handoff. Make it technically complete: what you changed and why, \
the path of every file you touched, how you verified the change (tests or commands run, with results), \
and anything left undone or worth follow-up. If you are stopped before finishing, the parent receives \
only what you have written so far, so keep the handoff current.";

/// v2 `session/agentLifecycle/profile/explore-overlay.md`, verbatim.
pub const EXPLORE_ROLE_SUFFIX: &str = "You are a codebase exploration specialist. Your role is EXCLUSIVELY to search, read, and \
analyze existing code and resources. You do NOT have access to file editing tools.

Guidelines:
- Use Glob for broad file pattern matching. Prefer patterns with a literal anchor (extension or subdirectory); pure wildcards like `*` or `**/*` are allowed but usually truncate at the match cap.
- Use Bash only for read-only operations (ls, git status, git log, git diff, find); never use it for file creation or modification.
- When WebSearch or FetchURL is available, use it when a question needs external context (library documentation, error messages, upstream APIs); the local codebase remains your primary domain.
- Adapt your search depth based on the thoroughness level specified by the caller.

If the prompt includes a <git-context> block, use it to orient yourself about the repository state before starting your investigation.

You are meant to be a fast agent. Complete the search request efficiently and report your findings clearly in a structured format.";

/// v2 `features/plan/profile/plan.ts:10-17`, verbatim.
pub const PLAN_ROLE_SUFFIX: &str = "If you lack sufficient codebase context, recommend an explore subagent for the remaining questions. Return:
1. What you know
2. Unanswered questions for explore
3. A preliminary plan if questions remain, otherwise a final implementation plan

You are read-only: do not run commands or modify files, even when general instructions say to make changes. \
Return the plan as your final message.";

/// v2 `session/agentLifecycle/profile/profiles.ts:15-74`: the main agent's
/// tool set, including the spec-mode, tower, repo-map and GitHub families.
const AGENT_TOOLS: &[&str] = &[
    "Read",
    "Write",
    "Edit",
    "Grep",
    "Glob",
    "Bash",
    "TaskList",
    "TaskOutput",
    "TaskStop",
    "WaitFor",
    "CronCreate",
    "CronList",
    "CronDelete",
    "TodoList",
    "Skill",
    "WebSearch",
    "Agent",
    "AgentSwarm",
    "FetchURL",
    "AskUserQuestion",
    "NotifyUser",
    "ExitPlanMode",
    "EnterSpecMode",
    "ExitSpecMode",
    "CreateGoal",
    "GetGoal",
    "SetGoalBudget",
    "UpdateGoal",
    "TowerInit",
    "TowerStatus",
    "TowerTeardown",
    "RepoMap",
    // v2 `...GITHUB_READONLY_TOOL_NAMES, ...GITHUB_MUTATING_TOOL_NAMES`.
    "GitHubGetRepo",
    "GitHubListBranches",
    "GitHubListCommits",
    "GitHubGetCommit",
    "GitHubGetFileContents",
    "GitHubGetRef",
    "GitHubListIssues",
    "GitHubGetIssue",
    "GitHubListIssueComments",
    "GitHubListPRs",
    "GitHubGetPR",
    "GitHubGetPRDiff",
    "GitHubGetPRFiles",
    "GitHubListPRReviewComments",
    "GitHubSearchCode",
    "GitHubSearchRepos",
    "GitHubSearchIssues",
    "GitHubListWorkflowRuns",
    "GitHubGetWorkflowRun",
    "GitHubListReleases",
    "GitHubGetLatestRelease",
    "GitHubGetMe",
    "GitHubCreateOrUpdateFile",
    "GitHubCreateTree",
    "GitHubCreateCommit",
    "GitHubUpdateRef",
    "GitHubCreateBranch",
    "GitHubCreateIssue",
    "GitHubUpdateIssue",
    "GitHubAddIssueComment",
    "GitHubCreatePR",
    "GitHubUpdatePR",
    "GitHubMergePR",
    "GitHubCreatePRReview",
    "mcp__*",
];

/// v2 `session/agentLifecycle/profile/profiles.ts:53-75`.
const CODER_TOOLS: &[&str] = &[
    "NotifyUser",
    "Bash",
    "CronCreate",
    "CronDelete",
    "CronList",
    "Edit",
    "ExitSpecMode",
    "Glob",
    "Grep",
    "Read",
    "RepoMap",
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

/// v2 `session/agentLifecycle/profile/profiles.ts:77-85`.
const EXPLORE_TOOLS: &[&str] = &[
    "NotifyUser",
    "Bash",
    "Read",
    "Glob",
    "Grep",
    "WebSearch",
    "FetchURL",
];

/// v2 `features/plan/profile/plan.ts:8`.
const PLAN_TOOLS: &[&str] = &[
    "NotifyUser",
    "Read",
    "Glob",
    "Grep",
    "WebSearch",
    "FetchURL",
];

/// Builtin profiles are not backed by a file source; this id is the v2
/// `BUILTIN_AGENT_PROFILE_SOURCE_ID`.
pub const BUILTIN_AGENT_PROFILE_SOURCE_ID: &str = "builtin";

/// v2 `app/agentProfileCatalog/agentProfileCatalog.ts:7`.
pub const DEFAULT_AGENT_PROFILE_NAME: &str = "agent";

/// The priority of the builtin registration. The value itself is only
/// observable through `inspect`; builtins are always projected first.
const BUILTIN_PROFILE_PRIORITY: i32 = i32::MIN;

fn tools(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_string()).collect()
}

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

/// Catalog holding the builtin AgentProfiles.
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
            name: DEFAULT_AGENT_PROFILE_NAME.into(),
            description: "Default agent".into(),
            when_to_use: None,
            role_additional: String::new(),
            tools: tools(AGENT_TOOLS),
            // v2 `session/agentLifecycle/profile/profiles.ts:99`: the main
            // agent's declared delegation set.
            subagents: Some(vec!["coder".into(), "explore".into(), "plan".into()]),
        };

        let coder = AgentProfile {
            name: "coder".into(),
            description: "General software engineering agent — the only subagent type with file-editing tools; use it for any delegated task that must modify code."
                .into(),
            when_to_use: Some(
                "Use this agent for non-trivial software engineering work that may require reading files, editing code, running commands, and returning a compact but technically complete summary to the parent agent."
                    .into(),
            ),
            role_additional: format!("{}\n\n{}", TASK_AGENT_ROLE_PREFIX, CODER_ROLE_SUFFIX),
            tools: tools(CODER_TOOLS),
            subagents: None,
        };

        let explore = AgentProfile {
            name: "explore".into(),
            description: "Fast codebase exploration with prompt-enforced read-only behavior.".into(),
            when_to_use: Some(
                concat!(
                    "Fast agent specialized for exploring codebases: it finds files by patterns (e.g. \"src/**/*.yaml\"), searches code for keywords (e.g. \"database connection\"), and answers questions about the codebase (e.g. \"how does the auth module work?\"). ",
                    "When calling this agent, specify the desired thoroughness level: \"quick\" for basic searches, \"medium\" for moderate exploration, or \"thorough\" for comprehensive analysis across multiple locations and naming conventions."
                )
                .into(),
            ),
            role_additional: format!("{}\n\n{}", TASK_AGENT_ROLE_PREFIX, EXPLORE_ROLE_SUFFIX),
            tools: tools(EXPLORE_TOOLS),
            subagents: None,
        };

        let plan = AgentProfile {
            name: "plan".into(),
            description: "Read-only implementation planning and architecture design.".into(),
            when_to_use: Some(
                "Use this agent when the parent agent needs a step-by-step implementation plan, key file identification, and architectural trade-off analysis before code changes are made."
                    .into(),
            ),
            role_additional: format!("{}\n\n{}", TASK_AGENT_ROLE_PREFIX, PLAN_ROLE_SUFFIX),
            tools: tools(PLAN_TOOLS),
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
        self.get(DEFAULT_AGENT_PROFILE_NAME)
            .expect("default 'agent' profile must exist")
    }

    /// List all registered profiles.
    pub fn list(&self) -> &[AgentProfile] {
        &self.profiles
    }
}

/// Why a candidate lost the projection (v2
/// `AgentProfileSuppressedCandidate.reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileSuppressedReason {
    /// A higher-priority candidate already won the name.
    Priority,
    /// The name was already merged and the candidate did not declare
    /// `override: true`.
    BuiltinOverrideRequired,
}

/// One losing candidate, recorded so `inspect` can explain what was dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileSuppressedCandidate {
    pub source_id: String,
    pub priority: i32,
    pub reason: ProfileSuppressedReason,
}

/// The projection record for one name (v2 `AgentProfileInspection`).
#[derive(Debug, Clone)]
pub struct ProfileInspection {
    pub name: String,
    pub profile: AgentProfile,
    pub source_id: String,
    pub priority: i32,
    pub suppressed: Vec<ProfileSuppressedCandidate>,
}

/// A profile contributed by one source (v2 `IAgentProfileRegistry` entry:
/// a source id, its priority, and the profiles it contributes).
#[derive(Debug, Clone)]
pub struct ProfileCandidate {
    pub profile: AgentProfile,
    pub source_id: String,
    pub priority: i32,
    /// v2 `AgentProfile.override`: a same-name collision requires it.
    pub override_builtin: bool,
}

/// The session-scoped merged view (v2 `ISessionAgentProfileCatalog`).
#[derive(Debug, Clone)]
pub struct MergedProfileCatalog {
    profiles: Vec<AgentProfile>,
    inspections: Vec<ProfileInspection>,
}

impl MergedProfileCatalog {
    /// Project registrations onto one profile per name, v2
    /// `SessionAgentProfileCatalogService.reproject()`:
    ///
    /// 1. builtins seed the map (they are the placeholder a file profile must
    ///    explicitly `override`);
    /// 2. other sources are ordered by priority, descending (the sort is
    ///    stable, so equal priorities keep registration order);
    /// 3. a candidate whose name is already merged needs `override: true`,
    ///    otherwise it is suppressed with `builtin-override-required`;
    /// 4. the winner inherits the replaced profile's `subagents` when it
    ///    declares none;
    /// 5. every candidate after the winner is suppressed with `priority`,
    ///    and the inspection records the losing sources.
    pub fn merge(builtins: &[AgentProfile], candidates: &[ProfileCandidate]) -> Self {
        let mut profiles: Vec<AgentProfile> = builtins.to_vec();
        let mut inspections: Vec<ProfileInspection> = builtins
            .iter()
            .map(|profile| ProfileInspection {
                name: profile.name.clone(),
                profile: profile.clone(),
                source_id: BUILTIN_AGENT_PROFILE_SOURCE_ID.to_string(),
                priority: BUILTIN_PROFILE_PRIORITY,
                suppressed: Vec::new(),
            })
            .collect();

        // v2 keeps one entry per name *within a source* (a `Map.set` per
        // registration, last declaration wins) before pairing candidates
        // across sources.
        let mut deduped: Vec<&ProfileCandidate> = Vec::new();
        for candidate in candidates {
            let name = candidate.profile.name.to_ascii_lowercase();
            match deduped.iter().position(|existing| {
                existing.source_id == candidate.source_id
                    && existing.profile.name.eq_ignore_ascii_case(&name)
            }) {
                Some(position) => deduped[position] = candidate,
                None => deduped.push(candidate),
            }
        }

        let mut ordered: Vec<&ProfileCandidate> = deduped;
        ordered.sort_by(|a, b| b.priority.cmp(&a.priority));

        // v2 groups candidates per entry by name first, then projects each
        // name's candidate list in priority order.
        let mut groups: Vec<Vec<&ProfileCandidate>> = Vec::new();
        for candidate in ordered {
            let key = candidate.profile.name.to_ascii_lowercase();
            match groups
                .iter_mut()
                .find(|group| group[0].profile.name.eq_ignore_ascii_case(&key))
            {
                Some(group) => group.push(candidate),
                None => groups.push(vec![candidate]),
            }
        }

        for group in groups {
            let mut suppressed: Vec<ProfileSuppressedCandidate> = Vec::new();
            let mut winner = false;
            for (index, candidate) in group.iter().enumerate() {
                let name = candidate.profile.name.to_ascii_lowercase();
                let existing = profiles
                    .iter()
                    .position(|profile| profile.name.eq_ignore_ascii_case(&name));
                if existing.is_some() && !candidate.override_builtin {
                    suppressed.push(ProfileSuppressedCandidate {
                        source_id: candidate.source_id.clone(),
                        priority: candidate.priority,
                        reason: ProfileSuppressedReason::BuiltinOverrideRequired,
                    });
                    continue;
                }

                let mut effective = candidate.profile.clone();
                if effective.subagents.is_none() {
                    if let Some(replaced) = existing.and_then(|position| profiles.get(position)) {
                        effective.subagents = replaced.subagents.clone();
                    }
                }
                match existing {
                    Some(position) => profiles[position] = effective.clone(),
                    None => profiles.push(effective.clone()),
                }

                let mut all_suppressed = suppressed.clone();
                all_suppressed.extend(group[index + 1..].iter().map(|rest| {
                    ProfileSuppressedCandidate {
                        source_id: rest.source_id.clone(),
                        priority: rest.priority,
                        reason: ProfileSuppressedReason::Priority,
                    }
                }));

                let inspection = ProfileInspection {
                    name: effective.name.clone(),
                    profile: effective,
                    source_id: candidate.source_id.clone(),
                    priority: candidate.priority,
                    suppressed: all_suppressed,
                };
                match inspections
                    .iter_mut()
                    .find(|record| record.name.eq_ignore_ascii_case(&name))
                {
                    Some(record) => *record = inspection,
                    None => inspections.push(inspection),
                }
                winner = true;
                break;
            }

            // Every candidate lost: the seeded inspection keeps the record of
            // what tried to replace it.
            if !winner && !suppressed.is_empty() {
                let name = &group[0].profile.name;
                if let Some(record) = inspections
                    .iter_mut()
                    .find(|record| record.name.eq_ignore_ascii_case(name))
                {
                    record.suppressed = suppressed;
                }
            }
        }

        Self {
            profiles,
            inspections,
        }
    }

    /// Retrieve the effective profile by name (case-insensitive).
    pub fn get(&self, name: &str) -> Option<&AgentProfile> {
        self.profiles
            .iter()
            .find(|profile| profile.name.eq_ignore_ascii_case(name))
    }

    /// Return the default 'agent' profile.
    pub fn default_profile(&self) -> &AgentProfile {
        self.get(DEFAULT_AGENT_PROFILE_NAME)
            .expect("default 'agent' profile must exist")
    }

    /// List every effective profile, builtin order first.
    pub fn list(&self) -> &[AgentProfile] {
        &self.profiles
    }

    /// v2 `ISessionAgentProfileCatalog.inspect`: the winning source and every
    /// suppressed candidate for a name.
    pub fn inspect(&self, name: &str) -> Option<&ProfileInspection> {
        self.inspections
            .iter()
            .find(|record| record.name.eq_ignore_ascii_case(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn test_catalog_builtins_and_case_insensitive_lookup() {
        let catalog = ProfileCatalog::with_builtins();

        // Verify catalog list count and default profile
        assert_eq!(catalog.list().len(), 4);
        assert_eq!(catalog.default_profile().name, "agent");
        assert_eq!(catalog.default_profile().description, "Default agent");

        // Verify each profile exists with exact name and non-empty description
        let agent = catalog.get("agent").expect("agent profile must exist");
        assert_eq!(agent.name, "agent");
        assert_eq!(agent.description, "Default agent");
        assert!(agent.role_additional.is_empty());

        let coder = catalog.get("coder").expect("coder profile must exist");
        assert_eq!(coder.name, "coder");
        assert_eq!(
            coder.description,
            "General software engineering agent — the only subagent type with file-editing tools; use it for any delegated task that must modify code."
        );

        let explore = catalog.get("explore").expect("explore profile must exist");
        assert_eq!(explore.name, "explore");
        assert_eq!(
            explore.description,
            "Fast codebase exploration with prompt-enforced read-only behavior."
        );

        let plan = catalog.get("plan").expect("plan profile must exist");
        assert_eq!(plan.name, "plan");
        assert_eq!(
            plan.description,
            "Read-only implementation planning and architecture design."
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
                .contains("the main agent is your caller")
        );
        assert!(
            coder
                .role_additional
                .contains("Your final message is the entire handoff")
        );

        // v2 `CODER_TOOLS`, verbatim: 21 entries, `NotifyUser` first.
        assert_eq!(coder.tools, names(CODER_TOOLS));
        assert_eq!(coder.tools.len(), 21);
        assert!(coder.tools.contains(&"NotifyUser".to_string()));
        assert!(coder.tools.contains(&"RepoMap".to_string()));
        assert!(coder.tools.contains(&"ExitSpecMode".to_string()));

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
                .contains("You are a codebase exploration specialist.")
        );
        assert!(
            explore
                .role_additional
                .contains("If the prompt includes a <git-context> block")
        );

        // v2 `EXPLORE_TOOLS`, verbatim.
        assert_eq!(explore.tools, names(EXPLORE_TOOLS));

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
        assert!(plan.role_additional.contains("You are read-only"));

        // v2 `PLAN_TOOLS`, verbatim: NotifyUser updates the user without
        // touching files; there is no shell execution.
        assert_eq!(plan.tools, names(PLAN_TOOLS));
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
        assert_eq!(agent.tools, names(AGENT_TOOLS));

        // Agent has top-level orchestration capabilities that subagents lack
        let orchestration_tools = [
            "Agent",
            "AgentSwarm",
            "CreateGoal",
            "GetGoal",
            "SetGoalBudget",
            "UpdateGoal",
            "AskUserQuestion",
            "ExitPlanMode",
            "EnterSpecMode",
            "ExitSpecMode",
            "TowerInit",
            "TowerStatus",
            "TowerTeardown",
            "RepoMap",
            "GitHubGetRepo",
            "GitHubCreateIssue",
            "mcp__*",
        ];
        for tool in orchestration_tools {
            assert!(
                agent.tools.contains(&tool.to_string()),
                "default agent must have {}",
                tool
            );
        }
        assert_eq!(agent.subagents, Some(names(&["coder", "explore", "plan"])));
        assert_eq!(agent.tools.len(), AGENT_TOOLS.len());
    }

    #[test]
    fn builtin_profiles_project_without_candidates() {
        let builtins = ProfileCatalog::with_builtins();
        let merged = MergedProfileCatalog::merge(builtins.list(), &[]);

        assert_eq!(merged.list().len(), 4);
        let agent = merged.inspect("agent").expect("agent inspection");
        assert_eq!(agent.source_id, BUILTIN_AGENT_PROFILE_SOURCE_ID);
        assert!(agent.suppressed.is_empty());
        assert_eq!(
            merged.inspect("EXPLORE").map(|record| record.name.as_str()),
            Some("explore")
        );
        assert!(merged.get("unknown_role").is_none());
    }

    #[test]
    fn a_same_name_candidate_without_override_is_suppressed() {
        let builtins = ProfileCatalog::with_builtins();
        let mut replacement = builtins.get("coder").unwrap().clone();
        replacement.description = "replacement".into();
        let merged = MergedProfileCatalog::merge(
            builtins.list(),
            &[ProfileCandidate {
                profile: replacement,
                source_id: "agent-file:/tmp/coder.md".into(),
                priority: 10,
                override_builtin: false,
            }],
        );

        // The builtin wins and the losing source is recorded.
        assert_eq!(
            merged.get("coder").unwrap().description,
            builtins.get("coder").unwrap().description
        );
        let inspection = merged.inspect("coder").expect("coder inspection");
        assert_eq!(inspection.source_id, BUILTIN_AGENT_PROFILE_SOURCE_ID);
        assert_eq!(
            inspection.suppressed,
            vec![ProfileSuppressedCandidate {
                source_id: "agent-file:/tmp/coder.md".into(),
                priority: 10,
                reason: ProfileSuppressedReason::BuiltinOverrideRequired,
            }]
        );
    }

    #[test]
    fn an_override_replaces_the_builtin_and_inherits_subagents() {
        let builtins = ProfileCatalog::with_builtins();
        let mut replacement = AgentProfile {
            name: "agent".into(),
            description: "custom main agent".into(),
            when_to_use: None,
            role_additional: "custom role".into(),
            tools: vec!["Read".into()],
            subagents: None,
        };
        let merged = MergedProfileCatalog::merge(
            builtins.list(),
            &[ProfileCandidate {
                profile: replacement.clone(),
                source_id: "agent-file:/tmp/agent.md".into(),
                priority: 5,
                override_builtin: true,
            }],
        );

        // The `subagents` list is inherited from the profile it replaced.
        let effective = merged.get("agent").unwrap();
        assert_eq!(effective.description, "custom main agent");
        assert_eq!(
            effective.subagents,
            Some(names(&["coder", "explore", "plan"]))
        );
        assert_eq!(merged.list().len(), 4);

        // A winner that declares its own list keeps it.
        replacement.subagents = Some(vec!["explore".into()]);
        let merged = MergedProfileCatalog::merge(
            builtins.list(),
            &[ProfileCandidate {
                profile: replacement,
                source_id: "agent-file:/tmp/agent.md".into(),
                priority: 5,
                override_builtin: true,
            }],
        );
        assert_eq!(
            merged.get("agent").unwrap().subagents,
            Some(vec!["explore".to_string()])
        );
        assert!(merged.inspect("agent").unwrap().suppressed.is_empty());
    }

    #[test]
    fn higher_priority_wins_and_the_rest_are_recorded_as_priority_suppressions() {
        let builtins = ProfileCatalog::with_builtins();
        let make = |description: &str| AgentProfile {
            name: "reviewer".into(),
            description: description.into(),
            when_to_use: None,
            role_additional: String::new(),
            tools: vec!["Read".into()],
            subagents: None,
        };
        let merged = MergedProfileCatalog::merge(
            builtins.list(),
            &[
                ProfileCandidate {
                    profile: make("low"),
                    source_id: "user".into(),
                    priority: 1,
                    override_builtin: true,
                },
                ProfileCandidate {
                    profile: make("high"),
                    source_id: "project".into(),
                    priority: 9,
                    override_builtin: true,
                },
                ProfileCandidate {
                    profile: make("middle"),
                    source_id: "extra".into(),
                    priority: 5,
                    override_builtin: true,
                },
            ],
        );

        let inspection = merged.inspect("reviewer").expect("reviewer inspection");
        assert_eq!(inspection.profile.description, "high");
        assert_eq!(inspection.source_id, "project");
        assert_eq!(
            inspection.suppressed,
            vec![
                ProfileSuppressedCandidate {
                    source_id: "extra".into(),
                    priority: 5,
                    reason: ProfileSuppressedReason::Priority,
                },
                ProfileSuppressedCandidate {
                    source_id: "user".into(),
                    priority: 1,
                    reason: ProfileSuppressedReason::Priority,
                },
            ]
        );
        // Builtins keep their positions; the new name is appended.
        assert_eq!(merged.list().len(), 5);
        assert_eq!(merged.list()[4].name, "reviewer");
    }

    #[test]
    fn a_higher_priority_candidate_wins_and_a_later_override_is_suppressed_by_priority() {
        let builtins = ProfileCatalog::with_builtins();
        let make = |description: &str| AgentProfile {
            name: "reviewer".into(),
            description: description.into(),
            when_to_use: None,
            role_additional: String::new(),
            tools: vec!["Read".into()],
            subagents: None,
        };
        let merged = MergedProfileCatalog::merge(
            builtins.list(),
            &[
                ProfileCandidate {
                    profile: make("high"),
                    source_id: "project".into(),
                    priority: 9,
                    override_builtin: false,
                },
                ProfileCandidate {
                    profile: make("low"),
                    source_id: "user".into(),
                    priority: 1,
                    override_builtin: true,
                },
            ],
        );

        // v2 stops at the first accepted candidate. The brand-new name did not
        // exist when the high-priority candidate was projected, so it needed no
        // override and won; the lower-priority candidate is suppressed by
        // priority even though it declares `override: true` — an override only
        // lets a candidate displace one that is already merged.
        let inspection = merged.inspect("reviewer").expect("reviewer inspection");
        assert_eq!(inspection.profile.description, "high");
        assert_eq!(inspection.source_id, "project");
        assert_eq!(
            inspection.suppressed,
            vec![ProfileSuppressedCandidate {
                source_id: "user".into(),
                priority: 1,
                reason: ProfileSuppressedReason::Priority,
            }]
        );
    }
}
