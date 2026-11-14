use std::sync::Arc;

use serde_json::{Map, Value};

use crate::human::llm::protocol::base::TraitContext;
use crate::human::llm::thinking::ThinkingRequestOptions;

#[derive(Debug, Clone)]
pub struct ThinkingContribution {
    pub kwargs: Map<String, Value>,
    pub preserve_thinking: Option<bool>,
}

pub type ThinkingStrategy = Arc<
    dyn Fn(&ThinkingRequestOptions, &TraitContext) -> Option<ThinkingContribution> + Send + Sync,
>;
pub type ThinkingFallback = Arc<
    dyn Fn(&ThinkingRequestOptions, &TraitContext) -> Option<Map<String, Value>> + Send + Sync,
>;

#[derive(Debug, Clone)]
pub struct AppliedThinking {
    pub kwargs: Map<String, Value>,
    pub preserve_thinking: bool,
}

pub fn apply_thinking(
    kwargs: Map<String, Value>,
    thinking: &ThinkingRequestOptions,
    strategy: Option<&ThinkingStrategy>,
    ctx: &TraitContext,
    fallback: Option<&ThinkingFallback>,
) -> AppliedThinking {
    let contribution = strategy.and_then(|strategy| strategy(thinking, ctx));
    let hooked_kwargs = match &contribution {
        Some(contribution) => Some(contribution.kwargs.clone()),
        None => fallback.and_then(|fallback| fallback(thinking, ctx)),
    };
    let mut kwargs = kwargs;
    if let Some(hooked) = hooked_kwargs {
        kwargs.extend(hooked);
    }
    AppliedThinking {
        kwargs,
        preserve_thinking: contribution
            .and_then(|contribution| contribution.preserve_thinking)
            .unwrap_or(false),
    }
}
