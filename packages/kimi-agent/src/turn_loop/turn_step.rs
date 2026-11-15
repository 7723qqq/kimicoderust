use std::time::Duration;

use super::retry::RetryConfig;
use super::retry::retry_delay;
use super::types::*;
use crate::rpc::types::BoxFuture;

fn boxed_err(s: String) -> Box<dyn std::error::Error + Send + Sync> {
    Box::new(std::io::Error::other(s))
}

fn llm_request_record(
    agent_id: &str,
    turn_id: &str,
    step: u32,
    llm: &dyn LLM,
    params: &LLMChatParams,
    tool_select: bool,
) -> serde_json::Value {
    crate::session::agent_records::llm_request_record(
        llm,
        params,
        crate::session::agent_records::LlmRequestSpec {
            agent_id,
            kind: "loop",
            turn_step: Some(format!("{turn_id}.{}", step + 1)),
            tool_select,
            dropped_count: None,
        },
    )
}

struct RetryDecision {
    error_name: &'static str,

    error_message: String,

    status_code: Option<u16>,

    wait_hint: Option<Duration>,
}

fn classify_llm_error(
    err: Box<dyn std::error::Error>,
    llm: &dyn LLM,
    attempt: u32,
    config: &RetryConfig,
    infinite_retry: bool,
) -> Result<RetryDecision, Box<dyn std::error::Error + Send + Sync>> {
    let typed = err.downcast_ref::<kimi_core::error::LlmError>();
    let typed_retry_after = typed.and_then(kimi_core::error::LlmError::retry_after);
    let typed_status = typed.and_then(kimi_core::error::LlmError::status_code);
    let err_str = err.to_string();

    if !llm.is_retryable_error(&err_str) {
        let status_code = typed_status.or_else(|| kimi_core::error::llm_http_status(&err_str));
        let trace_id = typed.and_then(kimi_core::error::LlmError::trace_id);
        if status_code.is_none() && trace_id.is_none() {
            return Err(boxed_err(err_str));
        }

        return Err(Box::new(
            kimi_core::error::LlmError::typed(&err_str, status_code).with_trace_id(trace_id),
        ));
    }

    if !infinite_retry && attempt >= config.max_attempts {
        let status_code = typed_status.or_else(|| kimi_core::error::llm_http_status(&err_str));
        let trace_id = typed.and_then(kimi_core::error::LlmError::trace_id);
        return Err(Box::new(
            kimi_core::error::LlmError::attempts_exhausted(attempt, &err_str, status_code)
                .with_trace_id(trace_id),
        ));
    }
    let status_code = typed_status.or_else(|| kimi_core::error::llm_http_status(&err_str));

    let error_name = if crate::llm::http::is_cancelled_error(&err_str) {
        "AbortError"
    } else if status_code.is_some() {
        "APIStatusError"
    } else if err_str.starts_with("llm transport error ") {
        "TransportError"
    } else {
        "Error"
    };
    Ok(RetryDecision {
        error_name,
        error_message: err_str.clone(),
        status_code,
        wait_hint: typed_retry_after.or_else(|| retry_after_hint(&err_str)),
    })
}

struct EmptyResponse {
    think_only: bool,
}

const EMPTY_RESPONSE_DETAIL: &str =
    "The API returned an empty response (no content, no tool calls).";
const THINK_ONLY_RESPONSE_DETAIL: &str = "The API returned a response containing only thinking content without any text or tool calls. This usually indicates the stream was interrupted or the output token budget was exhausted during reasoning.";

pub(crate) fn is_content_filtered(finish_reason: Option<&str>) -> bool {
    matches!(
        finish_reason,
        Some(
            "content_filter"
                | "filtered"
                | "refusal"
                | "safety"
                | "recitation"
                | "blocklist"
                | "prohibited_content"
                | "spii"
                | "image_safety"
        )
    )
}

fn empty_response_of(response: &LLMChatResponse) -> Option<EmptyResponse> {
    let has_tool_calls = !response.tool_calls.is_empty();
    if response.content.is_empty() && response.thinking.is_empty() && !has_tool_calls {
        return Some(EmptyResponse { think_only: false });
    }

    let has_think = !response.thinking.is_empty();
    let has_text = !response.content.trim().is_empty();
    if has_think && !has_text && !has_tool_calls {
        return Some(EmptyResponse { think_only: true });
    }
    None
}

fn empty_response_message(
    empty: &EmptyResponse,
    response: &LLMChatResponse,
    llm: &dyn LLM,
) -> String {
    let detail = if empty.think_only {
        THINK_ONLY_RESPONSE_DETAIL
    } else {
        EMPTY_RESPONSE_DETAIL
    };
    let finish_hint = match response.finish_reason.as_deref() {
        None => String::new(),
        Some(reason) => format!(" Provider stop details: finishReason={reason}."),
    };
    format!(
        "{detail}{finish_hint} Provider: {}, model: {}",
        llm.transport(),
        llm.model_name()
    )
}

fn empty_response_retryable(response: &LLMChatResponse, llm: &dyn LLM) -> Option<EmptyResponse> {
    if llm.transport() != "native-http" {
        return None;
    }
    if is_content_filtered(response.finish_reason.as_deref()) {
        return None;
    }
    empty_response_of(response)
}

