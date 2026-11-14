use std::borrow::Cow;
use std::collections::HashMap;

use sha2::{Digest, Sha256};

use crate::agent::media::media_ref::{build_media_path_tag, media_kind_of_part};
use crate::human::llm::message::ContentPart;
use crate::llm_adapter::contract::message::Message;

use super::context_projector::MediaStripSnapshot;

#[derive(Debug, Clone, Copy)]
pub struct MediaPlaceholderSet {
    pub image_url: &'static str,
    pub audio_url: &'static str,
    pub video_url: &'static str,
}

const MEDIA_DEGRADED_PLACEHOLDERS: MediaPlaceholderSet = MediaPlaceholderSet {
    image_url: "[image omitted: dropped to fit the provider request size limit; re-read the file to view it]",
    audio_url: "[audio omitted: dropped to fit the provider request size limit; re-read the file to hear it]",
    video_url: "[video omitted: dropped to fit the provider request size limit; re-read the file to view it]",
};

pub const MEDIA_STRIPPED_PLACEHOLDERS: MediaPlaceholderSet = MediaPlaceholderSet {
    image_url: "[image omitted for provider compatibility; re-read the file to view it or get conversion guidance]",
    audio_url: "[audio omitted for provider compatibility; re-read the file to hear it]",
    video_url: "[video omitted for provider compatibility; re-read the file to view it]",
};

struct MediaContainer<'a> {
    part_type: &'static str,
    url: &'a str,
    id: Option<&'a str>,
}

fn media_container(part: &ContentPart) -> Option<MediaContainer<'_>> {
    match part {
        ContentPart::ImageUrl { image_url } => Some(MediaContainer {
            part_type: "image_url",
            url: &image_url.url,
            id: image_url.id.as_deref(),
        }),
        ContentPart::AudioUrl { audio_url } => Some(MediaContainer {
            part_type: "audio_url",
            url: &audio_url.url,
            id: audio_url.id.as_deref(),
        }),
        ContentPart::VideoUrl { video_url } => Some(MediaContainer {
            part_type: "video_url",
            url: &video_url.url,
            id: video_url.id.as_deref(),
        }),
        _ => None,
    }
}

fn media_strip_key(part: &ContentPart) -> Option<String> {
    let container = media_container(part)?;
    let mut hasher = Sha256::new();
    hasher.update(container.part_type.as_bytes());
    hasher.update([0]);
    hasher.update(container.id.unwrap_or("").as_bytes());
    hasher.update([0]);
    hasher.update(container.url.as_bytes());
    Some(format!("{:x}", hasher.finalize()))
}

fn placeholder(part: &ContentPart, placeholders: &MediaPlaceholderSet) -> Option<&'static str> {
    match part {
        ContentPart::ImageUrl { .. } => Some(placeholders.image_url),
        ContentPart::AudioUrl { .. } => Some(placeholders.audio_url),
        ContentPart::VideoUrl { .. } => Some(placeholders.video_url),
        _ => None,
    }
}

fn media_path_tag(
    part: &ContentPart,
    media_paths: Option<&HashMap<String, String>>,
) -> Option<ContentPart> {
    let container = media_container(part)?;
    let path = media_paths?.get(container.url)?;
    let kind = media_kind_of_part(part)?;
    Some(ContentPart::Text {
        text: build_media_path_tag(kind, path),
        meta: None,
    })
}

fn replacement(
    part: &ContentPart,
    placeholders: &MediaPlaceholderSet,
    media_paths: Option<&HashMap<String, String>>,
) -> ContentPart {
    if let Some(tag) = media_path_tag(part, media_paths) {
        return tag;
    }
    match placeholder(part, placeholders) {
        Some(text) => ContentPart::Text {
            text: text.to_string(),
            meta: None,
        },
        None => part.clone(),
    }
}

pub fn capture_media_strip_snapshot(messages: &[Message]) -> MediaStripSnapshot {
    let mut keys = std::collections::HashSet::new();
    for message in messages {
        for part in &message.content {
            if let Some(key) = media_strip_key(part) {
                keys.insert(key);
            }
        }
    }
    MediaStripSnapshot { keys }
}

pub fn strip_media_parts_by_snapshot<'a>(
    messages: &'a [Message],
    snapshot: &MediaStripSnapshot,
    media_paths: Option<&HashMap<String, String>>,
) -> Cow<'a, [Message]> {
    let mut changed = false;
    let result = messages
        .iter()
        .map(|message| {
            let mut message_changed = false;
            let content = message
                .content
                .iter()
                .map(|part| {
                    let Some(key) = media_strip_key(part) else {
                        return part.clone();
                    };
                    if !snapshot.keys.contains(&key) {
                        return part.clone();
                    }
                    changed = true;
                    message_changed = true;
                    replacement(part, &MEDIA_STRIPPED_PLACEHOLDERS, media_paths)
                })
                .collect::<Vec<_>>();
            if message_changed {
                Message {
                    content,
                    ..message.clone()
                }
            } else {
                message.clone()
            }
        })
        .collect::<Vec<_>>();
    if changed {
        Cow::Owned(result)
    } else {
        Cow::Borrowed(messages)
    }
}

pub fn count_media_parts(messages: &[Message]) -> usize {
    messages
        .iter()
        .map(|message| {
            message
                .content
                .iter()
                .filter(|part| media_container(part).is_some())
                .count()
        })
        .sum()
}

pub fn degrade_leading_media_parts<'a>(
    messages: &'a [Message],
    count: usize,
    placeholders: Option<&MediaPlaceholderSet>,
    media_paths: Option<&HashMap<String, String>>,
) -> Cow<'a, [Message]> {
    if count == 0 {
        return Cow::Borrowed(messages);
    }
    let placeholders = placeholders.unwrap_or(&MEDIA_DEGRADED_PLACEHOLDERS);
    let mut to_degrade = count;
    Cow::Owned(
        messages
            .iter()
            .map(|message| {
                if to_degrade == 0
                    || !message
                        .content
                        .iter()
                        .any(|part| media_container(part).is_some())
                {
                    return message.clone();
                }
                let content = message
                    .content
                    .iter()
                    .map(|part| {
                        if to_degrade == 0 || media_container(part).is_none() {
                            return part.clone();
                        }
                        to_degrade -= 1;
                        replacement(part, placeholders, media_paths)
                    })
                    .collect();
                Message {
                    content,
                    ..message.clone()
                }
            })
            .collect(),
    )
}
