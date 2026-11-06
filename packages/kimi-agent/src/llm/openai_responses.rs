//! OpenAI Responses API (`/v1/responses`) adapter.
//!
//! Handles OpenAI's structured responses endpoint with streaming SSE accumulation.

use serde_json::{Value, json};

use crate::llm::wire::{StreamDelta, WireMessage};
use crate::rpc::types::TokenUsage;
use crate::turn_loop::types::{ContentBlock, LLMChatResponse, ToolCall, ToolInfo};

/// v2 `OPENAI_RESPONSES_DEVELOPER_ROLE_MODELS`
/// (`openai-responses/lower.ts:104-116`): the exact set, plus a prefix match so
/// a dated alias (`o3-mini-2025-01-31`) keeps the role.
///
/// The set is v2's verbatim, and that includes its omission of the plain
/// `gpt-5` / `gpt-5-mini` / `gpt-5.1` ids (only `gpt-5-codex` is listed). Do not
/// "fix" that here: a member added on the Rust side alone has no v2
/// counterpart, and the wrong role is what a vendor rejects. Whether v2's set
/// should gain the gpt-5 family is an upstream decision.
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

/// The bare model id behind an OpenRouter-style `provider/model` alias.
///
/// The Responses body carries the model string verbatim, so a relay alias
/// reaches the wire — but the *role* is a property of the model family, not of
/// the relay that fronts it, so the prefix is stripped before matching (the
/// same one-line normalization `crate::llm::http::google_model_id` applies on
/// the Google URL path; it lives in the transport module, which the wire
/// adapters must not depend on).
fn bare_model_id(model: &str) -> &str {
    model.rsplit('/').next().unwrap_or(model)
}

/// Whether the model requires the `developer` role in place of `system` in the
/// Responses API.
///
/// A substring test (`contains("o1")`) misfires on any unrelated model whose id
/// happens to carry those characters, and a vendor rejects the resulting
/// `developer` role. v2 matches an explicit set plus a `<name>-` prefix — after
/// lowercasing, which is what keeps `openai/o3-mini` and `zenmux/o1-pro` on
/// the developer role instead of regressing them to `system`.
pub fn uses_developer_role(model: &str) -> bool {
    let normalized = bare_model_id(model).to_ascii_lowercase();
    if DEVELOPER_ROLE_MODELS.contains(&normalized.as_str()) {
        return true;
    }
    DEVELOPER_ROLE_MODELS
        .iter()
        .any(|known| normalized.starts_with(&format!("{known}-")))
}

