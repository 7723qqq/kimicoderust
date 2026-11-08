//! v2 `agent/toolSelect/toolSelectOps.ts` + `dynamicTools.ts` (final state).
//!
//! A session records an initial tool table, then an ordered list of changes
//! as the live registry is reconciled. Announcements are keyed by a revision
//! number so a model that has only seen revision N receives the delta from N
//! to the current one, skipping changes that contain no dynamically loadable
//! tool. Unsupported-dynamic-tool history shaping rewrites a prior
//! `<tools_added>` announcement into the runtime-change reminder.

use crate::tools::registry::{ToolDefinition, ToolDisclosure};

/// One ordered change to the live tool table (v2 `ToolSelectChange`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolChange {
    pub added: Vec<ToolDefinition>,
    pub removed: Vec<ToolDefinition>,
}

/// The persisted per-session disclosure state (v2 `ToolSelectModelState`).
#[derive(Debug, Default)]
pub struct ToolSelectState {
    initial: Option<Vec<ToolDefinition>>,
    current: Vec<ToolDefinition>,
    changes: Vec<ToolChange>,
    /// Signatures already reported by `notifyUpdatedTools` (once per
    /// service instance, v2 `notifiedUpdates`).
    notified_updates: std::collections::HashSet<String>,
    /// Reminder texts waiting for the next turn's injection provider
    /// (v2 `AgentReminder.notify` → `reminder` history message).
    pending_notices: Vec<String>,
}

impl ToolSelectState {
    pub fn new() -> Self {
        Self::default()
    }

    /// v2 `ToolSelectInitialized`: pin the initial table. Repeated calls are
    /// a no-op, like the v2 reducer.
    pub fn initialize(&mut self, tools: Vec<ToolDefinition>) {
        if self.initial.is_none() {
            self.initial = Some(tools.clone());
            self.current = tools;
        }
    }

    /// v2 `reconcile`: diff the live table against `current` and append a
    /// change for genuinely added / removed names. Same-name definition
    /// updates do not produce a delta (the active schema stays pinned).
    pub fn reconcile(&mut self, live: Vec<ToolDefinition>) -> Option<&ToolChange> {
        if self.initial.is_none() {
            self.initialize(live);
            return None;
        }
        let live_names: std::collections::HashSet<String> =
            live.iter().map(|tool| tool.name.clone()).collect();
        let current_names: std::collections::HashSet<&str> =
            self.current.iter().map(|tool| tool.name.as_str()).collect();
        let removed: Vec<ToolDefinition> = self
            .current
            .iter()
            .filter(|tool| !live_names.contains(&tool.name))
            .cloned()
            .collect();
        let added: Vec<ToolDefinition> = live
            .into_iter()
            .filter(|tool| !current_names.contains(tool.name.as_str()))
            .collect();
        if added.is_empty() && removed.is_empty() {
            return None;
        }
        self.current
            .retain(|tool| live_names.contains(tool.name.as_str()));
        self.current.extend(added.iter().cloned());
        self.current
            .sort_by(|left, right| left.name.cmp(&right.name));
        self.changes.push(ToolChange { added, removed });
        self.changes.last()
    }

    pub fn revision(&self) -> usize {
        self.changes.len()
    }

    pub fn current(&self) -> &[ToolDefinition] {
        &self.current
    }

    pub fn initial(&self) -> Option<&[ToolDefinition]> {
        self.initial.as_deref()
    }

    /// v2 `loadableToolsAnnouncement` delta walk: from `last_revision`, find
    /// the first change that has a loadable added or removed tool, filtering
    /// out non-loadable announcements.
    pub fn announcement_since(
        &self,
        last_revision: usize,
    ) -> Option<(usize, Vec<String>, Vec<String>)> {
        for (index, change) in self.changes.iter().enumerate().skip(last_revision) {
            let added: Vec<String> = change
                .added
                .iter()
                .filter(|tool| tool.disclosure == ToolDisclosure::Deferred)
                .map(|tool| tool.name.clone())
                .collect();
            let removed: Vec<String> = change
                .removed
                .iter()
                .filter(|tool| {
                    tool.disclosure == ToolDisclosure::Deferred
                        || !change.added.iter().any(|next| {
                            next.name == tool.name && next.disclosure != ToolDisclosure::Deferred
                        })
                })
                .map(|tool| tool.name.clone())
                .collect();
            if added.is_empty() && removed.is_empty() {
                continue;
            }
            return Some((index + 1, added, removed));
        }
        None
    }

