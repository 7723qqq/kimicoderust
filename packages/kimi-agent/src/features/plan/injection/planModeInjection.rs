use std::sync::{Arc, LazyLock};

use serde_json::{Value, json};

use super::super::plan::IAgentPlanService;
use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::agent::context_memory::context_memory::IAgentContextMemoryService;
use crate::agent::context_memory::types::ContextMessage;
use crate::agent::state::agent_state_v2::IAgentStateService;
use crate::features::reminder::reminder_service::ReminderService;
use crate::features::reminder::types::{
    ContextInjectionContent, ContextInjectionContext, ContextInjectionResult,
};
use crate::human::llm::message::Role;
use crate::state::state::{ReplayableOptions, ReplayableStateKey};
use crate::turn_loop::types::LLMMessage;

pub const PLAN_MODE_DEDUP_MIN_TURNS: usize = 2;
pub const PLAN_MODE_FULL_REFRESH_TURNS: usize = 5;
pub const PLAN_MODE_INJECTION_VARIANT: &str = "plan_mode";

pub static PLAN_WAS_ACTIVE_KEY: LazyLock<Arc<ReplayableStateKey>> = LazyLock::new(|| {
    Arc::new(ReplayableStateKey::new(
        "plan.wasActive".to_string(),
        Arc::new(|| Value::Bool(false)),
        ReplayableOptions {
            schema: json!({}),
            durable: Some(false),
            blobs: None,
            mutable_replay: None,
        },
    ))
});

pub fn plan_was_active_key() -> Arc<ReplayableStateKey> {
    PLAN_WAS_ACTIVE_KEY.clone()
}

pub const PLAN_MODE_EXIT_REMINDER: &str = include_str!("plan-mode-exit-reminder.md");
pub const PLAN_MODE_FULL_REMINDER: &str = include_str!("plan-mode-full-reminder.md");
pub const PLAN_MODE_INLINE_FULL_REMINDER: &str = include_str!("plan-mode-inline-full-reminder.md");
pub const PLAN_MODE_INLINE_REENTRY_REMINDER: &str =
    include_str!("plan-mode-inline-reentry-reminder.md");
pub const PLAN_MODE_INLINE_SPARSE_REMINDER: &str =
    include_str!("plan-mode-inline-sparse-reminder.md");
pub const PLAN_MODE_REENTRY_REMINDER: &str = include_str!("plan-mode-reentry-reminder.md");
pub const PLAN_MODE_SPARSE_REMINDER: &str = include_str!("plan-mode-sparse-reminder.md");

struct PlanModeInjectionInner {
    state_registration: DisposableRef,
    reminder_registration: Arc<dyn Fn() + Send + Sync>,
}

impl Drop for PlanModeInjectionInner {
    fn drop(&mut self) {
        (self.reminder_registration)();
        self.state_registration.dispose();
    }
}

pub struct PlanModeInjection {
    _inner: Arc<PlanModeInjectionInner>,
}

