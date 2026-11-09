use serde_json::{Value, json};

use crate::llm::wire::{StreamDelta, WireMessage};
use crate::rpc::types::TokenUsage;
use kimi_core::types::{ContentBlock, LLMChatResponse, ToolCall, ToolInfo};

pub fn build_request(
    messages: &[WireMessage],
    tools: &[ToolInfo],
    thinking_budget: Option<u32>,
    include_thoughts: bool,
) -> Value {
    build_request_full(messages, tools, thinking_budget, include_thoughts)
}

pub fn model_supports_thoughts(model_id: &str) -> bool {
    let id = model_id.to_ascii_lowercase();
    id.contains("gemini-2.5")
        || id.contains("gemini-3")
        || id.contains("gemini-4")
        || id.contains("-latest")
        || id.contains("thinking")
}

fn takes_thinking_level(model_id: &str) -> bool {
    let id = model_id.to_ascii_lowercase();
    id.contains("gemini-3") || id.contains("gemini-4")
}

pub fn thinking_off_rejection(model_id: &str, effort: Option<&str>) -> Option<String> {
    let id = model_id.to_ascii_lowercase();
    if !id.contains("gemini-4") {
        return None;
    }
    let token = match effort {
        Some(token @ ("off" | "none")) => token,
        _ => return None,
    };
    Some(format!(
        "Model '{}' always reasons and thinking cannot be turned off. Choose a concrete thinking effort (low, medium, high) instead of '{}'.",
        model_id, token
    ))
}

fn thinking_level_for(model_id: &str, effort: Option<&str>) -> Option<&'static str> {
    if !takes_thinking_level(model_id) {
        return None;
    }
    if model_id.to_ascii_lowercase().contains("gemini-4") && matches!(effort, Some("off" | "none"))
    {
        return None;
    }
    let level = match effort {
        Some("off" | "none") => "MINIMAL",
        Some("minimal") => "MINIMAL",
        Some("low") => "LOW",
        Some("medium") => "MEDIUM",
        Some("high" | "xhigh" | "max") => "HIGH",
        other => {
            let effort = other.unwrap_or("<unset>");
            if other == Some("on") {
                tracing::debug!(
                    model = model_id,
                    "gemini-3 effort 'on' carries no level; includeThoughts only (v2 default arm)"
                );
            } else {
                tracing::warn!(
                    model = model_id,
                    effort,
                    "gemini-3 effort has no thinkingLevel mapping; includeThoughts only (v2 default)"
                );
            }
            return None;
        }
    };
    Some(level)
}

fn thinking_budget_for(model_id: &str, effort: Option<&str>) -> Option<u32> {
    if takes_thinking_level(model_id) {
        return None;
    }
    match effort {
        Some("off" | "none") => Some(0),
        Some("low") => Some(1024),
        Some("medium") => Some(4096),
        Some("high" | "xhigh" | "max") => Some(32000),
        _ => None,
    }
}

pub fn build_request_full(
    messages: &[WireMessage],
    tools: &[ToolInfo],
    thinking_budget: Option<u32>,
    include_thoughts: bool,
) -> Value {
    build_request_for_model(messages, tools, thinking_budget, include_thoughts, "", None)
}

