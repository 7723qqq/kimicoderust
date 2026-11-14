use std::sync::Arc;
use std::time::Duration;

use eventsource_stream::Eventsource;
use futures_util::StreamExt;

use crate::llm::model_family::family_default_completion_cap;
use crate::llm::request_options::RequestOptions;
use crate::llm::tool_call_id::{ToolCallIdPolicy, normalize_tool_call_ids};
use crate::llm::wire::{StreamDelta, WireMessage, to_wire};
use crate::llm::{anthropic, google_genai, openai, openai_responses, opencode_adapter};
use crate::rpc::types::{BoxFuture, NativeLlmConfig};
use kimi_core::types::{LLM, LLMChatParams, LLMChatResponse, ToolInfo};

pub const LLM_HEADERS_TIMEOUT_ENV: &str = "KIMI_CODE_LLM_HEADERS_TIMEOUT_MS";

pub const LLM_BODY_TIMEOUT_ENV: &str = "KIMI_CODE_LLM_BODY_TIMEOUT_MS";

const MAX_HEADERS_TIMEOUT_MS: u64 = 2_147_483_647;

const DEFAULT_LLM_IO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

fn parse_llm_headers_timeout_ms(raw: Option<&str>) -> Result<Option<std::time::Duration>, String> {
    let Some(raw) = raw else {
        return Ok(Some(DEFAULT_LLM_IO_TIMEOUT));
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Some(DEFAULT_LLM_IO_TIMEOUT));
    }
    let valid = trimmed
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0 && *value <= MAX_HEADERS_TIMEOUT_MS);
    match valid {
        Some(ms) => Ok(Some(std::time::Duration::from_millis(ms))),
        None => Err(format!(
            "config.invalid: {LLM_HEADERS_TIMEOUT_ENV} must be a positive integer no greater than {MAX_HEADERS_TIMEOUT_MS}, got {raw:?}."
        )),
    }
}

fn parse_llm_body_timeout_ms(raw: Option<&str>) -> Result<Option<std::time::Duration>, String> {
    let Some(raw) = raw else {
        return Ok(Some(DEFAULT_LLM_IO_TIMEOUT));
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Some(DEFAULT_LLM_IO_TIMEOUT));
    }
    let valid = trimmed
        .parse::<u64>()
        .ok()
        .filter(|value| *value <= MAX_HEADERS_TIMEOUT_MS);
    match valid {
        Some(0) => Ok(None),
        Some(ms) => Ok(Some(std::time::Duration::from_millis(ms))),
        None => Err(format!(
            "config.invalid: {LLM_BODY_TIMEOUT_ENV} must be an integer between 0 and {MAX_HEADERS_TIMEOUT_MS}, got {raw:?}."
        )),
    }
}

fn resolve_llm_body_timeout_ms() -> Result<Option<std::time::Duration>, String> {
    #[cfg(test)]
    if let Some(scoped) = body_timeout_override::current() {
        return scoped;
    }
    parse_llm_body_timeout_ms(std::env::var(LLM_BODY_TIMEOUT_ENV).ok().as_deref())
}

#[cfg(test)]
mod body_timeout_override {
    use std::cell::RefCell;

    type Resolved = Result<Option<std::time::Duration>, String>;

    thread_local! {
        static SLOT: RefCell<Option<Resolved>> = const { RefCell::new(None) };
    }

    pub(super) fn current() -> Option<Resolved> {
        SLOT.with(|slot| slot.borrow().clone())
    }

    pub(super) struct Scoped;

    impl Scoped {
        pub(super) fn set(value: Resolved) -> Self {
            SLOT.with(|slot| *slot.borrow_mut() = Some(value));
            Self
        }
    }

    impl Drop for Scoped {
        fn drop(&mut self) {
            SLOT.with(|slot| *slot.borrow_mut() = None);
        }
    }
}

fn resolve_llm_headers_timeout_ms() -> Result<Option<std::time::Duration>, String> {
    #[cfg(test)]
    if let Some(scoped) = headers_timeout_override::current() {
        return scoped;
    }
    parse_llm_headers_timeout_ms(std::env::var(LLM_HEADERS_TIMEOUT_ENV).ok().as_deref())
}

#[cfg(test)]
mod headers_timeout_override {
    use std::cell::RefCell;

    type Resolved = Result<Option<std::time::Duration>, String>;

    thread_local! {
        static SLOT: RefCell<Option<Resolved>> = const { RefCell::new(None) };
    }

    pub(super) fn current() -> Option<Resolved> {
        SLOT.with(|slot| slot.borrow().clone())
    }

    pub(super) struct Scoped;

    impl Scoped {
        pub(super) fn set(value: Resolved) -> Self {
            SLOT.with(|slot| *slot.borrow_mut() = Some(value));
            Self
        }
    }

    impl Drop for Scoped {
        fn drop(&mut self) {
            SLOT.with(|slot| *slot.borrow_mut() = None);
        }
    }
}

fn dump_raw_frame(protocol: &str, data: &str) {
    use std::io::Write;
    let Ok(path) = std::env::var("KIMI_SSE_DUMP") else {
        return;
    };
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "[{protocol}] {data}");
    }
}

pub type EventSink = Arc<dyn Fn(serde_json::Value) + Send + Sync>;

pub type AuthTokenProvider =
    Arc<dyn Fn(bool) -> BoxFuture<'static, Result<String, String>> + Send + Sync>;

enum Accumulator {
    OpenAI(openai::StreamAccumulator),
    Responses(openai_responses::StreamAccumulator),
    Anthropic(anthropic::StreamAccumulator),
    Google(google_genai::StreamAccumulator),
}

impl Accumulator {
    fn feed(&mut self, value: &serde_json::Value) -> Option<StreamDelta> {
        match self {
            Self::OpenAI(acc) => acc.feed(value),
            Self::Responses(acc) => acc.feed(value),
            Self::Anthropic(acc) => acc.feed(value),
            Self::Google(acc) => acc.feed(value),
        }
    }

    fn take_tool_call_deltas(&mut self) -> Vec<StreamDelta> {
        match self {
            Self::OpenAI(acc) => acc.take_tool_call_deltas(),
            Self::Responses(acc) => acc.take_tool_call_deltas(),
            Self::Anthropic(acc) => acc.take_tool_call_deltas(),
            Self::Google(acc) => acc.take_tool_call_deltas(),
        }
    }

    fn finish(self) -> LLMChatResponse {
        match self {
            Self::OpenAI(acc) => acc.finish(),
            Self::Responses(acc) => acc.finish(),
            Self::Anthropic(acc) => acc.finish(),
            Self::Google(acc) => acc.finish(),
        }
    }

    fn detected_reasoning_key(&self) -> Option<&'static str> {
        match self {
            Self::OpenAI(acc) => acc.detected_reasoning_key(),
            _ => None,
        }
    }

    fn take_error(&mut self) -> Option<String> {
        match self {
            Self::Responses(acc) => acc.take_error(),
            _ => None,
        }
    }
}

pub(crate) static SHARED_HTTP_CLIENT: once_cell::sync::Lazy<reqwest::Client> =
    once_cell::sync::Lazy::new(|| {
        reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(15))
            .build()
            .unwrap_or_default()
    });

pub struct NativeHttpLlm {
    config: NativeLlmConfig,
    system_prompt: String,
    client: reqwest::Client,
    sink: Option<EventSink>,

    auth: Option<AuthTokenProvider>,

    cached_token: std::sync::Mutex<Option<String>>,

    fetch_gate: tokio::sync::Mutex<()>,

    tool_call_ids: std::sync::Mutex<Option<Arc<kimi_core::tool_call_id::ToolCallIdNormalizer>>>,

    stream_agent_id: std::sync::Mutex<Option<String>>,

    turn_watch: std::sync::Mutex<Option<Arc<kimi_core::watch::TurnWatchState>>>,

    cache_key: Option<String>,
    last_trace_id: std::sync::Mutex<Option<String>>,

    interrupted_response: std::sync::Mutex<Option<LLMChatResponse>>,

    request_options: RequestOptions,
}

static DETECTED_REASONING_KEYS: once_cell::sync::Lazy<
    std::sync::Mutex<std::collections::HashMap<String, String>>,
> = once_cell::sync::Lazy::new(Default::default);

fn detected_reasoning_key_for(base_url: &str, model: &str) -> Option<String> {
    DETECTED_REASONING_KEYS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&format!("{base_url}#{model}"))
        .cloned()
}

fn record_detected_reasoning_key(base_url: &str, model: &str, key: &str) {
    DETECTED_REASONING_KEYS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(format!("{base_url}#{model}"))
        .or_insert_with(|| key.to_string());
}

fn thinking_is_on(config: &NativeLlmConfig) -> bool {
    config
        .reasoning_effort
        .as_deref()
        .is_some_and(|effort| !effort.is_empty() && effort != "off" && effort != "none")
        || config.thinking_budget.is_some_and(|budget| budget > 0)
}

impl NativeHttpLlm {
    pub fn new(mut config: NativeLlmConfig, system_prompt: String) -> Self {
        let system_prompt = config.system_prompt.clone().unwrap_or(system_prompt);

        if config.reasoning_effort.is_none() {
            config.reasoning_effort = config.off_effort.clone();
        }

        if config.thinking_keep.is_none()
            && thinking_is_on(&config)
            && (config.protocol == "anthropic"
                || config
                    .provider_type
                    .as_deref()
                    .is_some_and(|value| value.eq_ignore_ascii_case("kimi")))
        {
            config.thinking_keep = Some("all".to_string());
        }
        let client = SHARED_HTTP_CLIENT.clone();
        Self {
            config,
            system_prompt,
            client,
            sink: None,
            auth: None,
            cached_token: std::sync::Mutex::new(None),
            fetch_gate: tokio::sync::Mutex::new(()),
            tool_call_ids: std::sync::Mutex::new(None),
            stream_agent_id: std::sync::Mutex::new(None),
            turn_watch: std::sync::Mutex::new(None),
            cache_key: None,
            last_trace_id: std::sync::Mutex::new(None),
            interrupted_response: std::sync::Mutex::new(None),
            request_options: RequestOptions::default(),
        }
    }

