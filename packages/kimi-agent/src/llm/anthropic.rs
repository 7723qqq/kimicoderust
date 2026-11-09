use serde_json::{Value, json};

use crate::llm::wire::{StreamDelta, WireMessage};
use crate::rpc::types::TokenUsage;
use kimi_core::types::{ContentBlock, LLMChatResponse, ToolCall, ToolInfo};

const CACHEABLE_TYPES: [&str; 8] = [
    "text",
    "image",
    "document",
    "search_result",
    "tool_use",
    "tool_result",
    "server_tool_use",
    "web_search_tool_result",
];

fn is_cacheable_block(block: &Value) -> bool {
    block
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|ty| CACHEABLE_TYPES.contains(&ty))
}

pub fn build_request(
    model: &str,
    max_tokens: u32,
    messages: &[WireMessage],
    tools: &[ToolInfo],
) -> Value {
    build_request_full(model, max_tokens, messages, tools, false, None, None)
}

pub fn build_request_with_options(
    model: &str,
    max_tokens: u32,
    messages: &[WireMessage],
    tools: &[ToolInfo],
    stream: bool,
) -> Value {
    build_request_full(model, max_tokens, messages, tools, stream, None, None)
}

pub fn build_request_full(
    model: &str,
    max_tokens: u32,
    messages: &[WireMessage],
    tools: &[ToolInfo],
    stream: bool,
    thinking_budget: Option<u32>,
    thinking_keep: Option<&str>,
) -> Value {
    build_request_for_provider(
        model,
        max_tokens,
        messages,
        tools,
        stream,
        thinking_budget,
        thinking_keep,
        None,
        None,
        None,
        None,
        false,
        false,
        &crate::llm::request_options::RequestOptions::default(),
    )
    .unwrap_or_else(|error| panic!("invalid anthropic request: {error}"))
}

fn budget_for_effort(effort: &str) -> Option<u32> {
    match effort {
        "low" => Some(1024),
        "medium" => Some(4096),
        "on" | "high" => Some(32_000),
        other => other.parse::<u32>().ok().filter(|value| *value > 0),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn build_request_for_provider(
    model: &str,
    max_tokens: u32,
    messages: &[WireMessage],
    tools: &[ToolInfo],
    stream: bool,
    thinking_budget: Option<u32>,
    thinking_keep: Option<&str>,
    reasoning_effort: Option<&str>,
    provider_type: Option<&str>,
    cache_key: Option<&str>,
    completion_cap: Option<u32>,
    adaptive_thinking: bool,
    use_beta_api: bool,
    options: &crate::llm::request_options::RequestOptions,
) -> Result<Value, String> {
    let mut system = String::new();
    let mut msgs: Vec<Value> = Vec::new();

    for m in messages {
        match m.role.as_str() {
            "system" => {
                if !system.is_empty() {
                    system.push_str("\n\n");
                }
                system.push_str(&m.content);
            }
            "assistant" => {
                let mut blocks: Vec<Value> = Vec::new();
                for b in &m.blocks {
                    if matches!(b, ContentBlock::Think { .. }) {
                        blocks.push(project_block(b));
                    }
                }
                if !m.content.is_empty() {
                    blocks.push(json!({ "type": "text", "text": m.content }));
                }
                for tc in &m.tool_calls {
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": tc.id,
                        "name": tc.name,
                        "input": tc.arguments,
                    }));
                }
                msgs.push(json!({ "role": "assistant", "content": blocks }));
            }
            "tool" => {
                let tool_use_id = m.tool_call_id.clone().unwrap_or_default();
                let content = if m.blocks.is_empty() {
                    json!(m.content)
                } else {
                    json!(tool_result_content(&m.blocks))
                };
                let block = json!({
                    "type": "tool_result",
                    "tool_use_id": tool_use_id,
                    "content": content,
                });
                if let Some(last_msg) = msgs.last_mut()
                    && last_msg.get("role").and_then(|r| r.as_str()) == Some("user")
                    && let Some(content_arr) =
                        last_msg.get_mut("content").and_then(|c| c.as_array_mut())
                {
                    content_arr.push(block);
                } else {
                    msgs.push(json!({ "role": "user", "content": [block] }));
                }
            }
            _ => {
                let content: Vec<Value> = if m.blocks.is_empty() {
                    vec![json!({ "type": "text", "text": m.content })]
                } else {
                    m.blocks.iter().map(project_block).collect()
                };
                if let Some(last_msg) = msgs.last_mut()
                    && last_msg.get("role").and_then(|r| r.as_str()) == Some("user")
                    && let Some(content_arr) =
                        last_msg.get_mut("content").and_then(|c| c.as_array_mut())
                {
                    content_arr.extend(content);
                } else {
                    msgs.push(json!({ "role": "user", "content": content }));
                }
            }
        }
    }

    if let Some(last_msg) = msgs.last_mut()
        && let Some(content_arr) = last_msg.get_mut("content").and_then(|c| c.as_array_mut())
        && let Some(last_block) = content_arr.last_mut()
        && is_cacheable_block(last_block)
    {
        last_block["cache_control"] = json!({ "type": "ephemeral" });
    }

    if msgs.len() >= 4 {
        let stable_idx = msgs.len() - 3;
        if let Some(stable_msg) = msgs.get_mut(stable_idx)
            && let Some(content_arr) = stable_msg.get_mut("content").and_then(|c| c.as_array_mut())
            && let Some(stable_block) = content_arr.last_mut()
            && is_cacheable_block(stable_block)
            && stable_block.get("cache_control").is_none()
        {
            stable_block["cache_control"] = json!({ "type": "ephemeral" });
        }
    }

    let is_kimi = provider_type.is_some_and(|value| value.eq_ignore_ascii_case("kimi"));
    let effort_thinking_off =
        reasoning_effort.is_some_and(|effort| matches!(effort, "" | "off" | "none"));
    let effort_budget = reasoning_effort
        .filter(|effort| !matches!(*effort, "" | "off" | "none"))
        .and_then(budget_for_effort);
    let thinking_budget = thinking_budget
        .filter(|budget| *budget > 0)
        .or(effort_budget);
    let effective_max_tokens = if is_kimi {
        completion_cap.unwrap_or(max_tokens)
    } else {
        let capped = completion_cap
            .map(|cap| max_tokens.min(cap))
            .unwrap_or(max_tokens);
        claude_max_tokens_ceiling(model).map_or(capped, |ceiling| capped.min(ceiling))
    };
    let thinking_budget = if !is_kimi && !adaptive_thinking {
        match thinking_budget {
            Some(budget) if budget > 0 && budget >= effective_max_tokens => {
                if effective_max_tokens <= 1024 {
                    return Err(format!(
                        "Thinking budget {budget} exceeds the max_tokens cap {effective_max_tokens}; \
                         max_tokens must be greater than 1024 while thinking is enabled."
                    ));
                }
                Some(effective_max_tokens - 1)
            }
            budget => budget,
        }
    } else {
        thinking_budget
    };

    let mut req = json!({
        "model": model,
        "max_tokens": effective_max_tokens,
        "messages": msgs,
    });
    if stream {
        req["stream"] = json!(true);
    }
    if let Some(cache_key) = cache_key.filter(|key| !key.is_empty()) {
        req["metadata"] = json!({ "user_id": cache_key });
    }
    if is_kimi {
        if let Some(effort) = reasoning_effort.filter(|value| !value.is_empty()) {
            if effort == "off" || effort == "none" {
                req["thinking"] = json!({ "type": "disabled" });
            } else {
                req["thinking"] = json!({ "type": "enabled" });
                if effort != "on" {
                    req["output_config"] = json!({ "effort": effort });
                }
            }
        }
    } else if adaptive_thinking {
        match reasoning_effort.filter(|value| !value.is_empty()) {
            Some(effort) if effort == "off" || effort == "none" => {
                req["thinking"] = json!({ "type": "disabled" });
            }
            Some(effort) => {
                req["thinking"] = json!({ "type": "adaptive", "display": "summarized" });
                if effort != "on" {
                    req["output_config"] = json!({ "effort": effort });
                }
            }
            None => {
                req["thinking"] = json!({ "type": "adaptive", "display": "summarized" });
            }
        }
    } else if let Some(budget) = thinking_budget {
        req["thinking"] = json!({
            "type": "enabled",
            "budget_tokens": budget,
        });
    } else if !effort_thinking_off && reasoning_effort.is_some() {
        req["thinking"] = json!({ "type": "enabled" });
    }
    if let Some(format) = options.response_format.as_ref() {
        let output_config = format.to_anthropic_output_config()?;
        let mut merged = req
            .get("output_config")
            .cloned()
            .unwrap_or_else(|| json!({}));
        if let (Some(target), Some(source)) = (merged.as_object_mut(), output_config.as_object()) {
            for (key, value) in source {
                target.insert(key.clone(), value.clone());
            }
        }
        req["output_config"] = merged;
    }
    if let Value::Object(map) = &mut req {
        if let Some(temperature) = options.temperature {
            map.entry("temperature".to_string())
                .or_insert(json!(temperature));
        }
        if let Some(top_p) = options.top_p {
            map.entry("top_p".to_string()).or_insert(json!(top_p));
        }
        for (key, value) in &options.extra_params.anthropic {
            map.insert(key.clone(), value.clone());
        }
    }
    if let Some(keep) = thinking_keep
        && !keep.is_empty()
    {
        req["context_management"] = json!({
            "edits": [{ "type": "clear_thinking_20251015", "keep": keep }],
        });
    }

    let betas = anthropic_betas(
        provider_type,
        adaptive_thinking,
        reasoning_effort.is_some() || thinking_budget.is_some(),
        thinking_keep,
    );
    if use_beta_api && !betas.is_empty() {
        req["betas"] = json!(betas);
    }

    if !system.is_empty() {
        req["system"] = json!([{
            "type": "text",
            "text": system,
            "cache_control": { "type": "ephemeral" },
        }]);
    }
    if !tools.is_empty() {
        let mut tool_defs: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema,
                })
            })
            .collect();
        if let Some(last) = tool_defs.last_mut() {
            last["cache_control"] = json!({ "type": "ephemeral" });
        }
        req["tools"] = json!(tool_defs);
    }

    Ok(req)
}

