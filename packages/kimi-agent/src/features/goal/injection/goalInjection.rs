use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::agent::context_memory::context_memory::IAgentContextMemoryService;
use crate::agent::context_memory::types::ContextMessage;
use crate::features::goal::types::{GoalSnapshot, GoalStatus};
use crate::features::reminder::reminder_service::ReminderService;
use crate::features::reminder::types::{
    ContextInjectionContent, ContextInjectionContext, ContextInjectionResult,
};
use crate::rpc::types::BoxFuture;

const GOAL_ACTIVE_REMINDER: &str = include_str!("goal-active-reminder.md");
const GOAL_ACTIVE_SPARSE_REMINDER: &str = include_str!("goal-active-sparse-reminder.md");
const GOAL_BLOCKED_REMINDER: &str = include_str!("goal-blocked-reminder.md");
const GOAL_PAUSED_REMINDER: &str = include_str!("goal-paused-reminder.md");

const BUDGET_GUIDANCE_NEARING: &str = "Budget guidance: you are nearing a budget. Converge on the objective and avoid starting new discretionary work.";
const BUDGET_GUIDANCE_WITHIN: &str =
    "Budget guidance: you are within budget. Make steady, focused progress toward the objective.";

pub const GOAL_WAIT_FOR_GUIDANCE: &str = "If you are waiting for background sub-agents or bash tasks to finish, call WaitFor to wait for them inside this turn instead of ending the turn; ending the turn just gets you re-invoked again and again. You can also use the waiting time to do useful parallel work. Either way, make sure every goal turn is productive.";
pub const GOAL_TAIL_ANCHOR_THRESHOLD_TOKENS: u64 = 20_000;

