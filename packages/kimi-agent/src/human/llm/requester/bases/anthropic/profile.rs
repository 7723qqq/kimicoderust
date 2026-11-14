use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value, json};

use crate::human::llm::model::LlmModel;
use crate::human::llm::thinking::{
    ThinkingConfigError, ThinkingConfigErrorCode, ThinkingEffort, ThinkingRequestOptions,
    thinking_metadata_of,
};

pub const INTERLEAVED_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";
pub const BUDGET_THINKING_EFFORTS: [&str; 3] = ["low", "medium", "high"];
pub const LATEST_OPUS_THINKING_EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
const ADAPTIVE_MAX_EFFORTS: [&str; 4] = ["low", "medium", "high", "max"];
const FALLBACK_MAX_TOKENS: u64 = 64000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnthropicThinkingMode {
    Budget,
    Adaptive,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnthropicModelProfile {
    pub mode: AnthropicThinkingMode,
    pub efforts: Vec<String>,
    pub supports_effort_param: bool,
    pub can_disable_thinking: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnthropicModelFamily {
    Opus,
    Sonnet,
    Haiku,
    Fable,
    Mythos,
}

impl AnthropicModelFamily {
    fn parse(value: &str) -> Self {
        match value {
            "opus" => Self::Opus,
            "sonnet" => Self::Sonnet,
            "haiku" => Self::Haiku,
            "fable" => Self::Fable,
            "mythos" => Self::Mythos,
            _ => unreachable!(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnthropicModelVersion {
    pub family: AnthropicModelFamily,
    pub major: i32,
    pub minor: Option<i32>,
}

fn efforts(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

fn budget_profile() -> AnthropicModelProfile {
    AnthropicModelProfile {
        mode: AnthropicThinkingMode::Budget,
        efforts: efforts(&BUDGET_THINKING_EFFORTS),
        supports_effort_param: false,
        can_disable_thinking: true,
    }
}

fn opus_45_profile() -> AnthropicModelProfile {
    AnthropicModelProfile {
        supports_effort_param: true,
        ..budget_profile()
    }
}

fn adaptive_max_profile() -> AnthropicModelProfile {
    AnthropicModelProfile {
        mode: AnthropicThinkingMode::Adaptive,
        efforts: efforts(&ADAPTIVE_MAX_EFFORTS),
        supports_effort_param: true,
        can_disable_thinking: true,
    }
}

pub static LATEST_OPUS_PROFILE: LazyLock<AnthropicModelProfile> = LazyLock::new(|| {
    AnthropicModelProfile {
        mode: AnthropicThinkingMode::Adaptive,
        efforts: efforts(&LATEST_OPUS_THINKING_EFFORTS),
        supports_effort_param: true,
        can_disable_thinking: true,
    }
});

fn always_adaptive_profile() -> AnthropicModelProfile {
    AnthropicModelProfile {
        can_disable_thinking: false,
        ..LATEST_OPUS_PROFILE.clone()
    }
}

fn always_adaptive_max_profile() -> AnthropicModelProfile {
    AnthropicModelProfile {
        can_disable_thinking: false,
        ..adaptive_max_profile()
    }
}

static FAMILY_FIRST_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(opus|sonnet|haiku|fable|mythos)[-._](\d{1,2})(?:[-._](\d{1,2}))?([^0-9]|$)")
        .unwrap()
});
static VERSION_FIRST_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(\d{1,2})[-._](\d{1,2})[-._](opus|sonnet|haiku)").unwrap()
});
static BARE_FAMILY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(\d{1,2})[-._](opus|sonnet|haiku)").unwrap());
static MYTHOS_PREVIEW_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"mythos[-._]preview").unwrap());
static CLAUDE_FAMILY_WORD_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(?:opus|sonnet|haiku|fable|mythos)\b").unwrap());

pub fn parse_anthropic_model_version(
    model: &str,
    require_claude_marker: bool,
) -> Option<AnthropicModelVersion> {
    let normalized = model.to_lowercase();
    if require_claude_marker && !normalized.contains("claude") {
        return None;
    }
    if let Some(captures) = FAMILY_FIRST_RE.captures(&normalized) {
        return Some(AnthropicModelVersion {
            family: AnthropicModelFamily::parse(captures.get(1)?.as_str()),
            major: captures.get(2)?.as_str().parse().ok()?,
            minor: captures
                .get(3)
                .and_then(|value| value.as_str().parse::<i32>().ok()),
        });
    }
    if let Some(captures) = VERSION_FIRST_RE.captures(&normalized) {
        return Some(AnthropicModelVersion {
            major: captures.get(1)?.as_str().parse().ok()?,
            minor: Some(captures.get(2)?.as_str().parse().ok()?),
            family: AnthropicModelFamily::parse(captures.get(3)?.as_str()),
        });
    }
    if let Some(captures) = BARE_FAMILY_RE.captures(&normalized) {
        return Some(AnthropicModelVersion {
            major: captures.get(1)?.as_str().parse().ok()?,
            minor: None,
            family: AnthropicModelFamily::parse(captures.get(2)?.as_str()),
        });
    }
    None
}

fn ceiling_by_family_version(
    family: AnthropicModelFamily,
    major: i32,
    minor: Option<i32>,
) -> Option<u64> {
    use AnthropicModelFamily::{Fable, Haiku, Mythos, Opus, Sonnet};
    match (family, major, minor) {
        (Fable | Mythos | Sonnet, 5, _) => Some(128000),
        (Opus, 4, Some(6..=8)) => Some(128000),
        (Opus, 4, Some(5)) => Some(64000),
        (Opus, 4, Some(0..=1)) | (Opus, 4, None) => Some(32000),
        (Sonnet, 4, Some(6)) => Some(128000),
        (Sonnet, 4, Some(0..=5)) | (Sonnet, 4, None) => Some(64000),
        (Haiku, 4, Some(0..=5)) | (Haiku, 4, None) => Some(64000),
        (Opus | Sonnet | Haiku, 3, Some(5 | 7)) => Some(8192),
        (Opus | Sonnet | Haiku, 3, _) => Some(4096),
        _ => None,
    }
}

fn lookup_claude_ceiling(version: AnthropicModelVersion) -> Option<u64> {
    if let Some(minor) = version.minor {
        for candidate in (0..=minor).rev() {
            if let Some(ceiling) =
                ceiling_by_family_version(version.family, version.major, Some(candidate))
            {
                return Some(ceiling);
            }
        }
    }
    ceiling_by_family_version(version.family, version.major, None)
}

pub fn resolve_default_max_tokens(model: &str) -> u64 {
    resolve_default_max_tokens_with_override(model, None)
}

pub fn resolve_default_max_tokens_with_override(model: &str, override_: Option<u64>) -> u64 {
    let ceiling = parse_anthropic_model_version(model, true).and_then(lookup_claude_ceiling);
    match ceiling {
        None => override_.unwrap_or(FALLBACK_MAX_TOKENS),
        Some(ceiling) => override_.map_or(ceiling, |override_| override_.min(ceiling)),
    }
}

pub fn match_known_anthropic_model_profile(model: &str) -> Option<AnthropicModelProfile> {
    let normalized = model.to_lowercase();
    if MYTHOS_PREVIEW_RE.is_match(&normalized) {
        return Some(always_adaptive_max_profile());
    }
    let version = parse_anthropic_model_version(model, false)?;
    match version.family {
        AnthropicModelFamily::Opus => {
            if version.major == 4 && matches!(version.minor, Some(7 | 8)) {
                return Some(LATEST_OPUS_PROFILE.clone());
            }
            if version.major == 4 && version.minor == Some(6) {
                return Some(adaptive_max_profile());
            }
            if version.major == 4 && version.minor == Some(5) {
                return Some(opus_45_profile());
            }
            if version.major < 4 || (version.major == 4 && version.minor.unwrap_or(0) < 5) {
                return Some(budget_profile());
            }
            None
        }
        AnthropicModelFamily::Sonnet => {
            if version.major == 5 {
                return Some(LATEST_OPUS_PROFILE.clone());
            }
            if version.major == 4 && version.minor == Some(6) {
                return Some(adaptive_max_profile());
            }
            if version.major < 4 || (version.major == 4 && version.minor.unwrap_or(0) <= 5) {
                return Some(budget_profile());
            }
            None
        }
        AnthropicModelFamily::Haiku => {
            if version.major < 4 || (version.major == 4 && version.minor.unwrap_or(0) <= 5) {
                return Some(budget_profile());
            }
            None
        }
        AnthropicModelFamily::Fable | AnthropicModelFamily::Mythos => {
            (version.major == 5).then(always_adaptive_profile)
        }
    }
}

pub fn infer_anthropic_model_profile(model: &str) -> AnthropicModelProfile {
    match_known_anthropic_model_profile(model).unwrap_or_else(|| LATEST_OPUS_PROFILE.clone())
}

pub fn match_unknown_claude_profile(model: &str) -> Option<AnthropicModelProfile> {
    let normalized = model.to_lowercase();
    (normalized.contains("claude") || CLAUDE_FAMILY_WORD_RE.is_match(&normalized))
        .then(|| LATEST_OPUS_PROFILE.clone())
}

pub fn should_preserve_unsigned_thinking(model: &str) -> bool {
    parse_anthropic_model_version(model, false).is_none()
        && match_known_anthropic_model_profile(model).is_none()
}

fn requires_adaptive_thinking(efforts: &[String]) -> bool {
    efforts
        .iter()
        .any(|effort| !["low", "medium", "high"].contains(&effort.as_str()))
}

pub fn resolve_thinking_profile(model: &LlmModel) -> AnthropicModelProfile {
    let inferred = infer_anthropic_model_profile(&model.model);
    let meta = thinking_metadata_of(model);
    let support_efforts = meta.as_ref().and_then(|meta| meta.support_efforts.clone());
    let adaptive_thinking = meta.as_ref().and_then(|meta| meta.adaptive_thinking);
    if adaptive_thinking == Some(false) {
        return AnthropicModelProfile {
            mode: AnthropicThinkingMode::Budget,
            efforts: support_efforts.unwrap_or_else(|| efforts(&BUDGET_THINKING_EFFORTS)),
            supports_effort_param: false,
            ..inferred
        };
    }
    if adaptive_thinking == Some(true) {
        return AnthropicModelProfile {
            mode: AnthropicThinkingMode::Adaptive,
            efforts: support_efforts.unwrap_or_else(|| inferred.efforts.clone()),
            supports_effort_param: true,
            ..inferred
        };
    }
    let Some(support_efforts) = support_efforts else {
        return inferred;
    };
    let adaptive = requires_adaptive_thinking(&support_efforts);
    AnthropicModelProfile {
        mode: if adaptive {
            AnthropicThinkingMode::Adaptive
        } else {
            inferred.mode
        },
        efforts: support_efforts,
        supports_effort_param: adaptive || inferred.supports_effort_param,
        ..inferred
    }
}

fn budget_tokens_for_effort(effort: &ThinkingEffort) -> Option<u64> {
    match effort.as_str() {
        "low" => Some(1024),
        "medium" => Some(4096),
        "on" | "high" => Some(32000),
        _ => None,
    }
}

pub fn encode_thinking(
    thinking: &ThinkingRequestOptions,
    model: &LlmModel,
) -> Option<Map<String, Value>> {
    let profile = resolve_thinking_profile(model);
    let effort = &thinking.effort;
    if effort == "off" {
        if !profile.can_disable_thinking {
            let error = ThinkingConfigError::new(
                ThinkingConfigErrorCode::ThinkingCannotDisable,
                format!(
                    "Model '{}' always reasons and thinking cannot be turned off. Choose a concrete thinking effort ({}) instead of 'off'.",
                    model.model,
                    profile.efforts.join(", ")
                ),
            );
            std::panic::panic_any(error);
        }
        let mut patch = Map::from_iter([(
            "thinking".to_string(),
            json!({ "type": "disabled" }),
        )]);
        if profile.mode == AnthropicThinkingMode::Adaptive {
            patch.insert("betaFeatures".to_string(), json!([]));
        }
        return Some(patch);
    }
    if profile.mode == AnthropicThinkingMode::Adaptive {
        let mut patch = Map::from_iter([
            (
                "thinking".to_string(),
                json!({ "type": "adaptive", "display": "summarized" }),
            ),
            ("betaFeatures".to_string(), json!([])),
        ]);
        if effort != "on" {
            patch.insert("output_config".to_string(), json!({ "effort": effort }));
        }
        return Some(patch);
    }
    let budget_tokens = budget_tokens_for_effort(effort);
    let thinking_value = match budget_tokens {
        Some(budget_tokens) => json!({ "type": "enabled", "budget_tokens": budget_tokens }),
        None => json!({ "type": "enabled" }),
    };
    let mut patch = Map::from_iter([("thinking".to_string(), thinking_value)]);
    if (profile.supports_effort_param || budget_tokens.is_none()) && effort != "on" {
        patch.insert("output_config".to_string(), json!({ "effort": effort }));
    }
    Some(patch)
}
