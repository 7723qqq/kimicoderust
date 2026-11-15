use std::sync::LazyLock;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::human::llm::media::image_compression_caption::extract_image_compression_captions;
use crate::human::llm::media::path_tag::match_single_media_path_tag;
use crate::human::llm::message::{ContentPart, Role, ToolCall};

pub const SKILL_ACTIVATION_PART_SOURCE: &str = "skill activation";

pub fn skill_activation_part(text: &str, activation_id: &str) -> ContentPart {
    ContentPart::Text {
        text: text.to_string(),
        meta: Some(Map::from_iter([
            (
                "source".to_string(),
                Value::String(SKILL_ACTIVATION_PART_SOURCE.to_string()),
            ),
            (
                "activationId".to_string(),
                Value::String(activation_id.to_string()),
            ),
        ])),
    }
}

pub fn is_skill_activation_part(part: &ContentPart) -> bool {
    matches!(
        part,
        ContentPart::Text { meta: Some(meta), .. }
            if meta.get("source").and_then(Value::as_str) == Some(SKILL_ACTIVATION_PART_SOURCE)
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BundledSkillActivation {
    pub activation_id: String,
    pub skill_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_args: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_source: Option<SkillSource>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillSource {
    Project,
    User,
    Extra,
    Builtin,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptFileAttachment {
    pub name: String,
    pub media_type: String,
    pub size: u64,
    pub path: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptOrigin {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_turn: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_metadata: Option<Vec<Map<String, Value>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_activations: Option<Vec<BundledSkillActivation>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Vec<PromptFileAttachment>>,
    #[serde(default, flatten)]
    pub extra: Map<String, Value>,
}

pub type UserPromptOrigin = PromptOrigin;

pub static USER_PROMPT_ORIGIN: LazyLock<UserPromptOrigin> = LazyLock::new(|| PromptOrigin {
    kind: "user".to_string(),
    ..Default::default()
});

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SteerMessage {
    pub content: Vec<ContentPart>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<PromptOrigin>,
}

pub fn annotate_bundled_skill_parts(
    content: &[ContentPart],
    bundled_activations: &[BundledSkillActivation],
) -> Vec<ContentPart> {
    if bundled_activations.is_empty() || content.iter().any(is_skill_activation_part) {
        return content.to_vec();
    }
    let mut index = 0;
    content
        .iter()
        .map(|part| {
            let Some(activation) = bundled_activations.get(index) else {
                return part.clone();
            };
            let ContentPart::Text { text, meta } = part else {
                return part.clone();
            };
            if meta
                .as_ref()
                .is_some_and(|meta| meta.contains_key("source"))
            {
                return part.clone();
            }
            index += 1;
            skill_activation_part(text, &activation.activation_id)
        })
        .collect()
}

fn user_origin_of(origin: Option<&PromptOrigin>) -> Option<&UserPromptOrigin> {
    origin.filter(|origin| origin.kind == "user")
}

fn bundled_skill_activations_of(message: &SteerMessage) -> &[BundledSkillActivation] {
    user_origin_of(message.origin.as_ref())
        .and_then(|origin| origin.skill_activations.as_deref())
        .unwrap_or(&[])
}

pub fn strip_bundled_skill_blocks(message: &SteerMessage) -> Vec<ContentPart> {
    annotate_bundled_skill_parts(
        &message.content,
        bundled_skill_activations_of(message),
    )
    .into_iter()
    .filter(|part| !is_skill_activation_part(part))
    .collect()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MergedSteerMessage {
    pub role: Role,
    pub content: Vec<ContentPart>,
    pub tool_calls: Vec<ToolCall>,
    pub origin: UserPromptOrigin,
}

pub fn merge_steer_messages(messages: &[SteerMessage]) -> MergedSteerMessage {
    let has_client_metadata = messages.iter().any(|message| {
        user_origin_of(message.origin.as_ref())
            .and_then(|origin| origin.client_metadata.as_ref())
            .is_some_and(|metadata| !metadata.is_empty())
    });
    let mut client_metadata = Vec::new();
    if has_client_metadata {
        for message in messages {
            let metadata = user_origin_of(message.origin.as_ref())
                .and_then(|origin| origin.client_metadata.as_ref())
                .filter(|metadata| !metadata.is_empty());
            if let Some(metadata) = metadata {
                client_metadata.extend(metadata.clone());
            } else {
                client_metadata.push(Map::from_iter([(
                    "display_text".to_string(),
                    json!(prompt_display_text_from_content_parts(
                        &strip_bundled_skill_blocks(message),
                    )),
                )]));
            }
        }
    }
    let skill_activations = messages
        .iter()
        .flat_map(|message| bundled_skill_activations_of(message).to_vec())
        .collect::<Vec<_>>();
    let attachments = messages
        .iter()
        .flat_map(|message| {
            user_origin_of(message.origin.as_ref())
                .and_then(|origin| origin.attachments.clone())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>();
    let mut content = messages
        .iter()
        .flat_map(|message| {
            annotate_bundled_skill_parts(
                &message.content,
                bundled_skill_activations_of(message),
            )
            .into_iter()
            .filter(is_skill_activation_part)
            .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    content.extend(
        messages
            .iter()
            .flat_map(|message| strip_bundled_skill_blocks(message)),
    );
    let origin = if skill_activations.is_empty()
        && attachments.is_empty()
        && client_metadata.is_empty()
    {
        USER_PROMPT_ORIGIN.clone()
    } else {
        PromptOrigin {
            kind: "user".to_string(),
            in_turn: None,
            client_metadata: (!client_metadata.is_empty()).then_some(client_metadata),
            skill_activations: (!skill_activations.is_empty()).then_some(skill_activations),
            attachments: (!attachments.is_empty()).then_some(attachments),
            ..Default::default()
        }
    };
    MergedSteerMessage {
        role: Role::User,
        content,
        tool_calls: Vec::new(),
        origin,
    }
}

pub fn prompt_display_text_from_content_parts(parts: &[ContentPart]) -> String {
    parts
        .iter()
        .filter_map(prompt_part_text)
        .collect::<Vec<_>>()
        .join("\n")
}

fn prompt_part_text(part: &ContentPart) -> Option<String> {
    match part {
        ContentPart::Text { text, .. } => {
            if match_single_media_path_tag(text).is_some() {
                return None;
            }
            let text = extract_image_compression_captions(text).text;
            (!text.trim().is_empty()).then_some(text)
        }
        ContentPart::ImageUrl { .. } => Some("[image]".to_string()),
        ContentPart::AudioUrl { .. } => Some("[audio]".to_string()),
        ContentPart::VideoUrl { .. } => Some("[video]".to_string()),
        ContentPart::DocumentUrl { .. } => Some("[document]".to_string()),
        ContentPart::Think { .. } => None,
    }
}
