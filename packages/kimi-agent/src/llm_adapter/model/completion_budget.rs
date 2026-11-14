use crate::llm_adapter::contract::capability::ModelCapability;
use crate::llm_adapter::model::model_types::CompletionBudgetParams;

const MIN_FLOOR: u32 = 1;

pub fn resolve_completion_budget(
    max_output_size: Option<u32>,
    max_completion_tokens_cap: Option<u32>,
) -> Option<u32> {
    if let Some(cap) = max_completion_tokens_cap {
        return (cap > 0).then_some(cap);
    }
    max_output_size.filter(|size| *size > 0)
}

pub fn completion_budget_params(
    budget: Option<u32>,
    capability: Option<&ModelCapability>,
    used_context_tokens: Option<u32>,
) -> Option<CompletionBudgetParams> {
    Some(CompletionBudgetParams {
        max_completion_tokens: budget?.max(MIN_FLOOR),
        used_context_tokens,
        max_context_tokens: capability.map(|capability| capability.max_context_tokens),
    })
}
