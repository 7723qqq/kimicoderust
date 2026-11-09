//! Model-family profiles used by the request builders (v2
//! `human/llm/modelFamily.ts`).
//!
//! A family is matched by model-name segments rather than by provider type:
//! the last segment is the model itself (`deepseek-chat`, `deepseek-reasoner`),
//! while a gateway in front of it shows up as an earlier segment
//! (`deepseek/deepseek-chat`). Gateways are matched only when they are not the
//! first segment — a host route named `deepseek/...` is the gateway, not a
//! model.

/// The output-token tiers a family declares for an unset completion cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FamilyMaxOutputDefaults {
    pub non_thinking: u32,
    pub thinking: u32,
    pub thinking_max_effort: u32,
}

/// The replay/sampling quirks of a family's thinking history.
#[derive(Debug, Clone, Copy)]
pub struct FamilyThinkingHistory {
    /// Replay hidden reasoning only when tools are in play (DeepSeek rejects
    /// the request otherwise).
    pub full_echo_with_tools: bool,
    /// Sampling fields the provider rejects while reasoning is on.
    pub ignored_while_thinking: &'static [&'static str],
}

/// A model-family profile. Only the fields the request builders consume are
/// modelled; the adapter-side ones (image pricing, adaptation prefixes) stay
/// with the host.
#[derive(Debug, Clone, Copy)]
pub struct ModelFamilyProfile {
    pub id: &'static str,
    pub prefixes: &'static [ModelFamilyPrefix],
    pub thinking_history: Option<FamilyThinkingHistory>,
    pub max_output_defaults: Option<FamilyMaxOutputDefaults>,
}

#[derive(Debug, Clone, Copy)]
pub struct ModelFamilyPrefix {
    pub value: &'static str,
    /// Match a prefix that is glued to the rest of the segment
    /// (`xopdeepseek-chat`), not only one followed by `-` or `.`.
    pub glued: bool,
}

const CLONE_SUFFIX: &str = "-clone";

const DEEPSEEK_PREFIXES: &[ModelFamilyPrefix] = &[
    ModelFamilyPrefix {
        value: "deepseek",
        glued: false,
    },
    ModelFamilyPrefix {
        value: "xopdeepseek",
        glued: true,
    },
];

const DEEPSEEK_MAX_OUTPUT_DEFAULTS: FamilyMaxOutputDefaults = FamilyMaxOutputDefaults {
    non_thinking: 8192,
    thinking: 65536,
    thinking_max_effort: 131072,
};

const DEEPSEEK_THINKING_HISTORY: FamilyThinkingHistory = FamilyThinkingHistory {
    full_echo_with_tools: true,
    ignored_while_thinking: &["temperature", "presence_penalty", "frequency_penalty"],
};

const MODEL_FAMILIES: &[ModelFamilyProfile] = &[ModelFamilyProfile {
    id: "deepseek",
    prefixes: DEEPSEEK_PREFIXES,
    thinking_history: Some(DEEPSEEK_THINKING_HISTORY),
    max_output_defaults: Some(DEEPSEEK_MAX_OUTPUT_DEFAULTS),
}];

fn model_name_segments(name: &str) -> Vec<String> {
    name.to_ascii_lowercase()
        .split('/')
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .map(str::to_string)
        .collect()
}

fn model_segment_matches(segment: &str, prefix: ModelFamilyPrefix) -> bool {
    if !segment.starts_with(prefix.value) {
        return false;
    }
    if segment.ends_with(CLONE_SUFFIX) {
        return false;
    }
    let rest = &segment[prefix.value.len()..];
    if rest.is_empty() || rest.starts_with('-') || rest.starts_with('.') {
        return true;
    }
    prefix.glued
}

fn gateway_segment_matches(segment: &str, prefix: &str) -> bool {
    segment.starts_with(prefix)
        && segment[prefix.len()..].starts_with('-')
        && !segment.ends_with(CLONE_SUFFIX)
}

fn segment_matches_family(
    segment: &str,
    index: usize,
    count: usize,
    prefix: ModelFamilyPrefix,
) -> bool {
    if index == count - 1 {
        return model_segment_matches(segment, prefix);
    }
    if index == 0 {
        return false;
    }
    gateway_segment_matches(segment, prefix.value)
}

/// Resolve the family profile for `name` (v2 `resolveModelFamily`). The longest
/// matching prefix wins.
pub fn resolve_model_family(name: &str) -> Option<&'static ModelFamilyProfile> {
    let segments = model_name_segments(name);
    if segments.is_empty() {
        return None;
    }
    let mut resolved: Option<&'static ModelFamilyProfile> = None;
    let mut resolved_length = 0;
    for family in MODEL_FAMILIES {
        for prefix in family.prefixes {
            let hit = segments.iter().enumerate().any(|(index, segment)| {
                segment_matches_family(segment, index, segments.len(), *prefix)
            });
            if hit && prefix.value.len() > resolved_length {
                resolved = Some(family);
                resolved_length = prefix.value.len();
            }
        }
    }
    resolved
}

