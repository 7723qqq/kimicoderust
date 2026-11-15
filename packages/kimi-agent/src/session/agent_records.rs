use serde_json::{Map, Value, json};

use crate::turn_loop::types::{LLM, LLMChatParams, LLMMessage, ToolInfo};

pub struct LlmRequestSpec<'a> {
    pub agent_id: &'a str,
    pub kind: &'a str,
    pub turn_step: Option<String>,
    pub tool_select: bool,
    pub dropped_count: Option<u32>,
}

pub fn sha256_hex(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    format!("{:x}", hasher.finalize())
}

pub fn tools_signature(tools: &[ToolInfo]) -> String {
    let signature: Vec<Value> = tools
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.input_schema,
            })
        })
        .collect();
    serde_json::to_string(&signature).unwrap_or_default()
}

pub fn llm_request_record(
    llm: &dyn LLM,
    params: &LLMChatParams,
    spec: LlmRequestSpec<'_>,
) -> Value {
    let leading_system = params
        .messages
        .first()
        .filter(|message| message.role == kimi_core::types::Role::System);
    let system_prompt =
        leading_system.map_or_else(|| llm.system_prompt(), |message| &message.content);
    let message_count = params.messages.len() - usize::from(leading_system.is_some());
    let mut record = json!({
        "type": "llm.request",
        "agent_id": spec.agent_id,
        "kind": spec.kind,
        "provider": llm.protocol().or_else(|| llm.provider_type()).unwrap_or_else(|| llm.transport()),
        "model": llm.model_name(),
        "toolSelect": spec.tool_select,
        "systemPromptHash": sha256_hex(system_prompt),
        "toolsHash": sha256_hex(&tools_signature(params.tools.as_ref())),
        "messageCount": message_count,
    });
    if system_prompt != llm.system_prompt() {
        record["systemPrompt"] = json!(system_prompt);
    }
    if let Some(max_output) = params.max_output_tokens {
        record["maxOutputTokens"] = json!(max_output);
    }
    if let Some(turn_step) = spec.turn_step {
        record["turnStep"] = json!(turn_step);
    }
    if let Some(alias) = llm.model_alias() {
        record["modelAlias"] = json!(alias);
    }
    if let Some(effort) = llm.thinking_effort() {
        record["thinkingEffort"] = json!(effort);
    }
    if let Some(max_tokens) = llm.effective_max_tokens(params) {
        record["maxTokens"] = json!(max_tokens);
    }
    if let Some(parameters) = llm.request_parameters() {
        if let Some(temperature) = parameters.temperature {
            record["temperature"] = json!(temperature);
        }
        if let Some(top_p) = parameters.top_p {
            record["topP"] = json!(top_p);
        }
        if let Some(beta_api) = parameters.beta_api {
            record["betaApi"] = json!(beta_api);
        }
        if let Some(thinking_keep) = parameters.thinking_keep {
            record["thinkingKeep"] = json!(thinking_keep);
        }
    }
    if let Some(dropped_count) = spec.dropped_count {
        record["droppedCount"] = json!(dropped_count);
    }
    record
}

pub fn goal_blocked_record(agent_id: &str, goal: &Value) -> Option<Value> {
    if goal.get("status").and_then(Value::as_str) != Some("blocked") {
        return None;
    }
    let mut record = json!({
        "type": "goal.update",
        "agent_id": agent_id,
        "status": "blocked",
    });
    if let Some(reason) = goal.get("terminalReason").and_then(Value::as_str) {
        record["reason"] = json!(reason);
    }
    if let Some(wall_clock_ms) = goal.get("wallClockMs").and_then(Value::as_u64) {
        record["wallClockMs"] = json!(wall_clock_ms);
    }
    Some(record)
}

pub fn origin_source(origin: &Value) -> String {
    let kind = origin.get("kind").and_then(Value::as_str);
    match kind {
        Some("user") => "input".to_string(),
        Some("skill_activation") | Some("plugin_command")
            if origin.get("trigger").and_then(Value::as_str) == Some("user-slash") =>
        {
            "input".to_string()
        }
        Some(kind) => kind.to_string(),
        None => "input".to_string(),
    }
}

pub fn content_parts(message: &LLMMessage) -> Vec<Value> {
    let mut parts = Vec::new();
    for block in &message.blocks {
        if let Some(part) = content_part(block) {
            parts.push(part);
        }
    }
    if parts.is_empty() && !message.content.is_empty() {
        parts.push(json!({ "type": "text", "text": message.content }));
    }
    parts
}

fn content_part(block: &kimi_core::types::ContentPart) -> Option<Value> {
    serde_json::to_value(block).ok()
}

fn tool_calls(message: &LLMMessage) -> Vec<Value> {
    message
        .tool_calls
        .iter()
        .map(|call| json!(call))
        .collect()
}

pub fn context_message(message: &LLMMessage) -> Option<Value> {
    if message.role == kimi_core::types::Role::System && !message.tools.is_empty() {
        let mut value = json!({
            "role": "system",
            "content": content_parts(message),
            "toolCalls": [],
            "tools": message.tools,
        });
        if let Some(origin) = message.origin.as_ref().filter(|origin| origin.is_object()) {
            value["origin"] = origin.clone();
        }
        return Some(value);
    }
    if message.role != kimi_core::types::Role::User {
        return None;
    }
    let parts = content_parts(message);
    if parts.is_empty() {
        return None;
    }
    let mut value = json!({
        "role": "user",
        "content": parts,
        "toolCalls": [],
    });
    if let Some(origin) = message.origin.as_ref().filter(|origin| origin.is_object()) {
        value["origin"] = origin.clone();
    }
    Some(value)
}

