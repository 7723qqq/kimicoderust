use super::super::types::GoalSnapshot;

pub fn build_goal_completion_summary_prompt(goal: &GoalSnapshot) -> String {
    format!(
        "{}\n\nWrite a concise final message for the user. State that the goal is complete, summarize the main work completed, and mention any validation you ran. Do not call more goal tools.",
        completion_message(goal)
    )
}

pub fn build_goal_blocked_reason_prompt(goal: &GoalSnapshot) -> String {
    format!(
        "{}\n\nWrite a concise final message for the user. State that the goal is blocked, explain the concrete blocker, and say what input or change is needed before work can continue. Do not call more goal tools.",
        blocked_message(goal)
    )
}

fn completion_message(goal: &GoalSnapshot) -> String {
    let head = match &goal.terminal_reason {
        Some(reason) if !reason.is_empty() => format!("Goal completed successfully: {reason}."),
        _ => "Goal completed successfully.".to_string(),
    };
    format!("{head}\n{}", stats(goal))
}

fn blocked_message(goal: &GoalSnapshot) -> String {
    format!("Goal blocked.\n{}", stats(goal))
}

fn stats(goal: &GoalSnapshot) -> String {
    let turns = if goal.turns_used == 1 { "turn" } else { "turns" };
    format!(
        "Worked {} {turns} over {}, using {} tokens.",
        goal.turns_used,
        format_elapsed(goal.wall_clock_ms),
        format_tokens(goal.tokens_used)
    )
}

fn format_elapsed(ms: i64) -> String {
    let total_seconds = ((ms as f64) / 1000.0).round() as i64;
    if total_seconds < 60 {
        return format!("{total_seconds}s");
    }
    let minutes = (total_seconds as f64 / 60.0).floor() as i64;
    let seconds = total_seconds % 60;
    if minutes < 60 {
        return format!("{minutes}m{seconds:02}s");
    }
    format!("{}h{:02}m", (minutes as f64 / 60.0).floor() as i64, minutes % 60)
}

fn format_tokens(tokens: u64) -> String {
    if tokens < 1000 {
        tokens.to_string()
    } else if tokens < 1_000_000 {
        js_to_fixed(tokens, 1_000, "k")
    } else {
        js_to_fixed(tokens, 1_000_000, "M")
    }
}

fn js_to_fixed(tokens: u64, divisor: u64, suffix: &str) -> String {
    let value = rquickjs::Runtime::new()
        .and_then(|runtime| {
            rquickjs::Context::full(&runtime).and_then(|context| {
                context.with(|ctx| {
                    ctx.eval::<String, _>(format!("({tokens} / {divisor}).toFixed(1)"))
                })
            })
        })
        .unwrap_or_else(|_| format!("{:.1}", tokens as f64 / divisor as f64));
    format!("{value}{suffix}")
}
