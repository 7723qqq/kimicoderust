use std::collections::HashMap;
use std::error::Error;
use std::sync::Arc;

use crate::_base::errors::errors::{Error2, SharedError};
use crate::_base::utils::abort::AbortSignal;
use crate::_base::utils::retry::{retry_backoff_delay, sleep_for_retry};
use crate::agent::llm_requester::llm_requester::{
    AgentLLMRequestOverrides, AgentLLMRequestSource, IAgentLLMRequesterService, PromptCacheProbe,
};
use crate::llm_adapter::contract::errors::{
    APIEmptyResponseError, is_retryable_generate_error,
};
use crate::llm_adapter::contract::message::{Message, create_user_message};
use crate::llm_adapter::contract::request_trace::LLMRequestTrace;

use super::compaction_utils::{
    CompactionAttemptResult, CompactionTruncatedError, collect_summary,
    drop_oldest_message_and_leading_tool_results, shared_error,
    shrink_compaction_history_after_overflow, snip_large_tool_results,
};

pub const COMPLETION_BUDGET_ESTIMATE_MARGIN_RATIO: f64 = 0.02;
pub const MAX_COMPACTION_OVERFLOW_SHRINK_ATTEMPTS: usize = 3;

#[derive(Debug, Clone, Default)]
pub struct CompactionGenerationState {
    pub retry_count: u32,
}

pub struct CompactionGenerationDeps<'a> {
    pub state: &'a mut CompactionGenerationState,
    pub history: &'a [Message],
    pub instruction: &'a str,
    pub signal: AbortSignal,
    pub max_attempts: u32,
    pub compaction_max_output_size: Option<u32>,
    pub origin_turn_id: Option<u64>,
    pub request_tokens: Arc<dyn Fn(&[Message]) -> u64 + Send + Sync>,
    pub estimate_message: Arc<dyn Fn(&Message) -> u64 + Send + Sync>,
    pub get_effective_max_context_tokens: Arc<dyn Fn() -> u32 + Send + Sync>,
    pub llm_requester: Arc<dyn IAgentLLMRequesterService>,
    pub pre_shrink_history:
        Arc<dyn Fn(&[Message], &str, Option<u32>) -> Vec<Message> + Send + Sync>,
    pub should_recover_from_context_overflow:
        Arc<dyn Fn(&(dyn Error + Send + Sync), u64) -> bool + Send + Sync>,
    pub observe_context_overflow: Arc<dyn Fn(u64) + Send + Sync>,
    pub set_trace: Arc<dyn Fn(LLMRequestTrace) + Send + Sync>,
}

#[derive(Debug, Clone)]
pub struct CompactionGenerationOutcome {
    pub attempt: CompactionAttemptResult,
    pub dropped_count: usize,
}

