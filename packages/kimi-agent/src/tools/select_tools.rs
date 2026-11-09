use std::collections::HashSet;

use serde_json::{Value, json};

use crate::tools::tool_policy;
use crate::turn_loop::types::ExecutableToolResult;

pub const SELECT_TOOLS_TOOL_NAME: &str = "select_tools";

pub const SELECT_TOOLS_DESCRIPTION: &str = "\
Load tools by exact name, or search for them by capability when the name is unknown. \
The loadable names are listed in the <tools_added>/<tools_removed> or <tools_relevant> \
announcements in the system context — use the latest announcement. \
If you have not seen a loadable-tools announcement in this session, no tools are currently loadable. \
Pass names to load them, or pass query to search all loadable tools; a search returns \
candidates only, so call select_tools again with exact names to load the ones you need. \
The full definitions become available immediately, so you can call them directly \
in your next tool call. Tools you already have available are called directly, \
never passed to select_tools.";

pub fn select_tools_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "names": {
                "type": "array",
                "items": { "type": "string" },
                "minItems": 1,
                "description": "Exact tool names to load, taken from the latest announcement or a prior search result."
            },
            "query": {
                "type": "string",
                "minLength": 1,
                "description": "Capability description to search for when the exact tool name is unknown; returns candidates without loading them."
            }
        },
        "additionalProperties": false
    })
}

pub const SELECT_TOOLS_PROVIDE_NAMES_OR_QUERY: &str = "Provide names to load or a query to search.";

pub const SELECT_TOOLS_UNAVAILABLE: &str = "select_tools is not available for the current model.";

pub fn text_result(content: &str, is_error: bool) -> ExecutableToolResult {
    ExecutableToolResult {
        spill: None,
        parts: Vec::new(),
        delivery: None,
        stop_turn: false,
        content: content.to_string(),
        is_error,
        note: None,
        display: None,
    }
}

