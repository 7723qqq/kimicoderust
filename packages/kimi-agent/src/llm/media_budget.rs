pub use kimi_core::types::{
    DroppedMedia, Entry, MediaRecovery, MediaRecoveryAction, MediaRecoveryState,
    SharedMediaRecovery,
};

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use crate::llm::media_resolver::{build_media_path_tag, unavailable_text};
use crate::rpc::types::{ContentPart, MediaKind};
use kimi_core::types::LLMMessage;

pub const REQUEST_MEDIA_BUDGET_BYTES: usize = 20 * 1024 * 1024;

pub const REQUEST_MEDIA_BUDGET_LOW_BYTES: usize = 10 * 1024 * 1024;

pub const MEDIA_BUDGET_EXCEEDED_CODE: &str = "media-budget-exceeded";

#[derive(Default)]
pub struct MediaBudget {
    dropped: HashSet<String>,
    shared: Option<DroppedMedia>,
}

impl MediaBudget {
    pub fn new(dropped: HashSet<String>) -> Self {
        Self {
            dropped,
            shared: None,
        }
    }

    pub fn shared(shared: DroppedMedia) -> Self {
        let dropped = shared.lock().unwrap_or_else(|e| e.into_inner()).clone();
        Self {
            dropped,
            shared: Some(shared),
        }
    }

    pub fn dropped(&self) -> &HashSet<String> {
        &self.dropped
    }

    pub fn apply<'a>(
        &mut self,
        messages: &mut Cow<'a, [LLMMessage]>,
        entries: &[Entry],
    ) -> Option<String> {
        if entries.is_empty() {
            return None;
        }

        let mut pending: Vec<(&str, usize)> = Vec::new();
        let mut seen: HashMap<&str, usize> = HashMap::new();
        for entry in entries {
            if self.dropped.contains(&entry.key) {
                replace_with_media_tag(messages.to_mut(), entry);
                continue;
            }
            match seen.get(entry.key.as_str()) {
                Some(&at) => pending[at].1 += entry.bytes,
                None => {
                    seen.insert(entry.key.as_str(), pending.len());
                    pending.push((entry.key.as_str(), entry.bytes));
                }
            }
        }
        let mut total: usize = pending.iter().map(|(_, bytes)| bytes).sum();
        if total <= REQUEST_MEDIA_BUDGET_BYTES {
            return None;
        }
        let mut dropped_now: HashSet<&str> = HashSet::new();
        for (key, bytes) in &pending {
            if total <= REQUEST_MEDIA_BUDGET_LOW_BYTES {
                break;
            }

            if *bytes == 0 {
                continue;
            }
            dropped_now.insert(key);
            total -= bytes;
        }
        for entry in entries {
            if dropped_now.contains(entry.key.as_str()) {
                replace_with_media_tag(messages.to_mut(), entry);
            }
        }
        self.dropped
            .extend(dropped_now.iter().map(|key| (*key).to_string()));
        if let Some(shared) = &self.shared {
            shared
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend(dropped_now.iter().map(|key| (*key).to_string()));
        }

        let has_untracked_inline = entries
            .iter()
            .any(|entry| dropped_now.contains(entry.key.as_str()) && entry.file_id.is_none());
        Some(format!(
            "Conversation media exceeded the {} MB per-request budget; {} older media item(s) were omitted{}",
            REQUEST_MEDIA_BUDGET_BYTES / (1024 * 1024),
            dropped_now.len(),
            if has_untracked_inline {
                "."
            } else {
                " and remain available at their saved paths."
            }
        ))
    }
}

pub(crate) fn replace_with_media_tag(messages: &mut [LLMMessage], entry: &Entry) {
    let text = match (&entry.file_id, &entry.path) {
        (Some(_), Some(path)) => build_media_path_tag(entry.kind, path),
        (Some(_), None) => unavailable_text(entry.kind).to_string(),
        _ => budget_omitted_media(entry.kind),
    };
    set_block_text(messages, entry, text);
}

fn set_block_text(messages: &mut [LLMMessage], entry: &Entry, text: String) {
    let Some(block) = messages
        .get_mut(entry.message_index)
        .and_then(|message| message.blocks.get_mut(entry.block_index))
    else {
        return;
    };
    *block = ContentPart::Text { text, meta: None };
}

pub fn budget_omitted_media(kind: MediaKind) -> String {
    format!(
        "[{} omitted: dropped to fit the request media budget]",
        kind.as_str()
    )
}

fn replace_with_recovery_text(
    messages: &mut [LLMMessage],
    entry: &Entry,
    placeholder: fn(MediaKind) -> String,
) {
    let text = match &entry.path {
        Some(path) => build_media_path_tag(entry.kind, path),
        None => placeholder(entry.kind),
    };
    set_block_text(messages, entry, text);
}