    fn tool_call_ledger(&self) -> Option<Arc<kimi_core::tool_call_id::ToolCallIdNormalizer>> {
        self.tool_call_ids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn with_sink(mut self, sink: EventSink) -> Self {
        self.sink = Some(sink);
        self
    }

    pub fn with_auth_provider(mut self, auth: AuthTokenProvider) -> Self {
        self.auth = Some(auth);
        self
    }

    pub fn with_cache_key(mut self, key: Option<String>) -> Self {
        self.cache_key = key.filter(|value| !value.is_empty());
        self
    }

    pub fn with_request_options(mut self, options: RequestOptions) -> Self {
        self.request_options = options;
        self
    }

    fn completion_cap(&self, params: &LLMChatParams) -> Option<u32> {
        if let Some(requested) = params.effective_max_output_tokens() {
            let requested = requested.max(1);
            let capped = match (params.max_context_tokens, params.used_context_tokens) {
                (Some(window), Some(used)) if window > 0 => {
                    requested.min(window.saturating_sub(used).max(1))
                }
                _ => requested,
            };
            return Some(capped);
        }
        let is_kimi = self
            .config
            .provider_type
            .as_deref()
            .is_some_and(|value| value.eq_ignore_ascii_case("kimi"));
        let Some(requested) = self.config.max_tokens else {
            let kimi_unset_budget = is_kimi
                && matches!(
                    self.config.protocol.as_str(),
                    "openai" | "openai_responses" | "openai-responses"
                );
            if !kimi_unset_budget {
                return None;
            }
            let (Some(window), Some(used)) =
                (params.max_context_tokens, params.used_context_tokens)
            else {
                return None;
            };
            if window == 0 {
                return None;
            }
            return Some(window.saturating_sub(used).max(1));
        };

        if requested == 0 {
            return None;
        }
        let cap = match (params.max_context_tokens, params.used_context_tokens) {
            (Some(window), Some(used)) if window > 0 => requested.min(window.saturating_sub(used)),
            _ => requested,
        };
        Some(cap.max(1))
    }

    fn openai_completion_cap(&self, params: &LLMChatParams) -> Option<u32> {
        if self.config.max_tokens.is_some() {
            return self.completion_cap(params);
        }
        let remaining = match (params.max_context_tokens, params.used_context_tokens) {
            (Some(window), Some(used)) if window > 0 => Some(window.saturating_sub(used)),
            _ => None,
        };
        self.completion_cap(params).or_else(|| {
            family_default_completion_cap(
                &self.config.model,
                self.config.reasoning_effort.as_deref(),
                remaining,
            )
        })
    }

    fn wire_messages(&self, params: &LLMChatParams) -> Vec<WireMessage> {
        let policy = match self.config.protocol.as_str() {
            "openai_responses" | "openai-responses" => Some(ToolCallIdPolicy::Responses),

            "google" | "google-genai" | "gemini" => None,
            _ => Some(ToolCallIdPolicy::Chat),
        };
        let Some(policy) = policy else {
            return to_wire(&params.messages);
        };
        let mut messages = params.messages.to_vec();
        normalize_tool_call_ids(&mut messages, policy, 64);
        to_wire(&messages)
    }

    fn beta_messages_api(&self) -> bool {
        self.config.beta_api
            || self
                .config
                .thinking_keep
                .as_deref()
                .is_some_and(|keep| !keep.is_empty())
    }

    fn endpoint(&self) -> String {
        let base = self.config.base_url.trim_end_matches('/');
        match self.config.protocol.as_str() {
            "anthropic" => {
                if self.beta_messages_api() {
                    format!("{base}/messages?beta=true")
                } else {
                    format!("{base}/messages")
                }
            }
            "openai_responses" | "openai-responses" => format!("{base}/responses"),
            "google" | "google-genai" | "gemini" => {
                let base = google_api_base(base);
                format!(
                    "{base}/models/{}:streamGenerateContent?alt=sse",
                    google_model_id(&self.config.model)
                )
            }
            _ => format!("{base}/chat/completions"),
        }
    }

    fn emit_delta(
        &self,
        delta: StreamDelta,
        ids: Option<&kimi_core::tool_call_id::ToolCallIdResponseNormalizer>,
    ) {
        if let Some(watch) = self
            .turn_watch
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            watch.note_partial();
        }

        let delta = match (delta, ids) {
            (
                StreamDelta::ToolCall {
                    id,
                    index,
                    arguments,
                },
                Some(ids),
            ) if !id.trim().is_empty() => {
                let slot = index.map(kimi_core::tool_call_id::StreamSlot::Index);
                let assigned = ids.remap_streamed_id(&id, slot.as_ref());
                StreamDelta::ToolCall {
                    id: assigned,
                    index,
                    arguments,
                }
            }
            (StreamDelta::ToolCall { .. }, _) => return,
            (delta, _) => delta,
        };
        if let Some(ref sink) = self.sink {
            let mut event = serde_json::json!({
                "type": "llm.delta",
                "part": delta.to_part(),
            });

            if let Some(agent_id) = self
                .stream_agent_id
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_deref()
            {
                event["agent_id"] = serde_json::Value::String(agent_id.to_string());
            }
            sink(event);
        }
    }

    fn emit(&self, event: serde_json::Value) {
        if let Some(ref sink) = self.sink {
            sink(event);
        }
    }

    async fn chat_impl(
        &self,
        params: LLMChatParams,
    ) -> Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>> {
        *self.last_trace_id.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.interrupted_response
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let wire = self.wire_messages(&params);
        let is_anthropic = self.config.protocol == "anthropic";
        let is_responses = matches!(
            self.config.protocol.as_str(),
            "openai_responses" | "openai-responses"
        );
        let is_google = matches!(
            self.config.protocol.as_str(),
            "google" | "google-genai" | "gemini"
        );
        let started_at = std::time::Instant::now();

        if is_google
            && let Some(rejection) = google_genai::thinking_off_rejection(
                &self.config.model,
                self.config.reasoning_effort.as_deref(),
            )
        {
            return Err(rejection.into());
        }

        let completion_cap = self.completion_cap(&params);
        let mut body = if is_anthropic {
            match anthropic::build_request_for_provider(
                &self.config.model,
                self.config
                    .max_tokens
                    .unwrap_or_else(|| anthropic::default_max_tokens_for_model(&self.config.model)),
                &wire,
                &params.tools,
                true,
                self.config.thinking_budget,
                self.config.thinking_keep.as_deref(),
                self.config.reasoning_effort.as_deref(),
                self.config.provider_type.as_deref(),
                self.cache_key.as_deref(),
                completion_cap,
                self.config.adaptive_thinking.unwrap_or(false),
                self.beta_messages_api(),
                &self.request_options,
            ) {
                Ok(body) => body,
                Err(error) => return Err(error.into()),
            }
        } else if is_responses {
            openai_responses::build_request_with_budget_and_options(
                &self.config.model,
                &wire,
                &params.tools,
                true,
                self.config.reasoning_effort.as_deref(),
                self.cache_key.as_deref(),
                completion_cap,
                &self.request_options,
            )
        } else if is_google {
            let reasoning_on = self
                .config
                .reasoning_effort
                .as_deref()
                .is_some_and(|e| !e.is_empty() && e != "off" && e != "none")
                || self.config.thinking_budget.is_some_and(|b| b > 0);
            let include_thoughts =
                reasoning_on && google_genai::model_supports_thoughts(&self.config.model);
            google_genai::build_request_for_model_with_budget(
                &wire,
                &params.tools,
                self.config.thinking_budget,
                include_thoughts,
                &self.config.model,
                self.config.reasoning_effort.as_deref(),
                completion_cap,
            )
        } else {
            let kimi_tools;
            let tools: &[ToolInfo] = if self
                .config
                .provider_type
                .as_deref()
                .is_some_and(|provider| provider.eq_ignore_ascii_case("kimi"))
            {
                kimi_tools = openai::normalize_kimi_tools(&params.tools)
                    .map_err(|error| kimi_core::error::LlmError::typed(&error, None))?;
                &kimi_tools
            } else {
                &params.tools
            };
            openai::build_request_for_provider_with_options(
                &self.config.model,
                &wire,
                tools,
                true,
                self.config.reasoning_effort.as_deref(),
                self.config.thinking_keep.as_deref(),
                self.config.reasoning_key.as_deref(),
                self.config.provider_type.as_deref(),
                self.cache_key.as_deref(),
                self.openai_completion_cap(&params),
                detected_reasoning_key_for(&self.config.base_url, &self.config.model).as_deref(),
                &self.request_options,
            )
        };

        if opencode_adapter::is_opencode_endpoint(&self.config.base_url) {
            opencode_adapter::apply(&mut body, &self.config.protocol);
        }

        let mut token = self.credential().await?;
        let mut response = self
            .send_request(&body, token.as_str(), params.cancel.as_ref())
            .await?;
        let mut status = response.status();

        if !status.is_success() && matches!(status.as_u16(), 401 | 403) && self.auth.is_some() {
            token = self.refresh_credential().await?;
            response = self
                .send_request(&body, token.as_str(), params.cancel.as_ref())
                .await?;
            status = response.status();
        }
        let t_headers = started_at.elapsed();

        let t_sent = started_at.elapsed();

        let body_timeout = resolve_llm_body_timeout_ms()?;
        if !status.is_success() {
            let header_retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(Duration::from_secs);

            let trace_id = response
                .headers()
                .get("x-trace-id")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let (brief, body_retry_after) = read_error_body(response, body_timeout).await;
            let retry_after = header_retry_after.or(body_retry_after);
            return Err(Box::new(
                kimi_core::error::LlmError::http(status.as_u16(), &brief, retry_after)
                    .with_trace_id(trace_id.as_deref()),
            ));
        }

        *self.last_trace_id.lock().unwrap_or_else(|e| e.into_inner()) = response
            .headers()
            .get("x-trace-id")
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.is_empty())
            .map(str::to_owned);

        let mut acc = if is_anthropic {
            Accumulator::Anthropic(anthropic::StreamAccumulator::new())
        } else if is_responses {
            Accumulator::Responses(openai_responses::StreamAccumulator::new())
        } else if is_google {
            Accumulator::Google(google_genai::StreamAccumulator::new())
        } else {
            Accumulator::OpenAI(
                openai::StreamAccumulator::new()
                    .with_reasoning_key(self.config.reasoning_key.as_deref()),
            )
        };

        let mut attempt = AttemptLedger::new(
            self.tool_call_ledger()
                .as_ref()
                .map(|ledger| ledger.begin_response()),
        );

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        let mut n_events: u32 = 0;

        let mut n_malformed: u32 = 0;
        let mut first_malformed: Option<String> = None;
        let mut in_band_error: Option<String> = None;
        let mut t_first_event: Option<std::time::Duration> = None;

        let mut thinking_guard = crate::llm::thinking_guard::ThinkingGuard::from_settings();
        let mut stream = response.bytes_stream().eventsource();

        while let Some(event) = tokio::select! {




            _ = cancelled_or_pending(params.cancel.as_ref()) => {
                *self
                    .interrupted_response
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(acc.finish());
                return Err(CANCELLED_MESSAGE.into());
            }
            event = async {
                match body_timeout {
                    Some(cap) => tokio::time::timeout(cap, stream.next())
                        .await
                        .map_err(|_| {
                            format!(
                                "llm sse decode error: body timeout after {}ms",
                                cap.as_millis()
                            )
                        }),
                    None => Ok(stream.next().await),
                }
            } => match event {
                Ok(event) => event,
                Err(message) => return Err(message.into()),
            },
        } {
            let event = event.map_err(|e| format!("llm sse decode error: {e}"))?;
            if event.data == "[DONE]"
                || event.event == "message_stop"
                || event.event == "response.done"
            {
                break;
            }
            let value: serde_json::Value = match serde_json::from_str(&event.data) {
                Ok(v) => v,

                Err(_) => {
                    n_malformed += 1;
                    if first_malformed.is_none() {
                        first_malformed = Some(truncate_for_diagnosis(&event.data));
                    }
                    continue;
                }
            };
            dump_raw_frame(&self.config.protocol, &event.data);
            if let Some(message) = crate::native::extract_in_band_error(&value) {
                in_band_error = Some(message);
                break;
            }
            n_events += 1;
            if t_first_event.is_none() {
                t_first_event = Some(started_at.elapsed());
            }
            if let Some(event_type) = value.get("type").and_then(|t| t.as_str())
                && (event_type == "message_stop" || event_type == "response.done")
            {
                break;
            }
            if let Some(delta) = acc.feed(&value)
                && let Some(delta) =
                    crate::llm::thinking_guard::gate_delta(&mut thinking_guard, delta)
            {
                self.emit_delta(delta, attempt.ids());
            }
            for delta in acc.take_tool_call_deltas() {
                self.emit_delta(delta, attempt.ids());
            }
        }

        if let Some(message) = in_band_error {
            return Err(format!("llm provider stream error: {message}").into());
        }

        let t_ended = started_at.elapsed();

        if let Some(message) = acc.take_error() {
            return Err(format!("llm provider stream error: {message}").into());
        }