    /// v2 `notifyUpdatedTools`: a same-name update that is not applied in
    /// this session is reported once per service instance, by signature.
    /// Returns the reminder text when it has not been reported before.
    pub fn notify_update_once(&mut self, name: &str, signature: &str) -> Option<String> {
        if !self.notified_updates.insert(signature.to_string()) {
            return None;
        }
        let text = format!(
            "Tool \"{name}\" has an update, but the update is not applied in this session. \
             Continue using the currently registered version."
        );
        self.pending_notices.push(text.clone());
        Some(text)
    }

    /// The next turn's `tool_update_pending` injection text, if any. One
    /// notice is delivered per turn, in the order the updates were seen.
    pub fn take_pending_notice(&mut self) -> Option<String> {
        if self.pending_notices.is_empty() {
            None
        } else {
            Some(self.pending_notices.remove(0))
        }
    }

    /// v2 `notifyUpdatedTools`: compare the active table this session sees
    /// against what a new session would register. A same-name definition
    /// whose version signature changed queues one reminder.
    pub fn observe_live_updates(&mut self, active: &[ToolDefinition], live: &[ToolDefinition]) {
        use crate::tools::registry::version_signature;
        let live_by_name: std::collections::HashMap<&str, &ToolDefinition> =
            live.iter().map(|tool| (tool.name.as_str(), tool)).collect();
        for tool in active {
            let Some(next) = live_by_name.get(tool.name.as_str()) else {
                continue;
            };
            let signature = version_signature(next);
            if signature == version_signature(tool) {
                continue;
            }
            self.notify_update_once(&tool.name, &signature);
        }
    }
}

/// v2 `matchToolNameBlocks` / `foldAnnouncedToolNames`: read the tool names
/// out of a `<tools_added>` or `<tools_removed>` block.
pub fn match_tool_name_blocks(text: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut names = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        let Some(end) = after.find(&close) else {
            break;
        };
        for line in after[..end].lines() {
            let name = line.trim();
            if !name.is_empty() {
                names.push(name.to_string());
            }
        }
        rest = &after[end + close.len()..];
    }
    names
}

/// v2 `shapeUnsupportedDynamicToolContext`: when the current model cannot
/// dynamically load tools, a prior `<tools_added>` announcement in the
/// history must not keep telling it to call `select_tools`. Rewrite that
/// message's text into the runtime-change reminder, preserving the added /
/// removed names for the model's context.
pub fn shape_unsupported_dynamic_tool_history(
    messages: &mut [crate::turn_loop::types::LLMMessage],
    supports_dynamic_loading: bool,
) {
    if supports_dynamic_loading {
        return;
    }
    for message in messages {
        if message.role != "user" || message.content.is_empty() {
            continue;
        }
        let added = match_tool_name_blocks(&message.content, "tools_added");
        let removed = match_tool_name_blocks(&message.content, "tools_removed");
        if added.is_empty() && removed.is_empty() {
            continue;
        }
        message.content = render_runtime_tool_change_reminder(&added, &removed);
        message.blocks.clear();
    }
}