pub fn build_request_for_model(
    messages: &[WireMessage],
    tools: &[ToolInfo],
    thinking_budget: Option<u32>,
    include_thoughts: bool,
    model_id: &str,
    effort: Option<&str>,
) -> Value {
    let mut system = String::new();
    let mut contents: Vec<Value> = Vec::new();
    let mut tool_name_by_id = std::collections::HashMap::new();

    for m in messages {
        match m.role.as_str() {
            "system" => {
                if !system.is_empty() {
                    system.push_str("\n\n");
                }
                system.push_str(&m.content);
            }
            "assistant" => {
                let mut parts: Vec<Value> = Vec::new();
                if !m.content.is_empty() {
                    parts.push(json!({ "text": m.content }));
                }
                for tc in &m.tool_calls {
                    tool_name_by_id.insert(tc.id.clone(), tc.name.clone());
                    let mut fc = json!({
                        "functionCall": {
                            "name": tc.name,
                            "args": tc.arguments,
                        }
                    });
                    if let Some(extras) = &tc.extras
                        && let Some(sig) =
                            extras.get("thought_signature_b64").and_then(|s| s.as_str())
                    {
                        fc["functionCall"]["thought_signature"] = json!(sig);
                    }
                    parts.push(fc);
                }
                contents.push(json!({ "role": "model", "parts": parts }));
            }
            "tool" => {
                let call_id = m.tool_call_id.as_deref().unwrap_or_default();
                let tool_name = tool_name_by_id
                    .get(call_id)
                    .cloned()
                    .unwrap_or_else(|| fallback_tool_name_from_id(call_id));

                let output_text = if m.blocks.is_empty() {
                    m.content.clone()
                } else {
                    m.blocks
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::Text { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("")
                };

                let media_parts: Vec<Value> = m.blocks.iter().filter_map(tool_media_part).collect();
                let part = json!({
                    "functionResponse": {
                        "name": tool_name,
                        "response": { "output": output_text },
                        "parts": media_parts,
                    }
                });
                if let Some(last_msg) = contents.last_mut()
                    && last_msg.get("role").and_then(|r| r.as_str()) == Some("user")
                    && let Some(parts_arr) =
                        last_msg.get_mut("parts").and_then(|p| p.as_array_mut())
                {
                    parts_arr.push(part);
                } else {
                    contents.push(json!({ "role": "user", "parts": [part] }));
                }
            }
            _ => {
                let mut parts: Vec<Value> = Vec::new();
                if !m.content.is_empty() {
                    parts.push(json!({ "text": m.content }));
                }
                for b in &m.blocks {
                    match b {
                        ContentBlock::Text { text, .. } => parts.push(json!({ "text": text })),
                        ContentBlock::Image {
                            media_type, data, ..
                        } => {
                            parts.push(json!({
                                "inlineData": {
                                    "mimeType": media_type,
                                    "data": data,
                                }
                            }));
                        }
                        ContentBlock::ImageUrl { url, .. } => {
                            parts.push(convert_media_url(url, "image/png"));
                        }
                        ContentBlock::AudioUrl { url, .. } => {
                            parts.push(convert_media_url(url, "audio/mpeg"));
                        }
                        ContentBlock::VideoUrl { url, .. } => {
                            parts.push(convert_media_url(url, "video/mp4"));
                        }
                        ContentBlock::Think { .. } => {}
                        ContentBlock::MediaRef { kind, .. } => parts.push(json!({
                            "text": crate::llm::media_resolver::unavailable_text(*kind),
                        })),
                    }
                }
                if parts.is_empty() {
                    parts.push(json!({ "text": "" }));
                }
                if let Some(last_msg) = contents.last_mut()
                    && last_msg.get("role").and_then(|r| r.as_str()) == Some("user")
                    && let Some(parts_arr) =
                        last_msg.get_mut("parts").and_then(|p| p.as_array_mut())
                {
                    parts_arr.extend(parts);
                } else {
                    contents.push(json!({ "role": "user", "parts": parts }));
                }
            }
        }
    }

    let mut req = json!({
        "contents": contents,
    });

    if !system.is_empty() {
        req["systemInstruction"] = json!({
            "parts": [{ "text": system }]
        });
    }

    if !tools.is_empty() {
        let funcs: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.input_schema,
                })
            })
            .collect();
        req["tools"] = json!([{ "functionDeclarations": funcs }]);
    }

    let level = thinking_level_for(model_id, effort);
    let effort_budget = model_supports_thoughts(model_id)
        .then(|| thinking_budget_for(model_id, effort))
        .flatten();
    let budget = if level.is_some() {
        None
    } else if effort_budget == Some(0) {
        Some(0)
    } else {
        thinking_budget
            .filter(|budget| *budget > 0)
            .or(effort_budget)
    };
    if include_thoughts || level.is_some() || budget.is_some() {
        let mut thinking_config = serde_json::Map::new();
        if let Some(level) = level {
            thinking_config.insert("thinkingLevel".to_string(), json!(level));
        }
        if let Some(budget) = budget {
            thinking_config.insert("thinkingBudget".to_string(), json!(budget));
        }
        if include_thoughts && budget != Some(0) {
            thinking_config.insert("includeThoughts".to_string(), json!(true));
        }
        req["generationConfig"] = json!({ "thinkingConfig": Value::Object(thinking_config) });
    }

    req
}

pub fn build_request_for_model_with_budget(
    messages: &[WireMessage],
    tools: &[ToolInfo],
    thinking_budget: Option<u32>,
    include_thoughts: bool,
    model_id: &str,
    effort: Option<&str>,
    max_output_tokens: Option<u32>,
) -> Value {
    let mut req = build_request_for_model(
        messages,
        tools,
        thinking_budget,
        include_thoughts,
        model_id,
        effort,
    );
    if let Some(cap) = max_output_tokens {
        let mut config = req
            .get("generationConfig")
            .cloned()
            .unwrap_or_else(|| json!({}));
        config["maxOutputTokens"] = json!(cap.max(1));
        req["generationConfig"] = config;
    }
    req
}

