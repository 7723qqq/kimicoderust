use serde_json::{Value, json};

use crate::llm::model_family::{ignored_sampling_params_for_model, resolve_model_family};
use crate::llm::request_options::{RequestOptions, strip_ignored_sampling};
use crate::llm::usage::read_openai_cache_fields;
use crate::llm::wire::{StreamDelta, WireMessage};
use crate::rpc::types::TokenUsage;
use kimi_core::types::{ContentBlock, LLMChatResponse, ToolCall, ToolInfo};

pub const TOOL_RESULT_MEDIA_PROMPT: &str = "Attached media from tool result:";
pub const TOOL_RESULT_MEDIA_PLACEHOLDER: &str = "(see attached media)";
const OMITTED_AUDIO_PLACEHOLDER: &str = "(audio omitted: not supported by this provider)";
const OMITTED_VIDEO_PLACEHOLDER: &str = "(video omitted: not supported by this provider)";

pub fn normalize_kimi_tools(tools: &[ToolInfo]) -> Result<Vec<ToolInfo>, String> {
    tools
        .iter()
        .map(|tool| {
            Ok(ToolInfo {
                name: tool.name.clone(),
                description: tool.description.clone(),
                input_schema: crate::llm::kimi_schema::normalize_kimi_tool_schema(
                    &tool.input_schema,
                )?,
            })
        })
        .collect()
}

pub fn build_request(model: &str, messages: &[WireMessage], tools: &[ToolInfo]) -> Value {
    build_request_full(model, messages, tools, false, None, None, None)
}

pub fn build_request_with_options(
    model: &str,
    messages: &[WireMessage],
    tools: &[ToolInfo],
    stream: bool,
) -> Value {
    build_request_full(model, messages, tools, stream, None, None, None)
}

pub fn build_request_full(
    model: &str,
    messages: &[WireMessage],
    tools: &[ToolInfo],
    stream: bool,
    reasoning_effort: Option<&str>,
    thinking_keep: Option<&str>,
    reasoning_key: Option<&str>,
) -> Value {
    build_request_for_provider(
        model,
        messages,
        tools,
        stream,
        reasoning_effort,
        thinking_keep,
        reasoning_key,
        None,
        None,
        None,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn build_request_for_provider(
    model: &str,
    messages: &[WireMessage],
    tools: &[ToolInfo],
    stream: bool,
    reasoning_effort: Option<&str>,
    thinking_keep: Option<&str>,
    reasoning_key: Option<&str>,
    provider_type: Option<&str>,
    cache_key: Option<&str>,
    max_tokens: Option<u32>,
    detected_reasoning_key: Option<&str>,
) -> Value {
    build_request_for_provider_with_options(
        model,
        messages,
        tools,
        stream,
        reasoning_effort,
        thinking_keep,
        reasoning_key,
        provider_type,
        cache_key,
        max_tokens,
        detected_reasoning_key,
        &RequestOptions::default(),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn build_request_for_provider_with_options(
    model: &str,
    messages: &[WireMessage],
    tools: &[ToolInfo],
    stream: bool,
    reasoning_effort: Option<&str>,
    thinking_keep: Option<&str>,
    reasoning_key: Option<&str>,
    provider_type: Option<&str>,
    cache_key: Option<&str>,
    max_tokens: Option<u32>,
    detected_reasoning_key: Option<&str>,
    options: &RequestOptions,
) -> Value {
    let is_kimi = provider_type.is_some_and(|value| value.eq_ignore_ascii_case("kimi"));
    let resolved_key = resolve_reasoning_key(messages, reasoning_key, detected_reasoning_key);
    let prepared = if is_kimi {
        std::borrow::Cow::Borrowed(messages)
    } else {
        extract_tool_media(messages)
    };
    let msgs: Vec<Value> = prepared
        .iter()
        .filter(|message| tools.is_empty() || !is_empty_system_projection(message))
        .map(|message| project_message(message, resolved_key.as_deref(), is_kimi))
        .collect();

    let mut req = json!({
        "model": model,
        "messages": msgs,
        "stream": stream,
    });
    if stream {
        req["stream_options"] = json!({ "include_usage": true });
    }
    if let Some(cache_key) = cache_key.filter(|key| !key.is_empty()) {
        req["prompt_cache_key"] = json!(cache_key);
    }
    if is_kimi {
        apply_kimi_thinking(&mut req, reasoning_effort, thinking_keep);
    } else {
        if let Some(effort) = crate::llm::effort::wire_reasoning_effort(reasoning_effort) {
            req["reasoning_effort"] = json!(effort);
        }
        if provider_type.is_none()
            && let Some(keep) = thinking_keep
            && !keep.is_empty()
        {
            let mut thinking = req.get("thinking").cloned().unwrap_or_else(|| json!({}));
            thinking["keep"] = json!(keep);
            req["thinking"] = thinking;
        }
    }
    if let Some(seed) = options.seed_for(model, messages) {
        req["seed"] = json!(seed);
    }
    if let Some(format) = options.response_format.as_ref() {
        req["response_format"] = format.to_openai_value();
    }
    if let Some(cap) = max_tokens {
        let capped = cap.clamp(1, 128 * 1024);
        if is_kimi || uses_max_completion_tokens(model) {
            req["max_completion_tokens"] = json!(capped);
        } else {
            req["max_tokens"] = json!(capped);
        }
    }
    if let Value::Object(map) = &mut req {
        for (key, value) in &options.extra_params.openai {
            map.insert(key.clone(), value.clone());
        }
        let sampling = options.openai_sampling();
        for (key, value) in sampling {
            map.entry(key).or_insert(value);
        }
        strip_ignored_sampling(map, ignored_sampling_params_for_model(model));
        if !tools.is_empty()
            && resolve_model_family(model).is_some_and(|family| family.id == "deepseek")
        {
            map.remove("response_format");
        }
    }

    if !tools.is_empty() {
        let tool_defs: Vec<Value> = tools
            .iter()
            .map(|t| {
                if is_kimi && t.name.starts_with('$') {
                    return json!({
                        "type": "builtin_function",
                        "function": { "name": t.name }
                    });
                }
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema,
                    }
                })
            })
            .collect();
        req["tools"] = json!(tool_defs);
    }

    req
}

fn is_empty_system_projection(message: &WireMessage) -> bool {
    message.role == "system" && message.content.is_empty() && message.blocks.is_empty()
}

fn is_extractable_media(block: &ContentBlock) -> bool {
    match block {
        ContentBlock::Image { .. } | ContentBlock::ImageUrl { .. } => true,
        ContentBlock::VideoUrl { url, .. } => !url.starts_with("data:"),
        _ => false,
    }
}

fn extract_tool_media(messages: &[WireMessage]) -> std::borrow::Cow<'_, [WireMessage]> {
    if !messages
        .iter()
        .any(|message| message.role == "tool" && message.blocks.iter().any(is_extractable_media))
    {
        return std::borrow::Cow::Borrowed(messages);
    }
    let mut out: Vec<WireMessage> = Vec::with_capacity(messages.len());
    let mut index = 0;
    while index < messages.len() {
        if messages[index].role != "tool" {
            out.push(messages[index].clone());
            index += 1;
            continue;
        }
        let start = index;
        while index < messages.len() && messages[index].role == "tool" {
            index += 1;
        }
        let run = &messages[start..index];
        let media: Vec<ContentBlock> = run
            .iter()
            .flat_map(|message| {
                message
                    .blocks
                    .iter()
                    .filter(|block| is_extractable_media(block))
                    .cloned()
            })
            .collect();
        if media.is_empty() {
            out.extend_from_slice(run);
            continue;
        }
        for message in run {
            let mut stripped = message.clone();
            stripped.blocks.retain(|block| !is_extractable_media(block));
            let had_image = message.blocks.iter().any(|block| {
                matches!(
                    block,
                    ContentBlock::Image { .. } | ContentBlock::ImageUrl { .. }
                )
            });
            let has_text = stripped
                .blocks
                .iter()
                .any(|block| matches!(block, ContentBlock::Text { text, .. } if !text.is_empty()));
            let has_audio = stripped
                .blocks
                .iter()
                .any(|block| matches!(block, ContentBlock::AudioUrl { .. }));
            let has_data_video = stripped
                .blocks
                .iter()
                .any(|block| matches!(block, ContentBlock::VideoUrl { url, .. } if url.starts_with("data:")));
            if !has_text && !has_audio && !has_data_video && had_image {
                if stripped.blocks.is_empty() {
                    stripped.content = TOOL_RESULT_MEDIA_PLACEHOLDER.to_string();
                } else {
                    stripped.blocks.insert(
                        0,
                        ContentBlock::Text {
                            text: TOOL_RESULT_MEDIA_PLACEHOLDER.to_string(),
                            meta: None,
                        },
                    );
                }
            }
            out.push(stripped);
        }
        let mut blocks = Vec::with_capacity(media.len() + 1);
        blocks.push(ContentBlock::Text {
            text: TOOL_RESULT_MEDIA_PROMPT.to_string(),
            meta: None,
        });
        blocks.extend(media);
        out.push(WireMessage::with_blocks("user", blocks));
    }
    std::borrow::Cow::Owned(out)
}

