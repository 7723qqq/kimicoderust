//! Plugin session-start injection, ported from v2's
//! `agent/plugin/agentPluginService.ts`.
//!
//! A plugin may declare `sessionStart.skill`. On the session's first turn the
//! skill's instructions are injected as the `plugin_session_start` variant, so
//! the model starts every conversation with the plugin's onboarding context.
//! The reminder is frozen once injected: a plugin installed mid-session does
//! NOT change it (v2's notification says the session keeps what it started
//! with); only a host-driven refresh supersedes it. When the set becomes empty
//! after such a refresh, the `NO_ACTIVE_SESSION_STARTS` notice replaces the old
//! blocks so the model does not keep following a removed plugin's rules.
//!
//! v2 keeps the frozen content in agent state and reads the previous reminder
//! from `injection.lastInjection`; the Rust engine has neither, so both are
//! recovered from the conversation history, exactly like the interruption and
//! permission-mode reminders.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::injection::InjectionRegistry;
use crate::skills::{ParsedSkillMeta, PluginSkillDir};
use crate::turn_loop::types::LLMMessage;

/// The injection variant v2 registers (`agentPluginService.ts:24`).
pub const PLUGIN_SESSION_START_VARIANT: &str = "plugin_session_start";

/// The variant of the mid-session plugin-mutation notice.
pub const PLUGIN_CHANGE_VARIANT: &str = "plugin_change";

/// v2's `SUPERSEDES_SUFFIX`, verbatim.
pub const SUPERSEDES_SUFFIX: &str =
    "This supersedes any earlier plugin_session_start reminder in this session.";

/// v2's `NO_ACTIVE_SESSION_STARTS`, verbatim (the suffix is part of the text).
pub const NO_ACTIVE_SESSION_STARTS: &str = "There are currently no active plugin session starts. This supersedes any earlier plugin_session_start reminder in this session.";

/// One plugin skill resolved from a plugin skill root.
struct FoundSkill {
    /// The skill name as v2's plugin registry exposes it (without the
    /// `<plugin>:` prefix).
    name: String,
    content: String,
    dir: PathBuf,
    argument_names: Vec<String>,
}

/// Render the session-start blocks for every enabled plugin that declares one.
///
/// `dirs` is the session's plugin skill roots (in plugin-id order, one entry
/// per declared root). A plugin with several roots contributes one block; a
/// plugin whose declared skill cannot be found is skipped with a warning
/// (v2 `warnedSkills` — the fork warns on every turn instead of remembering,
/// because the scan is per turn and the log is the only consumer).
pub fn render_session_start(dirs: &[PluginSkillDir], session_id: &str) -> Option<String> {
    let mut blocks = Vec::new();
    let mut rendered_plugins: Vec<&str> = Vec::new();
    for dir in dirs {
        if rendered_plugins.contains(&dir.plugin_id.as_str()) {
            continue;
        }
        let Some(requested) = dir.session_start.as_deref() else {
            continue;
        };
        rendered_plugins.push(dir.plugin_id.as_str());
        let Some(skill) = find_plugin_skill(dirs, &dir.plugin_id, requested) else {
            tracing::warn!(
                plugin_id = %dir.plugin_id,
                skill = requested,
                "plugin sessionStart skill not found"
            );
            continue;
        };
        let content = render_skill_content(dirs, &dir.plugin_id, &skill, session_id);
        blocks.push(format!(
            "<plugin_session_start plugin=\"{}\" skill=\"{}\">\n{}\n</plugin_session_start>",
            crate::native::escape::escape_xml_attr(&dir.plugin_id),
            crate::native::escape::escape_xml_attr(&skill.name),
            content
        ));
    }
    (!blocks.is_empty()).then(|| blocks.join("\n"))
}