fn convert_media_url(url: &str, fallback_mime_type: &str) -> Value {
    if let Some(rest) = url.strip_prefix("data:") {
        let (meta, data) = match rest.find(',') {
            Some(idx) => (&rest[..idx], &rest[idx + 1..]),
            None => (rest, ""),
        };
        let mime_type = meta.split(';').next().unwrap_or(fallback_mime_type);
        return json!({ "inlineData": { "mimeType": mime_type, "data": data } });
    }
    let mime_type = guess_mime_from_url(url).unwrap_or(fallback_mime_type);
    json!({ "fileData": { "fileUri": url, "mimeType": mime_type } })
}

fn tool_media_part(block: &ContentBlock) -> Option<Value> {
    match block {
        ContentBlock::Image {
            media_type, data, ..
        } => Some(json!({
            "inlineData": {
                "mimeType": media_type,
                "data": data,
            }
        })),
        ContentBlock::ImageUrl { url, .. } => Some(convert_media_url(url, "image/jpeg")),
        ContentBlock::AudioUrl { url, .. } => Some(convert_media_url(url, "audio/mpeg")),
        ContentBlock::VideoUrl { url, .. } => Some(convert_media_url(url, "video/mp4")),
        ContentBlock::MediaRef { kind, .. } => Some(json!({
            "text": crate::llm::media_resolver::unavailable_text(*kind),
        })),
        _ => None,
    }
}

fn guess_mime_from_url(url: &str) -> Option<&'static str> {
    let path = url::Url::parse(url).ok()?.path().to_ascii_lowercase();
    if path.ends_with(".png") {
        Some("image/png")
    } else if path.ends_with(".jpg") || path.ends_with(".jpeg") {
        Some("image/jpeg")
    } else if path.ends_with(".gif") {
        Some("image/gif")
    } else if path.ends_with(".webp") {
        Some("image/webp")
    } else if path.ends_with(".mp3") || path.ends_with(".mpeg") {
        Some("audio/mpeg")
    } else if path.ends_with(".wav") {
        Some("audio/wav")
    } else if path.ends_with(".ogg") {
        Some("audio/ogg")
    } else if path.ends_with(".mp4") {
        Some("video/mp4")
    } else if path.ends_with(".webm") {
        Some("video/webm")
    } else {
        None
    }
}

pub fn parse_response(v: &Value) -> Result<LLMChatResponse, String> {
    let candidate = v
        .get("candidates")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .ok_or_else(|| "missing candidates array in Google GenAI response".to_string())?;

    let finish_reason = candidate
        .get("finishReason")
        .and_then(|f| f.as_str())
        .map(|fr| match fr {
            "STOP" => "stop".to_string(),
            "MAX_TOKENS" => "length".to_string(),
            _ => fr.to_lowercase(),
        });

    let mut content = String::new();
    let mut thinking = Vec::new();
    let mut tool_calls = Vec::new();

    if let Some(parts) = candidate
        .get("content")
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.as_array())
    {
        for (i, part) in parts.iter().enumerate() {
            let is_thought = part
                .get("thought")
                .and_then(|t| t.as_bool())
                .unwrap_or(false);

            if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                if is_thought {
                    thinking.push(ContentBlock::Think {
                        think: text.to_string(),
                        encrypted: None,
                        details_index: None,
                        reasoning_key: None,
                        details_summary: None,
                    });
                } else {
                    content.push_str(text);
                }
            }

            if let Some(fc) = part.get("functionCall")
                && let Some(name) = fc.get("name").and_then(|n| n.as_str())
            {
                let arguments = fc.get("args").cloned().unwrap_or(json!({}));
                let id = fc
                    .get("id")
                    .and_then(|id| id.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| format!("{}_{}", name, i));
                let extras = part
                    .get("thoughtSignature")
                    .or_else(|| part.get("thought_signature"))
                    .and_then(|s| s.as_str())
                    .map(|sig| json!({ "thought_signature_b64": sig }));
                tool_calls.push(ToolCall {
                    id,
                    name: name.to_string(),
                    arguments,
                    extras,
                    args_parse_error: None,
                });
            }
        }
    }

    let usage = parse_usage(v.get("usageMetadata"));

    Ok(LLMChatResponse {
        content,
        thinking,
        tool_calls,
        finish_reason,
        usage,
        timing: None,
    })
}