pub fn history_entry(message: &LLMMessage) -> Option<Value> {
    let parts = content_parts(message);
    let body = match message.role.as_str() {
        "user" => json!({ "role": "user", "content": parts }),
        "system" => json!({ "role": "system", "content": parts }),
        "assistant" => json!({
            "role": "assistant",
            "content": parts,
            "toolCalls": tool_calls(message),
        }),
        "tool" => json!({
            "role": "tool",
            "content": parts,
            "toolCallId": message.tool_call_id.clone().unwrap_or_default(),
        }),
        _ => return None,
    };
    let mut entry = json!({ "message": body });
    if message.role == kimi_core::types::Role::User
        && let Some(origin) = message.origin.as_ref().filter(|origin| origin.is_object())
    {
        entry["meta"] = json!({
            "source": origin_source(origin),
            "origin": origin.clone(),
        });
    } else if message.role == kimi_core::types::Role::Assistant
        && let Some(source) = message
            .origin
            .as_ref()
            .and_then(|origin| origin.get("source"))
            .and_then(Value::as_str)
    {
        entry["meta"] = json!({ "source": source });
    }
    Some(entry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kimi_core::types::{ContentPart, ToolCall};

    #[test]
    fn a_text_user_message_becomes_a_v2_history_entry() {
        let mut message = LLMMessage::user("hello");
        message.origin = Some(json!({ "kind": "injection", "variant": "agents_md" }));
        let entry = history_entry(&message).unwrap();
        assert_eq!(entry["message"]["role"], json!("user"));
        assert_eq!(entry["message"]["content"][0]["text"], json!("hello"));
        assert_eq!(entry["meta"]["source"], json!("injection"));
        assert_eq!(entry["meta"]["origin"]["variant"], json!("agents_md"));
    }

    #[test]
    fn an_assistant_message_carries_stringified_tool_arguments() {
        let mut message = LLMMessage::assistant("");
        message.tool_calls = vec![ToolCall {
            id: "call-1".into(),
            name: "Read".into(),
            arguments: Some(json!({ "path": "a" }).to_string()),
            extras: None,
            raw_id: None,
            _stream_index: None,
        }];
        let entry = history_entry(&message).unwrap();
        assert_eq!(entry["message"]["toolCalls"][0]["type"], json!("function"));
        assert_eq!(entry["message"]["toolCalls"][0]["id"], json!("call-1"));
        assert_eq!(
            entry["message"]["toolCalls"][0]["arguments"],
            json!("{\"path\":\"a\"}")
        );
        assert!(entry.get("meta").is_none());
    }

    #[test]
    fn a_salvaged_assistant_carries_its_source_meta() {
        let mut message = LLMMessage::assistant("partial");
        message.origin = Some(json!({ "source": "salvaged" }));
        let entry = history_entry(&message).unwrap();
        assert_eq!(entry["meta"]["source"], json!("salvaged"));
    }

    #[test]
    fn a_tool_message_keeps_its_call_id() {
        let mut message = LLMMessage::new(kimi_core::types::Role::Tool, "body");
        message.tool_call_id = Some("call-1".into());
        let entry = history_entry(&message).unwrap();
        assert_eq!(entry["message"]["role"], json!("tool"));
        assert_eq!(entry["message"]["toolCallId"], json!("call-1"));
    }

    #[test]
    fn blocks_are_mapped_to_v2_content_parts_including_file_media_urls() {
        let mut message = LLMMessage::assistant("");
        message.blocks = vec![
            ContentPart::Think {
                think: "why".into(),
                encrypted: None,
                details_index: Some(2),
                hidden: None,
                reasoning_key: None,
            },
            crate::llm::media_resolver::media_url_part(
                kimi_core::types::MediaKind::Image,
                "kimi-file://img-1".into(),
                Some("img-1".into()),
                None,
            ),
            crate::llm::media_resolver::file_media_part(
                "file-1".into(),
                kimi_core::types::MediaKind::Image,
            ),
            ContentPart::Text {
                text: "answer".into(),
                meta: None,
            },
        ];
        let entry = history_entry(&message).unwrap();
        let parts = entry["message"]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0]["type"], json!("think"));
        assert_eq!(parts[0]["detailsIndex"], json!(2));
        assert!(parts[0].get("detailsSummary").is_none());
        assert_eq!(parts[1]["imageUrl"]["url"], json!("kimi-file://img-1"));
        assert_eq!(parts[2]["type"], json!("image_url"));
        assert_eq!(parts[2]["imageUrl"]["url"], json!("kimi-file://file-1"));
        assert_eq!(parts[3]["text"], json!("answer"));
    }

    #[test]
    fn the_origin_maps_to_the_v2_source_label() {
        assert_eq!(origin_source(&json!({ "kind": "user" })), "input");
        assert_eq!(
            origin_source(&json!({ "kind": "skill_activation", "trigger": "user-slash" })),
            "input"
        );
        assert_eq!(
            origin_source(&json!({ "kind": "skill_activation", "trigger": "model" })),
            "skill_activation"
        );
        assert_eq!(origin_source(&Value::Null), "input");
    }
}