pub fn search_output(query: &str, hits: &[crate::tools::tool_retriever::ToolSearchHit]) -> String {
    if hits.is_empty() {
        return format!(
            "No loadable tools matched \"{query}\". Try a different capability description \
or use a tool you already have."
        );
    }
    let mut lines = vec![format!("Search results for \"{query}\":")];
    for hit in hits {
        let description = hit
            .description
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let description: String = description.chars().take(120).collect();
        if description.is_empty() {
            lines.push(hit.name.clone());
        } else {
            lines.push(format!("{} — {}", hit.name, description));
        }
    }
    lines.push(
        "These are candidates. Tools you already have can be called directly; \
call select_tools with exact names to load the others."
            .into(),
    );
    lines.join("\n")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadToolsResult {
    pub to_load: Vec<String>,
    pub already_available: Vec<String>,

    pub already_callable: Vec<String>,

    pub stale_loaded: Vec<String>,

    pub policy_denied: Vec<String>,
    pub unknown: Vec<String>,

    pub escalated: Vec<(String, usize)>,

    pub consecutive_failures: usize,

    pub loadable: Vec<String>,
}

pub const STOP_RETRY_THRESHOLD: usize = 2;

#[derive(Debug, Default)]
pub struct SelectionFailureTracker {
    failed_attempts: std::collections::HashMap<String, usize>,
    consecutive_failures: usize,
    environment: Option<String>,
}

impl SelectionFailureTracker {
    pub fn reset_if_environment_changed(&mut self, environment: String) {
        if self.environment.as_deref() == Some(environment.as_str()) {
            return;
        }
        self.environment = Some(environment);
        self.consecutive_failures = 0;
        self.failed_attempts.clear();
    }

    fn record(&mut self, succeeded: &[String], failed: &[String]) -> (Vec<(String, usize)>, usize) {
        self.consecutive_failures = if succeeded.is_empty() {
            self.consecutive_failures + 1
        } else {
            0
        };
        for name in succeeded {
            self.failed_attempts.remove(&name.to_ascii_lowercase());
        }
        let mut escalated = Vec::new();
        let mut counted_this_call = HashSet::new();
        for name in failed {
            let key = name.to_ascii_lowercase();
            if !counted_this_call.insert(key.clone()) {
                continue;
            }
            let attempts = self.failed_attempts.get(&key).copied().unwrap_or(0) + 1;
            self.failed_attempts.insert(key, attempts);
            if attempts >= STOP_RETRY_THRESHOLD {
                escalated.push((name.clone(), attempts));
            }
        }
        (escalated, self.consecutive_failures)
    }
}

pub fn selection_environment_fingerprint(
    policy: Option<&tool_policy::ToolsFilter>,
    registered_tools: &HashSet<String>,
    loadable_tools: &HashSet<String>,
) -> String {
    let sorted_names = |set: &HashSet<String>| {
        let mut names: Vec<String> = set.iter().cloned().collect();
        names.sort();
        names
    };
    let mut enabled = policy.map(|f| f.enabled.clone()).unwrap_or_default();
    enabled.sort();
    enabled.dedup();
    let mut disabled = policy.map(|f| f.disabled.clone()).unwrap_or_default();
    disabled.sort();
    disabled.dedup();
    json!([
        enabled,
        disabled,
        sorted_names(registered_tools),
        sorted_names(loadable_tools),
    ])
    .to_string()
}

pub fn evaluate_load_tools(
    requested: &[String],
    loadable_tools: &HashSet<String>,
    registered_tools: &HashSet<String>,
    currently_loaded: &HashSet<String>,
    policy: Option<&tool_policy::ToolsFilter>,
    tracker: &mut SelectionFailureTracker,
) -> LoadToolsResult {
    let is_denied = |name: &str| {
        policy.is_some_and(|filter| {
            filter
                .disabled
                .iter()
                .any(|pattern| tool_policy::matches_tool_pattern(pattern, name))
        })
    };

    let is_active = |name: &str| policy.is_none_or(|filter| filter.allows(name));

    let mut to_load = Vec::new();
    let mut already_available = Vec::new();
    let mut already_callable = Vec::new();
    let mut stale_loaded = Vec::new();
    let mut policy_denied = Vec::new();
    let mut unknown = Vec::new();

    let mut seen = HashSet::new();
    for name in requested {
        if !seen.insert(name) {
            continue;
        }
        let registered = registered_tools.contains(name);
        if is_denied(name) || (registered && !is_active(name)) {
            policy_denied.push(name.clone());
        } else if loadable_tools.contains(name) {
            if currently_loaded.contains(name) {
                already_available.push(name.clone());
            } else {
                to_load.push(name.clone());
            }
        } else if registered {
            already_callable.push(name.clone());
        } else if currently_loaded.contains(name) {
            stale_loaded.push(name.clone());
        } else {
            unknown.push(name.clone());
        }
    }

    let mut succeeded = to_load.clone();
    succeeded.extend(already_available.iter().cloned());
    succeeded.extend(already_callable.iter().cloned());
    let mut failed = stale_loaded.clone();
    failed.extend(policy_denied.iter().cloned());
    failed.extend(unknown.iter().cloned());
    let (escalated, consecutive_failures) = tracker.record(&succeeded, &failed);

    let mut loadable: Vec<String> = loadable_tools
        .iter()
        .filter(|name| !currently_loaded.contains(*name))
        .cloned()
        .collect();
    loadable.sort();

    LoadToolsResult {
        to_load,
        already_available,
        already_callable,
        stale_loaded,
        policy_denied,
        unknown,
        escalated,
        consecutive_failures,
        loadable,
    }
}

fn suggest_tool_names(name: &str, pool: &[String]) -> Vec<String> {
    let lower = name.to_ascii_lowercase();
    let mut case_fix: Vec<String> = pool
        .iter()
        .filter(|candidate| candidate.to_ascii_lowercase() == lower)
        .cloned()
        .collect();
    case_fix.sort();
    if !case_fix.is_empty() {
        case_fix.truncate(3);
        return case_fix;
    }
    let mut matches = HashSet::new();
    let stripped = lower.strip_prefix("mcp__").unwrap_or(&lower);
    if stripped.len() >= 3 {
        for candidate in pool {
            if candidate.to_ascii_lowercase().contains(stripped) {
                matches.insert(candidate.clone());
            }
        }
    }
    if matches.is_empty() {
        for candidate in pool {
            let last = candidate.rsplit("__").next().unwrap_or(candidate);
            if last.len() >= 3 && lower.contains(&last.to_ascii_lowercase()) {
                matches.insert(candidate.clone());
            }
        }
    }
    let mut sorted: Vec<String> = matches.into_iter().collect();
    sorted.sort();
    sorted.truncate(3);
    sorted
}

pub fn execute_select_tools(
    args: &Value,
    loadable_tools: &HashSet<String>,
    registered_tools: &HashSet<String>,
    currently_loaded: &mut HashSet<String>,
    policy: Option<&tool_policy::ToolsFilter>,
    tracker: &mut SelectionFailureTracker,
) -> ExecutableToolResult {
    let names: Vec<String> = match args.get("names").and_then(|v| v.as_array()) {
        Some(arr) => arr
            .iter()
            .filter_map(|item| item.as_str().map(|s| s.trim().to_string()))
            .filter(|s| !s.is_empty())
            .collect(),
        None => {
            return ExecutableToolResult {
                spill: None,
                parts: Vec::new(),
                delivery: None,
                stop_turn: false,
                content: "Invalid arguments: 'names' array is required.".into(),
                is_error: true,
                note: None,
                display: None,
            };
        }
    };

    if names.is_empty() {
        return ExecutableToolResult {
            spill: None,
            parts: Vec::new(),
            delivery: None,
            stop_turn: false,
            content: "Invalid arguments: 'names' must contain at least one tool name.".into(),
            is_error: true,
            note: None,
            display: None,
        };
    }

    let result = evaluate_load_tools(
        &names,
        loadable_tools,
        registered_tools,
        currently_loaded,
        policy,
        tracker,
    );

    let mut suggestion_pool: Vec<String> = loadable_tools.iter().cloned().collect();
    suggestion_pool.sort();

    let mut lines = Vec::new();
    if !result.to_load.is_empty() {
        lines.push(format!("Loaded: {}", result.to_load.join(", ")));
        for name in &result.to_load {
            currently_loaded.insert(name.clone());
        }
    }
    if !result.already_available.is_empty() {
        lines.push(format!(
            "Already available: {}",
            result.already_available.join(", ")
        ));
    }
    for name in &result.already_callable {
        lines.push(format!(
            "\"{name}\" is already available — call it directly; \
select_tools is only for names in the <tools_added> announcements."
        ));
    }

    for name in &result.stale_loaded {
        lines.push(format!(
            "\"{name}\" was previously selected or loaded but is no longer registered. \
select_tools cannot restore it. Do not select it again unless a new \
<tools_added> announcement lists it."
        ));
    }
    for name in &result.policy_denied {
        lines.push(format!(
            "\"{name}\" is disabled by the current tool policy. \
select_tools cannot enable it. Do not retry unless the policy changes; \
use an available tool instead."
        ));
    }
    for name in &result.unknown {
        let reason = format!(
            "Unknown tool: {name}. This name is not in the current loadable tool list. \
Call select_tools with a query to search by capability, then load an exact name; \
or use an available tool. "
        );
        let candidates = suggest_tool_names(name, &suggestion_pool);
        if !candidates.is_empty() {
            lines.push(format!(
                "{reason}Did you mean: {}? Use an exact listed name.",
                candidates.join(", ")
            ));
        } else {
            let available = if result.loadable.is_empty() {
                "No additional tools can be loaded right now; use the tools you already have."
                    .to_string()
            } else if result.loadable.len() <= 5 {
                format!("Loadable tools: {}.", result.loadable.join(", "))
            } else {
                "Select only exact names from the current announcements.".to_string()
            };
            lines.push(format!(
                "{reason}{available} Do not guess aliases; use an available tool \
or tell the user what capability is missing."
            ));
        }
    }
    for (name, attempts) in &result.escalated {
        lines.push(format!(
            "\"{name}\" has already failed selection {attempts} times. \
Stop retrying this name until its availability changes."
        ));
    }
    if result.consecutive_failures >= 3 {
        lines.push(format!(
            "{} consecutive select_tools calls failed without making any tool available. \
Stop guessing names or retrying unavailable tools. Use only exact names from the current \
announcements, use tools already available, or tell the user what capability is missing.",
            result.consecutive_failures
        ));
    }

    let is_error = result.to_load.is_empty()
        && result.already_available.is_empty()
        && result.already_callable.is_empty();

    ExecutableToolResult {
        spill: None,
        parts: Vec::new(),
        delivery: None,
        stop_turn: false,
        content: lines.join("\n"),
        is_error,
        note: None,
        display: None,
    }
}

pub fn not_loaded_tool_output(name: &str) -> String {
    format!(
        "Tool \"{name}\" is available but not loaded. \
Call select_tools with [\"{name}\"] first, then call the tool."
    )
}

pub fn missing_tool_output(name: &str) -> String {
    if name.starts_with("mcp__") {
        return format!(
            "Tool \"{name}\" was loaded but its MCP server is currently disconnected. \
It may become available again when the server reconnects; do not retry immediately."
        );
    }
    format!(
        "Tool \"{name}\" was loaded but is no longer registered. \
Do not retry it unless it becomes available again."
    )
}

pub fn inactive_loaded_tool_output(name: &str) -> String {
    format!(
        "Tool \"{name}\" was loaded but is no longer active. \
Ask the user to enable it before calling it again."
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolRegistration {
    Inline,

    Deferred,

    Missing,
}

pub fn missing_refusal(
    enabled: bool,
    tool_name: &str,
    registration: ToolRegistration,
    loaded: bool,
) -> Option<String> {
    (enabled && loaded && registration == ToolRegistration::Missing)
        .then(|| missing_tool_output(tool_name))
}

pub fn unavailable_refusal(
    enabled: bool,
    tool_name: &str,
    registration: ToolRegistration,
    loaded: bool,
) -> Option<String> {
    if !enabled || tool_name == SELECT_TOOLS_TOOL_NAME {
        return None;
    }
    match (registration, loaded) {
        (ToolRegistration::Inline, true) => Some(inactive_loaded_tool_output(tool_name)),
        (ToolRegistration::Deferred, false) => Some(not_loaded_tool_output(tool_name)),
        _ => None,
    }
}

pub fn render_loadable_tools_announcement(
    added: &[String],
    removed: &[String],
    has_loadable: bool,
) -> Option<String> {
    if added.is_empty() && removed.is_empty() && has_loadable {
        return None;
    }
    let mut sections = Vec::new();
    if !added.is_empty() {
        sections.push(format!(
            "<tools_added>\n{}\n</tools_added>",
            added.join("\n")
        ));
    }
    if !removed.is_empty() {
        sections.push(format!(
            "<tools_removed>\n{}\n</tools_removed>",
            removed.join("\n")
        ));
    }

    if !has_loadable {
        sections.push(
            "No tools are currently available for dynamic loading in this session. \
Do not call select_tools or guess tool names; use the tools you already have. \
Only a later <tools_added> announcement can make new tools loadable."
                .into(),
        );
        return Some(sections.join("\n\n"));
    }
    sections.push(
        "Use the select_tools tool with exact names to load full definitions of the \
announced tools before calling them. \
Only the announced names are loadable — tools you already have available are called \
directly, never passed to select_tools; plugin, skill, or category names do not work. \
Names listed as removed are no longer loadable — do not select them. \
Fold all announcements in this conversation in order to get the current list."
            .into(),
    );
    Some(sections.join("\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_select_tools_schema_validity() {
        let schema = select_tools_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["names"].is_object());
        assert!(schema["properties"]["query"].is_object());
        assert_eq!(schema["properties"]["names"]["minItems"], 1);
        assert_eq!(schema["properties"]["query"]["minLength"], 1);
        assert!(
            schema.get("required").is_none(),
            "v2 makes both arguments optional and enforces the either/or in code"
        );
        assert_eq!(schema["additionalProperties"], false);
    }

    #[test]
    fn test_search_output_matches_v2_shape() {
        assert_eq!(
            search_output("quantum", &[]),
            "No loadable tools matched \"quantum\". Try a different capability description \
or use a tool you already have."
        );

        let hits = vec![crate::tools::tool_retriever::ToolSearchHit {
            name: "mcp__github__search_issues".into(),
            description: "Search   issues\nand pull requests".into(),
            score: 1.0,
        }];
        assert_eq!(
            search_output("issues", &hits),
            "Search results for \"issues\":\n\
mcp__github__search_issues — Search issues and pull requests\n\
These are candidates. Tools you already have can be called directly; \
call select_tools with exact names to load the others."
        );

        let long = vec![crate::tools::tool_retriever::ToolSearchHit {
            name: "mcp__x__y".into(),
            description: "a".repeat(200),
            score: 1.0,
        }];
        let rendered = search_output("a", &long);
        let line = rendered.lines().nth(1).expect("the hit line");
        let description = line.split_once(" — ").expect("name — description").1;
        assert_eq!(description.chars().count(), 120, "the description is cut");

        let bare = vec![crate::tools::tool_retriever::ToolSearchHit {
            name: "mcp__x__y".into(),
            description: String::new(),
            score: 1.0,
        }];
        assert_eq!(search_output("y", &bare).lines().nth(1), Some("mcp__x__y"));
    }

    #[test]
    fn test_evaluate_load_tools() {
        let loadable = set(&["mcp__github__search", "mcp__github__create_pr"]);
        let registered = set(&["mcp__github__search", "mcp__github__create_pr", "Grep"]);
        let loaded = set(&["mcp__github__search"]);
        let mut tracker = SelectionFailureTracker::default();

        let requested = vec![
            "mcp__github__search".into(),
            "mcp__github__create_pr".into(),
            "unknown_tool".into(),
        ];

        let res = evaluate_load_tools(
            &requested,
            &loadable,
            &registered,
            &loaded,
            None,
            &mut tracker,
        );
        assert_eq!(res.already_available, vec!["mcp__github__search"]);
        assert_eq!(res.to_load, vec!["mcp__github__create_pr"]);
        assert_eq!(res.unknown, vec!["unknown_tool"]);
        assert!(res.policy_denied.is_empty());
        assert!(res.stale_loaded.is_empty());
        assert_eq!(res.loadable, vec!["mcp__github__create_pr"]);
    }

    #[test]
    fn test_already_callable_is_reported_not_loaded() {
        let loadable = set(&["mcp__github__search"]);
        let registered = set(&["mcp__github__search", "Grep"]);
        let loaded = HashSet::new();
        let mut tracker = SelectionFailureTracker::default();

        let res = evaluate_load_tools(
            &["Grep".into(), "mcp__github__search".into()],
            &loadable,
            &registered,
            &loaded,
            None,
            &mut tracker,
        );
        assert_eq!(res.already_callable, vec!["Grep"]);
        assert_eq!(res.to_load, vec!["mcp__github__search"]);
    }

    #[test]
    fn test_stale_loaded_is_not_reported_as_available() {
        let loadable = HashSet::new();
        let registered = set(&["Grep"]);
        let loaded = set(&["mcp__github__search"]);
        let mut tracker = SelectionFailureTracker::default();

        let res = evaluate_load_tools(
            &["mcp__github__search".into()],
            &loadable,
            &registered,
            &loaded,
            None,
            &mut tracker,
        );
        assert_eq!(res.stale_loaded, vec!["mcp__github__search"]);
        assert!(res.already_available.is_empty());
        assert_eq!(res.consecutive_failures, 1);
    }

    #[test]
    fn test_policy_denied_buckets() {
        let loadable = set(&["mcp__github__search"]);
        let registered = set(&["mcp__github__search", "Grep", "Bash"]);
        let loaded = HashSet::new();
        let filter = tool_policy::ToolsFilter {
            enabled: vec!["Grep".into()],
            disabled: vec!["mcp__github__*".into()],
        };
        let mut tracker = SelectionFailureTracker::default();

        let res = evaluate_load_tools(
            &[
                "mcp__github__search".into(),
                "mcp__github__get_issue".into(),
                "Bash".into(),
                "Grep".into(),
                "mcp__other__ping".into(),
            ],
            &loadable,
            &registered,
            &loaded,
            Some(&filter),
            &mut tracker,
        );
        assert_eq!(
            res.policy_denied,
            vec!["mcp__github__search", "mcp__github__get_issue", "Bash"]
        );
        assert_eq!(res.already_callable, vec!["Grep"]);
        assert!(res.to_load.is_empty());
        assert_eq!(res.unknown, vec!["mcp__other__ping"]);
    }

    #[test]
    fn test_failure_tracking_escalates_and_resets() {
        let loadable = set(&["mcp__github__search"]);
        let registered = set(&["mcp__github__search"]);
        let loaded = HashSet::new();
        let mut tracker = SelectionFailureTracker::default();

        let res1 = evaluate_load_tools(
            &["bogus".into()],
            &loadable,
            &registered,
            &loaded,
            None,
            &mut tracker,
        );
        assert!(res1.escalated.is_empty());
        assert_eq!(res1.consecutive_failures, 1);

        let res2 = evaluate_load_tools(
            &["bogus".into()],
            &loadable,
            &registered,
            &loaded,
            None,
            &mut tracker,
        );
        assert_eq!(res2.escalated, vec![("bogus".to_string(), 2)]);
        assert_eq!(res2.consecutive_failures, 2);

        let res2b = evaluate_load_tools(
            &["other".into(), "other".into()],
            &loadable,
            &registered,
            &loaded,
            None,
            &mut tracker,
        );
        assert!(res2b.escalated.is_empty());
        assert_eq!(res2b.consecutive_failures, 3);

        let res3 = evaluate_load_tools(
            &["mcp__github__search".into(), "bogus".into()],
            &loadable,
            &registered,
            &loaded,
            None,
            &mut tracker,
        );
        assert_eq!(res3.consecutive_failures, 0);
        assert_eq!(res3.escalated, vec![("bogus".to_string(), 3)]);

        tracker.reset_if_environment_changed("env-a".to_string());
        tracker.reset_if_environment_changed("env-a".to_string());
        let res4 = evaluate_load_tools(
            &["bogus".into()],
            &loadable,
            &registered,
            &loaded,
            None,
            &mut tracker,
        );
        assert_eq!(res4.consecutive_failures, 1);
        tracker.reset_if_environment_changed("env-b".to_string());
        let res5 = evaluate_load_tools(
            &["bogus".into()],
            &loadable,
            &registered,
            &loaded,
            None,
            &mut tracker,
        );

        assert!(res5.escalated.is_empty());
        assert_eq!(res5.consecutive_failures, 1);
    }

    #[test]
    fn test_suggest_tool_names() {
        let pool = vec![
            "mcp__github__search_code".into(),
            "mcp__github__create_pr".into(),
            "Grep".into(),
        ];

        assert_eq!(suggest_tool_names("grep", &pool), vec!["Grep"]);

        assert_eq!(
            suggest_tool_names("mcp__github__search", &pool),
            vec!["mcp__github__search_code"]
        );

        assert_eq!(
            suggest_tool_names("my_create_pr", &pool),
            vec!["mcp__github__create_pr"]
        );
        assert!(suggest_tool_names("zzz", &pool).is_empty());
    }

    #[test]
    fn test_execute_select_tools_lifecycle() {
        let loadable = set(&["tool_a", "tool_b"]);
        let registered = set(&["tool_a", "tool_b", "grep"]);
        let mut loaded = HashSet::new();
        let mut tracker = SelectionFailureTracker::default();

        let res1 = execute_select_tools(
            &json!({ "names": ["tool_a", "tool_a", "tool_b", "nonexistent"] }),
            &loadable,
            &registered,
            &mut loaded,
            None,
            &mut tracker,
        );
        assert!(!res1.is_error);
        assert_eq!(
            res1.content,
            "Loaded: tool_a, tool_b\n\
Unknown tool: nonexistent. This name is not in the current loadable tool list. \
Call select_tools with a query to search by capability, then load an exact name; \
or use an available tool. Loadable tools: tool_a, tool_b. Do not guess aliases; use an \
available tool or tell the user what capability is missing."
        );
        assert!(loaded.contains("tool_a"));
        assert!(loaded.contains("tool_b"));

        let res2 = execute_select_tools(
            &json!({ "names": ["tool_a"] }),
            &loadable,
            &registered,
            &mut loaded,
            None,
            &mut tracker,
        );
        assert!(!res2.is_error);
        assert_eq!(res2.content, "Already available: tool_a");

        let res3 = execute_select_tools(
            &json!({ "names": ["bogus"] }),
            &loadable,
            &registered,
            &mut loaded,
            None,
            &mut tracker,
        );
        assert!(res3.is_error);
        assert_eq!(
            res3.content,
            "Unknown tool: bogus. This name is not in the current loadable tool list. \
Call select_tools with a query to search by capability, then load an exact name; \
or use an available tool. No additional tools can be loaded right now; use the tools you \
already have. Do not guess aliases; use an available tool or tell the user what \
capability is missing."
        );

        let res_empty = execute_select_tools(
            &json!({ "names": [] }),
            &loadable,
            &registered,
            &mut loaded,
            None,
            &mut tracker,
        );
        assert!(res_empty.is_error);
        assert_eq!(
            res_empty.content,
            "Invalid arguments: 'names' must contain at least one tool name."
        );

        let res_missing = execute_select_tools(
            &json!({}),
            &loadable,
            &registered,
            &mut loaded,
            None,
            &mut tracker,
        );
        assert!(res_missing.is_error);
        assert_eq!(
            res_missing.content,
            "Invalid arguments: 'names' array is required."
        );

        let res6 = execute_select_tools(
            &json!({ "names": ["grep"] }),
            &loadable,
            &registered,
            &mut loaded,
            None,
            &mut tracker,
        );
        assert!(!res6.is_error);
        assert_eq!(
            res6.content,
            "\"grep\" is already available — call it directly; select_tools is only for names in the <tools_added> announcements."
        );

        assert_eq!(
            not_loaded_tool_output("mcp__github__search"),
            "Tool \"mcp__github__search\" is available but not loaded. Call select_tools with [\"mcp__github__search\"] first, then call the tool."
        );
    }

    #[test]
    fn test_render_loadable_tools_announcement() {
        let added = vec!["mcp__tool1".into(), "mcp__tool2".into()];
        let removed = vec!["mcp__tool3".into()];
        let announcement = render_loadable_tools_announcement(&added, &removed, true).unwrap();
        assert!(announcement.contains("<tools_added>\nmcp__tool1\nmcp__tool2\n</tools_added>"));
        assert!(announcement.contains("<tools_removed>\nmcp__tool3\n</tools_removed>"));
        assert!(announcement.contains("Use the select_tools tool with exact names"));

        let empty = render_loadable_tools_announcement(&[], &[], false).unwrap();
        assert!(
            empty.contains("No tools are currently available for dynamic loading in this session.")
        );
        assert!(!empty.contains("Use the select_tools tool"));

        assert!(render_loadable_tools_announcement(&[], &[], true).is_none());
    }

    #[test]
    fn test_refusal_describers_match_v2() {
        assert_eq!(
            missing_refusal(true, "mcp__gone__x", ToolRegistration::Missing, true).as_deref(),
            Some(
                "Tool \"mcp__gone__x\" was loaded but its MCP server is currently disconnected. \
It may become available again when the server reconnects; do not retry immediately."
            )
        );
        assert_eq!(
            missing_refusal(true, "plugin_tool", ToolRegistration::Missing, true).as_deref(),
            Some(
                "Tool \"plugin_tool\" was loaded but is no longer registered. \
Do not retry it unless it becomes available again."
            )
        );

        assert!(missing_refusal(true, "mcp__x__y", ToolRegistration::Missing, false).is_none());

        assert!(missing_refusal(true, "mcp__x__y", ToolRegistration::Deferred, true).is_none());
        assert!(missing_refusal(false, "mcp__gone__x", ToolRegistration::Missing, true).is_none());

        assert_eq!(
            unavailable_refusal(
                true,
                "mcp__github__search",
                ToolRegistration::Deferred,
                false
            )
            .as_deref(),
            Some(
                "Tool \"mcp__github__search\" is available but not loaded. \
Call select_tools with [\"mcp__github__search\"] first, then call the tool."
            )
        );
        assert_eq!(
            unavailable_refusal(true, "mcp__github__search", ToolRegistration::Inline, true)
                .as_deref(),
            Some(
                "Tool \"mcp__github__search\" was loaded but is no longer active. \
Ask the user to enable it before calling it again."
            )
        );
        assert!(
            unavailable_refusal(true, "mcp__x__y", ToolRegistration::Deferred, true).is_none(),
            "a loaded deferred tool is callable"
        );
        assert!(
            unavailable_refusal(true, "mcp__x__y", ToolRegistration::Missing, true).is_none(),
            "a missing name belongs to the missing describer"
        );
        assert!(
            unavailable_refusal(true, SELECT_TOOLS_TOOL_NAME, ToolRegistration::Inline, true)
                .is_none()
        );
        assert!(
            unavailable_refusal(
                false,
                "mcp__github__search",
                ToolRegistration::Deferred,
                false
            )
            .is_none()
        );
    }
}