/// The plugin-mutation notices still to announce (v2 `plugin_change`).
///
/// `next_seq` numbers every notice; `last_seen` records how far each session
/// has read. The queue is capped: a process that mutates plugins thousands of
/// times between two turns gets the newest notices, not unbounded growth.
///
/// One process-wide queue serves every host (the HTTP engine and the napi
/// session pump): a mutation must be announced to every live session exactly
/// once, regardless of which transport performed it.
#[derive(Default)]
pub struct PluginChangeNotices {
    next_seq: u64,
    notices: Vec<(u64, String)>,
    last_seen: std::collections::HashMap<String, u64>,
}

impl PluginChangeNotices {
    const MAX_NOTICES: usize = 64;

    pub fn push(&mut self, text: String) {
        self.next_seq += 1;
        self.notices.push((self.next_seq, text));
        if self.notices.len() > Self::MAX_NOTICES {
            self.notices.remove(0);
        }
    }

    pub fn take_since(&mut self, session_id: &str) -> Option<String> {
        let Some(&seen) = self.last_seen.get(session_id) else {
            // A session that has not taken a turn yet renders its prompt from
            // the current plugin set, so notices queued before its first turn
            // are already reflected there; start it at the head instead of
            // replaying mutations it never saw.
            self.last_seen.insert(session_id.to_string(), self.next_seq);
            return None;
        };
        let latest = self.notices.last().map(|(seq, _)| *seq).unwrap_or(seen);
        let fresh: Vec<&str> = self
            .notices
            .iter()
            .filter(|(seq, _)| *seq > seen)
            .map(|(_, text)| text.as_str())
            .collect();
        self.last_seen.insert(session_id.to_string(), latest);
        (!fresh.is_empty()).then(|| fresh.join("\n\n"))
    }
}

fn global_notices() -> &'static Mutex<PluginChangeNotices> {
    static NOTICES: std::sync::OnceLock<Mutex<PluginChangeNotices>> = std::sync::OnceLock::new();
    NOTICES.get_or_init(|| Mutex::new(PluginChangeNotices::default()))
}

/// Queue a model-visible notice for a plugin mutation. Every live session
/// receives it once on its next turn.
pub fn publish_plugin_change_notice(kind: &str, id: &str) {
    let Some(text) = plugin_change_reminder(kind, id) else {
        return;
    };
    global_notices()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .push(text);
}

/// The plugin-mutation notices this session has not been told yet.
pub fn take_plugin_change_notices(session_id: &str) -> Option<String> {
    global_notices()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .take_since(session_id)
}

/// The mid-session plugin-mutation notice (v2 `renderPluginChangeReminder`):
/// the session keeps the prompt and tools it started with, so the model should
/// not claim the change is live.
pub fn plugin_change_reminder(kind: &str, id: &str) -> Option<String> {
    let verb = match kind {
        "install" => "installed",
        "enable" => "enabled",
        "disable" => "disabled",
        "remove" => "removed",
        "mcp-server" => "updated",
        _ => return None,
    };
    Some(format!(
        "Plugin \"{id}\" was {verb}. This session keeps the prompt and tools it started with; run /new or /reload to apply the change, and tell the user if they expect it now."
    ))
}

/// Reconcile the desired session-start text against the reminder already in
/// the conversation (v2 `reconcileSessionStartReminder`).
///
/// `force_refresh` is v2's `refreshPending` (set by `refreshSessionStart`,
/// called after `/reload`); without it a real change is frozen at the first
/// injection.
pub fn reconcile_session_start(
    desired: Option<&str>,
    history: &[LLMMessage],
    force_refresh: bool,
) -> Option<String> {
    let latest = last_session_start(history);
    let Some(desired) = desired else {
        if latest.is_none() && (!force_refresh || !should_neutralize(history)) {
            return None;
        }
        if latest.as_deref() == Some(NO_ACTIVE_SESSION_STARTS) {
            return None;
        }
        return Some(NO_ACTIVE_SESSION_STARTS.to_string());
    };
    let Some(latest) = latest else {
        return Some(desired.to_string());
    };
    let superseded = format!("{desired}\n\n{SUPERSEDES_SUFFIX}");
    if !force_refresh && (latest == desired.trim() || latest == superseded.trim()) {
        return None;
    }
    Some(superseded)
}

