//! Spec-mode reminders, ported from v2's `SpecModeInjection`
//! (`agent-core-v2/src/features/spec/injection/specModeInjection.ts`).
//!
//! v2 registers a `spec_mode` reminder variant whose provider reads the spec
//! service and re-announces the mode on a cadence: a full reminder when the mode
//! (re)activates or the conversation has moved on, a sparse one after two
//! assistant turns, nothing in between, and an exit reminder once. The Rust side
//! reads the `spec` state domain and reconstructs the last disclosure from the
//! conversation, the same shape as `swarm_mode` / `hello_world_mode`.

use std::path::Path;
use std::sync::Arc;

use crate::injection::state::DomainValueSource;
use crate::injection::InjectionContext;
use crate::primitives::read::ReadOutcome;
use crate::turn_loop::types::LLMMessage;

pub const SPEC_MODE_INJECTION_VARIANT: &str = "spec_mode";

const SPEC_MODE_DEDUP_MIN_TURNS: usize = 2;
const SPEC_MODE_FULL_REFRESH_TURNS: usize = 5;

const FULL_MARKER: &str = "Spec mode is active. You MUST NOT";
const REENTRY_MARKER: &str = "Spec mode is already active";
const SPARSE_MARKER: &str = "Spec mode still active.";
const EXIT_MARKER: &str = "Spec mode is no longer active.";

pub const SPEC_MODE_FULL_REMINDER: &str = include_str!("spec-mode-full-reminder.md");
pub const SPEC_MODE_REENTRY_REMINDER: &str = include_str!("spec-mode-reentry-reminder.md");
pub const SPEC_MODE_SPARSE_REMINDER: &str = include_str!("spec-mode-sparse-reminder.md");
pub const SPEC_MODE_EXIT_REMINDER: &str = include_str!("spec-mode-exit-reminder.md");

enum SpecModeCadence {
    Full,
    Sparse,
}

/// Register the `spec_mode` variant (v2 `SpecModeInjection`). `workspace_root`
/// resolves the spec directory for the footer; `state_store` reads the `spec`
/// domain.
pub fn register_spec_mode_injection<S>(
    registry: &mut crate::injection::InjectionRegistry,
    state_store: Arc<S>,
    workspace_root: String,
) where
    S: DomainValueSource + Send + Sync + 'static,
{
    registry.register(
        SPEC_MODE_INJECTION_VARIANT,
        Box::new(move |ctx: &InjectionContext| {
            let ReadOutcome::Found(state) = state_store.read_domain_state("spec") else {
                return None;
            };
            let active = state.get("active").and_then(|value| value.as_bool()) == Some(true);
            let id = state
                .get("id")
                .and_then(|value| value.as_str())
                .filter(|id| !id.is_empty());
            let last = last_disclosure(ctx.history);
            if !active {
                // v2: the exit reminder fires only after the mode was announced.
                return match last {
                    Some((_, true)) => Some(SPEC_MODE_EXIT_REMINDER.to_string()),
                    _ => None,
                };
            }
            let Some(id) = id else { return None };
            let spec_dir = crate::tools::spec_mode::spec_dir(Path::new(&workspace_root), id);
            let spec_dir = spec_dir.to_string_lossy().to_string();
            let has_content = spec_has_content(&spec_dir);
            let body = match last {
                // Still announced: the cadence decides between the full and the
                // sparse body (v2 `specModeReminderVariant`).
                Some((index, true)) => match reminder_variant(index, ctx.history) {
                    Some(SpecModeCadence::Full) => SPEC_MODE_FULL_REMINDER,
                    Some(SpecModeCadence::Sparse) => SPEC_MODE_SPARSE_REMINDER,
                    None => return None,
                },
                // First announcement after an entry, or a re-entry after the exit
                // reminder: the re-entry body when the spec already has content.
                None | Some((_, false)) => {
                    if has_content {
                        SPEC_MODE_REENTRY_REMINDER
                    } else {
                        SPEC_MODE_FULL_REMINDER
                    }
                }
            };
            Some(with_spec_dir_footer(body, &spec_dir))
        }),
    );
}

