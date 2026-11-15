use std::sync::{Arc, LazyLock};

use serde_json::{Value, json};

use super::super::spec::IAgentSpecService;
use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::agent::context_memory::context_memory::IAgentContextMemoryService;
use crate::agent::context_memory::types::ContextMessage;
use crate::agent::state::agent_state::IAgentStateService;
use crate::features::reminder::reminder_service::ReminderService;
use crate::features::reminder::types::{
    ContextInjectionContent, ContextInjectionContext, ContextInjectionResult,
};
use crate::state::state::{ReplayableOptions, ReplayableStateKey};

pub const SPEC_MODE_DEDUP_MIN_TURNS: usize = 2;
pub const SPEC_MODE_FULL_REFRESH_TURNS: usize = 5;
pub const SPEC_MODE_INJECTION_VARIANT: &str = "spec_mode";

pub static SPEC_WAS_ACTIVE_KEY: LazyLock<Arc<ReplayableStateKey>> = LazyLock::new(|| {
    Arc::new(ReplayableStateKey::new(
        "spec.wasActive".to_string(),
        Arc::new(|| Value::Bool(false)),
        ReplayableOptions {
            schema: json!({}),
            durable: Some(false),
            blobs: None,
            mutable_replay: None,
        },
    ))
});

pub fn spec_was_active_key() -> Arc<ReplayableStateKey> {
    SPEC_WAS_ACTIVE_KEY.clone()
}

pub const SPEC_MODE_EXIT_REMINDER: &str = include_str!("spec-mode-exit-reminder.md");
pub const SPEC_MODE_FULL_REMINDER: &str = include_str!("spec-mode-full-reminder.md");
pub const SPEC_MODE_REENTRY_REMINDER: &str = include_str!("spec-mode-reentry-reminder.md");
pub const SPEC_MODE_SPARSE_REMINDER: &str = include_str!("spec-mode-sparse-reminder.md");

struct SpecModeInjectionInner {
    state_registration: DisposableRef,
    reminder_registration: Arc<dyn Fn() + Send + Sync>,
}

impl Drop for SpecModeInjectionInner {
    fn drop(&mut self) {
        (self.reminder_registration)();
        self.state_registration.dispose();
    }
}

pub struct SpecModeInjection {
    _inner: Arc<SpecModeInjectionInner>,
}

impl SpecModeInjection {
    pub fn new(
        injector: Arc<dyn ReminderService>,
        spec: Arc<dyn IAgentSpecService>,
        context: Arc<dyn IAgentContextMemoryService>,
        states: Arc<dyn IAgentStateService>,
    ) -> Self {
        let state_registration = states
            .contribute_state(spec_was_active_key().state_key())
            .unwrap_or_else(|error| panic!("{error}"));
        let reminder_registration = injector.register(
            SPEC_MODE_INJECTION_VARIANT,
            Arc::new(move |injection_context: ContextInjectionContext| {
                let spec = spec.clone();
                let context = context.clone();
                let states = states.clone();
                Box::pin(async move {
                    let Some(data) = spec.status().await? else {
                        if !was_active(&states) {
                            return Ok(None);
                        }
                        states.set(&spec_was_active_key().state_key(), Value::Bool(false));
                        return Ok(Some(injection(SPEC_MODE_EXIT_REMINDER.to_string())));
                    };
                    let spec_dir = data.dir.clone();
                    if !was_active(&states) {
                        states.set(&spec_was_active_key().state_key(), Value::Bool(true));
                        let has_content = data
                            .files
                            .values()
                            .any(|content| !trim_js(content).is_empty());
                        let body = if has_content {
                            SPEC_MODE_REENTRY_REMINDER
                        } else {
                            SPEC_MODE_FULL_REMINDER
                        };
                        return Ok(Some(injection(with_spec_dir_footer(body, &spec_dir))));
                    }
                    let history = context.get();
                    match spec_mode_reminder_variant(
                        injection_context.last_injected_at,
                        &history,
                    ) {
                        Some(SpecModeReminderVariant::Full) => Ok(Some(injection(
                            with_spec_dir_footer(SPEC_MODE_FULL_REMINDER, &spec_dir),
                        ))),
                        Some(SpecModeReminderVariant::Sparse) => Ok(Some(injection(
                            with_spec_dir_footer(SPEC_MODE_SPARSE_REMINDER, &spec_dir),
                        ))),
                        None => Ok(None),
                    }
                })
                    as crate::rpc::types::BoxFuture<
                        'static,
                        Result<Option<ContextInjectionResult>, String>,
                    >
            }),
        );
        Self {
            _inner: Arc::new(SpecModeInjectionInner {
                state_registration,
                reminder_registration,
            }),
        }
    }
}

fn was_active(states: &Arc<dyn IAgentStateService>) -> bool {
    states.get(&spec_was_active_key().state_key()).as_bool() == Some(true)
}

fn injection(content: String) -> ContextInjectionResult {
    ContextInjectionResult {
        content: ContextInjectionContent::Text(content),
        disclosure: None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecModeReminderVariant {
    Full,
    Sparse,
}

pub fn spec_mode_reminder_variant(
    injected_at: Option<usize>,
    history: &[ContextMessage],
) -> Option<SpecModeReminderVariant> {
    let Some(injected_at) = injected_at else {
        return Some(SpecModeReminderVariant::Full);
    };
    let mut assistant_turns = 0usize;
    for message in history.iter().skip(injected_at + 1) {
        if message.role == "assistant" {
            assistant_turns += 1;
            continue;
        }
        if message.role == "user" {
            return Some(SpecModeReminderVariant::Full);
        }
    }
    if assistant_turns >= SPEC_MODE_FULL_REFRESH_TURNS {
        return Some(SpecModeReminderVariant::Full);
    }
    if assistant_turns >= SPEC_MODE_DEDUP_MIN_TURNS {
        return Some(SpecModeReminderVariant::Sparse);
    }
    None
}

pub fn with_spec_dir_footer(body: &str, spec_dir: &str) -> String {
    if spec_dir.is_empty() {
        return body.to_string();
    }
    format!("{body}\n\nSpec directory: {spec_dir}")
}

fn trim_js(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

fn is_js_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'
            | '\u{000a}'
            | '\u{000b}'
            | '\u{000c}'
            | '\u{000d}'
            | '\u{0020}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'
            | '\u{2001}'
            | '\u{2002}'
            | '\u{2003}'
            | '\u{2004}'
            | '\u{2005}'
            | '\u{2006}'
            | '\u{2007}'
            | '\u{2008}'
            | '\u{2009}'
            | '\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}