pub(crate) fn retry_after_hint(error: &str) -> Option<Duration> {
    let marker = " (retry-after ";
    let start = error.rfind(marker)? + marker.len();
    let rest = &error[start..];
    let end = rest.find('s')?;
    rest[..end]
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

pub(crate) fn step_delay(backoff: Duration, hint: Option<Duration>) -> Duration {
    match hint {
        Some(hint) if hint > backoff => hint,
        _ => backoff,
    }
}

pub fn execute_loop_step<'a>(
    turn_id: &'a str,
    step: u32,
    llm: &'a dyn LLM,
    messages: &'a [LLMMessage],
    tools: &'a [&'a dyn ExecutableTool],
    tool_defs: &'a [ToolInfo],
    cancel: Option<&'a tokio_util::sync::CancellationToken>,
) -> BoxFuture<'a, Result<StepResult, Box<dyn std::error::Error + Send + Sync>>> {
    execute_loop_step_with_retry(
        turn_id,
        step,
        llm,
        messages,
        tools,
        tool_defs,
        &RetryConfig::default(),
        cancel,
        None,
        None,
        None,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn execute_loop_step_with_retry<'a>(
    turn_id: &'a str,
    step: u32,
    llm: &'a dyn LLM,
    messages: &'a [LLMMessage],
    tools: &'a [&'a dyn ExecutableTool],
    tool_defs: &'a [ToolInfo],
    retry_config: &RetryConfig,
    cancel: Option<&'a tokio_util::sync::CancellationToken>,
    telemetry: Option<&'a (dyn Fn(serde_json::Value) + Send + Sync)>,
    used_context_tokens: Option<u32>,
    max_context_tokens: Option<u32>,
    turn_input_tokens: Option<u32>,
) -> BoxFuture<'a, Result<StepResult, Box<dyn std::error::Error + Send + Sync>>> {
    execute_loop_step_with_projection(
        turn_id,
        step,
        llm,
        messages,
        tools,
        tool_defs,
        retry_config,
        cancel,
        telemetry,
        used_context_tokens,
        max_context_tokens,
        turn_input_tokens,
        false,
        crate::callbacks::MAIN_AGENT_ID,
        false,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn execute_loop_step_with_projection<'a>(
    turn_id: &'a str,
    step: u32,
    llm: &'a dyn LLM,
    messages: &'a [LLMMessage],
    _tools: &'a [&'a dyn ExecutableTool],
    tool_defs: &'a [ToolInfo],
    retry_config: &RetryConfig,
    cancel: Option<&'a tokio_util::sync::CancellationToken>,
    telemetry: Option<&'a (dyn Fn(serde_json::Value) + Send + Sync)>,
    used_context_tokens: Option<u32>,
    max_context_tokens: Option<u32>,
    turn_input_tokens: Option<u32>,
    structure_strict: bool,
    agent_id: &'a str,
    tool_select: bool,
    emit: Option<&'a (dyn Fn(serde_json::Value) + Send + Sync)>,
) -> BoxFuture<'a, Result<StepResult, Box<dyn std::error::Error + Send + Sync>>> {
    let retry_config = retry_config.clone();
    let cancel_token = cancel.cloned();
    Box::pin(async move {
        let projected = if structure_strict {
            crate::llm::wire::project_strict(messages)
        } else {
            crate::llm::wire::project_messages(messages)
        };
        let messages: std::sync::Arc<[LLMMessage]> = std::sync::Arc::from(projected);
        let tools: std::sync::Arc<[ToolInfo]> = std::sync::Arc::from(tool_defs);
        let build_params = || LLMChatParams {
            messages: std::sync::Arc::clone(&messages),
            tools: std::sync::Arc::clone(&tools),
            cancel: cancel_token.clone(),
            used_context_tokens,
            max_context_tokens,

            max_output_tokens: None,
            max_output_size: None,
        };
        let mut params = Some(build_params());

        let mut attempt: u32 = 0;
        let infinite_retry = crate::turn_loop::retry::infinite_retry_enabled();

        let started = std::time::Instant::now();
        let response = loop {
            attempt += 1;

            let call_params = match params.take() {
                Some(params) => params,
                None => build_params(),
            };
            if let Some(emit) = emit {
                emit(llm_request_record(
                    agent_id,
                    turn_id,
                    step,
                    llm,
                    &call_params,
                    tool_select,
                ));
            }
            let (break_resp, return_err, retry_decision) = match llm.chat(call_params).await {
                Ok(resp) => match empty_response_retryable(&resp, llm) {
                    None => (Some(resp), None, None),

                    Some(empty) => {
                        let message = empty_response_message(&empty, &resp, llm);
                        if !infinite_retry && attempt >= retry_config.max_attempts {
                            (None, Some((boxed_err(message), true)), None)
                        } else {
                            (
                                None,
                                None,
                                Some(RetryDecision {
                                    error_name: "EmptyResponseError",
                                    error_message: message,
                                    status_code: None,
                                    wait_hint: None,
                                }),
                            )
                        }
                    }
                },
                Err(err) => {
                    let retryable = llm.is_retryable_error(&err.to_string());
                    match classify_llm_error(err, llm, attempt, &retry_config, infinite_retry) {
                        Ok(decision) => (None, None, Some(decision)),
                        Err(e) => (None, Some((e, retryable)), None),
                    }
                }
            };
            if let Some(resp) = break_resp {
                break resp;
            }
            if let Some((e, retryable)) = return_err {
                if let Some(telemetry) = telemetry {
                    let text = e.to_string();
                    let typed = e.downcast_ref::<kimi_core::error::LlmError>();

                    let status_code = typed
                        .and_then(kimi_core::error::LlmError::status_code)
                        .or_else(|| kimi_core::error::llm_http_status(&text));

                    let error_type = if crate::llm::http::is_cancelled_error(&text) {
                        "AbortError"
                    } else if status_code.is_some() {
                        "APIStatusError"
                    } else if text.starts_with("llm transport error ") {
                        "TransportError"
                    } else {
                        "Error"
                    };
                    let mut payload = serde_json::json!({
                        "event": "api_error",
                        "error_type": error_type,
                        "model": llm.model_name(),
                        "retryable": retryable,
                        "duration_ms": started.elapsed().as_millis() as u64,
                        "request_kind": "turn",
                        "step_no": step,
                    });

                    if let Some(alias) = llm.model_alias() {
                        payload["alias"] = serde_json::json!(alias);
                    }
                    if let Some(provider) = llm.provider_type() {
                        payload["provider_type"] = serde_json::json!(provider);
                    }
                    if let Some(protocol) = llm.protocol() {
                        payload["protocol"] = serde_json::json!(protocol);
                    }
                    if let Some(tokens) = turn_input_tokens {
                        payload["input_tokens"] = serde_json::json!(tokens);
                    }
                    if let Some(index) = crate::turn_loop::turn_index(turn_id) {
                        payload["turn_id"] = serde_json::json!(index);
                    }
                    if let Some(code) = status_code {
                        payload["status_code"] = serde_json::json!(code);
                    }
                    if let Some(id) = typed.and_then(kimi_core::error::LlmError::trace_id) {
                        payload["request_id"] = serde_json::json!(id);
                    }
                    telemetry(payload);
                }
                return Err(e);
            }
            let decision = retry_decision.expect("a retryable failure carries its decision");
            let delay = step_delay(retry_delay(attempt, &retry_config), decision.wait_hint);

            tracing::warn!(
                turn_id = turn_id,
                step = step,
                failed_attempt = attempt,
                next_attempt = attempt + 1,
                max_attempts = retry_config.max_attempts,
                infinite_retry = infinite_retry,
                delay_ms = delay.as_millis() as u64,
                error_name = decision.error_name,
                status_code = decision.status_code,
                error_message = %decision.error_message,
                "TurnStepRetrying"
            );
            if let Some(telemetry) = telemetry {
                telemetry(serde_json::json!({
                    "event": "TurnStepRetrying",
                    "turn_id": turn_id,
                    "step": step,
                    "failed_attempt": attempt,
                    "next_attempt": attempt + 1,
                    "max_attempts": retry_config.max_attempts,
                    "infinite_retry": infinite_retry,
                    "delay_ms": delay.as_millis() as u64,



                    "error_name": decision.error_name,
                    "error_message": decision.error_message,
                    "status_code": decision.status_code,
                }));
            }

            match cancel {
                Some(token) => tokio::select! {
                    _ = token.cancelled() => {
                        return Err(boxed_err("llm cancelled during retry backoff".into()));
                    }
                    _ = tokio::time::sleep(delay) => {}
                },
                None => tokio::time::sleep(delay).await,
            }
        };

        let usage = response.usage.clone();
        let attempts = attempt;
        let finish_reason = response.finish_reason.clone();
        let trace_id = llm.last_trace_id();

        let thinking = response.thinking.clone();
        if response.tool_calls.is_empty() {
            Ok(StepResult {
                usage,
                stop_reason: LoopStepStopReason::Complete,
                content: response.content,
                thinking,
                attempts,
                finish_reason,
                trace_id,
                timing: response.timing,
            })
        } else {
            let mut tool_calls = response.tool_calls;

            for tc in &mut tool_calls {
                if tc.id.trim().is_empty() {
                    tc.id = format!("toolu_{}", fastrand::u64(..));
                }
            }
            Ok(StepResult {
                usage,
                stop_reason: LoopStepStopReason::ToolCalls(tool_calls),
                content: response.content,
                thinking,
                attempts,
                finish_reason,
                trace_id,
                timing: response.timing,
            })
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kimi_core::types::TokenUsage;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct FlakyLlm {
        system_prompt: String,
        model_name: String,
        fail_count: u32,
        calls: AtomicU32,
        retryable: bool,
    }

    impl FlakyLlm {
        fn new(fail_count: u32, retryable: bool) -> Self {
            Self {
                system_prompt: "test".into(),
                model_name: "flaky".into(),
                fail_count,
                calls: AtomicU32::new(0),
                retryable,
            }
        }
    }

    impl LLM for FlakyLlm {
        fn system_prompt(&self) -> &str {
            &self.system_prompt
        }
        fn model_name(&self) -> &str {
            &self.model_name
        }
        fn is_retryable_error(&self, _error: &str) -> bool {
            self.retryable
        }

        fn chat(
            &self,
            _params: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let fail_count = self.fail_count;
            Box::pin(async move {
                if call < fail_count {
                    return Err(format!("simulated failure {}", call + 1).into());
                }
                Ok(LLMChatResponse {
                    content: String::new(),
                    thinking: vec![],
                    tool_calls: vec![],
                    finish_reason: Some("stop".into()),
                    usage: TokenUsage {
                        input_other: 1,
                        output: 1,
                                                ..Default::default()
                    },
                    timing: None,
                })
            })
        }
    }

    #[tokio::test]
    async fn test_retry_succeeds_after_failures() {
        let llm = FlakyLlm::new(2, true);
        let config = RetryConfig {
            max_attempts: 3,
            base_delay_ms: 1,
            max_delay_ms: 10,
        };
        let result = execute_loop_step_with_retry(
            "t1",
            1,
            &llm,
            &[],
            &[],
            &[],
            &config,
            None,
            None,
            None,
            None,
            None,
        )
        .await;
        assert!(
            result.is_ok(),
            "should succeed after retries: {:?}",
            result.err()
        );
        assert_eq!(llm.calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn test_retry_exhausted() {
        let llm = FlakyLlm::new(100, true);
        let config = RetryConfig {
            max_attempts: 2,
            base_delay_ms: 1,
            max_delay_ms: 10,
        };
        let result = execute_loop_step_with_retry(
            "t1",
            1,
            &llm,
            &[],
            &[],
            &[],
            &config,
            None,
            None,
            None,
            None,
            None,
        )
        .await;
        assert!(result.is_err());
        assert_eq!(llm.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_non_retryable_fails_immediately() {
        let llm = FlakyLlm::new(100, false);
        let config = RetryConfig {
            max_attempts: 5,
            base_delay_ms: 1,
            max_delay_ms: 10,
        };
        let result = execute_loop_step_with_retry(
            "t1",
            1,
            &llm,
            &[],
            &[],
            &[],
            &config,
            None,
            None,
            None,
            None,
            None,
        )
        .await;
        assert!(result.is_err());
        assert_eq!(llm.calls.load(Ordering::SeqCst), 1);
    }

    struct StatusLlm {
        system_prompt: String,
        model_name: String,
        error: String,
    }

    impl StatusLlm {
        fn new(error: &str) -> Self {
            Self {
                system_prompt: "test".into(),
                model_name: "status".into(),
                error: error.into(),
            }
        }
    }

    impl LLM for StatusLlm {
        fn system_prompt(&self) -> &str {
            &self.system_prompt
        }
        fn model_name(&self) -> &str {
            &self.model_name
        }
        fn model_alias(&self) -> Option<&str> {
            Some("alias-under-test")
        }
        fn provider_type(&self) -> Option<&str> {
            Some("kimi")
        }
        fn protocol(&self) -> Option<&str> {
            Some("openai")
        }
        fn thinking_effort(&self) -> Option<&str> {
            Some("high")
        }
        fn is_retryable_error(&self, error: &str) -> bool {
            kimi_core::error::llm_http_status(error) == Some(429)
        }
        fn chat(
            &self,
            _params: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            let error = self.error.clone();
            Box::pin(async move { Err(error.into()) })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn turn_step_retrying_carries_the_v2_error_fields() {
        let llm =
            StatusLlm::new("llm http status 429 Too Many Requests: slow down (retry-after 7s)");
        let config = RetryConfig {
            max_attempts: 2,
            base_delay_ms: 1,
            max_delay_ms: 10,
        };
        let events: Arc<std::sync::Mutex<Vec<serde_json::Value>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&events);
        let telemetry = move |event: serde_json::Value| {
            sink.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(event);
        };

        let _ = execute_loop_step_with_retry(
            "turn-7",
            3,
            &llm,
            &[],
            &[],
            &[],
            &config,
            None,
            Some(&telemetry),
            None,
            None,
            Some(1234),
        )
        .await;

        let recorded = events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();

        assert_eq!(
            recorded.len(),
            2,
            "one backoff, one api_error: {recorded:?}"
        );

        let event = &recorded[0];
        assert_eq!(event["event"], "TurnStepRetrying");
        assert_eq!(event["step"], 3);
        assert_eq!(event["failed_attempt"], 1);
        assert_eq!(event["next_attempt"], 2);
        assert_eq!(event["max_attempts"], 2);

        assert_eq!(event["delay_ms"], 7000);
        assert_eq!(event["error_name"], "APIStatusError");
        assert_eq!(event["status_code"], 429);

        let api_error = &recorded[1];
        assert_eq!(api_error["event"], "api_error");

        assert_eq!(api_error["step_no"], 3);
        assert_eq!(api_error["status_code"], 429);
        assert_eq!(api_error["error_type"], "APIStatusError");
        assert_eq!(api_error["model"], "status");

        assert_eq!(api_error["retryable"], true);
        assert!(api_error["duration_ms"].is_u64());

        assert_eq!(api_error["alias"], "alias-under-test");
        assert_eq!(api_error["provider_type"], "kimi");
        assert_eq!(api_error["protocol"], "openai");
        assert_eq!(api_error["request_kind"], "turn");
        assert_eq!(api_error["turn_id"], 7);
        assert_eq!(api_error["input_tokens"], 1234);

        assert!(api_error.get("request_id").is_none());
        assert!(api_error.get("trace_id").is_none());
        assert!(api_error.get("attempts").is_none());
        assert!(api_error.get("step").is_none());

        assert!(api_error.get("error_message").is_none());
        assert!(
            event["error_message"]
                .as_str()
                .unwrap_or_default()
                .contains("slow down"),
            "the failure text must ride along: {event}"
        );
    }

    #[tokio::test]
    async fn turn_step_retrying_names_a_cancelled_failure() {
        let llm = StatusLlm::new("llm cancelled: request aborted");
        let config = RetryConfig {
            max_attempts: 2,
            base_delay_ms: 1,
            max_delay_ms: 10,
        };
        let events: Arc<std::sync::Mutex<Vec<serde_json::Value>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&events);
        let telemetry = move |event: serde_json::Value| {
            sink.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(event);
        };

        let _ = execute_loop_step_with_retry(
            "t1",
            1,
            &llm,
            &[],
            &[],
            &[],
            &config,
            None,
            Some(&telemetry),
            None,
            None,
            None,
        )
        .await;

        let recorded = events.lock().unwrap().clone();
        assert!(
            recorded
                .iter()
                .all(|event| event["event"] != "TurnStepRetrying"),
            "a cancellation never retries: {recorded:?}"
        );
        assert!(crate::llm::http::is_cancelled_error(
            "llm cancelled: request aborted"
        ));
        assert!(!crate::llm::http::is_cancelled_error(
            "llm http status 429: slow down"
        ));
    }

    #[tokio::test]
    async fn test_no_retry_needed() {
        let llm = FlakyLlm::new(0, true);
        let config = RetryConfig::default();
        let result = execute_loop_step_with_retry(
            "t1",
            1,
            &llm,
            &[],
            &[],
            &[],
            &config,
            None,
            None,
            None,
            None,
            None,
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(llm.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_empty_tool_call_id_gets_synthesized() {
        struct EmptyIdLlm;
        impl LLM for EmptyIdLlm {
            fn system_prompt(&self) -> &str {
                "test"
            }
            fn model_name(&self) -> &str {
                "empty-id-llm"
            }
            fn is_retryable_error(&self, _: &str) -> bool {
                false
            }
            fn chat(
                &self,
                _: LLMChatParams,
            ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
            {
                Box::pin(async {
                    Ok(LLMChatResponse {
                        content: String::new(),
                        thinking: vec![],
                        tool_calls: vec![
                            ToolCall {
                                id: String::new(),
                                name: "read".into(),
                                arguments: Some(serde_json::json!({}).to_string()),
                                extras: None,
                                raw_id: None,
                                _stream_index: None,
                            },
                            ToolCall {
                                id: "  ".into(),
                                name: "glob".into(),
                                arguments: Some(serde_json::json!({}).to_string()),
                                extras: None,
                                raw_id: None,
                                _stream_index: None,
                            },
                        ],
                        finish_reason: Some("tool_calls".into()),
                        usage: kimi_core::types::TokenUsage::default(),
                        timing: None,
                    })
                })
            }
        }
        let result = execute_loop_step_with_retry(
            "t1",
            1,
            &EmptyIdLlm,
            &[],
            &[],
            &[],
            &RetryConfig::default(),
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        let LoopStepStopReason::ToolCalls(tool_calls) = result.stop_reason else {
            panic!("expected tool calls");
        };
        assert_eq!(tool_calls.len(), 2);
        for tc in &tool_calls {
            assert!(!tc.id.trim().is_empty(), "id must be synthesized");
        }
        assert_ne!(tool_calls[0].id, tool_calls[1].id, "ids must be unique");
    }

    #[tokio::test]
    async fn test_retry_tool_calls_in_response() {
        let calls = Arc::new(AtomicU32::new(0));
        struct ToolCallLlm {
            calls: Arc<AtomicU32>,
        }
        impl LLM for ToolCallLlm {
            fn system_prompt(&self) -> &str {
                "test"
            }
            fn model_name(&self) -> &str {
                "tool-llm"
            }
            fn is_retryable_error(&self, _: &str) -> bool {
                false
            }
            fn chat(
                &self,
                _: LLMChatParams,
            ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
            {
                let call = self.calls.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move {
                    Ok(LLMChatResponse {
                        content: String::new(),
                        thinking: vec![],
                        tool_calls: if call == 0 {
                            vec![ToolCall {
                                id: "c1".into(),
                                name: "read".into(),
                                arguments: Some(serde_json::json!({}).to_string()),
                                extras: None,
                                raw_id: None,
                                _stream_index: None,
                            }]
                        } else {
                            vec![]
                        },
                        finish_reason: Some("stop".into()),
                        usage: TokenUsage {
                            input_other: 1,
                            output: 1,
                                                        ..Default::default()
                        },
                        timing: None,
                    })
                })
            }
        }

        let llm = ToolCallLlm {
            calls: calls.clone(),
        };
        let result = execute_loop_step_with_retry(
            "t1",
            1,
            &llm,
            &[],
            &[],
            &[],
            &RetryConfig::default(),
            None,
            None,
            None,
            None,
            None,
        )
        .await;
        assert!(result.is_ok());
        let step = result.unwrap();
        match step.stop_reason {
            LoopStepStopReason::ToolCalls(tcs) => assert_eq!(tcs.len(), 1),
            _ => panic!("expected ToolCalls stop reason"),
        }
    }

    #[tokio::test]
    async fn test_retry_max_attempts_one() {
        let llm = FlakyLlm::new(1, true);
        let config = RetryConfig {
            max_attempts: 1,
            base_delay_ms: 1,
            max_delay_ms: 10,
        };
        let result = execute_loop_step_with_retry(
            "t1",
            1,
            &llm,
            &[],
            &[],
            &[],
            &config,
            None,
            None,
            None,
            None,
            None,
        )
        .await;
        assert!(result.is_err());
        assert_eq!(llm.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_retry_succeeds_on_first_retry() {
        let llm = FlakyLlm::new(1, true);
        let config = RetryConfig {
            max_attempts: 2,
            base_delay_ms: 1,
            max_delay_ms: 10,
        };
        let result = execute_loop_step_with_retry(
            "t1",
            1,
            &llm,
            &[],
            &[],
            &[],
            &config,
            None,
            None,
            None,
            None,
            None,
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(llm.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_execute_loop_step_no_retry_config() {
        let llm = FlakyLlm::new(0, true);
        let result = execute_loop_step("t1", 1, &llm, &[], &[], &[], None).await;
        assert!(result.is_ok());
    }

    #[test]
    fn retry_after_hint_is_parsed_from_the_error_text() {
        let with_hint = "llm http status 429 Too Many Requests: slow down (retry-after 30s)";
        assert_eq!(retry_after_hint(with_hint), Some(Duration::from_secs(30)));

        assert_eq!(retry_after_hint("llm http status 429: slow down"), None);
        assert_eq!(
            retry_after_hint("llm http status 429: (retry-after Wed, 21 Oct 2015 07:28:00 GMT)"),
            None
        );
    }

    #[tokio::test]
    async fn test_retry_waits_at_least_as_long_as_the_provider_asks() {
        assert_eq!(
            step_delay(Duration::from_secs(1), Some(Duration::from_secs(30))),
            Duration::from_secs(30)
        );

        assert_eq!(
            step_delay(Duration::from_secs(5), Some(Duration::from_millis(200))),
            Duration::from_secs(5)
        );

        assert_eq!(
            step_delay(Duration::from_secs(3), None),
            Duration::from_secs(3)
        );
    }

    #[test]
    fn classify_llm_error_honors_infinite_retry() {
        let llm = FlakyLlm::new(100, true);
        let config = RetryConfig {
            max_attempts: 2,
            base_delay_ms: 1,
            max_delay_ms: 10,
        };

        let exhausted = classify_llm_error(
            Box::new(std::io::Error::other("simulated failure")),
            &llm,
            2,
            &config,
            false,
        );
        assert!(exhausted.is_err(), "cap reached without infinite mode");

        let infinite = classify_llm_error(
            Box::new(std::io::Error::other("simulated failure")),
            &llm,
            2,
            &config,
            true,
        );
        assert!(
            infinite.is_ok(),
            "infinite mode must keep retrying past max_attempts"
        );

        let non_retryable_llm = FlakyLlm::new(100, false);
        let fatal = classify_llm_error(
            Box::new(std::io::Error::other("bad request")),
            &non_retryable_llm,
            1,
            &config,
            true,
        );
        assert!(fatal.is_err(), "non-retryable errors never retry");
    }

    #[tokio::test]
    async fn test_cancel_aborts_during_backoff_wait() {
        let llm = FlakyLlm::new(u32::MAX, true);
        let config = RetryConfig {
            max_attempts: 10,
            base_delay_ms: 30_000,
            max_delay_ms: 30_000,
        };
        let token = tokio_util::sync::CancellationToken::new();
        let cancel = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            cancel.cancel();
        });
        let started = std::time::Instant::now();
        let result = execute_loop_step_with_retry(
            "t1",
            1,
            &llm,
            &[],
            &[],
            &[],
            &config,
            Some(&token),
            None,
            None,
            None,
            None,
        )
        .await;
        assert!(result.is_err(), "cancelled step must resolve with an error");
        assert_eq!(llm.calls.load(Ordering::SeqCst), 1, "no retry after cancel");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "cancel must not wait out the 30s backoff, took {:?}",
            started.elapsed()
        );
    }

    struct EmptyThenLlm {
        empty_times: u32,
        transport: &'static str,
        calls: AtomicU32,
    }

    impl EmptyThenLlm {
        fn new(empty_times: u32, transport: &'static str) -> Self {
            Self {
                empty_times,
                transport,
                calls: AtomicU32::new(0),
            }
        }
    }

    fn answer(content: &str, think: bool, finish: &'static str) -> LLMChatResponse {
        LLMChatResponse {
            content: content.into(),
            thinking: if think {
                vec![ContentPart::Text {
                    text: "reasoning".into(),
                    meta: None,
                }]
            } else {
                vec![]
            },
            tool_calls: vec![],
            finish_reason: Some(finish.into()),
            usage: TokenUsage::default(),
            timing: None,
        }
    }

    fn tool_call() -> ToolCall {
        ToolCall {
            id: "call-1".into(),
            name: "Read".into(),
            arguments: Some(serde_json::json!({ "path": "a.txt" }).to_string()),
            extras: None,
            raw_id: None,
            _stream_index: None,
        }
    }

    impl LLM for EmptyThenLlm {
        fn system_prompt(&self) -> &str {
            "test"
        }
        fn model_name(&self) -> &str {
            "empty-then"
        }
        fn is_retryable_error(&self, _error: &str) -> bool {
            true
        }
        fn transport(&self) -> &'static str {
            self.transport
        }
        fn chat(
            &self,
            _params: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let empty_times = self.empty_times;
            Box::pin(async move {
                if call < empty_times {
                    Ok(answer("", false, "stop"))
                } else {
                    Ok(answer("a real answer", false, "stop"))
                }
            })
        }
    }

    #[test]
    fn empty_response_of_matches_v2_empty_response_error() {
        let empty = empty_response_of(&answer("", false, "stop")).expect("plain empty response");
        assert!(!empty.think_only);

        let think = empty_response_of(&answer("", true, "stop")).expect("think-only response");
        assert!(think.think_only);

        assert!(empty_response_of(&answer("   ", false, "stop")).is_none());

        assert!(empty_response_of(&answer("hi", false, "stop")).is_none());
        assert!(empty_response_of(&answer("hi", true, "stop")).is_none());

        let mut with_call = answer("", false, "tool_calls");
        with_call.tool_calls = vec![tool_call()];
        assert!(empty_response_of(&with_call).is_none());
        let mut think_and_call = answer("", true, "tool_calls");
        think_and_call.tool_calls = vec![tool_call()];
        assert!(empty_response_of(&think_and_call).is_none());
    }

    #[test]
    fn is_content_filtered_covers_the_turn_level_filter_vocabulary() {
        for filtered in [
            "content_filter",
            "filtered",
            "refusal",
            "safety",
            "recitation",
            "blocklist",
            "prohibited_content",
            "spii",
            "image_safety",
        ] {
            assert!(
                is_content_filtered(Some(filtered)),
                "{filtered} must count as a filter stop"
            );
        }

        for ordinary in ["stop", "length", "max_tokens", "tool_calls"] {
            assert!(
                !is_content_filtered(Some(ordinary)),
                "{ordinary} is not a filter stop"
            );
        }
        assert!(!is_content_filtered(None));
    }

    #[tokio::test]
    async fn an_empty_response_is_retried_until_the_provider_answers() {
        let llm = EmptyThenLlm::new(2, "native-http");
        let config = RetryConfig {
            max_attempts: 5,
            base_delay_ms: 1,
            max_delay_ms: 2,
        };
        let result = execute_loop_step_with_retry(
            "t1",
            1,
            &llm,
            &[],
            &[],
            &[],
            &config,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .expect("the third attempt carries a real answer");
        assert_eq!(llm.calls.load(Ordering::SeqCst), 3, "two empty retries");
        assert_eq!(result.content, "a real answer");
    }

    #[tokio::test]
    async fn a_content_filtered_empty_response_is_not_retried() {
        for filtered in ["content_filter", "refusal", "safety", "image_safety"] {
            let llm = FilteredEmptyLlm {
                finish: filtered,
                calls: AtomicU32::new(0),
            };
            let config = RetryConfig {
                max_attempts: 5,
                base_delay_ms: 1,
                max_delay_ms: 2,
            };
            let result = execute_loop_step_with_retry(
                "t1",
                1,
                &llm,
                &[],
                &[],
                &[],
                &config,
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .expect("a filtered turn completes rather than erroring");
            assert_eq!(
                llm.calls.load(Ordering::SeqCst),
                1,
                "{filtered} must not spend a retry"
            );
            assert!(
                matches!(result.stop_reason, LoopStepStopReason::Complete),
                "a filtered turn completes; got {:?}",
                result.stop_reason
            );
        }
    }

    struct FilteredEmptyLlm {
        finish: &'static str,
        calls: AtomicU32,
    }

    impl LLM for FilteredEmptyLlm {
        fn system_prompt(&self) -> &str {
            "test"
        }
        fn model_name(&self) -> &str {
            "filtered"
        }
        fn is_retryable_error(&self, _error: &str) -> bool {
            true
        }
        fn transport(&self) -> &'static str {
            "native-http"
        }
        fn chat(
            &self,
            _params: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let finish = self.finish;
            Box::pin(async move { Ok(answer("", false, finish)) })
        }
    }

    #[tokio::test]
    async fn an_empty_response_on_the_host_proxy_is_left_alone() {
        let llm = EmptyThenLlm::new(u32::MAX, "host-proxy");
        let config = RetryConfig {
            max_attempts: 5,
            base_delay_ms: 1,
            max_delay_ms: 2,
        };
        let result = execute_loop_step_with_retry(
            "t1",
            1,
            &llm,
            &[],
            &[],
            &[],
            &config,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .expect("the proxy leg owns its own transcript");
        assert_eq!(llm.calls.load(Ordering::SeqCst), 1, "no retry on the proxy");
        assert!(result.content.is_empty());
    }

    #[tokio::test]
    async fn a_persistently_empty_response_surfaces_after_the_budget() {
        let llm = EmptyThenLlm::new(u32::MAX, "native-http");
        let config = RetryConfig {
            max_attempts: 3,
            base_delay_ms: 1,
            max_delay_ms: 2,
        };
        let err = execute_loop_step_with_retry(
            "t1",
            1,
            &llm,
            &[],
            &[],
            &[],
            &config,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .expect_err("a permanently empty provider must not read as success");
        assert_eq!(llm.calls.load(Ordering::SeqCst), 3, "the budget is spent");
        assert!(
            err.to_string().contains("empty response"),
            "the error must name the cause, got: {err}"
        );
    }

    struct TypedErrorLlm {
        system_prompt: String,
        model_name: String,
        error: kimi_core::error::LlmError,
    }

    impl LLM for TypedErrorLlm {
        fn system_prompt(&self) -> &str {
            &self.system_prompt
        }
        fn model_name(&self) -> &str {
            &self.model_name
        }
        fn is_retryable_error(&self, _error: &str) -> bool {
            false
        }
        fn chat(
            &self,
            _params: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            let error = self.error.clone();
            Box::pin(
                async move { Err(Box::new(error) as Box<dyn std::error::Error + Send + Sync>) },
            )
        }
    }

    #[tokio::test]
    async fn api_error_carries_the_provider_trace_id() {
        let llm = TypedErrorLlm {
            system_prompt: "test".into(),
            model_name: "typed".into(),
            error: kimi_core::error::LlmError::http(503, "upstream unavailable", None)
                .with_trace_id(Some("trace-42")),
        };
        let config = RetryConfig {
            max_attempts: 3,
            base_delay_ms: 1,
            max_delay_ms: 10,
        };
        let events: Arc<std::sync::Mutex<Vec<serde_json::Value>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&events);
        let telemetry = move |event: serde_json::Value| {
            sink.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(event);
        };

        let _ = execute_loop_step_with_retry(
            "t1",
            1,
            &llm,
            &[],
            &[],
            &[],
            &config,
            None,
            Some(&telemetry),
            None,
            None,
            None,
        )
        .await;

        let recorded = events.lock().unwrap().clone();
        assert_eq!(
            recorded.len(),
            1,
            "terminal on the first attempt: {recorded:?}"
        );
        assert_eq!(recorded[0]["event"], "api_error");
        assert_eq!(recorded[0]["status_code"], 503);
        assert_eq!(recorded[0]["request_id"], "trace-42");

        assert_eq!(recorded[0]["error_type"], "APIStatusError");
        assert_eq!(recorded[0]["model"], "typed");
        assert_eq!(recorded[0]["retryable"], false);
        assert!(recorded[0]["duration_ms"].is_u64());

        assert!(recorded[0].get("error_message").is_none());
    }

    #[test]
    fn an_llm_request_record_carries_the_v2_fields() {
        let llm = FlakyLlm::new(0, false);
        let tools = vec![ToolInfo {
            name: "Read".into(),
            description: "reads".into(),
            input_schema: serde_json::json!({ "type": "object" }),
        }];
        let messages = vec![LLMMessage::user("hi")];
        let params = LLMChatParams {
            messages: std::sync::Arc::from(messages),
            tools: std::sync::Arc::from(tools.clone()),
            ..Default::default()
        };
        let record = llm_request_record("main", "turn-1", 1, &llm, &params, true);
        assert_eq!(record["agent_id"], serde_json::json!("main"));
        assert_eq!(record["kind"], serde_json::json!("loop"));
        assert_eq!(record["provider"], serde_json::json!("custom"));
        assert_eq!(record["model"], serde_json::json!("flaky"));
        assert_eq!(record["toolSelect"], serde_json::json!(true));
        assert_eq!(record["messageCount"], serde_json::json!(1));
        assert_eq!(record["turnStep"], serde_json::json!("turn-1.2"));
        assert_eq!(
            record["systemPromptHash"],
            serde_json::json!(crate::session::agent_records::sha256_hex("test"))
        );
        assert_eq!(
            record["toolsHash"],
            serde_json::json!(crate::session::agent_records::sha256_hex(
                &crate::session::agent_records::tools_signature(&tools)
            ))
        );
        assert!(record.get("modelAlias").is_none());
        assert!(record.get("thinkingEffort").is_none());
    }

    struct ParameterLlm;

    impl LLM for ParameterLlm {
        fn system_prompt(&self) -> &str {
            "prompt"
        }
        fn model_name(&self) -> &str {
            "kimi-k2"
        }
        fn is_retryable_error(&self, _error: &str) -> bool {
            false
        }
        fn protocol(&self) -> Option<&str> {
            Some("openai")
        }
        fn request_parameters(&self) -> Option<kimi_core::types::LlmRequestParameters> {
            Some(kimi_core::types::LlmRequestParameters {
                temperature: Some(0.3),
                top_p: Some(0.9),
                beta_api: Some(true),
                thinking_keep: Some("all".into()),
            })
        }
        fn effective_max_tokens(&self, _params: &LLMChatParams) -> Option<u32> {
            Some(4096)
        }
        fn chat(
            &self,
            _params: LLMChatParams,
        ) -> BoxFuture<
            '_,
            Result<
                crate::turn_loop::types::LLMChatResponse,
                Box<dyn std::error::Error + Send + Sync>,
            >,
        > {
            Box::pin(async { Err("not used".into()) })
        }
    }

    #[test]
    fn recorded_request_parameters_mirror_what_is_sent() {
        let params = LLMChatParams {
            messages: std::sync::Arc::from(vec![LLMMessage::user("hi")]),
            ..Default::default()
        };
        let record = llm_request_record("main", "turn-1", 0, &ParameterLlm, &params, false);
        assert_eq!(record["maxTokens"], serde_json::json!(4096));
        assert_eq!(record["temperature"], serde_json::json!(0.3));
        assert_eq!(record["topP"], serde_json::json!(0.9));
        assert_eq!(record["betaApi"], serde_json::json!(true));
        assert_eq!(record["thinkingKeep"], serde_json::json!("all"));
        assert!(record.get("attempt").is_none());
        assert!(record.get("projection").is_none());
        assert!(record.get("droppedCount").is_none());
    }
}