#[derive(Clone)]
pub struct GoalInjectionOptions {
    pub get_goal: Arc<dyn Fn() -> Option<GoalSnapshot> + Send + Sync>,
    pub is_wait_for_enabled: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoalReminderDisclosure {
    status: GoalStatus,
    goal_id: String,
    objective: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    completion_criterion: Option<String>,
    active: bool,
    budget_band: BudgetBand,
    wait_for_enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum BudgetBand {
    Within,
    Nearing,
}

struct GoalInjectionInner {
    options: GoalInjectionOptions,
    context: Arc<dyn IAgentContextMemoryService>,
    anchor_goal_id: Mutex<Option<String>>,
    anchor_tokens: Mutex<Option<u64>>,
}

impl GoalInjectionInner {
    fn reminder(&self, injection: &ContextInjectionContext) -> Option<ContextInjectionResult> {
        if !injection.is_new_turn {
            return None;
        }
        let goal = (self.options.get_goal)()?;
        let wait_for_enabled = self
            .options
            .is_wait_for_enabled
            .as_ref()
            .is_some_and(|enabled| enabled());
        let disclosure = goal_disclosure(&goal, wait_for_enabled);
        let full = goal.status != GoalStatus::Active
            || needs_full_goal_reminder(injection, &disclosure, &self.context.get());
        Some(ContextInjectionResult {
            content: ContextInjectionContent::Text(build_goal_reminder(
                &goal,
                wait_for_enabled,
                full,
            )),
            disclosure: serde_json::to_value(disclosure).ok(),
        })
    }

    fn tail_contract(&self, estimated_tokens: Option<usize>) -> Option<String> {
        let estimated_tokens = estimated_tokens? as u64;
        let goal = (self.options.get_goal)()?;
        if goal.status != GoalStatus::Active {
            return None;
        }
        {
            let mut anchor_goal_id = self.anchor_goal_id.lock();
            if anchor_goal_id.as_deref() != Some(goal.goal_id.as_str()) {
                *anchor_goal_id = Some(goal.goal_id.clone());
                *self.anchor_tokens.lock() = None;
            }
        }
        let constraints = goal
            .completion_criterion
            .as_ref()
            .map(|criterion| vec![criterion.clone()])
            .unwrap_or_default();
        let text = crate::injection::goal_plan::plan_tail_contract(
            estimated_tokens,
            GOAL_TAIL_ANCHOR_THRESHOLD_TOKENS,
            *self.anchor_tokens.lock(),
            &goal.objective,
            &constraints,
        )?;
        *self.anchor_tokens.lock() = Some(estimated_tokens);
        Some(text)
    }
}

pub struct GoalInjection {
    disposables: Mutex<Vec<Arc<dyn Fn() + Send + Sync>>>,
}

impl GoalInjection {
    pub fn new(
        options: GoalInjectionOptions,
        injector: Arc<dyn ReminderService>,
        context: Arc<dyn IAgentContextMemoryService>,
    ) -> Self {
        let inner = Arc::new(GoalInjectionInner {
            options,
            context,
            anchor_goal_id: Mutex::new(None),
            anchor_tokens: Mutex::new(None),
        });
        let goal = {
            let inner = inner.clone();
            injector.register(
                "goal",
                Arc::new(
                    move |injection| -> BoxFuture<
                        'static,
                        Result<Option<ContextInjectionResult>, String>,
                    > {
                        let inner = inner.clone();
                        Box::pin(async move { Ok(inner.reminder(&injection)) })
                    },
                ),
            )
        };
        let tail = {
            let inner = inner.clone();
            injector.register(
                "goal_tail",
                Arc::new(
                    move |injection| -> BoxFuture<
                        'static,
                        Result<Option<ContextInjectionResult>, String>,
                    > {
                        let inner = inner.clone();
                        Box::pin(async move {
                            Ok(if injection.is_new_turn {
                                inner.tail_contract(injection.estimated_tokens).map(|text| {
                                    ContextInjectionResult {
                                        content: ContextInjectionContent::Text(text),
                                        disclosure: None,
                                    }
                                })
                            } else {
                                None
                            })
                        })
                    },
                ),
            )
        };
        Self {
            disposables: Mutex::new(vec![goal, tail]),
        }
    }

    pub fn dispose(&self) {
        for disposable in std::mem::take(&mut *self.disposables.lock()) {
            disposable();
        }
    }
}

impl Drop for GoalInjection {
    fn drop(&mut self) {
        self.dispose();
    }
}

fn goal_disclosure(goal: &GoalSnapshot, wait_for_enabled: bool) -> GoalReminderDisclosure {
    GoalReminderDisclosure {
        status: goal.status,
        goal_id: goal.goal_id.clone(),
        objective: goal.objective.clone(),
        completion_criterion: goal.completion_criterion.clone(),
        active: goal.status == GoalStatus::Active,
        budget_band: if is_nearing_budget(goal) {
            BudgetBand::Nearing
        } else {
            BudgetBand::Within
        },
        wait_for_enabled,
    }
}

fn needs_full_goal_reminder(
    injection: &ContextInjectionContext,
    goal: &GoalReminderDisclosure,
    history: &[ContextMessage],
) -> bool {
    let previous = injection
        .last_disclosure
        .as_ref()
        .and_then(|value| serde_json::from_value::<GoalReminderDisclosure>(value.clone()).ok());
    let Some(previous) = previous else {
        return true;
    };
    if injection.injected_positions.is_empty()
        || !previous.active
        || previous.goal_id != goal.goal_id
        || previous.status != goal.status
        || previous.objective != goal.objective
        || previous.completion_criterion != goal.completion_criterion
        || previous.budget_band != goal.budget_band
        || previous.wait_for_enabled != goal.wait_for_enabled
    {
        return true;
    }
    history
        .get(injection.last_injected_at.unwrap_or(0)..)
        .unwrap_or(&[])
        .iter()
        .any(crate::compaction::is_compaction_summary_message)
}

fn build_blocked_note(goal: &GoalSnapshot) -> String {
    render_prompt(
        GOAL_BLOCKED_REMINDER,
        &[
            ("reason_suffix", reason_suffix(goal)),
            ("objective", escape_untrusted_text(&goal.objective)),
            (
                "completion_criterion_block",
                completion_criterion_block(goal),
            ),
        ],
    )
}

fn build_paused_note(goal: &GoalSnapshot) -> String {
    render_prompt(
        GOAL_PAUSED_REMINDER,
        &[
            ("reason_suffix", reason_suffix(goal)),
            ("objective", escape_untrusted_text(&goal.objective)),
            (
                "completion_criterion_block",
                completion_criterion_block(goal),
            ),
        ],
    )
}

fn build_goal_reminder(goal: &GoalSnapshot, wait_for_enabled: bool, full: bool) -> String {
    if goal.status == GoalStatus::Blocked {
        return build_blocked_note(goal);
    }
    if goal.status == GoalStatus::Paused {
        return build_paused_note(goal);
    }
    let budgets = format_budgets(goal);
    render_prompt(
        if full {
            GOAL_ACTIVE_REMINDER
        } else {
            GOAL_ACTIVE_SPARSE_REMINDER
        },
        &[
            ("objective", escape_untrusted_text(&goal.objective)),
            (
                "completion_criterion_block",
                completion_criterion_block(goal),
            ),
            ("status", goal.status.to_string()),
            (
                "progress",
                format!(
                    "{} continuation turns, {} tokens, {} elapsed",
                    goal.turns_used,
                    goal.tokens_used,
                    format_elapsed(goal.wall_clock_ms)
                ),
            ),
            (
                "budgets_block",
                if budgets.is_empty() {
                    String::new()
                } else {
                    format!("Budgets: {budgets}.\n")
                },
            ),
            (
                "budget_guidance",
                if is_nearing_budget(goal) {
                    BUDGET_GUIDANCE_NEARING.to_string()
                } else {
                    BUDGET_GUIDANCE_WITHIN.to_string()
                },
            ),
            (
                "wait_for_guidance",
                if wait_for_enabled {
                    format!(" {GOAL_WAIT_FOR_GUIDANCE}")
                } else {
                    String::new()
                },
            ),
        ],
    )
}

fn reason_suffix(goal: &GoalSnapshot) -> String {
    goal.terminal_reason
        .as_ref()
        .map(|reason| format!(" ({})", escape_untrusted_text(reason)))
        .unwrap_or_default()
}

fn completion_criterion_block(goal: &GoalSnapshot) -> String {
    goal.completion_criterion
        .as_ref()
        .map(|criterion| {
            format!(
                "<untrusted_completion_criterion>\n{}\n</untrusted_completion_criterion>\n",
                escape_untrusted_text(criterion)
            )
        })
        .unwrap_or_default()
}

fn format_budgets(goal: &GoalSnapshot) -> String {
    let mut lines = Vec::new();
    if let Some(turn_budget) = goal.budget.turn_budget {
        lines.push(format!(
            "turns {}/{} (remaining {})",
            goal.turns_used,
            turn_budget,
            goal.budget
                .remaining_turns
                .map(|remaining| remaining.to_string())
                .unwrap_or_else(|| "null".to_string())
        ));
    }
    if let Some(token_budget) = goal.budget.token_budget {
        lines.push(format!(
            "tokens {}/{} (remaining {})",
            goal.tokens_used,
            token_budget,
            goal.budget
                .remaining_tokens
                .map(|remaining| remaining.to_string())
                .unwrap_or_else(|| "null".to_string())
        ));
    }
    if let Some(wall_clock_budget_ms) = goal.budget.wall_clock_budget_ms {
        lines.push(format!(
            "time {}/{} (remaining {})",
            format_elapsed(goal.wall_clock_ms),
            format_elapsed(wall_clock_budget_ms),
            format_elapsed(goal.budget.remaining_wall_clock_ms.unwrap_or(0))
        ));
    }
    lines.join("; ")
}

fn is_nearing_budget(goal: &GoalSnapshot) -> bool {
    max_budget_fraction(goal) >= 0.75
}

fn max_budget_fraction(goal: &GoalSnapshot) -> f64 {
    let mut fractions = Vec::new();
    if let Some(turn_budget) = goal.budget.turn_budget.filter(|budget| *budget > 0) {
        fractions.push(goal.turns_used as f64 / turn_budget as f64);
    }
    if let Some(token_budget) = goal.budget.token_budget.filter(|budget| *budget > 0) {
        fractions.push(goal.tokens_used as f64 / token_budget as f64);
    }
    if let Some(wall_clock_budget_ms) = goal
        .budget
        .wall_clock_budget_ms
        .filter(|budget| *budget > 0)
    {
        fractions.push(goal.wall_clock_ms as f64 / wall_clock_budget_ms as f64);
    }
    fractions.into_iter().fold(0.0, f64::max)
}

fn escape_untrusted_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn format_elapsed(ms: i64) -> String {
    let total_seconds = (ms as f64 / 1000.0).round() as i64;
    if total_seconds < 60 {
        return format!("{total_seconds}s");
    }
    let minutes = total_seconds / 60;
    let seconds = total_seconds % 60;
    if minutes < 60 {
        return format!("{minutes}m{seconds:02}s");
    }
    let hours = minutes / 60;
    format!("{hours}h{:02}m", minutes % 60)
}

fn render_prompt(template: &str, vars: &[(&str, String)]) -> String {
    let mut output = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("${") {
        output.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            output.push_str(&rest[start..]);
            return output;
        };
        let name = &after[..end];
        let valid = name
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
            && name
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '_');
        match vars.iter().find(|(key, _)| valid && *key == name) {
            Some((_, value)) => output.push_str(value),
            None => {
                output.push_str("${");
                output.push_str(name);
                output.push('}');
            }
        }
        rest = &after[end + 1..];
    }
    output.push_str(rest);
    output
}
