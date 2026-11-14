use serde_json::{Map, Value};

use crate::human::llm::message::ContentPart;

pub const USER_PROMPT_SUBMIT_HOOK_SOURCE: &str = "user prompt submit hook";
pub const USER_PROMPT_SUBMIT_HOOK_CONTENT_TYPE: &str = "text/xml";

pub type TextPart = ContentPart;

pub fn user_prompt_submit_hook_part(text: impl Into<String>) -> TextPart {
    let mut meta = Map::new();
    meta.insert(
        "contentType".to_string(),
        Value::String(USER_PROMPT_SUBMIT_HOOK_CONTENT_TYPE.to_string()),
    );
    meta.insert(
        "source".to_string(),
        Value::String(USER_PROMPT_SUBMIT_HOOK_SOURCE.to_string()),
    );
    ContentPart::Text {
        text: text.into(),
        meta: Some(meta),
    }
}

pub fn is_user_prompt_submit_hook_part(part: &ContentPart) -> bool {
    matches!(
        part,
        ContentPart::Text { meta: Some(meta), .. }
            if meta.get("source").and_then(Value::as_str)
                == Some(USER_PROMPT_SUBMIT_HOOK_SOURCE)
    )
}