/// v2 `renderRuntimeToolChangeReminder` (final wording, #4720). A model that
/// cannot dynamically load tools sees this instead of a stale
/// `<tools_added>` announcement from an earlier capable model.
pub fn render_runtime_tool_change_reminder(added: &[String], removed: &[String]) -> String {
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
    sections.push(
        "The available tools changed during this session. The current model cannot load runtime \
         tool additions with select_tools. Do not call select_tools. Use the tool definitions \
         provided in this request. Removed tools may be unavailable or fail when called."
            .to_string(),
    );
    sections.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::registry::{ToolSource, schema_signature};
    use serde_json::json;

    fn tool(name: &str, deferred: bool) -> ToolDefinition {
        ToolDefinition {
            name: name.into(),
            description: format!("{name} description"),
            input_schema: json!({ "type": "object" }),
            source: ToolSource::Mcp,
            disclosure: if deferred {
                ToolDisclosure::Deferred
            } else {
                ToolDisclosure::Inline
            },
        }
    }

    #[test]
    fn reconcile_records_only_real_additions_and_removals() {
        let mut state = ToolSelectState::new();
        state.initialize(vec![tool("A", false)]);
        assert_eq!(state.reconcile(vec![tool("A", false)]), None);
        let change = state
            .reconcile(vec![tool("A", false), tool("B", true)])
            .unwrap()
            .clone();
        assert_eq!(change.added.len(), 1);
        assert_eq!(change.added[0].name, "B");
        assert!(change.removed.is_empty());
        assert_eq!(state.revision(), 1);
        assert_eq!(state.current().len(), 2);
    }

    #[test]
    fn announcement_walk_skips_changes_without_loadable_tools() {
        let mut state = ToolSelectState::new();
        state.initialize(vec![tool("A", false)]);
        state.reconcile(vec![tool("A", false), tool("inline", false)]);
        state.reconcile(vec![tool("A", false), tool("deferred", true)]);
        // The first change (adding an inline tool) has no loadable delta and
        // is skipped. The second replaces it with a deferred tool: the
        // deferred addition is announced, and the inline removal cannot be
        // hidden by a same-name non-loadable addition (v2's removed filter).
        let (revision, added, removed) = state.announcement_since(0).unwrap();
        assert_eq!(revision, 2);
        assert_eq!(added, vec!["deferred"]);
        assert_eq!(removed, vec!["inline"]);
        assert!(state.announcement_since(revision).is_none());
    }

    #[test]
    fn runtime_reminder_uses_the_final_wording_and_keeps_both_blocks() {
        let text = render_runtime_tool_change_reminder(&["new_tool".into()], &["old_tool".into()]);
        assert!(text.contains("<tools_added>\nnew_tool\n</tools_added>"));
        assert!(text.contains("<tools_removed>\nold_tool\n</tools_removed>"));
        assert!(text.contains("Use the tool definitions provided in this request."));
        assert!(!text.contains("initial session snapshot"));
    }

    #[test]
    fn a_pending_update_queues_one_notice_per_signature() {
        use crate::tools::registry::version_signature;
        let mut state = ToolSelectState::new();
        let active = vec![tool("A", false)];
        let mut updated = tool("A", false);
        updated.description = "changed".into();
        let signature = version_signature(&updated);
        state.observe_live_updates(&active, std::slice::from_ref(&updated));
        state.observe_live_updates(&active, std::slice::from_ref(&updated));
        assert_eq!(
            state.take_pending_notice().as_deref(),
            Some(
                "Tool \"A\" has an update, but the update is not applied in this session. \
                 Continue using the currently registered version."
            )
        );
        assert!(state.take_pending_notice().is_none());
        assert_eq!(version_signature(&updated), signature);
    }

    #[test]
    fn unsupported_history_rewrites_a_prior_announcement() {
        let mut messages = vec![
            crate::turn_loop::types::LLMMessage::user("hello"),
            crate::turn_loop::types::LLMMessage::user(
                "<tools_added>\nA\nB\n</tools_added>\n\n<tools_removed>\nC\n</tools_removed>",
            ),
        ];
        shape_unsupported_dynamic_tool_history(&mut messages, false);
        assert_eq!(messages[0].content, "hello");
        assert!(
            messages[1]
                .content
                .contains("Use the tool definitions provided in this request.")
        );
        assert!(
            messages[1]
                .content
                .contains("<tools_added>\nA\nB\n</tools_added>")
        );
        assert!(
            messages[1]
                .content
                .contains("<tools_removed>\nC\n</tools_removed>")
        );

        let mut capable = vec![crate::turn_loop::types::LLMMessage::user(
            "<tools_added>\nA\n</tools_added>",
        )];
        shape_unsupported_dynamic_tool_history(&mut capable, true);
        assert_eq!(capable[0].content, "<tools_added>\nA\n</tools_added>");
    }

    #[test]
    fn update_notification_is_once_per_signature() {
        let mut state = ToolSelectState::new();
        let signature = schema_signature(&tool("A", false));
        assert!(state.notify_update_once("A", &signature).is_some());
        assert!(state.notify_update_once("A", &signature).is_none());
        assert!(
            state
                .notify_update_once("A", "different-signature")
                .is_some()
        );
    }
}
