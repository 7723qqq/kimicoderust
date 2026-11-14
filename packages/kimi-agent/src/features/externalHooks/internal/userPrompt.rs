use serde_json::{Map, Value};

use crate::rpc::types::ContentBlock;

use super::types::{HookAction, HookResult};

pub fn render_hook_result(event: &str, message: &str) -> String {
    format!("<hook_result hook_event=\"{event}\">\n{message}\n</hook_result>")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedHookResult {
    pub event: String,
    pub message: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RenderedUserPromptHookParts {
    pub event: String,
    pub messages: Vec<String>,
    pub parts: Vec<ContentBlock>,
}

pub fn render_user_prompt_hook_result(
    results: &[HookResult],
) -> Option<RenderedUserPromptHookParts> {
    let messages: Vec<String> = results
        .iter()
        .filter(|result| result.action != HookAction::Block)
        .filter_map(user_prompt_hook_message)
        .filter(|message| !message.is_empty())
        .collect();
    if messages.is_empty() {
        return None;
    }
    let parts = messages
        .iter()
        .map(|message| {
            user_prompt_submit_hook_part(render_hook_result("UserPromptSubmit", message))
        })
        .collect();
    Some(RenderedUserPromptHookParts {
        event: "UserPromptSubmit".to_string(),
        messages,
        parts,
    })
}

pub fn render_user_prompt_hook_block_result(
    results: &[HookResult],
) -> Option<RenderedHookResult> {
    let block = results
        .iter()
        .find(|result| result.action == HookAction::Block)?;
    if let Some(message) = non_empty(block.message.as_deref()) {
        return Some(rendered("UserPromptSubmit", message));
    }
    let message = non_empty(block.reason.as_deref())
        .unwrap_or_else(|| "Blocked by UserPromptSubmit hook".to_string());
    Some(rendered("UserPromptSubmit", message))
}

fn rendered(event: &str, message: String) -> RenderedHookResult {
    RenderedHookResult {
        event: event.to_string(),
        text: render_hook_result(event, &message),
        message,
    }
}

fn user_prompt_hook_message(result: &HookResult) -> Option<String> {
    if result.timed_out == Some(true) || result.exit_code.is_some_and(|code| code != 0) {
        return None;
    }
    if let Some(message) = non_empty(result.message.as_deref()) {
        return Some(message);
    }
    if result.structured_output == Some(true) {
        return None;
    }
    non_empty(result.stdout.as_deref())
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(trim_js)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn trim_js(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

fn is_js_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'
            | '\u{000a}'
            | '\u{000b}'
            | '\u{000c}'
            | '\u{000d}'
            | '\u{0020}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'
            | '\u{2001}'
            | '\u{2002}'
            | '\u{2003}'
            | '\u{2004}'
            | '\u{2005}'
            | '\u{2006}'
            | '\u{2007}'
            | '\u{2008}'
            | '\u{2009}'
            | '\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

fn user_prompt_submit_hook_part(text: String) -> ContentBlock {
    let mut meta = Map::new();
    meta.insert("contentType".to_string(), Value::String("text/xml".to_string()));
    meta.insert(
        "source".to_string(),
        Value::String("user prompt submit hook".to_string()),
    );
    ContentBlock::Text {
        text,
        meta: Some(meta),
    }
}