/// Build an OpenAI `/v1/responses` request payload.
pub fn build_request_full(
    model: &str,
    messages: &[WireMessage],
    tools: &[ToolInfo],
    stream: bool,
    reasoning_effort: Option<&str>,
) -> Value {
    /// Rebuild v2 `reasoning` input items from the assistant message's think
    /// blocks (`openai-responses/lower.ts:176-203`). Consecutive parts that share
    /// an `encrypted` value coalesce into one item carrying several
    /// `summary_text` entries, exactly as v2 accumulates them.
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
            // v2 sends `part.think` verbatim; an empty part contributes no
            // summary entry, so an attestation-only part (the shape v2's
            // `output_item.done` handler produces,
            // `openai-responses/format.ts:557`) rides as an empty summary list
            // rather than as an empty string on the wire.
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
        // An item with neither a summary nor an encrypted payload is nothing
        // the server can act on — but one carrying only `encrypted_content` is
        // the whole point of the replay (v2 sends exactly that shape with an
        // empty `summary`), so it must not be dropped here.
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

    for m in messages {
        match m.role.as_str() {
            "system" => {
                let role = if dev_role { "developer" } else { "system" };
                input.push(json!({
                    "type": "message",
                    "role": role,
                    "content": m.content,
                }));
            }
            "assistant" => {
                // v2 `lowerMessage` (openai-responses/lower.ts:176-203):
                // reasoning is its own input item, emitted BEFORE the
                // text message, and consecutive think parts sharing an
                // `encrypted` value coalesce into one item with several
                // summaries. Dropping it loses the model's own prior
                // reasoning on every replay — and without
                // `encrypted_content` the Responses server cannot
                // restore reasoning continuity across turns.
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
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": m.content,
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
        // v2 sends `store: false` on this wire
        // (`kosong/src/providers/openai-responses.ts:1138`). Without it the
        // server keeps the response and expects continuity through
        // `previous_response_id`; this engine never sends one — it replays the
        // whole history itself — so the stored copy is both unused and a
        // retention the user did not ask for.
        "store": false,
    });

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

    // Same host-level tokens as the chat-completions body: `on` is the
    // boolean-model sentinel and has no wire meaning, while a declared
    // `none` / `minimal` / `xhigh` is a real effort the provider accepts.
    if let Some(effort) = crate::llm::effort::wire_reasoning_effort(reasoning_effort) {
        req["reasoning"] = json!({ "effort": effort });
        // `reasoning.encrypted_content` is returned ONLY when it is asked for
        // (v2 `normalizeOpenAIResponsesReasoning`,
        // `openai-responses/format.ts:346-359`), and it rides the same gate as
        // `reasoning` itself there. Without this the replayed item below would
        // carry an `encrypted` value the engine can never have been given.
        req["include"] = json!(["reasoning.encrypted_content"]);
    }

    req
}

/// [`build_request_full`] plus the session cache key and the per-request
/// completion cap. v2's Responses trait sends the key as `prompt_cache_key`
/// and the cap as `max_output_tokens` (`resolveMaxCompletionCap`).
pub fn build_request_with_budget(
    model: &str,
    messages: &[WireMessage],
    tools: &[ToolInfo],
    stream: bool,
    reasoning_effort: Option<&str>,
    cache_key: Option<&str>,
    max_output_tokens: Option<u32>,
) -> Value {
    let mut req = build_request_full(model, messages, tools, stream, reasoning_effort);
    if let Some(cache_key) = cache_key.filter(|key| !key.is_empty()) {
        req["prompt_cache_key"] = json!(cache_key);
    }
    if let Some(cap) = max_output_tokens {
        req["max_output_tokens"] = json!(cap.max(1));
    }
    req
}

/// Parse an OpenAI `/v1/responses` non-streaming response.
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
                    // v2 `parseToolCallArguments`: an absent or empty payload is
                    // a valid no-args call, and one that will not decode keeps
                    // its call with the reason attached — the loop answers it
                    // with an error result. Failing the whole response (what
                    // this did) turned one corrupt call into a dead turn the
                    // model could not correct.
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

    let input_cache_read = u
        .get("input_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|x| x.as_u64())
        .unwrap_or(0) as u32;

    let input_tokens = raw_input.saturating_sub(input_cache_read);

    let total_tokens = u
        .get("total_tokens")
        .and_then(|x| x.as_u64())
        .map(|t| t as u32)
        .unwrap_or_else(|| {
            // `input_tokens` is the uncached remainder; the provider total (or
            // this reconstruction) includes the cached prompt.
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

// ── Streaming (SSE) accumulator for /v1/responses ─────────────────────

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
    /// Terminal transport-level failure observed mid-stream (truncated
    /// function-call arguments, `response.failed`). The transport must turn
    /// this into an `Err`, never into a silently completed empty answer.
    error: Option<String>,
    /// Tool-call argument fragments the last [`Self::feed`] carried, drained by
    /// the caller through [`Self::take_tool_call_deltas`].
    pending_tool_calls: Vec<StreamDelta>,
    /// The `encrypted_content` of the reasoning item the stream closed with
    /// (v2 `openai-responses/format.ts:553-559`).
    ///
    /// It arrives on `response.output_item.done`, not on the summary deltas, so
    /// the accumulator dropped the item on that event and the Think block
    /// [`Self::finish`] built always carried `encrypted: None` — a state the
    /// request's `encrypted_content` replay could never be given.
    reasoning_encrypted: Option<String>,
}

impl StreamAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drain the tool-call argument fragments the last [`Self::feed`] carried,
    /// in arrival order.
    pub fn take_tool_call_deltas(&mut self) -> Vec<StreamDelta> {
        std::mem::take(&mut self.pending_tool_calls)
    }

    /// Feed an SSE event object from the responses stream.
    pub fn feed(&mut self, v: &Value) -> Option<StreamDelta> {
        let event_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");

        // v2 #4485 (`openai-responses/decode.ts:462-475`): usage is read on
        // every event, not only the terminal ones — an ordinary event
        // contributes the nested response's usage when it has one and its
        // own top-level usage otherwise; the two terminal events read the
        // nested usage only, so a stale top-level figure cannot overwrite
        // the terminal accounting.
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
                    // The call's id is announced before its arguments stream, so
                    // it is available here; without it the fragment could not be
                    // addressed to the tool-call entity.
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
                // v2 `openai-responses/format.ts:553-559` reads the closed
                // reasoning item's attestation here and nothing else; a
                // function_call item contributes only its final arguments,
                // which the flushed accumulator already holds. An empty string
                // is treated as no attestation rather than replayed as one.
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
                // Usage is read for every event at the top of `feed` (v2
                // #4485): terminal events take the nested figure, ordinary
                // events fall back to their own top-level usage.
            }
            "response.failed" => {
                let resp = v.get("response").unwrap_or(v);
                let err_msg = resp
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("response.failed");
                self.finish_reason = Some(format!("failed: {err_msg}"));
                // A failed response is a terminal transport failure, not an
                // empty answer: record it so the transport returns Err even
                // if a caller feeds the accumulator without the shared
                // in-band-error pre-check.
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
            // v2 `parseToolCallArguments`: a truncated argument stream keeps its
            // call with the reason attached, so the model reads an error result
            // for it. Failing the whole response (what this did) left the model
            // nothing to correct and turned one corrupt call into a dead turn.
            // An empty stream is a valid no-args call.
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

    /// Terminal failure observed while accumulating, if any. The transport
    /// must return it as `Err` instead of completing the turn silently.
    pub fn take_error(&mut self) -> Option<String> {
        self.error.take()
    }

    pub fn finish(mut self) -> LLMChatResponse {
        self.flush_current_tool_call();

        // Fail closed on a provider or stream failure: handing upward a partial
        // tool set — or an empty answer with a "failed" finish reason — is what
        // turned provider failures into silent empty completions. A tool call
        // whose arguments did not decode is not this case: it stays in the
        // response with its reason, and the loop answers that one call.
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
        // v2 emits a think part carrying only the attestation
        // (`openai-responses/format.ts:557`, `think: ''`), so the block
        // exists on the encrypted value alone — the same rule the Anthropic
        // accumulator applies to its signature (`anthropic.rs:624`).
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

    /// v2 `lowerMessage` rebuilds assistant reasoning as its own `reasoning`
    /// input item (openai-responses/lower.ts:176-203). Without it the
    /// model loses its prior reasoning on every replay, and without
    /// `encrypted_content` the server cannot restore continuity.
    /// v2 matches an explicit set plus a `<name>-` prefix. A substring test
    /// claims any model whose id merely contains "o1"/"o3"/"o4".
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
            // Dated / regional aliases reach the prefix arm.
            "o3-mini-2025-01-31",
            "O1",
            // OpenRouter-style `provider/model` aliases: the body carries the
            // prefix verbatim, the role belongs to the model behind it.
            "openai/o3-mini",
            "zenmux/o1-pro",
            "OpenAI/O4-Mini",
        ] {
            assert!(uses_developer_role(model), "{model} needs developer role");
        }

        // A relay model that merely carries those substrings does not.
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

    /// The replay is only reachable if the engine can actually obtain an
    /// `encrypted` value, so this drives the real stream path: the
    /// `response.output_item.done` event a reasoning turn ends with, the
    /// accumulator's Think block, and the request built from it. The
    /// `encrypted_content` the request asks for is asserted here too, because
    /// the field only comes back when it is included.
    #[test]
    fn assistant_thinking_is_replayed_as_a_reasoning_item() {
        let mut acc = StreamAccumulator::new();
        // Summary text streams as deltas ...
        acc.feed(&json!({
            "type": "response.reasoning_summary_text.delta",
            "delta": "first step"
        }));
        acc.feed(&json!({
            "type": "response.reasoning_summary_text.delta",
            "delta": "second step"
        }));
        // ... and the attestation only arrives with the closed item.
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
        // The field is only returned when the request asks for it.
        assert_eq!(req["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(req["store"], false);
        let input = req["input"].as_array().unwrap();

        // Reasoning precedes the text message.
        assert_eq!(input[0]["type"], "reasoning");
        assert_eq!(input[0]["encrypted_content"], "enc-1");
        let summary = input[0]["summary"].as_array().unwrap();
        // The accumulator holds one reasoning text buffer, so the two
        // `reasoning_summary_text.delta` events coalesce into a single
        // summary item rather than one item per delta.
        assert_eq!(summary.len(), 1, "the deltas coalesce into one item");
        assert_eq!(summary[0]["type"], "summary_text");
        assert_eq!(summary[0]["text"], "first stepsecond step");

        assert_eq!(input[1]["type"], "message");
        assert_eq!(input[1]["content"], "the answer is 4");
    }

    /// A turn whose reasoning carried no summary text at all — the shape v2
    /// produces from `response.output_item.done` alone
    /// (`openai-responses/format.ts:557`) — must still replay the attestation:
    /// that value is the only thing restoring the model's reasoning continuity.
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
        assert_eq!(req["input"].as_array().unwrap().len(), 2);
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

        // `on` is the boolean-model sentinel, not a wire value; `off` means
        // thinking is disabled. Neither belongs in `reasoning.effort`.
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

    /// v2 #4485's other half: an ordinary Responses event contributes its
    /// own top-level usage when the nested response carries none, while a
    /// terminal event reads the nested figure only — a stale top-level
    /// `usage` on the completed event must not overwrite the accounting.
    #[test]
    fn ordinary_events_fall_back_to_top_level_usage_terminal_events_do_not() {
        let mut acc = StreamAccumulator::new();
        // Ordinary delta with no nested response: the top-level usage counts.
        acc.feed(&json!({
            "type": "response.output_text.delta",
            "delta": "Hello",
            "usage": { "input_tokens": 7, "output_tokens": 1, "total_tokens": 8 }
        }));
        // Ordinary delta WITH a nested usage: the nested figure wins over
        // its own top-level one (v2 `responseUsage ?? event.usage`).
        acc.feed(&json!({
            "type": "response.output_text.delta",
            "delta": " world",
            "response": { "usage": { "input_tokens": 9, "output_tokens": 2, "total_tokens": 11 } },
            "usage": { "input_tokens": 1, "output_tokens": 1, "total_tokens": 2 }
        }));
        // A terminal event carrying only a top-level figure contributes
        // nothing: the ordinary events' accounting survives it.
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
        // Stream cut mid-arguments: `{"command": "rm -rf /tmp/x` never parses.
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
        ];
        let req_o3 = build_request_full("o3-mini", &msgs, &[], true, None);
        let input_o3 = req_o3["input"].as_array().unwrap();
        // o1/o3 model assertion: the system role must map to developer and
        // carry type: message
        assert_eq!(input_o3[0]["type"], "message");
        assert_eq!(input_o3[0]["role"], "developer");
        assert_eq!(input_o3[0]["content"], "You are an expert coder.");
        assert_eq!(input_o3[1]["type"], "message");
        assert_eq!(input_o3[1]["role"], "user");

        // An ordinary model keeps role: system
        let req_4o = build_request_full("gpt-4o", &msgs, &[], true, None);
        let input_4o = req_4o["input"].as_array().unwrap();
        assert_eq!(input_4o[0]["type"], "message");
        assert_eq!(input_4o[0]["role"], "system");
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
        // The key assertion: uncached input tokens must subtract the cached
        // portion (1000 - 800 = 200)
        assert_eq!(parsed.input_tokens, 200);
        assert_eq!(parsed.output_tokens, 50);
    }
}