pub const MEDIA_DEGRADE_KEEP_RECENT: usize = 2;

pub fn degraded_media_placeholder(kind: MediaKind) -> String {
    match kind {
        MediaKind::Image => {
            "[image omitted: dropped to fit the provider request size limit; re-read the file to view it]"
        }
        MediaKind::Audio => {
            "[audio omitted: dropped to fit the provider request size limit; re-read the file to hear it]"
        }
        MediaKind::Video => {
            "[video omitted: dropped to fit the provider request size limit; re-read the file to view it]"
        }
    }
    .to_string()
}

pub fn stripped_media_placeholder(kind: MediaKind) -> String {
    match kind {
        MediaKind::Image => {
            "[image omitted for provider compatibility; re-read the file to view it or get conversion guidance]"
        }
        MediaKind::Audio => {
            "[audio omitted for provider compatibility; re-read the file to hear it]"
        }
        MediaKind::Video => {
            "[video omitted for provider compatibility; re-read the file to view it]"
        }
    }
    .to_string()
}

pub const MEDIA_DEGRADED_CODE: &str = "media-degraded";

pub const MEDIA_STRIPPED_CODE: &str = "media-stripped";

pub fn is_request_too_large(error: &str) -> bool {
    kimi_core::error::llm_http_status(error) == Some(413)
}

pub fn is_image_format_error(error: &str) -> bool {
    if kimi_core::error::llm_http_status(error) != Some(400) {
        return false;
    }
    let lower = error.to_lowercase();
    const PATTERNS: [&str; 19] = [
        "unsupported image url",
        "unsupported image format",
        "unsupported image type",
        "does not represent a valid image",
        "could not process image",
        "could not process the image",
        "could not process input image",
        "could not decode image",
        "could not decode the image",
        "could not decode input image",
        "unable to process image",
        "unable to process the image",
        "unable to process input image",
        "failed to decode image",
        "failed to decode the image",
        "invalid image",
        "invalid image data",
        "invalid image type",
        "invalid image format",
    ];
    if PATTERNS.iter().any(|pattern| lower.contains(pattern)) {
        return true;
    }

    let names_media_type = lower.contains("media_type")
        || lower.contains("mediatype")
        || lower.contains("mime_type")
        || lower.contains("mimetype");
    names_media_type && lower.contains("image")
}

pub fn degrade_older_media(
    messages: &mut [LLMMessage],
    entries: &[Entry],
    keep_recent: usize,
) -> usize {
    let to_degrade = entries.len().saturating_sub(keep_recent);
    degrade_leading_media(messages, entries, to_degrade)
}

pub fn degrade_leading_media(
    messages: &mut [LLMMessage],
    entries: &[Entry],
    count: usize,
) -> usize {
    let to_degrade = count.min(entries.len());
    for entry in entries.iter().take(to_degrade) {
        replace_with_recovery_text(messages, entry, degraded_media_placeholder);
    }
    to_degrade
}

pub fn strip_all_media(messages: &mut [LLMMessage], entries: &[Entry]) -> usize {
    let mut replaced = 0;
    for entry in entries {
        replace_with_recovery_text(messages, entry, stripped_media_placeholder);
        replaced += 1;
    }
    replaced
}

