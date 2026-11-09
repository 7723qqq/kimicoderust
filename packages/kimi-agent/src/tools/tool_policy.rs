use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use crate::turn_loop::types::ToolInfo;

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ToolsFilter {
    #[serde(default)]
    pub enabled: Vec<String>,
    #[serde(default)]
    pub disabled: Vec<String>,
}

impl ToolsFilter {
    pub fn is_active(&self) -> bool {
        !self.enabled.is_empty() || !self.disabled.is_empty()
    }

    pub fn allows(&self, name: &str) -> bool {
        is_tool_active_ref(
            PolicyRef {
                tools: (!self.enabled.is_empty()).then_some(self.enabled.as_slice()),
                disallowed_tools: (!self.disabled.is_empty()).then_some(self.disabled.as_slice()),
            },
            name,
            ToolSource::of_name(name),
        )
    }

    pub fn apply(&self, defs: Vec<ToolInfo>) -> Vec<ToolInfo> {
        if !self.is_active() {
            return defs;
        }
        defs.into_iter()
            .filter(|definition| self.allows(&definition.name))
            .collect()
    }

    pub fn blocks_call(&self, name: &str) -> bool {
        if !self.is_active() {
            return false;
        }
        let matches = |pattern: &str| {
            if is_mcp_tool_name(pattern) {
                matches_tool_pattern(pattern, name)
            } else {
                pattern.eq_ignore_ascii_case(name)
            }
        };
        let denies = |pattern: &str| {
            if is_mcp_tool_name(pattern) {
                denies_tool_pattern(pattern, name)
            } else {
                pattern.eq_ignore_ascii_case(name)
            }
        };
        let enabled = self.enabled.is_empty() || self.enabled.iter().any(|p| matches(p));
        !enabled || self.disabled.iter().any(|p| denies(p))
    }
}

pub fn is_mcp_tool_name(name: &str) -> bool {
    name.starts_with("mcp__")
}

pub fn matches_tool_pattern(pattern: &str, name: &str) -> bool {
    if !is_mcp_tool_name(pattern) {
        return pattern == name;
    }
    glob_match(pattern, name)
}

pub fn matches_profile_tool_pattern(pattern: &str, tool_name: &str) -> bool {
    if pattern.starts_with("mcp__") {
        matches_tool_pattern(pattern, tool_name)
    } else {
        pattern.eq_ignore_ascii_case(tool_name)
    }
}

static MCP_GLOB_CACHE: LazyLock<Mutex<HashMap<String, Option<globset::GlobMatcher>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn mcp_glob_matcher(pattern: &str) -> Option<globset::GlobMatcher> {
    if let Some(cached) = MCP_GLOB_CACHE
        .lock()
        .ok()
        .and_then(|cache| cache.get(pattern).cloned())
    {
        return cached;
    }
    let compiled = match globset::GlobBuilder::new(pattern)
        .case_insensitive(false)
        .build()
    {
        Ok(glob) => Some(glob.compile_matcher()),
        Err(error) => {
            tracing::error!(
                pattern,
                %error,
                "tool policy glob could not be compiled; no tool may match this pattern"
            );
            None
        }
    };
    if let Ok(mut cache) = MCP_GLOB_CACHE.lock() {
        cache.insert(pattern.to_string(), compiled.clone());
    }
    compiled
}

fn glob_match(pattern: &str, name: &str) -> bool {
    mcp_glob_matcher(pattern).is_some_and(|matcher| matcher.is_match(name))
}

fn denies_tool_pattern(pattern: &str, name: &str) -> bool {
    mcp_glob_matcher(pattern).is_none_or(|matcher| matcher.is_match(name))
}