fn parse_usage(usage: Option<&Value>) -> TokenUsage {
    let prompt_tokens = usage
        .and_then(|u| u.get("promptTokenCount"))
        .and_then(|x| x.as_u64())
        .unwrap_or(0) as u32;
    let output_tokens = usage
        .and_then(|u| u.get("candidatesTokenCount"))
        .and_then(|x| x.as_u64())
        .unwrap_or(0) as u32;
    let input_cache_read = usage
        .and_then(|u| u.get("cachedContentTokenCount"))
        .and_then(|x| x.as_u64())
        .unwrap_or(0) as u32;
    let input_tokens = prompt_tokens.saturating_sub(input_cache_read);
    let total_tokens = usage
        .and_then(|u| u.get("totalTokenCount"))
        .and_then(|x| x.as_u64())
        .map(|t| t as u32)
        .unwrap_or_else(|| {
            input_tokens
                .saturating_add(input_cache_read)
                .saturating_add(output_tokens)
        });

    TokenUsage {
        input_tokens,
        output_tokens,
        total_tokens,
        input_cache_read,
        input_cache_creation: 0,
    }
}

#[derive(Debug, Default)]
pub struct StreamAccumulator {
    content: String,
    thinking: String,
    tool_calls: Vec<ToolCall>,
    finish_reason: Option<String>,
    usage: TokenUsage,
}

impl StreamAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn take_tool_call_deltas(&mut self) -> Vec<StreamDelta> {
        Vec::new()
    }

    pub fn feed(&mut self, v: &Value) -> Option<StreamDelta> {
        if let Some(usage) = v.get("usageMetadata") {
            self.usage = parse_usage(Some(usage));
        }

        let candidate = v
            .get("candidates")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())?;

        if let Some(fr) = candidate.get("finishReason").and_then(|f| f.as_str()) {
            self.finish_reason = Some(match fr {
                "STOP" => "stop".to_string(),
                "MAX_TOKENS" => "length".to_string(),
                _ => fr.to_lowercase(),
            });
        }

        let parts = candidate
            .get("content")
            .and_then(|c| c.get("parts"))
            .and_then(|p| p.as_array())?;

        let mut returned_delta = None;

        for (i, part) in parts.iter().enumerate() {
            let is_thought = part
                .get("thought")
                .and_then(|t| t.as_bool())
                .unwrap_or(false);

            if let Some(text) = part.get("text").and_then(|t| t.as_str())
                && !text.is_empty()
            {
                if is_thought {
                    self.thinking.push_str(text);
                    if returned_delta.is_none() {
                        returned_delta = Some(StreamDelta::Think(text.to_string()));
                    }
                } else {
                    self.content.push_str(text);
                    if returned_delta.is_none() {
                        returned_delta = Some(StreamDelta::Text(text.to_string()));
                    }
                }
            }

            if let Some(fc) = part.get("functionCall")
                && let Some(name) = fc.get("name").and_then(|n| n.as_str())
            {
                let arguments = fc.get("args").cloned().unwrap_or(json!({}));
                let id = fc
                    .get("id")
                    .and_then(|id| id.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| format!("{}_{}", name, self.tool_calls.len() + i));
                let extras = part
                    .get("thoughtSignature")
                    .or_else(|| part.get("thought_signature"))
                    .and_then(|s| s.as_str())
                    .map(|sig| json!({ "thought_signature_b64": sig }));
                self.tool_calls.push(ToolCall {
                    id,
                    name: name.to_string(),
                    arguments,
                    extras,
                    args_parse_error: None,
                });
            }
        }

        returned_delta
    }

    pub fn finish(self) -> LLMChatResponse {
        let mut thinking = Vec::new();
        if !self.thinking.is_empty() {
            thinking.push(ContentBlock::Think {
                think: self.thinking,
                encrypted: None,
                details_index: None,
                reasoning_key: None,
                details_summary: None,
            });
        }

        LLMChatResponse {
            content: self.content,
            thinking,
            tool_calls: self.tool_calls,
            finish_reason: self.finish_reason,
            usage: self.usage,
            timing: None,
        }
    }
}

