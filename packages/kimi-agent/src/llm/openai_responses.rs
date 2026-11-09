use serde_json::{Value, json};

use crate::llm::request_options::RequestOptions;
use crate::llm::usage::read_openai_cache_fields;
use crate::llm::wire::{StreamDelta, WireMessage};
use crate::rpc::types::TokenUsage;
use kimi_core::types::{ContentBlock, LLMChatResponse, ToolCall, ToolInfo};

const DEVELOPER_ROLE_MODELS: &[&str] = &[
    "gpt-4.1",
    "gpt-4.1-mini",
    "gpt-4.1-nano",
    "gpt-5-codex",
    "o1",
    "o1-mini",
    "o1-pro",
    "o3",
    "o3-mini",
    "o3-pro",
    "o4-mini",
];

fn bare_model_id(model: &str) -> &str {
    model.rsplit('/').next().unwrap_or(model)
}

pub fn uses_developer_role(model: &str) -> bool {
    let normalized = bare_model_id(model).to_ascii_lowercase();
    if DEVELOPER_ROLE_MODELS.contains(&normalized.as_str()) {
        return true;
    }
    DEVELOPER_ROLE_MODELS
        .iter()
        .any(|known| normalized.starts_with(&format!("{known}-")))
}

pub fn build_request_full(
    model: &str,
    messages: &[WireMessage],
    tools: &[ToolInfo],
    stream: bool,
    reasoning_effort: Option<&str>,
) -> Value {
    build_request_full_with_options(
        model,
        messages,
        tools,
        stream,
        reasoning_effort,
        &RequestOptions::default(),
    )
}

pub fn build_request_full_with_options(
    model: &str,
    messages: &[WireMessage],
    tools: &[ToolInfo],
    stream: bool,
    reasoning_effort: Option<&str>,
    options: &RequestOptions,
) -> Value {
    fn reasoning_items(blocks: &[ContentBlock]) -> Vec<Value> {
        let mut items: Vec<Value> = Vec::new();
        let mut current: Option<(Option<&str>, Vec<Value>)> = None;

        for block in blocks {
            let ContentBlock::Think {
                think, encrypted, ..
            } = block
            else {
                flush_reasoning(&mut items, &mut current);
                continue;
            };
            let text = think.as_str();
            let matches = current
                .as_ref()
                .is_some_and(|(open_encrypted, _)| open_encrypted == &encrypted.as_deref());
            if !matches {
                flush_reasoning(&mut items, &mut current);
                current = Some((encrypted.as_deref(), Vec::new()));
            }
            if let Some((_, summaries)) = current.as_mut()
                && !text.is_empty()
            {
                summaries.push(json!({ "type": "summary_text", "text": text }));
            }
        }
        flush_reasoning(&mut items, &mut current);
        items
    }

    fn flush_reasoning(items: &mut Vec<Value>, current: &mut Option<(Option<&str>, Vec<Value>)>) {
        let Some((encrypted, summaries)) = current.take() else {
            return;
        };
        if summaries.is_empty() && encrypted.is_none() {
            return;
        }
        let mut item = json!({ "type": "reasoning", "summary": summaries });
        if let Some(value) = encrypted {
            item["encrypted_content"] = json!(value);
        }
        items.push(item);
    }
    let mut input: Vec<Value> = Vec::new();
    let dev_role = uses_developer_role(model);
    let mut instructions: Option<String> = None;

    for (index, m) in messages.iter().enumerate() {
        match m.role.as_str() {
            "system" => {
                if index == 0 && instructions.is_none() && !m.content.is_empty() {
                    instructions = Some(m.content.clone());
                    continue;
                }
                if m.content.is_empty() && m.blocks.is_empty() {
                    continue;
                }
                let role = if dev_role { "developer" } else { "system" };
                input.push(json!({
                    "type": "message",
                    "role": role,
                    "content": m.content,
                }));
            }
            "assistant" => {
                for item in reasoning_items(&m.blocks) {
                    input.push(item);
                }
                if !m.content.is_empty() {
                    input.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": m.content,
                    }));
                }
                for tc in &m.tool_calls {
                    input.push(json!({
                        "type": "function_call",
                        "call_id": tc.id,
                        "name": tc.name,
                        "arguments": tc.arguments.to_string(),
                    }));
                }
            }
            "tool" => {
                let call_id = m.tool_call_id.as_deref().unwrap_or("call_default");
                let output = if m.blocks.is_empty() {
                    json!(m.content)
                } else {
                    json!(function_output_items(&m.blocks))
                };
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": output,
                }));
            }
            _ => {
                if !m.blocks.is_empty() {
                    let mut parts = Vec::new();
                    for b in &m.blocks {
                        match b {
                            ContentBlock::Text { text, .. } => {
                                parts.push(json!({ "type": "input_text", "text": text }));
                            }
                            ContentBlock::Image {
                                media_type, data, ..
                            } => {
                                parts.push(json!({
                                    "type": "input_image",
                                    "image_url": format!("data:{};base64,{}", media_type, data),
                                }));
                            }
                            _ => {}
                        }
                    }
                    input.push(json!({
                        "type": "message",
                        "role": "user",
                        "content": parts,
                    }));
                } else {
                    input.push(json!({
                        "type": "message",
                        "role": "user",
                        "content": m.content,
                    }));
                }
            }
        }
    }

    let mut req = json!({
        "model": model,
        "input": input,
        "stream": stream,
        "store": false,
    });
    if let Some(instructions) = instructions {
        req["instructions"] = json!(instructions);
    }
    if let Some(seed) = options.seed_for(model, messages) {
        req["seed"] = json!(seed);
    }

    if !tools.is_empty() {
        let tool_defs: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.input_schema,
                })
            })
            .collect();
        req["tools"] = json!(tool_defs);
    }

    if let Some(effort) = crate::llm::effort::wire_reasoning_effort(reasoning_effort) {
        req["reasoning"] = json!({ "effort": effort, "summary": "auto" });
        req["include"] = json!(["reasoning.encrypted_content"]);
    }
    if let Some(format) = options.response_format.as_ref() {
        let mut text = req.get("text").cloned().unwrap_or_else(|| json!({}));
        text["format"] = format.to_responses_text_value();
        req["text"] = text;
    }
    if let Value::Object(map) = &mut req {
        if let Some(temperature) = options.temperature {
            map.entry("temperature".to_string())
                .or_insert(json!(temperature));
        }
        if let Some(top_p) = options.top_p {
            map.entry("top_p".to_string()).or_insert(json!(top_p));
        }
        for (key, value) in &options.extra_params.responses {
            map.insert(key.clone(), value.clone());
        }
    }

    req
}