/// Register the session-start provider. The reminder heads the turn: it is
/// injected on the turn's first pass only, so a later step or a mid-turn
/// compaction rearm (which sees the injected message in history) cannot repeat
/// it.
pub fn register_plugin_session_start(
    registry: &mut InjectionRegistry,
    dirs: &[PluginSkillDir],
    session_id: &str,
    history: &[LLMMessage],
    force_refresh: bool,
) {
    let Some(reminder) = session_start_reminder(dirs, session_id, history, force_refresh) else {
        return;
    };
    let mut emitted = false;
    registry.register(
        PLUGIN_SESSION_START_VARIANT,
        Box::new(move |ctx| {
            if !ctx.is_new_turn || emitted {
                return None;
            }
            emitted = true;
            Some(reminder.clone())
        }),
    );
}

/// The session-start reminder for one turn, reconciled against `history`.
///
/// Shared by the injection registry (the HTTP engine) and the session pump
/// (the napi/TUI host, which prepends messages to the turn's history instead
/// of registering a provider).
pub fn session_start_reminder(
    dirs: &[PluginSkillDir],
    session_id: &str,
    history: &[LLMMessage],
    force_refresh: bool,
) -> Option<String> {
    let latest = last_session_start(history);
    // v2's frozen snapshot: when the session already announced its plugins,
    // later plugin changes do not rewrite the reminder until a refresh.
    let desired = match (latest.as_deref(), force_refresh) {
        (Some(previous), false) => frozen_content(previous),
        _ => render_session_start(dirs, session_id),
    };
    reconcile_session_start(desired.as_deref(), history, force_refresh)
}

/// v2 `frozenSessionStartContent`: the stored content without the supersede
/// suffix; the neutralized notice means "no session starts".
fn frozen_content(rendered: &str) -> Option<String> {
    if rendered == NO_ACTIVE_SESSION_STARTS {
        return None;
    }
    let suffix = format!("\n\n{SUPERSEDES_SUFFIX}");
    Some(
        rendered
            .strip_suffix(&suffix)
            .unwrap_or(rendered)
            .to_string(),
    )
}

/// The last session-start reminder's inner text, decoded from the history.
fn last_session_start(history: &[LLMMessage]) -> Option<String> {
    history.iter().rev().find_map(|message| {
        let origin_variant = message
            .origin
            .as_ref()
            .and_then(|origin| origin.get("variant"))
            .and_then(serde_json::Value::as_str);
        let is_variant = origin_variant == Some(PLUGIN_SESSION_START_VARIANT);
        let content = message.content.as_str();
        if !is_variant && !content.contains("<plugin_session_start") {
            return None;
        }
        Some(inner_content(content))
    })
}

/// Strip the `<system-reminder>` envelope and return the trimmed body.
fn inner_content(content: &str) -> String {
    content
        .trim()
        .strip_prefix(crate::injection::SYSTEM_REMINDER_PREFIX)
        .and_then(|rest| rest.strip_suffix(crate::injection::SYSTEM_REMINDER_SUFFIX))
        .unwrap_or(content)
        .trim()
        .to_string()
}

/// v2 `shouldNeutralizePluginSessionStart`: only a session that already
/// injected the reminder (or was compacted) needs the "no active" notice.
fn should_neutralize(history: &[LLMMessage]) -> bool {
    history.iter().any(|message| {
        crate::compaction::is_compaction_summary_message(message)
            || message
                .origin
                .as_ref()
                .and_then(|origin| origin.get("variant"))
                .and_then(serde_json::Value::as_str)
                == Some(PLUGIN_SESSION_START_VARIANT)
    })
}

