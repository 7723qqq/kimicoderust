use std::error::Error;
use std::fmt;
use std::sync::Arc;

use futures_util::future::{BoxFuture, Shared};
use futures_util::FutureExt;
use parking_lot::Mutex;
use serde_json::{Map, Value, json};

use crate::_base::log::log::ILogService;
use crate::app::auth::auth::IOAuthService;
use crate::app::event::event::IEventService;
use crate::app::telemetry::telemetry::ITelemetryService;
use crate::mcp_core::client_shared::ReqwestFetch;
use crate::mcp_core::client_http::OAuthClientProvider;
use crate::session::agent_lifecycle::agent_lifecycle::{IAgentLifecycleService, MAIN_AGENT_ID};
use crate::session::session_context::session_context::ISessionContext;
use crate::session::session_metadata::session_metadata::ISessionMetadata;
use crate::session::session_metadata::session_meta_events::SessionMetaUpdated;
use crate::wire::record::WireAsyncError;

use super::agent_title_prompt_source::IAgentTitlePromptSource;
use super::session_title::{ISessionTitleService, SessionTitleSource};

const MAX_GENERATED_TITLE_LENGTH: usize = 200;
const MAX_TITLE_INPUT_LENGTH: usize = 1000;
const MAX_TITLE_PROMPTS: usize = 3;
const MAX_TITLE_USER_SEGMENT: usize = 400;
const MAX_TITLE_FIRST_TURN_ASSISTANT: usize = 300;
const MAX_TITLE_DIGEST_USER_SEGMENT: usize = 200;
const MAX_TITLE_DIGEST_ASSISTANT: usize = 200;
const MAX_TITLE_DIGEST_INPUT_LENGTH: usize = 3000;

#[derive(Debug, Clone)]
struct SharedTitleError(Arc<String>);

impl SharedTitleError {
    fn new(error: WireAsyncError) -> Self {
        Self(Arc::new(error.to_string()))
    }

    fn boxed(self) -> WireAsyncError {
        Box::new(self)
    }
}

impl fmt::Display for SharedTitleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.as_str())
    }
}

impl Error for SharedTitleError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TitleOutcome {
    Ok,
    Truncated,
}

