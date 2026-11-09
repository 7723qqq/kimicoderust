//! The model-facing status wrapper on a tool result, ported from v2
//! `agent-core-v2/src/agent/contextMemory/toolResultRender.ts`.
//!
//! v2 runs every `role: "tool"` message through `renderToolResultForModel`
//! before it reaches the model (`projection.ts:342`). The wrapper is how the
//! model tells a failed call from an empty one: the executor's error flag
//! otherwise dies with the event stream, and a tool that returns nothing reads
//! to the model exactly like one that succeeded with nothing to say.
//!
//! Only v2's status step lives in this module; its other two run at the same
//! call site in `run_turn`, in v2's order:
//!
//!   - the wall-time header is `wall_time::prepend_wall_time`, prepended
//!     *after* this wrapper on the tools that declare it;
//!   - the trailing `note` append is `content + '\n' + note`, applied last.
//!
//! `note` used to be overloaded as an internal provenance tag
//! (`"tool_policy"`, `"tool_select"`, `"native_subagent"`, `"mcp:<server>"`)
//! besides model-facing prose, so that append wrote the tags into the model's
//! context on every MCP call and every refused call. The tags had no reader —
//! the host never forwarded a note — and are gone; `note` now means what it
//! means in v2 (`ToolResult.note`: prose the model is meant to read, e.g. the
//! Read/Edit encoding guidance and the readMedia video note).
//!
//! The markers stay English on purpose: `<system>…</system>` is format
//! scaffolding, which the repository's i18n rules put under "what not to
//! translate", and v2 hardcodes these same strings. The unused
//! `toolsV2.*.emptyOutput` catalog entry is therefore *not* the source here —
//! it stays orphan debt.

use crate::rpc::types::ContentBlock;

/// v2 `TOOL_ERROR_STATUS`.
const TOOL_ERROR_STATUS: &str = "<system>ERROR: Tool execution failed.</system>";

/// v2 `TOOL_EMPTY_STATUS`.
const TOOL_EMPTY_STATUS: &str = "<system>Tool output is empty.</system>";

/// v2 `TOOL_EMPTY_ERROR_STATUS`.
const TOOL_EMPTY_ERROR_STATUS: &str =
    "<system>ERROR: Tool execution failed. Tool output is empty.</system>";

/// v2 `TOOL_OUTPUT_EMPTY_TEXT`: a tool that reports this text counts as empty.
const TOOL_OUTPUT_EMPTY_TEXT: &str = "Tool output is empty.";

/// v2 `renderStatus`, for this engine's text-only results.
///
/// `ExecutableToolResult::content` is always a single text payload, so v2's
/// `ContentPart[]` branches collapse into this one shape.
///
/// The empty checks are not symmetric, and that is v2's behaviour, not an
/// oversight: the error arm tests the raw length, so an error whose content is
/// only whitespace keeps that whitespace after the marker, while the
/// non-error arm trims and also treats `TOOL_OUTPUT_EMPTY_TEXT` itself as empty.
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

/// v2 `isEmptyOutputText`.
fn is_empty_output_text(content: &str) -> bool {
    let trimmed = content.trim();
    trimmed.is_empty() || trimmed == TOOL_OUTPUT_EMPTY_TEXT
}

/// v2 `renderStatus` for a `ContentPart[]` output.
///
/// Same three branches as v2: an empty array gets a marker, a lone text part
/// is treated exactly like the string form, and a mixed payload only gains the
/// error marker when the call failed.
pub fn render_parts(parts: &[ContentBlock], is_error: bool) -> Vec<ContentBlock> {
    if parts.is_empty() {
        return vec![text_block(if is_error {
            TOOL_EMPTY_ERROR_STATUS
        } else {
            TOOL_EMPTY_STATUS
        })];
    }
    if let [ContentBlock::Text { text, .. }] = parts {
        return vec![text_block(render_status(text, is_error))];
    }
    if is_error {
        let mut rendered = vec![text_block(TOOL_ERROR_STATUS)];
        rendered.extend_from_slice(parts);
        return rendered;
    }
    parts.to_vec()
}

/// v2's header step for a `ContentPart[]`: the header joins the first text
/// part, or becomes a new leading part when the payload opens with media.
pub fn prepend_wall_time_parts(parts: &mut Vec<ContentBlock>, duration_ms: u64) {
    let header = crate::turn_loop::wall_time::wall_time_header(duration_ms);
    match parts.first_mut() {
        Some(ContentBlock::Text { text, .. }) => {
            *text = format!("{header}\n{text}");
        }
        _ => parts.insert(0, text_block(header)),
    }
}

/// v2's trailing note step for a `ContentPart[]`: a lone text part absorbs the
/// note after a newline, any other payload gets it as a final text part.
pub fn append_note_parts(parts: &mut Vec<ContentBlock>, note: &str) {
    if let [ContentBlock::Text { text, .. }] = parts.as_mut_slice() {
        text.push('\n');
        text.push_str(note);
        return;
    }
    parts.push(text_block(note));
}

fn text_block(text: impl Into<String>) -> ContentBlock {
    ContentBlock::Text {
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
