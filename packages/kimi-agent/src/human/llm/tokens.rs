use std::collections::HashMap;
use std::sync::LazyLock;

use parking_lot::Mutex;

use crate::human::llm::message::{ContentPart, ToolCall, ToolDescription};
use crate::human::llm::model_family::ImageTokenPricing;

pub const MEDIA_TOKEN_ESTIMATE: u64 = 2000;

static TOOL_TEXT_TOKEN_CACHE: LazyLock<Mutex<HashMap<String, u64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn image_tokens_for(
    pricing: &ImageTokenPricing,
    dimensions: Option<(u32, u32)>,
) -> u64 {
    let Some((width, height)) = dimensions else {
        return pricing.fallback_tokens as u64;
    };
    let mut width = width as f64;
    let mut height = height as f64;
    if !(width > 0.0) || !(height > 0.0) {
        return pricing.fallback_tokens as u64;
    }
    let cell = (pricing.patch_px * pricing.downsample_ratio) as f64;
    let short_edge = width.min(height);
    if short_edge < pricing.scale_up_floor_px as f64 {
        let scale = pricing.scale_up_floor_px as f64 / short_edge;
        width *= scale;
        height *= scale;
    }
    let mut cols = ((width / cell).ceil() as u32).max(1);
    let mut rows = ((height / cell).ceil() as u32).max(1);
    for _ in 0..8 {
        if cols * rows <= pricing.token_cap {
            break;
        }
        let scale = (pricing.token_cap as f64 / (cols * rows) as f64).sqrt();
        width *= scale;
        height *= scale;
        cols = ((width / cell).ceil() as u32).max(1);
        rows = ((height / cell).ceil() as u32).max(1);
    }
    (cols * rows).min(pricing.token_cap) as u64
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenCharCounts {
    pub ascii: u64,
    pub non_ascii: u64,
}

pub fn count_token_chars(text: &str) -> TokenCharCounts {
    let mut counts = TokenCharCounts::default();
    for character in text.chars() {
        if character as u32 <= 127 {
            counts.ascii += 1;
        } else {
            counts.non_ascii += 1;
        }
    }
    counts
}

pub fn tokens_for_char_counts(counts: TokenCharCounts) -> u64 {
    counts.ascii.div_ceil(4) + counts.non_ascii
}

pub fn estimate_tokens(text: &str) -> u64 {
    tokens_for_char_counts(count_token_chars(text))
}

#[derive(Debug, Clone, PartialEq)]
pub struct EstimableMessage {
    pub role: String,
    pub content: Vec<ContentPart>,
    pub tool_calls: Option<Vec<ToolCall>>,
}

impl From<&crate::llm_adapter::contract::message::Message> for EstimableMessage {
    fn from(message: &crate::llm_adapter::contract::message::Message) -> Self {
        Self {
            role: message.role.as_str().to_string(),
            content: message.content.clone(),
            tool_calls: (!message.tool_calls.is_empty()).then(|| message.tool_calls.clone()),
        }
    }
}

pub fn estimate_tokens_for_messages(
    messages: &[EstimableMessage],
    pricing: Option<&ImageTokenPricing>,
) -> u64 {
    messages
        .iter()
        .map(|message| estimate_tokens_for_message(message, pricing))
        .sum()
}

fn estimate_tool_text_tokens(text: &str) -> u64 {
    if let Some(cached) = TOOL_TEXT_TOKEN_CACHE.lock().get(text).copied() {
        return cached;
    }
    let estimate = estimate_tokens(text);
    TOOL_TEXT_TOKEN_CACHE
        .lock()
        .insert(text.to_string(), estimate);
    estimate
}

pub fn estimate_tokens_for_tools(tools: &[ToolDescription]) -> u64 {
    let mut total = 0;
    for tool in tools {
        total += estimate_tool_text_tokens(&tool.name);
        total += estimate_tool_text_tokens(&tool.description);
        total += estimate_tokens(&serde_json::to_string(&tool.parameters).unwrap());
    }
    total
}

pub fn estimate_tokens_for_message(
    message: &EstimableMessage,
    pricing: Option<&ImageTokenPricing>,
) -> u64 {
    let mut total = estimate_tokens(&message.role);
    total += estimate_tokens_for_content_parts(&message.content, pricing);
    if let Some(tool_calls) = &message.tool_calls {
        for call in tool_calls {
            total += estimate_tokens(&call.name);
            total += estimate_tokens(&serde_json::to_string(&call.arguments).unwrap());
        }
    }
    total
}

pub fn estimate_tokens_for_content_parts(
    parts: &[ContentPart],
    pricing: Option<&ImageTokenPricing>,
) -> u64 {
    parts
        .iter()
        .map(|part| estimate_tokens_for_content_part(part, pricing))
        .sum()
}

pub fn estimate_tokens_for_content_part(
    part: &ContentPart,
    pricing: Option<&ImageTokenPricing>,
) -> u64 {
    match part {
        ContentPart::Text { text, .. } => estimate_tokens(text),
        ContentPart::Think { think, .. } => estimate_tokens(think),
        ContentPart::ImageUrl { image_url, .. } => match pricing {
            Some(pricing) => image_tokens_for(
                pricing,
                image_url
                    .dimensions
                    .as_ref()
                    .map(|dimensions| (dimensions.width, dimensions.height)),
            ),
            None => MEDIA_TOKEN_ESTIMATE,
        },
        ContentPart::AudioUrl { .. }
        | ContentPart::VideoUrl { .. }
        | ContentPart::DocumentUrl { .. } => MEDIA_TOKEN_ESTIMATE,
    }
}
