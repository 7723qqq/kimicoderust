//! The three `agent/changeNotifier` variants the engine can observe itself:
//! skills, subagents and workspace directories.
//!
//! v2 (`agent/changeNotifier/changeNotifierService.ts`) subscribes to the skill
//! catalog, the agent profile catalog and the workspace-info service, and
//! records a snapshot after every successful observation. A reminder is only
//! queued when the freshly observed value differs from that snapshot, so a
//! change is announced once and a failed read never consumes a real change.
//!
//! This module keeps the same state machine but owns no I/O: the engine hands
//! in the values it already computed for the turn (the skills scan it did for
//! the system prompt, the subagent profile list, the session's workspace
//! roots), and gets back the reminder text or `None`. That keeps the detection
//! bounded — one observation per turn, reusing the prompt's scan — and makes
//! the whole comparison testable without a running engine.
//!
//! The `agents_md` variant lives separately in [`super`] (`AgentsMdReminder`);
//! it keeps its own state because it is driven by tool accesses, not by a
//! catalog snapshot.

use std::collections::BTreeMap;

use crate::primitives::read::ReadOutcome;

/// v2 variant names (`changeNotifierService.ts`).
pub const SKILLS_CHANGE_VARIANT: &str = "skills_change";
pub const SUBAGENTS_CHANGE_VARIANT: &str = "subagents_change";
pub const WORKSPACE_DIRS_CHANGE_VARIANT: &str = "workspace_dirs_change";

/// The marker v2's rendered skill listing starts with (`SKILL_LISTING_MARKER`).
/// The fork's prompt prose carries the same sentence, so a prompt that already
/// contains it was rendered against a catalog.
const SKILL_LISTING_MARKER: &str = "DISREGARD any earlier skill listings";

/// The tool whose composed activeness gates the skills reminder (v2
/// `isToolActiveComposed('Skill')`).
pub const SKILL_TOOL_NAME: &str = "Skill";

/// The tools either of which gates the subagent reminder in v2
/// (`isToolActiveComposed('Agent') || isToolActiveComposed('AgentSwarm')`).
pub const AGENT_TOOL_NAMES: [&str; 2] = ["Agent", "AgentSwarm"];

/// One profile of the live subagent catalog, as the reminder names it when it
/// appears (`addedProfileLabel`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentProfile {
    pub name: String,
    pub description: String,
    pub when_to_use: Option<String>,
}

/// The subagent model pool, comparable across turns. v2 snapshots
/// `{ defaultModel?, models: Record<alias, description> }`; the fork's
/// [`crate::rpc::types::SecondaryModelPool`] carries the same two facts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelPoolSnapshot {
    pub default_model: Option<String>,
    /// alias → description, ordered so equality does not depend on insertion.
    pub models: BTreeMap<String, String>,
}

/// The workspace directory set. The fork has a single work directory per
/// session (the session root), so only the additional-dir set can move; the
/// work directory itself is reported as the session cwd in the reminder.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceSnapshot {
    pub additional: Vec<String>,
}

/// Per-session change-notifier snapshots (v2 `ChangeNotifierSnapshotState`).
///
/// Every field is `None` until the first successful observation. A field that
/// stays `None` after [`Self::skills`] means the read failed: the engine must
/// keep observing rather than treat the failed read as an empty catalog.
#[derive(Debug, Clone, Default)]
pub struct ChangeNotifierState {
    skills_listing: Option<String>,
    subagent_names: Option<Vec<String>>,
    model_pool: Option<ModelPoolSnapshot>,
    workspace_additional: Option<Vec<String>>,
}