        tokio::spawn(async move {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while let Some(Ok(_)) = stream.next().await {}
            })
            .await;
        });

        if std::env::var_os("KIMI_HTTP_TIMING").is_some_and(|v| v == "1") {
            eprintln!(
                "[native-http-timing] total={}ms headers={}ms first_event={}ms events={} model={}",
                started_at.elapsed().as_millis(),
                t_headers.as_millis(),
                t_first_event.map(|d| d.as_millis()).unwrap_or(0),
                n_events,
                self.config.model
            );
        }

        let observed_reasoning_key = acc.detected_reasoning_key();
        let mut response = acc.finish();
        if let Some(key) = observed_reasoning_key {
            record_detected_reasoning_key(&self.config.base_url, &self.config.model, key);
        }

        if let Some(ids) = attempt.ids() {
            kimi_core::tool_call_id::remap_response_tool_calls(&mut response, ids);
        }

        if n_events == 0 {
            return Err(
                empty_stream_error(&content_type, n_malformed, first_malformed.as_deref()).into(),
            );
        }
        if response.content.is_empty()
            && response.tool_calls.is_empty()
            && response.finish_reason.is_none()
        {
            return Err(format!(
                "llm stream ended without content, tool calls or a finish reason ({n_events} event(s), {n_malformed} unparsable frame(s), content-type \"{content_type}\"). The endpoint answered 200 but nothing usable was decoded."
            )
            .into());
        }

        let timing = crate::llm::LlmTiming::from_marks(
            0,
            Some(t_sent.as_millis().min(i64::MAX as u128) as i64),
            t_first_event.map(|d| d.as_millis().min(i64::MAX as u128) as i64),
            t_ended.as_millis().min(i64::MAX as u128) as i64,
        );
        let timing = (!timing.is_empty()).then_some(timing);
        self.emit(serde_json::json!({
            "type": "llm.step.end",
            "content": response.content,
            "tool_calls": response.tool_calls.iter().map(|tc| serde_json::json!({
                "id": tc.id,
                "name": tc.name,
                "arguments": tc.arguments,
            })).collect::<Vec<_>>(),
            "finish_reason": response.finish_reason,
            "latency_ms": started_at.elapsed().as_millis().min(u64::MAX as u128) as u64,
            "timing": serde_json::to_value(&timing).unwrap_or(serde_json::Value::Null),
            "usage": {
                "input_tokens": response.usage.input_tokens,
                "output_tokens": response.usage.output_tokens,
                "total_tokens": response.usage.total_tokens,
                "input_cache_read": response.usage.input_cache_read,
                "input_cache_creation": response.usage.input_cache_creation,
            },
        }));

        attempt.commit();
        response.timing = timing;
        Ok(response)
    }

    async fn send_request(
        &self,
        body: &serde_json::Value,
        token: &str,
        cancel: Option<&tokio_util::sync::CancellationToken>,
    ) -> Result<reqwest::Response, String> {
        let is_google = matches!(
            self.config.protocol.as_str(),
            "google" | "google-genai" | "gemini"
        );
        let is_anthropic = self.config.protocol == "anthropic";
        let mut req = self.client.post(self.endpoint()).json(body);
        if is_google {
            if token.starts_with("ya29.") || self.config.auth_provider.is_some() {
                req = req.header("authorization", format!("Bearer {token}"));
            } else {
                req = req.header("x-goog-api-key", token);
            }
        } else if is_anthropic {
            req = req
                .header("x-api-key", token)
                .header("anthropic-version", "2023-06-01");

            if !self.beta_messages_api() {
                let betas = anthropic::anthropic_betas(
                    self.config.provider_type.as_deref(),
                    self.config.adaptive_thinking.unwrap_or(false),
                    self.config.reasoning_effort.is_some() || self.config.thinking_budget.is_some(),
                    self.config.thinking_keep.as_deref(),
                );
                if !betas.is_empty() {
                    req = req.header("anthropic-beta", betas.join(","));
                }
            }
        } else {
            req = req.header("authorization", format!("Bearer {token}"));
        }
        for (k, v) in &self.config.custom_headers {
            req = req.header(k.as_str(), v.as_str());
        }

        let send = req.send();

        let headers_cap = resolve_llm_headers_timeout_ms()?;
        let send = async move {
            let result = match headers_cap {
                Some(cap) => match tokio::time::timeout(cap, send).await {
                    Ok(result) => result,
                    Err(_elapsed) => {
                        return Err(format!(
                            "llm transport error headers_timeout: no response headers within {}ms",
                            cap.as_millis()
                        ));
                    }
                },
                None => send.await,
            };
            result.map_err(|e| format!("{}: {e}", transport_error_message(&e)))
        };
        match cancel {
            Some(cancel) => tokio::select! {
                _ = cancel.cancelled() => Err(CANCELLED_MESSAGE.into()),
                sent = send => sent,
            },
            None => send.await,
        }
    }

    async fn credential(&self) -> Result<String, String> {
        if let Some(env_name) = self.config.api_key_env.as_deref() {
            if !self.config.api_key.trim().is_empty() || self.config.auth_provider.is_some() {
                return Err("config.invalid: api_key_env is mutually exclusive with api_key and auth_provider".into());
            }
            return std::env::var(env_name)
                .ok()
                .filter(|key| !key.trim().is_empty())
                .ok_or_else(|| format!(
                    "config.invalid: api_key_env {env_name:?} environment variable is not set or is empty (a Unicode value is required)"
                ));
        }
        if self.config.auth_provider.is_some() {
            let fetch = self.auth.as_ref().ok_or_else(|| {
                format!(
                    "native transport names auth_provider {:?} but no token channel is wired",
                    self.config.auth_provider
                )
            })?;

            if let Some(token) = self.cached_token_value() {
                return Ok(token);
            }

            let _gate = self.fetch_gate.lock().await;
            if let Some(token) = self.cached_token_value() {
                return Ok(token);
            }
            return self.store_token(fetch(false).await?);
        }
        Ok(self.config.api_key.clone())
    }

    async fn refresh_credential(&self) -> Result<String, String> {
        match &self.auth {
            Some(fetch) => {
                let _gate = self.fetch_gate.lock().await;
                self.store_token(fetch(true).await?)
            }
            None => Ok(self.config.api_key.clone()),
        }
    }

    fn cached_token_value(&self) -> Option<String> {
        self.cached_token
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn store_token(&self, token: String) -> Result<String, String> {
        *self.cached_token.lock().unwrap_or_else(|e| e.into_inner()) = Some(token.clone());
        Ok(token)
    }
}

pub fn google_api_base(base: &str) -> String {
    if url_has_api_version(base) {
        return base.to_string();
    }
    format!("{base}/v1beta")
}

pub fn url_has_api_version(url: &str) -> bool {
    url.split('/').any(is_api_version_segment)
}

pub(crate) fn google_model_id(model: &str) -> &str {
    model.rsplit('/').next().unwrap_or(model)
}

fn is_api_version_segment(segment: &str) -> bool {
    let Some(rest) = segment.strip_prefix(['v', 'V']) else {
        return false;
    };
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    digits > 0 && rest[digits..].chars().all(|c| c.is_ascii_alphabetic())
}

const ERROR_BODY_MAX_BYTES: usize = 16 * 1024;

const CANCELLED_MESSAGE: &str = "llm cancelled: request aborted";

pub fn is_cancelled_error(error: &str) -> bool {
    error.starts_with(CANCELLED_MESSAGE)
}

pub fn is_retryable_status_code(code: u16) -> bool {
    matches!(code, 408 | 409 | 425 | 429 | 500 | 502 | 503 | 504 | 529)
}

const NOT_AN_SSE_ENDPOINT_PREFIX: &str = crate::native::NOT_AN_SSE_ENDPOINT_PREFIX;

fn truncate_for_diagnosis(text: &str) -> String {
    crate::native::truncate_for_diagnosis(text)
}

fn empty_stream_error(
    content_type: &str,
    n_malformed: u32,
    first_malformed: Option<&str>,
) -> String {
    crate::native::empty_stream_error(content_type, n_malformed, first_malformed)
}

async fn cancelled_or_pending(cancel: Option<&tokio_util::sync::CancellationToken>) {
    match cancel {
        Some(cancel) => cancel.cancelled().await,
        None => std::future::pending::<()>().await,
    }
}

fn transport_error_message(e: &reqwest::Error) -> String {
    let kind = if e.is_builder() {
        "invalid_request"
    } else if e.is_timeout() {
        "timeout"
    } else if e.is_connect() {
        "connect"
    } else if e.is_decode() || e.is_body() {
        "decode"
    } else {
        "transport"
    };
    format!("llm transport error {kind}")
}