pub fn build_request_with_budget(
    model: &str,
    messages: &[WireMessage],
    tools: &[ToolInfo],
    stream: bool,
    reasoning_effort: Option<&str>,
    cache_key: Option<&str>,
    max_output_tokens: Option<u32>,
) -> Value {
    build_request_with_budget_and_options(
        model,
        messages,
        tools,
        stream,
        reasoning_effort,
        cache_key,
        max_output_tokens,
        &RequestOptions::default(),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn build_request_with_budget_and_options(
    model: &str,
    messages: &[WireMessage],
    tools: &[ToolInfo],
    stream: bool,
    reasoning_effort: Option<&str>,
    cache_key: Option<&str>,
    max_output_tokens: Option<u32>,
    options: &RequestOptions,
) -> Value {
    let mut req =
        build_request_full_with_options(model, messages, tools, stream, reasoning_effort, options);
    if let Some(cache_key) = cache_key.filter(|key| !key.is_empty()) {
        req["prompt_cache_key"] = json!(cache_key);
    }
    if let Some(cap) = max_output_tokens {
        req["max_output_tokens"] = json!(cap.max(1));
    }
    req
}

const OMITTED_AUDIO_PLACEHOLDER: &str = "(audio omitted: unsupported audio format)";
const OMITTED_VIDEO_PLACEHOLDER: &str = "(video omitted: not supported by this provider)";

fn function_output_items(blocks: &[ContentBlock]) -> Vec<Value> {
    let mut items = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text, .. } => {
                if !text.is_empty() {
                    items.push(json!({ "type": "input_text", "text": text }));
                }
            }
            ContentBlock::Image {
                media_type, data, ..
            } => items.push(json!({
                "type": "input_image",
                "image_url": format!("data:{media_type};base64,{data}"),
            })),
            ContentBlock::ImageUrl { url, .. } => {
                items.push(json!({ "type": "input_image", "image_url": url }));
            }
            ContentBlock::AudioUrl { url, .. } => items.push(map_audio_url(url).unwrap_or_else(
                || json!({ "type": "input_text", "text": OMITTED_AUDIO_PLACEHOLDER }),
            )),
            ContentBlock::VideoUrl { .. } => {
                items.push(json!({ "type": "input_text", "text": OMITTED_VIDEO_PLACEHOLDER }));
            }
            ContentBlock::MediaRef { kind, .. } => items.push(json!({
                "type": "input_text",
                "text": crate::llm::media_resolver::unavailable_text(*kind),
            })),
            ContentBlock::Think { .. } => {}
        }
    }
    items
}