impl ChangeNotifierState {
    /// Observe the model-visible skills listing and return the reminder text,
    /// if any.
    ///
    /// `listing` is [`ReadOutcome::Found`] with the listing rendered exactly as
    /// the system prompt renders it (`""` when the catalog has no skills).
    /// `Unreadable`/`Absent` keep the previous snapshot: a directory that
    /// temporarily cannot be listed must not be mistaken for "all skills were
    /// deleted".
    ///
    /// `prompt` is the turn's system prompt. It gates the **first**
    /// observation only (v2 `snapshot.skillsHash === undefined`): a prompt that
    /// already carries the listing was rendered from this same catalog, and a
    /// reminder would only repeat it. Later observations always notify on a
    /// real change — the prompt rebuild is not a second suppression gate,
    /// because a shrunken listing can be a substring of the old one and would
    /// otherwise hide a removal.
    pub fn skills(
        &mut self,
        listing: ReadOutcome<String>,
        prompt: &str,
        is_tool_active: &dyn Fn(&str) -> bool,
    ) -> Option<String> {
        let ReadOutcome::Found(listing) = listing else {
            // A failed read is not an empty catalog. Leave the baseline alone
            // so the next successful observation still sees the real diff.
            return None;
        };
        if self.skills_listing.as_deref() == Some(listing.as_str()) {
            return None;
        }
        let first = self.skills_listing.is_none();
        if first {
            if listing.is_empty() {
                // v2: silence when the prompt never had a listing section, so
                // a fresh session with no skills does not open with a removal.
                if !prompt.contains(SKILL_LISTING_MARKER) {
                    self.skills_listing = Some(listing);
                    return None;
                }
            } else if prompt.contains(&listing) {
                self.skills_listing = Some(listing);
                return None;
            }
        }
        self.skills_listing = Some(listing.clone());
        // v2 gates the skills change on `Skill` being active in the composed
        // tool policy — a per-tool verdict, not "native tools are on": a
        // workspace/profile can disable Skill while every other tool stays.
        if !is_tool_active(SKILL_TOOL_NAME) {
            return None;
        }
        Some(skills_change_text(&listing))
    }

    /// Observe the live subagent profile list and model pool.
    ///
    /// The first successful observation only seeds the baseline (v2): there is
    /// nothing to compare against yet. Later observations diff names and model
    /// aliases, record the new snapshot, then queue a reminder only when the
    /// visible diff is non-empty and one of the agent tools is active.
    pub fn subagents(
        &mut self,
        profiles: &[SubagentProfile],
        pool: Option<ModelPoolSnapshot>,
        is_tool_active: &dyn Fn(&str) -> bool,
    ) -> Option<String> {
        let current_names: Vec<String> = profiles
            .iter()
            .map(|profile| profile.name.clone())
            .collect();
        let has_baseline = self.subagent_names.is_some();
        if has_baseline
            && self.subagent_names.as_deref() == Some(current_names.as_slice())
            && self.model_pool == pool
        {
            return None;
        }

        let base_names = self
            .subagent_names
            .clone()
            .unwrap_or_else(|| current_names.clone());
        let base_pool = if has_baseline {
            self.model_pool.clone()
        } else {
            pool.clone()
        };

        let added: Vec<&SubagentProfile> = current_names
            .iter()
            .filter(|name| !base_names.contains(name))
            .filter_map(|name| profiles.iter().find(|profile| &profile.name == name))
            .collect();
        let removed: Vec<String> = base_names
            .iter()
            .filter(|name| !current_names.contains(name))
            .cloned()
            .collect();
        let current_aliases = pool_aliases(pool.as_ref());
        let base_aliases = pool_aliases(base_pool.as_ref());
        let models_added: Vec<String> = current_aliases
            .iter()
            .filter(|alias| !base_aliases.contains(alias))
            .cloned()
            .collect();
        let models_removed: Vec<String> = base_aliases
            .iter()
            .filter(|alias| !current_aliases.contains(alias))
            .cloned()
            .collect();
        let models_updated: Vec<String> = current_aliases
            .iter()
            .filter(|alias| {
                base_pool
                    .as_ref()
                    .and_then(|base| base.models.get(*alias))
                    .is_some_and(|before| {
                        pool.as_ref()
                            .and_then(|current| current.models.get(*alias))
                            .is_some_and(|after| before != after)
                    })
            })
            .cloned()
            .collect();
        let default_model_change = match (pool.as_ref(), base_pool.as_ref()) {
            (Some(current), Some(base)) if current.default_model != base.default_model => {
                Some((base.default_model.clone(), current.default_model.clone()))
            }
            _ => None,
        };

        self.subagent_names = Some(current_names);
        self.model_pool = pool;

        if added.is_empty()
            && removed.is_empty()
            && models_added.is_empty()
            && models_removed.is_empty()
            && models_updated.is_empty()
            && default_model_change.is_none()
        {
            return None;
        }
        // v2 gates this reminder on either agent tool being active in the
        // composed policy, per tool.
        if !AGENT_TOOL_NAMES.iter().any(|name| is_tool_active(name)) {
            return None;
        }
        Some(subagents_change_text(
            &added,
            &removed,
            &models_added,
            &models_removed,
            &models_updated,
            default_model_change,
        ))
    }

