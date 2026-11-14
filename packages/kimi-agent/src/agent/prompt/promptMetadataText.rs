use std::sync::LazyLock;

use regex::{Regex, RegexBuilder};
use serde_json::Value;

use crate::human::agent::origin::prompt_display_text_from_content_parts;
use crate::human::llm::message::ContentPart;

const MAX_TITLE_LENGTH: usize = 200;
const MAX_LAST_PROMPT_LENGTH: usize = 4000;

static PRIVATE_KEY_RE: LazyLock<Regex> = LazyLock::new(|| {
    RegexBuilder::new(r"-----BEGIN [^-]*PRIVATE KEY-----[\s\S]*?-----END [^-]*PRIVATE KEY-----")
        .case_insensitive(true)
        .unicode(false)
        .build()
        .unwrap()
});
static AUTHORIZATION_RE: LazyLock<Regex> = LazyLock::new(|| {
    RegexBuilder::new(r"\b(authorization)\s*:\s*bearer\s+\S+")
        .case_insensitive(true)
        .unicode(false)
        .build()
        .unwrap()
});
static SECRET_RE: LazyLock<Regex> = LazyLock::new(|| {
    RegexBuilder::new(
        r#"\b(api[_-]?key|token|secret|password|passwd|pwd)\b\s*[:=]\s*(?:"[^"]*"|'[^']*'|\S+)"#,
    )
    .case_insensitive(true)
    .build()
    .unwrap()
});
static API_TOKEN_RE: LazyLock<Regex> = LazyLock::new(|| {
    RegexBuilder::new(r"\bsk-[A-Za-z0-9_-]{12,}\b")
        .unicode(false)
        .build()
        .unwrap()
});
static LONG_TOKEN_RE: LazyLock<Regex> = LazyLock::new(|| {
    RegexBuilder::new(r"\b[A-Za-z0-9][A-Za-z0-9+/=_-]{39,}\b")
        .unicode(false)
        .build()
        .unwrap()
});
static WHITESPACE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[\u0009-\u000D \u00A0\u1680\u2000-\u200A\u2028\u2029\u202F\u205F\u3000\uFEFF]+").unwrap()
});

pub fn title_from_prompt_metadata_text(text: &str) -> String {
    String::from_utf16_lossy(&text.encode_utf16().take(MAX_TITLE_LENGTH).collect::<Vec<_>>())
}

pub fn prompt_metadata_text_from_content_parts(
    parts: &[ContentPart],
    client_metadata: Option<&Value>,
) -> Option<String> {
    if let Some(entries) = client_metadata.and_then(Value::as_array).filter(|entries| !entries.is_empty()) {
        let display_texts = entries
            .iter()
            .map(|entry| {
                entry
                    .get("display_text")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect::<Vec<_>>();
        if display_texts.iter().all(Option::is_some) {
            return prompt_metadata_text_from_text(
                &display_texts
                    .into_iter()
                    .map(Option::unwrap)
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        }
    }
    prompt_metadata_text_from_text(&prompt_display_text_from_content_parts(parts))
}

fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'..='\u{000D}'
            | ' '
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{FEFF}'
    )
}

pub fn prompt_metadata_text_from_text(text: &str) -> Option<String> {
    let mut sanitized = PRIVATE_KEY_RE.replace_all(text, "[redacted]").into_owned();
    sanitized = AUTHORIZATION_RE
        .replace_all(&sanitized, "$1: Bearer [redacted]")
        .into_owned();
    sanitized = SECRET_RE
        .replace_all(&sanitized, "$1=[redacted]")
        .into_owned();
    sanitized = API_TOKEN_RE.replace_all(&sanitized, "[redacted]").into_owned();
    sanitized = LONG_TOKEN_RE.replace_all(&sanitized, "[redacted]").into_owned();
    sanitized = sanitized
        .chars()
        .map(|character| if character.is_control() { ' ' } else { character })
        .collect();
    sanitized = WHITESPACE_RE
        .replace_all(&sanitized, " ")
        .trim_matches(is_js_whitespace)
        .to_string();
    if sanitized.is_empty() {
        return None;
    }
    Some(String::from_utf16_lossy(
        &sanitized
            .encode_utf16()
            .take(MAX_LAST_PROMPT_LENGTH)
            .collect::<Vec<_>>(),
    ))
}
