use serde::{Deserialize, Serialize};

use crate::human::llm::message::ContentPart;

const MEDIA_REF_SCHEME: &str = "media://";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaRefKind {
    Image,
    Video,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaRef {
    pub kind: MediaRefKind,
    #[serde(rename = "ref")]
    pub ref_: String,
}

pub fn build_media_ref_url(ref_: &str) -> String {
    format!("{MEDIA_REF_SCHEME}{ref_}")
}

pub fn parse_media_ref_url(url: &str) -> Option<String> {
    let ref_ = url.strip_prefix(MEDIA_REF_SCHEME)?;
    (!ref_.is_empty()).then(|| ref_.to_string())
}

pub fn media_ref_from_part(part: &ContentPart) -> Option<MediaRef> {
    match part {
        ContentPart::ImageUrl { image_url } => parse_media_ref_url(&image_url.url).map(|ref_| MediaRef {
            kind: MediaRefKind::Image,
            ref_,
        }),
        ContentPart::VideoUrl { video_url } => parse_media_ref_url(&video_url.url).map(|ref_| MediaRef {
            kind: MediaRefKind::Video,
            ref_,
        }),
        _ => None,
    }
}