    /// Observe the session's additional directory set.
    ///
    /// `None` means the roots are not available for this turn (the engine could
    /// not publish them); like a failed skills read, that must not be read as
    /// "every directory was removed", so the baseline stays put.
    ///
    /// v2 stays silent on the first observation: the snapshot is seeded without
    /// a reminder, because the session's prompt was rendered with it.
    pub fn workspace_dirs(
        &mut self,
        current: Option<WorkspaceSnapshot>,
        cwd: &str,
    ) -> Option<String> {
        let Some(current) = current else {
            return None;
        };
        if self.workspace_additional.as_ref() == Some(&current.additional) {
            return None;
        }
        let previous = self.workspace_additional.clone();
        self.workspace_additional = Some(current.additional.clone());
        let Some(previous) = previous else {
            return None;
        };
        let added: Vec<String> = current
            .additional
            .iter()
            .filter(|dir| !previous.contains(dir))
            .cloned()
            .collect();
        let removed: Vec<String> = previous
            .iter()
            .filter(|dir| !current.additional.contains(dir))
            .cloned()
            .collect();
        if added.is_empty() && removed.is_empty() {
            return None;
        }
        Some(workspace_dirs_change_text(&added, &removed, cwd))
    }
}

/// v2 `skillsChangeText`, verbatim.
fn skills_change_text(listing: &str) -> String {
    if listing.is_empty() {
        "The skill catalog changed since your system prompt was rendered; no skills are currently available.".to_string()
    } else {
        format!(
            "The skill catalog changed since your system prompt was rendered; the listing below replaces it.\n\n{listing}"
        )
    }
}

/// v2 `addedProfileLabel`: `name (description; whenToUse)`.
fn added_profile_label(profile: &SubagentProfile) -> String {
    let details: Vec<&str> = [
        Some(profile.description.as_str()).filter(|part| !part.is_empty()),
        profile
            .when_to_use
            .as_deref()
            .filter(|part| !part.is_empty()),
    ]
    .into_iter()
    .flatten()
    .collect();
    if details.is_empty() {
        profile.name.clone()
    } else {
        format!("{} ({})", profile.name, details.join("; "))
    }
}

/// v2 `subagentsChangeText`, verbatim.
fn subagents_change_text(
    added: &[&SubagentProfile],
    removed: &[String],
    models_added: &[String],
    models_removed: &[String],
    models_updated: &[String],
    default_model_change: Option<(Option<String>, Option<String>)>,
) -> String {
    let mut lines = vec!["The available subagent profiles and models changed:".to_string()];
    if !added.is_empty() {
        let labels: Vec<String> = added
            .iter()
            .map(|profile| added_profile_label(profile))
            .collect();
        lines.push(format!("- Profiles added: {}", labels.join(", ")));
    }
    if !removed.is_empty() {
        lines.push(format!("- Profiles removed: {}", removed.join(", ")));
    }
    if !models_added.is_empty() {
        lines.push(format!("- Models added: {}", models_added.join(", ")));
    }
    if !models_removed.is_empty() {
        lines.push(format!("- Models removed: {}", models_removed.join(", ")));
    }
    if !models_updated.is_empty() {
        lines.push(format!(
            "- Model descriptions updated: {}",
            models_updated.join(", ")
        ));
    }
    if let Some((from, to)) = default_model_change {
        match (from, to) {
            (Some(from), Some(to)) => lines.push(format!("- Default model changed: {from} → {to}")),
            (None, Some(to)) => lines.push(format!("- Default model set: {to}")),
            (Some(from), None) => lines.push(format!("- Default model cleared (was {from})")),
            (None, None) => {}
        }
    }
    lines.push(
        "You can use them right away — the Agent tool validates subagent_type against the live catalog."
            .to_string(),
    );
    lines.join("\n")
}

/// v2 `workspaceDirsChangeText`, with the fork's single-work-directory shape
/// (no per-workspace primary mapping to report).
fn workspace_dirs_change_text(added: &[String], removed: &[String], cwd: &str) -> String {
    let mut lines = vec!["Project directory configuration changed.".to_string()];
    if !added.is_empty() {
        lines.push(format!("Added directories: {}.", added.join(", ")));
    }
    if !removed.is_empty() {
        lines.push(format!("Removed directories: {}.", removed.join(", ")));
    }
    lines.push(format!("Session cwd remains {cwd}."));
    lines.push(
        "The updated directory set is already effective for subsequent tool calls and supersedes earlier directory information."
            .to_string(),
    );
    lines.join("\n")
}

