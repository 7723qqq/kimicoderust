use super::promptOptimizer::{PROMPT_OPTIMIZER_FLAG_ENV, PROMPT_OPTIMIZER_FLAG_ID};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptOptimizerFlag {
    pub id: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub env: &'static str,
    pub default: bool,
    pub surface: &'static str,
}

pub const PROMPT_OPTIMIZER_FLAG: PromptOptimizerFlag = PromptOptimizerFlag {
    id: PROMPT_OPTIMIZER_FLAG_ID,
    title: "Prompt optimizer",
    description: "Let the input box rewrite a drafted prompt through the current model, reaching it from the TUI shortcut and the session API.",
    env: PROMPT_OPTIMIZER_FLAG_ENV,
    default: false,
    surface: "both",
};
