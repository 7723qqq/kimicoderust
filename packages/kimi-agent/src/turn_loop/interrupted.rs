use kimi_core::types::{ContentPart, LLMChatResponse, LLMMessage};

fn is_vacuous_content_part(part: &ContentPart) -> bool {
    match part {
        ContentPart::Text { text, .. } => text.trim().is_empty(),
        ContentPart::Think {
            think,
            encrypted,
            hidden,
            ..
        } => encrypted.is_none() && *hidden != Some(true) && think.trim().is_empty(),
        _ => false,
    }
}

pub(crate) fn salvage_interrupted_response(response: LLMChatResponse) -> Option<LLMMessage> {
    let content = if response.content.trim().is_empty() {
        String::new()
    } else {
        response.content
    };
    let blocks: Vec<ContentPart> = response
        .thinking
        .into_iter()
        .filter(|part| !is_vacuous_content_part(part))
        .collect();
    if content.is_empty() && blocks.is_empty() {
        return None;
    }
    Some(LLMMessage {
        role: kimi_core::types::Role::Assistant,
        content,
        blocks,
        tool_calls: Vec::new(),
        tool_call_id: None,
        prompt_id: None,
        origin: Some(serde_json::json!({ "source": "salvaged" })),
        tools: Vec::new(),
    })
}
