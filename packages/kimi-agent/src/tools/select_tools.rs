//! Native `select_tools` progressive tool disclosure tool.
//!
//! Mirrors `packages/agent-core-v2/src/agent/toolSelect/` and
//! `packages/agent-core-v2/src/agent/tools/select-tools/`.
//!
//! # Wiring (2026-09-14)
//!
//! Live:
//! - `execute_select_tools` is dispatched from the `"select_tools"` arm of
//!   [`crate::tools::NativeToolset::execute`], and the loaded set is now the
//!   toolset's **session-scoped** `loaded_tools` set instead of a set created
//!   and dropped on every call. `already_available` therefore reports names
//!   loaded by an earlier call, and `to_load` reports only genuinely new ones.
//! - `render_loadable_tools_announcement` is reached from
//!   `execute_select_tools` and delivered to the model as a follow-up `user`
//!   message via [`ExecutableToolResult::delivery`], so the
//!   `<tools_added>` block the tool description refers to actually arrives.
//!
//! Deliberately unreachable:
//! - `not_loaded_tool_output` has no call site **by design**. v2 deferred MCP
//!   schemas out of `tools[]` and refused a call to a deferred-but-unloaded
//!   tool. This engine advertises every tool it can execute
//!   (`GET /api/v1/tools` reports all builtins and all MCP tools as
//!   `active: true`), so no advertised tool is ever "not loaded" — gating a
//!   call would reject tools the model can see. Keeping the helper here
//!   documents the v2 wording; it is intentionally not wired.

use std::collections::HashSet;

use serde_json::{Value, json};

use crate::tools::tool_policy;
use crate::turn_loop::types::ExecutableToolResult;

pub const SELECT_TOOLS_TOOL_NAME: &str = "select_tools";

pub const SELECT_TOOLS_DESCRIPTION: &str = "\
Load one or more tools by name so you can call them. \
The loadable names are listed in the <tools_added>/<tools_removed> announcements \
in the system context — fold them in order to get the current list. \
If you have not seen a <tools_added> announcement in this session, no tools are \
currently loadable — do not call select_tools. \
Pass the exact tool name(s) you need — plugin, skill, or category names do not work. \
The full definitions become available immediately, so you can call them directly \
in your next tool call. \
Only announced names are loadable — tools you already have available are called \
directly, never passed to select_tools.";

pub fn select_tools_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "names": {
                "type": "array",
                "items": { "type": "string" },
                "minItems": 1,
                "description": "Exact tool names to load, taken from the latest announced tool list."
            }
        },
        "required": ["names"]
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadToolsResult {
    pub to_load: Vec<String>,
    pub already_available: Vec<String>,
    /// Requested names the model can already call directly — registered and
    /// not behind the deferred-disclosure gate (upstream #3885
    /// `isStaticCallable`).
    pub already_callable: Vec<String>,
    /// Names an earlier `select_tools` call loaded that are no longer
    /// registered (upstream #4190 `staleLoaded`): loading cannot restore
    /// them, only a new `<tools_added>` announcement can.
    pub stale_loaded: Vec<String>,
    /// Names the current tool policy refuses (upstream #4190 `policyDenied`):
    /// a deny-list hit on any name, or a registered name the allowlist
    /// excludes. select_tools cannot enable them.
    pub policy_denied: Vec<String>,
    pub unknown: Vec<String>,
    /// Names whose failed-selection count reached [`STOP_RETRY_THRESHOLD`]
    /// (upstream #4190 `escalated`), with the attempt count.
    pub escalated: Vec<(String, usize)>,
    /// Consecutive `select_tools` calls that made nothing available (upstream
    /// #4190 `consecutiveFailures`).
    pub consecutive_failures: usize,
    /// The still-unloaded loadable names (upstream `result.loadable`), sorted
    /// so rendered guidance is deterministic.
    pub loadable: Vec<String>,
}