pub const INTERLEAVED_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";
pub const CONTEXT_MANAGEMENT_BETA: &str = "context-management-2025-06-27";

pub fn anthropic_betas(
    provider_type: Option<&str>,
    adaptive_thinking: bool,
    thinking_configured: bool,
    thinking_keep: Option<&str>,
) -> Vec<&'static str> {
    let is_kimi = provider_type.is_some_and(|value| value.eq_ignore_ascii_case("kimi"));
    let mut betas = Vec::new();
    if thinking_configured && is_kimi {
        betas.push(CONTEXT_MANAGEMENT_BETA);
    } else if !(thinking_configured && adaptive_thinking) {
        betas.push(INTERLEAVED_THINKING_BETA);
    }
    if thinking_keep.is_some_and(|keep| !keep.is_empty())
        && !betas.contains(&CONTEXT_MANAGEMENT_BETA)
    {
        betas.push(CONTEXT_MANAGEMENT_BETA);
    }
    betas
}

const CLAUDE_CEILINGS: &[(&str, u32)] = &[
    ("fable-5", 128_000),
    ("mythos-5", 128_000),
    ("opus-4-8", 128_000),
    ("opus-4-7", 128_000),
    ("opus-4-6", 128_000),
    ("opus-4-5", 64_000),
    ("opus-4-1", 32_000),
    ("opus-4-0", 32_000),
    ("opus-4", 32_000),
    ("sonnet-5", 128_000),
    ("sonnet-4-6", 128_000),
    ("sonnet-4-5", 64_000),
    ("sonnet-4-0", 64_000),
    ("sonnet-4", 64_000),
    ("haiku-4-5", 64_000),
    ("haiku-4", 64_000),
    ("opus-3-5", 8192),
    ("sonnet-3-5", 8192),
    ("sonnet-3-7", 8192),
    ("haiku-3-5", 8192),
    ("opus-3", 4096),
    ("sonnet-3", 4096),
    ("haiku-3", 4096),
];

fn claude_max_tokens_ceiling(model: &str) -> Option<u32> {
    let normalized = model.to_ascii_lowercase();
    if !normalized.contains("claude") {
        return None;
    }
    let (family, major, minor) = parse_anthropic_model_version(&normalized)?;
    lookup_claude_ceiling(&family, major, minor)
}

fn lookup_claude_ceiling(family: &str, major: u32, minor: Option<u32>) -> Option<u32> {
    if let Some(minor) = minor {
        for candidate in (0..=minor).rev() {
            let key = format!("{family}-{major}-{candidate}");
            if let Some((_, ceiling)) = CLAUDE_CEILINGS.iter().find(|(name, _)| *name == key) {
                return Some(*ceiling);
            }
        }
    }
    let key = format!("{family}-{major}");
    CLAUDE_CEILINGS
        .iter()
        .find(|(name, _)| *name == key)
        .map(|(_, ceiling)| *ceiling)
}

const ANTHROPIC_FAMILY_TOKENS: [&str; 5] = ["opus", "sonnet", "haiku", "fable", "mythos"];

fn read_version_number(text: &str) -> Option<(u32, &str)> {
    let digits = text.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 || digits > 2 {
        return None;
    }
    let (number, rest) = text.split_at(digits);
    Some((number.parse().ok()?, rest))
}

fn parse_family_first(model: &str) -> Option<(String, u32, Option<u32>)> {
    let mut best: Option<(usize, &'static str)> = None;
    for family in ANTHROPIC_FAMILY_TOKENS {
        let mut from = 0;
        while let Some(offset) = model[from..].find(family) {
            let index = from + offset;
            let after = &model[index + family.len()..];
            if after.starts_with(['-', '.', '_']) {
                if best.is_none_or(|(best_index, _)| index < best_index) {
                    best = Some((index, family));
                }
                break;
            }
            from = index + family.len();
        }
    }
    let (index, family) = best?;
    let rest = &model[index + family.len() + 1..];
    let (major, rest) = read_version_number(rest)?;
    let minor = rest
        .strip_prefix(['-', '.', '_'])
        .and_then(read_version_number)
        .map(|(minor, _)| minor);
    Some((family.to_string(), major, minor))
}

fn parse_anthropic_model_version(model: &str) -> Option<(String, u32, Option<u32>)> {
    use once_cell::sync::Lazy;
    use regex::Regex;

    static VERSION_FIRST: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"(\d{1,2})[-._](\d{1,2})[-._](opus|sonnet|haiku)")
            .expect("valid anthropic version regex")
    });
    static BARE_FAMILY: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"(\d{1,2})[-._](opus|sonnet|haiku)").expect("valid anthropic bare regex")
    });

    if let Some(parsed) = parse_family_first(model) {
        return Some(parsed);
    }
    if let Some(captures) = VERSION_FIRST.captures(model) {
        return Some((
            captures.get(3)?.as_str().to_string(),
            captures.get(1)?.as_str().parse().ok()?,
            captures.get(2)?.as_str().parse().ok(),
        ));
    }
    let captures = BARE_FAMILY.captures(model)?;
    Some((
        captures.get(2)?.as_str().to_string(),
        captures.get(1)?.as_str().parse().ok()?,
        None,
    ))
}

fn project_block(b: &ContentBlock) -> Value {
    match b {
        ContentBlock::Text { text, .. } => json!({ "type": "text", "text": text }),
        ContentBlock::Image {
            media_type, data, ..
        } => json!({
            "type": "image",
            "source": { "type": "base64", "media_type": media_type, "data": data },
        }),
        ContentBlock::ImageUrl { url, .. } => json!({
            "type": "image",
            "source": { "type": "url", "url": url },
        }),
        ContentBlock::AudioUrl { .. } => json!({
            "type": "text",
            "text": "[audio omitted: not supported by this provider; re-read the file to hear it]",
        }),
        ContentBlock::VideoUrl { url, .. } => {
            if !url.starts_with("data:") {
                json!({ "type": "video", "source": { "type": "url", "url": url } })
            } else if let Some((media_type, data)) = url[5..].split_once(";base64,") {
                json!({
                    "type": "video",
                    "source": { "type": "base64", "media_type": media_type, "data": data },
                })
            } else {
                json!({
                    "type": "text",
                    "text": "[video omitted: not supported by this provider; re-read the file to view it]",
                })
            }
        }
        ContentBlock::Think {
            think, encrypted, ..
        } => {
            let mut obj = json!({ "type": "thinking", "thinking": think });
            if let Some(sig) = encrypted {
                obj["signature"] = json!(sig);
            }
            obj
        }
        ContentBlock::MediaRef { kind, .. } => json!({
            "type": "text",
            "text": crate::llm::media_resolver::unavailable_text(*kind),
        }),
    }
}