pub fn apply_media_recovery(
    messages: &mut [LLMMessage],
    entries: &[Entry],
    recovery: &MediaRecovery,
) -> (bool, bool) {
    match &recovery.action {
        MediaRecoveryAction::Degraded(count) => {
            degrade_leading_media(messages, entries, *count);
            (true, false)
        }
        MediaRecoveryAction::Stripped(keys) => {
            let mut remaining = 0usize;
            for entry in entries {
                if keys.iter().any(|key| key == &entry.key) {
                    replace_with_recovery_text(messages, entry, stripped_media_placeholder);
                } else {
                    remaining += 1;
                }
            }
            (true, remaining == 0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn media_message(block: ContentPart) -> LLMMessage {
        LLMMessage {
            role: kimi_core::types::Role::User,
            content: String::new(),
            blocks: vec![block],
            tool_calls: Vec::new(),
            tool_call_id: None,
            tools: Vec::new(),

            prompt_id: None,
            origin: None,
        }
    }

    fn inline_entry(index: usize, tag: &str, bytes: usize) -> Entry {
        Entry {
            message_index: index,
            block_index: 0,
            key: format!("inline\0{tag}"),
            file_id: None,
            path: None,
            kind: MediaKind::Image,
            bytes,
        }
    }

    fn ref_entry(index: usize, file_id: &str, bytes: usize) -> Entry {
        Entry {
            message_index: index,
            block_index: 0,
            key: file_id.to_string(),
            file_id: Some(file_id.to_string()),
            path: Some(format!("/blobs/files/{file_id}")),
            kind: MediaKind::Image,
            bytes,
        }
    }

    fn omitted_text(message: &LLMMessage) -> Option<&str> {
        match message.blocks.first() {
            Some(ContentPart::Text { text, .. }) => Some(text.as_str()),
            _ => None,
        }
    }

    fn apply(
        budget: &mut MediaBudget,
        messages: &mut Vec<LLMMessage>,
        entries: &[Entry],
    ) -> Option<String> {
        let mut request = Cow::Owned(std::mem::take(messages));
        let warning = budget.apply(&mut request, entries);
        *messages = request.into_owned();
        warning
    }

    fn inline_image(tag: &str) -> ContentPart {
        crate::llm::media_resolver::inline_image_part(
            "image/png".into(),
            tag.to_string(),
            None,
        )
    }

    #[test]
    fn request_error_classification_matches_media_recovery_errors() {
        assert!(is_request_too_large(
            "llm http status 413: request too large"
        ));
        assert!(!is_request_too_large("llm http status 500: boom"));
        assert!(!is_request_too_large("context length exceeded"));

        assert!(is_image_format_error(
            "llm http status 400: unsupported image format image/heic"
        ));
        assert!(is_image_format_error(
            "llm http status 400: does not represent a valid image"
        ));
        assert!(is_image_format_error(
            "llm http status 400: media_type image/svg+xml is not supported"
        ));
        assert!(!is_image_format_error(
            "llm http status 400: media_type video/mp4 is not supported"
        ));
        assert!(!is_image_format_error(
            "llm http status 413: unsupported image format"
        ));
        assert!(!is_image_format_error("llm http status 500: boom"));
    }

    #[test]
    fn test_under_budget_passes_every_media_through() {
        let mut messages = vec![
            media_message(inline_image("a")),
            media_message(inline_image("b")),
        ];
        let entries = vec![inline_entry(0, "a", 1024), inline_entry(1, "b", 2048)];
        let mut budget = MediaBudget::default();

        let warning = apply(&mut budget, &mut messages, &entries);

        assert_eq!(warning, None);
        assert!(omitted_text(&messages[0]).is_none());
        assert!(omitted_text(&messages[1]).is_none());
    }

    #[test]
    fn test_over_budget_omits_the_oldest_media_first() {
        let mut messages = vec![
            media_message(inline_image("a")),
            media_message(inline_image("b")),
            media_message(inline_image("c")),
        ];
        let entries = vec![
            inline_entry(0, "a", 8 * 1024 * 1024),
            inline_entry(1, "b", 8 * 1024 * 1024),
            inline_entry(2, "c", 8 * 1024 * 1024),
        ];
        let mut budget = MediaBudget::default();

        apply(&mut budget, &mut messages, &entries);

        assert_eq!(
            omitted_text(&messages[0]),
            Some("[image omitted: dropped to fit the request media budget]")
        );
        assert_eq!(
            omitted_text(&messages[1]),
            Some("[image omitted: dropped to fit the request media budget]")
        );
        assert!(
            omitted_text(&messages[2]).is_none(),
            "the newest media must survive: {:?}",
            messages[2].blocks
        );
    }

    #[test]
    fn test_an_omitted_reference_leaves_its_saved_path() {
        let mut messages = vec![
            media_message(inline_image("a")),
            media_message(inline_image("b")),
        ];
        let entries = vec![
            ref_entry(0, "f_old", 12 * 1024 * 1024),
            ref_entry(1, "f_new", 9 * 1024 * 1024),
        ];
        let mut budget = MediaBudget::default();

        let warning = apply(&mut budget, &mut messages, &entries);

        assert_eq!(
            omitted_text(&messages[0]),
            Some("<image path=\"/blobs/files/f_old\"></image>")
        );
        assert!(omitted_text(&messages[1]).is_none());
        assert_eq!(
            warning.as_deref(),
            Some(
                "Conversation media exceeded the 20 MB per-request budget; \
                 1 older media item(s) were omitted and remain available at their saved paths."
            )
        );
    }

    #[test]
    fn test_an_omitted_inline_payload_gets_the_plain_placeholder() {
        let mut messages = vec![
            media_message(inline_image("a")),
            media_message(inline_image("b")),
        ];
        let entries = vec![
            inline_entry(0, "a", 12 * 1024 * 1024),
            inline_entry(1, "b", 9 * 1024 * 1024),
        ];
        let mut budget = MediaBudget::default();

        let warning = apply(&mut budget, &mut messages, &entries);

        assert_eq!(
            omitted_text(&messages[0]),
            Some("[image omitted: dropped to fit the request media budget]")
        );
        assert_eq!(
            warning.as_deref(),
            Some(
                "Conversation media exceeded the 20 MB per-request budget; \
                 1 older media item(s) were omitted."
            )
        );
    }

    #[test]
    fn test_omission_stops_at_the_low_mark() {
        let mut messages = vec![
            media_message(inline_image("a")),
            media_message(inline_image("b")),
        ];
        let entries = vec![
            inline_entry(0, "a", 12 * 1024 * 1024),
            inline_entry(1, "b", 9 * 1024 * 1024),
        ];
        let mut budget = MediaBudget::default();

        apply(&mut budget, &mut messages, &entries);

        assert!(omitted_text(&messages[0]).is_some());
        assert!(omitted_text(&messages[1]).is_none());
    }

    #[test]
    fn test_omitted_media_stay_omitted_on_the_next_request() {
        let mut messages = vec![
            media_message(inline_image("a")),
            media_message(inline_image("b")),
            media_message(inline_image("c")),
        ];
        let entries = vec![
            inline_entry(0, "a", 8 * 1024 * 1024),
            inline_entry(1, "b", 8 * 1024 * 1024),
            inline_entry(2, "c", 8 * 1024 * 1024),
        ];
        let mut budget = MediaBudget::default();
        assert!(apply(&mut budget, &mut messages, &entries).is_some());

        let mut again = vec![
            media_message(inline_image("a")),
            media_message(inline_image("b")),
            media_message(inline_image("c")),
        ];
        let warning = apply(&mut budget, &mut again, &entries);

        assert!(omitted_text(&again[0]).is_some());
        assert!(omitted_text(&again[1]).is_some());
        assert!(omitted_text(&again[2]).is_none());
        assert_eq!(warning, None, "nothing new was omitted");
    }

    #[test]
    fn test_a_seeded_budget_keeps_an_earlier_turns_omission() {
        let mut messages = vec![
            media_message(inline_image("a")),
            media_message(inline_image("b")),
        ];
        let entries = vec![inline_entry(0, "a", 1024), inline_entry(1, "b", 1024)];
        let mut budget = MediaBudget::new(HashSet::from(["inline\0a".to_string()]));

        let warning = apply(&mut budget, &mut messages, &entries);

        assert!(omitted_text(&messages[0]).is_some());
        assert!(omitted_text(&messages[1]).is_none());
        assert_eq!(warning, None, "the omission was decided by an earlier turn");
    }

    #[test]
    fn test_repeated_media_are_counted_per_occurrence_and_omitted_everywhere() {
        let mut messages = vec![
            media_message(inline_image("a")),
            media_message(inline_image("a")),
        ];
        let entries = vec![
            inline_entry(0, "a", 11 * 1024 * 1024),
            inline_entry(1, "a", 11 * 1024 * 1024),
        ];
        let mut budget = MediaBudget::default();

        let warning = apply(&mut budget, &mut messages, &entries);

        assert!(omitted_text(&messages[0]).is_some());
        assert!(omitted_text(&messages[1]).is_some());
        assert_eq!(
            warning.as_deref(),
            Some(
                "Conversation media exceeded the 20 MB per-request budget; \
                 1 older media item(s) were omitted."
            )
        );
    }

    #[test]
    fn a_recovery_applies_only_to_its_model_and_revision() {
        let mut state = MediaRecoveryState::default();
        state.mark("kimi".into(), MediaRecoveryAction::Degraded(2));
        assert!(state.applicable("kimi").is_some());
        assert!(state.applicable("other-model").is_none());
        state.advance_revision();
        assert!(state.applicable("kimi").is_none());

        state.mark("kimi".into(), MediaRecoveryAction::Degraded(2));
        assert!(state.applicable("kimi").is_some());
    }

    #[test]
    fn test_a_zero_byte_reference_is_never_omitted() {
        let mut messages = vec![
            media_message(inline_image("ref")),
            media_message(inline_image("a")),
            media_message(inline_image("b")),
        ];
        let entries = vec![
            ref_entry(0, "f_uploaded", 0),
            inline_entry(1, "a", 12 * 1024 * 1024),
            inline_entry(2, "b", 9 * 1024 * 1024),
        ];
        let mut budget = MediaBudget::default();

        apply(&mut budget, &mut messages, &entries);

        assert!(
            omitted_text(&messages[0]).is_none(),
            "a zero-byte reference must survive"
        );
        assert!(omitted_text(&messages[1]).is_some());
    }
}
