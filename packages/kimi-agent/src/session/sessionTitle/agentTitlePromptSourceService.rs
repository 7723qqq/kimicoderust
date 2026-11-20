use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;

use serde_json::Value;

use crate::agent::context_memory::context_memory::IAgentContextMemoryService;
use crate::agent::context_memory::hook_parts::is_user_prompt_submit_hook_part;
use crate::agent::context_memory::types::{ContextMessage, PromptOrigin};
use crate::agent::r#loop::r#loop::IAgentLoopService;
use crate::agent::prompt::prompt_metadata_text::{
    prompt_metadata_text_from_content_parts, prompt_metadata_text_from_text,
};
use crate::human::agent::origin::{
    BundledSkillActivation, SkillSource, annotate_bundled_skill_parts, is_skill_activation_part,
};
use crate::human::llm::message::ContentPart;
use crate::wire::record::WireAsyncError;

use super::agent_title_prompt_source::{
    I_AGENT_TITLE_PROMPT_SOURCE, IAgentTitlePromptSource, TitleDigestExcerpt, TitleDigestTurn,
    TitleTurnExcerpt,
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
                role: crate::human::llm::message::Role::User,
                content: item.message.content.clone(),
                tool_calls: Vec::new(),
                origin: item
                    .meta
                    .and_then(|meta| meta.origin)
                    .and_then(|origin| serde_json::to_value(origin).ok())
                    .and_then(|origin| serde_json::from_value(origin).ok()),
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
        && message
            .origin
            .as_ref()
            .is_none_or(|origin| origin.kind() == "user")
}

fn prompt_metadata_text_from_user_message(message: &ContextMessage) -> Option<String> {
    let (bundled, client_metadata) = match message.origin.as_ref() {
        Some(PromptOrigin::User(origin)) if origin.kind == "user" => (
            kernel_skill_activations(origin.skill_activations.as_deref()),
            origin.client_metadata.clone(),
        ),
        _ => (Vec::new(), None),
    };
    let parts = annotate_bundled_skill_parts(&message.content, &bundled)
        .into_iter()
        .filter(|part| !is_skill_activation_part(part) && !is_user_prompt_submit_hook_part(part))
        .collect::<Vec<_>>();
    let client_metadata = client_metadata.map(|entries| {
        Value::Array(entries.into_iter().map(Value::Object).collect())
    });
    prompt_metadata_text_from_content_parts(&parts, client_metadata.as_ref())
}

fn kernel_skill_activations(
    activations: Option<&[crate::agent::context_memory::types::BundledSkillActivation]>,
) -> Vec<BundledSkillActivation> {
    activations
        .unwrap_or_default()
        .iter()
        .map(|activation| BundledSkillActivation {
            activation_id: activation.activation_id.clone(),
            skill_name: activation.skill_name.clone(),
            skill_args: activation.skill_args.clone(),
            skill_type: activation.skill_type.clone(),
            skill_path: activation.skill_path.clone(),
            skill_source: activation.skill_source.map(|source| match source {
                crate::agent::context_memory::types::SkillSource::Project => SkillSource::Project,
                crate::agent::context_memory::types::SkillSource::User => SkillSource::User,
                crate::agent::context_memory::types::SkillSource::Extra => SkillSource::Extra,
                crate::agent::context_memory::types::SkillSource::Builtin => SkillSource::Builtin,
            }),
        })
        .collect()
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
            ContentPart::Text { text, .. } if !text.trim().is_empty() => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    if texts.is_empty() {
        return None;
    }
    prompt_metadata_text_from_text(&texts.join("\n"))
}