fn pool_aliases(pool: Option<&ModelPoolSnapshot>) -> Vec<String> {
    pool.map(|pool| pool.models.keys().cloned().collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::primitives::read::{ReadOutcome, UnreadableCause};

    fn profile(name: &str, description: &str, when_to_use: Option<&str>) -> SubagentProfile {
        SubagentProfile {
            name: name.to_string(),
            description: description.to_string(),
            when_to_use: when_to_use.map(str::to_string),
        }
    }

    fn listing(names: &[&str]) -> String {
        names
            .iter()
            .map(|name| format!("- {name}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The prompt in these tests is empty (a host-supplied prompt), so the
    /// reminder is the only place the model sees the change.
    const NO_PROMPT: &str = "";

    /// The composed policy admits every tool.
    fn all_tools(_: &str) -> bool {
        true
    }

    /// The composed policy admits everything except `Skill` — the shape a
    /// workspace/profile disable produces.
    fn all_tools_but_skill(name: &str) -> bool {
        name != SKILL_TOOL_NAME
    }

    /// A prompt rendered against `listing`, the realistic first-observation
    /// shape: the engine's own prompt already carries the catalog.
    fn prompt_with(listing: &str) -> String {
        format!("{SKILL_LISTING_MARKER}\n\n{listing}")
    }

    #[test]
    fn a_new_skill_is_announced_once() {
        let mut state = ChangeNotifierState::default();
        assert_eq!(
            state.skills(
                ReadOutcome::found(listing(&["alpha"])),
                &prompt_with(&listing(&["alpha"])),
                &all_tools,
            ),
            None,
            "the first observation only seeds the baseline"
        );
        let text = state
            .skills(
                ReadOutcome::found(listing(&["alpha", "beta"])),
                NO_PROMPT,
                &all_tools,
            )
            .expect("the added skill is announced");
        assert!(text.contains("the listing below replaces it"));
        assert!(text.contains("- beta"), "{text}");
        assert_eq!(
            state.skills(
                ReadOutcome::found(listing(&["alpha", "beta"])),
                NO_PROMPT,
                &all_tools,
            ),
            None,
            "the same catalog on the next turn is silent"
        );
    }

    #[test]
    fn a_removed_skill_names_the_empty_catalog() {
        let mut state = ChangeNotifierState::default();
        state.skills(
            ReadOutcome::found(listing(&["alpha"])),
            &prompt_with(&listing(&["alpha"])),
            &all_tools,
        );
        let text = state
            .skills(ReadOutcome::found(String::new()), NO_PROMPT, &all_tools)
            .expect("an emptied catalog is announced");
        assert_eq!(
            text,
            "The skill catalog changed since your system prompt was rendered; no skills are currently available."
        );
    }

    #[test]
    fn a_modified_skill_listing_is_announced() {
        let mut state = ChangeNotifierState::default();
        state.skills(
            ReadOutcome::found(listing(&["alpha"])),
            &prompt_with(&listing(&["alpha"])),
            &all_tools,
        );
        let text = state
            .skills(
                ReadOutcome::found(listing(&["alpha — new description"])),
                NO_PROMPT,
                &all_tools,
            )
            .expect("a changed listing is a change");
        assert!(text.contains("new description"), "{text}");
    }

    /// The failure that must never become "all skills were deleted": a root
    /// that cannot be listed keeps the baseline, and the real change that
    /// follows is still announced.
    #[test]
    fn an_unreadable_catalog_does_not_consume_the_next_real_change() {
        let mut state = ChangeNotifierState::default();
        state.skills(
            ReadOutcome::found(listing(&["alpha"])),
            &prompt_with(&listing(&["alpha"])),
            &all_tools,
        );

        let unreadable = ReadOutcome::Unreadable(UnreadableCause::new("skills", "denied"));
        assert_eq!(
            state.skills(unreadable, NO_PROMPT, &all_tools),
            None,
            "a failed read is not a removal"
        );
        assert_eq!(
            state.skills(
                ReadOutcome::found(listing(&["alpha"])),
                NO_PROMPT,
                &all_tools
            ),
            None,
            "the baseline survived the failed read"
        );
        let text = state
            .skills(
                ReadOutcome::found(listing(&["alpha", "beta"])),
                NO_PROMPT,
                &all_tools,
            )
            .expect("the real change is still announced");
        assert!(text.contains("- beta"), "{text}");
    }

    #[test]
    fn a_prompt_that_already_carries_the_listing_is_silent() {
        let mut state = ChangeNotifierState::default();
        let current = listing(&["alpha"]);
        let prompt = format!("{SKILL_LISTING_MARKER}\n\n{current}");
        assert_eq!(
            state.skills(ReadOutcome::found(current.clone()), &prompt, &all_tools),
            None,
            "the rebuilt prompt already supersedes the listing"
        );
        // A later real change with a stale host prompt still reminds.
        let text = state
            .skills(
                ReadOutcome::found(listing(&["alpha", "beta"])),
                &prompt,
                &all_tools,
            )
            .expect("the prompt no longer carries the current listing");
        assert!(text.contains("- beta"), "{text}");
    }

    #[test]
    fn a_tail_removal_is_announced_against_a_pre_removal_prompt() {
        // v2 gates the "prompt already carries it" case on the first
        // observation only. Later, a prompt rendered before the removal still
        // contains the new (shorter) listing as a substring — treating that as
        // "already announced" would hide the removal.
        let mut state = ChangeNotifierState::default();
        let before = listing(&["alpha", "beta"]);
        state.skills(
            ReadOutcome::found(before.clone()),
            &prompt_with(&before),
            &all_tools,
        );
        let text = state
            .skills(
                ReadOutcome::found(listing(&["alpha"])),
                &prompt_with(&before),
                &all_tools,
            )
            .expect("the removal is announced even though the old prompt carries the suffix");
        assert!(text.contains("the listing below replaces it"), "{text}");
    }

    #[test]
    fn an_inactive_skill_tool_consumes_the_change_without_a_reminder() {
        let mut state = ChangeNotifierState::default();
        state.skills(
            ReadOutcome::found(listing(&["alpha"])),
            &prompt_with(&listing(&["alpha"])),
            &all_tools,
        );
        assert_eq!(
            state.skills(
                ReadOutcome::found(listing(&["alpha", "beta"])),
                NO_PROMPT,
                &all_tools_but_skill
            ),
            None
        );
        assert_eq!(
            state.skills(
                ReadOutcome::found(listing(&["alpha", "beta"])),
                NO_PROMPT,
                &all_tools
            ),
            None,
            "the consumed snapshot is not replayed when the tool returns"
        );
    }

    #[test]
    fn the_first_observation_supplies_a_listing_the_prompt_lacks() {
        // A host-supplied prompt has no skills section, so v2's first check
        // cannot short-circuit and the listing is delivered once.
        let mut state = ChangeNotifierState::default();
        let text = state
            .skills(
                ReadOutcome::found(listing(&["alpha"])),
                NO_PROMPT,
                &all_tools,
            )
            .expect("the prompt needs the catalog");
        assert!(text.contains("- alpha"), "{text}");
        assert_eq!(
            state.skills(
                ReadOutcome::found(listing(&["alpha"])),
                NO_PROMPT,
                &all_tools
            ),
            None,
            "and only once"
        );
    }

    #[test]
    fn a_new_subagent_profile_is_named_with_its_details() {
        let mut state = ChangeNotifierState::default();
        let base = vec![profile("explorer", "Looks around", None)];
        assert_eq!(state.subagents(&base, None, &all_tools), None, "seed only");
        let current = vec![
            profile("explorer", "Looks around", None),
            profile("reviewer", "Reviews code", Some("before merging")),
        ];
        let text = state
            .subagents(&current, None, &all_tools)
            .expect("the new profile is announced");
        assert!(
            text.contains("- Profiles added: reviewer (Reviews code; before merging)"),
            "{text}"
        );
        assert!(
            text.contains("the Agent tool validates subagent_type"),
            "{text}"
        );
        assert_eq!(
            state.subagents(&current, None, &all_tools),
            None,
            "once only"
        );
    }

    #[test]
    fn the_skill_gate_is_per_tool_not_all_or_nothing() {
        let mut state = ChangeNotifierState::default();
        state.skills(
            ReadOutcome::found(listing(&["alpha"])),
            &prompt_with(&listing(&["alpha"])),
            &all_tools,
        );
        assert_eq!(
            state.skills(
                ReadOutcome::found(listing(&["alpha", "beta"])),
                NO_PROMPT,
                &all_tools_but_skill,
            ),
            None,
            "disabling only Skill silences the reminder, other tools stay active"
        );
    }

    #[test]
    fn the_subagent_gate_reads_the_agent_tools_individually() {
        let mut state = ChangeNotifierState::default();
        let base = vec![profile("explorer", "Looks around", None)];
        assert_eq!(state.subagents(&base, None, &all_tools), None, "seed only");
        let current = vec![
            profile("explorer", "Looks around", None),
            profile("reviewer", "Reviews code", None),
        ];
        // `Skill` active does not stand in for `Agent`/`AgentSwarm`.
        let only_skill = |name: &str| name == SKILL_TOOL_NAME;
        assert_eq!(
            state.subagents(&current, None, &only_skill),
            None,
            "the subagent reminder needs an agent tool, not merely native tools"
        );
    }

    #[test]
    fn a_removed_subagent_profile_is_named() {
        let mut state = ChangeNotifierState::default();
        let base = vec![profile("a", "", None), profile("b", "", None)];
        state.subagents(&base, None, &all_tools);
        let text = state
            .subagents(&[profile("a", "", None)], None, &all_tools)
            .expect("the removal is announced");
        assert!(text.contains("- Profiles removed: b"), "{text}");
    }

    #[test]
    fn model_pool_changes_are_announced() {
        let mut state = ChangeNotifierState::default();
        let mut pool = ModelPoolSnapshot {
            default_model: Some("kimi-small".into()),
            models: BTreeMap::new(),
        };
        pool.models.insert("fast".into(), "Fast model".into());
        state.subagents(&[profile("a", "", None)], Some(pool.clone()), &all_tools);

        let mut updated = pool.clone();
        updated.models.insert("fast".into(), "Fastest model".into());
        updated.models.insert("deep".into(), "Deep model".into());
        updated.default_model = Some("kimi-large".into());
        let text = state
            .subagents(&[profile("a", "", None)], Some(updated), &all_tools)
            .expect("pool changes are announced");
        assert!(text.contains("- Models added: deep"), "{text}");
        assert!(
            text.contains("- Model descriptions updated: fast"),
            "{text}"
        );
        assert!(
            text.contains("- Default model changed: kimi-small → kimi-large"),
            "{text}"
        );
    }

    #[test]
    fn the_first_workspace_observation_is_silent() {
        let mut state = ChangeNotifierState::default();
        assert_eq!(
            state.workspace_dirs(
                Some(WorkspaceSnapshot {
                    additional: vec!["/work/extra".into()],
                }),
                "/work",
            ),
            None
        );
    }

    #[test]
    fn workspace_dir_additions_and_removals_are_named() {
        let mut state = ChangeNotifierState::default();
        state.workspace_dirs(
            Some(WorkspaceSnapshot {
                additional: vec!["/work/old".into()],
            }),
            "/work",
        );
        let text = state
            .workspace_dirs(
                Some(WorkspaceSnapshot {
                    additional: vec!["/work/new".into()],
                }),
                "/work",
            )
            .expect("the directory change is announced");
        assert!(text.contains("Added directories: /work/new."), "{text}");
        assert!(text.contains("Removed directories: /work/old."), "{text}");
        assert!(text.contains("Session cwd remains /work."), "{text}");
        assert_eq!(
            state.workspace_dirs(
                Some(WorkspaceSnapshot {
                    additional: vec!["/work/new".into()],
                }),
                "/work",
            ),
            None,
            "once only"
        );
    }

    /// A turn the engine could not publish roots for must not look like an
    /// empty set and delete every directory in the reminder.
    #[test]
    fn unavailable_workspace_roots_do_not_look_like_a_removal() {
        let mut state = ChangeNotifierState::default();
        state.workspace_dirs(
            Some(WorkspaceSnapshot {
                additional: vec!["/work/extra".into()],
            }),
            "/work",
        );
        assert_eq!(state.workspace_dirs(None, "/work"), None);
        assert_eq!(
            state.workspace_dirs(
                Some(WorkspaceSnapshot {
                    additional: vec!["/work/extra".into()],
                }),
                "/work",
            ),
            None,
            "the baseline is intact"
        );
    }
}