enum TitleGenerationResult {
    Title { title: String, outcome: TitleOutcome },
    Failure { reason: &'static str },
}

pub struct SessionTitleService {
    ctx: Arc<dyn ISessionContext>,
    metadata: Arc<dyn ISessionMetadata>,
    agent_lifecycle: Arc<dyn IAgentLifecycleService>,
    event_service: Arc<dyn IEventService>,
    providers: Arc<dyn crate::llm_adapter::provider::provider::IProviderService>,
    oauth: Arc<dyn IOAuthService>,
    host_headers: Arc<dyn crate::llm_adapter::model::host_request_headers::IHostRequestHeaders>,
    telemetry: Arc<dyn ITelemetryService>,
    log: Arc<dyn ILogService>,
    shared: Arc<Mutex<Option<Shared<BoxFuture<'static, Result<Option<String>, SharedTitleError>>>>>>,
}

impl SessionTitleService {
    pub fn new(
        ctx: Arc<dyn ISessionContext>,
        metadata: Arc<dyn ISessionMetadata>,
        agent_lifecycle: Arc<dyn IAgentLifecycleService>,
        event_service: Arc<dyn IEventService>,
        providers: Arc<dyn crate::llm_adapter::provider::provider::IProviderService>,
        oauth: Arc<dyn IOAuthService>,
        host_headers: Arc<dyn crate::llm_adapter::model::host_request_headers::IHostRequestHeaders>,
        telemetry: Arc<dyn ITelemetryService>,
        log: Arc<dyn ILogService>,
    ) -> Self {
        Self {
            ctx,
            metadata,
            agent_lifecycle,
            event_service,
            providers,
            oauth,
            host_headers,
            telemetry,
            log,
            shared: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn generate_title(&self, force: Option<bool>, source: Option<SessionTitleSource>) -> Result<Option<String>, WireAsyncError> {
        let force = force == Some(true);
        let source = source.unwrap_or(SessionTitleSource::UserPrompts);
        if force {
            return self.track_generation(true, source).await;
        }
        if let Some(shared) = self.shared.lock().clone() {
            return shared.await.map_err(SharedTitleError::boxed);
        }
        let this = self.clone_handle();
        let tracked = async move {
            let result = this.track_generation(false, source).await;
            *this.shared.lock() = None;
            result.map_err(SharedTitleError::new)
        }
        .boxed()
        .shared();
        *self.shared.lock() = Some(tracked.clone());
        tokio::spawn(tracked.clone());
        tracked.await.map_err(SharedTitleError::boxed)
    }

    async fn track_generation(&self, force: bool, source: SessionTitleSource) -> Result<Option<String>, WireAsyncError> {
        let started_at = chrono::Utc::now().timestamp_millis();
        let trigger = if force { "manual" } else { "auto" };
        let track = |result: TitleTrackResult| {
            let mut properties = Map::new();
            properties.insert("outcome".to_string(), result.outcome.into());
            if let Some(title_chars) = result.title_chars {
                properties.insert("title_chars".to_string(), title_chars.into());
            }
            if let Some(title_outcome) = result.title_outcome {
                properties.insert("title_outcome".to_string(), title_outcome.into());
            }
            if let Some(reason) = result.reason {
                properties.insert("reason".to_string(), reason.into());
            }
            if let Some(error_type) = result.error_type {
                properties.insert("error_type".to_string(), error_type.into());
            }
            properties.insert("trigger".to_string(), trigger.into());
            properties.insert("source".to_string(), source.as_str().into());
            properties.insert("duration_ms".to_string(), (chrono::Utc::now().timestamp_millis() - started_at).into());
            self.telemetry.track2("session_title_generation_finished", Some(properties));
        };
        let result = self.generate_title_once(force, source).await;
        match &result {
            Some(TitleGenerationResult::Title { title, outcome }) => {
                track(TitleTrackResult {
                    outcome: "success",
                    title_chars: Some(title.encode_utf16().count()),
                    title_outcome: Some(match outcome {
                        TitleOutcome::Ok => "ok",
                        TitleOutcome::Truncated => "truncated",
                    }),
                    reason: None,
                    error_type: None,
                });
            }
            Some(TitleGenerationResult::Failure { reason }) => {
                track(TitleTrackResult {
                    outcome: "unavailable",
                    title_chars: None,
                    title_outcome: None,
                    reason: Some(reason),
                    error_type: None,
                });
            }
            None => {}
        }
        match result {
            Some(TitleGenerationResult::Title { title, .. }) => Ok(Some(title)),
            Some(TitleGenerationResult::Failure { .. }) | None => Ok(None),
        }
    }

    async fn generate_title_once(&self, force: bool, source: SessionTitleSource) -> Option<TitleGenerationResult> {
        let current = self.metadata.read().await.ok()?;
        if !force && matches!(current.title_kind, Some(crate::session::session_metadata::session_metadata::SessionTitleKind::Custom | crate::session::session_metadata::session_metadata::SessionTitleKind::Generated)) {
            return None;
        }
        let main = self.agent_lifecycle.handle_of(MAIN_AGENT_ID);
        let Some(main) = main else {
            return Some(TitleGenerationResult::Failure { reason: "no_agent" });
        };
        let prompt_source = main.accessor().get::<IAgentTitlePromptSource>();
        let input = compose_title_input(prompt_source.as_ref(), source).await?;
        Some(self.generate_and_apply(&input, force).await)
    }

    async fn generate_and_apply(&self, chat_content: &str, force: bool) -> TitleGenerationResult {
        let current = match self.metadata.read().await {
            Ok(current) => current,
            Err(_) => return TitleGenerationResult::Failure { reason: "unexpected" },
        };
        if !force && current.title_kind == Some(crate::session::session_metadata::session_metadata::SessionTitleKind::Custom) {
            return TitleGenerationResult::Failure { reason: "custom_title" };
        }
        if self.kimi_title_provider().is_none() {
            return self.generate_local_title(chat_content, force).await;
        }
        self.request_hosted_title(chat_content, force).await
    }

    fn kimi_title_provider(&self) -> Option<crate::llm_adapter::provider::provider::ProviderConfig> {
        let provider = self.providers.get("kimi-code");
        if provider.is_none()
            || !crate::llm_adapter::provider::provider_definition::is_oauth_catalog_vendor(&provider.as_ref().unwrap().type_)
            || provider.as_ref().unwrap().oauth.is_none()
        {
            return None;
        }
        provider
    }

    async fn request_hosted_title(&self, chat_content: &str, force: bool) -> TitleGenerationResult {
        let Some(provider) = self.kimi_title_provider() else {
            return TitleGenerationResult::Failure { reason: "provider_unavailable" };
        };
        let runtime_auth = crate::app::auth::auth::KimiCodeRuntimeAuth {
            base_url: provider.base_url.clone(),
            oauth_ref: provider.oauth.clone().unwrap(),
        };
        let Some(token_provider) = self.oauth.resolve_token_provider("kimi-code", &runtime_auth.oauth_ref) else {
            return TitleGenerationResult::Failure { reason: "token_unavailable" };
        };
        let mut token = match token_provider.get_access_token(false).await {
            Ok(token) => token,
            Err(error) if is_oauth_error(error.as_ref()) => {
                self.log.debug(&format!("chat_title request unavailable: {error}"), None);
                return TitleGenerationResult::Failure { reason: "oauth_unavailable" };
            }
            Err(error) => {
                let _ = error;
                return TitleGenerationResult::Failure { reason: "unexpected" };
            }
        };
        let mut result = self.fetch_hosted_title(&runtime_auth.base_url, &provider, &token, chat_content).await;
        if result.status == Some(401) {
            token = match token_provider.get_access_token(true).await {
                Ok(token) => token,
                Err(error) if is_oauth_error(error.as_ref()) => {
                    self.log.debug(&format!("chat_title request unavailable: {error}"), None);
                    return TitleGenerationResult::Failure { reason: "oauth_unavailable" };
                }
                Err(error) => {
                    let _ = error;
                    return TitleGenerationResult::Failure { reason: "unexpected" };
                }
            };
            result = self.fetch_hosted_title(&runtime_auth.base_url, &provider, &token, chat_content).await;
        }
        if result.kind != ChatTitleKind::Ok {
            self.log.debug(&format!("chat_title request failed: {}", result.message), None);
            return TitleGenerationResult::Failure { reason: "request_failed" };
        }
        let title = take_utf16(&result.title, MAX_GENERATED_TITLE_LENGTH);
        let outcome = if result.title.encode_utf16().count() > MAX_GENERATED_TITLE_LENGTH {
            TitleOutcome::Truncated
        } else {
            TitleOutcome::Ok
        };
        self.apply_title(&title, force, outcome).await
    }

    async fn fetch_hosted_title(
        &self,
        base_url: &str,
        provider: &crate::llm_adapter::provider::provider::ProviderConfig,
        access_token: &str,
        chat_content: &str,
    ) -> ChatTitleResult {
        let mut headers = provider.custom_headers.clone().unwrap_or_default();
        headers.extend(self.host_headers.headers());
        headers.insert("Authorization".to_string(), format!("Bearer {access_token}"));
        let response = ReqwestFetch::new()
            .fetch(
                base_url,
                crate::_base::utils::fetch_like::FetchInit {
                    headers: headers.into_iter().collect(),
                    method: Some("POST".to_string()),
                    body: Some(json!({
                        "messages": [{"role": "user", "content": chat_content}],
                    }).to_string()),
                    signal: None,
                    manual_redirect: false,
                    timeout_ms: None,
                },
            )
            .await;
        match response {
            Ok(response) if response.ok => match response.text().await {
                Ok(text) => match serde_json::from_str::<Value>(&text) {
                    Ok(value) => ChatTitleResult {
                        kind: ChatTitleKind::Ok,
                        title: value.get("title").and_then(Value::as_str).unwrap_or("").to_string(),
                        status: Some(response.status),
                        message: String::new(),
                    },
                    Err(error) => ChatTitleResult {
                        kind: ChatTitleKind::Error,
                        title: String::new(),
                        status: Some(response.status),
                        message: error.to_string(),
                    },
                },
                Err(error) => ChatTitleResult {
                    kind: ChatTitleKind::Error,
                    title: String::new(),
                    status: Some(response.status),
                    message: error.to_string(),
                },
            },
            Ok(response) => ChatTitleResult {
                kind: ChatTitleKind::Error,
                title: String::new(),
                status: Some(response.status),
                message: response.status_text,
            },
            Err(error) => ChatTitleResult {
                kind: ChatTitleKind::Error,
                title: String::new(),
                status: None,
                message: error.to_string(),
            },
        }
    }

    async fn generate_local_title(&self, chat_content: &str, force: bool) -> TitleGenerationResult {
        let Some(agent) = self.agent_lifecycle.handle_of(MAIN_AGENT_ID) else {
            return TitleGenerationResult::Failure { reason: "no_agent" };
        };
        let requester = agent.accessor().get::<crate::agent::llm_requester::llm_requester::IAgentLLMRequesterService>();
        let prompt = [
            "Write a short title for this conversation, at most 40 characters. Reply with the title only: no quotes, no preamble, no trailing period.",
            "",
            chat_content,
        ]
        .join("\n");
        let mut parts = Vec::new();
        let result = requester
            .request(
                crate::agent::llm_requester::llm_requester::LlmRequest {
                    messages: vec![crate::agent::llm_requester::llm_requester::LlmMessage {
                        role: "user".to_string(),
                        content: vec![json!({"type": "text", "text": prompt})],
                        tool_calls: Vec::new(),
                    }],
                    source: crate::agent::llm_requester::llm_requester::LlmRequestSource {
                        type_: "operation".to_string(),
                        request_kind: "session_title".to_string(),
                    },
                },
                Arc::new(move |part| {
                    if part.type_ == "text" {
                        parts.push(part.text.clone());
                    }
                }),
            )
            .await;
        if let Err(error) = result {
            self.log.debug(
                &format!("chat_title local generation failed: {error}"),
                None,
            );
            return TitleGenerationResult::Failure { reason: "request_failed" };
        }
        let title = parts.join("").trim().trim_matches(|ch| matches!(ch, '\'' | '"' | '“' | '”')).to_string();
        if title.is_empty() {
            return TitleGenerationResult::Failure { reason: "request_failed" };
        }
        let outcome = if title.encode_utf16().count() > MAX_GENERATED_TITLE_LENGTH {
            TitleOutcome::Truncated
        } else {
            TitleOutcome::Ok
        };
        let title = take_utf16(&title, MAX_GENERATED_TITLE_LENGTH);
        self.apply_title(&title, force, outcome).await
    }

    async fn apply_title(&self, title: &str, force: bool, outcome: TitleOutcome) -> TitleGenerationResult {
        let applied = match self.metadata.set_generated_title_if_uncustomized(title, Some(force)).await {
            Ok(applied) => applied,
            Err(_) => return TitleGenerationResult::Failure { reason: "unexpected" },
        };
        if !applied {
            return TitleGenerationResult::Failure { reason: "custom_title" };
        }
        self.event_service.publish(SessionMetaUpdated::new(
            crate::session::session_metadata::session_meta_events::SessionMetaUpdatedPayload {
                agent_id: "main".to_string(),
                session_id: self.ctx.session_id(),
                title: title.to_string(),
                patch: json!({"title": title, "isCustomTitle": false}),
            },
        ));
        TitleGenerationResult::Title {
            title: title.to_string(),
            outcome,
        }
    }

    fn clone_handle(&self) -> Self {
        Self {
            ctx: self.ctx.clone(),
            metadata: self.metadata.clone(),
            agent_lifecycle: self.agent_lifecycle.clone(),
            event_service: self.event_service.clone(),
            providers: self.providers.clone(),
            oauth: self.oauth.clone(),
            host_headers: self.host_headers.clone(),
            telemetry: self.telemetry.clone(),
            log: self.log.clone(),
            shared: self.shared.clone(),
        }
    }
}

#[async_trait::async_trait]
impl ISessionTitleService for SessionTitleService {
    async fn generate_title(&self, force: Option<bool>, source: Option<SessionTitleSource>) -> Result<Option<String>, WireAsyncError> {
        SessionTitleService::generate_title(self, force, source).await
    }
}

struct TitleTrackResult {
    outcome: &'static str,
    title_chars: Option<usize>,
    title_outcome: Option<&'static str>,
    reason: Option<&'static str>,
    error_type: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChatTitleKind {
    Ok,
    Error,
}

struct ChatTitleResult {
    kind: ChatTitleKind,
    title: String,
    status: Option<u16>,
    message: String,
}

fn title_input_from_prompts(prompts: &[String]) -> Option<String> {
    if prompts.is_empty() {
        return None;
    }
    Some(
        prompts
            .iter()
            .map(|prompt| format!("user: {}", take_utf16(prompt, MAX_TITLE_USER_SEGMENT)))
            .collect::<Vec<_>>()
            .join("\n")
            .pipe_take_utf16(MAX_TITLE_INPUT_LENGTH),
    )
}

async fn compose_title_input(
    prompt_source: &dyn IAgentTitlePromptSource,
    source: SessionTitleSource,
) -> Option<String> {
    match source {
        SessionTitleSource::FirstTurn => {
            let excerpt = prompt_source.first_turn_excerpt().await.ok()?;
            Some(format!(
                "user: {}\nassistant: {}",
                take_utf16(&excerpt.user?, MAX_TITLE_USER_SEGMENT),
                take_utf16(&excerpt.assistant?, MAX_TITLE_FIRST_TURN_ASSISTANT)
            ))
        }
        SessionTitleSource::Digest => {
            let excerpt = prompt_source.digest_excerpt().await.ok()?;
            let mut turns = Vec::new();
            for turn in excerpt.turns {
                let mut group = vec![format!("user: {}", take_utf16(&turn.user, MAX_TITLE_DIGEST_USER_SEGMENT))];
                if let Some(assistant) = turn.assistant {
                    group.push(format!("assistant: {}", take_utf16(&assistant, MAX_TITLE_DIGEST_ASSISTANT)));
                }
                turns.push(group);
            }
            elide_title_digest_turns(&turns)
        }
        SessionTitleSource::UserPrompts => title_input_from_prompts(&prompt_source.first_user_prompts(MAX_TITLE_PROMPTS).await.ok()?),
    }
}

const TITLE_DIGEST_ELISION_MARKER: &str = "...";

fn elide_title_digest_turns(turns: &[Vec<String>]) -> Option<String> {
    if turns.is_empty() {
        return None;
    }
    let joined = turns.iter().flatten().cloned().collect::<Vec<_>>().join("\n");
    if joined.encode_utf16().count() <= MAX_TITLE_DIGEST_INPUT_LENGTH {
        return Some(joined);
    }
    let mut budget = MAX_TITLE_DIGEST_INPUT_LENGTH - TITLE_DIGEST_ELISION_MARKER.len() - 2;
    let mut head = Vec::new();
    for line in &turns[0] {
        if budget < line.encode_utf16().count() + 1 {
            break;
        }
        head.push(line.clone());
        budget -= line.encode_utf16().count() + 1;
    }
    let mut tail = Vec::new();
    for group in turns.iter().skip(1).rev() {
        let cost = group.iter().map(|line| line.encode_utf16().count() + 1).sum::<usize>();
        if budget < cost {
            break;
        }
        tail.splice(0..0, group.clone());
        budget -= cost;
    }
    Some(
        head
            .into_iter()
            .chain([TITLE_DIGEST_ELISION_MARKER.to_string()])
            .chain(tail)
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

fn pipe_take_utf16(value: String, max: usize) -> String {
    take_utf16(&value, max)
}

fn take_utf16(value: &str, max: usize) -> String {
    let mut units = 0;
    let mut end = value.len();
    for (index, ch) in value.char_indices() {
        let next = units + ch.len_utf16();
        if next > max {
            end = index;
            break;
        }
        units = next;
    }
    value[..end].to_string()
}

fn is_oauth_error(_error: &(dyn Error + 'static)) -> bool {
    true
}

#[allow(dead_code)]
fn _marker(_: &dyn OAuthClientProvider) {}