fn tool_result_content(blocks: &[ContentBlock]) -> Vec<Value> {
    blocks
        .iter()
        .filter(|block| !matches!(block, ContentBlock::Think { .. }))
        .filter(|block| !matches!(block, ContentBlock::Text { text, .. } if text.is_empty()))
        .map(project_block)
        .collect()
}

pub fn parse_response(v: &Value) -> Result<LLMChatResponse, String> {
    let content = v
        .get("content")
        .and_then(|c| c.as_array())
        .ok_or("anthropic response missing content array")?;

    let mut text = String::new();
    let mut thinking = Vec::new();
    let mut tool_calls = Vec::new();
    for block in content {
        match block.get("type").and_then(|t| t.as_str()) {
            Some("thinking") => {
                let think = block.get("thinking").and_then(|x| x.as_str()).unwrap_or("");
                let signature = block
                    .get("signature")
                    .and_then(|x| x.as_str())
                    .map(|s| s.to_string());
                thinking.push(ContentBlock::Think {
                    think: think.to_string(),
                    encrypted: signature,
                    details_index: None,
                    reasoning_key: None,
                    details_summary: None,
                });
            }
            Some("redacted_thinking") => {
                let data = block
                    .get("data")
                    .and_then(|x| x.as_str())
                    .map(|s| s.to_string());
                thinking.push(ContentBlock::Think {
                    think: String::new(),
                    encrypted: data,
                    details_index: None,
                    reasoning_key: None,
                    details_summary: None,
                });
            }
            Some("text") => {
                if let Some(t) = block.get("text").and_then(|x| x.as_str()) {
                    text.push_str(t);
                }
            }
            Some("tool_use") => {
                let id = block
                    .get("id")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = block
                    .get("name")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                let arguments = block.get("input").cloned().unwrap_or_else(|| json!({}));
                tool_calls.push(ToolCall {
                    id,
                    name,
                    arguments,
                    extras: None,
                    args_parse_error: None,
                });
            }
            _ => {}
        }
    }

    let finish_reason = v
        .get("stop_reason")
        .and_then(|s| s.as_str())
        .map(|s| s.to_string());

    let raw_input = v
        .get("usage")
        .and_then(|u| u.get("input_tokens"))
        .and_then(|x| x.as_u64())
        .unwrap_or(0) as u32;
    let output_tokens = v
        .get("usage")
        .and_then(|u| u.get("output_tokens"))
        .and_then(|x| x.as_u64())
        .unwrap_or(0) as u32;
    let input_cache_read = v
        .get("usage")
        .and_then(|u| u.get("cache_read_input_tokens"))
        .and_then(|x| x.as_u64())
        .unwrap_or(0) as u32;
    let input_cache_creation = v
        .get("usage")
        .and_then(|u| u.get("cache_creation_input_tokens"))
        .and_then(|x| x.as_u64())
        .unwrap_or(0) as u32;
    let input_tokens = raw_input;

    Ok(LLMChatResponse {
        content: text,
        thinking,
        tool_calls,
        finish_reason,
        usage: TokenUsage {
            input_tokens,
            output_tokens,
            total_tokens: input_tokens
                .saturating_add(input_cache_read)
                .saturating_add(input_cache_creation)
                .saturating_add(output_tokens),
            input_cache_read,
            input_cache_creation,
        },
        timing: None,
    })
}

pub fn default_max_tokens_for_model(model: &str) -> u32 {
    claude_max_tokens_ceiling(model).unwrap_or(64_000)
}

#[derive(Debug, Clone)]
enum PartialBlock {
    Text,
    Thinking {
        thinking: String,
        signature: Option<String>,
    },
    ToolUse {
        id: String,
        name: String,
        input_json: String,
    },
}

const MAX_STREAM_BLOCKS: usize = 256;

#[derive(Debug, Default)]
pub struct StreamAccumulator {
    content: String,
    blocks: Vec<Option<PartialBlock>>,
    finish_reason: Option<String>,
    usage: TokenUsage,
    pending_tool_calls: Vec<StreamDelta>,
}