/// Resolve a plugin's declared session-start skill from its skill roots.
///
/// The manifest names the skill the way v2's plugin registry registers it (the
/// bare name, optionally prefixed with `<plugin>:`). The scan mirrors the
/// skill catalog's layout: a root-level `SKILL.md`, direct child directories
/// holding `SKILL.md`, and bare `.md` files at the root.
fn find_plugin_skill(
    dirs: &[PluginSkillDir],
    plugin_id: &str,
    requested: &str,
) -> Option<FoundSkill> {
    let requested = requested
        .strip_prefix(&format!("{plugin_id}:"))
        .unwrap_or(requested);
    for dir in dirs.iter().filter(|dir| dir.plugin_id == plugin_id) {
        for candidate in skill_candidates(&dir.dir) {
            let Ok(content) = std::fs::read_to_string(&candidate.path) else {
                continue;
            };
            let meta: ParsedSkillMeta =
                crate::skills::parse_skill_frontmatter(&content, &candidate.name);
            if meta.name != requested {
                continue;
            }
            return Some(FoundSkill {
                name: meta.name,
                content,
                dir: candidate.dir,
                argument_names: meta.argument_names,
            });
        }
    }
    None
}

struct SkillCandidate {
    name: String,
    path: PathBuf,
    dir: PathBuf,
}

fn skill_candidates(root: &Path) -> Vec<SkillCandidate> {
    let mut out = Vec::new();
    if let Some(path) = skill_file_in(root) {
        let name = root
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        out.push(SkillCandidate {
            name,
            path,
            dir: root.to_path_buf(),
        });
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let file_name = entry.file_name().to_string_lossy().to_string();
        if file_type.is_dir() {
            if let Some(path) = skill_file_in(&entry.path()) {
                out.push(SkillCandidate {
                    name: file_name,
                    path,
                    dir: entry.path(),
                });
            }
        } else if file_type.is_file()
            && file_name.ends_with(".md")
            && file_name != "SKILL.md"
            && file_name != "skill.md"
        {
            out.push(SkillCandidate {
                name: file_name.trim_end_matches(".md").to_string(),
                path: entry.path(),
                dir: root.to_path_buf(),
            });
        }
    }
    out
}