fn uses_max_completion_tokens(model: &str) -> bool {
    let name = model.to_ascii_lowercase();
    let o_series = name.strip_prefix('o').is_some_and(|rest| {
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        !digits.is_empty() && matches!(rest.chars().nth(digits.len()), None | Some('-') | Some('.'))
    });
    let gpt_series = name.strip_prefix("gpt-").is_some_and(|rest| {
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty()
            || rest
                .chars()
                .nth(digits.len())
                .is_some_and(|c| c != '-' && c != '.')
        {
            return false;
        }
        digits.len() >= 2 || digits.parse::<u32>().is_ok_and(|version| version >= 5)
    });
    o_series || gpt_series
}

fn apply_kimi_thinking(
    req: &mut Value,
    reasoning_effort: Option<&str>,
    thinking_keep: Option<&str>,
) {
    let effort = reasoning_effort.filter(|value| !value.is_empty());
    let keep = thinking_keep.filter(|value| !value.is_empty());
    if effort.is_none() && keep.is_none() {
        return;
    }
    let mut thinking = match effort {
        Some(value) if value == "off" || value == "none" => json!({ "type": "disabled" }),
        Some("on") | None => json!({ "type": "enabled" }),
        Some(level) => json!({ "type": "enabled", "effort": level }),
    };
    if let Some(keep) = keep {
        thinking["keep"] = json!(keep);
    }
    req["thinking"] = thinking;
}

const REASONING_PROBE_KEYS: [&str; 4] = [
    "reasoning_content",
    "reasoning",
    "reasoning_text",
    "thought",
];

const REASONING_WRITE_KEYS: [&str; 2] = ["reasoning_content", "reasoning"];

pub fn resolve_reasoning_key(
    messages: &[WireMessage],
    declared: Option<&str>,
    detected: Option<&str>,
) -> Option<String> {
    declared
        .map(str::to_string)
        .or_else(|| detected.map(str::to_string))
        .or_else(|| history_reasoning_key(messages))
}

fn history_reasoning_key(messages: &[WireMessage]) -> Option<String> {
    for message in messages.iter().rev() {
        for block in message.blocks.iter().rev() {
            if let ContentBlock::Think {
                reasoning_key: Some(key),
                ..
            } = block
                && key != REASONING_DETAILS_KEY
                && REASONING_WRITE_KEYS.contains(&key.as_str())
            {
                return Some(key.clone());
            }
        }
    }
    None
}

fn project_tool_content(m: &WireMessage, keep_parts: bool) -> Value {
    let parts: Vec<&ContentBlock> = m
        .blocks
        .iter()
        .filter(|block| !matches!(block, ContentBlock::Think { .. }))
        .collect();
    if keep_parts {
        if parts.len() == 1 {
            if let ContentBlock::Text { text, .. } = parts[0] {
                return json!(text);
            }
        }
        if parts.is_empty() {
            return json!(m.content);
        }
        return json!(
            parts
                .iter()
                .map(|block| project_block(block))
                .collect::<Vec<_>>()
        );
    }
    let mut text = String::new();
    let mut text_only = true;
    for block in &parts {
        match block {
            ContentBlock::Text { text: part, .. } => text.push_str(part),
            ContentBlock::MediaRef { kind, .. } => {
                text.push_str(crate::llm::media_resolver::unavailable_text(*kind));
            }
            _ => text_only = false,
        }
    }
    if text_only {
        return if m.blocks.is_empty() {
            json!(m.content)
        } else {
            json!(text)
        };
    }
    let mut lines: Vec<String> = Vec::new();
    if !text.is_empty() {
        lines.push(text);
    }
    if parts
        .iter()
        .any(|block| matches!(block, ContentBlock::AudioUrl { .. }))
    {
        lines.push(OMITTED_AUDIO_PLACEHOLDER.to_string());
    }
    if parts.iter().any(
        |block| matches!(block, ContentBlock::VideoUrl { url, .. } if url.starts_with("data:")),
    ) {
        lines.push(OMITTED_VIDEO_PLACEHOLDER.to_string());
    }
    if lines.is_empty()
        && parts.iter().any(|block| {
            matches!(
                block,
                ContentBlock::Image { .. } | ContentBlock::ImageUrl { .. }
            )
        })
    {
        return json!(TOOL_RESULT_MEDIA_PLACEHOLDER);
    }
    json!(lines.join("\n"))
}

fn project_message(m: &WireMessage, reasoning_key: Option<&str>, tool_keep_parts: bool) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("role".into(), json!(m.role));

    let think_blocks: Vec<&ContentBlock> = if m.role == "assistant" {
        m.blocks
            .iter()
            .filter(|block| matches!(block, ContentBlock::Think { .. }))
            .collect()
    } else {
        Vec::new()
    };
    let mut details: Vec<Value> = Vec::new();
    let mut string_fields: Vec<(String, String)> = Vec::new();
    let mut unstamped = String::new();
    let mut all_thinking = String::new();
    for block in &think_blocks {
        let ContentBlock::Think {
            think,
            encrypted,
            details_index,
            reasoning_key: part_key,
            details_summary,
        } = block
        else {
            continue;
        };
        all_thinking.push_str(think);
        if details_index.is_some() {
            if let Some(summary) = details_summary {
                details.push(json!({ "type": "summary", "summary": summary }));
            } else if !think.is_empty() {
                details.push(json!({ "type": "summary", "summary": think }));
            }
            if let Some(encrypted) = encrypted {
                details.push(json!({ "type": "encrypted", "encrypted": encrypted }));
            }
            continue;
        }
        if let Some(key) = part_key
            .as_deref()
            .filter(|key| *key != REASONING_DETAILS_KEY)
        {
            push_string_field(&mut string_fields, key, think);
            continue;
        }
        unstamped.push_str(think);
    }
    let redirected = reasoning_key.is_some() || !details.is_empty() || !string_fields.is_empty();
    if redirected && !unstamped.is_empty() {
        let key = match reasoning_key {
            Some(key) => Some(key.to_string()),
            None if !details.is_empty() => Some(DEFAULT_REASONING_KEY.to_string()),
            None => None,
        };
        if let Some(key) = key {
            push_string_field(&mut string_fields, &key, &unstamped);
        }
    }

    if m.role == "tool" {
        obj.insert("content".into(), project_tool_content(m, tool_keep_parts));
    } else if !m.blocks.is_empty() {
        let parts: Vec<Value> = m
            .blocks
            .iter()
            .filter(|block| !redirected || !matches!(block, ContentBlock::Think { .. }))
            .map(project_block)
            .collect();
        if parts.is_empty() {
            obj.insert("content".into(), Value::Null);
        } else {
            obj.insert("content".into(), json!(parts));
        }
    } else if m.role == "assistant" && !m.tool_calls.is_empty() && m.content.is_empty() {
        obj.insert("content".into(), Value::Null);
    } else {
        obj.insert("content".into(), json!(m.content));
    }
    if !details.is_empty() {
        obj.insert(REASONING_DETAILS_KEY.into(), json!(details));
        let default_value = string_field(&string_fields, DEFAULT_REASONING_KEY)
            .cloned()
            .unwrap_or_else(|| all_thinking.clone());
        obj.insert(DEFAULT_REASONING_KEY.into(), json!(default_value));
    }
    for (key, value) in &string_fields {
        obj.insert(key.clone(), json!(value));
    }
    if !redirected && !all_thinking.is_empty() {
        if let Some(key) = reasoning_key {
            obj.insert(key.to_string(), json!(all_thinking));
        }
    }

    if !m.tool_calls.is_empty() {
        let tcs: Vec<Value> = m
            .tool_calls
            .iter()
            .map(|tc| {
                json!({
                    "id": tc.id,
                    "type": "function",
                    "function": {
                        "name": tc.name,
                        "arguments": serde_json::to_string(&tc.arguments)
                            .unwrap_or_else(|_| "{}".to_string()),
                    }
                })
            })
            .collect();
        obj.insert("tool_calls".into(), json!(tcs));
    }

    if let Some(ref tcid) = m.tool_call_id {
        obj.insert("tool_call_id".into(), json!(tcid));
    }

    Value::Object(obj)
}

pub const REASONING_DETAILS_KEY: &str = "reasoning_details";

pub const DEFAULT_REASONING_KEY: &str = "reasoning_content";

fn push_string_field(fields: &mut Vec<(String, String)>, key: &str, text: &str) {
    if let Some(entry) = fields.iter_mut().find(|(existing, _)| existing == key) {
        entry.1.push_str(text);
    } else {
        fields.push((key.to_string(), text.to_string()));
    }
}

fn string_field<'a>(fields: &'a [(String, String)], key: &str) -> Option<&'a String> {
    fields
        .iter()
        .find(|(existing, _)| existing == key)
        .map(|(_, value)| value)
}