impl StreamAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn take_tool_call_deltas(&mut self) -> Vec<StreamDelta> {
        std::mem::take(&mut self.pending_tool_calls)
    }

    fn slot(&mut self, index: usize) -> Option<&mut Option<PartialBlock>> {
        if index >= MAX_STREAM_BLOCKS {
            return None;
        }
        while self.blocks.len() <= index {
            self.blocks.push(None);
        }
        self.blocks.get_mut(index)
    }

    pub fn feed(&mut self, v: &Value) -> Option<StreamDelta> {
        let event_type = v.get("type").and_then(|t| t.as_str())?;
        match event_type {
            "message_start" => {
                if let Some(usage) = v.get("message").and_then(|m| m.get("usage")) {
                    let raw_input = usage
                        .get("input_tokens")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(0) as u32;
                    self.usage.input_cache_read = usage
                        .get("cache_read_input_tokens")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(0) as u32;
                    self.usage.input_cache_creation = usage
                        .get("cache_creation_input_tokens")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(0) as u32;
                    self.usage.input_tokens = raw_input;
                }
                None
            }
            "content_block_start" => {
                let index = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                let block = v.get("content_block")?;
                let partial = match block.get("type").and_then(|t| t.as_str()) {
                    Some("thinking") => {
                        let think = block.get("thinking").and_then(|t| t.as_str()).unwrap_or("");
                        let signature = block
                            .get("signature")
                            .and_then(|s| s.as_str())
                            .map(|s| s.to_string());
                        PartialBlock::Thinking {
                            thinking: think.to_string(),
                            signature,
                        }
                    }
                    Some("redacted_thinking") => PartialBlock::Thinking {
                        thinking: String::new(),
                        signature: block
                            .get("data")
                            .and_then(|x| x.as_str())
                            .map(|s| s.to_string()),
                    },
                    Some("tool_use") => PartialBlock::ToolUse {
                        id: block
                            .get("id")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string(),
                        name: block
                            .get("name")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string(),
                        input_json: String::new(),
                    },
                    _ => PartialBlock::Text,
                };
                if let Some(slot) = self.slot(index) {
                    *slot = Some(partial);
                }
                None
            }
            "content_block_delta" => {
                let index = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                let delta = v.get("delta")?;
                match delta.get("type").and_then(|t| t.as_str()) {
                    Some("text_delta") => {
                        let text = delta.get("text").and_then(|x| x.as_str())?;
                        if text.is_empty() {
                            return None;
                        }
                        self.content.push_str(text);
                        Some(StreamDelta::Text(text.to_string()))
                    }
                    Some("thinking_delta") => {
                        let think = delta.get("thinking").and_then(|x| x.as_str())?;
                        if let Some(Some(PartialBlock::Thinking { thinking, .. })) =
                            self.blocks.get_mut(index)
                        {
                            thinking.push_str(think);
                        }
                        if think.is_empty() {
                            return None;
                        }
                        Some(StreamDelta::Think(think.to_string()))
                    }
                    Some("signature_delta") => {
                        if let Some(Some(PartialBlock::Thinking { signature, .. })) =
                            self.blocks.get_mut(index)
                            && let Some(sig) = delta.get("signature").and_then(|x| x.as_str())
                        {
                            if let Some(s) = signature.as_mut() {
                                s.push_str(sig);
                            } else {
                                *signature = Some(sig.to_string());
                            }
                        }
                        None
                    }
                    Some("input_json_delta") => {
                        if let Some(Some(PartialBlock::ToolUse { id, input_json, .. })) =
                            self.blocks.get_mut(index)
                            && let Some(fragment) =
                                delta.get("partial_json").and_then(|x| x.as_str())
                        {
                            input_json.push_str(fragment);
                            let id = id.clone();
                            self.pending_tool_calls.push(StreamDelta::ToolCall {
                                id,
                                index: Some(index),
                                arguments: fragment.to_string(),
                            });
                        }
                        None
                    }
                    _ => None,
                }
            }
            "message_delta" => {
                if let Some(sr) = v
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(|s| s.as_str())
                {
                    self.finish_reason = Some(sr.to_string());
                }
                if let Some(out) = v
                    .get("usage")
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(|x| x.as_u64())
                {
                    self.usage.output_tokens = out as u32;
                }
                None
            }
            _ => None,
        }
    }

    pub fn finish(mut self) -> LLMChatResponse {
        self.usage.total_tokens = self
            .usage
            .input_tokens
            .saturating_add(self.usage.input_cache_read)
            .saturating_add(self.usage.input_cache_creation)
            .saturating_add(self.usage.output_tokens);
        let mut thinking = Vec::new();
        let mut tool_calls = Vec::new();

        for block in self.blocks.into_iter().flatten() {
            match block {
                PartialBlock::Thinking {
                    thinking: think,
                    signature,
                } => {
                    if !think.is_empty() || signature.is_some() {
                        thinking.push(ContentBlock::Think {
                            think,
                            encrypted: signature,
                            details_index: None,
                            reasoning_key: None,
                            details_summary: None,
                        });
                    }
                }
                PartialBlock::ToolUse {
                    id,
                    name,
                    input_json,
                } if !name.is_empty() => {
                    let (arguments, args_parse_error) =
                        crate::llm::tool_args::parse_tool_arguments(Some(input_json.as_str()));
                    tool_calls.push(ToolCall {
                        id,
                        name,
                        arguments,
                        extras: None,
                        args_parse_error,
                    });
                }
                _ => {}
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_request_hoists_system_and_encodes_tool_use_as_object() {
        let messages = vec![
            WireMessage::text("system", "sys-a"),
            WireMessage::text("system", "sys-b"),
            WireMessage::text("user", "hi"),
            WireMessage::assistant_tool_calls(
                "let me look",
                vec![ToolCall {
                    id: "tu_1".into(),
                    name: "Read".into(),
                    arguments: json!({ "path": "a.txt" }),
                    extras: None,
                    args_parse_error: None,
                }],
            ),
            WireMessage::tool_result("tu_1", "file body"),
        ];
        let tools = vec![ToolInfo {
            name: "Read".into(),
            description: "read a file".into(),
            input_schema: json!({ "type": "object" }),
        }];

        let req = build_request("claude-x", 4096, &messages, &tools);

        assert_eq!(req["model"], "claude-x");
        assert_eq!(req["max_tokens"], 4096);
        assert_eq!(req["system"][0]["text"], "sys-a\n\nsys-b");
        assert_eq!(req["system"][0]["cache_control"]["type"], "ephemeral");

        let msgs = req["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"][0]["type"], "text");

        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["content"][0]["type"], "text");
        let tool_use = &msgs[1]["content"][1];
        assert_eq!(tool_use["type"], "tool_use");
        assert_eq!(tool_use["id"], "tu_1");
        assert_eq!(tool_use["name"], "Read");
        assert_eq!(tool_use["input"], json!({ "path": "a.txt" }));

        assert_eq!(msgs[2]["role"], "user");
        let tr = &msgs[2]["content"][0];
        assert_eq!(tr["type"], "tool_result");
        assert_eq!(tr["tool_use_id"], "tu_1");
        assert_eq!(tr["content"], "file body");
        assert_eq!(tr["cache_control"]["type"], "ephemeral");

        assert_eq!(req["tools"][0]["name"], "Read");
        assert_eq!(req["tools"][0]["input_schema"], json!({ "type": "object" }));
        assert_eq!(req["tools"][0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn build_request_merges_consecutive_user_and_tool_messages() {
        let messages = vec![
            WireMessage::text("user", "part 1"),
            WireMessage::text("user", "part 2"),
            WireMessage::assistant_tool_calls(
                "calling",
                vec![
                    ToolCall {
                        id: "call_1".into(),
                        name: "Read".into(),
                        arguments: json!({ "path": "a.txt" }),
                        extras: None,
                        args_parse_error: None,
                    },
                    ToolCall {
                        id: "call_2".into(),
                        name: "Read".into(),
                        arguments: json!({ "path": "b.txt" }),
                        extras: None,
                        args_parse_error: None,
                    },
                ],
            ),
            WireMessage::tool_result("call_1", "res 1"),
            WireMessage::tool_result("call_2", "res 2"),
            WireMessage::text("user", "continue please"),
        ];

        let req = build_request("claude-x", 4096, &messages, &[]);
        let msgs = req["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);

        assert_eq!(msgs[0]["role"], "user");
        let u0 = msgs[0]["content"].as_array().unwrap();
        assert_eq!(u0.len(), 2);
        assert_eq!(u0[0]["text"], "part 1");
        assert_eq!(u0[1]["text"], "part 2");

        assert_eq!(msgs[1]["role"], "assistant");

        assert_eq!(msgs[2]["role"], "user");
        let u2 = msgs[2]["content"].as_array().unwrap();
        assert_eq!(u2.len(), 3);
        assert_eq!(u2[0]["type"], "tool_result");
        assert_eq!(u2[0]["tool_use_id"], "call_1");
        assert_eq!(u2[1]["type"], "tool_result");
        assert_eq!(u2[1]["tool_use_id"], "call_2");
        assert_eq!(u2[2]["type"], "text");
        assert_eq!(u2[2]["text"], "continue please");
        assert_eq!(u2[2]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn a_tool_result_with_an_image_block_carries_it_in_the_content_blocks() {
        let messages = vec![
            WireMessage::text("user", "look at this"),
            WireMessage::assistant_tool_calls(
                "reading",
                vec![ToolCall {
                    id: "tu_1".into(),
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
                tool_call_id: Some("tu_1".into()),
            },
        ];

        let req = build_request("claude-x", 4096, &messages, &[]);
        let msgs = req["messages"].as_array().unwrap();
        let tr = &msgs.last().unwrap()["content"][0];
        assert_eq!(tr["type"], "tool_result");
        let content = tr["content"]
            .as_array()
            .expect("tool_result content must be content blocks");
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["source"]["type"], "base64");
        assert_eq!(content[1]["source"]["media_type"], "image/png");
        assert_eq!(content[1]["source"]["data"], "QUJD");
    }

    #[test]
    fn a_tool_result_with_a_video_block_carries_a_video_block() {
        let messages = vec![
            WireMessage::text("user", "watch this"),
            WireMessage {
                role: "tool".into(),
                content: "".into(),
                blocks: vec![
                    ContentBlock::Text {
                        text: "<video path=\"a.mp4\">".into(),
                        meta: None,
                    },
                    ContentBlock::VideoUrl {
                        url: "data:video/mp4;base64,AAAA".into(),
                        id: None,
                        name: None,
                    },
                    ContentBlock::VideoUrl {
                        url: "https://example.test/b.webm".into(),
                        id: None,
                        name: None,
                    },
                ],
                tool_calls: Vec::new(),
                tool_call_id: Some("tu_1".into()),
            },
        ];

        let req = build_request("claude-x", 4096, &messages, &[]);
        let msgs = req["messages"].as_array().unwrap();
        let tool_result = msgs.last().unwrap()["content"]
            .as_array()
            .unwrap()
            .iter()
            .find(|block| block["type"] == "tool_result")
            .expect("the tool result must be projected");
        let content = tool_result["content"].as_array().unwrap();
        assert_eq!(content[1]["type"], "video");
        assert_eq!(content[1]["source"]["type"], "base64");
        assert_eq!(content[1]["source"]["media_type"], "video/mp4");
        assert_eq!(content[1]["source"]["data"], "AAAA");
        assert_eq!(content[2]["type"], "video");
        assert_eq!(content[2]["source"]["type"], "url");
        assert_eq!(content[2]["source"]["url"], "https://example.test/b.webm");
    }

    #[test]
    fn build_request_without_system_omits_field() {
        let req = build_request("m", 100, &[WireMessage::text("user", "x")], &[]);
        assert!(req.get("system").is_none());
        assert!(req.get("tools").is_none());
    }

    #[test]
    fn parse_response_extracts_tool_use_and_usage() {
        let v = json!({
            "content": [
                { "type": "text", "text": "sure" },
                { "type": "tool_use", "id": "tu_9", "name": "Grep", "input": { "q": "foo" } }
            ],
            "stop_reason": "tool_use",
            "usage": { "input_tokens": 20, "output_tokens": 8 }
        });

        let parsed = parse_response(&v).unwrap();
        assert_eq!(parsed.finish_reason.as_deref(), Some("tool_use"));
        assert_eq!(parsed.tool_calls.len(), 1);
        assert_eq!(parsed.tool_calls[0].id, "tu_9");
        assert_eq!(parsed.tool_calls[0].name, "Grep");
        assert_eq!(parsed.tool_calls[0].arguments, json!({ "q": "foo" }));
        assert_eq!(parsed.usage.input_tokens, 20);
        assert_eq!(parsed.usage.output_tokens, 8);
        assert_eq!(parsed.usage.total_tokens, 28);
    }

    #[test]
    fn parse_response_extracts_cache_usage() {
        let v = json!({
            "content": [{ "type": "text", "text": "sure" }],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 50,
                "output_tokens": 6,
                "cache_read_input_tokens": 40,
                "cache_creation_input_tokens": 5
            }
        });
        let parsed = parse_response(&v).unwrap();
        assert_eq!(parsed.usage.input_cache_read, 40);
        assert_eq!(parsed.usage.input_cache_creation, 5);
        assert_eq!(parsed.usage.input_tokens, 50);
        assert_eq!(parsed.usage.total_tokens, 101);
    }

    #[test]
    fn parse_response_plain_text_has_no_tool_calls() {
        let v = json!({
            "content": [{ "type": "text", "text": "hello" }],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 3, "output_tokens": 2 }
        });
        let parsed = parse_response(&v).unwrap();
        assert!(parsed.tool_calls.is_empty());
        assert_eq!(parsed.finish_reason.as_deref(), Some("end_turn"));
    }

    #[test]
    fn parse_response_errors_on_missing_content() {
        assert!(parse_response(&json!({})).is_err());
    }

    #[test]
    fn parse_response_extracts_text_content() {
        let v = json!({
            "content": [
                { "type": "text", "text": "sure, " },
                { "type": "text", "text": "here" }
            ],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 1, "output_tokens": 1 }
        });
        let parsed = parse_response(&v).unwrap();
        assert_eq!(parsed.content, "sure, here");
    }

    #[test]
    fn build_request_streaming_sets_stream_flag() {
        let req =
            build_request_with_options("m", 100, &[WireMessage::text("user", "x")], &[], true);
        assert_eq!(req["stream"], true);
    }

    #[test]
    fn build_request_projects_image_blocks() {
        use kimi_core::types::ContentBlock;
        let msg = WireMessage::with_blocks(
            "user",
            vec![
                ContentBlock::Text {
                    text: "look".into(),
                    meta: None,
                },
                ContentBlock::Image {
                    media_type: "image/jpeg".into(),
                    data: "BBBB".into(),
                    name: None,
                },
            ],
        );
        let req = build_request("m", 100, &[msg], &[]);
        let content = req["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["source"]["type"], "base64");
        assert_eq!(content[1]["source"]["media_type"], "image/jpeg");
        assert_eq!(content[1]["source"]["data"], "BBBB");
    }

    #[test]
    fn build_request_degrades_audio_and_sends_video_blocks() {
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
                    id: None,
                    name: None,
                },
            ],
        );
        let req = build_request("m", 100, &[msg], &[]);
        let content = req["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "text");
        assert!(
            content[0]["text"]
                .as_str()
                .unwrap()
                .contains("audio omitted")
        );
        assert_eq!(content[1]["type"], "video");
        assert_eq!(content[1]["source"]["url"], "https://example.com/v.mp4");
    }

    #[test]
    fn stream_accumulator_collects_text_tool_use_and_usage() {
        let mut acc = StreamAccumulator::new();

        acc.feed(
            &json!({ "type": "message_start", "message": { "usage": { "input_tokens": 25 } } }),
        );
        acc.feed(&json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text" } }));
        let d1 = acc.feed(&json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "Hi " } }));
        assert_eq!(d1, Some(StreamDelta::Text("Hi ".into())));
        let d2 = acc.feed(&json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "there" } }));
        assert_eq!(d2, Some(StreamDelta::Text("there".into())));
        acc.feed(&json!({ "type": "content_block_stop", "index": 0 }));

        acc.feed(&json!({ "type": "content_block_start", "index": 1, "content_block": { "type": "tool_use", "id": "tu_1", "name": "Read" } }));
        acc.feed(&json!({ "type": "content_block_delta", "index": 1, "delta": { "type": "input_json_delta", "partial_json": "{\"path\":" } }));
        acc.feed(&json!({ "type": "content_block_delta", "index": 1, "delta": { "type": "input_json_delta", "partial_json": "\"a.txt\"}" } }));
        acc.feed(&json!({ "type": "content_block_stop", "index": 1 }));

        acc.feed(&json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" }, "usage": { "output_tokens": 9 } }));
        acc.feed(&json!({ "type": "message_stop" }));

        let resp = acc.finish();
        assert_eq!(resp.content, "Hi there");
        assert_eq!(resp.finish_reason.as_deref(), Some("tool_use"));
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].id, "tu_1");
        assert_eq!(resp.tool_calls[0].name, "Read");
        assert_eq!(resp.tool_calls[0].arguments, json!({ "path": "a.txt" }));
        assert_eq!(resp.usage.input_tokens, 25);
        assert_eq!(resp.usage.output_tokens, 9);
        assert_eq!(resp.usage.total_tokens, 34);
    }

    #[test]
    fn stream_accumulator_reports_uncached_input() {
        let mut acc = StreamAccumulator::new();
        acc.feed(&json!({
            "type": "message_start",
            "message": { "usage": {
                "input_tokens": 50,
                "cache_read_input_tokens": 40,
                "cache_creation_input_tokens": 5
            } }
        }));
        acc.feed(&json!({ "type": "message_delta", "usage": { "output_tokens": 9 } }));

        let resp = acc.finish();
        assert_eq!(resp.usage.input_cache_read, 40);
        assert_eq!(resp.usage.input_cache_creation, 5);
        assert_eq!(resp.usage.input_tokens, 50);
        assert_eq!(resp.usage.total_tokens, 104);
    }

    #[test]
    fn stream_accumulator_collects_thinking_blocks_and_deltas() {
        let mut acc = StreamAccumulator::new();

        acc.feed(
            &json!({ "type": "message_start", "message": { "usage": { "input_tokens": 10 } } }),
        );
        acc.feed(&json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "thinking" } }));
        let d1 = acc.feed(&json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "thinking_delta", "thinking": "Let's calculate..." }
        }));
        assert_eq!(d1, Some(StreamDelta::Think("Let's calculate...".into())));

        let d_sig = acc.feed(&json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "signature_delta", "signature": "sig_abc123" }
        }));
        assert_eq!(d_sig, None);
        acc.feed(&json!({ "type": "content_block_stop", "index": 0 }));

        acc.feed(&json!({ "type": "content_block_start", "index": 1, "content_block": { "type": "text" } }));
        let d2 = acc.feed(&json!({
            "type": "content_block_delta",
            "index": 1,
            "delta": { "type": "text_delta", "text": "42" }
        }));
        assert_eq!(d2, Some(StreamDelta::Text("42".into())));
        acc.feed(&json!({ "type": "content_block_stop", "index": 1 }));

        acc.feed(&json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" }, "usage": { "output_tokens": 15 } }));
        acc.feed(&json!({ "type": "message_stop" }));

        let resp = acc.finish();
        assert_eq!(resp.content, "42");
        assert_eq!(resp.finish_reason.as_deref(), Some("end_turn"));
        assert_eq!(resp.thinking.len(), 1);
        assert_eq!(
            resp.thinking[0],
            ContentBlock::Think {
                think: "Let's calculate...".into(),
                encrypted: Some("sig_abc123".into()),
                details_index: None,
                reasoning_key: None,
                details_summary: None,
            }
        );
    }

    #[test]
    fn parse_response_extracts_thinking_blocks() {
        let v = json!({
            "content": [
                {
                    "type": "thinking",
                    "thinking": "Step-by-step reasoning",
                    "signature": "sig_xyz"
                },
                {
                    "type": "text",
                    "text": "The answer."
                }
            ],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 10,
                "output_tokens": 20
            }
        });
        let parsed = parse_response(&v).unwrap();
        assert_eq!(parsed.content, "The answer.");
        assert_eq!(parsed.thinking.len(), 1);
        assert_eq!(
            parsed.thinking[0],
            ContentBlock::Think {
                think: "Step-by-step reasoning".into(),
                encrypted: Some("sig_xyz".into()),
                details_index: None,
                reasoning_key: None,
                details_summary: None,
            }
        );
    }

    #[test]
    fn parse_response_keeps_redacted_thinking_as_an_encrypted_think_block() {
        let v = json!({
            "content": [
                { "type": "redacted_thinking", "data": "encrypted_blob" },
                { "type": "thinking", "thinking": "visible", "signature": "sig_1" },
                { "type": "text", "text": "done" }
            ],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 10,
                "output_tokens": 20
            }
        });
        let parsed = parse_response(&v).unwrap();
        assert_eq!(parsed.content, "done");
        assert_eq!(
            parsed.thinking,
            vec![
                ContentBlock::Think {
                    think: String::new(),
                    encrypted: Some("encrypted_blob".into()),
                    details_index: None,
                    reasoning_key: None,
                    details_summary: None,
                },
                ContentBlock::Think {
                    think: "visible".into(),
                    encrypted: Some("sig_1".into()),
                    details_index: None,
                    reasoning_key: None,
                    details_summary: None,
                },
            ],
            "the redacted block must survive, in wire order, beside the signed one"
        );
    }

    #[test]
    fn redacted_thinking_round_trips_back_into_the_next_request() {
        let parsed = parse_response(&json!({
            "content": [{ "type": "redacted_thinking", "data": "encrypted_blob" }],
            "usage": { "input_tokens": 4, "output_tokens": 2 }
        }))
        .unwrap();
        let req = build_request(
            "claude-sonnet-4-5",
            1024,
            &[WireMessage::with_blocks("assistant", parsed.thinking)],
            &[],
        );
        let blocks = req["messages"][0]["content"].as_array().unwrap();
        assert_eq!(
            blocks[0],
            json!({ "type": "thinking", "thinking": "", "signature": "encrypted_blob" }),
            "the attestation must go back out as the thinking signature"
        );
    }

    #[test]
    fn stream_accumulator_keeps_a_redacted_thinking_block() {
        let mut acc = StreamAccumulator::new();
        let delta = acc.feed(&json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": { "type": "redacted_thinking", "data": "encrypted_blob" }
        }));
        assert_eq!(delta, None, "a redacted block has no text to stream");
        assert_eq!(
            acc.finish().thinking,
            vec![ContentBlock::Think {
                think: String::new(),
                encrypted: Some("encrypted_blob".into()),
                details_index: None,
                reasoning_key: None,
                details_summary: None,
            }]
        );
    }

    #[test]
    fn stream_accumulator_ignores_implausible_block_index() {
        let mut acc = StreamAccumulator::new();
        acc.feed(&json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text" } }));
        acc.feed(&json!({ "type": "content_block_start", "index": 50_000_000, "content_block": { "type": "tool_use", "id": "boom", "name": "Boom" } }));

        assert!(acc.blocks.len() <= MAX_STREAM_BLOCKS);
        let resp = acc.finish();
        assert_eq!(
            resp.tool_calls.len(),
            0,
            "the out-of-range block must be dropped"
        );
    }

    #[test]
    fn finish_after_truncated_stream_keeps_partial_content() {
        let mut acc = StreamAccumulator::default();
        acc.feed(&json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "text_delta", "text": "hel" }
        }));
        acc.feed(&json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "text_delta", "text": "lo" }
        }));
        let resp = acc.finish();
        assert_eq!(resp.content, "hello");
        assert!(resp.finish_reason.is_none());
        assert_eq!(resp.usage.total_tokens, 0);
    }

    #[test]
    fn truncated_tool_use_input_keeps_the_call_and_its_reason() {
        let mut acc = StreamAccumulator::default();
        acc.feed(&json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": { "type": "tool_use", "id": "tu_1", "name": "Bash" }
        }));
        acc.feed(&json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "input_json_delta", "partial_json": "{\"command\": \"ech" }
        }));
        let resp = acc.finish();
        assert_eq!(
            resp.tool_calls.len(),
            1,
            "the call survives its own parse failure"
        );
        assert_eq!(resp.tool_calls[0].arguments, json!({}));
        assert!(
            resp.tool_calls[0].args_parse_error.is_some(),
            "the reason must travel with the call so the loop can report it"
        );
    }

    #[test]
    fn complete_tool_use_input_is_kept() {
        let mut acc = StreamAccumulator::default();
        acc.feed(&json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": { "type": "tool_use", "id": "tu_1", "name": "Read" }
        }));
        for fragment in ["{\"path\"", ": \"a.txt\"}"] {
            acc.feed(&json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": { "type": "input_json_delta", "partial_json": fragment }
            }));
        }
        let resp = acc.finish();
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].name, "Read");
        assert_eq!(resp.tool_calls[0].arguments["path"], "a.txt");
    }

    #[test]
    fn test_build_request_thinking_budget() {
        let msgs = vec![WireMessage {
            role: "user".into(),
            content: "hello".into(),
            blocks: vec![],
            tool_calls: vec![],
            tool_call_id: None,
        }];
        let req_thinking = build_request_full(
            "claude-3-7-sonnet-20250219",
            4096,
            &msgs,
            &[],
            true,
            Some(4096),
            None,
        );
        assert_eq!(req_thinking["thinking"]["type"], "enabled");
        assert_eq!(req_thinking["thinking"]["budget_tokens"], 4095);
        assert_eq!(req_thinking["max_tokens"], 4096);

        let req_no_thinking = build_request_full(
            "claude-3-7-sonnet-20250219",
            4096,
            &msgs,
            &[],
            true,
            None,
            None,
        );
        assert!(req_no_thinking.get("thinking").is_none());
        assert_eq!(req_no_thinking["max_tokens"], 4096);
    }

    #[test]
    fn test_build_request_thinking_keep() {
        let msgs = vec![WireMessage::text("user", "hello")];
        let req_keep = build_request_full(
            "claude-sonnet-4-5",
            64000,
            &msgs,
            &[],
            true,
            None,
            Some("all"),
        );
        assert_eq!(
            req_keep["context_management"]["edits"][0]["type"],
            "clear_thinking_20251015"
        );
        assert_eq!(req_keep["context_management"]["edits"][0]["keep"], "all");

        let req_no_keep =
            build_request_full("claude-sonnet-4-5", 64000, &msgs, &[], true, None, None);
        assert!(req_no_keep.get("context_management").is_none());
    }

    #[test]
    fn test_build_request_adaptive_thinking() {
        let msgs = vec![WireMessage::text("user", "hello")];
        let build = |effort: Option<&str>| {
            build_request_for_provider(
                "claude-opus-4-6",
                64_000,
                &msgs,
                &[],
                true,
                None,
                None,
                effort,
                Some("anthropic"),
                None,
                Some(50_000),
                true,
                false,
                &crate::llm::request_options::RequestOptions::default(),
            )
            .expect("adaptive request")
        };
        let high = build(Some("high"));
        assert_eq!(
            high["thinking"],
            json!({ "type": "adaptive", "display": "summarized" })
        );
        assert_eq!(high["output_config"]["effort"], "high");
        assert!(high["thinking"].get("budget_tokens").is_none());
        let on = build(Some("on"));
        assert_eq!(on["thinking"]["type"], "adaptive");
        assert!(on.get("output_config").is_none());
        let off = build(Some("off"));
        assert_eq!(off["thinking"], json!({ "type": "disabled" }));
        assert!(off.get("output_config").is_none());
    }

    #[test]
    fn test_build_request_derives_the_budget_from_the_effort() {
        let msgs = vec![WireMessage::text("user", "hello")];
        let build = |effort: Option<&str>| {
            build_request_for_provider(
                "claude-sonnet-4-5",
                64_000,
                &msgs,
                &[],
                true,
                None,
                None,
                effort,
                Some("anthropic"),
                None,
                None,
                false,
                false,
                &crate::llm::request_options::RequestOptions::default(),
            )
            .expect("budget request")
        };
        for (effort, budget) in [
            ("low", 1024),
            ("medium", 4096),
            ("high", 32_000),
            ("8192", 8192),
        ] {
            let req = build(Some(effort));
            assert_eq!(req["thinking"]["type"], "enabled", "{effort}");
            assert_eq!(req["thinking"]["budget_tokens"], budget, "{effort}");
        }
        assert!(build(Some("off")).get("thinking").is_none());
        assert!(build(None).get("thinking").is_none());
    }

    #[test]
    fn test_build_request_kimi_anthropic_trait() {
        let msgs = vec![WireMessage::text("user", "hello")];
        let build = |effort: Option<&str>| {
            build_request_for_provider(
                "k3",
                64000,
                &msgs,
                &[],
                true,
                None,
                Some("all"),
                effort,
                Some("kimi"),
                Some("sess-9"),
                Some(50_000),
                false,
                false,
                &crate::llm::request_options::RequestOptions::default(),
            )
            .expect("kimi request")
        };

        let high = build(Some("high"));
        assert_eq!(high["thinking"], json!({ "type": "enabled" }));
        assert_eq!(high["output_config"]["effort"], "high");
        assert!(high["thinking"].get("budget_tokens").is_none());
        assert_eq!(high["max_tokens"], 50_000);
        assert_eq!(high["metadata"]["user_id"], "sess-9");
        assert_eq!(high["context_management"]["edits"][0]["keep"], "all");

        let on = build(Some("on"));
        assert_eq!(on["thinking"], json!({ "type": "enabled" }));
        assert!(on.get("output_config").is_none());

        let off = build(Some("off"));
        assert_eq!(off["thinking"], json!({ "type": "disabled" }));
        assert!(off.get("output_config").is_none());

        let standard = build_request_for_provider(
            "claude-sonnet-4-5",
            64_000,
            &msgs,
            &[],
            true,
            Some(32_000),
            None,
            Some("high"),
            Some("anthropic"),
            None,
            Some(50_000),
            false,
            false,
            &crate::llm::request_options::RequestOptions::default(),
        )
        .expect("standard request");
        assert_eq!(standard["thinking"]["budget_tokens"], 32_000);
        assert!(standard.get("output_config").is_none());
        assert_eq!(standard["max_tokens"], 50_000);
    }

    #[test]
    fn default_max_tokens_matches_the_v2_ceiling_table() {
        assert_eq!(default_max_tokens_for_model("claude-sonnet-4-5"), 64_000);
        assert_eq!(default_max_tokens_for_model("claude-opus-4-1"), 32_000);
        assert_eq!(default_max_tokens_for_model("claude-sonnet-4-6"), 128_000);
        assert_eq!(default_max_tokens_for_model("claude-opus-4-7"), 128_000);
        assert_eq!(default_max_tokens_for_model("claude-haiku-4-5"), 64_000);
        assert_eq!(
            default_max_tokens_for_model("claude-3-7-sonnet-20250219"),
            8192
        );
        assert_eq!(default_max_tokens_for_model("claude-3-5-haiku"), 8192);
        assert_eq!(default_max_tokens_for_model("claude-3-opus"), 4096);
        assert_eq!(default_max_tokens_for_model("some-unknown-model"), 64_000);
    }

    #[test]
    fn default_max_tokens_agrees_with_v2_on_every_real_claude_id() {
        let cases = [
            ("claude-3-opus-20240229", 4_096),
            ("claude-3-sonnet-20240229", 4_096),
            ("claude-3-haiku-20240307", 4_096),
            ("claude-3-5-sonnet-20241022", 8_192),
            ("claude-3-5-haiku-20241022", 8_192),
            ("claude-3-7-sonnet-20250219", 8_192),
            ("claude-opus-4-20250514", 32_000),
            ("claude-opus-4-1-20250805", 32_000),
            ("claude-sonnet-4-20250514", 64_000),
            ("claude-sonnet-4-5-20250929", 64_000),
            ("claude-haiku-4-5-20251001", 64_000),
            ("claude-sonnet-4-6", 128_000),
            ("claude-opus-4-6", 128_000),
            ("claude-opus-4-7", 128_000),
            ("claude-opus-4-8", 128_000),
            ("claude-sonnet-5", 128_000),
            ("claude-fable-5", 128_000),
            ("claude-mythos-5", 128_000),
        ];
        for (model, expected) in cases {
            assert_eq!(
                default_max_tokens_for_model(model),
                expected,
                "{model} ceiling must match v2 CEILING_BY_FAMILY_VERSION"
            );
        }

        for model in ["my-opus-4-5-clone", "my-sonnet-4-6-clone", "relay-opus-4-1"] {
            assert_eq!(
                default_max_tokens_for_model(model),
                64_000,
                "{model} has no `claude` marker, so v2 answers its 64000 fallback"
            );
        }
    }

    #[test]
    fn test_anthropic_prompt_caching_stable_history_and_tail() {
        let msgs = vec![
            WireMessage::text("user", "turn 1"),
            WireMessage::text("assistant", "answer 1"),
            WireMessage::text("user", "turn 2"),
            WireMessage::text("assistant", "answer 2"),
            WireMessage::text("user", "turn 3"),
        ];
        let tools = vec![ToolInfo {
            name: "read_file".into(),
            description: "read a file".into(),
            input_schema: json!({ "type": "object" }),
        }];
        let req = build_request_full("claude-3-7-sonnet", 4096, &msgs, &tools, true, None, None);

        let req_tools = req["tools"].as_array().unwrap();
        assert_eq!(req_tools[0]["cache_control"]["type"], "ephemeral");

        let req_msgs = req["messages"].as_array().unwrap();
        assert_eq!(req_msgs.len(), 5);

        let stable_content = req_msgs[2]["content"].as_array().unwrap();
        assert_eq!(stable_content[0]["cache_control"]["type"], "ephemeral");

        let tail_content = req_msgs[4]["content"].as_array().unwrap();
        assert_eq!(tail_content[0]["cache_control"]["type"], "ephemeral");

        let mid_content = req_msgs[1]["content"].as_array().unwrap();
        assert!(mid_content[0].get("cache_control").is_none());
    }

    #[test]
    fn test_anthropic_prompt_caching_tail_on_assistant_tool_use() {
        let msgs = vec![
            WireMessage::text("user", "list files"),
            WireMessage {
                role: "assistant".into(),
                content: "Sure, let me check:".into(),
                blocks: vec![],
                tool_calls: vec![ToolCall {
                    id: "call_ls".into(),
                    name: "glob".into(),
                    arguments: json!({ "pattern": "*.rs" }),
                    extras: None,
                    args_parse_error: None,
                }],
                tool_call_id: None,
            },
        ];
        let req = build_request("claude-3-7-sonnet", 4096, &msgs, &[]);
        let req_msgs = req["messages"].as_array().unwrap();
        assert_eq!(req_msgs.len(), 2);

        let asst_content = req_msgs[1]["content"].as_array().unwrap();
        assert_eq!(asst_content.len(), 2);
        assert_eq!(asst_content[1]["type"], "tool_use");
        assert_eq!(asst_content[1]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn cacheable_block_types_match_the_v2_vocabulary() {
        for cacheable in CACHEABLE_TYPES {
            assert!(
                is_cacheable_block(&json!({ "type": cacheable })),
                "{cacheable} is cacheable in v2"
            );
        }
        for not_cacheable in ["thinking", "redacted_thinking", "some_future_block"] {
            assert!(
                !is_cacheable_block(&json!({ "type": not_cacheable })),
                "{not_cacheable} is not in v2's CACHEABLE_TYPES"
            );
        }
        assert!(!is_cacheable_block(&json!({ "text": "typeless" })));
    }

    #[test]
    fn tail_breakpoint_skips_a_thinking_block() {
        let msgs = vec![
            WireMessage::text("user", "turn 1"),
            WireMessage::with_blocks(
                "assistant",
                vec![ContentBlock::Think {
                    think: "still reasoning".into(),
                    encrypted: Some("sig-abc".into()),
                    details_index: None,
                    reasoning_key: None,
                    details_summary: None,
                }],
            ),
        ];
        let req = build_request("claude-3-7-sonnet", 4096, &msgs, &[]);
        let req_msgs = req["messages"].as_array().unwrap();
        let tail = req_msgs[1]["content"].as_array().unwrap();
        assert_eq!(tail[0]["type"], "thinking");
        assert!(
            tail[0].get("cache_control").is_none(),
            "a thinking tail must not take a breakpoint: {tail:?}"
        );
    }

    #[test]
    fn tail_breakpoint_lands_on_the_cacheable_tail_shapes() {
        for (label, msgs) in [
            ("text", vec![WireMessage::text("user", "hi")]),
            (
                "tool_use",
                vec![WireMessage::assistant_tool_calls(
                    "",
                    vec![ToolCall {
                        id: "call_1".into(),
                        name: "glob".into(),
                        arguments: json!({ "pattern": "*.rs" }),
                        extras: None,
                        args_parse_error: None,
                    }],
                )],
            ),
            (
                "image",
                vec![WireMessage::with_blocks(
                    "user",
                    vec![
                        ContentBlock::Text {
                            text: "look".into(),
                            meta: None,
                        },
                        ContentBlock::Image {
                            media_type: "image/jpeg".into(),
                            data: "BBBB".into(),
                            name: None,
                        },
                    ],
                )],
            ),
        ] {
            let req = build_request("claude-3-7-sonnet", 4096, &msgs, &[]);
            let tail = req["messages"].as_array().unwrap()[0]["content"]
                .as_array()
                .unwrap();
            assert_eq!(
                tail[tail.len() - 1]["cache_control"]["type"],
                "ephemeral",
                "{label} tail must take the breakpoint: {tail:?}"
            );
        }
    }

    #[test]
    fn stable_breakpoint_skips_a_thinking_block() {
        let think_only = |why: &str| {
            WireMessage::with_blocks(
                "assistant",
                vec![ContentBlock::Think {
                    think: why.into(),
                    encrypted: Some("sig-abc".into()),
                    details_index: None,
                    reasoning_key: None,
                    details_summary: None,
                }],
            )
        };
        let msgs = vec![
            WireMessage::text("user", "turn 1"),
            think_only("first pass"),
            think_only("second pass"),
            WireMessage::text("user", "turn 2"),
        ];
        let req = build_request("claude-3-7-sonnet", 4096, &msgs, &[]);
        let req_msgs = req["messages"].as_array().unwrap();
        assert_eq!(req_msgs.len(), 4);

        let stable = req_msgs[1]["content"].as_array().unwrap();
        assert_eq!(stable[0]["type"], "thinking");
        assert!(
            stable[0].get("cache_control").is_none(),
            "a thinking stable block must not take a breakpoint: {stable:?}"
        );

        let tail = req_msgs[3]["content"].as_array().unwrap();
        assert_eq!(tail[0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn stable_breakpoint_lands_on_a_cacheable_image_block() {
        let msgs = vec![
            WireMessage::text("user", "turn 1"),
            WireMessage::text("assistant", "answer 1"),
            WireMessage::with_blocks(
                "user",
                vec![
                    ContentBlock::Text {
                        text: "look at this".into(),
                        meta: None,
                    },
                    ContentBlock::Image {
                        media_type: "image/png".into(),
                        data: "AAAA".into(),
                        name: None,
                    },
                ],
            ),
            WireMessage::text("assistant", "answer 2"),
            WireMessage::text("user", "turn 3"),
        ];
        let req = build_request("claude-3-7-sonnet", 4096, &msgs, &[]);
        let req_msgs = req["messages"].as_array().unwrap();
        assert_eq!(req_msgs.len(), 5);

        let stable = req_msgs[2]["content"].as_array().unwrap();
        assert_eq!(stable[1]["type"], "image");
        assert_eq!(stable[1]["cache_control"]["type"], "ephemeral");
        assert!(stable[0].get("cache_control").is_none());

        let tail = req_msgs[4]["content"].as_array().unwrap();
        assert_eq!(tail[0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn test_assistant_thinking_blocks_round_trip_first_with_signature() {
        use crate::rpc::types::ContentBlock;
        let msgs = vec![WireMessage {
            role: "assistant".into(),
            content: "Let me check.".into(),
            blocks: vec![ContentBlock::Think {
                think: "need to list files".into(),
                encrypted: Some("sig-abc".into()),
                details_index: None,
                reasoning_key: None,
                details_summary: None,
            }],
            tool_calls: vec![ToolCall {
                id: "call_ls".into(),
                name: "glob".into(),
                arguments: json!({ "pattern": "*.rs" }),
                extras: None,
                args_parse_error: None,
            }],
            tool_call_id: None,
        }];
        let req = build_request("claude-3-7-sonnet", 4096, &msgs, &[]);
        let asst_content = req["messages"][0]["content"].as_array().unwrap();
        assert_eq!(asst_content.len(), 3, "{asst_content:?}");
        assert_eq!(asst_content[0]["type"], "thinking");
        assert_eq!(asst_content[0]["thinking"], "need to list files");
        assert_eq!(asst_content[0]["signature"], "sig-abc");
        assert_eq!(asst_content[1]["type"], "text");
        assert_eq!(asst_content[2]["type"], "tool_use");
    }

    fn request_with_options(
        model: &str,
        max_tokens: u32,
        budget: Option<u32>,
        keep: Option<&str>,
        provider: Option<&str>,
        use_beta_api: bool,
        options: &crate::llm::request_options::RequestOptions,
    ) -> Result<Value, String> {
        build_request_for_provider(
            model,
            max_tokens,
            &[WireMessage::text("user", "hello")],
            &[],
            true,
            budget,
            keep,
            None,
            provider,
            None,
            None,
            false,
            use_beta_api,
            options,
        )
    }

    #[test]
    fn thinking_budget_is_lowered_never_the_cap_raised() {
        let req = request_with_options(
            "claude-sonnet-4-5",
            8000,
            Some(20_000),
            None,
            Some("anthropic"),
            false,
            &crate::llm::request_options::RequestOptions::default(),
        )
        .expect("valid request");
        assert_eq!(req["max_tokens"], 8000, "the cap is untouched");
        assert_eq!(req["thinking"]["budget_tokens"], 7999);

        let error = request_with_options(
            "claude-sonnet-4-5",
            1024,
            Some(4096),
            None,
            Some("anthropic"),
            false,
            &crate::llm::request_options::RequestOptions::default(),
        )
        .expect_err("a 1024 cap cannot host thinking");
        assert!(error.contains("1024"), "{error}");

        let req = request_with_options(
            "claude-sonnet-4-5",
            64_000,
            Some(4096),
            None,
            Some("anthropic"),
            false,
            &crate::llm::request_options::RequestOptions::default(),
        )
        .expect("valid request");
        assert_eq!(req["thinking"]["budget_tokens"], 4096);
    }

    #[test]
    fn beta_features_follow_the_thinking_profile() {
        assert_eq!(
            anthropic_betas(Some("anthropic"), false, true, None),
            vec![INTERLEAVED_THINKING_BETA]
        );
        assert!(anthropic_betas(Some("anthropic"), true, true, None).is_empty());
        assert_eq!(
            anthropic_betas(Some("anthropic"), false, true, Some("all")),
            vec![INTERLEAVED_THINKING_BETA, CONTEXT_MANAGEMENT_BETA]
        );
        assert_eq!(
            anthropic_betas(Some("kimi"), false, true, None),
            vec![CONTEXT_MANAGEMENT_BETA],
            "the Kimi trait replaces the interleaved beta"
        );
        assert_eq!(
            anthropic_betas(Some("anthropic"), false, false, None),
            vec![INTERLEAVED_THINKING_BETA]
        );
    }

    #[test]
    fn the_beta_list_is_a_body_field_only_on_the_beta_api() {
        let options = crate::llm::request_options::RequestOptions::default();
        let beta_body = request_with_options(
            "claude-sonnet-4-5",
            64_000,
            Some(4096),
            Some("all"),
            Some("anthropic"),
            true,
            &options,
        )
        .expect("valid request");
        assert_eq!(
            beta_body["betas"],
            json!([INTERLEAVED_THINKING_BETA, CONTEXT_MANAGEMENT_BETA])
        );

        let standard = request_with_options(
            "claude-sonnet-4-5",
            64_000,
            Some(4096),
            None,
            Some("anthropic"),
            false,
            &options,
        )
        .expect("valid request");
        assert!(
            standard.get("betas").is_none(),
            "the standard API carries betas as a header, not in the body"
        );
    }

    #[test]
    fn response_format_and_sampling_reach_the_body() {
        use crate::llm::request_options::{RequestOptions, ResponseFormat};

        let options = RequestOptions {
            response_format: Some(ResponseFormat::JsonSchema {
                name: "answer".into(),
                schema: json!({ "type": "object" }),
                strict: None,
                description: Some("the answer".into()),
            }),
            temperature: Some(0.3),
            top_p: Some(0.8),
            ..RequestOptions::default()
        };
        let req = request_with_options(
            "claude-sonnet-4-5",
            64_000,
            None,
            None,
            Some("anthropic"),
            false,
            &options,
        )
        .expect("valid request");
        assert_eq!(
            req["output_config"],
            json!({ "format": { "type": "json_schema", "schema": { "type": "object" } } })
        );
        assert_eq!(req["temperature"], 0.3);
        assert_eq!(req["top_p"], 0.8);

        let mut options = RequestOptions {
            temperature: Some(0.3),
            ..RequestOptions::default()
        };
        options
            .extra_params
            .anthropic
            .insert("temperature".into(), json!(0.9));
        options
            .extra_params
            .anthropic
            .insert("top_k".into(), json!(5));
        let req = request_with_options(
            "claude-sonnet-4-5",
            64_000,
            None,
            None,
            Some("anthropic"),
            false,
            &options,
        )
        .expect("valid request");
        assert_eq!(req["temperature"], 0.9, "extraParams wins");
        assert_eq!(req["top_k"], 5);
    }

    #[test]
    fn json_object_response_format_is_rejected_for_anthropic() {
        use crate::llm::request_options::{RequestOptions, ResponseFormat};

        let options = RequestOptions {
            response_format: Some(ResponseFormat::JsonObject),
            ..RequestOptions::default()
        };
        assert!(
            request_with_options(
                "claude-sonnet-4-5",
                64_000,
                None,
                None,
                Some("anthropic"),
                false,
                &options,
            )
            .is_err()
        );
    }
}
