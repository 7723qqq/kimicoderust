use std::sync::Arc;

use async_trait::async_trait;
use futures_util::StreamExt;
use parking_lot::Mutex;

use crate::human::llm::errors::{is_abort_error, to_llm_error_message, to_llm_transport_error_message};
use crate::human::llm::finish_reason::FinishInfo;
use crate::human::llm::message::Message;
use crate::human::llm::protocol::base::{ProtocolBase, ProtocolRequesterOptions};
use crate::human::llm::requester::bases::antigravity::antigravity::{
    AntigravityChatProvider, AntigravityGenerateOptions, AntigravityOptions,
    get_antigravity_model_capability,
};
use crate::human::llm::requester::bases::antigravity::r#trait::AntigravityTrait;
use crate::human::llm::requester::requester::{
    LlmRequestConfig, LlmRequestContent, LlmRequestControl, LlmRequestEvent, LlmRequester,
};
use crate::human::llm::thinking::thinking_metadata_of;
use crate::human::llm::usage::TokenUsagePatch;
use crate::wire::record::WireAsyncError;

fn same_message(a: &Message, b: &Message) -> bool {
    if !matches!(
        (a, b),
        (Message::System(_), Message::System(_))
            | (Message::User(_), Message::User(_))
            | (Message::Assistant(_), Message::Assistant(_))
            | (Message::Tool(_), Message::Tool(_))
    ) {
        return false;
    }
    let content = |message: &Message| match message {
        Message::System(message) => &message.content,
        Message::User(message) => &message.content,
        Message::Assistant(message) => &message.content,
        Message::Tool(message) => &message.content,
    };
    let (left, right) = (content(a), content(b));
    if left.len() != right.len()
        || left
            .iter()
            .zip(right)
            .any(|(left, right)| serde_json::to_string(left).unwrap() != serde_json::to_string(right).unwrap())
    {
        return false;
    }
    if let (Message::Tool(left), Message::Tool(right)) = (a, b)
        && left.tool_call_id != right.tool_call_id
    {
        return false;
    }
    if let (Message::Assistant(left), Message::Assistant(right)) = (a, b)
        && (left.tool_calls.len() != right.tool_calls.len()
            || left.tool_calls.iter().zip(&right.tool_calls).any(|(left, right)| {
                serde_json::to_string(left).unwrap() != serde_json::to_string(right).unwrap()
            }))
    {
        return false;
    }
    true
}

fn extends_messages(previous: &[Message], next: &[Message]) -> bool {
    next.len() > previous.len()
        && previous
            .iter()
            .zip(next.iter())
            .all(|(before, after)| same_message(before, after))
}

struct RememberedConversation {
    id: String,
    messages: Vec<Message>,
}

pub struct AntigravityRequester {
    remembered: Mutex<Option<RememberedConversation>>,
}

impl AntigravityRequester {
    fn remote_error(&self, error: &WireAsyncError) -> crate::human::llm::errors::LlmRemoteErrorMessage {
        if is_abort_error(error) {
            to_llm_error_message(error)
        } else {
            to_llm_transport_error_message(&error.to_string())
        }
    }
}

#[async_trait]
impl LlmRequester for AntigravityRequester {
    async fn generate(
        &self,
        config: LlmRequestConfig,
        content: LlmRequestContent,
        control: LlmRequestControl,
    ) -> Result<(), WireAsyncError> {
        (control.on_event)(LlmRequestEvent::Sent);
        let messages = content.messages;
        let resume_from = {
            let remembered = self.remembered.lock();
            remembered.as_ref().and_then(|remembered| {
                extends_messages(&remembered.messages, &messages)
                    .then(|| remembered.id.clone())
            })
        };
        if resume_from.is_none() {
            *self.remembered.lock() = None;
        }
        let provider = AntigravityChatProvider::new(AntigravityOptions {
            model: config.model.model.clone(),
            binary_path: None,
            thinking_effort: (thinking_metadata_of(&config.model)
                .and_then(|metadata| metadata.adaptive_thinking)
                == Some(true))
            .then(|| "high".to_string()),
        });
        let streamed = match provider
            .generate(
                config.system_prompt.as_deref().unwrap_or(""),
                &config.tools.unwrap_or_default(),
                messages.clone(),
                AntigravityGenerateOptions {
                    signal: Some(control.signal.clone()),
                    conversation_id: resume_from,
                    on_request_sent: None,
                },
            )
            .await
        {
            Ok(streamed) => streamed,
            Err(error) => {
                *self.remembered.lock() = None;
                (control.on_event)(LlmRequestEvent::FailedRemote {
                    error: self.remote_error(&error),
                    raw_error: None,
                });
                return Ok(());
            }
        };
        let mut stream = streamed.stream();
        while let Some(part) = stream.next().await {
            match part {
                Ok(part) => (control.on_event)(LlmRequestEvent::StreamingPart { part }),
                Err(error) => {
                    *self.remembered.lock() = None;
                    (control.on_event)(LlmRequestEvent::FailedRemote {
                        error: self.remote_error(&error),
                        raw_error: None,
                    });
                    return Ok(());
                }
            }
        }
        if let Some(id) = streamed.id() {
            (control.on_event)(LlmRequestEvent::StreamingMessageId {
                message_id: id.clone(),
            });
            *self.remembered.lock() = Some(RememberedConversation { id, messages });
        }
        if let Some(usage) = streamed.usage() {
            (control.on_event)(LlmRequestEvent::StreamingUsage {
                usage: TokenUsagePatch {
                    input_other: Some(usage.input_other),
                    output: Some(usage.output),
                    input_cache_read: Some(usage.input_cache_read),
                    input_cache_creation: Some(usage.input_cache_creation),
                    raw: usage.raw,
                },
            });
        }
        (control.on_event)(LlmRequestEvent::StreamingFinish {
            finish: FinishInfo {
                finish_reason: streamed.finish_reason(),
                raw_finish_reason: streamed.raw_finish_reason(),
            },
        });
        (control.on_event)(LlmRequestEvent::Done);
        Ok(())
    }
}

pub fn create_antigravity_requester() -> Arc<AntigravityRequester> {
    Arc::new(AntigravityRequester {
        remembered: Mutex::new(None),
    })
}

pub struct AntigravityBase;

impl ProtocolBase<AntigravityTrait> for AntigravityBase {
    fn capability(&self, _model_name: &str) -> Option<crate::human::llm::capability::ModelCapability> {
        Some(get_antigravity_model_capability())
    }

    fn create_requester(
        &self,
        _options: ProtocolRequesterOptions<AntigravityTrait>,
    ) -> Result<Arc<dyn LlmRequester>, WireAsyncError> {
        Ok(create_antigravity_requester())
    }
}

pub fn create_antigravity_base() -> AntigravityBase {
    AntigravityBase
}

pub static ANTIGRAVITY_BASE: AntigravityBase = AntigravityBase;