fn has_glob_magic(pattern: &str) -> bool {
    pattern
        .chars()
        .any(|c| matches!(c, '*' | '?' | '[' | ']' | '{' | '}'))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolSource {
    #[default]
    Builtin,
    User,
    Mcp,
}

impl ToolSource {
    pub fn of_name(name: &str) -> Self {
        if is_mcp_tool_name(name) {
            Self::Mcp
        } else {
            Self::Builtin
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ToolActivationPolicy {
    #[serde(default)]
    pub tools: Option<Vec<String>>,
    #[serde(default)]
    pub disallowed_tools: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GlobalToolsPolicy {
    #[serde(default)]
    pub enabled: Option<Vec<String>>,
    #[serde(default)]
    pub disabled: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ToolPolicyLayers {
    #[serde(default)]
    pub workspace_disabled_tools: Option<Vec<String>>,
    #[serde(default)]
    pub profile: ToolActivationPolicy,
    #[serde(default)]
    pub global: Option<GlobalToolsPolicy>,
    #[serde(default)]
    pub session_disabled_tools: Option<Vec<String>>,
}

impl From<&ToolsFilter> for GlobalToolsPolicy {
    fn from(filter: &ToolsFilter) -> Self {
        Self {
            enabled: (!filter.enabled.is_empty()).then(|| filter.enabled.clone()),
            disabled: (!filter.disabled.is_empty()).then(|| filter.disabled.clone()),
        }
    }
}

struct PolicyRef<'a> {
    tools: Option<&'a [String]>,
    disallowed_tools: Option<&'a [String]>,
}

impl<'a> From<&'a ToolActivationPolicy> for PolicyRef<'a> {
    fn from(policy: &'a ToolActivationPolicy) -> Self {
        Self {
            tools: policy.tools.as_deref(),
            disallowed_tools: policy.disallowed_tools.as_deref(),
        }
    }
}

pub fn is_tool_active(policy: &ToolActivationPolicy, name: &str, source: ToolSource) -> bool {
    is_tool_active_ref(policy.into(), name, source)
}

fn is_tool_active_ref(policy: PolicyRef<'_>, name: &str, source: ToolSource) -> bool {
    if let Some(tools) = policy.tools {
        let allowed = if source == ToolSource::Mcp {
            tools
                .iter()
                .filter(|pattern| is_mcp_tool_name(pattern.as_str()))
                .any(|pattern| glob_match(pattern, name))
        } else {
            tools.iter().any(|pattern| pattern.as_str() == name)
        };
        if !allowed {
            return false;
        }
    }
    let Some(disallowed_tools) = policy.disallowed_tools else {
        return true;
    };
    if source != ToolSource::Mcp {
        return !disallowed_tools
            .iter()
            .any(|pattern| pattern.as_str() == name);
    }
    !disallowed_tools
        .iter()
        .filter(|pattern| is_mcp_tool_name(pattern.as_str()))
        .any(|pattern| denies_tool_pattern(pattern, name))
}

pub fn is_tool_active_composed(layers: &ToolPolicyLayers, name: &str, source: ToolSource) -> bool {
    let global = layers.global.as_ref();
    let global_layer = PolicyRef {
        tools: global
            .and_then(|policy| policy.enabled.as_deref())
            .filter(|enabled| !enabled.is_empty()),
        disallowed_tools: global.and_then(|policy| policy.disabled.as_deref()),
    };
    let workspace_layer = PolicyRef {
        tools: None,
        disallowed_tools: layers.workspace_disabled_tools.as_deref(),
    };
    let session_layer = PolicyRef {
        tools: None,
        disallowed_tools: layers.session_disabled_tools.as_deref(),
    };
    is_tool_active_ref(workspace_layer, name, source)
        && is_tool_active_ref((&layers.profile).into(), name, source)
        && is_tool_active_ref(global_layer, name, source)
        && is_tool_active_ref(session_layer, name, source)
}

pub fn resolve_active_tool_names(policy: &ToolActivationPolicy) -> Option<Vec<String>> {
    let tools = policy.tools.as_ref()?;
    Some(
        tools
            .iter()
            .filter(|pattern| {
                is_tool_active(
                    policy,
                    pattern.as_str(),
                    ToolSource::of_name(pattern.as_str()),
                )
            })
            .cloned()
            .collect(),
    )
}

/// Narrow an allowlist to what a bound agent profile permits. An empty
/// allowlist adopts the profile's list outright; a non-empty one keeps only
/// entries the profile also allows. MCP entries compare as globs, other names
/// exactly (v2 `resolveActiveToolNames` composed with the session filter).
pub fn restrict_to_profile(enabled: &mut Vec<String>, profile_tools: &[String]) {
    if profile_tools.is_empty() {
        return;
    }
    if enabled.is_empty() {
        *enabled = profile_tools.to_vec();
        return;
    }
    enabled.retain(|pattern| {
        if is_mcp_tool_name(pattern.as_str()) {
            profile_tools
                .iter()
                .any(|allowed| is_mcp_tool_name(allowed.as_str()) && glob_match(allowed, pattern))
        } else {
            profile_tools.iter().any(|allowed| allowed == pattern)
        }
    });
}

pub fn literal_tool_names(patterns: &[String]) -> Vec<String> {
    patterns
        .iter()
        .filter(|pattern| {
            let pattern = pattern.as_str();
            !is_mcp_tool_name(pattern) && !has_glob_magic(pattern)
        })
        .cloned()
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InactiveToolPatternKind {
    WildcardNotMcp,

    IncompleteMcpName,

    UnknownTool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InactiveToolPattern {
    pub pattern: String,
    pub kind: InactiveToolPatternKind,
}

pub fn find_inactive_tool_patterns(
    patterns: &[String],
    is_known_tool_name: Option<&dyn Fn(&str) -> bool>,
) -> Vec<InactiveToolPattern> {
    let mut issues = Vec::new();
    for pattern in patterns {
        if is_mcp_tool_name(pattern) {
            if !has_glob_magic(pattern) && !&pattern["mcp__".len()..].contains("__") {
                issues.push(InactiveToolPattern {
                    pattern: pattern.clone(),
                    kind: InactiveToolPatternKind::IncompleteMcpName,
                });
            }
            continue;
        }
        if has_glob_magic(pattern) {
            issues.push(InactiveToolPattern {
                pattern: pattern.clone(),
                kind: InactiveToolPatternKind::WildcardNotMcp,
            });
            continue;
        }
        if let Some(is_known_tool_name) = is_known_tool_name
            && !is_known_tool_name(pattern)
        {
            issues.push(InactiveToolPattern {
                pattern: pattern.clone(),
                kind: InactiveToolPatternKind::UnknownTool,
            });
        }
    }
    issues
}

fn subagent_defs(
    pool: Option<&crate::subagent::secondary::SecondaryModelRuntime>,
) -> Vec<ToolInfo> {
    vec![
        crate::tools::agent_tool::agent_tool_def(pool),
        crate::tools::swarm_tool::agent_swarm_tool_def(pool),
    ]
}

pub fn native_tool_defs(
    github_available: bool,
    tower_enabled: bool,
    filter: Option<&ToolsFilter>,
    secondary_model: Option<&crate::subagent::secondary::SecondaryModelRuntime>,
) -> Vec<ToolInfo> {
    native_tool_defs_with_notify(
        github_available,
        tower_enabled,
        true,
        filter,
        secondary_model,
    )
}

pub fn native_tool_defs_with_notify(
    github_available: bool,
    tower_enabled: bool,
    notify_user_enabled: bool,
    filter: Option<&ToolsFilter>,
    secondary_model: Option<&crate::subagent::secondary::SecondaryModelRuntime>,
) -> Vec<ToolInfo> {
    let mut defs = crate::tools::core_tool_defs::core_tool_defs();
    defs.extend(subagent_defs(secondary_model));
    defs.push(crate::tools::todo_list::todo_list_tool_def());
    defs.push(crate::tools::ask_user_question::ask_user_question_tool_def());
    defs.push(crate::tools::exit_plan_mode::exit_plan_mode_tool_def());
    defs.push(crate::tools::spec_mode::enter_spec_mode_tool_def());
    defs.push(crate::tools::spec_mode::exit_spec_mode_tool_def());
    defs.push(crate::tools::get_goal::get_goal_tool_def());
    defs.push(crate::tools::create_goal::create_goal_tool_def());
    defs.push(crate::tools::goal_tools::update_goal_tool_def());
    defs.push(crate::tools::goal_tools::set_goal_budget_tool_def());
    defs.push(crate::tools::cron_tools::cron_list_tool_def());
    defs.push(crate::tools::cron_tools::cron_create_tool_def());
    defs.push(crate::tools::cron_tools::cron_delete_tool_def());
    defs.push(crate::tools::task_tools::task_list_tool_def());
    defs.push(crate::tools::task_tools::task_output_tool_def());
    defs.push(crate::tools::task_tools::task_stop_tool_def());
    defs.push(crate::tools::task_tools::wait_for_tool_def());
    defs.push(crate::tools::skill::skill_tool_def());
    defs.push(crate::tools::knowledge_tool::knowledge_tool_def());

    if crate::tools::memory_action::memory_tool_enabled() {
        defs.push(crate::tools::core_tool_defs::memory_tool_def());
    }
    defs.push(crate::tools::workflow::workflow_tool_def());
    defs.push(crate::tools::run_code::run_code_tool_def());

    defs.push(crate::tools::core_tool_defs::session_query_tool_def());
    if github_available {
        defs.extend(crate::tools::github::github_tool_defs());
    }
    if notify_user_enabled {
        defs.push(crate::tools::core_tool_defs::notify_user_tool_def());
    }
    if tower_enabled {
        defs.extend(crate::tools::tower::tower_tool_defs_with_models(
            secondary_model,
        ));
    }
    match filter {
        Some(filter) => filter.apply(defs),
        None => defs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def_names(defs: &[ToolInfo]) -> Vec<&str> {
        defs.iter().map(|d| d.name.as_str()).collect()
    }

    #[test]
    fn empty_filter_allows_everything() {
        let filter = ToolsFilter::default();
        assert!(!filter.is_active());
        assert!(filter.allows("Read"));
        assert!(filter.allows("mcp__github__search"));
    }

    #[test]
    fn allowlist_intersects_and_denylist_subtracts() {
        let filter = ToolsFilter {
            enabled: vec!["Read".into(), "Bash".into()],
            disabled: vec!["Bash".into()],
        };
        assert!(filter.allows("Read"));
        assert!(!filter.allows("Bash"), "denylist wins over the allowlist");
        assert!(!filter.allows("Grep"), "not in the allowlist");
    }

    #[test]
    fn builtin_names_match_exactly_and_case_sensitively() {
        let filter = ToolsFilter {
            enabled: Vec::new(),
            disabled: vec!["read".into(), "Bash(rm -rf*)".into()],
        };
        assert!(filter.allows("Read"), "built-ins are case-sensitive");
        assert!(
            filter.allows("Bash"),
            "argument patterns do not match names"
        );
        assert!(
            filter.allows("mcp__github__read"),
            "non-mcp patterns do not glob"
        );
        assert!(!filter.allows("read"));
    }

    #[test]
    fn mcp_globs_match_the_qualified_name() {
        let filter = ToolsFilter {
            enabled: Vec::new(),
            disabled: vec!["mcp__github__*".into()],
        };
        assert!(!filter.allows("mcp__github__search"));
        assert!(filter.allows("mcp__slack__search"));
        assert!(
            filter.allows("mcp__github"),
            "glob requires the tool segment"
        );

        let bare = ToolsFilter {
            enabled: Vec::new(),
            disabled: vec!["mcp__*".into()],
        };
        assert!(!bare.allows("mcp__github__search"));
    }

    #[test]
    fn mcp_middle_glob_matches() {
        assert!(glob_match("mcp__*__search", "mcp__github__search"));
        assert!(!glob_match("mcp__*__read", "mcp__github__search"));
        assert!(glob_match("*search", "mcp__github__search"));
        assert!(glob_match("mcp__github__*", "mcp__github__search"));
    }

    #[test]
    fn mcp_globs_support_the_full_picomatch_syntax() {
        assert!(glob_match(
            "mcp__github__create_issu?",
            "mcp__github__create_issue"
        ));
        assert!(!glob_match(
            "mcp__github__create_issu?",
            "mcp__github__create_issues"
        ));

        assert!(glob_match("mcp__github__tool_[0-9]", "mcp__github__tool_7"));
        assert!(!glob_match(
            "mcp__github__tool_[0-9]",
            "mcp__github__tool_x"
        ));

        assert!(glob_match(
            "mcp__github__{read,write}",
            "mcp__github__write"
        ));
        assert!(!glob_match(
            "mcp__github__{read,write}",
            "mcp__github__delete"
        ));

        assert!(glob_match("mcp__github__**", "mcp__github__create_issue"));
        assert!(glob_match("mcp__**", "mcp__github__search"));

        let filter = ToolsFilter {
            enabled: Vec::new(),
            disabled: vec!["mcp__github__tool_?".into()],
        };
        assert!(!filter.allows("mcp__github__tool_7"));
        assert!(filter.allows("mcp__github__tool_77"), "`?` is one char");

        assert!(!filter.allows("mcp__github__tool_x"));

        let builtin = ToolsFilter {
            enabled: Vec::new(),
            disabled: vec!["Bash?".into()],
        };
        assert!(builtin.allows("Bash1"), "built-in patterns never glob");
    }

    #[test]
    fn non_mcp_names_match_exactly_and_case_sensitively() {
        let lowercase_denylist = ToolActivationPolicy {
            tools: None,
            disallowed_tools: Some(vec!["read".into()]),
        };
        assert!(is_tool_active(
            &lowercase_denylist,
            "Read",
            ToolSource::Builtin
        ));

        let allowlist = ToolActivationPolicy {
            tools: Some(vec!["Read".into()]),
            disallowed_tools: None,
        };
        assert!(is_tool_active(&allowlist, "Read", ToolSource::Builtin));
        assert!(
            !is_tool_active(&allowlist, "read", ToolSource::Builtin),
            "the allowlist is exact too"
        );

        let mcp_allowlist = ToolActivationPolicy {
            tools: Some(vec!["Read".into(), "mcp__github__*".into()]),
            disallowed_tools: None,
        };
        assert!(is_tool_active(
            &mcp_allowlist,
            "mcp__github__search",
            ToolSource::Mcp
        ));
        assert!(
            !is_tool_active(&mcp_allowlist, "mcp__github__search", ToolSource::Builtin),
            "a non-MCP source compares literally, never by glob"
        );
    }

    #[test]
    fn call_enforcement_keeps_the_wire_name_case_insensitive() {
        let filter = ToolsFilter {
            enabled: Vec::new(),
            disabled: vec!["read".into()],
        };

        assert!(filter.allows("Read"), "allows() is case-sensitive");
        assert!(!filter.allows("read"), "the lowercase name matches exactly");

        assert!(filter.blocks_call("Read"));
        assert!(filter.blocks_call("read"));
    }

    #[test]
    fn an_uncompilable_glob_never_falls_back_to_a_literal_match() {
        let disabled = ToolsFilter {
            enabled: Vec::new(),
            disabled: vec!["mcp__[".into()],
        };
        assert!(disabled.blocks_call("mcp__github__search"));
        assert!(!disabled.allows("mcp__github__search"));

        let enabled = ToolsFilter {
            enabled: vec!["mcp__[".into()],
            disabled: Vec::new(),
        };
        assert!(!enabled.allows("mcp__github__search"));
        assert!(enabled.blocks_call("mcp__github__search"));
    }

    #[test]
    fn composed_layers_apply_every_v2_layer() {
        let layers = ToolPolicyLayers {
            workspace_disabled_tools: Some(vec!["Bash".into()]),
            profile: ToolActivationPolicy {
                tools: Some(vec!["Bash".into(), "Read".into()]),
                disallowed_tools: None,
            },
            global: Some(GlobalToolsPolicy {
                enabled: Some(vec!["Bash".into(), "Read".into()]),
                disabled: None,
            }),
            session_disabled_tools: Some(Vec::new()),
        };
        assert!(
            !is_tool_active_composed(&layers, "Bash", ToolSource::Builtin),
            "the workspace layer vetoes what every other layer allows"
        );
        assert!(is_tool_active_composed(
            &layers,
            "Read",
            ToolSource::Builtin
        ));
        assert!(
            !is_tool_active_composed(&layers, "Grep", ToolSource::Builtin),
            "the global allowlist still applies"
        );

        let workspace_glob = ToolPolicyLayers {
            workspace_disabled_tools: Some(vec!["mcp__blocked__*".into()]),
            ..Default::default()
        };
        assert!(!is_tool_active_composed(
            &workspace_glob,
            "mcp__blocked__write",
            ToolSource::Mcp
        ));
        assert!(is_tool_active_composed(
            &workspace_glob,
            "mcp__allowed__write",
            ToolSource::Mcp
        ));

        let session_veto = ToolPolicyLayers {
            session_disabled_tools: Some(vec!["Bash".into()]),
            ..Default::default()
        };
        assert!(!is_tool_active_composed(
            &session_veto,
            "Bash",
            ToolSource::Builtin
        ));

        let empty_global = ToolPolicyLayers {
            global: Some(GlobalToolsPolicy {
                enabled: Some(Vec::new()),
                disabled: None,
            }),
            ..Default::default()
        };
        assert!(is_tool_active_composed(
            &empty_global,
            "Read",
            ToolSource::Builtin
        ));

        let empty_profile = ToolPolicyLayers {
            profile: ToolActivationPolicy {
                tools: Some(Vec::new()),
                disallowed_tools: None,
            },
            ..Default::default()
        };
        assert!(!is_tool_active_composed(
            &empty_profile,
            "Read",
            ToolSource::Builtin
        ));

        let legacy = ToolsFilter {
            enabled: vec!["Read".into()],
            disabled: Vec::new(),
        };
        let from_table = ToolPolicyLayers {
            global: Some(GlobalToolsPolicy::from(&legacy)),
            ..Default::default()
        };
        assert!(is_tool_active_composed(
            &from_table,
            "Read",
            ToolSource::Builtin
        ));
        assert!(!is_tool_active_composed(
            &from_table,
            "Grep",
            ToolSource::Builtin
        ));
    }

    #[test]
    fn resolve_active_tool_names_drops_the_denied_ones() {
        let policy = ToolActivationPolicy {
            tools: Some(vec!["Read".into(), "Bash".into(), "mcp__github__*".into()]),
            disallowed_tools: Some(vec!["Bash".into()]),
        };
        assert_eq!(
            resolve_active_tool_names(&policy),
            Some(vec!["Read".to_string(), "mcp__github__*".to_string()])
        );
        assert_eq!(
            resolve_active_tool_names(&ToolActivationPolicy::default()),
            None,
            "no allowlist means unrestricted, not empty"
        );
        assert_eq!(
            resolve_active_tool_names(&ToolActivationPolicy {
                tools: Some(Vec::new()),
                disallowed_tools: None,
            }),
            Some(Vec::new())
        );
    }

    #[test]
    fn literal_names_exclude_mcp_and_glob_patterns() {
        let patterns: Vec<String> = ["Read", "mcp__*", "Bash*", "mcp__github__create_issue"]
            .into_iter()
            .map(str::to_string)
            .collect();
        assert_eq!(literal_tool_names(&patterns), vec!["Read".to_string()]);
    }

    #[test]
    fn inactive_patterns_report_every_kind() {
        fn known(name: &str) -> bool {
            matches!(name, "Read" | "Bash" | "Skill")
        }
        let known: &dyn Fn(&str) -> bool = &known;
        let passing: Vec<String> = [
            "Read",
            "Bash",
            "mcp__github__*",
            "mcp__*",
            "mcp__github__create_issue",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        assert!(find_inactive_tool_patterns(&passing, Some(known)).is_empty());

        let flagged: Vec<String> = ["Bashh", "read", "*", "Bash*", "mcp__github", "mcp__"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let issues = find_inactive_tool_patterns(&flagged, Some(known));
        let kinds: Vec<InactiveToolPatternKind> = issues.iter().map(|issue| issue.kind).collect();
        assert_eq!(
            kinds,
            vec![
                InactiveToolPatternKind::UnknownTool,
                InactiveToolPatternKind::UnknownTool,
                InactiveToolPatternKind::WildcardNotMcp,
                InactiveToolPatternKind::WildcardNotMcp,
                InactiveToolPatternKind::IncompleteMcpName,
                InactiveToolPatternKind::IncompleteMcpName,
            ]
        );
        assert_eq!(issues[0].pattern, "Bashh");

        assert!(
            find_inactive_tool_patterns(&["AnythingGoes".to_string()], None).is_empty(),
            "no vocabulary means no unknown-tool verdict"
        );
    }

    #[test]
    fn call_enforcement_is_case_insensitive_for_builtins() {
        let filter = ToolsFilter {
            enabled: Vec::new(),
            disabled: vec!["Read".into()],
        };
        assert!(filter.blocks_call("read"), "lowercase wire name is blocked");
        assert!(filter.blocks_call("Read"));
        assert!(!filter.blocks_call("Grep"));

        let allow_only_read = ToolsFilter {
            enabled: vec!["Read".into()],
            disabled: Vec::new(),
        };
        assert!(!allow_only_read.blocks_call("read"));
        assert!(allow_only_read.blocks_call("Bash"));
    }

    #[test]
    fn native_tool_defs_expose_the_orchestration_surface() {
        let defs = native_tool_defs(false, false, None, None);
        let names = def_names(&defs);
        for expected in [
            "Read",
            "Grep",
            "Glob",
            "Write",
            "Edit",
            "Bash",
            "FetchURL",
            "WebSearch",
            "Agent",
            "AgentSwarm",
            "TodoList",
            "AskUserQuestion",
            "ExitPlanMode",
            "EnterSpecMode",
            "ExitSpecMode",
            "GetGoal",
            "CreateGoal",
            "UpdateGoal",
            "SetGoalBudget",
            "CronList",
            "CronCreate",
            "CronDelete",
            "TaskList",
            "TaskOutput",
            "TaskStop",
            "WaitFor",
            "Skill",
            "Knowledge",
            "Workflow",
            "run_code",
            "session_query",
        ] {
            assert!(
                names.contains(&expected),
                "missing tool definition: {expected}"
            );
        }
        assert!(!names.contains(&"TowerInit"), "tower tools are gated");
        assert!(
            !names.contains(&"lsp"),
            "tower/lsp stay out of the advertised table"
        );
    }

    #[test]
    fn native_tool_defs_apply_the_filter() {
        let filter = ToolsFilter {
            enabled: Vec::new(),
            disabled: vec!["WebSearch".into(), "mcp__github__*".into()],
        };
        let defs = native_tool_defs(false, false, Some(&filter), None);
        let names = def_names(&defs);
        assert!(!names.contains(&"WebSearch"));
        assert!(names.contains(&"FetchURL"));

        let allow_only_read = ToolsFilter {
            enabled: vec!["Read".into()],
            disabled: Vec::new(),
        };
        let defs = native_tool_defs(false, false, Some(&allow_only_read), None);
        assert_eq!(def_names(&defs), vec!["Read"]);
    }

    #[test]
    fn github_and_tower_defs_follow_their_switches() {
        let github_defs = native_tool_defs(true, false, None, None);
        let with_github = def_names(&github_defs);
        assert!(
            with_github.iter().any(|name| name.starts_with("GitHub")),
            "github tools are advertised when a token is configured"
        );
        let tower_defs = native_tool_defs(false, true, None, None);
        let with_tower = def_names(&tower_defs);
        assert!(with_tower.contains(&"TowerInit"));
    }
}
