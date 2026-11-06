//! The subagent-type allowlist (v2 `app/agentProfileCatalog/profile-shared.ts`:
//! `subagentAllowlistFor` / `profileCanDelegate` / `withoutDelegatingTargets`
//! / `subagentTypeNotAllowedMessage`).
//!
//! A profile declares the subagent types it may spawn in its frontmatter
//! (`subagents: [...]`); undeclared callers fall back to the default
//! profile's list, and undeclared callers may not chain delegation — a
//! target that can itself spawn is filtered out. `['*']` declares no
//! restriction.

use std::collections::HashMap;

use crate::subagent::types::SubagentDefinition;

/// The default profile's declared delegation set (v2
/// `session/agentLifecycle/profile/profiles.ts:91`: the main `agent`
/// profile spawns only these).
pub const DEFAULT_MAIN_SUBAGENTS: [&str; 3] = ["coder", "explore", "plan"];

/// v2 `subagentAllowlistFor`: the caller's own declaration wins, then the
/// default profile's; `['*']` lifts the restriction, and the caller's
/// extras (the main agent's discovered types) merge in deduplicated.
/// `None` means unrestricted.
pub fn subagent_allowlist_for(
    default_subagents: Option<&[String]>,
    caller_subagents: Option<&[String]>,
    extras: Option<&[String]>,
) -> Option<Vec<String>> {
    let declared = caller_subagents.or(default_subagents)?;
    if declared.len() == 1 && declared[0] == "*" {
        return None;
    }
    let mut merged: Vec<String> = declared.to_vec();
    if let Some(extras) = extras {
        for extra in extras {
            if !merged.contains(extra) {
                merged.push(extra.clone());
            }
        }
    }
    Some(merged)
}

/// v2 `profileCanDelegate`: the profile's own tool set includes `Agent` or
/// `AgentSwarm` (empty `tools` = every tool, mirroring v2's `undefined`),
/// not removed by `disallowedTools`. The engine's definitions mix
/// lowercased builtin names with as-written file names, so the match is
/// case-insensitive.
pub fn profile_can_delegate(tools: &[String], disallowed_tools: &[String]) -> bool {
    let possesses = |name: &str| {
        (tools.is_empty() || tools.iter().any(|t| t.eq_ignore_ascii_case(name)))
            && !disallowed_tools
                .iter()
                .any(|t| t.eq_ignore_ascii_case(name))
    };
    possesses("Agent") || possesses("AgentSwarm")
}

/// v2 `withoutDelegatingTargets`: undeclared callers may not chain
/// delegation — targets that can themselves spawn are removed. An unknown
/// name stays (the unknown-type error reports it better than the allowlist
/// would).
pub fn without_delegating_targets(
    definitions: &HashMap<String, SubagentDefinition>,
    allowlist: &[String],
) -> Vec<String> {
    allowlist
        .iter()
        .filter(|name| {
            definitions
                .get(*name)
                .is_none_or(|def| !profile_can_delegate(&def.tools, &def.disallowed_tools))
        })
        .cloned()
        .collect()
}

/// v2 `subagentTypeNotAllowedMessage`, verbatim: the model reads this to
/// learn the set it may pick from.
pub fn subagent_type_not_allowed_message(name: &str, allowlist: &[String]) -> String {
    let allowed = if allowlist.is_empty() {
        "none".to_string()
    } else {
        allowlist.join(", ")
    };
    format!(
        "Subagent type \"{name}\" is not allowed for this agent. Allowed subagent types: {allowed}."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    #[test]
    fn the_callers_declaration_wins_and_star_unrestricts() {
        let default = s(&["coder", "explore", "plan"]);
        // Caller-declared wins over the default.
        assert_eq!(
            subagent_allowlist_for(Some(&default), Some(&s(&["explore"])), None),
            Some(s(&["explore"]))
        );
        // Undeclared falls back to the default.
        assert_eq!(
            subagent_allowlist_for(Some(&default), None, None),
            Some(default.clone())
        );
        // `['*']` is unrestricted, on either level.
        assert_eq!(
            subagent_allowlist_for(Some(&default), Some(&s(&["*"])), None),
            None
        );
        assert_eq!(subagent_allowlist_for(Some(&s(&["*"])), None, None), None);
        // Extras merge deduplicated.
        assert_eq!(
            subagent_allowlist_for(Some(&default), None, Some(&s(&["coder", "reviewer"]))),
            Some(s(&["coder", "explore", "plan", "reviewer"]))
        );
    }

    #[test]
    fn delegating_targets_are_filtered_for_undeclared_callers() {
        let mut definitions = HashMap::new();
        let mut register = |name: &str, tools: &[&str]| {
            definitions.insert(
                name.to_string(),
                SubagentDefinition {
                    name: name.into(),
                    description: String::new(),
                    system_prompt: String::new(),
                    tools: tools.iter().map(|t| t.to_string()).collect(),
                    disallowed_tools: vec![],
                    when_to_use: None,
                    prompt_prefix: None,
                    summary_policy: None,
                    model: None,
                    subagents: None,
                },
            );
        };
        register("agent", &["Agent", "Read"]);
        register("coder", &["Read", "Edit"]);
        // An unknown name stays; the delegating target drops.
        assert_eq!(
            without_delegating_targets(&definitions, &s(&["agent", "coder", "custom"])),
            s(&["coder", "custom"])
        );
    }

    #[test]
    fn the_denial_message_names_the_allowed_set() {
        assert_eq!(
            subagent_type_not_allowed_message("agent", &s(&["coder", "explore"])),
            "Subagent type \"agent\" is not allowed for this agent. Allowed subagent types: coder, explore."
        );
        assert_eq!(
            subagent_type_not_allowed_message("coder", &[]),
            "Subagent type \"coder\" is not allowed for this agent. Allowed subagent types: none."
        );
    }
}
