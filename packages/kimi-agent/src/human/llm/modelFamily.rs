use std::sync::LazyLock;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageTokenPricing {
    pub patch_px: u32,
    pub downsample_ratio: u32,
    pub scale_up_floor_px: u32,
    pub token_cap: u32,
    pub fallback_tokens: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelFamilyPrefix {
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glued: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FamilyThinkingHistory {
    pub full_echo_with_tools: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignored_while_thinking: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FamilyReasoningEffort {
    pub history_default: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FamilyMaxOutputDefaults {
    pub non_thinking: u32,
    pub thinking: u32,
    pub thinking_max_effort: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelFamilyProfile {
    pub id: String,
    pub prefixes: Vec<ModelFamilyPrefix>,
    pub adaptation_prefixes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub curated_adaptation: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_pricing: Option<ImageTokenPricing>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<FamilyReasoningEffort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_history: Option<FamilyThinkingHistory>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_defaults: Option<FamilyMaxOutputDefaults>,
}

pub static MODEL_FAMILIES: LazyLock<Vec<ModelFamilyProfile>> = LazyLock::new(|| {
    vec![ModelFamilyProfile {
        id: "deepseek".to_string(),
        prefixes: vec![
            ModelFamilyPrefix {
                value: "deepseek".to_string(),
                glued: None,
            },
            ModelFamilyPrefix {
                value: "xopdeepseek".to_string(),
                glued: Some(true),
            },
        ],
        adaptation_prefixes: vec!["deepseek".to_string()],
        curated_adaptation: Some(true),
        image_pricing: Some(ImageTokenPricing {
            patch_px: 14,
            downsample_ratio: 3,
            scale_up_floor_px: 544,
            token_cap: 1024,
            fallback_tokens: 1024,
        }),
        reasoning_effort: Some(FamilyReasoningEffort {
            history_default: "high".to_string(),
        }),
        thinking_history: Some(FamilyThinkingHistory {
            full_echo_with_tools: true,
            ignored_while_thinking: Some(vec![
                "temperature".to_string(),
                "presence_penalty".to_string(),
                "frequency_penalty".to_string(),
            ]),
        }),
        max_output_defaults: Some(FamilyMaxOutputDefaults {
            non_thinking: 8192,
            thinking: 65536,
            thinking_max_effort: 131072,
        }),
    }]
});

const CLONE_SUFFIX: &str = "-clone";

fn model_name_segments(name: &str) -> Vec<String> {
    name.to_lowercase()
        .split('/')
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .map(str::to_string)
        .collect()
}

fn model_segment_matches(segment: &str, prefix: &ModelFamilyPrefix) -> bool {
    if !segment.starts_with(&prefix.value) || segment.ends_with(CLONE_SUFFIX) {
        return false;
    }
    let rest = &segment[prefix.value.len()..];
    if rest.is_empty() || rest.starts_with('-') || rest.starts_with('.') {
        return true;
    }
    prefix.glued == Some(true)
}

fn gateway_segment_matches(segment: &str, prefix: &str) -> bool {
    segment.starts_with(&format!("{prefix}-")) && !segment.ends_with(CLONE_SUFFIX)
}

fn segment_matches_family(
    segment: &str,
    index: usize,
    count: usize,
    prefix: &ModelFamilyPrefix,
) -> bool {
    if index == count - 1 {
        return model_segment_matches(segment, prefix);
    }
    if index == 0 {
        return false;
    }
    gateway_segment_matches(segment, &prefix.value)
}

pub fn resolve_model_family(name: &str) -> Option<&'static ModelFamilyProfile> {
    let segments = model_name_segments(name);
    if segments.is_empty() {
        return None;
    }
    let mut resolved = None;
    let mut resolved_length = 0usize;
    for family in MODEL_FAMILIES.iter() {
        for prefix in &family.prefixes {
            let hit = segments.iter().enumerate().any(|(index, segment)| {
                segment_matches_family(segment, index, segments.len(), prefix)
            });
            if hit && prefix.value.len() > resolved_length {
                resolved = Some(family);
                resolved_length = prefix.value.len();
            }
        }
    }
    resolved
}

pub fn family_adaptation_prefixes(name: &str) -> &'static [String] {
    resolve_model_family(name)
        .map(|family| family.adaptation_prefixes.as_slice())
        .unwrap_or(&[])
}

pub fn image_pricing_for_model(name: Option<&str>) -> Option<&'static ImageTokenPricing> {
    resolve_model_family(name?)?.image_pricing.as_ref()
}

pub fn max_output_defaults_for_model(
    name: Option<&str>,
) -> Option<&'static FamilyMaxOutputDefaults> {
    resolve_model_family(name?)?.max_output_defaults.as_ref()
}

pub fn reasoning_effort_for_model(name: Option<&str>) -> Option<&'static FamilyReasoningEffort> {
    resolve_model_family(name?)?.reasoning_effort.as_ref()
}

pub fn thinking_history_for_model(name: Option<&str>) -> Option<&'static FamilyThinkingHistory> {
    resolve_model_family(name?)?.thinking_history.as_ref()
}

pub fn full_reasoning_echo_required(name: Option<&str>, has_tools: bool) -> bool {
    if name.is_none() || !has_tools {
        return false;
    }
    thinking_history_for_model(name).is_some_and(|history| history.full_echo_with_tools)
}

pub fn ignored_sampling_params_for_model(name: Option<&str>) -> &'static [String] {
    thinking_history_for_model(name)
        .and_then(|history| history.ignored_while_thinking.as_deref())
        .unwrap_or(&[])
}
