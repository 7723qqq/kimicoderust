use crate::rpc::types::ContentPart;

const TOOL_ERROR_STATUS: &str = "<system>ERROR: Tool execution failed.</system>";

const TOOL_EMPTY_STATUS: &str = "<system>Tool output is empty.</system>";

const TOOL_EMPTY_ERROR_STATUS: &str =
    "<system>ERROR: Tool execution failed. Tool output is empty.</system>";

const TOOL_OUTPUT_EMPTY_TEXT: &str = "Tool output is empty.";

pub fn render_status(content: &str, is_error: bool) -> String {
    if is_error {
        if content.is_empty() {
            return TOOL_EMPTY_ERROR_STATUS.to_string();
        }
        return format!("{TOOL_ERROR_STATUS}\n{content}");
    }
    if is_empty_output_text(content) {
        return TOOL_EMPTY_STATUS.to_string();
    }
    content.to_string()
}

fn is_empty_output_text(content: &str) -> bool {
    let trimmed = content.trim();
    trimmed.is_empty() || trimmed == TOOL_OUTPUT_EMPTY_TEXT
}

pub fn render_parts(parts: &[ContentPart], is_error: bool) -> Vec<ContentPart> {
    if parts.is_empty() {
        return vec![text_block(if is_error {
            TOOL_EMPTY_ERROR_STATUS
        } else {
            TOOL_EMPTY_STATUS
        })];
    }
    if let [ContentPart::Text { text, .. }] = parts {
        return vec![text_block(render_status(text, is_error))];
    }
    if is_error {
        let mut rendered = vec![text_block(TOOL_ERROR_STATUS)];
        rendered.extend_from_slice(parts);
        return rendered;
    }
    parts.to_vec()
}

pub fn prepend_wall_time_parts(parts: &mut Vec<ContentPart>, duration_ms: u64) {
    let header = crate::turn_loop::wall_time::wall_time_header(duration_ms);
    match parts.first_mut() {
        Some(ContentPart::Text { text, .. }) => {
            *text = format!("{header}\n{text}");
        }
        _ => parts.insert(0, text_block(header)),
    }
}

pub fn append_note_parts(parts: &mut Vec<ContentPart>, note: &str) {
    if let [ContentPart::Text { text, .. }] = parts.as_mut_slice() {
        text.push('\n');
        text.push_str(note);
        return;
    }
    parts.push(text_block(note));
}

fn text_block(text: impl Into<String>) -> ContentPart {
    ContentPart::Text {
        text: text.into(),
        meta: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_call_is_prefixed_with_the_error_marker() {
        assert_eq!(
            render_status("permission denied", true),
            "<system>ERROR: Tool execution failed.</system>\npermission denied"
        );
    }

    #[test]
    fn a_failed_call_with_no_output_gets_the_combined_marker() {
        assert_eq!(render_status("", true), TOOL_EMPTY_ERROR_STATUS);
    }

    #[test]
    fn the_error_arm_keeps_whitespace_content_verbatim() {
        assert_eq!(
            render_status("   ", true),
            "<system>ERROR: Tool execution failed.</system>\n   "
        );
    }

    #[test]
    fn a_successful_call_passes_through_unchanged() {
        assert_eq!(render_status("file contents", false), "file contents");
    }

    #[test]
    fn an_empty_success_gets_the_empty_marker() {
        assert_eq!(render_status("", false), TOOL_EMPTY_STATUS);
        assert_eq!(render_status("  \n ", false), TOOL_EMPTY_STATUS);
    }

    #[test]
    fn the_empty_output_sentence_counts_as_empty_after_trimming() {
        assert_eq!(
            render_status(TOOL_OUTPUT_EMPTY_TEXT, false),
            TOOL_EMPTY_STATUS
        );
        assert_eq!(
            render_status("  Tool output is empty.  ", false),
            TOOL_EMPTY_STATUS
        );
        assert_eq!(
            render_status("Tool output is empty, but here is more", false),
            "Tool output is empty, but here is more"
        );
    }
}
