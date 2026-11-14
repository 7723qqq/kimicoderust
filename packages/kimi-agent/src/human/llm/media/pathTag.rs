use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

static MEDIA_PATH_TAG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<(image|video|audio|document|file)\b[^>]*?\bpath="([^"]*)"[^>]*>"#).unwrap()
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaPathTagKind {
    Image,
    Video,
    Audio,
    Document,
    File,
}

impl MediaPathTagKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Document => "document",
            Self::File => "file",
        }
    }

    fn parse(value: &str) -> Self {
        match value {
            "image" => Self::Image,
            "video" => Self::Video,
            "audio" => Self::Audio,
            "document" => Self::Document,
            "file" => Self::File,
            _ => unreachable!(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaPathTag {
    pub kind: MediaPathTagKind,
    pub path: String,
    pub index: usize,
    pub text: String,
}

pub fn escape_media_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn unescape_media_attribute(value: &str) -> String {
    value
        .replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

pub fn build_media_path_tag(kind: MediaPathTagKind, path: &str) -> String {
    let kind = kind.as_str();
    format!("<{kind} path=\"{}\"></{kind}>", escape_media_attribute(path))
}

pub fn match_media_path_tags(text: &str) -> Vec<MediaPathTag> {
    MEDIA_PATH_TAG_RE
        .captures_iter(text)
        .map(|captures| {
            let matched = captures.get(0).unwrap();
            let kind = MediaPathTagKind::parse(captures.get(1).unwrap().as_str());
            let closing = format!("</{}>", kind.as_str());
            let end = if text[matched.end()..].starts_with(&closing) {
                matched.end() + closing.len()
            } else {
                matched.end()
            };
            MediaPathTag {
                kind,
                path: unescape_media_attribute(captures.get(2).unwrap().as_str()),
                index: text[..matched.start()].encode_utf16().count(),
                text: text[matched.start()..end].to_string(),
            }
        })
        .collect()
}

pub fn match_single_media_path_tag(text: &str) -> Option<MediaPathTag> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let tags = match_media_path_tags(trimmed);
    if tags.len() != 1 {
        return None;
    }
    let tag = tags.into_iter().next().unwrap();
    (tag.index == 0 && tag.text.len() == trimmed.len()).then_some(tag)
}
