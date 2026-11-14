use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;

use crate::_base::di::scope::{ScopeActivation, register_scoped_service};
use crate::agent::context_memory::context_memory::IAgentContextMemoryService;
use crate::agent::context_memory::hook_parts::is_user_prompt_submit_hook_part;
use crate::agent::context_memory::types::ContextMessage;
use crate::agent::r#loop::r#loop::IAgentLoopService;
use crate::agent::prompt::prompt_metadata_text::{
    prompt_metadata_text_from_content_parts, prompt_metadata_text_from_text,
};
use crate::app::scopes::LifecycleScope;
use crate::human::agent::origin::{annotate_bundled_skill_parts, is_skill_activation_part};
use crate::human::llm::message::ContentPart;
use crate::wire::record::WireAsyncError;

use super::agent_title_prompt_source::{
    IAgentTitlePromptSource, TitleDigestExcerpt, TitleDigestTurn, TitleTurnExcerpt,
};

pub struct AgentTitlePromptSourceService {
    context: Arc<dyn IAgentContextMemoryService>,
    loop_: Arc<dyn IAgentLoopService>,
}

impl AgentTitlePromptSourceService {
    pub fn new(
        context: Arc<dyn IAgentContextMemoryService>,
        loop_: Arc<dyn IAgentLoopService>,
    ) -> Self {
        Self { context, loop_ }
    }

    pub fn first_user_prompts(&self, limit: usize) -> Vec<String> {
        if limit == 0 {
            return Vec::new();
        }
        let mut result = Vec::new();
        let mut seen_message_ids = HashSet::new();
        for message in self.combined_messages() {
            if result.len() >= limit || !is_natural_language_prompt(&message) {
                continue;
            }
            if let Some(id) = message.id.as_deref()
                && !seen_message_ids.insert(id.to_string())
            {
                continue;
            }
            if let Some(text) = prompt_metadata_text_from_user_message(&message) {
                result.push(text);
            }
        }
        result
    }

    pub fn first_turn_excerpt(&self) -> TitleTurnExcerpt {
        let all = self.combined_messages();
        let Some(first_user_index) = all.iter().position(is_natural_language_prompt) else {
            return TitleTurnExcerpt::default();
        };
        let user = prompt_metadata_text_from_user_message(&all[first_user_index]);
        let mut span = Vec::new();
        for message in &all[first_user_index + 1..] {
            if is_natural_language_prompt(message) {
                break;
            }
            span.push(message.clone());
        }
        TitleTurnExcerpt {
            user,
            assistant: final_assistant_text(&span),
        }
    }

    pub fn digest_excerpt(&self) -> TitleDigestExcerpt {
        let all = self.combined_messages();
        let mut seen_message_ids = HashSet::new();
        let mut user_indexes = Vec::new();
        for (index, message) in all.iter().enumerate() {
            if !is_natural_language_prompt(message) {
                continue;
            }
            if let Some(id) = message.id.as_deref()
                && !seen_message_ids.insert(id.to_string())
            {
                continue;
            }
            user_indexes.push(index);
        }
        let mut turns = Vec::new();
        for (index, user_index) in user_indexes.iter().copied().enumerate() {
            let Some(user) = prompt_metadata_text_from_user_message(&all[user_index]) else {
                continue;
            };
            let span_end = user_indexes.get(index + 1).copied().unwrap_or(all.len());
            let assistant = final_assistant_text(&all[user_index + 1..span_end]);
            turns.push(TitleDigestTurn { user, assistant });
        }
        TitleDigestExcerpt { turns }
    }

    fn combined_messages(&self) -> Vec<ContextMessage> {
        let snapshot = self.loop_.snapshot();
        let mut all = self.context.get();
        if let Some(active_prompt_id) = snapshot.active_prompt_id.as_deref()
            && let Some(active_handle) = self.loop_.prompt_handle(active_prompt_id)
        {
            all.push(active_handle.message);
        }
        for item in snapshot.queue {
            if item.meta.as_ref().and_then(|meta| meta.tracked) != Some(true) {
                continue;
            }
            all.push(ContextMessage {
                role: "user".to_string(),
                content: item.message.content.clone(),
                tool_calls: Vec::new(),
                origin: item.meta.and_then(|meta| meta.origin),
                ..Default::default()
            });
        }
        all
    }
}

#[async_trait]
impl IAgentTitlePromptSource for AgentTitlePromptSourceService {
    async fn first_user_prompts(&self, limit: usize) -> Result<Vec<String>, WireAsyncError> {
        Ok(AgentTitlePromptSourceService::first_user_prompts(self, limit))
    }

    async fn first_turn_excerpt(&self) -> Result<TitleTurnExcerpt, WireAsyncError> {
        Ok(AgentTitlePromptSourceService::first_turn_excerpt(self))
    }

    async fn digest_excerpt(&self) -> Result<TitleDigestExcerpt, WireAsyncError> {
        Ok(AgentTitlePromptSourceService::digest_excerpt(self))
    }
}

fn is_natural_language_prompt(message: &ContextMessage) -> bool {
    message.role == "user"
        && message.origin.as_ref().is_none_or(|origin| {
            serde_json::to_value(origin)
                .ok()
                .and_then(|origin| origin.get("kind").and_then(|kind| kind.as_str()).map(ToString::to_string))
                .as_deref()
                == Some("user")
        })
}

fn prompt_metadata_text_from_user_message(message: &ContextMessage) -> Option<String> {
    let origin = message.origin.as_ref().and_then(|origin| serde_json::to_value(origin).ok());
    let bundled = origin
        .as_ref()
        .and_then(|origin| origin.get("skillActivations"))
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default();
    let parts = annotate_bundled_skill_parts(&message.content, bundled)
        .into_iter()
        .filter(|part| !is_skill_activation_part(part) && !is_user_prompt_submit_hook_part(part))
        .collect::<Vec<_>>();
    let client_metadata = origin
        .and_then(|origin| origin.get("clientMetadata").cloned())
        .and_then(|value| serde_json::from_value(value).ok());
    prompt_metadata_text_from_content_parts(&parts, client_metadata)
}

fn final_assistant_text(messages: &[ContextMessage]) -> Option<String> {
    for message in messages.iter().rev() {
        if message.role != "assistant" {
            continue;
        }
        if let Some(text) = assistant_text_from_content_parts(&message.content) {
            return Some(text);
        }
    }
    None
}

fn assistant_text_from_content_parts(parts: &[ContentPart]) -> Option<String> {
    let texts = parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text(text) if !text.text.trim().is_empty() => Some(text.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    if texts.is_empty() {
        return None;
    }
    Some(prompt_metadata_text_from_text(&texts.join("\n")))
}

pub fn register_agent_title_prompt_source_service() {
    register_scoped_service(
        LifecycleScope::Agent,
        ScopeActivation::OnDemand,
        "sessionTitle",
    );
}