/// v2 `specModeReminderVariant`: the full reminder after a non-injection user
/// message or five assistant turns, a sparse one after two, nothing before that.
fn reminder_variant(last_index: usize, history: &[LLMMessage]) -> Option<SpecModeCadence> {
    let mut assistant_turns = 0;
    for message in history.iter().skip(last_index + 1) {
        match message.role.as_str() {
            "assistant" => assistant_turns += 1,
            "user" => {
                if !crate::injection::is_system_reminder(&message.content) {
                    return Some(SpecModeCadence::Full);
                }
            }
            _ => {}
        }
    }
    if assistant_turns >= SPEC_MODE_FULL_REFRESH_TURNS {
        return Some(SpecModeCadence::Full);
    }
    if assistant_turns >= SPEC_MODE_DEDUP_MIN_TURNS {
        return Some(SpecModeCadence::Sparse);
    }
    None
}

/// The last spec-mode disclosure in the conversation: its index and whether it
/// announced an active mode.
fn last_disclosure(history: &[LLMMessage]) -> Option<(usize, bool)> {
    for (index, message) in history.iter().enumerate().rev() {
        if !crate::injection::is_system_reminder(&message.content) {
            continue;
        }
        let content = message.content.as_str();
        if content.contains(EXIT_MARKER) {
            return Some((index, false));
        }
        if content.contains(FULL_MARKER)
            || content.contains(REENTRY_MARKER)
            || content.contains(SPARSE_MARKER)
        {
            return Some((index, true));
        }
    }
    None
}

fn spec_has_content(spec_dir: &str) -> bool {
    crate::tools::spec_mode::SPEC_REQUIRED_FILES.iter().any(|file| {
        std::fs::read_to_string(Path::new(spec_dir).join(file))
            .map(|content| !content.trim().is_empty())
            .unwrap_or(false)
    })
}