fn project_block(b: &ContentBlock) -> Value {
    match b {
        ContentBlock::Text { text, .. } => json!({ "type": "text", "text": text }),
        ContentBlock::Image {
            media_type, data, ..
        } => json!({
            "type": "image_url",
            "image_url": { "url": format!("data:{media_type};base64,{data}") },
        }),
        ContentBlock::ImageUrl { url, id, .. } => {
            let mut image = json!({ "url": url });
            if let Some(id) = id {
                image["id"] = Value::String(id.clone());
            }
            json!({ "type": "image_url", "image_url": image })
        }
        ContentBlock::AudioUrl { url, id, .. } => {
            let mut audio = json!({ "url": url });
            if let Some(id) = id {
                audio["id"] = Value::String(id.clone());
            }
            json!({ "type": "audio_url", "audio_url": audio })
        }
        ContentBlock::VideoUrl { url, id, .. } => {
            let mut video = json!({ "url": url });
            if let Some(id) = id {
                video["id"] = Value::String(id.clone());
            }
            json!({ "type": "video_url", "video_url": video })
        }
        ContentBlock::Think { think, .. } => json!({ "type": "text", "text": think }),
        ContentBlock::MediaRef { kind, .. } => json!({
            "type": "text",
            "text": crate::llm::media_resolver::unavailable_text(*kind),
        }),
    }
}

pub fn parse_response(v: &Value) -> Result<LLMChatResponse, String> {
    let choice = v
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .ok_or("openai response missing choices[0]")?;
    let message = choice
        .get("message")
        .ok_or("openai response missing choices[0].message")?;

    let finish_reason = choice
        .get("finish_reason")
        .and_then(|f| f.as_str())
        .map(|s| s.to_string());

    let content = message
        .get("content")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();

    let mut tool_calls = Vec::new();
    if let Some(tcs) = message.get("tool_calls").and_then(|t| t.as_array()) {
        for tc in tcs {
            let id = tc
                .get("id")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let func = tc
                .get("function")
                .ok_or("openai tool_call missing function")?;
            let name = func
                .get("name")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let (arguments, args_parse_error) = match func.get("arguments") {
                Some(serde_json::Value::String(raw)) => {
                    crate::llm::tool_args::parse_tool_arguments(Some(raw.as_str()))
                }
                Some(other) => (other.clone(), None),
                None => crate::llm::tool_args::parse_tool_arguments(None),
            };
            tool_calls.push(ToolCall {
                id,
                name,
                arguments,
                extras: None,
                args_parse_error,
            });
        }
    }

    let mut thinking = Vec::new();
    let reasoning = message
        .get("reasoning_content")
        .and_then(|c| c.as_str())
        .or_else(|| message.get("reasoning").and_then(|c| c.as_str()));
    if let Some(think) = reasoning
        && !think.is_empty()
    {
        thinking.push(ContentBlock::Think {
            think: think.to_string(),
            encrypted: None,
            details_index: None,
            reasoning_key: None,
            details_summary: None,
        });
    }

    Ok(LLMChatResponse {
        content,
        thinking,
        tool_calls,
        finish_reason,
        usage: parse_usage(v.get("usage")),
        timing: None,
    })
}

fn parse_usage(usage: Option<&Value>) -> TokenUsage {
    let Some(u) = usage else {
        return TokenUsage::default();
    };

    let prompt_tokens = u.get("prompt_tokens").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
    let output_tokens = u
        .get("completion_tokens")
        .and_then(|x| x.as_u64())
        .unwrap_or(0) as u32;

    let split = read_openai_cache_fields(
        u.get("cached_tokens")
            .and_then(|x| x.as_u64())
            .map(|x| x as u32),
        u.get("prompt_tokens_details")
            .and_then(|details| details.get("cached_tokens"))
            .and_then(|x| x.as_u64())
            .map(|x| x as u32),
        u.get("input_tokens_details")
            .and_then(|details| details.get("cached_tokens"))
            .and_then(|x| x.as_u64())
            .map(|x| x as u32),
        u.get("prompt_cache_hit_tokens")
            .and_then(|x| x.as_u64())
            .map(|x| x as u32),
        u.get("prompt_cache_miss_tokens")
            .and_then(|x| x.as_u64())
            .map(|x| x as u32),
        prompt_tokens,
    );
    let cached = split.cached();
    let input_tokens = split.input_other(prompt_tokens);

    let total_tokens = u
        .get("total_tokens")
        .and_then(|x| x.as_u64())
        .map(|t| t as u32)
        .unwrap_or_else(|| {
            input_tokens
                .saturating_add(cached)
                .saturating_add(output_tokens)
        });

    TokenUsage {
        input_tokens,
        output_tokens,
        total_tokens,
        input_cache_read: cached,
        input_cache_creation: 0,
    }
}

#[derive(Debug, Default, Clone)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

const MAX_STREAM_TOOL_CALLS: usize = 256;

#[derive(Debug, Default)]
pub struct StreamAccumulator {
    content: String,
    thinking: String,
    stamped: Vec<ContentBlock>,
    saw_reasoning_text: bool,
    tool_calls: Vec<PartialToolCall>,
    finish_reason: Option<String>,
    usage: TokenUsage,
    reasoning_key: Option<String>,
    detected_reasoning_key: Option<&'static str>,
    pending_tool_calls: Vec<StreamDelta>,
}

impl StreamAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn take_tool_call_deltas(&mut self) -> Vec<StreamDelta> {
        std::mem::take(&mut self.pending_tool_calls)
    }

    pub fn with_reasoning_key(mut self, key: Option<&str>) -> Self {
        self.reasoning_key = key.map(str::to_string);
        self
    }

    fn reasoning_delta<'a>(&self, delta: &'a Value) -> Option<&'a str> {
        if let Some(key) = self.reasoning_key.as_deref() {
            return delta.get(key).and_then(|c| c.as_str());
        }
        REASONING_PROBE_KEYS
            .iter()
            .find_map(|key| delta.get(key).and_then(|c| c.as_str()))
    }

    pub fn detected_reasoning_key(&self) -> Option<&'static str> {
        self.detected_reasoning_key
    }

    fn saw_reasoning_text(&self, delta: &Value) -> bool {
        self.reasoning_delta(delta)
            .is_some_and(|text| !text.is_empty())
    }

    fn tool_call_slot(&mut self, index: usize) -> Option<&mut PartialToolCall> {
        if index >= MAX_STREAM_TOOL_CALLS {
            return None;
        }
        while self.tool_calls.len() <= index {
            self.tool_calls.push(PartialToolCall::default());
        }
        self.tool_calls.get_mut(index)
    }

    pub fn feed(&mut self, v: &Value) -> Option<StreamDelta> {
        if let Some(usage) = v.get("usage")
            && !usage.is_null()
        {
            self.usage = parse_usage(Some(usage));
        }
        let choice = v
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())?;
        if let Some(fr) = choice.get("finish_reason").and_then(|f| f.as_str()) {
            self.finish_reason = Some(fr.to_string());
        }
        let delta = choice.get("delta")?;

        if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
            for tc in tcs {
                let index = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                let id_fragment = tc.get("id").and_then(|x| x.as_str());
                let function = tc.get("function");
                let name_fragment = function
                    .and_then(|f| f.get("name"))
                    .and_then(|x| x.as_str());
                let arguments_fragment = function
                    .and_then(|f| f.get("arguments"))
                    .and_then(|x| x.as_str());
                let Some(slot) = self.tool_call_slot(index) else {
                    continue;
                };
                if let Some(id) = id_fragment {
                    slot.id.push_str(id);
                }
                if let Some(name) = name_fragment {
                    slot.name.push_str(name);
                }
                if let Some(args) = arguments_fragment {
                    slot.arguments.push_str(args);
                }
                if let Some(args) = arguments_fragment {
                    let id = slot.id.clone();
                    self.pending_tool_calls.push(StreamDelta::ToolCall {
                        id,
                        index: Some(index),
                        arguments: args.to_string(),
                    });
                }
            }
        }

        let mut think_delta = None;
        if let Some(think) = self.reasoning_delta(delta)
            && !think.is_empty()
        {
            if self.reasoning_key.is_none()
                && self.detected_reasoning_key.is_none()
                && let Some(key) = REASONING_PROBE_KEYS.iter().find(|key| {
                    REASONING_WRITE_KEYS.contains(key)
                        && delta
                            .get(**key)
                            .and_then(|c| c.as_str())
                            .is_some_and(|text| !text.is_empty())
                })
            {
                self.detected_reasoning_key = Some(*key);
            }
            self.thinking.push_str(think);
            think_delta = Some(StreamDelta::Think(think.to_string()));
        }
        if self.saw_reasoning_text(delta) {
            self.saw_reasoning_text = true;
        }
        if self.reasoning_key.is_none() {
            self.stamped
                .extend(reasoning_details_parts(delta, self.saw_reasoning_text));
        }
        if let Some(delta) = think_delta {
            return Some(delta);
        }

        if let Some(text) = delta.get("content").and_then(|c| c.as_str())
            && !text.is_empty()
        {
            self.content.push_str(text);
            return Some(StreamDelta::Text(text.to_string()));
        }

        None
    }

    pub fn finish(self) -> LLMChatResponse {
        let tool_calls = self
            .tool_calls
            .into_iter()
            .filter(|tc| !tc.name.is_empty())
            .map(|tc| {
                let (arguments, args_parse_error) =
                    crate::llm::tool_args::parse_tool_arguments(Some(tc.arguments.as_str()));
                ToolCall {
                    id: tc.id,
                    name: tc.name,
                    arguments,
                    extras: None,
                    args_parse_error,
                }
            })
            .collect();

        let thinking = if self.thinking.is_empty() {
            Vec::new()
        } else {
            vec![ContentBlock::Think {
                think: self.thinking,
                encrypted: None,
                details_index: None,
                reasoning_key: None,
                details_summary: None,
            }]
        };
        let mut thinking = thinking;
        thinking.extend(self.stamped);
        if !self.saw_reasoning_text {
            for block in &mut thinking {
                if let ContentBlock::Think {
                    think,
                    details_summary,
                    ..
                } = block
                    && think.is_empty()
                    && let Some(summary) = details_summary.clone()
                {
                    *think = summary;
                }
            }
        }

        LLMChatResponse {
            content: self.content,
            thinking,
            tool_calls,
            finish_reason: self.finish_reason,
            usage: self.usage,
            timing: None,
        }
    }
}