async fn read_error_body(
    response: reqwest::Response,
    body_timeout: Option<Duration>,
) -> (String, Option<Duration>) {
    let mut stream = response;
    let mut buf: Vec<u8> = Vec::new();
    let mut unreadable: Option<String> = None;
    while buf.len() < ERROR_BODY_MAX_BYTES {
        let next = match body_timeout {
            Some(cap) => match tokio::time::timeout(cap, stream.chunk()).await {
                Ok(next) => next,
                Err(_elapsed) => {
                    unreadable = Some(format!("read timed out after {}ms", cap.as_millis()));
                    break;
                }
            },
            None => stream.chunk().await,
        };
        match next {
            Ok(Some(chunk)) => buf.extend_from_slice(&chunk),
            Ok(None) => break,
            Err(error) => {
                unreadable = Some(error.to_string());
                break;
            }
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let retry_after = parse_google_retry_info_delay(&text);
    let mut brief: String = text.chars().take(500).collect();
    if let Some(reason) = unreadable {
        brief.push_str(&format!(" [body unreadable: {reason}]"));
    }
    (brief, retry_after)
}

fn parse_google_retry_info_delay(body: &str) -> Option<Duration> {
    let json_start = body.find('{')?;
    let parsed: serde_json::Value = serde_json::from_str(&body[json_start..]).ok()?;
    let details = parsed.get("error")?.get("details")?.as_array()?;
    for detail in details {
        let type_url = detail.get("@type").and_then(|v| v.as_str()).unwrap_or("");
        if !type_url.ends_with("google.rpc.RetryInfo") {
            continue;
        }
        let Some(delay) = detail.get("retryDelay").and_then(|v| v.as_str()) else {
            continue;
        };
        if let Some(wait) = parse_protobuf_duration(delay) {
            return Some(wait);
        }
    }
    None
}

fn parse_protobuf_duration(text: &str) -> Option<Duration> {
    let seconds: f64 = text.trim().strip_suffix('s')?.parse().ok()?;
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    Some(Duration::from_secs_f64(seconds))
}

fn is_quota_exhaustion_error(error: &str) -> bool {
    const QUOTA_MARKERS: &[&str] = &[
        "exceeded_current_quota_error",
        "insufficient_quota",
        "exceeded your current quota",
        "exceeded your current token quota",
        "check your account balance",
        "insufficient balance",
        "recharge your account",
        "please recharge",
        "account is in arrears",
        "account in arrears",
    ];
    let lower = error.to_lowercase();
    QUOTA_MARKERS.iter().any(|m| lower.contains(m))
}

struct AttemptLedger {
    ids: Option<kimi_core::tool_call_id::ToolCallIdResponseNormalizer>,
    committed: bool,
}

impl AttemptLedger {
    fn new(ids: Option<kimi_core::tool_call_id::ToolCallIdResponseNormalizer>) -> Self {
        Self {
            ids,
            committed: false,
        }
    }

    fn ids(&self) -> Option<&kimi_core::tool_call_id::ToolCallIdResponseNormalizer> {
        self.ids.as_ref()
    }

    fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for AttemptLedger {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if let Some(ids) = &self.ids {
            ids.rollback();
        }
    }
}

impl LLM for NativeHttpLlm {
    fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    fn set_tool_call_ids(&self, ledger: Arc<kimi_core::tool_call_id::ToolCallIdNormalizer>) {
        *self.tool_call_ids.lock().unwrap_or_else(|e| e.into_inner()) = Some(ledger);
    }

    fn set_stream_agent_id(&self, agent_id: &str) {
        *self
            .stream_agent_id
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(agent_id.to_string());
    }

    fn set_turn_watch(&self, watch: Option<Arc<kimi_core::watch::TurnWatchState>>) {
        *self.turn_watch.lock().unwrap_or_else(|e| e.into_inner()) = watch;
    }

    fn take_interrupted_response(&self) -> Option<LLMChatResponse> {
        self.interrupted_response
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    fn model_name(&self) -> &str {
        &self.config.model
    }

    fn transport(&self) -> &'static str {
        "native-http"
    }

    fn model_alias(&self) -> Option<&str> {
        self.config.alias.as_deref()
    }

    fn provider_type(&self) -> Option<&str> {
        self.config.provider_type.as_deref()
    }

    fn protocol(&self) -> Option<&str> {
        Some(self.config.protocol.as_str())
    }

    fn thinking_effort(&self) -> Option<&str> {
        self.config.reasoning_effort.as_deref()
    }

    fn request_parameters(&self) -> Option<kimi_core::types::LlmRequestParameters> {
        Some(kimi_core::types::LlmRequestParameters {
            temperature: self.request_options.temperature,
            top_p: self.request_options.top_p,
            beta_api: Some(self.beta_messages_api()),
            thinking_keep: self.config.thinking_keep.clone(),
        })
    }

    fn effective_max_tokens(&self, params: &LLMChatParams) -> Option<u32> {
        if self.config.protocol == "anthropic" {
            let is_kimi = self
                .config
                .provider_type
                .as_deref()
                .is_some_and(|value| value.eq_ignore_ascii_case("kimi"));
            let max_tokens = self.config.max_tokens.unwrap_or_else(|| {
                crate::llm::anthropic::default_max_tokens_for_model(&self.config.model)
            });
            return Some(crate::llm::anthropic::effective_max_tokens(
                &self.config.model,
                max_tokens,
                self.completion_cap(params),
                is_kimi,
            ));
        }
        if matches!(
            self.config.protocol.as_str(),
            "openai_responses" | "openai-responses" | "google" | "google-genai" | "gemini"
        ) {
            return self.completion_cap(params);
        }
        self.openai_completion_cap(params)
    }

    fn last_trace_id(&self) -> Option<String> {
        self.last_trace_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn media_target(&self) -> Option<crate::llm::media_resolver::MediaTarget> {
        let capabilities = self.config.capabilities.as_deref().unwrap_or_default();
        let declares = |name: &str| capabilities.iter().any(|c| c == name);
        Some(crate::llm::media_resolver::MediaTarget {
            protocol: self.config.protocol.clone(),
            base_url: self.config.base_url.clone(),
            provider_key: self
                .config
                .auth_provider
                .clone()
                .unwrap_or_else(|| self.config.protocol.clone()),
            provider_type: self.config.provider_type.clone().unwrap_or_default(),

            image_in: capabilities.is_empty() || declares("image_in"),
            video_in: capabilities.is_empty() || declares("video_in"),
            audio_in: capabilities.is_empty() || declares("audio_in"),

            uploads_media: self.config.auth_provider.is_some(),

            inline_video: matches!(
                self.config.protocol.as_str(),
                "google" | "google-genai" | "gemini"
            ),
        })
    }

    fn media_upload_credential(&self) -> Option<crate::llm::files_upload::CredentialFuture<'_>> {
        self.config.auth_provider.as_ref()?;
        Some(Box::pin(async move { self.credential().await }))
    }

    fn is_retryable_error(&self, error: &str) -> bool {
        if is_cancelled_error(error) {
            return false;
        }

        if error.starts_with(NOT_AN_SSE_ENDPOINT_PREFIX) {
            return false;
        }
        if error.starts_with("llm http status ") {
            let code = kimi_core::error::llm_http_status(error).unwrap_or(0);

            if code == 429 && is_quota_exhaustion_error(error) {
                return false;
            }
            return is_retryable_status_code(code);
        }

        if let Some(rest) = error.strip_prefix("llm transport error ") {
            return !rest.starts_with("invalid_request");
        }

        if error.starts_with("config.invalid:") {
            return false;
        }

        const RETRYABLE: &[&str] = &[
            "overloaded",
            "timed out",
            "timeout",
            "connect",
            "connection",
            "sse decode error",
            "produced no events",
        ];
        let lower = error.to_lowercase();
        if is_quota_exhaustion_error(&lower) {
            return false;
        }
        RETRYABLE.iter().any(|s| lower.contains(s))
    }

    fn chat(
        &self,
        params: LLMChatParams,
    ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>> {
        Box::pin(async move { self.chat_impl(params).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kimi_core::types::LLMMessage;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn google_base_url_gets_a_version_segment() {
        assert_eq!(
            google_api_base("https://generativelanguage.googleapis.com"),
            "https://generativelanguage.googleapis.com/v1beta"
        );
        assert_eq!(
            google_api_base("http://127.0.0.1:3001"),
            "http://127.0.0.1:3001/v1beta"
        );
        assert_eq!(
            google_api_base("http://107.173.87.151:8045"),
            "http://107.173.87.151:8045/v1beta"
        );
    }

    #[test]
    fn google_base_url_keeps_an_explicit_version_segment() {
        assert_eq!(
            google_api_base("https://generativelanguage.googleapis.com/v1beta"),
            "https://generativelanguage.googleapis.com/v1beta"
        );
        assert_eq!(
            google_api_base("http://127.0.0.1:3001/v1"),
            "http://127.0.0.1:3001/v1"
        );
        assert_eq!(
            google_api_base("https://proxy.example.com/v2alpha"),
            "https://proxy.example.com/v2alpha"
        );
    }

    #[test]
    fn google_model_id_strips_a_provider_prefix() {
        assert_eq!(
            google_model_id("zhongzhuan/gemini-3.8-flash-high"),
            "gemini-3.8-flash-high"
        );
        assert_eq!(google_model_id("gemini-2.5-pro"), "gemini-2.5-pro");
        assert_eq!(google_model_id("models/gemini-2.5-pro"), "gemini-2.5-pro");
    }

    #[test]
    fn a_non_version_segment_is_not_mistaken_for_one() {
        assert_eq!(
            google_api_base("https://x.example.com/vertex"),
            "https://x.example.com/vertex/v1beta"
        );
    }

    #[test]
    fn google_retry_info_delay_is_read_from_the_error_details() {
        let body = r#"{"error":{"code":429,"message":"Resource exhausted","details":[
            {"@type":"type.googleapis.com/google.rpc.Help","links":[]},
            {"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"7s"}]}}"#;
        assert_eq!(
            parse_google_retry_info_delay(body),
            Some(Duration::from_secs(7))
        );

        let fractional = r#"{"error":{"details":[{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"1.5s"}]}}"#;
        assert_eq!(
            parse_google_retry_info_delay(fractional),
            Some(Duration::from_millis(1500))
        );

        let prefixed = r#"{"error":{"details":[{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"3s"}]}}"#;
        assert_eq!(
            parse_google_retry_info_delay(prefixed),
            Some(Duration::from_secs(3))
        );
    }

    #[test]
    fn google_retry_info_absent_or_unreadable_yields_nothing() {
        assert_eq!(
            parse_google_retry_info_delay("<html>502 Bad Gateway</html>"),
            None
        );

        assert_eq!(
            parse_google_retry_info_delay(r#"{"error":{"code":500,"message":"boom"}}"#),
            None
        );

        assert_eq!(
            parse_google_retry_info_delay(
                r#"{"error":{"details":[{"@type":"type.googleapis.com/google.rpc.Help"}]}}"#
            ),
            None
        );

        assert_eq!(
            parse_google_retry_info_delay(
                r#"{"error":{"details":[{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"soon"}]}}"#
            ),
            None
        );
        assert_eq!(
            parse_google_retry_info_delay(
                r#"{"error":{"details":[{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"-1s"}]}}"#
            ),
            None
        );
    }

    #[test]
    fn protobuf_duration_parsing_keeps_sub_second_precision() {
        assert_eq!(parse_protobuf_duration("7s"), Some(Duration::from_secs(7)));
        assert_eq!(
            parse_protobuf_duration(" 1.5s "),
            Some(Duration::from_millis(1500))
        );
        assert_eq!(parse_protobuf_duration("0s"), Some(Duration::ZERO));

        assert_eq!(parse_protobuf_duration("7"), None);
        assert_eq!(parse_protobuf_duration("s"), None);
        assert_eq!(parse_protobuf_duration("infinity"), None);
    }

    #[tokio::test]
    async fn streamed_tool_call_fragments_share_the_finalized_id() {
        let (addr, server) = spawn_fixed_200_server(
            "text/event-stream",
            concat!(
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"Read\",\"arguments\":\"{\\\"pa\"}}]}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"th\\\":\\\"a.txt\\\"}\"}}]}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
                "data: [DONE]\n\n",
            ),
        )
        .await;

        let frames = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = frames.clone();
        let llm = NativeHttpLlm::new(
            config("openai", &format!("http://{addr}/v1")),
            String::new(),
        )
        .with_sink(Arc::new(move |event| {
            sink.lock().unwrap().push(event);
        }));

        let ledger = Arc::new(kimi_core::tool_call_id::ToolCallIdNormalizer::new());
        ledger.seed_from(&[kimi_core::types::LLMMessage {
            role: "assistant".into(),
            content: String::new(),
            blocks: Vec::new(),
            tool_calls: vec![kimi_core::types::ToolCall {
                id: "call_1".into(),
                name: "Read".into(),
                arguments: serde_json::json!({}),
                extras: None,
                args_parse_error: None,
            }],
            tool_call_id: None,
            tools: Vec::new(),
            prompt_id: None,
            origin: None,
        }]);
        llm.set_tool_call_ids(Arc::clone(&ledger));

        let response = chat_once(&llm).await.expect("the stream succeeds");
        server.abort();

        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(
            response.tool_calls[0].id, "call_1__2",
            "the finalized call must carry the disambiguated id"
        );
        let parts: Vec<serde_json::Value> = frames
            .lock()
            .unwrap()
            .iter()
            .filter_map(|frame| frame.get("part").cloned())
            .filter(|part| part.get("type").and_then(|t| t.as_str()) == Some("tool_call"))
            .collect();
        assert_eq!(parts.len(), 2, "both fragments must be forwarded");
        for part in &parts {
            assert_eq!(
                part.get("id").and_then(|v| v.as_str()),
                Some("call_1__2"),
                "a fragment must carry the id its call enters history with"
            );
        }
        assert_eq!(
            parts[0].get("arguments").and_then(|v| v.as_str()),
            Some("{\"pa")
        );
        assert_eq!(
            parts[1].get("arguments").and_then(|v| v.as_str()),
            Some("th\":\"a.txt\"}")
        );
    }

    #[tokio::test]
    async fn failed_request_releases_the_tool_call_ids_it_streamed() {
        let (addr, server) = spawn_fixed_200_server(
            "text/event-stream",
            concat!(
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"Read\",\"arguments\":\"{\"}}]}}]}\n\n",
                "data: {\"error\":{\"message\":\"upstream exploded\"}}\n\n",
            ),
        )
        .await;
        let llm = NativeHttpLlm::new(
            config("openai", &format!("http://{addr}/v1")),
            String::new(),
        );
        let ledger = Arc::new(kimi_core::tool_call_id::ToolCallIdNormalizer::new());
        llm.set_tool_call_ids(Arc::clone(&ledger));

        chat_once(&llm)
            .await
            .expect_err("the in-band error must fail the request");
        server.abort();

        assert!(
            !ledger.is_taken("call_1"),
            "the failed attempt must hand its claim back"
        );
        assert_eq!(
            ledger.begin_response().remap_streamed_id("call_1", None),
            "call_1",
            "the retry must reuse the provider's raw id"
        );
    }

    #[test]
    fn attempt_ledger_commits_or_releases() {
        let ledger = Arc::new(kimi_core::tool_call_id::ToolCallIdNormalizer::new());

        let mut committed = AttemptLedger::new(Some(ledger.begin_response()));
        assert_eq!(
            committed.ids().unwrap().remap_streamed_id("call_1", None),
            "call_1"
        );
        committed.commit();
        drop(committed);
        assert!(
            ledger.is_taken("call_1"),
            "a committed attempt keeps its ids"
        );

        {
            let attempt = AttemptLedger::new(Some(ledger.begin_response()));
            assert_eq!(
                attempt.ids().unwrap().remap_streamed_id("call_2", None),
                "call_2"
            );
        }
        assert!(
            !ledger.is_taken("call_2"),
            "a dropped attempt releases its ids"
        );

        assert_eq!(
            ledger.begin_response().remap_streamed_id("call_1", None),
            "call_1__2"
        );
    }

    #[test]
    fn streamed_deltas_carry_the_installed_agent_id() {
        let llm = NativeHttpLlm::new(config("anthropic", "http://127.0.0.1:9"), String::new());
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink_seen = seen.clone();
        let llm = llm.with_sink(Arc::new(move |event| {
            sink_seen.lock().unwrap().push(event);
        }));

        llm.emit_delta(StreamDelta::Text("leak".into()), None);
        assert!(seen.lock().unwrap()[0].get("agent_id").is_none());

        llm.set_stream_agent_id("subagent-1");
        llm.emit_delta(StreamDelta::Text("body".into()), None);
        llm.emit_delta(StreamDelta::Think("reasoning".into()), None);

        let events = seen.lock().unwrap();
        assert_eq!(events[1]["agent_id"], "subagent-1");
        assert_eq!(events[1]["part"]["type"], "text");
        assert_eq!(events[2]["agent_id"], "subagent-1");
        assert_eq!(events[2]["part"]["type"], "think");
    }

    fn config(protocol: &str, base_url: &str) -> NativeLlmConfig {
        NativeLlmConfig {
            protocol: protocol.into(),
            alias: None,
            provider_type: None,
            base_url: base_url.into(),
            api_key: "test-key".into(),
            api_key_env: None,
            model: "test-model".into(),
            max_tokens: None,
            custom_headers: HashMap::new(),
            reasoning_effort: None,
            thinking_budget: None,
            auth_provider: None,
            thinking_keep: None,
            beta_api: false,
            capabilities: None,
            system_prompt: None,
            max_input_size: None,
            adaptive_thinking: None,
            reasoning_key: None,
            off_effort: None,
            response_format: None,
            sampling: None,
            determinism: None,
            extra_params: None,
            binary_path: None,
            antigravity: None,
        }
    }

    #[test]
    fn thinking_keep_defaults_to_all_where_the_wire_consumes_it() {
        let build = |provider_type: Option<&str>, protocol: &str, effort: Option<&str>, budget| {
            let mut cfg = config(protocol, "https://api.example.com/v1");
            cfg.provider_type = provider_type.map(str::to_string);
            cfg.reasoning_effort = effort.map(str::to_string);
            cfg.thinking_budget = budget;
            NativeHttpLlm::new(cfg, String::new())
        };

        assert_eq!(
            build(Some("kimi"), "openai", Some("high"), None)
                .config
                .thinking_keep
                .as_deref(),
            Some("all")
        );
        assert_eq!(
            build(Some("anthropic"), "anthropic", None, Some(32_000))
                .config
                .thinking_keep
                .as_deref(),
            Some("all"),
            "an anthropic thinking budget is thinking on"
        );
        assert_eq!(
            build(Some("kimi"), "openai", Some("off"), None)
                .config
                .thinking_keep,
            None,
            "thinking off carries no keep"
        );
        assert_eq!(
            build(Some("openai"), "openai", Some("high"), None)
                .config
                .thinking_keep,
            None,
            "a generic OpenAI wire has no keep encoding"
        );

        let mut explicit = config("openai", "https://api.example.com/v1");
        explicit.provider_type = Some("kimi".into());
        explicit.reasoning_effort = Some("high".into());
        explicit.thinking_keep = Some("1".into());
        assert_eq!(
            NativeHttpLlm::new(explicit, String::new())
                .config
                .thinking_keep
                .as_deref(),
            Some("1"),
            "an explicit keep value outranks the default"
        );
    }

    async fn spawn_capturing_server() -> (
        std::net::SocketAddr,
        tokio::task::JoinHandle<()>,
        tokio::sync::oneshot::Receiver<String>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut tx = Some(tx);
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut raw = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    raw.extend_from_slice(&buf[..n]);
                    let Some(header_end) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
                        continue;
                    };
                    let header_end = header_end + 4;
                    let headers = String::from_utf8_lossy(&raw[..header_end]).to_ascii_lowercase();
                    let content_length = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .and_then(|value| value.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if raw.len() >= header_end + content_length {
                        break;
                    }
                }
                if let Some(tx) = tx.take() {
                    let body_start = raw
                        .windows(4)
                        .position(|w| w == b"\r\n\r\n")
                        .map(|i| i + 4)
                        .unwrap_or(raw.len());
                    let _ = tx.send(String::from_utf8_lossy(&raw[body_start..]).into_owned());
                }
                let body =
                    "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n";
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        (addr, handle, rx)
    }

    #[tokio::test]
    async fn chat_puts_the_cache_key_and_completion_cap_on_the_openai_wire() {
        let (addr, server, body) = spawn_capturing_server().await;
        let mut cfg = config("openai", &format!("http://{addr}/v1"));
        cfg.model = "gpt-4o".into();
        cfg.max_tokens = Some(64_000);
        let llm = NativeHttpLlm::new(cfg, String::new()).with_cache_key(Some("sess-1".to_string()));
        let result = llm
            .chat(LLMChatParams {
                messages: Arc::from(vec![kimi_core::types::LLMMessage::user("hi")]),
                tools: Arc::from(Vec::new()),
                used_context_tokens: Some(10_000),
                max_context_tokens: Some(20_000),
                max_output_tokens: None,
                max_output_size: None,
                ..Default::default()
            })
            .await;
        assert!(result.is_ok(), "{:?}", result.err());
        let body = body.await.unwrap();
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["prompt_cache_key"], "sess-1");

        assert_eq!(json["max_tokens"], 10_000);
        server.abort();
    }

    #[tokio::test]
    async fn chat_puts_the_kimi_anthropic_trait_on_the_wire() {
        let (addr, server, body) = spawn_capturing_server().await;
        let mut cfg = config("anthropic", &format!("http://{addr}/v1"));
        cfg.model = "k3".into();
        cfg.provider_type = Some("kimi".into());
        cfg.reasoning_effort = Some("high".into());
        cfg.max_tokens = Some(64_000);
        let llm = NativeHttpLlm::new(cfg, String::new()).with_cache_key(Some("sess-2".to_string()));

        let _ = llm
            .chat(LLMChatParams {
                messages: Arc::from(vec![kimi_core::types::LLMMessage::user("hi")]),
                tools: Arc::from(Vec::new()),
                used_context_tokens: Some(10_000),
                max_context_tokens: Some(50_000),
                max_output_tokens: None,
                max_output_size: None,
                ..Default::default()
            })
            .await;
        let body = body.await.unwrap();
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["thinking"], serde_json::json!({ "type": "enabled" }));
        assert_eq!(json["output_config"]["effort"], "high");
        assert!(json["thinking"].get("budget_tokens").is_none());
        assert_eq!(json["metadata"]["user_id"], "sess-2");
        assert_eq!(json["max_tokens"], 40_000);
        server.abort();
    }

    #[test]
    fn detected_reasoning_keys_are_recorded_once_per_connection() {
        let base = "http://detected-keys.test/v1";
        assert_eq!(detected_reasoning_key_for(base, "m-record"), None);
        record_detected_reasoning_key(base, "m-record", "reasoning");
        record_detected_reasoning_key(base, "m-record", "reasoning_content");
        assert_eq!(
            detected_reasoning_key_for(base, "m-record").as_deref(),
            Some("reasoning")
        );
        assert_eq!(detected_reasoning_key_for(base, "m-other"), None);
    }

    #[test]
    fn headers_timeout_env_parses_like_v2() {
        assert_eq!(
            parse_llm_headers_timeout_ms(None).unwrap(),
            Some(DEFAULT_LLM_IO_TIMEOUT)
        );
        assert_eq!(
            parse_llm_headers_timeout_ms(Some("  ")).unwrap(),
            Some(DEFAULT_LLM_IO_TIMEOUT)
        );
        for (raw, expect_ms) in [("45000", 45_000), ("2147483647", 2_147_483_647), ("+5", 5)] {
            assert_eq!(
                parse_llm_headers_timeout_ms(Some(raw))
                    .unwrap()
                    .map(|d| d.as_millis() as u64),
                Some(expect_ms),
                "{raw}"
            );
        }
        for raw in ["0", "-5", "abc", "1.5", "2147483648"] {
            let error = parse_llm_headers_timeout_ms(Some(raw)).unwrap_err();
            assert!(
                error.starts_with(
                    "config.invalid: KIMI_CODE_LLM_HEADERS_TIMEOUT_MS must be a positive integer no greater than 2147483647,"
                ),
                "{raw}: {error}"
            );
        }
    }

    #[test]
    fn body_timeout_env_parses_like_v2() {
        assert_eq!(
            parse_llm_body_timeout_ms(None).unwrap(),
            Some(DEFAULT_LLM_IO_TIMEOUT)
        );
        assert_eq!(
            parse_llm_body_timeout_ms(Some("")).unwrap(),
            Some(DEFAULT_LLM_IO_TIMEOUT)
        );
        assert_eq!(
            parse_llm_body_timeout_ms(Some("   ")).unwrap(),
            Some(DEFAULT_LLM_IO_TIMEOUT)
        );
        assert_eq!(
            parse_llm_body_timeout_ms(Some("0")).unwrap(),
            None,
            "v2's NO_TIMEOUT_MS is an explicit no-cap, not a zero-length timeout"
        );
        for (raw, expect_ms) in [("120000", 120_000), ("1", 1), ("2147483647", 2_147_483_647)] {
            assert_eq!(
                parse_llm_body_timeout_ms(Some(raw))
                    .unwrap()
                    .map(|d| d.as_millis() as u64),
                Some(expect_ms),
                "{raw}"
            );
        }
        for raw in ["-5", "abc", "1.5", "2147483648"] {
            let error = parse_llm_body_timeout_ms(Some(raw)).unwrap_err();
            assert!(
                error.starts_with(
                    "config.invalid: KIMI_CODE_LLM_BODY_TIMEOUT_MS must be an integer between 0 and 2147483647,"
                ),
                "{raw}: {error}"
            );
        }
    }

    #[test]
    fn headers_timeout_reads_the_env_var() {
        struct EnvGuard(Option<std::ffi::OsString>);
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                unsafe {
                    match &self.0 {
                        Some(value) => std::env::set_var(LLM_HEADERS_TIMEOUT_ENV, value),
                        None => std::env::remove_var(LLM_HEADERS_TIMEOUT_ENV),
                    }
                }
            }
        }
        let _guard = EnvGuard(std::env::var_os(LLM_HEADERS_TIMEOUT_ENV));

        let _env_lock = crate::env::ENV_TEST_LOCK.blocking_lock();
        unsafe { std::env::set_var(LLM_HEADERS_TIMEOUT_ENV, "45000") };
        assert_eq!(
            resolve_llm_headers_timeout_ms()
                .unwrap()
                .map(|d| d.as_millis() as u64),
            Some(45_000)
        );
    }

    #[tokio::test]
    async fn headers_timeout_cap_and_bad_value_gate_the_wait() {
        let chat_params = || LLMChatParams {
            messages: Arc::from(vec![kimi_core::types::LLMMessage::user("hi")]),
            tools: Arc::from(Vec::new()),
            ..Default::default()
        };

        let (addr, server, body) = spawn_capturing_server().await;
        {
            let _cap = headers_timeout_override::Scoped::set(Ok(Some(
                std::time::Duration::from_millis(45_000),
            )));
            let llm = NativeHttpLlm::new(
                config("openai", &format!("http://{addr}/v1")),
                String::new(),
            );
            let ok = llm.chat(chat_params()).await;
            assert!(ok.is_ok(), "{:?}", ok.err());
        }
        let _ = body.await;
        server.abort();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let hang_addr = listener.local_addr().unwrap();
        let hang = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                held.push(sock);
            }
        });
        {
            let _cap = headers_timeout_override::Scoped::set(Ok(Some(
                std::time::Duration::from_millis(2_000),
            )));
            let llm = NativeHttpLlm::new(
                config("openai", &format!("http://{hang_addr}/v1")),
                String::new(),
            );
            let started = std::time::Instant::now();
            let error = llm
                .chat(chat_params())
                .await
                .expect_err("a silent gateway trips the headers cap")
                .to_string();
            assert!(
                started.elapsed() < std::time::Duration::from_secs(30),
                "the 2s cap must beat the 600s request budget"
            );
            assert!(
                error.starts_with("llm transport error headers_timeout"),
                "{error}"
            );
            assert!(
                llm.is_retryable_error(&error),
                "a headers timeout is transient: {error}"
            );
        }

        let _bad_value =
            headers_timeout_override::Scoped::set(parse_llm_headers_timeout_ms(Some("abc")));
        let llm = NativeHttpLlm::new(
            config("openai", &format!("http://{hang_addr}/v1")),
            String::new(),
        );
        let error = llm
            .chat(chat_params())
            .await
            .expect_err("a bad env value fails the request")
            .to_string();
        assert!(
            error.starts_with("config.invalid: KIMI_CODE_LLM_HEADERS_TIMEOUT_MS"),
            "{error}"
        );
        assert!(
            !llm.is_retryable_error(&error),
            "a bad env value is deterministic: {error}"
        );
        hang.abort();
    }

    #[tokio::test]
    async fn a_detected_dialect_replays_unmarked_history_on_the_next_request() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let server = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut calls = 0usize;
            while let Ok((mut sock, _)) = listener.accept().await {
                calls += 1;
                let mut raw = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    raw.extend_from_slice(&buf[..n]);
                    let Some(header_end) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
                        continue;
                    };
                    let header_end = header_end + 4;
                    let headers = String::from_utf8_lossy(&raw[..header_end]).to_ascii_lowercase();
                    let content_length = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .and_then(|value| value.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if raw.len() >= header_end + content_length {
                        break;
                    }
                }
                let body_start = raw
                    .windows(4)
                    .position(|w| w == b"\r\n\r\n")
                    .map(|i| i + 4)
                    .unwrap_or(raw.len());
                let _ = tx.send(String::from_utf8_lossy(&raw[body_start..]).into_owned());
                let body = if calls == 1 {
                    "data: {\"choices\":[{\"delta\":{\"reasoning\":\"live dialect\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"
                } else {
                    "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n"
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });

        let base = format!("http://{addr}/v1");
        let model = "m-detect-dialect-e2e";
        let mut cfg = config("openai", &base);
        cfg.model = model.into();

        let llm = NativeHttpLlm::new(cfg.clone(), String::new());
        let first = llm
            .chat(LLMChatParams {
                messages: Arc::from(vec![kimi_core::types::LLMMessage::user("hi")]),
                tools: Arc::from(Vec::new()),
                ..Default::default()
            })
            .await;
        assert!(first.is_ok(), "{:?}", first.err());
        assert_eq!(
            detected_reasoning_key_for(&base, model).as_deref(),
            Some("reasoning")
        );
        let _ = rx.recv().await;

        let llm = NativeHttpLlm::new(cfg, String::new());
        let history = kimi_core::types::LLMMessage {
            role: "assistant".into(),
            content: String::new(),
            blocks: vec![kimi_core::types::ContentBlock::Think {
                think: "earlier reasoning".into(),
                encrypted: None,
                details_index: None,
                reasoning_key: None,
                details_summary: None,
            }],
            ..Default::default()
        };
        let second = llm
            .chat(LLMChatParams {
                messages: Arc::from(vec![
                    history,
                    kimi_core::types::LLMMessage::user("and now?"),
                ]),
                tools: Arc::from(Vec::new()),
                ..Default::default()
            })
            .await;
        assert!(second.is_ok(), "{:?}", second.err());
        let body = rx.recv().await.unwrap();
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["messages"][0]["reasoning"], "earlier reasoning");
        assert!(json["messages"][0].get("reasoning_content").is_none());
        server.abort();
    }

    #[test]
    fn a_per_request_output_cap_overrides_the_config() {
        let llm = NativeHttpLlm::new(
            config("openai", "https://api.example.com/v1"),
            String::new(),
        );
        let params = |used: Option<u32>, window: Option<u32>| LLMChatParams {
            used_context_tokens: used,
            max_context_tokens: window,
            max_output_tokens: Some(4096),
            max_output_size: None,
            ..Default::default()
        };
        assert_eq!(llm.completion_cap(&params(None, None)), Some(4096));
        assert_eq!(
            llm.completion_cap(&params(Some(900), Some(1000))),
            Some(100)
        );
    }

    #[test]
    fn completion_cap_is_absent_unless_explicitly_configured() {
        let params = |used: Option<u32>, window: Option<u32>| LLMChatParams {
            used_context_tokens: used,
            max_context_tokens: window,
            max_output_tokens: None,
            max_output_size: None,
            ..Default::default()
        };

        let llm = NativeHttpLlm::new(
            config("openai", "https://api.example.com/v1"),
            String::new(),
        );
        assert_eq!(
            llm.completion_cap(&params(Some(90_000), Some(100_000))),
            None
        );
        assert_eq!(llm.completion_cap(&params(None, None)), None);
        assert_eq!(
            llm.completion_cap(&params(Some(200_000), Some(100_000))),
            None
        );

        let mut cfg = config("openai", "https://api.example.com/v1");
        cfg.max_tokens = Some(64_000);
        let llm = NativeHttpLlm::new(cfg, String::new());
        assert_eq!(
            llm.completion_cap(&params(Some(10_000), Some(200_000))),
            Some(64_000)
        );
        assert_eq!(
            llm.completion_cap(&params(Some(60_000), Some(64_000))),
            Some(4_000)
        );
        assert_eq!(
            llm.completion_cap(&params(Some(200_000), Some(100_000))),
            Some(1)
        );

        let mut opted_out = config("openai", "https://api.example.com/v1");
        opted_out.max_tokens = Some(0);
        let llm = NativeHttpLlm::new(opted_out, String::new());
        assert_eq!(
            llm.completion_cap(&params(Some(10_000), Some(100_000))),
            None
        );

        let mut kimi = config("openai", "https://api.example.com/v1");
        kimi.provider_type = Some("kimi".into());
        let kimi = NativeHttpLlm::new(kimi, String::new());
        assert_eq!(
            kimi.completion_cap(&params(Some(90_000), Some(100_000))),
            Some(10_000)
        );
        assert_eq!(kimi.completion_cap(&params(None, Some(100_000))), None);
        assert_eq!(kimi.completion_cap(&params(None, None)), None);

        let mut kimi_anthropic = config("anthropic", "https://api.anthropic.com/v1");
        kimi_anthropic.model = "k3".into();
        kimi_anthropic.provider_type = Some("kimi".into());
        let kimi_anthropic = NativeHttpLlm::new(kimi_anthropic, String::new());
        assert_eq!(
            kimi_anthropic.completion_cap(&params(Some(90_000), Some(100_000))),
            None
        );

        let mut claude = config("anthropic", "https://api.anthropic.com/v1");
        claude.model = "claude-sonnet-4-5".into();
        let claude = NativeHttpLlm::new(claude, String::new());
        assert_eq!(claude.completion_cap(&params(None, None)), None);
    }

    #[test]
    fn an_unset_openai_budget_falls_back_to_the_model_family_tier() {
        let params = |used: Option<u32>, window: Option<u32>| LLMChatParams {
            used_context_tokens: used,
            max_context_tokens: window,
            max_output_tokens: None,
            max_output_size: None,
            ..Default::default()
        };
        let llm = |model: &str, effort: Option<&str>, max_tokens: Option<u32>| {
            let mut cfg = config("openai", "https://api.deepseek.com/v1");
            cfg.model = model.into();
            cfg.reasoning_effort = effort.map(str::to_string);
            cfg.max_tokens = max_tokens;
            NativeHttpLlm::new(cfg, String::new())
        };

        let deepseek = llm("deepseek-chat", Some("high"), None);
        assert_eq!(
            deepseek.openai_completion_cap(&params(None, None)),
            Some(65_536)
        );
        assert_eq!(
            llm("deepseek-chat", Some("off"), None).openai_completion_cap(&params(None, None)),
            Some(8_192)
        );
        assert_eq!(
            llm("deepseek-chat", Some("max"), None).openai_completion_cap(&params(None, None)),
            Some(131_072)
        );

        assert_eq!(
            deepseek.openai_completion_cap(&params(Some(10_000), Some(20_000))),
            Some(10_000)
        );
        assert_eq!(
            deepseek.openai_completion_cap(&params(Some(20_000), Some(20_000))),
            None
        );

        assert_eq!(
            llm("deepseek-chat", Some("high"), Some(4_000))
                .openai_completion_cap(&params(None, None)),
            Some(4_000)
        );
        assert_eq!(
            llm("deepseek-chat", Some("high"), Some(0)).openai_completion_cap(&params(None, None)),
            None
        );

        assert_eq!(
            llm("gpt-5", Some("high"), None).openai_completion_cap(&params(None, None)),
            None
        );
    }

    #[test]
    fn wire_history_normalises_tool_call_ids_per_protocol() {
        let call = |id: &str| LLMMessage {
            role: "assistant".into(),
            content: String::new(),
            tool_calls: vec![kimi_core::types::ToolCall {
                id: id.into(),
                name: "Read".into(),
                arguments: serde_json::json!({}),
                extras: None,
                args_parse_error: None,
            }],
            ..LLMMessage::default()
        };
        let messages: std::sync::Arc<[LLMMessage]> = std::sync::Arc::from(vec![
            LLMMessage::user("hi"),
            call("rs_1|call_abc/1"),
            LLMMessage::tool_result("rs_1|call_abc/1", "ok"),
        ]);
        let params = LLMChatParams {
            messages,
            ..Default::default()
        };

        let chat = NativeHttpLlm::new(
            config("openai", "https://api.example.com/v1"),
            String::new(),
        );
        let wire = chat.wire_messages(&params);
        assert_eq!(wire[1].tool_calls[0].id, "rs_1_call_abc_1");
        assert_eq!(wire[2].tool_call_id.as_deref(), Some("rs_1_call_abc_1"));

        let responses = NativeHttpLlm::new(
            config("openai_responses", "https://api.example.com/v1"),
            String::new(),
        );
        let wire = responses.wire_messages(&params);
        assert_eq!(wire[1].tool_calls[0].id, "rs_1");
        assert_eq!(wire[2].tool_call_id.as_deref(), Some("rs_1"));

        let google = NativeHttpLlm::new(
            config("google", "https://generativelanguage.googleapis.com/v1beta"),
            String::new(),
        );
        let wire = google.wire_messages(&params);
        assert_eq!(wire[1].tool_calls[0].id, "rs_1|call_abc/1");
        assert_eq!(wire[2].tool_call_id.as_deref(), Some("rs_1|call_abc/1"));
    }

    #[test]
    fn only_the_google_provider_inlines_video() {
        for protocol in ["google", "google-genai", "gemini"] {
            let llm = NativeHttpLlm::new(
                config(protocol, "https://generativelanguage.googleapis.com/v1beta"),
                String::new(),
            );
            assert!(
                llm.media_target().unwrap().inline_video,
                "{protocol} must inline video"
            );
        }
        for protocol in ["openai", "openai_responses", "anthropic"] {
            let llm = NativeHttpLlm::new(
                config(protocol, "https://api.example.com/v1"),
                String::new(),
            );
            assert!(
                !llm.media_target().unwrap().inline_video,
                "{protocol} must not inline video"
            );
        }
    }

    #[tokio::test]
    async fn unconfigured_completion_budget_stays_off_the_wire() {
        for (protocol, field) in [
            ("openai", "max_tokens"),
            ("openai_responses", "max_output_tokens"),
            ("google", "maxOutputTokens"),
        ] {
            let (addr, server, body) = spawn_capturing_server().await;
            let llm = NativeHttpLlm::new(
                config(protocol, &format!("http://{addr}/v1")),
                String::new(),
            );
            let _ = llm
                .chat(LLMChatParams {
                    messages: Arc::from(vec![kimi_core::types::LLMMessage::user("hi")]),
                    tools: Arc::from(Vec::new()),
                    used_context_tokens: Some(90_000),
                    max_context_tokens: Some(100_000),
                    max_output_tokens: None,
                    max_output_size: None,
                    ..Default::default()
                })
                .await;
            let body = body.await.unwrap();
            let json: serde_json::Value = serde_json::from_str(&body).unwrap();
            let sent = match field {
                "max_tokens" => json.get(field).is_some(),
                "max_output_tokens" => json.get(field).is_some(),
                _ => json
                    .get("generationConfig")
                    .and_then(|config| config.get(field))
                    .is_some(),
            };
            assert!(!sent, "{protocol} sent {field}: {json}");
            server.abort();
        }

        let (addr, server, body) = spawn_capturing_server().await;
        let mut cfg = config("anthropic", &format!("http://{addr}/v1"));
        cfg.model = "claude-sonnet-4-5".into();
        let llm = NativeHttpLlm::new(cfg, String::new());
        let _ = llm
            .chat(LLMChatParams {
                messages: Arc::from(vec![kimi_core::types::LLMMessage::user("hi")]),
                tools: Arc::from(Vec::new()),
                used_context_tokens: Some(90_000),
                max_context_tokens: Some(100_000),
                max_output_tokens: None,
                max_output_size: None,
                ..Default::default()
            })
            .await;
        let body = body.await.unwrap();
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["max_tokens"], 64_000, "{json}");
        server.abort();

        let (addr, server, body) = spawn_capturing_server().await;
        let mut cfg = config("openai", &format!("http://{addr}/v1"));
        cfg.provider_type = Some("kimi".into());
        let llm = NativeHttpLlm::new(cfg, String::new());
        let _ = llm
            .chat(LLMChatParams {
                messages: Arc::from(vec![kimi_core::types::LLMMessage::user("hi")]),
                tools: Arc::from(Vec::new()),
                used_context_tokens: Some(90_000),
                max_context_tokens: Some(100_000),
                max_output_tokens: None,
                max_output_size: None,
                ..Default::default()
            })
            .await;
        let body = body.await.unwrap();
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["max_completion_tokens"], 10_000, "{json}");
        server.abort();
    }

    #[test]
    fn endpoint_joins_openai_and_anthropic_paths() {
        let llm = NativeHttpLlm::new(
            config("openai", "https://api.example.com/v1/"),
            String::new(),
        );
        assert_eq!(
            llm.endpoint(),
            "https://api.example.com/v1/chat/completions"
        );

        let llm = NativeHttpLlm::new(
            config("anthropic", "https://api.example.com/v1"),
            String::new(),
        );
        assert_eq!(llm.endpoint(), "https://api.example.com/v1/messages");
    }

    #[test]
    fn retryable_error_classification() {
        let llm = NativeHttpLlm::new(
            config("openai", "https://api.example.com/v1"),
            String::new(),
        );
        assert!(llm.is_retryable_error("llm http status 429 Too Many Requests: slow down"));
        assert!(llm.is_retryable_error("llm http status 409 Conflict: concurrent edit"));
        assert!(llm.is_retryable_error("llm http status 503 Service Unavailable: busy"));
        assert!(llm.is_retryable_error("llm transport error connect: connection refused"));
        assert!(llm.is_retryable_error("llm transport error timeout: operation timed out"));
        assert!(llm.is_retryable_error("llm sse decode error: expected value at line 1"));
        assert!(!llm.is_retryable_error("llm http status 401 Unauthorized: bad key"));
        assert!(!llm.is_retryable_error("llm http status 400 Bad Request: invalid schema"));
        assert!(
            !llm.is_retryable_error("llm http status 402 Payment Required: Insufficient Balance")
        );
    }

    #[test]
    fn retryable_status_set_matches_v2_explicit_list() {
        let llm = NativeHttpLlm::new(
            config("openai", "https://api.example.com/v1"),
            String::new(),
        );
        for code in [408, 409, 425, 429, 500, 502, 503, 504, 529] {
            assert!(is_retryable_status_code(code), "{code} must be retryable");
            assert!(
                llm.is_retryable_error(&format!("llm http status {code} Err: transient")),
                "status {code} must be retryable"
            );
        }
        for code in [
            400, 401, 402, 403, 404, 410, 422, 423, 426, 501, 505, 506, 507, 508, 510, 511, 520,
            521, 523, 530, 599,
        ] {
            assert!(!is_retryable_status_code(code), "{code} must fail fast");
            assert!(
                !llm.is_retryable_error(&format!("llm http status {code} Err: deterministic")),
                "status {code} must fail fast"
            );
        }
    }

    #[test]
    fn both_transports_agree_on_every_status() {
        let native = NativeHttpLlm::new(
            config("openai", "https://api.example.com/v1"),
            String::new(),
        );
        let proxy = crate::llm::proxy::HostLlmProxy::new(String::new(), "m".into());
        for code in 400..=599u16 {
            let message = format!("llm http status {code} Err: boom");
            assert_eq!(
                native.is_retryable_error(&message),
                proxy.is_retryable_error(&message),
                "the transports disagree on status {code}"
            );
        }
    }

    #[test]
    fn quota_exhaustion_is_not_retryable() {
        let llm = NativeHttpLlm::new(
            config("openai", "https://api.example.com/v1"),
            String::new(),
        );
        let quota_bodies = [
            r#"{"error":{"code":"insufficient_quota","message":"You exceeded your current quota, please check your plan and billing details."}}"#,
            r#"{"error":{"code":"exceeded_current_quota_error","type":"quota","message":"quota reached"}}"#,
            r#"{"error":{"message":"Insufficient Balance in your account"}}"#,
            r#"{"error":{"message":"Please recharge your account to continue"}}"#,
            r#"{"error":{"message":"Your account is in arrears. Settle the balance to resume."}}"#,
            "Check your account balance before retrying",
            "You exceeded your current token quota: 1000000 tokens",
        ];
        for body in quota_bodies {
            assert!(
                !llm.is_retryable_error(&format!("llm http status 429 Too Many Requests: {body}")),
                "quota body must be non-retryable: {body}"
            );
        }

        assert!(llm.is_retryable_error(
            "llm http status 429 Too Many Requests: rate limit exceeded, slow down"
        ));

        assert!(!llm.is_retryable_error(
            "llm sse decode error: exceeded_current_quota_error while decoding"
        ));
    }

    #[test]
    fn transport_errors_are_classified_by_stamped_kind() {
        let llm = NativeHttpLlm::new(
            config("anthropic", "https://api.example.com/v1"),
            String::new(),
        );
        assert!(llm.is_retryable_error(
            "llm transport error timeout: error sending request for url (https://api.example.com): connection timed out"
        ));
        assert!(llm.is_retryable_error(
            "llm transport error connect: error sending request for url (https://api.example.com)"
        ));
        assert!(llm.is_retryable_error(
            "llm transport error transport: failed to lookup address information: Name or service not known"
        ));
        assert!(llm.is_retryable_error("llm transport error decode: error decoding response body"));
        assert!(!llm.is_retryable_error(
            "llm transport error invalid_request: builder error: relative URL without a base"
        ));
    }

    #[test]
    fn retryable_error_ignores_status_body_keywords() {
        let llm = NativeHttpLlm::new(
            config("openai", "https://api.example.com/v1"),
            String::new(),
        );

        assert!(
            !llm.is_retryable_error("llm http status 400 Bad Request: unknown field 'connection'")
        );
        assert!(!llm.is_retryable_error(
            "llm http status 401 Unauthorized: session timeout, please re-authenticate"
        ));
        assert!(llm.is_retryable_error("llm http status 429 Too Many Requests: (retry-after 30s)"));
        assert!(llm.is_retryable_error("llm http status 529 overloaded"));
    }

    #[test]
    fn model_name_and_system_prompt_come_from_config() {
        let llm = NativeHttpLlm::new(config("openai", "https://api.example.com/v1"), "sys".into());
        assert_eq!(llm.model_name(), "test-model");
        assert_eq!(llm.system_prompt(), "sys");
    }

    #[test]
    fn a_models_own_system_prompt_replaces_the_session_prompt() {
        let mut cfg = config("openai", "https://api.example.com/v1");
        cfg.system_prompt = Some("model prompt".into());
        let llm = NativeHttpLlm::new(cfg, "session prompt".into());
        assert_eq!(llm.system_prompt(), "model prompt");
    }

    #[test]
    fn thinking_keep_selects_the_beta_messages_endpoint_like_v2() {
        let mut cfg = config("anthropic", "https://api.example.com/v1");
        cfg.thinking_keep = Some("all".into());
        let llm = NativeHttpLlm::new(cfg, "sys".into());
        assert_eq!(
            llm.endpoint(),
            "https://api.example.com/v1/messages?beta=true"
        );

        let mut cfg = config("anthropic", "https://api.example.com/v1");
        cfg.thinking_keep = Some("all".into());
        cfg.beta_api = true;
        let llm = NativeHttpLlm::new(cfg, "sys".into());
        assert_eq!(
            llm.endpoint(),
            "https://api.example.com/v1/messages?beta=true"
        );
    }

    #[test]
    fn off_effort_fills_a_missing_reasoning_effort() {
        let mut cfg = config("openai", "https://api.example.com/v1");
        cfg.off_effort = Some("none".into());
        let llm = NativeHttpLlm::new(cfg, String::new());
        assert_eq!(llm.config.reasoning_effort.as_deref(), Some("none"));

        let mut declared = config("openai", "https://api.example.com/v1");
        declared.off_effort = Some("none".into());
        declared.reasoning_effort = Some("high".into());
        let llm = NativeHttpLlm::new(declared, String::new());
        assert_eq!(llm.config.reasoning_effort.as_deref(), Some("high"));
    }

    #[tokio::test]
    async fn chat_fails_cleanly_on_unreachable_endpoint() {
        let mut cfg = config("openai", "http://127.0.0.1:1/v1");
        cfg.custom_headers.insert("x-test".into(), "1".into());
        let llm = NativeHttpLlm::new(cfg, String::new());
        let result = llm
            .chat(LLMChatParams {
                cancel: None,
                messages: Arc::from(Vec::new()),
                tools: Arc::from(Vec::new()),
                ..Default::default()
            })
            .await;
        assert!(result.is_err());
        let msg = result.err().unwrap().to_string();
        assert!(
            msg.contains("llm transport error"),
            "unexpected error: {msg}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn chat_cancels_mid_stream_and_never_retries() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;

            let body = "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"why\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"hel\"}}]}\n\n";
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{body}"
            );
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.flush().await;

            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        });

        let cfg = config("openai", &format!("http://{addr}/v1"));
        let llm = std::sync::Arc::new(NativeHttpLlm::new(cfg, String::new()));
        let token = tokio_util::sync::CancellationToken::new();

        let chat_llm = llm.clone();
        let chat_token = token.clone();
        let chat = tokio::spawn(async move {
            chat_llm
                .chat(LLMChatParams {
                    cancel: Some(chat_token),
                    messages: Arc::from(vec![kimi_core::types::LLMMessage {
                        role: "user".into(),
                        content: "hi".into(),
                        ..Default::default()
                    }]),
                    tools: Arc::from(Vec::new()),
                    ..Default::default()
                })
                .await
        });

        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        token.cancel();

        let result = tokio::time::timeout(std::time::Duration::from_secs(5), chat)
            .await
            .expect("a cancelled chat must return promptly, not drain the stream")
            .unwrap();
        assert!(result.is_err(), "a cancelled chat must fail");
        let msg = result.err().unwrap().to_string();
        assert!(msg.starts_with("llm cancelled"), "unexpected error: {msg}");
        assert!(
            !llm.is_retryable_error(&msg),
            "cancellation must never be classified as retryable"
        );
        let partial = llm
            .take_interrupted_response()
            .expect("the cancelled attempt exposes its accumulated response");
        assert_eq!(partial.content, "hel");
        assert!(matches!(
            partial.thinking.as_slice(),
            [kimi_core::types::ContentBlock::Think { think, .. }] if think == "why"
        ));
        assert!(llm.take_interrupted_response().is_none());

        server.abort();
    }

    async fn spawn_401_then_ok_server() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut requests = 0;
            while let Ok((mut sock, _)) = listener.accept().await {
                requests += 1;
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;

                let response = if requests == 1 {
                    "HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                        .to_string()
                } else {
                    sse_response(
                        "data: {\"choices\":[{\"delta\":{\"content\":\"recovered\"}}]}\n\ndata: [DONE]\n\n",
                    )
                };
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        (addr, handle)
    }

    async fn spawn_fixed_200_server(
        content_type: &'static str,
        body: &'static str,
    ) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        (addr, handle)
    }

    fn sse_response(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    async fn chat_once(llm: &NativeHttpLlm) -> Result<LLMChatResponse, String> {
        llm.chat(LLMChatParams {
            cancel: None,
            messages: Arc::from(Vec::new()),
            tools: Arc::from(Vec::new()),
            ..Default::default()
        })
        .await
        .map_err(|e| e.to_string())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_call_keeps_the_provider_trace_id() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let body = "{\"error\":\"boom\"}";

                let response = format!(
                    "HTTP/1.1 500 Internal Server Error\r\ncontent-type: application/json\r\nx-trace-id: trace-abc-123\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });

        let llm = NativeHttpLlm::new(
            config("openai", &format!("http://{addr}/v1")),
            String::new(),
        );
        let result = llm
            .chat(LLMChatParams {
                cancel: None,
                messages: Arc::from(vec![kimi_core::types::LLMMessage {
                    role: "user".into(),
                    content: "hi".into(),
                    ..Default::default()
                }]),
                tools: Arc::from(Vec::new()),
                ..Default::default()
            })
            .await;

        let error = result.expect_err("a 500 must fail the call");
        let typed = error
            .downcast_ref::<kimi_core::error::LlmError>()
            .expect("the transport must keep its typed error, not only its text");
        assert_eq!(typed.status_code(), Some(500));
        assert_eq!(
            typed.trace_id(),
            Some("trace-abc-123"),
            "the provider's request id must survive the failure"
        );

        assert!(
            typed.to_string().contains("[trace trace-abc-123]"),
            "{typed}"
        );

        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_successful_call_records_the_provider_trace_id_per_request() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut requests = 0u32;
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                requests += 1;
                let body =
                    "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
                let trace = if requests == 1 {
                    "x-trace-id: trace-ok\r\n"
                } else {
                    ""
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n{trace}content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });

        let llm = NativeHttpLlm::new(
            config("openai", &format!("http://{addr}/v1")),
            String::new(),
        );
        chat_once(&llm).await.expect("first call succeeds");
        assert_eq!(llm.last_trace_id().as_deref(), Some("trace-ok"));
        chat_once(&llm).await.expect("second call succeeds");
        assert_eq!(llm.last_trace_id(), None);

        server.abort();
    }

    #[tokio::test]
    async fn google_retry_info_reaches_the_retry_layer_as_a_typed_wait() {
        let body = r#"{"error":{"code":429,"message":"Resource exhausted","status":"RESOURCE_EXHAUSTED","details":[{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"1.5s"}]}}"#;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let response = format!(
                    "HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        let llm = NativeHttpLlm::new(
            config("google-genai", &format!("http://{addr}")),
            String::new(),
        );
        let err = llm
            .chat(LLMChatParams {
                cancel: None,
                messages: Arc::from(Vec::new()),
                tools: Arc::from(Vec::new()),
                ..Default::default()
            })
            .await
            .expect_err("a 429 must not be Ok");

        let typed = err
            .downcast_ref::<kimi_core::error::LlmError>()
            .expect("the transport must hand the retry layer a typed error, not a bare string");
        assert_eq!(typed.status_code(), Some(429));
        assert_eq!(typed.retry_after(), Some(Duration::from_millis(1500)));

        assert!(
            typed.to_string().starts_with("llm http status 429"),
            "{typed}"
        );
        assert!(llm.is_retryable_error(&typed.to_string()));
        server.abort();
    }

    #[tokio::test]
    async fn retry_after_header_is_honoured_over_the_body() {
        let body = r#"{"error":{"code":429,"message":"slow down"}}"#;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let response = format!(
                    "HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\nretry-after: 30\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        let llm = NativeHttpLlm::new(
            config("openai", &format!("http://{addr}/v1")),
            String::new(),
        );
        let err = llm
            .chat(LLMChatParams {
                cancel: None,
                messages: Arc::from(Vec::new()),
                tools: Arc::from(Vec::new()),
                ..Default::default()
            })
            .await
            .expect_err("a 429 must not be Ok");
        let typed = err
            .downcast_ref::<kimi_core::error::LlmError>()
            .expect("typed error");
        assert_eq!(typed.retry_after(), Some(Duration::from_secs(30)));
        server.abort();
    }

    #[tokio::test]
    async fn empty_sse_body_is_reported_instead_of_returning_an_empty_answer() {
        let (addr, server) = spawn_fixed_200_server("text/event-stream", "").await;
        let llm = NativeHttpLlm::new(
            config("openai", &format!("http://{addr}/v1")),
            String::new(),
        );
        let error = chat_once(&llm)
            .await
            .expect_err("empty body must not be Ok");
        assert!(
            error.contains("produced no events"),
            "unexpected error: {error}"
        );
        assert!(
            llm.is_retryable_error(&error),
            "a transient empty relay body must be retried: {error}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn non_sse_body_is_reported_as_a_misconfigured_endpoint() {
        let (addr, server) =
            spawn_fixed_200_server("text/html", "<html><body>index of /</body></html>").await;
        let llm = NativeHttpLlm::new(
            config("openai", &format!("http://{addr}/v1")),
            String::new(),
        );
        let error = chat_once(&llm)
            .await
            .expect_err("non-SSE body must not be Ok");
        assert!(
            error.starts_with(NOT_AN_SSE_ENDPOINT_PREFIX),
            "unexpected error: {error}"
        );
        assert!(
            error.contains("content-type \"text/html\""),
            "the diagnosis must name the content type: {error}"
        );
        assert!(
            !llm.is_retryable_error(&error),
            "a misconfigured endpoint must not be retried: {error}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn in_band_error_frame_is_surfaced() {
        let (addr, server) = spawn_fixed_200_server(
            "text/event-stream",
            "data: {\"error\":{\"message\":\"upstream exploded\",\"type\":\"upstream_error\"}}\n\n",
        )
        .await;
        let llm = NativeHttpLlm::new(
            config("openai", &format!("http://{addr}/v1")),
            String::new(),
        );
        let error = chat_once(&llm)
            .await
            .expect_err("in-band error must not be Ok");
        assert!(
            error.contains("upstream exploded"),
            "the provider message must survive: {error}"
        );
        assert!(error.contains("llm provider stream error"), "{error}");
        server.abort();
    }

    #[tokio::test]
    async fn a_normal_sse_stream_still_decodes() {
        let (addr, server) = spawn_fixed_200_server(
            "text/event-stream",
            "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\ndata: [DONE]\n\n",
        )
        .await;
        let llm = NativeHttpLlm::new(
            config("openai", &format!("http://{addr}/v1")),
            String::new(),
        );
        let response = chat_once(&llm).await.expect("a normal stream must succeed");
        assert_eq!(response.content, "hello");
        server.abort();
    }

    #[tokio::test]
    async fn oauth_token_refreshes_once_on_401() {
        let (addr, server) = spawn_401_then_ok_server().await;

        let mut cfg = config("openai", &format!("http://{addr}/v1"));
        cfg.api_key = String::new();
        cfg.auth_provider = Some("kimi".into());
        let fetches = Arc::new(AtomicU32::new(0));
        let fetch_count = fetches.clone();
        let llm =
            NativeHttpLlm::new(cfg, String::new()).with_auth_provider(Arc::new(move |force| {
                let n = fetch_count.fetch_add(1, Ordering::SeqCst) + 1;
                Box::pin(async move { Ok(format!("token-{n}-force-{force}")) })
            }));

        let result = llm
            .chat(LLMChatParams {
                cancel: None,
                messages: Arc::from(Vec::new()),
                tools: Arc::from(Vec::new()),
                ..Default::default()
            })
            .await;
        let response = result.expect("expected the 401 to be recovered");
        assert_eq!(
            response.content, "recovered",
            "the post-refresh request must return the streamed content"
        );
        assert_eq!(
            fetches.load(Ordering::SeqCst),
            2,
            "initial fetch + exactly one forced refresh"
        );
        server.abort();
    }

    #[tokio::test]
    async fn api_key_env_missing_is_config_invalid_without_fallback() {
        let env_name = format!("KIMI_TEST_MISSING_KEY_{}", ulid::Ulid::new());
        assert!(std::env::var_os(&env_name).is_none());
        let mut wire = serde_json::json!({
            "protocol": "openai",
            "base_url": "http://127.0.0.1:1/v1",
            "model": "test-model",
            "api_key": "",
            "api_key_env": env_name,
        });
        let cfg: NativeLlmConfig = serde_json::from_value(wire.take()).unwrap();
        let llm = NativeHttpLlm::new(cfg, String::new());
        let error = llm
            .credential()
            .await
            .expect_err("missing environment key must not fall back");
        assert!(error.starts_with("config.invalid:"), "{error}");
        assert!(error.contains(&env_name), "{error}");
        assert!(error.contains("not set or is empty"), "{error}");
    }

    struct CredentialEnv {
        name: String,
        previous: Option<std::ffi::OsString>,
    }

    impl CredentialEnv {
        fn new() -> Self {
            let name = format!("KIMI_TEST_RUNTIME_KEY_{}", ulid::Ulid::new());
            let previous = std::env::var_os(&name);
            Self { name, previous }
        }

        fn set(&self, value: Option<&str>) {
            unsafe {
                match value {
                    Some(value) => std::env::set_var(&self.name, value),
                    None => std::env::remove_var(&self.name),
                }
            }
        }
    }

    impl Drop for CredentialEnv {
        fn drop(&mut self) {
            unsafe {
                match &self.previous {
                    Some(value) => std::env::set_var(&self.name, value),
                    None => std::env::remove_var(&self.name),
                }
            }
        }
    }

    #[tokio::test]
    async fn api_key_env_reads_each_time_and_rejects_removed_or_blank_values() {
        let env = CredentialEnv::new();
        let mut cfg = config("openai", "http://127.0.0.1:1/v1");
        cfg.api_key.clear();
        cfg.api_key_env = Some(env.name.clone());
        let llm = NativeHttpLlm::new(cfg, String::new());
        env.set(Some("example-first-key"));
        assert_eq!(llm.credential().await.unwrap(), "example-first-key");
        env.set(Some("example-second-key"));
        assert_eq!(llm.credential().await.unwrap(), "example-second-key");
        for value in [None, Some(""), Some(" \t ")] {
            env.set(value);
            let error = llm.credential().await.unwrap_err();
            assert!(error.starts_with("config.invalid:"), "{error}");
            assert!(error.contains("not set or is empty"), "{error}");
            assert!(!error.contains("example-second-key"), "{error}");
        }
        assert!(llm.config.api_key.is_empty());
        assert!(llm.cached_token_value().is_none());
    }

    #[tokio::test]
    async fn api_key_env_rejects_inline_and_oauth_conflicts_before_lookup() {
        let env = CredentialEnv::new();
        for oauth in [false, true] {
            let mut cfg = config("openai", "http://127.0.0.1:1/v1");
            cfg.api_key_env = Some(env.name.clone());
            if oauth {
                cfg.api_key.clear();
                cfg.auth_provider = Some("example-provider".into());
            }
            let llm = NativeHttpLlm::new(cfg, String::new());
            let error = llm.credential().await.unwrap_err();
            assert!(error.starts_with("config.invalid:"), "{error}");
            assert!(error.contains("mutually exclusive"), "{error}");
            assert!(!error.contains("not set or is empty"), "{error}");
            assert!(!error.contains("test-key"), "{error}");
        }
    }

    #[tokio::test]
    async fn oauth_cached_token_is_reused_across_requests() {
        let mut cfg = config("openai", "http://127.0.0.1:1/v1");
        cfg.api_key = String::new();
        cfg.auth_provider = Some("kimi".into());
        let fetches = Arc::new(AtomicU32::new(0));
        let fetch_count = fetches.clone();
        let llm =
            NativeHttpLlm::new(cfg, String::new()).with_auth_provider(Arc::new(move |_force| {
                let n = fetch_count.fetch_add(1, Ordering::SeqCst) + 1;
                Box::pin(async move { Ok(format!("token-{n}")) })
            }));

        let t1 = llm.credential().await.unwrap();
        assert_eq!(t1, "token-1");
        assert_eq!(fetches.load(Ordering::SeqCst), 1, "first call must fetch");

        let t2 = llm.credential().await.unwrap();
        assert_eq!(t2, "token-1", "cached token must be returned unchanged");
        assert_eq!(
            fetches.load(Ordering::SeqCst),
            1,
            "the cached token must serve the second call without a host round-trip"
        );
    }

    #[tokio::test]
    async fn oauth_without_wired_channel_fails_cleanly() {
        let mut cfg = config("openai", "http://127.0.0.1:1/v1");
        cfg.api_key = String::new();
        cfg.auth_provider = Some("kimi".into());
        let llm = NativeHttpLlm::new(cfg, String::new());
        let result = llm
            .chat(LLMChatParams {
                cancel: None,
                messages: Arc::from(Vec::new()),
                tools: Arc::from(Vec::new()),
                ..Default::default()
            })
            .await;
        let msg = result.err().unwrap().to_string();
        assert!(
            msg.contains("no token channel is wired"),
            "unexpected error: {msg}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn oauth_cold_fetch_is_single_flighted_across_concurrent_callers() {
        let mut cfg = config("openai", "http://127.0.0.1:1/v1");
        cfg.api_key = String::new();
        cfg.auth_provider = Some("kimi".into());
        let fetches = Arc::new(AtomicU32::new(0));
        let fetch_count = fetches.clone();
        let llm = Arc::new(
            NativeHttpLlm::new(cfg, String::new()).with_auth_provider(Arc::new(move |_force| {
                let n = fetch_count.fetch_add(1, Ordering::SeqCst) + 1;
                Box::pin(async move {
                    tokio::task::yield_now().await;
                    Ok(format!("token-{n}"))
                })
            })),
        );

        let mut joins = Vec::new();
        for _ in 0..8 {
            let llm = Arc::clone(&llm);
            joins.push(tokio::spawn(async move { llm.credential().await.unwrap() }));
        }
        let mut tokens = Vec::new();
        for join in joins {
            tokens.push(join.await.unwrap());
        }
        assert_eq!(
            fetches.load(Ordering::SeqCst),
            1,
            "concurrent cold fetches must collapse to a single host round-trip"
        );
        assert!(
            tokens.iter().all(|t| t == "token-1"),
            "every caller reuses the winner's token: {tokens:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_thinking_loop_is_dropped_from_the_stream() {
        const LOOP_DELTAS: usize = 60;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;

            let mut body = String::from(
                "data: {\"type\":\"content_block_start\",\"index\":0,\
                 \"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
            );
            for _ in 0..LOOP_DELTAS {
                body.push_str(
                    "data: {\"type\":\"content_block_delta\",\"index\":0,\
                     \"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Let me check. \"}}\n\n",
                );
            }
            body.push_str(
                "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
                 data: {\"type\":\"content_block_start\",\"index\":1,\
                 \"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
                 data: {\"type\":\"content_block_delta\",\"index\":1,\
                 \"delta\":{\"type\":\"text_delta\",\"text\":\"Here is the fix.\"}}\n\n\
                 data: {\"type\":\"message_stop\"}\n\n",
            );
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{body}"
            );
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.flush().await;
        });

        let seen: Arc<std::sync::Mutex<Vec<serde_json::Value>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink_seen = seen.clone();
        let cfg = config("anthropic", &format!("http://{addr}"));
        let llm = NativeHttpLlm::new(cfg, String::new()).with_sink(Arc::new(move |event| {
            sink_seen.lock().unwrap().push(event);
        }));

        llm.chat(LLMChatParams {
            cancel: None,
            messages: Arc::from(vec![kimi_core::types::LLMMessage {
                role: "user".into(),
                content: "hi".into(),
                ..Default::default()
            }]),
            tools: Arc::from(Vec::new()),
            ..Default::default()
        })
        .await
        .expect("the stream completes");

        let events = seen.lock().unwrap().clone();
        let think_deltas = events
            .iter()
            .filter(|e| e["part"]["type"] == "think")
            .count();
        let text_deltas = events
            .iter()
            .filter(|e| e["part"]["type"] == "text")
            .count();

        assert!(
            think_deltas > 0,
            "the opening of the thinking block is still shown"
        );
        assert!(
            think_deltas < LOOP_DELTAS,
            "the loop must stop reaching the host: {think_deltas} of {LOOP_DELTAS} forwarded"
        );
        assert_eq!(text_deltas, 1, "the answer is never gated");

        server.abort();
    }
}
