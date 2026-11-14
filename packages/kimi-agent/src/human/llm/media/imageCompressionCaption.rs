use std::sync::LazyLock;

use regex::{Captures, Regex};

const CAPTION_OPENING: &str = "<system>Image compressed to fit model limits:";
static CAPTION_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"<system>(Image compressed to fit model limits:[\s\S]*?)</system>").unwrap()
});

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageCompressionCaptionExtraction {
    pub captions: Vec<String>,
    pub text: String,
}

pub fn extract_image_compression_captions(text: &str) -> ImageCompressionCaptionExtraction {
    if !text.contains(CAPTION_OPENING) {
        return ImageCompressionCaptionExtraction {
            captions: Vec::new(),
            text: text.to_string(),
        };
    }
    let mut captions = Vec::new();
    let remainder = CAPTION_PATTERN
        .replace_all(text, |captures: &Captures<'_>| {
            captions.push(captures[1].to_string());
            ""
        })
        .into_owned();
    ImageCompressionCaptionExtraction {
        captions,
        text: remainder,
    }
}