fn reasoning_details_parts(delta: &Value, _string_dialect_seen: bool) -> Vec<ContentBlock> {
    let Some(array) = delta.get(REASONING_DETAILS_KEY).and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut parts = Vec::new();
    for (index, element) in array.iter().enumerate() {
        let kind = element
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let summary = element.get("summary").and_then(Value::as_str);
        let encrypted = element.get("encrypted").and_then(Value::as_str);
        let stamped = |think: String, encrypted: Option<String>, summary: Option<String>| {
            ContentBlock::Think {
                think,
                encrypted,
                details_index: Some(index as u32),
                reasoning_key: Some(REASONING_DETAILS_KEY.to_string()),
                details_summary: summary,
            }
        };
        if kind != "encrypted" && summary.is_some_and(|text| !text.is_empty()) {
            parts.push(stamped(
                String::new(),
                None,
                Some(summary.unwrap_or_default().to_string()),
            ));
        }
        if kind != "summary" && encrypted.is_some_and(|text| !text.is_empty()) {
            parts.push(stamped(
                String::new(),
                Some(encrypted.unwrap_or_default().to_string()),
                None,
            ));
        }
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::llm::request_options::ResponseFormat;

    #[test]
    fn build_request_projects_roles_tools_and_stringifies_arguments() {
        let messages = vec![
            WireMessage::text("system", "sys"),
            WireMessage::text("user", "hi"),
            WireMessage::assistant_tool_calls(
                "",
                vec![ToolCall {
                    id: "call_1".into(),
                    name: "Read".into(),
                    arguments: json!({ "path": "a.txt" }),
                    extras: None,
                    args_parse_error: None,
                }],
            ),
            WireMessage::tool_result("call_1", "file body"),
        ];
        let tools = vec![ToolInfo {
            name: "Read".into(),
            description: "read a file".into(),
            input_schema: json!({ "type": "object" }),
        }];

        let req = build_request("kimi-k2", &messages, &tools);

        assert_eq!(req["model"], "kimi-k2");
        assert_eq!(req["stream"], false);
        let msgs = req["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 4);

        let assistant = &msgs[2];
        assert!(assistant["content"].is_null());
        let tc = &assistant["tool_calls"][0];
        assert_eq!(tc["id"], "call_1");
        assert_eq!(tc["type"], "function");
        assert_eq!(tc["function"]["name"], "Read");
        let args = tc["function"]["arguments"].as_str().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(args).unwrap(),
            json!({ "path": "a.txt" })
        );

        let tool_msg = &msgs[3];
        assert_eq!(tool_msg["role"], "tool");
        assert_eq!(tool_msg["tool_call_id"], "call_1");
        assert_eq!(tool_msg["content"], "file body");

        assert_eq!(req["tools"][0]["type"], "function");
        assert_eq!(req["tools"][0]["function"]["name"], "Read");
        assert_eq!(
            req["tools"][0]["function"]["parameters"],
            json!({ "type": "object" })
        );
    }

    #[test]
    fn build_request_omits_tools_when_empty() {
        let req = build_request("m", &[WireMessage::text("user", "x")], &[]);
        assert!(req.get("tools").is_none());
    }

    fn tool_media_messages() -> Vec<WireMessage> {
        vec![
            WireMessage::text("user", "look at this"),
            WireMessage::assistant_tool_calls(
                "",
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
        ]
    }

    #[test]
    fn kimi_tool_result_keeps_media_inside_the_tool_message() {
        let messages = tool_media_messages();
        let req = build_request_for_provider(
            "kimi-k2",
            &messages,
            &[],
            false,
            None,
            None,
            None,
            Some("kimi"),
            None,
            None,
            None,
        );

        let msgs = req["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[2]["role"], "tool");
        let parts = msgs[2]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,QUJD");
    }

    #[test]
    fn other_providers_replace_tool_media_with_a_following_user_message() {
        let messages = tool_media_messages();
        let req = build_request_for_provider(
            "gpt-4o",
            &messages,
            &[],
            false,
            None,
            None,
            None,
            Some("openai"),
            None,
            None,
            None,
        );

        let msgs = req["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[2]["role"], "tool");
        assert_eq!(msgs[2]["content"], "<image path=\"a.png\">");
        assert_eq!(msgs[3]["role"], "user");
        let parts = msgs[3]["content"].as_array().unwrap();
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "Attached media from tool result:");
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,QUJD");
    }

    #[test]
    fn an_image_only_tool_result_is_replaced_by_the_placeholder() {
        let messages = vec![
            WireMessage::text("user", "look at this"),
            WireMessage {
                role: "tool".into(),
                content: String::new(),
                blocks: vec![ContentBlock::Image {
                    media_type: "image/png".into(),
                    data: "QUJD".into(),
                    name: None,
                }],
                tool_calls: Vec::new(),
                tool_call_id: Some("call_2".into()),
            },
        ];

        let req = build_request_for_provider(
            "gpt-4o",
            &messages,
            &[],
            false,
            None,
            None,
            None,
            Some("openai"),
            None,
            None,
            None,
        );

        let msgs = req["messages"].as_array().unwrap();
        assert_eq!(msgs[1]["role"], "tool");
        assert_eq!(msgs[1]["content"], "(see attached media)");
        assert_eq!(msgs[2]["role"], "user");
        assert_eq!(msgs[2]["content"][1]["type"], "image_url");
    }

    #[test]
    fn a_text_only_tool_result_is_a_string_on_every_provider() {
        let messages = vec![
            WireMessage::text("user", "read it"),
            WireMessage {
                role: "tool".into(),
                content: "file body".into(),
                blocks: vec![ContentBlock::Text {
                    text: "file body".into(),
                    meta: None,
                }],
                tool_calls: Vec::new(),
                tool_call_id: Some("call_1".into()),
            },
        ];
        for provider in ["kimi", "openai"] {
            let req = build_request_for_provider(
                "m",
                &messages,
                &[],
                false,
                None,
                None,
                None,
                Some(provider),
                None,
                None,
                None,
            );
            let msgs = req["messages"].as_array().unwrap();
            assert_eq!(msgs[1]["role"], "tool");
            assert_eq!(msgs[1]["content"], "file body", "provider={provider}");
        }
    }

    #[test]
    fn a_multi_part_tool_result_without_media_stays_a_string() {
        let messages = vec![WireMessage {
            role: "tool".into(),
            content: String::new(),
            blocks: vec![
                ContentBlock::Text {
                    text: "alpha".into(),
                    meta: None,
                },
                ContentBlock::Text {
                    text: "beta".into(),
                    meta: None,
                },
            ],
            tool_calls: Vec::new(),
            tool_call_id: Some("call_1".into()),
        }];
        let req = build_request_for_provider(
            "m",
            &messages,
            &[],
            false,
            None,
            None,
            None,
            Some("openai"),
            None,
            None,
            None,
        );
        let msgs = req["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["content"], "alphabeta");
    }

    #[test]
    fn parse_response_extracts_tool_calls_finish_and_usage() {
        let v = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_9",
                        "type": "function",
                        "function": { "name": "Grep", "arguments": "{\"q\":\"foo\"}" }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": { "prompt_tokens": 12, "completion_tokens": 7, "total_tokens": 19 }
        });

        let parsed = parse_response(&v).unwrap();
        assert_eq!(parsed.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(parsed.tool_calls.len(), 1);
        assert_eq!(parsed.tool_calls[0].id, "call_9");
        assert_eq!(parsed.tool_calls[0].name, "Grep");
        assert_eq!(parsed.tool_calls[0].arguments, json!({ "q": "foo" }));
        assert_eq!(parsed.usage.input_tokens, 12);
        assert_eq!(parsed.usage.output_tokens, 7);
        assert_eq!(parsed.usage.total_tokens, 19);
    }

    #[test]
    fn parse_response_extracts_cached_prompt_tokens() {
        let v = json!({
            "choices": [{ "message": { "role": "assistant", "content": "hi" }, "finish_reason": "stop" }],
            "usage": {
                "prompt_tokens": 40,
                "completion_tokens": 5,
                "total_tokens": 45,
                "prompt_tokens_details": { "cached_tokens": 30 }
            }
        });
        let parsed = parse_response(&v).unwrap();
        assert_eq!(parsed.usage.input_cache_read, 30);
        assert_eq!(parsed.usage.input_tokens, 10);
        assert_eq!(parsed.usage.total_tokens, 45);
        assert_eq!(parsed.usage.input_cache_creation, 0);
    }

    #[test]
    fn parse_usage_missing_details_defaults_cache_to_zero() {
        let parsed = parse_response(&json!({
            "choices": [{ "message": { "role": "assistant", "content": "hi" }, "finish_reason": "stop" }],
            "usage": { "prompt_tokens": 3, "completion_tokens": 2 }
        }))
        .unwrap();
        assert_eq!(parsed.usage.input_cache_read, 0);
        assert_eq!(parsed.usage.input_cache_creation, 0);
    }

    #[test]
    fn parse_response_plain_text_has_no_tool_calls() {
        let v = json!({
            "choices": [{ "message": { "role": "assistant", "content": "hello" }, "finish_reason": "stop" }],
            "usage": { "prompt_tokens": 3, "completion_tokens": 2 }
        });
        let parsed = parse_response(&v).unwrap();
        assert!(parsed.tool_calls.is_empty());
        assert_eq!(parsed.finish_reason.as_deref(), Some("stop"));
        assert_eq!(parsed.usage.total_tokens, 5);
    }

    #[test]
    fn parse_response_errors_on_missing_choices() {
        assert!(parse_response(&json!({})).is_err());
    }

    #[test]
    fn build_request_streaming_sets_stream_options() {
        let req = build_request_with_options("m", &[WireMessage::text("user", "x")], &[], true);
        assert_eq!(req["stream"], true);
        assert_eq!(req["stream_options"]["include_usage"], true);
    }

    #[test]
    fn replayed_thinking_rides_the_declared_reasoning_field() {
        let assistant = WireMessage {
            role: "assistant".into(),
            content: String::new(),
            blocks: vec![
                ContentBlock::Think {
                    think: "I should add the numbers first.".into(),
                    encrypted: None,
                    details_index: None,
                    reasoning_key: None,
                    details_summary: None,
                },
                ContentBlock::Text {
                    text: "The answer is 7.".into(),
                    meta: None,
                },
            ],
            tool_calls: Vec::new(),
            tool_call_id: None,
        };
        let messages = vec![assistant];

        let req = build_request_full(
            "m",
            &messages,
            &[],
            true,
            None,
            None,
            Some("reasoning_content"),
        );
        let first = &req["messages"][0];
        assert_eq!(
            first["reasoning_content"], "I should add the numbers first.",
            "the think block rides the declared field"
        );
        let parts = first["content"].as_array().unwrap();
        assert_eq!(parts.len(), 1, "only the text block remains in content");
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "The answer is 7.");

        let req = build_request_full("m", &messages, &[], true, None, None, None);
        let parts = req["messages"][0]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert!(req["messages"][0].get("reasoning_content").is_none());
    }

    fn assistant_with_think_key(key: Option<&str>) -> WireMessage {
        WireMessage {
            role: "assistant".into(),
            content: String::new(),
            blocks: vec![ContentBlock::Think {
                think: "hm".into(),
                encrypted: None,
                details_index: None,
                reasoning_key: key.map(str::to_string),
                details_summary: None,
            }],
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    #[test]
    fn resolve_reasoning_key_orders_declared_detected_history() {
        let history = vec![assistant_with_think_key(Some("reasoning"))];
        assert_eq!(
            resolve_reasoning_key(&history, Some("reasoning_content"), Some("reasoning"))
                .as_deref(),
            Some("reasoning_content")
        );
        assert_eq!(
            resolve_reasoning_key(&history, None, Some("reasoning_content")).as_deref(),
            Some("reasoning_content")
        );
        assert_eq!(
            resolve_reasoning_key(&history, None, None).as_deref(),
            Some("reasoning")
        );
        assert_eq!(resolve_reasoning_key(&[], None, None), None);
    }

    #[test]
    fn history_reasoning_key_takes_the_newest_writeable_mark() {
        let history = vec![
            assistant_with_think_key(Some("reasoning_content")),
            assistant_with_think_key(Some("reasoning")),
        ];
        assert_eq!(
            resolve_reasoning_key(&history, None, None).as_deref(),
            Some("reasoning")
        );

        let history = vec![
            assistant_with_think_key(Some("reasoning")),
            assistant_with_think_key(Some("reasoning_details")),
        ];
        assert_eq!(
            resolve_reasoning_key(&history, None, None).as_deref(),
            Some("reasoning")
        );

        let history = vec![assistant_with_think_key(Some("reasoning_text"))];
        assert_eq!(resolve_reasoning_key(&history, None, None), None);
    }

    #[test]
    fn unmarked_thinking_replays_under_the_detected_dialect() {
        let messages = vec![assistant_with_think_key(None)];
        let req = build_request_for_provider(
            "m",
            &messages,
            &[],
            true,
            None,
            None,
            None,
            None,
            None,
            None,
            Some("reasoning"),
        );
        let first = &req["messages"][0];
        assert_eq!(first["reasoning"], "hm");
        assert!(first.get("reasoning_content").is_none());
    }

    #[test]
    fn stamped_think_parts_replay_as_the_reasoning_details_array() {
        let assistant = WireMessage {
            role: "assistant".into(),
            content: String::new(),
            blocks: vec![
                ContentBlock::Think {
                    think: "weighing options".into(),
                    encrypted: None,
                    details_index: Some(0),
                    reasoning_key: Some("reasoning_details".into()),
                    details_summary: None,
                },
                ContentBlock::Think {
                    think: String::new(),
                    encrypted: Some("sig-abc".into()),
                    details_index: Some(1),
                    reasoning_key: Some("reasoning_details".into()),
                    details_summary: None,
                },
                ContentBlock::Think {
                    think: "and the plain rest".into(),
                    encrypted: None,
                    details_index: None,
                    reasoning_key: None,
                    details_summary: None,
                },
                ContentBlock::Text {
                    text: "done".into(),
                    meta: None,
                },
            ],
            tool_calls: Vec::new(),
            tool_call_id: None,
        };

        let req = build_request_full("m", &[assistant], &[], true, None, None, None);
        let first = &req["messages"][0];
        assert_eq!(
            first["reasoning_details"],
            json!([
                { "type": "summary", "summary": "weighing options" },
                { "type": "encrypted", "encrypted": "sig-abc" },
            ]),
        );
        assert_eq!(first["reasoning_content"], "and the plain rest");
        let parts = first["content"].as_array().unwrap();
        assert_eq!(parts.len(), 1, "only the text block remains in content");
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "done");
    }

    #[test]
    fn keyed_think_parts_replay_into_their_own_field() {
        let assistant = WireMessage {
            role: "assistant".into(),
            content: String::new(),
            blocks: vec![
                ContentBlock::Think {
                    think: "first ".into(),
                    encrypted: None,
                    details_index: None,
                    reasoning_key: Some("reasoning".into()),
                    details_summary: None,
                },
                ContentBlock::Think {
                    think: "second".into(),
                    encrypted: None,
                    details_index: None,
                    reasoning_key: Some("reasoning".into()),
                    details_summary: None,
                },
            ],
            tool_calls: Vec::new(),
            tool_call_id: None,
        };

        let req = build_request_full("m", &[assistant], &[], true, None, None, None);
        let first = &req["messages"][0];
        assert_eq!(first["reasoning"], "first second");
        assert!(first.get("reasoning_content").is_none());
        assert!(first["content"].is_null(), "no non-think block remains");
    }

    #[test]
    fn a_reasoning_details_delta_produces_stamped_think_parts() {
        let mut acc = StreamAccumulator::new();
        acc.feed(&json!({
            "choices": [{ "delta": {
                "reasoning_details": [
                    { "type": "summary", "summary": "step one" },
                    { "type": "encrypted", "encrypted": "sig-1" },
                    { "type": "other" },
                ],
            } }],
        }));
        let response = acc.finish();
        assert_eq!(
            response.thinking,
            vec![
                ContentBlock::Think {
                    think: "step one".into(),
                    encrypted: None,
                    details_index: Some(0),
                    reasoning_key: Some("reasoning_details".into()),
                    details_summary: Some("step one".into()),
                },
                ContentBlock::Think {
                    think: String::new(),
                    encrypted: Some("sig-1".into()),
                    details_index: Some(1),
                    reasoning_key: Some("reasoning_details".into()),
                    details_summary: None,
                },
            ],
            "an element that is neither summary nor encrypted is dropped"
        );
    }

    #[test]
    fn a_seen_summary_keeps_its_array_entry_without_repeating_its_text() {
        let mut acc = StreamAccumulator::new();
        acc.feed(&json!({
            "choices": [{ "delta": { "reasoning_content": "the short form" } }],
        }));
        acc.feed(&json!({
            "choices": [{ "delta": {
                "reasoning_details": [{ "type": "summary", "summary": "the long form" }],
            } }],
        }));
        let response = acc.finish();
        assert_eq!(
            response.thinking,
            vec![
                ContentBlock::Think {
                    think: "the short form".into(),
                    encrypted: None,
                    details_index: None,
                    reasoning_key: None,
                    details_summary: None,
                },
                ContentBlock::Think {
                    think: String::new(),
                    encrypted: None,
                    details_index: Some(0),
                    reasoning_key: Some("reasoning_details".into()),
                    details_summary: Some("the long form".into()),
                },
            ],
        );
        assert_eq!(
            response.content, "",
            "the summary must not leak into the answer text either"
        );

        let req = build_request_full(
            "m",
            &[WireMessage {
                role: "assistant".into(),
                content: String::new(),
                blocks: response.thinking,
                tool_calls: Vec::new(),
                tool_call_id: None,
            }],
            &[],
            true,
            None,
            None,
            None,
        );
        let first = &req["messages"][0];
        assert_eq!(
            first["reasoning_details"],
            json!([{ "type": "summary", "summary": "the long form" }]),
        );
        assert_eq!(first["reasoning_content"], "the short form");
    }

    #[test]
    fn an_array_before_the_string_still_yields_one_visible_copy() {
        let mut acc = StreamAccumulator::new();
        acc.feed(&json!({
            "choices": [{ "delta": {
                "reasoning_details": [{ "type": "summary", "summary": "the long form" }],
            } }],
        }));
        acc.feed(&json!({
            "choices": [{ "delta": { "reasoning_content": "the short form" } }],
        }));
        acc.feed(&json!({
            "choices": [{ "delta": { "content": "the answer" } }],
        }));
        let response = acc.finish();

        let visible: String = response
            .thinking
            .iter()
            .map(|block| match block {
                ContentBlock::Think { think, .. } => think.as_str(),
                _ => "",
            })
            .collect();
        assert_eq!(
            visible, "the short form",
            "the summary must not be rendered when the string dialect showed up later"
        );
        assert_eq!(response.content, "the answer");
        assert!(
            response.thinking.iter().any(|block| matches!(
                block,
                ContentBlock::Think { details_summary: Some(summary), .. } if summary == "the long form"
            )),
            "the array entry still round-trips"
        );
    }

    #[test]
    fn a_reasoning_field_counts_as_the_string_dialect() {
        let mut acc = StreamAccumulator::new();
        acc.feed(&json!({
            "choices": [{ "delta": {
                "reasoning": "the streamed thought",
                "reasoning_details": [{ "type": "summary", "summary": "the summary" }],
            } }],
        }));
        let response = acc.finish();

        let visible: String = response
            .thinking
            .iter()
            .map(|b| match b {
                ContentBlock::Think { think, .. } => think.as_str(),
                _ => "",
            })
            .collect();
        assert_eq!(
            visible, "the streamed thought",
            "only the string dialect may be rendered"
        );
        assert!(
            response.thinking.iter().any(|b| matches!(
                b,
                ContentBlock::Think { details_summary: Some(s), .. } if s == "the summary"
            )),
            "the summary survives as replay payload"
        );
    }

    #[test]
    fn an_array_only_summary_stays_visible_and_keeps_its_entry() {
        let mut acc = StreamAccumulator::new();
        acc.feed(&json!({
            "choices": [{ "delta": {
                "reasoning_details": [{ "type": "summary", "summary": "only reasoning" }],
            } }],
        }));
        let response = acc.finish();
        assert_eq!(
            response.thinking,
            vec![ContentBlock::Think {
                think: "only reasoning".into(),
                encrypted: None,
                details_index: Some(0),
                reasoning_key: Some("reasoning_details".into()),
                details_summary: Some("only reasoning".into()),
            }],
            "with no string dialect the summary is the user's only reasoning"
        );

        let req = build_request_full(
            "m",
            &[WireMessage {
                role: "assistant".into(),
                content: String::new(),
                blocks: response.thinking,
                tool_calls: Vec::new(),
                tool_call_id: None,
            }],
            &[],
            true,
            None,
            None,
            None,
        );
        assert_eq!(
            req["messages"][0]["reasoning_details"],
            json!([{ "type": "summary", "summary": "only reasoning" }]),
        );
    }

    #[test]
    fn build_request_projects_image_blocks() {
        use kimi_core::types::ContentBlock;
        let msg = WireMessage::with_blocks(
            "user",
            vec![
                ContentBlock::Text {
                    text: "what is this?".into(),
                    meta: None,
                },
                ContentBlock::Image {
                    media_type: "image/png".into(),
                    data: "AAAA".into(),
                    name: None,
                },
                ContentBlock::ImageUrl {
                    url: "https://example.com/x.png".into(),
                    id: None,
                    name: None,
                },
            ],
        );
        let req = build_request("m", &[msg], &[]);
        let content = req["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 3);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "what is this?");
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(content[1]["image_url"]["url"], "data:image/png;base64,AAAA");
        assert_eq!(content[2]["image_url"]["url"], "https://example.com/x.png");
    }

    #[test]
    fn build_request_projects_audio_and_video_blocks() {
        use kimi_core::types::ContentBlock;
        let msg = WireMessage::with_blocks(
            "user",
            vec![
                ContentBlock::AudioUrl {
                    url: "https://example.com/a.mp3".into(),
                    id: None,
                    name: None,
                },
                ContentBlock::VideoUrl {
                    url: "https://example.com/v.mp4".into(),
                    id: Some("v1".into()),
                    name: None,
                },
            ],
        );
        let req = build_request("m", &[msg], &[]);
        let content = req["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "audio_url");
        assert_eq!(content[0]["audio_url"]["url"], "https://example.com/a.mp3");
        assert!(content[0]["audio_url"].get("id").is_none());
        assert_eq!(content[1]["type"], "video_url");
        assert_eq!(content[1]["video_url"]["url"], "https://example.com/v.mp4");
        assert_eq!(content[1]["video_url"]["id"], "v1");
    }

    #[test]
    fn parse_response_extracts_content_text() {
        let v = json!({
            "choices": [{ "message": { "role": "assistant", "content": "hello" }, "finish_reason": "stop" }],
        });
        let parsed = parse_response(&v).unwrap();
        assert_eq!(parsed.content, "hello");
    }

    #[test]
    fn stream_accumulator_collects_text_and_tool_calls() {
        let mut acc = StreamAccumulator::new();

        let d1 = acc.feed(&json!({ "choices": [{ "delta": { "content": "Hel" } }] }));
        assert_eq!(d1, Some(StreamDelta::Text("Hel".into())));
        let d2 = acc.feed(&json!({ "choices": [{ "delta": { "content": "lo" } }] }));
        assert_eq!(d2, Some(StreamDelta::Text("lo".into())));

        acc.feed(&json!({ "choices": [{ "delta": { "tool_calls": [
            { "index": 0, "id": "call_1", "function": { "name": "Grep", "arguments": "{\"q\":" } }
        ] } }] }));
        acc.feed(&json!({ "choices": [{ "delta": { "tool_calls": [
            { "index": 0, "function": { "arguments": "\"foo\"}" } }
        ] } }] }));

        acc.feed(&json!({ "choices": [{ "delta": {}, "finish_reason": "tool_calls" }] }));
        acc.feed(
            &json!({ "choices": [], "usage": { "prompt_tokens": 7, "completion_tokens": 3 } }),
        );

        let resp = acc.finish();
        assert_eq!(resp.content, "Hello");
        assert_eq!(resp.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].id, "call_1");
        assert_eq!(resp.tool_calls[0].name, "Grep");
        assert_eq!(resp.tool_calls[0].arguments, json!({ "q": "foo" }));
        assert_eq!(resp.usage.input_tokens, 7);
        assert_eq!(resp.usage.output_tokens, 3);
        assert_eq!(resp.usage.total_tokens, 10);
    }

    #[test]
    fn stream_accumulator_parallel_tool_calls_by_index() {
        let mut acc = StreamAccumulator::new();
        acc.feed(&json!({ "choices": [{ "delta": { "tool_calls": [
            { "index": 0, "id": "a", "function": { "name": "Read", "arguments": "{}" } },
            { "index": 1, "id": "b", "function": { "name": "Glob", "arguments": "{}" } }
        ] } }] }));
        let resp = acc.finish();
        assert_eq!(resp.tool_calls.len(), 2);
        assert_eq!(resp.tool_calls[0].name, "Read");
        assert_eq!(resp.tool_calls[1].name, "Glob");
    }

    #[test]
    fn stream_accumulator_ignores_implausible_tool_call_index() {
        let mut acc = StreamAccumulator::new();
        acc.feed(&json!({ "choices": [{ "delta": { "tool_calls": [
            { "index": 0, "id": "a", "function": { "name": "Read", "arguments": "{}" } },
            { "index": 50_000_000, "id": "b", "function": { "name": "Boom", "arguments": "{}" } }
        ] } }] }));
        assert!(acc.tool_calls.len() <= MAX_STREAM_TOOL_CALLS);
        let resp = acc.finish();
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].name, "Read");
    }

    #[test]
    fn finish_after_truncated_stream_keeps_partial_content() {
        let mut acc = StreamAccumulator::default();
        acc.feed(&json!({ "choices": [{ "delta": { "content": "hel" } }] }));
        acc.feed(&json!({ "choices": [{ "delta": { "content": "lo" } }] }));
        let resp = acc.finish();
        assert_eq!(resp.content, "hello");
        assert!(resp.finish_reason.is_none());
        assert_eq!(resp.usage.output_tokens, 0);
    }

    #[test]
    fn truncated_tool_call_arguments_keep_the_call_and_its_reason() {
        let mut acc = StreamAccumulator::default();
        acc.feed(&json!({
            "choices": [{
                "delta": { "tool_calls": [{ "index": 0, "id": "call_1", "function": { "name": "Bash", "arguments": "{\"command\": \"ech" } }] }
            }]
        }));
        acc.feed(&json!({
            "choices": [{
                "delta": { "tool_calls": [{ "index": 0, "function": { "arguments": "" } }] }
            }]
        }));
        let resp = acc.finish();
        assert_eq!(
            resp.tool_calls.len(),
            1,
            "the call survives its own parse failure"
        );
        assert_eq!(resp.tool_calls[0].name, "Bash");
        assert_eq!(resp.tool_calls[0].arguments, json!({}));
        assert!(
            resp.tool_calls[0].args_parse_error.is_some(),
            "the reason must travel with the call so the loop can report it"
        );
        assert!(resp.content.is_empty());
    }

    #[test]
    fn complete_tool_call_arguments_are_kept() {
        let mut acc = StreamAccumulator::default();
        acc.feed(&json!({
            "choices": [{
                "delta": { "tool_calls": [{ "index": 0, "id": "call_1", "function": { "name": "Read", "arguments": "{\"path\": \"a.txt\"}" } }] }
            }]
        }));
        let resp = acc.finish();
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].name, "Read");
        assert_eq!(resp.tool_calls[0].arguments["path"], "a.txt");
    }

    #[test]
    fn test_stream_accumulator_reasoning_content() {
        let mut acc = StreamAccumulator::default();
        let delta1 = acc.feed(&json!({
            "choices": [{
                "delta": { "reasoning_content": "Thinking about " }
            }]
        }));
        assert_eq!(delta1, Some(StreamDelta::Think("Thinking about ".into())));

        let delta2 = acc.feed(&json!({
            "choices": [{
                "delta": { "content": "the solution." }
            }]
        }));
        assert_eq!(delta2, Some(StreamDelta::Text("the solution.".into())));

        let resp = acc.finish();
        assert_eq!(resp.content, "the solution.");
        assert_eq!(resp.thinking.len(), 1);
        assert_eq!(
            resp.thinking[0],
            ContentBlock::Think {
                think: "Thinking about ".into(),
                encrypted: None,
                details_index: None,
                reasoning_key: None,
                details_summary: None,
            }
        );
    }

    #[test]
    fn declared_reasoning_key_is_read_before_the_probe_list() {
        let mut declared = StreamAccumulator::new().with_reasoning_key(Some("thinking_text"));
        let delta = declared.feed(&json!({
            "choices": [{ "delta": { "thinking_text": "step by step" } }]
        }));
        assert_eq!(delta, Some(StreamDelta::Think("step by step".into())));

        let mut probed = StreamAccumulator::new();
        let delta = probed.feed(&json!({
            "choices": [{ "delta": { "reasoning_content": "probed" } }]
        }));
        assert_eq!(delta, Some(StreamDelta::Think("probed".into())));
    }

    #[test]
    fn the_first_writeable_reasoning_dialect_is_detected() {
        let mut acc = StreamAccumulator::new();
        acc.feed(&json!({ "choices": [{ "delta": { "reasoning": "" } }] }));
        assert_eq!(acc.detected_reasoning_key(), None);

        let delta = acc.feed(&json!({ "choices": [{ "delta": { "reasoning": "step" } }] }));
        assert_eq!(delta, Some(StreamDelta::Think("step".into())));
        assert_eq!(acc.detected_reasoning_key(), Some("reasoning"));

        acc.feed(&json!({ "choices": [{ "delta": { "reasoning_content": "more" } }] }));
        assert_eq!(acc.detected_reasoning_key(), Some("reasoning"));
    }

    #[test]
    fn probe_only_spellings_are_not_a_replay_dialect() {
        let mut acc = StreamAccumulator::new();
        let delta = acc.feed(&json!({ "choices": [{ "delta": { "reasoning_text": "step" } }] }));
        assert_eq!(delta, Some(StreamDelta::Think("step".into())));
        assert_eq!(acc.detected_reasoning_key(), None);
    }

    #[test]
    fn a_declared_reasoning_key_suppresses_detection() {
        let mut acc = StreamAccumulator::new().with_reasoning_key(Some("thinking_text"));
        acc.feed(
            &json!({ "choices": [{ "delta": { "thinking_text": "step", "reasoning": "x" } }] }),
        );
        assert_eq!(acc.detected_reasoning_key(), None);
    }

    #[test]
    fn parse_response_extracts_reasoning_content() {
        let v = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "42",
                    "reasoning_content": "Let me calculate 6 * 7"
                },
                "finish_reason": "stop"
            }],
        });
        let parsed = parse_response(&v).unwrap();
        assert_eq!(parsed.content, "42");
        assert_eq!(parsed.thinking.len(), 1);
        assert_eq!(
            parsed.thinking[0],
            ContentBlock::Think {
                think: "Let me calculate 6 * 7".into(),
                encrypted: None,
                details_index: None,
                reasoning_key: None,
                details_summary: None,
            }
        );
    }

    #[test]
    fn test_build_request_reasoning_effort() {
        let msgs = vec![WireMessage {
            role: "user".into(),
            content: "hello".into(),
            blocks: vec![],
            tool_calls: vec![],
            tool_call_id: None,
        }];
        let req_high = build_request_full("gpt-4o", &msgs, &[], true, Some("high"), None, None);
        assert_eq!(req_high["reasoning_effort"], "high");

        let req_off = build_request_full("gpt-4o", &msgs, &[], true, Some("off"), None, None);
        assert!(req_off.get("reasoning_effort").is_none());

        let req_on = build_request_full("gpt-4o", &msgs, &[], true, Some("on"), None, None);
        assert!(req_on.get("reasoning_effort").is_none());

        let req_none = build_request_full("gpt-4o", &msgs, &[], true, Some("none"), None, None);
        assert_eq!(req_none["reasoning_effort"], "none");

        let req_absent = build_request_full("gpt-4o", &msgs, &[], true, None, None, None);
        assert!(req_absent.get("reasoning_effort").is_none());
    }

    #[test]
    fn test_build_request_thinking_keep() {
        let msgs = vec![WireMessage::text("user", "hello")];
        let req_keep = build_request_full("kimi-k2", &msgs, &[], true, None, Some("all"), None);
        assert_eq!(req_keep["thinking"]["keep"], "all");

        let req_no_keep = build_request_full("kimi-k2", &msgs, &[], true, None, None, None);
        assert!(req_no_keep.get("thinking").is_none());
    }

    #[test]
    fn test_build_request_kimi_thinking_object() {
        let msgs = vec![WireMessage::text("user", "hello")];
        let build = |effort: Option<&str>, keep: Option<&str>| {
            build_request_for_provider(
                "kimi-k2",
                &msgs,
                &[],
                true,
                effort,
                keep,
                None,
                Some("kimi"),
                None,
                None,
                None,
            )
        };

        let off = build(Some("off"), None);
        assert_eq!(off["thinking"], json!({ "type": "disabled" }));
        assert!(off.get("reasoning_effort").is_none());

        let on = build(Some("on"), Some("all"));
        assert_eq!(on["thinking"], json!({ "type": "enabled", "keep": "all" }));
        assert!(on.get("reasoning_effort").is_none());

        let high = build(Some("high"), Some("all"));
        assert_eq!(
            high["thinking"],
            json!({ "type": "enabled", "effort": "high", "keep": "all" })
        );
        assert!(high.get("reasoning_effort").is_none());

        let absent = build(None, None);
        assert!(absent.get("thinking").is_none());
        assert!(absent.get("reasoning_effort").is_none());

        let generic = build_request_for_provider(
            "gpt-4o",
            &msgs,
            &[],
            true,
            Some("high"),
            Some("all"),
            None,
            Some("openai"),
            None,
            None,
            None,
        );
        assert_eq!(generic["reasoning_effort"], "high");
        assert!(generic.get("thinking").is_none());
    }

    #[test]
    fn test_build_request_cache_key_and_completion_budget() {
        let msgs = vec![WireMessage::text("user", "hello")];

        let generic = build_request_for_provider(
            "gpt-4o",
            &msgs,
            &[],
            true,
            None,
            None,
            None,
            Some("openai"),
            Some("sess-1"),
            Some(50_000),
            None,
        );
        assert_eq!(generic["prompt_cache_key"], "sess-1");
        assert_eq!(generic["max_tokens"], 50_000);
        assert!(generic.get("max_completion_tokens").is_none());

        for model in ["gpt-5", "gpt-5-mini", "o3", "o4-mini"] {
            let req = build_request_for_provider(
                model,
                &msgs,
                &[],
                true,
                None,
                None,
                None,
                Some("openai"),
                None,
                Some(4096),
                None,
            );
            assert_eq!(
                req["max_completion_tokens"], 4096,
                "{model} must use the completion spelling"
            );
            assert!(req.get("max_tokens").is_none(), "{model}");
        }

        let kimi = build_request_for_provider(
            "kimi-k3",
            &msgs,
            &[],
            true,
            None,
            None,
            None,
            Some("kimi"),
            Some("sess-2"),
            Some(500_000),
            None,
        );
        assert_eq!(kimi["max_completion_tokens"], 128 * 1024);
        assert_eq!(kimi["prompt_cache_key"], "sess-2");
        assert!(kimi.get("max_tokens").is_none());
    }

    #[test]
    fn test_parse_usage_cache_variants() {
        let ds_usage = json!({
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "prompt_cache_hit_tokens": 80,
            "prompt_cache_miss_tokens": 20
        });
        let ds_res = parse_usage(Some(&ds_usage));
        assert_eq!(ds_res.input_cache_read, 80);
        assert_eq!(ds_res.input_tokens, 20);
        assert_eq!(ds_res.output_tokens, 20);
        assert_eq!(ds_res.total_tokens, 120);

        let moonshot_usage = json!({
            "prompt_tokens": 1000,
            "completion_tokens": 50,
            "cached_tokens": 900
        });
        let ms_res = parse_usage(Some(&moonshot_usage));
        assert_eq!(ms_res.input_cache_read, 900);
        assert_eq!(ms_res.input_tokens, 100);
        assert_eq!(ms_res.output_tokens, 50);

        let openai_usage = json!({
            "prompt_tokens": 500,
            "completion_tokens": 30,
            "prompt_tokens_details": {
                "cached_tokens": 350
            }
        });
        let oai_res = parse_usage(Some(&openai_usage));
        assert_eq!(oai_res.input_cache_read, 350);
        assert_eq!(oai_res.input_tokens, 150);
        assert_eq!(oai_res.output_tokens, 30);
    }

    fn read_tool() -> ToolInfo {
        ToolInfo {
            name: "Read".into(),
            description: "read a file".into(),
            input_schema: json!({ "type": "object" }),
        }
    }

    #[test]
    fn an_empty_system_turn_is_dropped_only_when_tools_are_offered() {
        let messages = vec![
            WireMessage::text("system", ""),
            WireMessage::text("user", "hi"),
        ];

        let with_tools =
            build_request_full("gpt-4o", &messages, &[read_tool()], true, None, None, None);
        let roles: Vec<&str> = with_tools["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|message| message["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, vec!["user"]);

        let without_tools = build_request_full("gpt-4o", &messages, &[], true, None, None, None);
        assert_eq!(
            without_tools["messages"].as_array().unwrap().len(),
            2,
            "v2 keeps it when there are no tools to ferry"
        );
    }

    #[test]
    fn request_options_carry_seed_format_and_sampling() {
        let messages = vec![WireMessage::text("user", "hi")];
        let options = RequestOptions {
            response_format: Some(ResponseFormat::JsonSchema {
                name: "answer".into(),
                schema: json!({ "type": "object" }),
                strict: Some(true),
                description: None,
            }),
            seed: Some(7),
            temperature: Some(0.2),
            top_p: Some(0.9),
            ..RequestOptions::default()
        };

        let req = build_request_for_provider_with_options(
            "gpt-4o",
            &messages,
            &[],
            true,
            None,
            None,
            None,
            Some("openai"),
            None,
            None,
            None,
            &options,
        );
        assert_eq!(req["seed"], 7);
        assert_eq!(req["response_format"]["type"], "json_schema");
        assert_eq!(req["response_format"]["json_schema"]["name"], "answer");
        assert_eq!(req["response_format"]["json_schema"]["strict"], true);
        assert!(
            req["response_format"]["json_schema"]
                .get("description")
                .is_none(),
            "an unset description is omitted, not sent as null"
        );
        assert_eq!(req["temperature"], 0.2);
        assert_eq!(req["top_p"], 0.9);

        let json_object = RequestOptions {
            response_format: Some(ResponseFormat::JsonObject),
            ..RequestOptions::default()
        };
        let req = build_request_for_provider_with_options(
            "gpt-4o",
            &messages,
            &[],
            true,
            None,
            None,
            None,
            Some("openai"),
            None,
            None,
            None,
            &json_object,
        );
        assert_eq!(req["response_format"], json!({ "type": "json_object" }));
    }

    #[test]
    fn a_derived_seed_is_stable_for_the_same_turn() {
        let messages = vec![WireMessage::text("user", "hi")];
        let options = RequestOptions {
            derive_seed: true,
            ..RequestOptions::default()
        };
        let build = || {
            build_request_for_provider_with_options(
                "deepseek-chat",
                &messages,
                &[],
                true,
                None,
                None,
                None,
                Some("openai"),
                None,
                None,
                None,
                &options,
            )
        };
        let expected =
            crate::llm::request_options::derive_determinism_seed("deepseek-chat", &messages);
        assert_eq!(build()["seed"], expected);
        assert_eq!(build()["seed"], build()["seed"]);
    }

    #[test]
    fn extra_params_override_the_writer_and_families_strip_sampling() {
        let messages = vec![WireMessage::text("user", "hi")];
        let mut options = RequestOptions {
            temperature: Some(0.2),
            ..RequestOptions::default()
        };
        options
            .extra_params
            .openai
            .insert("temperature".into(), json!(0.7));
        options
            .extra_params
            .openai
            .insert("service_tier".into(), json!("flex"));

        let req = build_request_for_provider_with_options(
            "gpt-4o",
            &messages,
            &[],
            true,
            None,
            None,
            None,
            Some("openai"),
            None,
            None,
            None,
            &options,
        );
        assert_eq!(req["temperature"], 0.7, "extraParams wins over the writer");
        assert_eq!(req["service_tier"], "flex");

        let mut options = RequestOptions {
            temperature: Some(0.2),
            top_p: Some(0.9),
            response_format: Some(ResponseFormat::JsonObject),
            ..RequestOptions::default()
        };
        options
            .extra_params
            .openai
            .insert("presence_penalty".into(), json!(0.5));
        let req = build_request_for_provider_with_options(
            "deepseek-chat",
            &messages,
            &[read_tool()],
            true,
            None,
            None,
            None,
            Some("openai"),
            None,
            None,
            None,
            &options,
        );
        for stripped in ["temperature", "presence_penalty"] {
            assert!(
                req.get(stripped).is_none(),
                "v2 `ignoredWhileThinking` strips {stripped}"
            );
        }
        assert_eq!(req["top_p"], 0.9, "top_p is not in the ignored set");
        assert!(
            req.get("response_format").is_none(),
            "a DeepSeek tool constraint preempts the response format"
        );
    }

    #[test]
    fn stamped_reasoning_replays_identically_with_and_without_tools() {
        let assistant = WireMessage {
            role: "assistant".into(),
            content: String::new(),
            blocks: vec![ContentBlock::Think {
                think: "weighing options".into(),
                encrypted: None,
                details_index: Some(0),
                reasoning_key: Some("reasoning_details".into()),
                details_summary: None,
            }],
            tool_calls: Vec::new(),
            tool_call_id: None,
        };
        let with_tools = build_request_full(
            "gpt-4o",
            std::slice::from_ref(&assistant),
            &[read_tool()],
            true,
            None,
            None,
            None,
        );
        let without_tools = build_request_full("gpt-4o", &[assistant], &[], true, None, None, None);
        assert_eq!(with_tools["messages"], without_tools["messages"]);
        assert_eq!(
            with_tools["messages"][0]["reasoning_details"],
            json!([{ "type": "summary", "summary": "weighing options" }])
        );
    }

    #[test]
    fn usage_reads_the_third_cache_spelling_and_guards_contradictions() {
        let nested = json!({
            "prompt_tokens": 500,
            "completion_tokens": 30,
            "input_tokens_details": { "cached_tokens": 350 }
        });
        let res = parse_usage(Some(&nested));
        assert_eq!(res.input_cache_read, 350);
        assert_eq!(res.input_tokens, 150);

        let explicit_plus_nested = json!({
            "prompt_tokens": 500,
            "completion_tokens": 30,
            "cached_tokens": 100,
            "input_tokens_details": { "cached_tokens": 350 }
        });
        assert_eq!(
            parse_usage(Some(&explicit_plus_nested)).input_cache_read,
            100
        );

        let contradictory = json!({
            "prompt_tokens": 100,
            "completion_tokens": 10,
            "cached_tokens": 70,
            "prompt_cache_hit_tokens": 90,
            "prompt_cache_miss_tokens": 40
        });
        let res = parse_usage(Some(&contradictory));
        assert_eq!(res.input_cache_read, 70);
        assert_eq!(
            res.input_tokens, 30,
            "the contradiction collapses to prompt - cached"
        );
    }
}
