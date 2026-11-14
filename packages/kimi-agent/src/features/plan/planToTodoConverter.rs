use super::parsePlanToTodos::parse_plan_to_todos;
use super::plan::PlanData;
use crate::features::todo::todoItem::TodoItem;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanTodoSkipReason {
    EmptyPlan,
    NoStructure,
    ExistingTodos,
    NoAgent,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlanToTodoOutcome {
    Converted { count: usize },
    Skipped { reason: PlanTodoSkipReason },
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlanTodoConversion {
    pub outcome: PlanToTodoOutcome,
    pub items: Option<Vec<TodoItem>>,
}

pub fn try_convert_plan_to_todos(
    plan_data: Option<&PlanData>,
    current_todos: &[TodoItem],
    agent_present: bool,
) -> PlanTodoConversion {
    if !agent_present {
        return skipped(PlanTodoSkipReason::NoAgent);
    }
    let Some(plan_data) = plan_data else {
        return skipped(PlanTodoSkipReason::EmptyPlan);
    };
    if trim_js(&plan_data.content).is_empty() {
        return skipped(PlanTodoSkipReason::EmptyPlan);
    }
    let Some(items) = parse_plan_to_todos(&plan_data.content) else {
        return skipped(PlanTodoSkipReason::NoStructure);
    };
    if items.is_empty() {
        return skipped(PlanTodoSkipReason::NoStructure);
    }
    if !current_todos.is_empty() {
        return skipped(PlanTodoSkipReason::ExistingTodos);
    }
    PlanTodoConversion {
        outcome: PlanToTodoOutcome::Converted { count: items.len() },
        items: Some(items),
    }
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

fn skipped(reason: PlanTodoSkipReason) -> PlanTodoConversion {
    PlanTodoConversion {
        outcome: PlanToTodoOutcome::Skipped { reason },
        items: None,
    }
}

pub fn outcome_skipped_reason(outcome: &PlanToTodoOutcome) -> Option<PlanTodoSkipReason> {
    match outcome {
        PlanToTodoOutcome::Skipped { reason } => Some(*reason),
        PlanToTodoOutcome::Converted { .. } => None,
    }
}