fn fallback_tool_name_from_id(call_id: &str) -> String {
    if call_id.is_empty() {
        return "tool".to_string();
    }
    if let Some(pos) = call_id.rfind('_') {
        let prefix = &call_id[..pos];
        if !prefix.is_empty() {
            return prefix.to_string();
        }
    }
    call_id.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gemini_3_carries_the_effort_as_a_thinking_level() {
        let messages = vec![WireMessage::text("user", "hi")];
        for (effort, level) in [
            ("off", "MINIMAL"),
            ("minimal", "MINIMAL"),
            ("low", "LOW"),
            ("medium", "MEDIUM"),
            ("high", "HIGH"),
            ("xhigh", "HIGH"),
            ("max", "HIGH"),
        ] {
            let req = build_request_for_model(
                &messages,
                &[],
                Some(2048),
                true,
                "gemini-3-pro",
                Some(effort),
            );
            assert_eq!(
                req["generationConfig"]["thinkingConfig"]["thinkingLevel"], level,
                "effort {effort} must reach gemini-3 as thinkingLevel {level}"
            );
            assert_eq!(
                req["generationConfig"]["thinkingConfig"]["includeThoughts"],
                true
            );
            assert!(
                req["generationConfig"]["thinkingConfig"]
                    .get("thinkingBudget")
                    .is_none(),
                "effort {effort} resolved a level, so no budget may ride along: {}",
                req["generationConfig"]["thinkingConfig"]
            );
        }

        let req = build_request_for_model(
            &messages,
            &[],
            Some(2048),
            true,
            "gemini-2.5-pro",
            Some("high"),
        );
        assert_eq!(
            req["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            2048
        );
        assert!(
            req["generationConfig"]["thinkingConfig"]
                .get("thinkingLevel")
                .is_none()
        );

        for effort in [Some("on"), None, Some("4096")] {
            let req = build_request_for_model(&messages, &[], None, true, "gemini-3-pro", effort);
            let config = &req["generationConfig"]["thinkingConfig"];
            assert_eq!(
                config["includeThoughts"], true,
                "effort {effort:?} still asks for thoughts"
            );
            assert!(
                config.get("thinkingLevel").is_none(),
                "effort {effort:?} has no level to send: {config}"
            );
        }
    }

    #[test]
    fn gemini_3_off_effort_sends_the_level_without_asking_for_thoughts() {
        let messages = vec![WireMessage::text("user", "hi")];
        for effort in ["off", "none"] {
            let req =
                build_request_for_model(&messages, &[], None, false, "gemini-3-pro", Some(effort));
            let config = &req["generationConfig"]["thinkingConfig"];
            assert_eq!(
                config["thinkingLevel"], "MINIMAL",
                "effort {effort} must still reach gemini-3 as a level"
            );
            assert!(
                config.get("includeThoughts").is_none(),
                "a disabled effort must not ask for thoughts: {config}"
            );
            assert!(
                config.get("thinkingBudget").is_none(),
                "a level is the whole depth on gemini-3: {config}"
            );
        }
    }

    #[test]
    fn gemini_2_5_carries_the_effort_as_a_thinking_budget() {
        let messages = vec![WireMessage::text("user", "hi")];

        for effort in ["off", "none"] {
            let req = build_request_for_model(
                &messages,
                &[],
                None,
                false,
                "gemini-2.5-pro",
                Some(effort),
            );
            let config = &req["generationConfig"]["thinkingConfig"];
            assert_eq!(
                config["thinkingBudget"], 0,
                "effort {effort} must send the budget that disables thinking: {req}"
            );
            assert!(
                config.get("includeThoughts").is_none(),
                "a disabled effort must not ask for thoughts: {config}"
            );
            assert!(
                config.get("thinkingLevel").is_none(),
                "a pre-gemini-3 model takes no level: {config}"
            );
        }

        for (effort, budget) in [
            ("low", 1024),
            ("medium", 4096),
            ("high", 32000),
            ("xhigh", 32000),
            ("max", 32000),
        ] {
            let req =
                build_request_for_model(&messages, &[], None, true, "gemini-2.5-pro", Some(effort));
            let config = &req["generationConfig"]["thinkingConfig"];
            assert_eq!(
                config["thinkingBudget"], budget,
                "effort {effort} must bound thinking at {budget} tokens: {config}"
            );
            assert_eq!(
                config["includeThoughts"], true,
                "effort {effort} must still ask for the thoughts: {config}"
            );
            assert!(
                config.get("thinkingLevel").is_none(),
                "a pre-gemini-3 model takes no level: {config}"
            );
        }

        for effort in [Some("on"), Some("4096")] {
            let req = build_request_for_model(&messages, &[], None, true, "gemini-2.5-pro", effort);
            let config = &req["generationConfig"]["thinkingConfig"];
            assert_eq!(
                config["includeThoughts"], true,
                "effort {effort:?} still asks for thoughts: {config}"
            );
            assert!(
                config.get("thinkingBudget").is_none(),
                "effort {effort:?} has no rung, so the model picks the depth: {config}"
            );
        }

        let req = build_request_for_model(&messages, &[], None, false, "gemini-2.5-pro", None);
        assert!(
            req.get("generationConfig").is_none(),
            "no effort is no thinking state: {req}"
        );

        for effort in ["off", "high"] {
            let req = build_request_for_model(
                &messages,
                &[],
                None,
                false,
                "gemini-2.0-flash",
                Some(effort),
            );
            assert!(
                req.get("generationConfig").is_none(),
                "a model that cannot think gets no thinkingConfig: {req}"
            );
        }
    }

    #[test]
    fn test_build_request_formats_system_contents_tools() {
        let messages = vec![
            WireMessage::text("system", "You are a bot"),
            WireMessage::text("user", "Hello"),
            WireMessage::assistant_tool_calls(
                "Let me check",
                vec![ToolCall {
                    id: "call_1".into(),
                    name: "Grep".into(),
                    arguments: json!({ "query": "abc" }),
                    extras: None,
                    args_parse_error: None,
                }],
            ),
            WireMessage::tool_result("Grep", "results: none"),
        ];

        let tools = vec![ToolInfo {
            name: "Grep".into(),
            description: "search regex".into(),
            input_schema: json!({ "type": "object" }),
        }];

        let req = build_request_full(&messages, &tools, Some(2048), false);
        assert_eq!(
            req["systemInstruction"]["parts"][0]["text"],
            "You are a bot"
        );

        let contents = req["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 3);
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[0]["parts"][0]["text"], "Hello");

        assert_eq!(contents[1]["role"], "model");
        assert_eq!(contents[1]["parts"][0]["text"], "Let me check");
        assert_eq!(contents[1]["parts"][1]["functionCall"]["name"], "Grep");

        assert_eq!(contents[2]["role"], "user");
        assert_eq!(contents[2]["parts"][0]["functionResponse"]["name"], "Grep");

        let decls = req["tools"][0]["functionDeclarations"].as_array().unwrap();
        assert_eq!(decls[0]["name"], "Grep");

        assert_eq!(
            req["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            2048
        );
    }

    #[test]
    fn test_parse_response_and_stream_accumulator() {
        let mut acc = StreamAccumulator::new();
        let chunk1 = json!({
            "candidates": [{
                "content": {
                    "parts": [{ "thought": true, "text": "Reasoning step" }]
                }
            }]
        });
        let d1 = acc.feed(&chunk1);
        assert_eq!(d1, Some(StreamDelta::Think("Reasoning step".into())));

        let chunk2 = json!({
            "candidates": [{
                "content": {
                    "parts": [{ "text": "Final answer" }]
                },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 15,
                "candidatesTokenCount": 8,
                "totalTokenCount": 23
            }
        });
        let d2 = acc.feed(&chunk2);
        assert_eq!(d2, Some(StreamDelta::Text("Final answer".into())));

        let resp = acc.finish();
        assert_eq!(resp.content, "Final answer");
        assert_eq!(resp.thinking.len(), 1);
        assert_eq!(resp.finish_reason.as_deref(), Some("stop"));
        assert_eq!(resp.usage.input_tokens, 15);
        assert_eq!(resp.usage.output_tokens, 8);
    }

    #[test]
    fn test_parse_response_function_call_id_preservation() {
        let resp_with_id = json!({
            "candidates": [{
                "content": {
                    "parts": [{
                        "functionCall": {
                            "id": "call_exact_123",
                            "name": "Write",
                            "args": { "path": "test.txt", "content": "hello" }
                        }
                    }]
                }
            }]
        });
        let parsed = parse_response(&resp_with_id).unwrap();
        assert_eq!(parsed.tool_calls.len(), 1);
        assert_eq!(parsed.tool_calls[0].id, "call_exact_123");
        assert_eq!(parsed.tool_calls[0].name, "Write");

        let resp_no_id = json!({
            "candidates": [{
                "content": {
                    "parts": [{
                        "functionCall": {
                            "name": "Read",
                            "args": { "path": "test.txt" }
                        }
                    }]
                }
            }]
        });
        let parsed_no_id = parse_response(&resp_no_id).unwrap();
        assert_eq!(parsed_no_id.tool_calls[0].id, "Read_0");
    }

    #[test]
    fn test_build_request_maps_function_response_name_correctly() {
        let msgs = vec![
            WireMessage::text("user", "read this"),
            WireMessage {
                role: "assistant".into(),
                content: "".into(),
                blocks: vec![],
                tool_calls: vec![ToolCall {
                    id: "call_abc_123".into(),
                    name: "read_file".into(),
                    arguments: json!({ "path": "test.txt" }),
                    extras: None,
                    args_parse_error: None,
                }],
                tool_call_id: None,
            },
            WireMessage {
                role: "tool".into(),
                content: "file content here".into(),
                blocks: vec![],
                tool_calls: vec![],
                tool_call_id: Some("call_abc_123".into()),
            },
        ];

        let req = build_request(&msgs, &[], None, false);
        let contents = req["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 3);

        assert_eq!(contents[2]["role"], "user");
        let resp_part = &contents[2]["parts"][0]["functionResponse"];
        assert_eq!(resp_part["name"], "read_file");
        assert!(resp_part["response"].is_object());
        assert_eq!(resp_part["response"]["output"], "file content here");
    }

    #[test]
    fn a_json_text_tool_result_stays_an_output_string() {
        let msgs = vec![
            WireMessage::text("user", "go"),
            WireMessage::tool_result("call_1", "{\"path\":\"a.txt\"}"),
        ];

        let req = build_request(&msgs, &[], None, false);
        let response = req["contents"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|content| content["parts"].as_array().unwrap())
            .find(|part| part.get("functionResponse").is_some())
            .unwrap()["functionResponse"]["response"]
            .clone();
        assert_eq!(response["output"], "{\"path\":\"a.txt\"}");
        assert!(response.get("path").is_none());
    }

    #[test]
    fn a_tool_result_with_an_image_block_carries_it_in_function_response_parts() {
        let msgs = vec![
            WireMessage::text("user", "read this"),
            WireMessage {
                role: "assistant".into(),
                content: "".into(),
                blocks: vec![],
                tool_calls: vec![ToolCall {
                    id: "call_abc_123".into(),
                    name: "readMedia".into(),
                    arguments: json!({ "path": "a.png" }),
                    extras: None,
                    args_parse_error: None,
                }],
                tool_call_id: None,
            },
            WireMessage {
                role: "tool".into(),
                content: "<image path=\"a.png\">".into(),
                blocks: vec![
                    ContentBlock::Text {
                        text: "<image path=\"a.png\">".into(),
                        meta: None,
                    },
                    ContentBlock::Image {
                        media_type: "image/png".into(),
                        data: "QUJD".into(),
                        name: None,
                    },
                ],
                tool_calls: vec![],
                tool_call_id: Some("call_abc_123".into()),
            },
        ];

        let req = build_request(&msgs, &[], None, false);
        let resp_part = &req["contents"][2]["parts"][0]["functionResponse"];
        assert_eq!(resp_part["name"], "readMedia");
        let media = resp_part["parts"]
            .as_array()
            .expect("a tool result with media must carry functionResponse.parts");
        assert_eq!(media.len(), 1);
        assert_eq!(media[0]["inlineData"]["mimeType"], "image/png");
        assert_eq!(media[0]["inlineData"]["data"], "QUJD");
    }

    #[test]
    fn test_build_request_media_urls() {
        let messages = vec![WireMessage::with_blocks(
            "user",
            vec![
                ContentBlock::ImageUrl {
                    url: "https://example.com/photo.png".into(),
                    id: None,
                    name: None,
                },
                ContentBlock::AudioUrl {
                    url: "https://example.com/sound.mp3".into(),
                    id: None,
                    name: None,
                },
                ContentBlock::VideoUrl {
                    url: "data:video/mp4;base64,AAAA".into(),
                    id: None,
                    name: None,
                },
                ContentBlock::ImageUrl {
                    url: "https://example.com/unknown.bin".into(),
                    id: None,
                    name: None,
                },
            ],
        )];

        let req = build_request_full(&messages, &[], None, false);
        let parts = req["contents"][0]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 4);

        assert_eq!(
            parts[0]["fileData"]["fileUri"],
            "https://example.com/photo.png"
        );
        assert_eq!(parts[0]["fileData"]["mimeType"], "image/png");
        assert_eq!(parts[1]["fileData"]["mimeType"], "audio/mpeg");
        assert_eq!(parts[2]["inlineData"]["mimeType"], "video/mp4");
        assert_eq!(parts[2]["inlineData"]["data"], "AAAA");
        assert_eq!(parts[3]["fileData"]["mimeType"], "image/png");
    }

    #[test]
    fn test_thought_signature_roundtrip() {
        let resp = json!({
            "candidates": [{
                "content": {
                    "parts": [{
                        "functionCall": {
                            "name": "Grep",
                            "args": { "query": "x" },
                            "id": "fc_1"
                        },
                        "thoughtSignature": "sig-b64"
                    }]
                }
            }]
        });
        let parsed = parse_response(&resp).unwrap();
        assert_eq!(parsed.tool_calls.len(), 1);
        let extras = parsed.tool_calls[0]
            .extras
            .as_ref()
            .expect("extras captured");
        assert_eq!(extras["thought_signature_b64"], "sig-b64");

        let messages = vec![WireMessage::assistant_tool_calls(
            "",
            vec![ToolCall {
                id: "fc_1".into(),
                name: "Grep".into(),
                arguments: json!({ "query": "x" }),
                extras: Some(json!({ "thought_signature_b64": "sig-b64" })),
                args_parse_error: None,
            }],
        )];
        let req = build_request_full(&messages, &[], None, false);
        let fc = &req["contents"][0]["parts"][0]["functionCall"];
        assert_eq!(fc["name"], "Grep");
        assert_eq!(fc["thought_signature"], "sig-b64");
    }

    #[test]
    fn streaming_accumulator_captures_the_thought_signature() {
        let mut acc = StreamAccumulator::new();
        let delta = acc.feed(&json!({
            "candidates": [{
                "content": {
                    "parts": [{
                        "functionCall": {
                            "name": "Grep",
                            "args": { "query": "x" },
                            "id": "fc_1"
                        },
                        "thoughtSignature": "sig-stream"
                    }]
                }
            }]
        }));
        assert!(
            delta.is_none(),
            "a function-call frame carries no text delta"
        );

        let finished = acc.finish();
        assert_eq!(finished.tool_calls.len(), 1);
        let extras = finished.tool_calls[0]
            .extras
            .as_ref()
            .expect("signature captured while streaming");
        assert_eq!(extras["thought_signature_b64"], "sig-stream");

        let messages = vec![WireMessage::assistant_tool_calls(
            "",
            finished.tool_calls.clone(),
        )];
        let req = build_request_full(&messages, &[], None, false);
        assert_eq!(
            req["contents"][0]["parts"][0]["functionCall"]["thought_signature"],
            "sig-stream"
        );
    }

    #[test]
    fn streaming_accumulator_accepts_the_snake_case_signature() {
        let mut acc = StreamAccumulator::new();
        acc.feed(&json!({
            "candidates": [{
                "content": {
                    "parts": [{
                        "functionCall": { "name": "Grep", "args": {}, "id": "fc_2" },
                        "thought_signature": "sig-snake"
                    }]
                }
            }]
        }));
        let finished = acc.finish();
        let extras = finished.tool_calls[0]
            .extras
            .as_ref()
            .expect("snake_case signature captured");
        assert_eq!(extras["thought_signature_b64"], "sig-snake");
    }

    #[test]
    fn thinking_request_asks_for_thoughts() {
        let messages = vec![WireMessage::text("user", "Hello")];

        let req = build_request_full(&messages, &[], None, true);
        assert_eq!(
            req["generationConfig"]["thinkingConfig"]["includeThoughts"].as_bool(),
            Some(true)
        );
        assert!(
            req["generationConfig"]["thinkingConfig"]
                .get("thinkingBudget")
                .is_none()
        );

        let req = build_request_full(&messages, &[], Some(2048), true);
        assert_eq!(
            req["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            2048
        );
        assert_eq!(
            req["generationConfig"]["thinkingConfig"]["includeThoughts"].as_bool(),
            Some(true)
        );

        let req = build_request_full(&messages, &[], None, false);
        assert!(req.get("generationConfig").is_none());
    }

    #[test]
    fn model_gate_admits_reasoning_families_only() {
        assert!(model_supports_thoughts("gemini-2.5-pro"));
        assert!(model_supports_thoughts("gemini-2.5-flash"));
        assert!(model_supports_thoughts("google/gemini-3-pro-preview"));
        assert!(model_supports_thoughts("gemini-4-pro"));
        assert!(model_supports_thoughts("gemini-flash-latest"));
        assert!(!model_supports_thoughts("gemini-2.0-flash"));
        assert!(!model_supports_thoughts("gemini-1.5-pro"));
    }

    #[test]
    fn gemini_4_uses_thinking_level_and_rejects_off() {
        let messages = vec![WireMessage::text("user", "hi")];
        for (effort, level) in [("low", "LOW"), ("medium", "MEDIUM"), ("high", "HIGH")] {
            let req = build_request_for_model(
                &messages,
                &[],
                Some(2048),
                true,
                "gemini-4-pro",
                Some(effort),
            );
            assert_eq!(
                req["generationConfig"]["thinkingConfig"]["thinkingLevel"], level,
                "effort {effort} must reach gemini-4 as thinkingLevel {level}"
            );
            assert!(
                req["generationConfig"]["thinkingConfig"]
                    .get("thinkingBudget")
                    .is_none(),
                "a level IS the depth on gemini-4, so no budget may ride along"
            );
        }

        let req = build_request_for_model(&messages, &[], None, false, "gemini-4-pro", Some("off"));
        assert!(req.get("generationConfig").is_none());

        assert!(thinking_off_rejection("gemini-4-pro", Some("off")).is_some());
        assert!(thinking_off_rejection("gemini-4-pro", Some("none")).is_some());
        assert!(thinking_off_rejection("gemini-4-pro", Some("low")).is_none());
        assert!(thinking_off_rejection("gemini-3-pro", Some("off")).is_none());
        assert!(thinking_off_rejection("gemini-2.5-pro", Some("off")).is_none());
    }
}
