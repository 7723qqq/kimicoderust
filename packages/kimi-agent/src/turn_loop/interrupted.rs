use kimi_core::types::{ContentBlock, LLMChatResponse, LLMMessage};

fn is_vacuous_content_part(part: &ContentBlock) -> bool {
    match part {
        ContentBlock::Text { text, .. } => text.trim().is_empty(),
        ContentBlock::Think {
            think,
            encrypted,
            details_summary,
            ..
        } => {
            encrypted.is_none()
                && think.trim().is_empty()
                && details_summary
                    .as_deref()
                    .is_none_or(|summary| summary.trim().is_empty())
        }
        _ => false,
    }
}

pub(crate) fn salvage_interrupted_response(response: LLMChatResponse) -> Option<LLMMessage> {
    let content = if response.content.trim().is_empty() {
        String::new()
    } else {
        response.content
    };
    let blocks: Vec<ContentBlock> = response
        .thinking
        .into_iter()
        .filter(|part| !is_vacuous_content_part(part))
        .collect();
    if content.is_empty() && blocks.is_empty() {
        return None;
    }
    Some(LLMMessage {
        role: "assistant".into(),
        content,
        blocks,
        tool_calls: Vec::new(),
        tool_call_id: None,
        prompt_id: None,
        origin: Some(serde_json::json!({ "source": "salvaged" })),
        tools: Vec::new(),
    })
}