pub async fn generate_compaction_summary(
    mut deps: CompactionGenerationDeps<'_>,
) -> Result<CompactionGenerationOutcome, SharedError> {
    let mut history_for_model = deps.history.to_vec();
    let mut dropped_count = 0usize;
    let mut overflow_shrink_count = 0usize;
    let mut request_attempts = 0u32;
    let mut cache_probe_pending = true;
    let pre_shrunk = (deps.pre_shrink_history)(
        &history_for_model,
        deps.instruction,
        deps.compaction_max_output_size,
    );
    dropped_count += history_for_model.len() - pre_shrunk.len();
    history_for_model = pre_shrunk;

    loop {
        let messages_to_compact = &history_for_model;
        let snipped = snip_large_tool_results(messages_to_compact, None);
        let mut messages = snipped;
        messages.push(create_user_message(deps.instruction));
        let estimated_compaction_request_tokens = (deps.request_tokens)(&messages);
        let effective_max_tokens = (deps.get_effective_max_context_tokens)();
        let used_context_tokens = (effective_max_tokens > 0).then(|| {
            estimated_compaction_request_tokens.saturating_add(
                (effective_max_tokens as f64 * COMPLETION_BUDGET_ESTIMATE_MARGIN_RATIO).ceil()
                    as u64,
            )
        });
        request_attempts += 1;

        let credential_provider = deps.llm_requester.current_credential_provider();
        let prompt_cache_probe = cache_probe_pending.then_some(PromptCacheProbe::Compaction);
        let mut run_request = || {
            let request = deps.llm_requester.start(
                Some(AgentLLMRequestOverrides {
                    messages: Some(messages.clone()),
                    max_output_size: deps.compaction_max_output_size,
                    used_context_tokens,
                    prompt_cache_probe,
                    source: Some(AgentLLMRequestSource::Operation {
                        turn_id: deps.origin_turn_id,
                        request_kind: Some("full_compaction".to_string()),
                        log_fields: Some(HashMap::from([(
                            "droppedCount".to_string(),
                            serde_json::json!(dropped_count),
                        )])),
                    }),
                    ..Default::default()
                }),
                None,
                Some(deps.signal.clone()),
            );
            (deps.set_trace)(request.trace.clone());
            request.result
        };
        let mut result = run_request().await;
        let recover = result.as_ref().err().is_some_and(|error| {
            !deps.signal.aborted()
                && credential_provider
                    .as_ref()
                    .is_some_and(|provider| provider.can_recover(error))
        });
        if recover {
            if let Some(provider) = &credential_provider {
                provider.invalidate();
            }
            result = run_request().await;
        }
        let result = match result {
            Ok(finish) => {
                cache_probe_pending = false;
                collect_summary(finish)
            }
            Err(error) => Err(shared_error(error)),
        };

        match result {
            Ok(attempt) => {
                return Ok(CompactionGenerationOutcome {
                    attempt,
                    dropped_count,
                });
            }
            Err(error) => {
                let is_context_overflow = (deps.should_recover_from_context_overflow)(
                    error.as_ref(),
                    estimated_compaction_request_tokens,
                );
                if is_context_overflow {
                    (deps.observe_context_overflow)(estimated_compaction_request_tokens);
                    overflow_shrink_count += 1;
                    if overflow_shrink_count > MAX_COMPACTION_OVERFLOW_SHRINK_ATTEMPTS
                        || request_attempts >= deps.max_attempts
                        || messages_to_compact.len() <= 1
                    {
                        return Err(error);
                    }
                    let before = messages_to_compact.len();
                    let estimate = |message: &Message| (deps.estimate_message)(message);
                    let shrunk = shrink_compaction_history_after_overflow(
                        messages_to_compact,
                        overflow_shrink_count,
                        Some(&estimate),
                    );
                    if shrunk.is_empty() {
                        return Err(error);
                    }
                    dropped_count += before - shrunk.len();
                    history_for_model = shrunk;
                    deps.state.retry_count = 0;
                    continue;
                }

                let unwrapped = unwrap_error_cause_ref(error.as_ref());
                let empty_response = unwrapped
                    .downcast_ref::<APIEmptyResponseError>()
                    .is_some_and(|empty| empty.finish_reason() != Some(crate::human::llm::finish_reason::FinishReason::Filtered));
                if (error.is::<CompactionTruncatedError>() || empty_response)
                    && messages_to_compact.len() > 1
                {
                    if request_attempts >= deps.max_attempts {
                        return Err(error);
                    }
                    let reduced = drop_oldest_message_and_leading_tool_results(messages_to_compact);
                    dropped_count += messages_to_compact.len() - reduced.len();
                    history_for_model = reduced;
                    deps.state.retry_count = 0;
                    continue;
                }
                if !is_retryable_generate_error(unwrapped) {
                    return Err(error);
                }
                if request_attempts >= deps.max_attempts {
                    return Err(error);
                }
                sleep_for_retry(
                    retry_backoff_delay(deps.state.retry_count),
                    Some(deps.signal.clone()),
                    None,
                )
                .await?;
                deps.state.retry_count += 1;
            }
        }
    }
}

fn unwrap_error_cause_ref<'a>(
    error: &'a (dyn Error + Send + Sync + 'static),
) -> &'a (dyn Error + Send + Sync + 'static) {
    if let Some(coded) = error.downcast_ref::<Error2>() {
        return match coded.cause.as_deref() {
            Some(cause) => unwrap_error_cause_ref(cause),
            None => error,
        };
    }
    if let Some(coded) = error.downcast_ref::<kimi_core::primitives::errors::Error2>() {
        return match coded.cause() {
            Some(cause) => unwrap_error_cause_ref(cause),
            None => error,
        };
    }
    error
}