impl PlanModeInjection {
    pub fn new(
        injector: Arc<dyn ReminderService>,
        plan: Arc<dyn IAgentPlanService>,
        context: Arc<dyn IAgentContextMemoryService>,
        states: Arc<dyn IAgentStateService>,
    ) -> Self {
        let state_registration = states
            .contribute_state(plan_was_active_key().state_key())
            .unwrap_or_else(|error| panic!("{error}"));
        let reminder_registration = injector.register(
            PLAN_MODE_INJECTION_VARIANT,
            Arc::new(move |injection_context: ContextInjectionContext| {
                let plan = plan.clone();
                let context = context.clone();
                let states = states.clone();
                Box::pin(async move {
                    let Some(data) = plan.status().await? else {
                        if !was_active(&states) {
                            return Ok(None);
                        }
                        states.set(&plan_was_active_key().state_key(), Value::Bool(false));
                        return Ok(Some(injection(PLAN_MODE_EXIT_REMINDER.to_string())));
                    };
                    let plan_file_path = data.path.as_deref();
                    if !was_active(&states) {
                        states.set(&plan_was_active_key().state_key(), Value::Bool(true));
                        if !trim_js(&data.content).is_empty() {
                            return Ok(Some(injection(reentry_reminder(plan_file_path))));
                        }
                        return Ok(Some(injection(full_reminder(plan_file_path))));
                    }
                    let history = reminder_history(&context.get());
                    match plan_mode_reminder_variant(
                        injection_context.last_injected_at,
                        &history,
                    ) {
                        Some(PlanModeReminderVariant::Full) => {
                            Ok(Some(injection(full_reminder(plan_file_path))))
                        }
                        Some(PlanModeReminderVariant::Sparse) => {
                            Ok(Some(injection(sparse_reminder(plan_file_path))))
                        }
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
            _inner: Arc::new(PlanModeInjectionInner {
                state_registration,
                reminder_registration,
            }),
        }
    }
}

fn was_active(states: &Arc<dyn IAgentStateService>) -> bool {
    states.get(&plan_was_active_key().state_key()).as_bool() == Some(true)
}

fn injection(content: String) -> ContextInjectionResult {
    ContextInjectionResult {
        content: ContextInjectionContent::Text(content),
        disclosure: None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanModeReminderVariant {
    Full,
    Sparse,
}

pub fn plan_mode_reminder_variant(
    injected_at: Option<usize>,
    history: &[LLMMessage],
) -> Option<PlanModeReminderVariant> {
    let Some(injected_at) = injected_at else {
        return Some(PlanModeReminderVariant::Full);
    };
    let mut assistant_turns = 0usize;
    for message in history.iter().skip(injected_at + 1) {
        if message.role == "assistant" {
            assistant_turns += 1;
            continue;
        }
        if message.role == "user"
            && message
                .origin
                .as_ref()
                .and_then(|origin| origin.get("kind"))
                .and_then(serde_json::Value::as_str)
                != Some("injection")
        {
            return Some(PlanModeReminderVariant::Full);
        }
    }
    if assistant_turns >= PLAN_MODE_FULL_REFRESH_TURNS {
        return Some(PlanModeReminderVariant::Full);
    }
    if assistant_turns >= PLAN_MODE_DEDUP_MIN_TURNS {
        return Some(PlanModeReminderVariant::Sparse);
    }
    None
}

pub fn with_plan_file_footer(body: &str, plan_file_path: Option<&str>) -> String {
    match plan_file_path.filter(|path| !path.is_empty()) {
        Some(path) => format!("{body}\n\nPlan file: {path}"),
        None => body.to_string(),
    }
}

pub fn full_reminder(plan_file_path: Option<&str>) -> String {
    match plan_file_path.filter(|path| !path.is_empty()) {
        Some(path) => with_plan_file_footer(PLAN_MODE_FULL_REMINDER, Some(path)),
        None => PLAN_MODE_INLINE_FULL_REMINDER.to_string(),
    }
}

pub fn sparse_reminder(plan_file_path: Option<&str>) -> String {
    match plan_file_path.filter(|path| !path.is_empty()) {
        Some(path) => with_plan_file_footer(PLAN_MODE_SPARSE_REMINDER, Some(path)),
        None => PLAN_MODE_INLINE_SPARSE_REMINDER.to_string(),
    }
}

pub fn reentry_reminder(plan_file_path: Option<&str>) -> String {
    match plan_file_path.filter(|path| !path.is_empty()) {
        Some(path) => with_plan_file_footer(PLAN_MODE_REENTRY_REMINDER, Some(path)),
        None => PLAN_MODE_INLINE_REENTRY_REMINDER.to_string(),
    }
}

fn reminder_history(messages: &[ContextMessage]) -> Vec<LLMMessage> {
    messages
        .iter()
        .map(|message| LLMMessage {
            role: match message.role {
                Role::System => "system",
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "tool",
            }
            .to_string(),
            origin: message
                .origin
                .as_ref()
                .map(|origin| json!({ "kind": origin.kind() })),
            ..Default::default()
        })
        .collect()
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