/// The family's unset-completion-cap tiers (v2 `maxOutputDefaultsForModel`).
pub fn max_output_defaults_for_model(name: &str) -> Option<&'static FamilyMaxOutputDefaults> {
    resolve_model_family(name).and_then(|family| family.max_output_defaults.as_ref())
}

/// Whether hidden reasoning must be echoed back (v2
/// `fullReasoningEchoRequired`): only DeepSeek, and only while tools are in
/// play.
pub fn full_reasoning_echo_required(name: &str, has_tools: bool) -> bool {
    if !has_tools {
        return false;
    }
    resolve_model_family(name)
        .and_then(|family| family.thinking_history)
        .is_some_and(|history| history.full_echo_with_tools)
}

/// Sampling fields the family rejects while reasoning is on (v2
/// `ignoredSamplingParamsForModel`).
pub fn ignored_sampling_params_for_model(name: &str) -> &'static [&'static str] {
    resolve_model_family(name)
        .and_then(|family| family.thinking_history)
        .map_or(&[], |history| history.ignored_while_thinking)
}

/// The family's default completion cap (v2 `familyDefaultCap`).
///
/// `remaining` is the model window minus the estimated prompt tokens. When the
/// caller has no estimate the tier itself is the cap; a fully consumed window
/// yields no cap at all.
pub fn family_default_completion_cap(
    model: &str,
    effort: Option<&str>,
    remaining: Option<u32>,
) -> Option<u32> {
    let defaults = max_output_defaults_for_model(model)?;
    let tier = tier_for_effort(defaults, effort);
    match remaining {
        None => Some(tier),
        Some(0) => None,
        Some(remaining) => Some(tier.min(remaining)),
    }
}

/// v2 `outputTierForEffort`: `off` is the non-thinking tier, `max` the
/// max-effort tier, and every other level (including an unresolved one) the
/// thinking tier.
fn tier_for_effort(defaults: &FamilyMaxOutputDefaults, effort: Option<&str>) -> u32 {
    match effort {
        Some(effort)
            if effort.eq_ignore_ascii_case("off") || effort.eq_ignore_ascii_case("none") =>
        {
            defaults.non_thinking
        }
        Some(effort) if effort.eq_ignore_ascii_case("max") => defaults.thinking_max_effort,
        _ => defaults.thinking,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_deepseek_models_and_gateways() {
        for name in [
            "deepseek-chat",
            "deepseek-reasoner",
            "deepseek/deepseek-chat",
            "openrouter/deepseek/deepseek-reasoner",
            "xopdeepseek-chat",
            "xopdeepseek/deepseek-chat",
        ] {
            assert_eq!(
                resolve_model_family(name).map(|family| family.id),
                Some("deepseek"),
                "{name} must resolve to the deepseek family"
            );
        }
    }

    #[test]
    fn does_not_resolve_unrelated_or_clone_models() {
        for name in [
            "gpt-5",
            "claude-sonnet-4",
            "kimi-k2",
            "deepseek-clone",
            "deepseek/deepseek-chat-clone",
            "other/model",
        ] {
            assert!(
                resolve_model_family(name).is_none(),
                "{name} must not resolve to a family"
            );
        }
    }

    #[test]
    fn max_output_defaults_match_upstream_tiers() {
        let defaults = max_output_defaults_for_model("deepseek-chat").expect("deepseek defaults");
        assert_eq!(defaults.non_thinking, 8192);
        assert_eq!(defaults.thinking, 65536);
        assert_eq!(defaults.thinking_max_effort, 131072);
    }

    #[test]
    fn family_default_cap_selects_the_effort_tier() {
        assert_eq!(
            family_default_completion_cap("deepseek-chat", Some("off"), None),
            Some(8192)
        );
        assert_eq!(
            family_default_completion_cap("deepseek-chat", Some("high"), None),
            Some(65536)
        );
        assert_eq!(
            family_default_completion_cap("deepseek-chat", Some("max"), None),
            Some(131072)
        );
        // Unresolved effort is the thinking tier.
        assert_eq!(
            family_default_completion_cap("deepseek-chat", None, None),
            Some(65536)
        );
    }

    #[test]
    fn family_default_cap_is_bounded_by_the_remaining_window() {
        assert_eq!(
            family_default_completion_cap("deepseek-chat", Some("max"), Some(4096)),
            Some(4096)
        );
        assert_eq!(
            family_default_completion_cap("deepseek-chat", Some("max"), Some(0)),
            None
        );
    }

    #[test]
    fn non_family_models_have_no_default_cap() {
        assert_eq!(
            family_default_completion_cap("gpt-5", Some("high"), None),
            None
        );
    }

    #[test]
    fn full_echo_requires_deepseek_and_tools() {
        assert!(full_reasoning_echo_required("deepseek-chat", true));
        assert!(!full_reasoning_echo_required("deepseek-chat", false));
        assert!(!full_reasoning_echo_required("gpt-5", true));
    }

    #[test]
    fn deepseek_ignores_sampling_while_thinking() {
        assert_eq!(
            ignored_sampling_params_for_model("deepseek-chat"),
            &["temperature", "presence_penalty", "frequency_penalty"]
        );
        assert!(ignored_sampling_params_for_model("gpt-5").is_empty());
    }
}