fn with_spec_dir_footer(body: &str, spec_dir: &str) -> String {
    if spec_dir.is_empty() {
        return body.to_string();
    }
    format!("{body}\n\nSpec directory: {spec_dir}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::injection::{InjectionRegistry, StepAccess, wrap_system_reminder};
    use std::sync::Mutex;

    struct FakeStore {
        state: Mutex<Option<serde_json::Value>>,
    }

    impl DomainValueSource for FakeStore {
        fn read_domain_state(&self, domain: &str) -> ReadOutcome<serde_json::Value> {
            if domain != "spec" {
                return ReadOutcome::Absent;
            }
            match self.state.lock().unwrap().clone() {
                Some(value) => ReadOutcome::Found(value),
                None => ReadOutcome::Absent,
            }
        }
    }

    fn store(state: Option<serde_json::Value>) -> Arc<FakeStore> {
        Arc::new(FakeStore {
            state: Mutex::new(state),
        })
    }

    fn active(id: &str) -> Option<serde_json::Value> {
        Some(serde_json::json!({ "active": true, "id": id }))
    }

    fn registry<S>(state: Arc<S>, root: &str) -> InjectionRegistry
    where
        S: DomainValueSource + Send + Sync + 'static,
    {
        let mut registry = InjectionRegistry::new();
        register_spec_mode_injection(&mut registry, state, root.to_string());
        registry
    }

    fn reminder_message(body: &str) -> LLMMessage {
        LLMMessage {
            role: kimi_core::types::Role::User,
            content: wrap_system_reminder(body),
            ..Default::default()
        }
    }

    fn assistant() -> LLMMessage {
        LLMMessage {
            role: kimi_core::types::Role::Assistant,
            content: "working".into(),
            ..Default::default()
        }
    }

    fn temp_root() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_string_lossy().to_string();
        (dir, root)
    }

    #[test]
    fn entering_an_undisclosed_spec_mode_injects_the_full_reminder_once() {
        let (_dir, root) = temp_root();
        let mut registry = registry(store(active("spec-1")), &root);
        let first = registry.build_injections(true);
        assert_eq!(first.len(), 1, "{first:?}");
        assert!(
            first[0].contains("Spec mode is active. You MUST NOT"),
            "{first:?}"
        );
        let spec_dir = crate::tools::spec_mode::spec_dir(Path::new(&root), "spec-1")
            .to_string_lossy()
            .to_string();
        assert!(
            first[0].contains(&format!("Spec directory: {spec_dir}")),
            "{first:?}"
        );
        // The turn loop passes the live history, so the disclosure the first
        // pass appended is what suppresses a repeat.
        let history = vec![LLMMessage {
            role: kimi_core::types::Role::User,
            content: first[0].clone(),
            ..Default::default()
        }];
        assert!(
            registry
                .build_injections_with_history(false, &StepAccess::default(), &history)
                .is_empty(),
            "an unchanged mode must not repeat"
        );
    }

    #[test]
    fn an_inactive_or_idless_spec_mode_is_silent() {
        let (_dir, root) = temp_root();
        let mut inactive = registry(
            store(Some(serde_json::json!({ "active": false }))),
            &root,
        );
        assert!(inactive.build_injections(true).is_empty());

        let mut idless = registry(store(Some(serde_json::json!({ "active": true }))), &root);
        assert!(idless.build_injections(true).is_empty());
    }

    #[test]
    fn leaving_an_announced_spec_mode_injects_the_exit_reminder_once() {
        let (_dir, root) = temp_root();
        let history = vec![reminder_message(SPEC_MODE_FULL_REMINDER)];
        let mut registry = registry(
            store(Some(serde_json::json!({ "active": false }))),
            &root,
        );
        let access = StepAccess::default();
        assert_eq!(
            registry.build_injections_with_history(true, &access, &history),
            vec![wrap_system_reminder(SPEC_MODE_EXIT_REMINDER)]
        );
        // Once the exit reminder is in history the mode is no longer "announced",
        // so nothing repeats.
        let mut after = history.clone();
        after.push(reminder_message(SPEC_MODE_EXIT_REMINDER));
        assert!(
            registry
                .build_injections_with_history(false, &access, &after)
                .is_empty()
        );
    }

    #[test]
    fn the_baseline_reads_the_last_disclosure() {
        assert_eq!(last_disclosure(&[]), None);
        assert_eq!(
            last_disclosure(&[reminder_message(SPEC_MODE_FULL_REMINDER)]),
            Some((0, true))
        );
        assert_eq!(
            last_disclosure(&[reminder_message(SPEC_MODE_EXIT_REMINDER)]),
            Some((0, false))
        );
        assert_eq!(
            last_disclosure(&[
                reminder_message(SPEC_MODE_FULL_REMINDER),
                reminder_message(SPEC_MODE_EXIT_REMINDER),
            ]),
            Some((1, false))
        );
    }

    #[test]
    fn the_cadence_walks_sparse_then_full() {
        let (_dir, root) = temp_root();
        let mut history = vec![reminder_message(SPEC_MODE_FULL_REMINDER)];
        let mut registry = registry(store(active("spec-1")), &root);
        let access = StepAccess::default();

        history.push(assistant());
        assert!(
            registry
                .build_injections_with_history(false, &access, &history)
                .is_empty(),
            "one assistant turn is not enough"
        );

        history.push(assistant());
        let sparse = registry.build_injections_with_history(false, &access, &history);
        assert_eq!(sparse.len(), 1, "{sparse:?}");
        assert!(sparse[0].contains("Spec mode still active."), "{sparse:?}");

        history.push(assistant());
        history.push(assistant());
        history.push(assistant());
        let full = registry.build_injections_with_history(false, &access, &history);
        assert_eq!(full.len(), 1, "{full:?}");
        assert!(
            full[0].contains("Spec mode is active. You MUST NOT"),
            "{full:?}"
        );
    }

    #[test]
    fn a_user_message_forces_the_full_reminder() {
        let (_dir, root) = temp_root();
        let mut history = vec![
            reminder_message(SPEC_MODE_FULL_REMINDER),
            assistant(),
            LLMMessage {
                role: kimi_core::types::Role::User,
                content: "keep going".into(),
                ..Default::default()
            },
        ];
        let mut registry = registry(store(active("spec-1")), &root);
        let access = StepAccess::default();
        let full = registry.build_injections_with_history(false, &access, &history);
        assert_eq!(full.len(), 1, "{full:?}");
        assert!(
            full[0].contains("Spec mode is active. You MUST NOT"),
            "{full:?}"
        );
    }
}
