use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::human::credentials::credentials::apply_credential;
use crate::human::llm::errors::{is_abort_error, to_llm_error_message};
use crate::human::llm::message::Message;
use crate::human::llm::model::LlmModel;
use crate::human::llm::requester::recovery::LlmRecoveryRecord;
use crate::human::llm::requester::requester::{
    LlmRequestConfig, LlmRequestContent, LlmRequestControl, LlmRequestEvent, LlmRequester,
};
use crate::human::xstate2::{FromCallback, XEvent, from_callback};
use crate::wire::record::WireAsyncError;

#[derive(Clone)]
pub struct LlmInput {
    pub config: LlmRequestConfig,
    pub content: LlmRequestContent,
    pub signal: tokio_util::sync::CancellationToken,
}

#[derive(Clone)]
pub struct MessageResolveContext {
    pub model: LlmModel,
    pub signal: tokio_util::sync::CancellationToken,
}

#[async_trait]
pub trait MessageResolver: Send + Sync {
    fn id(&self) -> &str;
    async fn resolve(
        &self,
        messages: Vec<Message>,
        ctx: MessageResolveContext,
    ) -> Result<Vec<Message>, WireAsyncError>;
}

#[derive(Clone)]
pub enum LlmEvent {
    Request(LlmRequestEvent),
    Sent {
        recovery: Option<LlmRecoveryRecord>,
    },
    Retrying {
        failed_attempt: u32,
        next_attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        error_name: String,
        error_message: String,
        status_code: Option<u16>,
    },
    Recovering {
        strategy: String,
        action: String,
        error_name: String,
        error_message: String,
        status_code: Option<u16>,
    },
}

pub fn create_request_actor(
    requester: Arc<dyn LlmRequester>,
    message_resolvers: Vec<Arc<dyn MessageResolver>>,
) -> FromCallback {
    from_callback(move |input| {
        let requester = requester.clone();
        let message_resolvers = message_resolvers.clone();
        let send_back = input.send_back.clone();
        tokio::spawn(async move {
            let runtime = input.runtime.downcast::<LlmInput>().unwrap();
            let result = async {
                let credential = match &runtime.config.credential_provider {
                    Some(provider) => provider.resolve().await?,
                    None => None,
                };
                let mut config = runtime.config.clone();
                if credential.is_some() {
                    config.model = apply_credential(&config.model, credential);
                }
                let mut messages = runtime.content.messages.clone();
                for resolver in message_resolvers {
                    messages = resolver
                        .resolve(
                            messages,
                            MessageResolveContext {
                                model: config.model.clone(),
                                signal: runtime.signal.clone(),
                            },
                        )
                        .await?;
                }
                requester
                    .generate(
                        config,
                        LlmRequestContent {
                            messages,
                            ..runtime.content.clone()
                        },
                        LlmRequestControl {
                            signal: runtime.signal.clone(),
                            on_event: Arc::new(move |event| {
                                send_back(actor_event_to_xevent(LlmEvent::Request(event)));
                            }),
                        },
                    )
                    .await
            }
            .await;
            if let Err(error) = result {
                if is_abort_error(&error) || runtime.signal.is_cancelled() {
                    return;
                }
                let message = to_llm_error_message(&error);
                input.send_back(actor_event_to_xevent(LlmEvent::Request(
                    LlmRequestEvent::FailedRemote {
                        error: message,
                        raw_error: Some(Value::String(error.to_string())),
                    },
                )));
            }
        });
        Box::new(|| {})
    })
}

pub fn actor_event_to_xevent(event: LlmEvent) -> XEvent {
    match event {
        LlmEvent::Sent { recovery } => XEvent::new(
            "llm.sent",
            recovery.map_or(Value::Null, |recovery| {
                serde_json::json!({
                    "strategy": recovery.strategy,
                    "action": recovery.action,
                })
            }),
        ),
        LlmEvent::Retrying {
            failed_attempt,
            next_attempt,
            max_attempts,
            delay_ms,
            error_name,
            error_message,
            status_code,
        } => XEvent::new(
            "llm.retrying",
            serde_json::json!({
                "failedAttempt": failed_attempt,
                "nextAttempt": next_attempt,
                "maxAttempts": max_attempts,
                "delayMs": delay_ms,
                "errorName": error_name,
                "errorMessage": error_message,
                "statusCode": status_code,
            }),
        ),
        LlmEvent::Recovering {
            strategy,
            action,
            error_name,
            error_message,
            status_code,
        } => XEvent::new(
            "llm.recovering",
            serde_json::json!({
                "strategy": strategy,
                "action": action,
                "errorName": error_name,
                "errorMessage": error_message,
                "statusCode": status_code,
            }),
        ),
        LlmEvent::Request(event) => request_event_to_xevent(event),
    }
}

fn request_event_to_xevent(event: LlmRequestEvent) -> XEvent {
    let (type_, payload) = match event {
        LlmRequestEvent::Sent => ("llm.sent", Value::Null),
        LlmRequestEvent::StreamingHeaders { headers } => (
            "llm.streaming.headers",
            serde_json::json!({ "headers": headers }),
        ),
        LlmRequestEvent::StreamingPart { part } => (
            "llm.streaming.part",
            serde_json::json!({ "part": part }),
        ),
        LlmRequestEvent::StreamingUsage { usage } => (
            "llm.streaming.usage",
            serde_json::json!({ "usage": usage }),
        ),
        LlmRequestEvent::StreamingFinish { finish } => (
            "llm.streaming.finish",
            serde_json::json!({ "finish": finish }),
        ),
        LlmRequestEvent::StreamingMessageId { message_id } => (
            "llm.streaming.message_id",
            serde_json::json!({ "messageId": message_id }),
        ),
        LlmRequestEvent::FailedSyntax { error } => (
            "llm.failed.syntax",
            serde_json::json!({ "error": error }),
        ),
        LlmRequestEvent::FailedRemote { error, raw_error } => (
            "llm.failed.remote",
            serde_json::json!({ "error": error, "rawError": raw_error }),
        ),
        LlmRequestEvent::RequestRetrying => ("llm.request.retrying", Value::Null),
        LlmRequestEvent::Done => ("llm.done", Value::Null),
    };
    XEvent::new(type_, payload)
}