fn skill_file_in(dir: &Path) -> Option<PathBuf> {
    for name in ["SKILL.md", "skill.md"] {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Render one skill prompt: strip the frontmatter, clear the argument
/// placeholders (a session start passes none), and prefix the plugin's own
/// `skillInstructions` the way v2's `renderSkillPrompt` does.
fn render_skill_content(
    dirs: &[PluginSkillDir],
    plugin_id: &str,
    skill: &FoundSkill,
    session_id: &str,
) -> String {
    let body = crate::tools::skill::strip_frontmatter(&skill.content);
    let content = crate::tools::skill::expand_skill_parameters(
        body,
        "",
        &skill.dir.to_string_lossy(),
        session_id,
        &skill.argument_names,
    );
    let instructions = dirs
        .iter()
        .find(|dir| dir.plugin_id == plugin_id)
        .and_then(|dir| dir.instructions.as_deref())
        .map(str::trim)
        .filter(|instructions| !instructions.is_empty());
    match instructions {
        Some(instructions) => format!(
            "<plugin-instructions plugin=\"{}\">\n{}\n</plugin-instructions>\n\n{content}",
            crate::native::escape::escape_xml_attr(plugin_id),
            instructions
        ),
        None => content,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::injection::{is_system_reminder, wrap_system_reminder};

    fn write_skill(dir: &Path, name: &str, body: &str) {
        let skill_dir = dir.join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: test skill\n---\n\n{body}"),
        )
        .unwrap();
    }

    fn skill_dir(root: &Path, plugin_id: &str, requested: &str) -> PluginSkillDir {
        PluginSkillDir {
            dir: root.to_path_buf(),
            plugin_id: plugin_id.into(),
            instructions: None,
            session_start: Some(requested.into()),
        }
    }

    fn reminder(content: &str) -> LLMMessage {
        crate::injection::injection_message_named(
            wrap_system_reminder(content),
            PLUGIN_SESSION_START_VARIANT,
        )
    }

    #[test]
    fn test_session_start_renders_the_skill_block() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("plugin").join("skills");
        write_skill(&root, "onboard", "Follow the house style.");
        let dirs = [skill_dir(&root, "brandpack", "onboard")];

        let text = render_session_start(&dirs, "ses_1").expect("renders");
        assert_eq!(
            text,
            "<plugin_session_start plugin=\"brandpack\" skill=\"onboard\">\nFollow the house style.\n</plugin_session_start>"
        );
        assert!(
            render_session_start(&dirs, "ses_1")
                .unwrap()
                .contains("Follow the house style."),
            "the body is the skill instructions"
        );
    }

    #[test]
    fn test_session_start_prefixes_plugin_instructions_and_expands_session_id() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("plugin").join("skills");
        write_skill(&root, "onboard", "session=${KIMI_SESSION_ID}");
        let mut entry = skill_dir(&root, "brandpack", "brandpack:onboard");
        entry.instructions = Some("  The brand rules.  ".into());

        let text = render_session_start(&[entry], "ses_9").unwrap();
        assert!(text.contains("session=ses_9"), "{text}");
        assert!(
            text.contains(
                "<plugin-instructions plugin=\"brandpack\">\nThe brand rules.\n</plugin-instructions>\n\nsession=ses_9"
            ),
            "{text}"
        );
    }

    #[test]
    fn test_missing_session_start_skill_is_skipped() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("plugin").join("skills");
        std::fs::create_dir_all(&root).unwrap();
        assert!(render_session_start(&[skill_dir(&root, "brandpack", "onboard")], "").is_none());
    }

    #[test]
    fn test_reconcile_injects_once_and_supersedes_changes() {
        // A first injection: no prior reminder.
        let text = "block";
        assert_eq!(
            reconcile_session_start(Some(text), &[], false),
            Some(text.to_string())
        );
        // The same content already in history stays silent.
        let history = [reminder(text)];
        assert_eq!(reconcile_session_start(Some(text), &history, false), None);
        // A different desired content supersedes the old block.
        assert_eq!(
            reconcile_session_start(Some("changed"), &history, false),
            Some(format!("changed\n\n{SUPERSEDES_SUFFIX}"))
        );
        // Once the superseding text is in history, it stays silent again.
        let superseded = [reminder(&format!("changed\n\n{SUPERSEDES_SUFFIX}"))];
        assert_eq!(
            reconcile_session_start(Some("changed"), &superseded, false),
            None
        );
    }

    #[test]
    fn test_registration_freezes_until_a_forced_refresh() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("plugin").join("skills");
        write_skill(&root, "onboard", "old");
        let dirs = [skill_dir(&root, "brandpack", "onboard")];
        let history = [reminder(
            "<plugin_session_start plugin=\"brandpack\" skill=\"onboard\">\nold\n</plugin_session_start>",
        )];

        // A plugin change mid-session must not rewrite the reminder.
        write_skill(&root, "onboard", "new");
        let mut registry = InjectionRegistry::new();
        register_plugin_session_start(&mut registry, &dirs, "ses_1", &history, false);
        assert!(
            registry.names().is_empty(),
            "the session keeps the session-start content it began with"
        );

        // A host-driven refresh supersedes it.
        let mut registry = InjectionRegistry::new();
        register_plugin_session_start(&mut registry, &dirs, "ses_1", &history, true);
        let texts = registry.build_injections(true);
        assert_eq!(texts.len(), 1);
        assert!(texts[0].contains("\nnew\n"), "{}", texts[0]);
        assert!(texts[0].contains(SUPERSEDES_SUFFIX), "{}", texts[0]);
    }

    #[test]
    fn test_removed_plugins_render_the_neutralized_notice_once() {
        let history = [reminder(
            "<plugin_session_start plugin=\"p\" skill=\"s\">\nx\n</plugin_session_start>",
        )];
        assert_eq!(
            reconcile_session_start(None, &history, false),
            Some(NO_ACTIVE_SESSION_STARTS.to_string())
        );
        let neutralized = [reminder(NO_ACTIVE_SESSION_STARTS)];
        assert_eq!(reconcile_session_start(None, &neutralized, false), None);
        assert_eq!(
            reconcile_session_start(None, &[], false),
            None,
            "a session that never announced plugins stays silent"
        );
    }

    #[test]
    fn test_neutralized_notice_after_compaction() {
        let mut summary = LLMMessage {
            role: "user".into(),
            content: "summary".into(),
            ..Default::default()
        };
        summary.origin = Some(serde_json::json!({ "kind": "compaction_summary" }));
        assert_eq!(
            reconcile_session_start(None, &[summary], true),
            Some(NO_ACTIVE_SESSION_STARTS.to_string()),
            "a forced refresh after compaction must tell the model there are none"
        );
    }

    #[test]
    fn test_registration_heads_the_turn_once() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("plugin").join("skills");
        write_skill(&root, "onboard", "Follow the house style.");
        let dirs = [skill_dir(&root, "brandpack", "onboard")];

        let mut registry = InjectionRegistry::new();
        register_plugin_session_start(&mut registry, &dirs, "ses_1", &[], false);
        let texts = registry.build_injections(true);
        assert_eq!(texts.len(), 1);
        assert!(is_system_reminder(&texts[0]));
        assert!(texts[0].contains("<plugin_session_start"));
        assert!(
            registry.build_injections(false).is_empty(),
            "a later step must not repeat the session-start reminder"
        );

        // Already announced: the resumed turn stays silent.
        let history = [reminder(&inner_content(&texts[0]))];
        let mut registry = InjectionRegistry::new();
        register_plugin_session_start(&mut registry, &dirs, "ses_1", &history, false);
        assert!(registry.names().is_empty());
    }

    #[test]
    fn test_plugin_change_reminder_uses_v2_text() {
        assert_eq!(
            plugin_change_reminder("install", "brandpack").unwrap(),
            "Plugin \"brandpack\" was installed. This session keeps the prompt and tools it started with; run /new or /reload to apply the change, and tell the user if they expect it now."
        );
        assert!(
            plugin_change_reminder("enable", "p")
                .unwrap()
                .contains("was enabled.")
        );
        assert!(
            plugin_change_reminder("disable", "p")
                .unwrap()
                .contains("was disabled.")
        );
        assert!(
            plugin_change_reminder("remove", "p")
                .unwrap()
                .contains("was removed.")
        );
        assert!(
            plugin_change_reminder("mcp-server", "p")
                .unwrap()
                .contains("was updated.")
        );
        assert!(plugin_change_reminder("unknown", "p").is_none());
    }

    #[test]
    fn test_plugin_change_notices_reach_a_live_session_once() {
        let mut notices = PluginChangeNotices::default();
        // The first turn seeds the watermark: mutations queued before it were
        // already reflected in that session's prompt.
        notices.push("Plugin \"a\" was installed.".into());
        assert_eq!(notices.take_since("sess-live"), None);
        notices.push("Plugin \"b\" was removed.".into());
        assert_eq!(
            notices.take_since("sess-live").as_deref(),
            Some("Plugin \"b\" was removed.")
        );
        assert_eq!(notices.take_since("sess-live"), None, "once per session");
    }

    #[test]
    fn test_plugin_change_notices_reach_every_session() {
        let mut notices = PluginChangeNotices::default();
        assert_eq!(notices.take_since("sess-a"), None);
        assert_eq!(notices.take_since("sess-b"), None);
        notices.push("Plugin \"a\" was enabled.".into());
        assert_eq!(
            notices.take_since("sess-a").as_deref(),
            Some("Plugin \"a\" was enabled.")
        );
        assert_eq!(
            notices.take_since("sess-b").as_deref(),
            Some("Plugin \"a\" was enabled.")
        );
    }

    #[test]
    fn test_plugin_change_notice_queue_is_bounded() {
        let mut notices = PluginChangeNotices::default();
        for index in 0..(PluginChangeNotices::MAX_NOTICES + 5) {
            notices.push(format!("notice {index}"));
        }
        assert_eq!(notices.notices.len(), PluginChangeNotices::MAX_NOTICES);
        assert_eq!(notices.notices[0].1, "notice 5");
    }
}