fn map_audio_url(url: &str) -> Option<Value> {
    if let Some(rest) = url.strip_prefix("data:audio/") {
        let (meta, data) = rest.split_once(',')?;
        let subtype = meta.split(';').next()?.to_ascii_lowercase();
        let ext = match subtype.as_str() {
            "mp3" | "mpeg" => "mp3",
            "wav" => "wav",
            _ => return None,
        };
        return Some(json!({
            "type": "input_file",
            "file_data": data,
            "filename": format!("inline.{ext}"),
        }));
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        return Some(json!({ "type": "input_file", "file_url": url }));
    }
    None
}

pub fn parse_response(v: &Value) -> Result<LLMChatResponse, String> {
    let mut content = String::new();
    let mut thinking = Vec::new();
    let mut tool_calls = Vec::new();

    if let Some(output) = v.get("output").and_then(|o| o.as_array()) {
        for item in output {
            let item_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
            match item_type {
                "message" => {
                    if let Some(parts) = item.get("content").and_then(|c| c.as_array()) {
                        for p in parts {
                            if let Some(text) = p.get("text").and_then(|t| t.as_str()) {
                                content.push_str(text);
                            }
                        }
                    }
                }
                "function_call" => {
                    let id = item
                        .get("call_id")
                        .or_else(|| item.get("id"))
                        .and_then(|i| i.as_str())
                        .unwrap_or("call_default")
                        .to_string();
                    let name = item
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .to_string();
                    let (arguments, args_parse_error) = match item.get("arguments") {
                        None => (json!({}), None),
                        Some(Value::String(raw)) => {
                            crate::llm::tool_args::parse_tool_arguments(Some(raw.as_str()))
                        }
                        Some(other) => (other.clone(), None),
                    };
                    tool_calls.push(ToolCall {
                        id,
                        name,
                        arguments,
                        extras: None,
                        args_parse_error,
                    });
                }
                "reasoning" => {
                    let encrypted = item
                        .get("encrypted_content")
                        .and_then(|e| e.as_str())
                        .map(|s| s.to_string());
                    if let Some(summary) = item.get("summary").and_then(|s| s.as_array()) {
                        for sp in summary {
                            if let Some(text) = sp.get("text").and_then(|t| t.as_str()) {
                                thinking.push(ContentBlock::Think {
                                    think: text.to_string(),
                                    encrypted: encrypted.clone(),
                                    details_index: None,
                                    reasoning_key: None,
                                    details_summary: None,
                                });
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    let finish_reason = v
        .get("status")
        .and_then(|s| s.as_str())
        .map(|s| s.to_string());

    let usage = parse_usage(v.get("usage"));

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
    let Some(u) = usage else {
        return TokenUsage::default();
    };

    let raw_input = u.get("input_tokens").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
    let output_tokens = u.get("output_tokens").and_then(|x| x.as_u64()).unwrap_or(0) as u32;

    let split = read_openai_cache_fields(
        None,
        None,
        u.get("input_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|x| x.as_u64())
            .map(|x| x as u32),
        u.get("prompt_cache_hit_tokens")
            .and_then(|x| x.as_u64())
            .map(|x| x as u32),
        u.get("prompt_cache_miss_tokens")
            .and_then(|x| x.as_u64())
            .map(|x| x as u32),
        raw_input,
    );
    let input_cache_read = split.cached();
    let input_tokens = split.input_other(raw_input);

    let total_tokens = u
        .get("total_tokens")
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
    current_call_id: Option<String>,
    current_call_name: Option<String>,
    current_call_args: String,
    tool_calls: Vec<ToolCall>,
    finish_reason: Option<String>,
    usage: TokenUsage,
    error: Option<String>,
    pending_tool_calls: Vec<StreamDelta>,
    reasoning_encrypted: Option<String>,
}

impl StreamAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn take_tool_call_deltas(&mut self) -> Vec<StreamDelta> {
        std::mem::take(&mut self.pending_tool_calls)
    }

    pub fn feed(&mut self, v: &Value) -> Option<StreamDelta> {
        let event_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");

        let nested_usage = v.get("response").and_then(|resp| resp.get("usage"));
        let usage = if event_type == "response.completed" || event_type == "response.incomplete" {
            nested_usage
        } else {
            nested_usage.or_else(|| v.get("usage"))
        };
        if let Some(usage) = usage {
            self.usage = parse_usage(Some(usage));
        }

        match event_type {
            "response.output_text.delta" | "response.text.delta" | "response.output_item.delta" => {
                if let Some(delta) = v.get("delta").and_then(|d| d.as_str()) {
                    self.content.push_str(delta);
                    return Some(StreamDelta::Text(delta.to_string()));
                }
            }
            "response.reasoning.delta" | "response.reasoning_summary_text.delta" => {
                if let Some(delta) = v.get("delta").and_then(|d| d.as_str()) {
                    self.thinking.push_str(delta);
                    return Some(StreamDelta::Think(delta.to_string()));
                }
            }
            "response.output_item.added" => {
                if let Some(item) = v.get("item")
                    && item.get("type").and_then(|t| t.as_str()) == Some("function_call")
                {
                    self.flush_current_tool_call();
                    self.current_call_id = item
                        .get("call_id")
                        .or_else(|| item.get("id"))
                        .and_then(|i| i.as_str())
                        .map(|s| s.to_string());
                    self.current_call_name = item
                        .get("name")
                        .and_then(|n| n.as_str())
                        .map(|s| s.to_string());
                    self.current_call_args.clear();
                }
            }
            "response.function_call_arguments.delta" => {
                if let Some(delta) = v.get("delta").and_then(|d| d.as_str()) {
                    self.current_call_args.push_str(delta);
                    if let Some(id) = self.current_call_id.clone() {
                        self.pending_tool_calls.push(StreamDelta::ToolCall {
                            id,
                            index: None,
                            arguments: delta.to_string(),
                        });
                    }
                }
            }
            "response.output_item.done" => {
                self.flush_current_tool_call();
                if let Some(item) = v.get("item")
                    && item.get("type").and_then(|t| t.as_str()) == Some("reasoning")
                    && let Some(encrypted) = item
                        .get("encrypted_content")
                        .and_then(|e| e.as_str())
                        .filter(|s| !s.is_empty())
                {
                    self.reasoning_encrypted = Some(encrypted.to_string());
                }
            }
            "response.completed" | "response.incomplete" | "response.done" => {
                let resp = v.get("response").unwrap_or(v);
                self.finish_reason = resp
                    .get("status")
                    .and_then(|s| s.as_str())
                    .map(|s| s.to_string());
            }
            "response.failed" => {
                let resp = v.get("response").unwrap_or(v);
                let err_msg = resp
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("response.failed");
                self.finish_reason = Some(format!("failed: {err_msg}"));
                if self.error.is_none() {
                    self.error = Some(format!("responses stream failed: {err_msg}"));
                }
            }
            "error" => {
                let err_msg = v
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("stream error");
                self.finish_reason = Some(format!("failed: {err_msg}"));
                if self.error.is_none() {
                    self.error = Some(format!("responses stream error: {err_msg}"));
                }
            }
            _ => {}
        }

        None
    }

    fn flush_current_tool_call(&mut self) {
        if let Some(name) = self.current_call_name.take() {
            let id = self
                .current_call_id
                .take()
                .unwrap_or_else(|| format!("call_{}", self.tool_calls.len()));
            let (arguments, args_parse_error) =
                crate::llm::tool_args::parse_tool_arguments(Some(self.current_call_args.as_str()));
            self.tool_calls.push(ToolCall {
                id,
                name,
                arguments,
                extras: None,
                args_parse_error,
            });
            self.current_call_args.clear();
        }
    }

    pub fn take_error(&mut self) -> Option<String> {
        self.error.take()
    }

    pub fn finish(mut self) -> LLMChatResponse {
        self.flush_current_tool_call();

        if self.error.is_some() {
            return LLMChatResponse {
                content: String::new(),
                thinking: Vec::new(),
                tool_calls: Vec::new(),
                finish_reason: None,
                usage: self.usage,
                timing: None,
            };
        }

        let mut thinking = Vec::new();
        if !self.thinking.is_empty() || self.reasoning_encrypted.is_some() {
            thinking.push(ContentBlock::Think {
                think: self.thinking,
                encrypted: self.reasoning_encrypted,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tool_result_with_an_image_block_carries_it_in_the_function_output() {
        let messages = vec![
            WireMessage::text("user", "look at this"),
            WireMessage::assistant_tool_calls(
                "reading",
                vec![ToolCall {
                    id: "call_1".into(),
                    name: "readMedia".into(),
                    arguments: json!({ "path": "a.png" }),
                    extras: None,
                    args_parse_error: None,
                }],
            ),
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
                tool_calls: Vec::new(),
                tool_call_id: Some("call_1".into()),
            },
        ];

        let req = build_request_full("gpt-5", &messages, &[], false, None);
        let output = req["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type"] == "function_call_output")
            .expect("the tool result must be a function_call_output item");
        assert_eq!(output["call_id"], "call_1");
        let parts = output["output"]
            .as_array()
            .expect("a tool result with media must carry output items");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["type"], "input_text");
        assert_eq!(parts[1]["type"], "input_image");
        assert_eq!(parts[1]["image_url"], "data:image/png;base64,QUJD");
    }

    #[test]
    fn developer_role_matches_the_v2_model_set_exactly() {
        for model in [
            "gpt-4.1",
            "gpt-4.1-mini",
            "gpt-4.1-nano",
            "gpt-5-codex",
            "o1",
            "o1-mini",
            "o1-pro",
            "o3",
            "o3-mini",
            "o3-pro",
            "o4-mini",
            "o3-mini-2025-01-31",
            "O1",
            "openai/o3-mini",
            "zenmux/o1-pro",
            "OpenAI/O4-Mini",
        ] {
            assert!(uses_developer_role(model), "{model} needs developer role");
        }

        for model in [
            "gpt-4o",
            "gpt-4o-mini",
            "claude-3-opus",
            "my-o1-clone",
            "deepseek-r1",
            "openrouter/openai/gpt-4o",
            "relay/o1x",
        ] {
            assert!(
                !uses_developer_role(model),
                "{model} must not be forced onto the developer role"
            );
        }
    }

    #[test]
    fn assistant_thinking_is_replayed_as_a_reasoning_item() {
        let mut acc = StreamAccumulator::new();
        acc.feed(&json!({
            "type": "response.reasoning_summary_text.delta",
            "delta": "first step"
        }));
        acc.feed(&json!({
            "type": "response.reasoning_summary_text.delta",
            "delta": "second step"
        }));
        acc.feed(&json!({
            "type": "response.output_item.done",
            "item": {
                "type": "reasoning",
                "id": "rs_1",
                "summary": [
                    { "type": "summary_text", "text": "first step" },
                    { "type": "summary_text", "text": "second step" }
                ],
                "encrypted_content": "enc-1"
            }
        }));
        acc.feed(&json!({
            "type": "response.output_text.delta",
            "delta": "the answer is 4"
        }));
        acc.feed(&json!({ "type": "response.completed", "response": { "status": "completed" } }));

        let response = acc.finish();
        assert_eq!(response.content, "the answer is 4");
        let (think, encrypted) = match &response.thinking[0] {
            ContentBlock::Think {
                think, encrypted, ..
            } => (think, encrypted),
            other => panic!("a reasoning turn must carry a Think block: {other:?}"),
        };
        assert_eq!(think, "first stepsecond step");
        assert_eq!(
            encrypted.as_deref(),
            Some("enc-1"),
            "the stream's encrypted_content must survive into the Think block"
        );

        let messages = vec![WireMessage {
            role: "assistant".into(),
            content: response.content.clone(),
            blocks: response.thinking,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }];
        let req = build_request_full("o3-mini", &messages, &[], true, Some("high"));
        assert_eq!(req["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(req["store"], false);
        let input = req["input"].as_array().unwrap();

        assert_eq!(input[0]["type"], "reasoning");
        assert_eq!(input[0]["encrypted_content"], "enc-1");
        let summary = input[0]["summary"].as_array().unwrap();
        assert_eq!(summary.len(), 1, "the deltas coalesce into one item");
        assert_eq!(summary[0]["type"], "summary_text");
        assert_eq!(summary[0]["text"], "first stepsecond step");

        assert_eq!(input[1]["type"], "message");
        assert_eq!(input[1]["content"], "the answer is 4");
    }

    #[test]
    fn an_attestation_only_turn_still_replays_its_reasoning_item() {
        let mut acc = StreamAccumulator::new();
        acc.feed(&json!({
            "type": "response.output_item.done",
            "item": { "type": "reasoning", "id": "rs_2", "encrypted_content": "enc-2" }
        }));
        acc.feed(&json!({ "type": "response.output_text.delta", "delta": "4" }));
        let response = acc.finish();

        let (think, encrypted) = match &response.thinking[0] {
            ContentBlock::Think {
                think, encrypted, ..
            } => (think, encrypted),
            other => panic!("an attestation alone is a Think block: {other:?}"),
        };
        assert!(think.is_empty());
        assert_eq!(encrypted.as_deref(), Some("enc-2"));

        let messages = vec![WireMessage {
            role: "assistant".into(),
            content: response.content.clone(),
            blocks: response.thinking,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }];
        let req = build_request_full("o3-mini", &messages, &[], true, Some("high"));
        let input = req["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], "reasoning");
        assert_eq!(input[0]["encrypted_content"], "enc-2");
        assert_eq!(
            input[0]["summary"],
            json!([]),
            "no summary text was streamed, so none is invented"
        );
    }

    #[test]
    fn test_build_request_and_parse_response() {
        let messages = vec![
            WireMessage::text("system", "Be helpful"),
            WireMessage::text("user", "What is 2+2?"),
        ];
        let tools = vec![ToolInfo {
            name: "Calculator".into(),
            description: "math".into(),
            input_schema: json!({ "type": "object" }),
        }];

        let req = build_request_full("gpt-4o", &messages, &tools, true, Some("low"));
        assert_eq!(req["model"], "gpt-4o");
        assert_eq!(req["instructions"], "Be helpful");
        assert_eq!(
            req["input"].as_array().unwrap().len(),
            1,
            "the system turn lives in `instructions`, not `input`"
        );
        assert_eq!(req["tools"].as_array().unwrap().len(), 1);
        assert_eq!(req["reasoning"]["effort"], "low");

        let resp_json = json!({
            "status": "completed",
            "output": [
                {
                    "type": "message",
                    "content": [{ "type": "text", "text": "4" }]
                }
            ],
            "usage": {
                "input_tokens": 10,
                "output_tokens": 2,
                "total_tokens": 12
            }
        });
        let parsed = parse_response(&resp_json).unwrap();
        assert_eq!(parsed.content, "4");
        assert_eq!(parsed.finish_reason.as_deref(), Some("completed"));
        assert_eq!(parsed.usage.total_tokens, 12);
    }

    #[test]
    fn test_build_request_omits_host_level_efforts() {
        let messages = vec![WireMessage::text("user", "hi")];

        for effort in ["on", "off", ""] {
            let req = build_request_full("gpt-5", &messages, &[], true, Some(effort));
            assert!(
                req.get("reasoning").is_none(),
                "effort {effort:?} must not reach the wire"
            );
        }

        let req_none = build_request_full("gpt-5", &messages, &[], true, Some("none"));
        assert_eq!(req_none["reasoning"]["effort"], "none");
    }

    #[test]
    fn test_stream_accumulator_output_text_and_reasoning() {
        let mut acc = StreamAccumulator::new();
        let delta1 = acc.feed(&json!({
            "type": "response.reasoning_summary_text.delta",
            "delta": "Thinking about it..."
        }));
        assert_eq!(
            delta1,
            Some(StreamDelta::Think("Thinking about it...".into()))
        );

        let delta2 = acc.feed(&json!({
            "type": "response.output_text.delta",
            "delta": "Hello world!"
        }));
        assert_eq!(delta2, Some(StreamDelta::Text("Hello world!".into())));

        acc.feed(&json!({
            "type": "response.completed",
            "response": {
                "status": "stop",
                "usage": {
                    "input_tokens": 15,
                    "output_tokens": 5,
                    "total_tokens": 20
                }
            }
        }));

        let resp = acc.finish();
        assert_eq!(resp.content, "Hello world!");
        assert_eq!(resp.finish_reason.as_deref(), Some("stop"));
        assert_eq!(resp.usage.input_tokens, 15);
        assert_eq!(resp.usage.output_tokens, 5);
        assert_eq!(resp.usage.total_tokens, 20);
        assert_eq!(resp.thinking.len(), 1);
    }

    #[test]
    fn ordinary_events_fall_back_to_top_level_usage_terminal_events_do_not() {
        let mut acc = StreamAccumulator::new();
        acc.feed(&json!({
            "type": "response.output_text.delta",
            "delta": "Hello",
            "usage": { "input_tokens": 7, "output_tokens": 1, "total_tokens": 8 }
        }));
        acc.feed(&json!({
            "type": "response.output_text.delta",
            "delta": " world",
            "response": { "usage": { "input_tokens": 9, "output_tokens": 2, "total_tokens": 11 } },
            "usage": { "input_tokens": 1, "output_tokens": 1, "total_tokens": 2 }
        }));
        acc.feed(&json!({
            "type": "response.completed",
            "usage": { "input_tokens": 100, "output_tokens": 50, "total_tokens": 150 }
        }));

        let resp = acc.finish();
        assert_eq!(resp.usage.input_tokens, 9);
        assert_eq!(resp.usage.output_tokens, 2);
        assert_eq!(resp.usage.total_tokens, 11);
    }

    #[test]
    fn test_stream_accumulator_tool_calls() {
        let mut acc = StreamAccumulator::new();
        acc.feed(&json!({
            "type": "response.output_item.added",
            "item": {
                "type": "function_call",
                "call_id": "call_123",
                "name": "search"
            }
        }));
        acc.feed(&json!({
            "type": "response.function_call_arguments.delta",
            "delta": "{\"query\":"
        }));
        acc.feed(&json!({
            "type": "response.function_call_arguments.delta",
            "delta": "\"rust\"}"
        }));
        acc.feed(&json!({
            "type": "response.output_item.done"
        }));

        let resp = acc.finish();
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].name, "search");
        assert_eq!(resp.tool_calls[0].id, "call_123");
        assert_eq!(resp.tool_calls[0].arguments["query"], "rust");
    }

    #[test]
    fn test_stream_accumulator_failure() {
        let mut acc = StreamAccumulator::new();
        acc.feed(&json!({
            "type": "response.failed",
            "response": {
                "error": {
                    "message": "Rate limit exceeded"
                }
            }
        }));

        let mut resp_acc = acc;
        let err = resp_acc.take_error();
        assert!(
            err.is_some_and(|m| m.contains("Rate limit exceeded")),
            "a failed response must surface as a transport error, not an empty completion"
        );
        let _ = resp_acc.finish();
    }

    #[test]
    fn test_truncated_tool_call_arguments_keep_the_call_for_the_loop_to_answer() {
        let mut acc = StreamAccumulator::new();
        acc.feed(&json!({
            "type": "response.output_item.added",
            "item": {
                "type": "function_call",
                "call_id": "call_9",
                "name": "Bash"
            }
        }));
        acc.feed(&json!({
            "type": "response.function_call_arguments.delta",
            "delta": "{\"command\": \"rm -rf /tmp/x"
        }));

        let mut acc2 = acc;
        assert!(
            acc2.take_error().is_none(),
            "one corrupt call is not a transport failure and must not poison the turn"
        );
        let resp = acc2.finish();
        assert_eq!(
            resp.tool_calls.len(),
            1,
            "the call is kept so the loop can answer it"
        );
        assert_eq!(resp.tool_calls[0].arguments, json!({}));
        assert!(
            resp.tool_calls[0].args_parse_error.is_some(),
            "the reason travels with the call (v2 `parseToolCallArguments`)"
        );
        assert!(resp.content.is_empty());
    }

    #[test]
    fn test_empty_tool_call_arguments_stay_a_valid_no_args_call() {
        let mut acc = StreamAccumulator::new();
        acc.feed(&json!({
            "type": "response.output_item.added",
            "item": {
                "type": "function_call",
                "call_id": "call_10",
                "name": "ListFiles"
            }
        }));
        acc.feed(&json!({ "type": "response.output_item.done" }));

        let mut acc2 = acc;
        assert!(acc2.take_error().is_none());
        let resp = acc2.finish();
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].arguments, json!({}));
    }

    #[test]
    fn test_parse_response_keeps_a_call_whose_arguments_do_not_decode() {
        let bad = json!({
            "status": "completed",
            "output": [{
                "type": "function_call",
                "call_id": "call_1",
                "name": "Write",
                "arguments": "{\"path\": \"/x"
            }]
        });
        let parsed = parse_response(&bad).expect(
            "a corrupt call is not a parse failure — the call is kept so the loop can answer it",
        );
        assert_eq!(parsed.tool_calls.len(), 1);
        assert_eq!(parsed.tool_calls[0].arguments, json!({}));
        assert!(
            parsed.tool_calls[0].args_parse_error.is_some(),
            "the reason travels with the call (v2 `parseToolCallArguments`)"
        );

        let no_args = json!({
            "status": "completed",
            "output": [{
                "type": "function_call",
                "call_id": "call_2",
                "name": "ListFiles"
            }]
        });
        let parsed = parse_response(&no_args).unwrap();
        assert_eq!(parsed.tool_calls[0].arguments, json!({}));
        assert!(
            parsed.tool_calls[0].args_parse_error.is_none(),
            "absent arguments are a valid no-args call, not a failure"
        );
    }

    #[test]
    fn test_responses_developer_role_for_o1_o3() {
        let msgs = vec![
            WireMessage::text("system", "You are an expert coder."),
            WireMessage::text("user", "Hello o3"),
            WireMessage::text("system", "Mid-conversation reminder."),
        ];
        let req_o3 = build_request_full("o3-mini", &msgs, &[], true, None);
        assert_eq!(req_o3["instructions"], "You are an expert coder.");
        let input_o3 = req_o3["input"].as_array().unwrap();
        assert_eq!(input_o3.len(), 2);
        assert_eq!(input_o3[1]["type"], "message");
        assert_eq!(input_o3[1]["role"], "developer");
        assert_eq!(input_o3[1]["content"], "Mid-conversation reminder.");

        let req_4o = build_request_full("gpt-4o", &msgs, &[], true, None);
        let input_4o = req_4o["input"].as_array().unwrap();
        assert_eq!(input_4o[1]["type"], "message");
        assert_eq!(input_4o[1]["role"], "system");
    }

    #[test]
    fn test_responses_usage_subtracts_cached_tokens() {
        let usage = json!({
            "input_tokens": 1000,
            "output_tokens": 50,
            "input_tokens_details": {
                "cached_tokens": 800
            }
        });
        let parsed = parse_usage(Some(&usage));
        assert_eq!(parsed.input_cache_read, 800);
        assert_eq!(parsed.input_tokens, 200);
        assert_eq!(parsed.output_tokens, 50);
    }

    #[test]
    fn test_responses_usage_splits_a_deepseek_cache_hit_and_miss() {
        let usage = json!({
            "input_tokens": 1000,
            "output_tokens": 50,
            "prompt_cache_hit_tokens": 800,
            "prompt_cache_miss_tokens": 200
        });
        let parsed = parse_usage(Some(&usage));
        assert_eq!(parsed.input_cache_read, 800);
        assert_eq!(parsed.input_tokens, 200);
        assert_eq!(parsed.output_tokens, 50);
    }

    #[test]
    fn test_the_system_prompt_is_instructions_not_an_input_item() {
        let msgs = vec![
            WireMessage::text("system", "be brief"),
            WireMessage::text("user", "hi"),
            WireMessage::text("system", ""),
        ];
        let req = build_request_full("gpt-5", &msgs, &[], true, None);
        assert_eq!(req["instructions"], "be brief");
        let input = req["input"].as_array().unwrap();
        assert_eq!(input.len(), 1, "only the user turn remains");
        assert_eq!(input[0]["role"], "user");
        assert!(
            input
                .iter()
                .all(|item| item["role"].as_str() != Some("system")),
            "the system prompt must not be duplicated as an input item"
        );
    }

    #[test]
    fn test_reasoning_requests_the_auto_summary() {
        let msgs = vec![WireMessage::text("user", "hi")];
        let req = build_request_full("gpt-5", &msgs, &[], true, Some("high"));
        assert_eq!(
            req["reasoning"],
            json!({ "effort": "high", "summary": "auto" })
        );
        assert_eq!(req["include"], json!(["reasoning.encrypted_content"]));

        let plain = build_request_full("gpt-5", &msgs, &[], true, None);
        assert!(
            plain.get("reasoning").is_none(),
            "no effort, no reasoning block"
        );
        assert!(plain.get("include").is_none());
    }
}
