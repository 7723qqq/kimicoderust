use serde::{Deserialize, Serialize};

pub mod judge;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Active,
    Paused,
    Blocked,
    Complete,
    BudgetLimited,
    UsageLimited,
}

impl GoalStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Blocked => "blocked",
            Self::Complete => "complete",
            Self::BudgetLimited => "budget_limited",
            Self::UsageLimited => "usage_limited",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalBudgetLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_budget: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_clock_budget_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalBudgetReport {
    pub token_budget: Option<u64>,
    pub turn_budget: Option<u64>,
    pub wall_clock_budget_ms: Option<u64>,
    pub remaining_tokens: Option<u64>,
    pub remaining_turns: Option<u64>,
    pub remaining_wall_clock_ms: Option<u64>,
    pub token_budget_reached: bool,
    pub turn_budget_reached: bool,
    pub wall_clock_budget_reached: bool,
    pub over_budget: bool,
    pub input_tokens_used: u64,
    pub output_tokens_used: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalState {
    pub goal_id: String,
    pub objective: String,
    #[serde(default)]
    pub completion_criterion: Option<String>,
    pub status: GoalStatus,
    pub turns_used: u64,
    pub tokens_used: u64,
    pub input_tokens_used: u64,
    pub output_tokens_used: u64,
    pub wall_clock_ms: u64,
    #[serde(default)]
    pub wall_clock_resumed_at: Option<u64>,
    #[serde(default, alias = "budget")]
    pub budget_limits: GoalBudgetLimits,
    #[serde(default)]
    pub terminal_reason: Option<String>,
    #[serde(default)]
    pub host_owned: bool,
    pub created_at: u64,
    pub updated_at: u64,
}

pub const MAX_GOAL_OBJECTIVE_CHARS: usize = 4000;

impl GoalState {
    pub fn edit_objective(&mut self, objective: &str) -> Result<bool, String> {
        if self.status == GoalStatus::Complete {
            return Err(format!(
                "Cannot edit a goal in status \"{}\"",
                self.status.as_str()
            ));
        }
        let objective = objective.trim();
        if objective.is_empty() {
            return Err("Goal objective cannot be empty".into());
        }
        if objective.chars().count() > MAX_GOAL_OBJECTIVE_CHARS {
            return Err(format!(
                "Goal objective cannot exceed {MAX_GOAL_OBJECTIVE_CHARS} characters. \
                 Put long content in a file and reference the file path."
            ));
        }
        if objective == self.objective {
            return Ok(false);
        }
        self.objective = objective.to_string();
        Ok(true)
    }

    pub fn live_wall_clock_ms(&self, now_ms: u64) -> u64 {
        if self.status == GoalStatus::Active
            && let Some(resumed_at) = self.wall_clock_resumed_at
        {
            return self.wall_clock_ms + now_ms.saturating_sub(resumed_at);
        }
        self.wall_clock_ms
    }

    pub fn snapshot_now(&self) -> GoalSnapshot {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or(0);
        to_snapshot(self, self.live_wall_clock_ms(now))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalSnapshot {
    pub goal_id: String,
    pub objective: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_criterion: Option<String>,
    pub status: GoalStatus,
    pub turns_used: u64,
    pub tokens_used: u64,
    pub input_tokens_used: u64,
    pub output_tokens_used: u64,
    pub wall_clock_ms: u64,
    pub budget: GoalBudgetReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_reason: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

pub fn to_snapshot(state: &GoalState, wall_clock_ms: u64) -> GoalSnapshot {
    GoalSnapshot {
        goal_id: state.goal_id.clone(),
        objective: state.objective.clone(),
        completion_criterion: state.completion_criterion.clone(),
        status: state.status,
        turns_used: state.turns_used,
        tokens_used: state.tokens_used,
        input_tokens_used: state.input_tokens_used,
        output_tokens_used: state.output_tokens_used,
        wall_clock_ms,
        budget: compute_budget_report(state, wall_clock_ms),
        terminal_reason: state.terminal_reason.clone(),
        created_at: state.created_at,
        updated_at: state.updated_at,
    }
}

pub fn compute_budget_report(state: &GoalState, wall_clock_ms: u64) -> GoalBudgetReport {
    let token_budget = state.budget_limits.token_budget;
    let turn_budget = state.budget_limits.turn_budget;
    let wall_clock_budget_ms = state.budget_limits.wall_clock_budget_ms;

    let token_budget_reached = token_budget.is_some_and(|b| state.tokens_used >= b);
    let turn_budget_reached = turn_budget.is_some_and(|b| state.turns_used >= b);
    let wall_clock_budget_reached = wall_clock_budget_ms.is_some_and(|b| wall_clock_ms >= b);

    GoalBudgetReport {
        token_budget,
        turn_budget,
        wall_clock_budget_ms,
        remaining_tokens: token_budget.map(|b| b.saturating_sub(state.tokens_used)),
        remaining_turns: turn_budget.map(|b| b.saturating_sub(state.turns_used)),
        remaining_wall_clock_ms: wall_clock_budget_ms.map(|b| b.saturating_sub(wall_clock_ms)),
        token_budget_reached,
        turn_budget_reached,
        wall_clock_budget_reached,
        over_budget: token_budget_reached || turn_budget_reached || wall_clock_budget_reached,
        input_tokens_used: state.input_tokens_used,
        output_tokens_used: state.output_tokens_used,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalForModel {
    pub objective: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_criterion: Option<String>,
    pub status: GoalStatus,
    pub turns_used: u64,
    pub tokens_used: u64,
    pub input_tokens_used: u64,
    pub output_tokens_used: u64,
    pub wall_clock_ms: u64,
    pub budget: GoalBudgetReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_reason: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GoalResultForModel {
    pub goal: Option<GoalForModel>,
}

pub fn goal_for_model(goal: &GoalSnapshot) -> GoalForModel {
    GoalForModel {
        objective: goal.objective.clone(),
        completion_criterion: goal.completion_criterion.clone(),
        status: goal.status,
        turns_used: goal.turns_used,
        tokens_used: goal.tokens_used,
        input_tokens_used: goal.input_tokens_used,
        output_tokens_used: goal.output_tokens_used,
        wall_clock_ms: goal.wall_clock_ms,
        budget: goal.budget.clone(),
        terminal_reason: goal.terminal_reason.clone(),
        created_at: goal.created_at,
        updated_at: goal.updated_at,
    }
}

pub fn goal_result_for_model(goal: Option<&GoalSnapshot>) -> GoalResultForModel {
    GoalResultForModel {
        goal: goal.map(goal_for_model),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetUnit {
    Turns,
    Tokens,
    Milliseconds,
    Seconds,
    Minutes,
    Hours,
}

impl BudgetUnit {
    pub fn parse_unit(s: &str) -> Option<Self> {
        match s {
            "turns" => Some(Self::Turns),
            "tokens" => Some(Self::Tokens),
            "milliseconds" => Some(Self::Milliseconds),
            "seconds" => Some(Self::Seconds),
            "minutes" => Some(Self::Minutes),
            "hours" => Some(Self::Hours),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Turns => "turns",
            Self::Tokens => "tokens",
            Self::Milliseconds => "milliseconds",
            Self::Seconds => "seconds",
            Self::Minutes => "minutes",
            Self::Hours => "hours",
        }
    }
}

pub const MIN_REASONABLE_TIME_BUDGET_MS: u64 = 1_000;

pub fn normalize_budget_input(value: f64, unit: BudgetUnit) -> f64 {
    match unit {
        BudgetUnit::Turns | BudgetUnit::Tokens => value.round().max(1.0),
        BudgetUnit::Milliseconds
        | BudgetUnit::Seconds
        | BudgetUnit::Minutes
        | BudgetUnit::Hours => value,
    }
}

pub fn to_milliseconds(value: f64, unit: BudgetUnit) -> Option<f64> {
    match unit {
        BudgetUnit::Milliseconds => Some(value),
        BudgetUnit::Seconds => Some(value * 1000.0),
        BudgetUnit::Minutes => Some(value * 60.0 * 1000.0),
        BudgetUnit::Hours => Some(value * 60.0 * 60.0 * 1000.0),
        BudgetUnit::Turns | BudgetUnit::Tokens => None,
    }
}

pub fn budget_limits_from_input(value: f64, unit: BudgetUnit) -> Option<GoalBudgetLimits> {
    match unit {
        BudgetUnit::Turns => Some(GoalBudgetLimits {
            turn_budget: Some(value as u64),
            ..GoalBudgetLimits::default()
        }),
        BudgetUnit::Tokens => Some(GoalBudgetLimits {
            token_budget: Some(value as u64),
            ..GoalBudgetLimits::default()
        }),
        BudgetUnit::Milliseconds
        | BudgetUnit::Seconds
        | BudgetUnit::Minutes
        | BudgetUnit::Hours => {
            let wall_clock_budget_ms = to_milliseconds(value, unit)?.round();
            if !wall_clock_budget_ms.is_finite()
                || wall_clock_budget_ms < MIN_REASONABLE_TIME_BUDGET_MS as f64
            {
                return None;
            }
            Some(GoalBudgetLimits {
                wall_clock_budget_ms: Some(wall_clock_budget_ms as u64),
                ..GoalBudgetLimits::default()
            })
        }
    }
}

pub fn format_budget(value: f64, unit: BudgetUnit) -> String {
    let unit_str = unit.as_str();
    let singular = unit_str.strip_suffix('s').unwrap_or(unit_str);
    if value == 1.0 {
        format!("{value} {singular}")
    } else {
        format!("{value} {unit_str}")
    }
}

pub fn format_elapsed(ms: u64) -> String {
    let total_seconds = (ms as f64 / 1000.0).round() as u64;
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

pub fn format_budgets(goal: &GoalSnapshot) -> String {
    let mut lines: Vec<String> = Vec::new();
    if let Some(turn_budget) = goal.budget.turn_budget {
        lines.push(format!(
            "turns {}/{} (remaining {})",
            goal.turns_used,
            turn_budget,
            goal.budget.remaining_turns.unwrap_or(0)
        ));
    }
    if let Some(token_budget) = goal.budget.token_budget {
        lines.push(format!(
            "tokens {}/{} (remaining {})",
            goal.tokens_used,
            token_budget,
            goal.budget.remaining_tokens.unwrap_or(0)
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

pub fn max_budget_fraction(goal: &GoalSnapshot) -> f64 {
    let mut fractions: Vec<f64> = Vec::new();
    if let Some(budget) = goal.budget.turn_budget.filter(|b| *b > 0) {
        fractions.push(goal.turns_used as f64 / budget as f64);
    }
    if let Some(budget) = goal.budget.token_budget.filter(|b| *b > 0) {
        fractions.push(goal.tokens_used as f64 / budget as f64);
    }
    if let Some(budget) = goal.budget.wall_clock_budget_ms.filter(|b| *b > 0) {
        fractions.push(goal.wall_clock_ms as f64 / budget as f64);
    }
    fractions.into_iter().fold(0.0, f64::max)
}

pub fn is_nearing_budget(goal: &GoalSnapshot) -> bool {
    max_budget_fraction(goal) >= 0.75
}

pub const GOAL_BUDGET_BLOCK_PREFIX: &str = "Blocked after goal budget reached";

pub const GOAL_RATE_LIMIT_PAUSE_REASON: &str = "Paused after provider rate limit";
pub const GOAL_PROVIDER_CONNECTION_PAUSE_PREFIX: &str = "Paused after provider connection error";
pub const GOAL_PROVIDER_AUTH_PAUSE_PREFIX: &str = "Paused after provider authentication error";
pub const GOAL_PROVIDER_API_PAUSE_PREFIX: &str = "Paused after provider API error";
pub const GOAL_MODEL_CONFIG_PAUSE_PREFIX: &str = "Paused after model configuration error";
pub const GOAL_RUNTIME_PAUSE_PREFIX: &str = "Paused after runtime error";
pub const GOAL_CONTINUATION_FAILURE_PAUSE_PREFIX: &str = "Paused after goal continuation failure";
pub const GOAL_PROVIDER_FILTERED_PAUSE_REASON: &str = "Paused after provider safety policy block";
pub const GOAL_INTERRUPTED_PAUSE_REASON: &str = "Paused after interruption";

pub const GOAL_CONTINUATION_PROMPT: &str = "Continue working toward the active goal. Keep the self-audit brief. Do not explore unrelated interpretations once the goal can be decided. If the objective is simple, already answered, impossible, unsafe, or contradictory, do not run another goal turn. Explain briefly if useful, then call UpdateGoal with `complete` or `blocked` in the same turn. Otherwise, weigh the objective and any completion criteria against the work done so far, choose one bounded, useful slice of work, and use the existing conversation context and your tools. Do not try to finish a broad goal in one turn unless the whole goal is genuinely small. Most goal turns should not call UpdateGoal: after completing a useful slice, if material work remains, end the turn normally without calling UpdateGoal so the runtime can continue the goal in the next turn. Call UpdateGoal with `complete` only when all required work is done, any stated validation has passed, and there is no useful next action. Completion audit: before calling `complete`, verify the current state against the actual objective and every explicit requirement. Treat weak or indirect evidence as not complete. Do not mark complete after only producing a plan, summary, first pass, or partial result. Do not mark complete merely because a budget is nearly exhausted or you want to stop. Blocked audit: do not call UpdateGoal with `blocked` the first time you hit a blocker. Use `blocked` only for a genuine impasse: an external condition, required user input, missing credentials or permissions, or a persistent technical failure. For those non-terminal blockers, the same blocking condition must repeat for at least 3 consecutive goal turns before you call `blocked`, counting the original/user-triggered turn and automatic continuations. If a previously blocked goal is resumed, treat the resumed run as a fresh blocked audit. Exception: if the objective itself is impossible, unsafe, or contradictory, call UpdateGoal with `blocked` in the same turn; do not run more goal turns just to satisfy the audit. Do not use `blocked` because the work is large, hard, slow, uncertain, incomplete, still needs validation, would benefit from clarification, or needs more goal turns. Once the 3-turn threshold is met and you cannot make meaningful progress without user input or an external-state change, call UpdateGoal with `blocked`; do not keep reporting the blocker while leaving the goal active. Do not ask the user for input unless a real blocker prevents progress.";

pub const GOAL_STEP_CAP_CONTINUATION_PREFIX: &str = "The previous goal turn reached the per-turn step limit before finishing its work, so a new turn was started for you. Pick up where that turn stopped and keep each slice of work small enough to fit the limit. ";

pub fn continuation_prompt(step_capped: bool) -> String {
    if step_capped {
        format!("{GOAL_STEP_CAP_CONTINUATION_PREFIX}{GOAL_CONTINUATION_PROMPT}")
    } else {
        GOAL_CONTINUATION_PROMPT.to_string()
    }
}

pub fn goal_budget_block_reason(budget: &GoalBudgetReport) -> Option<String> {
    let mut reached: Vec<String> = Vec::new();
    if budget.turn_budget_reached
        && let Some(turn_budget) = budget.turn_budget
    {
        reached.push(format!("turn budget {turn_budget}"));
    }
    if budget.token_budget_reached
        && let Some(token_budget) = budget.token_budget
    {
        reached.push(format!("token budget {token_budget}"));
    }
    if budget.wall_clock_budget_reached
        && let Some(wall_clock_budget_ms) = budget.wall_clock_budget_ms
    {
        reached.push(format!("wall-clock budget {wall_clock_budget_ms}ms"));
    }
    if reached.is_empty() {
        return None;
    }
    Some(format!(
        "{GOAL_BUDGET_BLOCK_PREFIX}: {}",
        reached.join(", ")
    ))
}

pub fn goal_failure_pause_reason(code: &str, message: Option<&str>) -> String {
    match code {
        "provider.rate_limit" => GOAL_RATE_LIMIT_PAUSE_REASON.to_string(),
        "provider.connection_error" => {
            pause_reason_with_message(GOAL_PROVIDER_CONNECTION_PAUSE_PREFIX, message)
        }
        "provider.auth_error" => {
            pause_reason_with_message(GOAL_PROVIDER_AUTH_PAUSE_PREFIX, message)
        }
        "provider.filtered" => GOAL_PROVIDER_FILTERED_PAUSE_REASON.to_string(),
        "provider.api_error" | "provider.not_found" | "provider.overloaded" => {
            pause_reason_with_message(GOAL_PROVIDER_API_PAUSE_PREFIX, message)
        }
        "model.not_configured" => pause_reason_with_message(
            GOAL_MODEL_CONFIG_PAUSE_PREFIX,
            Some("LLM not set, send \"/login\" to login"),
        ),
        "model.config_invalid" => {
            pause_reason_with_message(GOAL_MODEL_CONFIG_PAUSE_PREFIX, message)
        }
        _ => pause_reason_with_message(GOAL_RUNTIME_PAUSE_PREFIX, message),
    }
}

pub fn pause_reason_with_message(prefix: &str, message: Option<&str>) -> String {
    match message.map(str::trim).filter(|message| !message.is_empty()) {
        Some(message) => format!("{prefix}: {message}"),
        None => prefix.to_string(),
    }
}

pub fn escape_untrusted_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn reason_suffix(goal: &GoalSnapshot) -> String {
    match goal.terminal_reason.as_deref() {
        Some(reason) => format!(" ({})", escape_untrusted_text(reason)),
        None => String::new(),
    }
}

pub fn completion_criterion_block(goal: &GoalSnapshot) -> String {
    match goal.completion_criterion.as_deref() {
        Some(criterion) => format!(
            "<untrusted_completion_criterion>\n{}\n</untrusted_completion_criterion>\n",
            escape_untrusted_text(criterion)
        ),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_state() -> GoalState {
        GoalState {
            goal_id: "goal-1".into(),
            objective: "Ship the feature".into(),
            completion_criterion: Some("Tests pass".into()),
            status: GoalStatus::Active,
            turns_used: 3,
            tokens_used: 100,
            input_tokens_used: 60,
            output_tokens_used: 40,
            wall_clock_ms: 42_000,
            wall_clock_resumed_at: None,
            budget_limits: GoalBudgetLimits {
                token_budget: Some(500),
                turn_budget: Some(10),
                wall_clock_budget_ms: Some(3_600_000),
            },
            host_owned: false,
            terminal_reason: None,
            created_at: 1_700_000_000_000,
            updated_at: 1_700_000_042_000,
        }
    }

    #[test]
    fn test_normalize_budget_input() {
        assert_eq!(normalize_budget_input(2.6, BudgetUnit::Turns), 3.0);
        assert_eq!(normalize_budget_input(0.4, BudgetUnit::Turns), 1.0);
        assert_eq!(normalize_budget_input(-5.0, BudgetUnit::Tokens), 1.0);
        assert_eq!(normalize_budget_input(2.5, BudgetUnit::Milliseconds), 2.5);
        assert_eq!(normalize_budget_input(1.5, BudgetUnit::Hours), 1.5);
    }

    #[test]
    fn test_to_milliseconds() {
        assert_eq!(
            to_milliseconds(1_000.0, BudgetUnit::Milliseconds),
            Some(1_000.0)
        );
        assert_eq!(to_milliseconds(2.0, BudgetUnit::Seconds), Some(2_000.0));
        assert_eq!(to_milliseconds(1.5, BudgetUnit::Minutes), Some(90_000.0));
        assert_eq!(to_milliseconds(2.0, BudgetUnit::Hours), Some(7_200_000.0));
        assert_eq!(to_milliseconds(1.0, BudgetUnit::Turns), None);
        assert_eq!(to_milliseconds(1.0, BudgetUnit::Tokens), None);
    }

    #[test]
    fn test_budget_limits_from_input() {
        let turns = budget_limits_from_input(5.0, BudgetUnit::Turns).unwrap();
        assert_eq!(turns.turn_budget, Some(5));
        assert_eq!(turns.token_budget, None);
        assert_eq!(turns.wall_clock_budget_ms, None);

        let tokens = budget_limits_from_input(500.0, BudgetUnit::Tokens).unwrap();
        assert_eq!(tokens.token_budget, Some(500));
        assert_eq!(tokens.turn_budget, None);

        let seconds = budget_limits_from_input(30.0, BudgetUnit::Seconds).unwrap();
        assert_eq!(seconds.wall_clock_budget_ms, Some(30_000));
        assert_eq!(seconds.token_budget, None);

        let half_hour = budget_limits_from_input(0.5, BudgetUnit::Hours).unwrap();
        assert_eq!(half_hour.wall_clock_budget_ms, Some(1_800_000));

        assert_eq!(budget_limits_from_input(0.9, BudgetUnit::Seconds), None);
        assert_eq!(
            budget_limits_from_input(1.0, BudgetUnit::Seconds)
                .unwrap()
                .wall_clock_budget_ms,
            Some(1_000)
        );
        assert_eq!(
            budget_limits_from_input(24.0, BudgetUnit::Hours)
                .unwrap()
                .wall_clock_budget_ms,
            Some(86_400_000)
        );
        assert_eq!(
            budget_limits_from_input(25.0, BudgetUnit::Hours)
                .unwrap()
                .wall_clock_budget_ms,
            Some(90_000_000)
        );
        assert_eq!(
            budget_limits_from_input(1000.0, BudgetUnit::Hours)
                .unwrap()
                .wall_clock_budget_ms,
            Some(3_600_000_000)
        );
        assert_eq!(
            budget_limits_from_input(f64::INFINITY, BudgetUnit::Hours),
            None
        );
    }

    #[test]
    fn test_format_budget() {
        assert_eq!(format_budget(1.0, BudgetUnit::Turns), "1 turn");
        assert_eq!(format_budget(2.0, BudgetUnit::Turns), "2 turns");
        assert_eq!(format_budget(1.0, BudgetUnit::Hours), "1 hour");
        assert_eq!(format_budget(2.0, BudgetUnit::Hours), "2 hours");
        assert_eq!(format_budget(1.5, BudgetUnit::Hours), "1.5 hours");
        assert_eq!(format_budget(1.0, BudgetUnit::Tokens), "1 token");
        assert_eq!(
            format_budget(1.0, BudgetUnit::Milliseconds),
            "1 millisecond"
        );
    }

    #[test]
    fn test_goal_state_deserializes_wire_shape() {
        let raw = json!({
            "goalId": "goal-1",
            "objective": "Ship the feature",
            "completionCriterion": "Tests pass",
            "status": "active",
            "turnsUsed": 3,
            "tokensUsed": 100,
            "inputTokensUsed": 60,
            "outputTokensUsed": 40,
            "wallClockMs": 42000,
            "budgetLimits": { "tokenBudget": 500, "turnBudget": 10, "wallClockBudgetMs": 3600000 },
            "createdAt": 1700000000000u64,
            "updatedAt": 1700000042000u64
        });
        let state: GoalState = serde_json::from_value(raw).unwrap();
        assert_eq!(state.goal_id, "goal-1");
        assert_eq!(state.status, GoalStatus::Active);
        assert_eq!(state.budget_limits.token_budget, Some(500));
        assert_eq!(state.budget_limits.turn_budget, Some(10));
        assert_eq!(state.budget_limits.wall_clock_budget_ms, Some(3_600_000));
        assert_eq!(state.wall_clock_resumed_at, None);
        assert_eq!(state.terminal_reason, None);
    }

    #[test]
    fn test_goal_state_minimal_defaults() {
        let raw = json!({
            "goalId": "goal-2",
            "objective": "Minimal",
            "status": "blocked",
            "turnsUsed": 0,
            "tokensUsed": 0,
            "inputTokensUsed": 0,
            "outputTokensUsed": 0,
            "wallClockMs": 0,
            "createdAt": 0,
            "updatedAt": 0
        });
        let state: GoalState = serde_json::from_value(raw).unwrap();
        assert_eq!(state.completion_criterion, None);
        assert_eq!(state.budget_limits, GoalBudgetLimits::default());
        assert_eq!(state.status, GoalStatus::Blocked);
    }

    #[test]
    fn edit_objective_validates_status_and_length() {
        let mut state: GoalState = serde_json::from_value(json!({
            "goalId": "goal-edit",
            "objective": "old objective",
            "status": "active",
            "turnsUsed": 0,
            "tokensUsed": 0,
            "inputTokensUsed": 0,
            "outputTokensUsed": 0,
            "wallClockMs": 0,
            "createdAt": 1,
            "updatedAt": 1
        }))
        .unwrap();
        assert!(state.edit_objective("  new objective  ").unwrap());
        assert_eq!(state.objective, "new objective");
        assert!(!state.edit_objective("new objective").unwrap());

        assert!(state.edit_objective("   ").is_err(), "empty is refused");
        assert!(
            state.edit_objective(&"x".repeat(4001)).is_err(),
            "over 4000 characters is refused"
        );
        state.status = GoalStatus::Complete;
        assert!(
            state.edit_objective("another").is_err(),
            "a completed goal is terminal"
        );
    }

    #[test]
    fn test_goal_state_round_trip() {
        let state = sample_state();
        let wire = serde_json::to_value(&state).unwrap();
        let back: GoalState = serde_json::from_value(wire).unwrap();
        assert_eq!(back, state);
    }

    #[test]
    fn test_budget_limits_wire_shape() {
        let limits = GoalBudgetLimits {
            token_budget: Some(500),
            ..GoalBudgetLimits::default()
        };
        let value = serde_json::to_value(limits).unwrap();
        assert_eq!(value, json!({ "tokenBudget": 500 }));
        assert!(value.get("turnBudget").is_none());
        assert!(value.get("wallClockBudgetMs").is_none());
    }

    #[test]
    fn test_snapshot_serializes_wire_shape() {
        let snapshot = to_snapshot(&sample_state(), 42_000);
        let wire = serde_json::to_string(&snapshot).unwrap();
        assert_eq!(
            wire,
            r#"{"goalId":"goal-1","objective":"Ship the feature","completionCriterion":"Tests pass","status":"active","turnsUsed":3,"tokensUsed":100,"inputTokensUsed":60,"outputTokensUsed":40,"wallClockMs":42000,"budget":{"tokenBudget":500,"turnBudget":10,"wallClockBudgetMs":3600000,"remainingTokens":400,"remainingTurns":7,"remainingWallClockMs":3558000,"tokenBudgetReached":false,"turnBudgetReached":false,"wallClockBudgetReached":false,"overBudget":false,"inputTokensUsed":60,"outputTokensUsed":40},"createdAt":1700000000000,"updatedAt":1700000042000}"#
        );
    }

    #[test]
    fn test_snapshot_omits_absent_optionals() {
        let mut state = sample_state();
        state.completion_criterion = None;
        state.terminal_reason = Some("Blocked after goal budget reached".into());
        let snapshot = to_snapshot(&state, 42_000);
        let wire = serde_json::to_string(&snapshot).unwrap();
        assert!(!wire.contains("completionCriterion"));
        assert!(wire.contains(r#""terminalReason":"Blocked after goal budget reached""#));
        assert!(!wire.contains("blockedStreak"));
    }

    #[test]
    fn test_budget_report_over_budget_and_saturating() {
        let mut state = sample_state();
        state.tokens_used = 600;
        state.turns_used = 10;
        let report = compute_budget_report(&state, 3_700_000);
        assert!(report.token_budget_reached);
        assert!(report.turn_budget_reached);
        assert!(report.wall_clock_budget_reached);
        assert!(report.over_budget);
        assert_eq!(report.remaining_tokens, Some(0));
        assert_eq!(report.remaining_turns, Some(0));
        assert_eq!(report.remaining_wall_clock_ms, Some(0));
    }

    #[test]
    fn test_budget_report_without_limits() {
        let mut state = sample_state();
        state.budget_limits = GoalBudgetLimits::default();
        let report = compute_budget_report(&state, 42_000);
        assert_eq!(report.token_budget, None);
        assert_eq!(report.turn_budget, None);
        assert_eq!(report.wall_clock_budget_ms, None);
        assert_eq!(report.remaining_tokens, None);
        assert!(!report.over_budget);
    }

    #[test]
    fn test_goal_for_model_strips_goal_id() {
        let snapshot = to_snapshot(&sample_state(), 42_000);
        let for_model = goal_for_model(&snapshot);
        let value = serde_json::to_value(&for_model).unwrap();
        assert!(value.get("goalId").is_none());
        assert_eq!(value["objective"], "Ship the feature");
        assert_eq!(value["status"], "active");
        assert_eq!(value["budget"]["tokenBudget"], 500);
    }

    #[test]
    fn test_goal_result_for_model() {
        let snapshot = to_snapshot(&sample_state(), 42_000);
        let result = goal_result_for_model(Some(&snapshot));
        let value = serde_json::to_value(&result).unwrap();
        assert!(value["goal"].get("goalId").is_none());
        assert_eq!(value["goal"]["objective"], "Ship the feature");

        let none = goal_result_for_model(None);
        let value = serde_json::to_value(&none).unwrap();
        assert_eq!(value, json!({ "goal": null }));
    }

    #[test]
    fn test_format_elapsed() {
        assert_eq!(format_elapsed(0), "0s");
        assert_eq!(format_elapsed(42_000), "42s");
        assert_eq!(format_elapsed(59_999), "1m00s");
        assert_eq!(format_elapsed(185_000), "3m05s");
        assert_eq!(format_elapsed(3_600_000), "1h00m");
        assert_eq!(format_elapsed(3_723_000), "1h02m");
    }

    #[test]
    fn test_format_budgets() {
        let snapshot = to_snapshot(&sample_state(), 42_000);
        assert_eq!(
            format_budgets(&snapshot),
            "turns 3/10 (remaining 7); tokens 100/500 (remaining 400); time 42s/1h00m (remaining 59m18s)"
        );
        let mut no_budgets = sample_state();
        no_budgets.budget_limits = GoalBudgetLimits::default();
        assert_eq!(format_budgets(&to_snapshot(&no_budgets, 0)), "");
    }

    #[test]
    fn test_max_budget_fraction_and_nearing() {
        let state = sample_state();
        assert!(!is_nearing_budget(&to_snapshot(&state, 42_000)));

        let mut state = sample_state();
        state.turns_used = 8;
        assert!(is_nearing_budget(&to_snapshot(&state, 42_000)));

        let mut state = sample_state();
        state.tokens_used = 375;
        let snapshot = to_snapshot(&state, 42_000);
        assert!((max_budget_fraction(&snapshot) - 0.75).abs() < 1e-9);
        assert!(is_nearing_budget(&snapshot));

        let mut state = sample_state();
        state.budget_limits = GoalBudgetLimits::default();
        assert_eq!(max_budget_fraction(&to_snapshot(&state, 42_000)), 0.0);
        assert!(!is_nearing_budget(&to_snapshot(&state, 42_000)));
    }

    #[test]
    fn test_escape_and_reminder_fragments() {
        assert_eq!(escape_untrusted_text("<a & b>"), "&lt;a &amp; b&gt;");

        let snapshot = to_snapshot(&sample_state(), 42_000);
        assert_eq!(
            completion_criterion_block(&snapshot),
            "<untrusted_completion_criterion>\nTests pass\n</untrusted_completion_criterion>\n"
        );
        assert_eq!(reason_suffix(&snapshot), "");

        let mut state = sample_state();
        state.terminal_reason = Some("Blocked after goal budget reached".into());
        let snapshot = to_snapshot(&state, 42_000);
        assert_eq!(
            reason_suffix(&snapshot),
            " (Blocked after goal budget reached)"
        );

        let mut state = sample_state();
        state.completion_criterion = None;
        assert_eq!(completion_criterion_block(&to_snapshot(&state, 42_000)), "");
    }

    #[test]
    fn test_goal_budget_block_reason_matches_v2() {
        let mut state = sample_state();
        assert_eq!(
            goal_budget_block_reason(&compute_budget_report(&state, 42_000)),
            None
        );

        state.turns_used = 10;
        assert_eq!(
            goal_budget_block_reason(&compute_budget_report(&state, 42_000)).as_deref(),
            Some("Blocked after goal budget reached: turn budget 10")
        );

        state.tokens_used = 500;
        state.wall_clock_ms = 3_600_000;
        assert_eq!(
            goal_budget_block_reason(&compute_budget_report(&state, 3_600_000)).as_deref(),
            Some(
                "Blocked after goal budget reached: turn budget 10, token budget 500, wall-clock budget 3600000ms"
            )
        );
    }

    #[test]
    fn test_continuation_prompts_match_v2() {
        assert_eq!(continuation_prompt(false), GOAL_CONTINUATION_PROMPT);
        assert_eq!(
            continuation_prompt(true),
            format!("{GOAL_STEP_CAP_CONTINUATION_PREFIX}{GOAL_CONTINUATION_PROMPT}")
        );
        assert!(
            continuation_prompt(false).starts_with("Continue working toward the active goal."),
            "the v2 continuation prompt is fixed text, not the old objective template"
        );
    }

    #[test]
    fn test_goal_failure_pause_reason_classifies_codes() {
        assert_eq!(
            goal_failure_pause_reason("provider.rate_limit", Some("slow down")),
            "Paused after provider rate limit"
        );
        assert_eq!(
            goal_failure_pause_reason("provider.connection_error", Some("connect refused")),
            "Paused after provider connection error: connect refused"
        );
        assert_eq!(
            goal_failure_pause_reason("provider.auth_error", None),
            "Paused after provider authentication error"
        );
        assert_eq!(
            goal_failure_pause_reason("provider.filtered", Some("ignored")),
            "Paused after provider safety policy block"
        );
        assert_eq!(
            goal_failure_pause_reason("provider.overloaded", Some(" 503  ")),
            "Paused after provider API error: 503"
        );
        assert_eq!(
            goal_failure_pause_reason("model.not_configured", None),
            "Paused after model configuration error: LLM not set, send \"/login\" to login"
        );
        assert_eq!(
            goal_failure_pause_reason("internal", None),
            "Paused after runtime error"
        );
        assert_eq!(
            goal_failure_pause_reason("internal", Some("boom")),
            "Paused after runtime error: boom"
        );
    }
}