/// Upstream #4190 `STOP_RETRY_THRESHOLD`: a name that failed selection this
/// many times is escalated with a "stop retrying" line.
pub const STOP_RETRY_THRESHOLD: usize = 2;

/// Upstream #4190 failure bookkeeping (`failedSelectionAttempts` +
/// `consecutiveFailures` + `selectionEnvironment`): per-name failed-selection
/// attempts and the consecutive-failure streak, both reset whenever the tool
/// environment (policy or catalogue) changes. Kept on the toolset so the
/// counts are session-scoped, like `loaded_tools`.
#[derive(Debug, Default)]
pub struct SelectionFailureTracker {
    failed_attempts: std::collections::HashMap<String, usize>,
    consecutive_failures: usize,
    environment: Option<String>,
}

impl SelectionFailureTracker {
    /// v2 `resetFailuresIfToolsChanged`: a policy or catalogue change makes
    /// earlier failures unactionable, so the counts start over.
    pub fn reset_if_environment_changed(&mut self, environment: String) {
        if self.environment.as_deref() == Some(environment.as_str()) {
            return;
        }
        self.environment = Some(environment);
        self.consecutive_failures = 0;
        self.failed_attempts.clear();
    }

    /// v2 `load()`'s bookkeeping: any success resets the streak, and a
    /// successful name clears only its own record — failed names in the same
    /// batch keep accumulating instead of being masked by sibling successes.
    /// A name repeated within one call counts once (v2 `attemptsByKey`).
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

/// The environment fingerprint a change in which resets the failure counts
/// (v2 `getPolicyFingerprint` + registry signature): the global `[tools]`
/// lists plus the registered and loadable name sets, sorted for determinism.
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

/// Classify the requested names (upstream #4190 `AgentToolSelectService.load`).
///
/// Order matters and mirrors v2 exactly: a policy refusal outranks everything
/// (a deny-list hit lands here even for a name that is not registered at all,
/// so the model is told the policy rather than handed a typo suggestion); a
/// loadable name loads or reports as already available; a registered name
/// that is not behind the disclosure gate is statically callable, so
/// selecting it is a category error, not a load; a name that was loaded
/// before but left the catalogue is stale; anything else is unknown.
pub fn evaluate_load_tools(
    requested: &[String],
    loadable_tools: &HashSet<String>,
    registered_tools: &HashSet<String>,
    currently_loaded: &HashSet<String>,
    policy: Option<&tool_policy::ToolsFilter>,
    tracker: &mut SelectionFailureTracker,
) -> LoadToolsResult {
    // v2 `isToolDenied`: deny-lists only, so a typo near an allowed name still
    // lands in `unknown` with suggestions instead of being called a policy
    // refusal. Deny patterns use the case-sensitive `isToolActive` match
    // semantics (the case-insensitive `blocks_call` is the execution gate,
    // not this classification).
    let is_denied = |name: &str| {
        policy.is_some_and(|filter| {
            filter
                .disabled
                .iter()
                .any(|pattern| tool_policy::matches_tool_pattern(pattern, name))
        })
    };
    // v2 `isToolActiveForSelection` minus its select_tools disclosure
    // special-case (this engine never registers select_tools in the
    // catalogue, so the special-case has no input to act on).
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

/// Upstream #3885 `suggestToolNames`: case-only fixes first, then substring
/// matches over the `mcp__`-stripped query, then matches against the last
/// `__` segment of the candidate. Sorted, capped at three.
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

    // Sorted so the rendered guidance is deterministic (the upstream TS array
    // inherited insertion order; a Rust HashSet does not). The suggestion
    // pool is the full loadable set (upstream `[...loadable]`), not the
    // already-loaded-filtered `result.loadable` used in the guidance lines.
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
    // Upstream #4190 bucket wording, verbatim.
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
select_tools only loads exact announced names; it cannot search for capabilities \
or install tools. "
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

    // Announce what just became loadable. `delivery` reaches the model as a
    // follow-up `user` message — the same channel v2 used for its
    // `loadable-tools` reminder. Without this the tool description told the
    // model to fold `<tools_added>` blocks that were never produced, and
    // `render_loadable_tools_announcement` had no caller at all.
    let delivery = render_loadable_tools_announcement(&result.to_load, &[], true).map(|text| {
        crate::turn_loop::types::ToolDelivery {
            blocks: vec![crate::rpc::types::ContentBlock::Text { text }],
            origin: None,
        }
    });

    ExecutableToolResult {
        delivery,
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
    // Upstream #4190: an empty loadable set replaces the usage trailer — the
    // model must stop calling select_tools instead of being invited to try.
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
        assert_eq!(schema["required"][0], "names");
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
        // A registered name that is not behind the disclosure gate is
        // statically callable (upstream #3885 `isStaticCallable`): selecting
        // it is answered with direct-call guidance, not a load.
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
        // Loaded in an earlier call, but the catalogue no longer carries the
        // name (the MCP server went away): v2 #4190 `staleLoaded`.
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
                "mcp__github__search".into(),    // deny-list hit on a loadable name
                "mcp__github__get_issue".into(), // deny-list glob on an unregistered name
                "Bash".into(),                   // registered but outside the allowlist
                "Grep".into(),                   // allowed: statically callable
                "mcp__other__ping".into(),       // matches no deny pattern: unknown
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

        // First failure: counted, not yet escalated.
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

        // Second failure of the same name crosses STOP_RETRY_THRESHOLD.
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

        // A repeated name inside one batch counts once (v2 `attemptsByKey`) —
        // attempts=1 stays below the escalation threshold.
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

        // A success resets the streak and clears only its own record; the
        // sibling failure in the same batch keeps accumulating (v2 #4190
        // "keep failure counts honest in mixed batches").
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

        // A changed environment (policy or catalogue) forgets the counts.
        tracker.reset_if_environment_changed("env-a".to_string());
        tracker.reset_if_environment_changed("env-a".to_string()); // no-op
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
        // After the reset the attempt count starts over: no escalation at 1.
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
        // Case-only fix wins over everything else.
        assert_eq!(suggest_tool_names("grep", &pool), vec!["Grep"]);
        // Substring over the mcp__-stripped query.
        assert_eq!(
            suggest_tool_names("mcp__github__search", &pool),
            vec!["mcp__github__search_code"]
        );
        // Last-segment containment: the query contains the candidate's final
        // `__` segment, and the substring branch found nothing first.
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

        // 1. Initial load with exact formatted output (upstream #4190 unknown
        //    wording: reason + loadable list + no-guessing guidance).
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
select_tools only loads exact announced names; it cannot search for capabilities \
or install tools. Loadable tools: tool_a, tool_b. Do not guess aliases; use an \
available tool or tell the user what capability is missing."
        );
        assert!(loaded.contains("tool_a"));
        assert!(loaded.contains("tool_b"));

        // 2. Subsequent load for already loaded tool
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

        // 3. Only unknown tools with nothing left loadable -> the "no tools"
        //    guidance (upstream #4190's loadable.length === 0 arm).
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
select_tools only loads exact announced names; it cannot search for capabilities \
or install tools. No additional tools can be loaded right now; use the tools you \
already have. Do not guess aliases; use an available tool or tell the user what \
capability is missing."
        );

        // 4. Empty names array -> is_error true
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

        // 5. Missing names field -> is_error true
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

        // 6. A statically callable name is answered with direct-call guidance
        //    and is not an error (upstream #3885).
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

        // 7. not_loaded_tool_output exact wording match
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

        // Upstream #4190: the empty loadable state replaces the usage trailer.
        let empty = render_loadable_tools_announcement(&[], &[], false).unwrap();
        assert!(
            empty.contains("No tools are currently available for dynamic loading in this session.")
        );
        assert!(!empty.contains("Use the select_tools tool"));

        // Nothing to say: no delta and tools remain loadable.
        assert!(render_loadable_tools_announcement(&[], &[], true).is_none());
    }
}
