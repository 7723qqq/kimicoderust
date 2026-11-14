use serde::{Deserialize, Serialize};

use crate::human::llm::message::ContentPart;
use crate::native::tool_naming::is_mcp_tool_name;

const TOOL_ERROR_STATUS: &str = "<system>ERROR: Tool execution failed.</system>";
const TOOL_EMPTY_STATUS: &str = "<system>Tool output is empty.</system>";
const TOOL_EMPTY_ERROR_STATUS: &str =
    "<system>ERROR: Tool execution failed. Tool output is empty.</system>";
const TOOL_OUTPUT_EMPTY_TEXT: &str = "Tool output is empty.";
const WALL_TIME_TOOL_NAMES: [&str; 7] = [
    "Agent",
    "AgentSwarm",
    "Bash",
    "FetchURL",
    "Glob",
    "Grep",
    "WebSearch",
];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RenderableToolOutput {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderableToolResult {
    pub output: RenderableToolOutput,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

pub fn should_render_wall_time(tool_name: &str) -> bool {
    WALL_TIME_TOOL_NAMES.contains(&tool_name) || is_mcp_tool_name(tool_name)
}

pub fn render_tool_result_for_model(result: &RenderableToolResult) -> Vec<ContentPart> {
    let mut rendered = render_status(result);
    if let Some(duration_ms) = result.duration_ms {
        let header = format!("Wall time: {:.3} seconds", duration_ms as f64 / 1000.0);
        if let Some(ContentPart::Text { text, .. }) = rendered.first() {
            let combined = format!("{header}\n{text}");
            rendered[0] = text_part(&combined);
        } else {
            rendered.insert(0, text_part(&header));
        }
    }
    let Some(note) = result.note.as_ref().filter(|note| !note.is_empty()) else {
        return rendered;
    };
    if rendered.len() == 1 {
        if let ContentPart::Text { text, .. } = &rendered[0] {
            let combined = format!("{text}\n{note}");
            rendered[0] = text_part(&combined);
            return rendered;
        }
    }
    rendered.push(text_part(note));
    rendered
}

fn render_status(result: &RenderableToolResult) -> Vec<ContentPart> {
    let single = match &result.output {
        RenderableToolOutput::Text(output) => Some(output.as_str()),
        RenderableToolOutput::Parts(parts) => single_text_part(parts),
    };
    if let Some(single) = single {
        if result.is_error == Some(true) {
            if is_empty_output_text(single) {
                return vec![text_part(TOOL_EMPTY_ERROR_STATUS)];
            }
            return vec![text_part(&format!("{TOOL_ERROR_STATUS}\n{single}"))];
        }
        return if is_empty_output_text(single) {
            vec![text_part(TOOL_EMPTY_STATUS)]
        } else {
            vec![text_part(single)]
        };
    }
    let RenderableToolOutput::Parts(parts) = &result.output else {
        unreachable!()
    };
    if is_empty_equivalent_content_array(parts) {
        return vec![text_part(if result.is_error == Some(true) {
            TOOL_EMPTY_ERROR_STATUS
        } else {
            TOOL_EMPTY_STATUS
        })];
    }
    let mut rendered = Vec::new();
    if result.is_error == Some(true) {
        rendered.push(text_part(TOOL_ERROR_STATUS));
    }
    rendered.extend(parts.iter().cloned());
    rendered
}

fn single_text_part(output: &[ContentPart]) -> Option<&str> {
    if output.len() != 1 {
        return None;
    }
    match &output[0] {
        ContentPart::Text { text, .. } => Some(text),
        _ => None,
    }
}

fn text_part(text: &str) -> ContentPart {
    ContentPart::Text {
        text: text.to_string(),
        meta: None,
    }
}

fn is_empty_output_text(output: &str) -> bool {
    output.trim().is_empty() || output.trim() == TOOL_OUTPUT_EMPTY_TEXT
}

fn is_empty_equivalent_content_array(output: &[ContentPart]) -> bool {
    output.iter().all(|part| {
        matches!(part, ContentPart::Text { text, .. } if text.trim().is_empty())
    })
}
