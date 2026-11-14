use crate::tools::tool_retriever::ToolSearchHit;
use crate::turn_loop::types::LLMMessage;

pub const LOADABLE_TOOLS_VARIANT: &str = "loadable-tools";

pub const RETRIEVED_TOOLS_VARIANT: &str = "retrieved-tools";

pub const DYNAMIC_TOOL_SCHEMA_VARIANT: &str = "dynamic_tool_schema";

#[cfg(test)]
const LEGACY_TOOL_SELECT_VARIANT: &str = "tool_select";

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ToolSelectAnnouncementDisclosure {
    Snapshot {
        revision: usize,
        tools: Vec<crate::tools::tool_select_ops::ToolSelectTool>,
    },
    Delta {
        revision: usize,
        added: Vec<crate::tools::tool_select_ops::ToolSelectTool>,
        removed: Vec<crate::tools::tool_select_ops::ToolSelectTool>,
    },
}

impl ToolSelectAnnouncementDisclosure {
    pub fn revision(&self) -> usize {
        match self {
            Self::Snapshot { revision, .. } | Self::Delta { revision, .. } => *revision,
        }
    }
}

pub fn last_announcement_disclosure(
    messages: &[LLMMessage],
) -> Option<ToolSelectAnnouncementDisclosure> {
    let message = messages.iter().rev().find(|message| {
        message.origin.as_ref().is_some_and(|origin| {
            origin.get("kind").and_then(|kind| kind.as_str()) == Some("injection")
                && origin.get("variant").and_then(|variant| variant.as_str())
                    == Some(LOADABLE_TOOLS_VARIANT)
        })
    })?;
    let disclosure = message.origin.as_ref()?.get("disclosure")?.clone();
    serde_json::from_value(disclosure).ok()
}

fn origin_variant(message: &LLMMessage) -> Option<&str> {
    let origin = message.origin.as_ref()?;
    match origin.get("kind")?.as_str()? {
        "injection" => origin.get("variant")?.as_str(),
        "system_trigger" => origin.get("name")?.as_str(),
        _ => None,
    }
}

pub fn is_retrieved_tools_announcement(message: &LLMMessage) -> bool {
    origin_variant(message) == Some(RETRIEVED_TOOLS_VARIANT)
}

fn is_added_removed_announcement(message: &LLMMessage) -> bool {
    origin_variant(message) == Some(LOADABLE_TOOLS_VARIANT)
}

pub fn is_loadable_tools_announcement(message: &LLMMessage) -> bool {
    is_retrieved_tools_announcement(message) || is_added_removed_announcement(message)
}

pub fn is_dynamic_tool_schema_message(message: &LLMMessage) -> bool {
    !message.tools.is_empty()
}

pub fn collect_loaded_dynamic_tools(
    messages: &[LLMMessage],
    current: &[crate::tools::tool_select_ops::ToolSelectTool],
) -> std::collections::HashMap<String, kimi_core::types::ToolDescription> {
    use std::collections::HashMap;

    let mut loaded: HashMap<String, kimi_core::types::ToolDescription> = HashMap::new();
    for message in messages {
        if is_loadable_tools_announcement(message) {
            let disclosure = message.origin.as_ref().and_then(|origin| {
                if origin.get("kind").and_then(|kind| kind.as_str()) != Some("injection") {
                    return None;
                }
                serde_json::from_value::<ToolSelectAnnouncementDisclosure>(
                    origin.get("disclosure")?.clone(),
                )
                .ok()
            });
            let removed = match disclosure {
                Some(ToolSelectAnnouncementDisclosure::Delta { removed, .. }) => {
                    removed.into_iter().map(|tool| tool.name).collect()
                }
                _ => match_tool_name_blocks(&message.content, "tools_removed"),
            };
            for name in removed {
                loaded.remove(&name);
            }
            continue;
        }
        if !is_dynamic_tool_schema_message(message) {
            continue;
        }
        for tool in &message.tools {
            loaded.insert(tool.name.clone(), tool.clone());
        }
    }
    let current_by_name: HashMap<&str, &crate::tools::tool_select_ops::ToolSelectTool> = current
        .iter()
        .map(|tool| (tool.name.as_str(), tool))
        .collect();
    loaded.retain(|name, schema| match current_by_name.get(name.as_str()) {
        None => crate::native::tool_naming::is_mcp_tool_name(name),
        Some(tool) => {
            crate::tools::tool_select_ops::tool_description_schema_signature(schema)
                == crate::tools::tool_select_ops::tool_select_schema_signature(tool)
        }
    });
    loaded
}

pub fn strip_dynamic_tool_context(messages: Vec<LLMMessage>) -> Vec<LLMMessage> {
    let hidden_call_ids = messages
        .iter()
        .filter(|message| message.role == "assistant")
        .flat_map(|message| message.tool_calls.iter())
        .filter(|call| call.name == crate::tools::select_tools::SELECT_TOOLS_TOOL_NAME)
        .map(|call| call.id.clone())
        .collect::<std::collections::HashSet<_>>();
    let is_schema = is_dynamic_tool_schema_message;
    if hidden_call_ids.is_empty()
        && !messages
            .iter()
            .any(|message| is_loadable_tools_announcement(message) || is_schema(message))
    {
        return messages;
    }
    let mut out = Vec::new();
    for mut message in messages {
        if is_loadable_tools_announcement(&message) {
            let disclosure = message.origin.as_ref().and_then(|origin| {
                if origin.get("kind").and_then(|kind| kind.as_str()) != Some("injection") {
                    return None;
                }
                serde_json::from_value::<ToolSelectAnnouncementDisclosure>(
                    origin.get("disclosure")?.clone(),
                )
                .ok()
            });
            if matches!(
                disclosure,
                Some(ToolSelectAnnouncementDisclosure::Snapshot { .. })
            ) {
                continue;
            }
            let (added, removed) = match disclosure {
                Some(ToolSelectAnnouncementDisclosure::Delta { added, removed, .. }) => (
                    added.into_iter().map(|tool| tool.name).collect(),
                    removed.into_iter().map(|tool| tool.name).collect(),
                ),
                _ => (
                    match_tool_name_blocks(&message.content, "tools_added"),
                    match_tool_name_blocks(&message.content, "tools_removed"),
                ),
            };
            message.content = crate::injection::wrap_system_reminder(
                &render_runtime_tool_change_reminder(&added, &removed),
            );
            message.blocks.clear();
            out.push(message);
            continue;
        }
        if is_schema(&message) {
            if !message.content.is_empty()
                || !message.blocks.is_empty()
                || !message.tool_calls.is_empty()
            {
                message.tools.clear();
                out.push(message);
            }
            continue;
        }
        if message.role == "tool" {
            if message
                .tool_call_id
                .as_ref()
                .is_none_or(|id| !hidden_call_ids.contains(id))
            {
                out.push(message);
            }
            continue;
        }
        if message.role != "assistant" {
            out.push(message);
            continue;
        }
        let original_len = message.tool_calls.len();
        message
            .tool_calls
            .retain(|call| call.name != crate::tools::select_tools::SELECT_TOOLS_TOOL_NAME);
        if message.tool_calls.len() == original_len {
            out.push(message);
        } else if !message.tool_calls.is_empty()
            || !message.content.is_empty()
            || !message.blocks.is_empty()
        {
            out.push(message);
        }
    }
    out
}

pub fn fold_announced_tool_names(messages: &[LLMMessage]) -> std::collections::HashSet<String> {
    let mut announced = std::collections::HashSet::new();
    for message in messages {
        if !is_loadable_tools_announcement(message) {
            continue;
        }
        for name in match_tool_name_blocks(&message.content, "tools_removed") {
            announced.remove(&name);
        }
        for name in match_tool_name_blocks(&message.content, "tools_added") {
            announced.insert(name);
        }
    }
    announced
}

pub fn match_tool_name_blocks(text: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut names = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        let Some(end) = after.find(&close) else {
            break;
        };
        for line in after[..end].lines() {
            let name = line.trim();
            if !name.is_empty() {
                names.push(name.to_string());
            }
        }
        rest = &after[end + close.len()..];
    }
    names
}

pub fn render_retrieved_tools_announcement(
    entries: &[ToolSearchHit],
    total_loadable: usize,
) -> String {
    let lines: Vec<String> = entries
        .iter()
        .map(|entry| {
            let description = one_line(&entry.description);
            let description = slice_utf16(&description, 120);
            if description.is_empty() {
                entry.name.clone()
            } else {
                format!("{} — {description}", entry.name)
            }
        })
        .collect();
    let block = if lines.is_empty() {
        "<tools_relevant>\nNo tool descriptions matched the current request.\n</tools_relevant>"
            .to_string()
    } else {
        format!("<tools_relevant>\n{}\n</tools_relevant>", lines.join("\n"))
    };
    let guidance = if entries.is_empty() {
        format!(
            "{total_loadable} tools are loadable. Call select_tools with a query to search by \
capability, then call it again with exact names to load tools."
        )
    } else {
        format!(
            "These are the {} most relevant of {total_loadable} loadable tools. Call select_tools \
with exact names to load them. If the capability you need is not listed, call select_tools with \
a query to search all loadable tools; search results are candidates until you load them by exact \
name.",
            entries.len()
        )
    };
    format!("{block}\n\n{guidance} A newer <tools_relevant> block replaces older ones.")
}

fn one_line(text: &str) -> String {
    text.split(is_js_whitespace)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn slice_utf16(text: &str, max: usize) -> String {
    String::from_utf16_lossy(&text.encode_utf16().take(max).collect::<Vec<_>>())
}

fn is_js_whitespace(character: char) -> bool {
    matches!(
        character,
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

pub fn render_runtime_tool_change_reminder(added: &[String], removed: &[String]) -> String {
    let mut sections = Vec::new();
    if !added.is_empty() {
        sections.push(format!(
            "<tools_added>\n{}\n</tools_added>",
            added.join("\n")
        ));
    }
    if !removed.is_empty() {
        sections.push(format!(
            "<tools_removed>\n{}\n</tools_removed>",
            removed.join("\n")
        ));
    }
    sections.push(
        "The available tools changed during this session. The current model cannot load runtime tool additions with select_tools. Do not call select_tools. Use the tool definitions provided in this request. Removed tools may be unavailable or fail when called."
            .to_string(),
    );
    sections.join("\n\n")
}

pub fn strip_stale_retrieved_tools_announcements(messages: Vec<LLMMessage>) -> Vec<LLMMessage> {
    let count = messages
        .iter()
        .filter(|message| is_retrieved_tools_announcement(message))
        .count();
    if count <= 1 {
        return messages;
    }
    let last = messages
        .iter()
        .rposition(is_retrieved_tools_announcement)
        .expect("count > 1 implies a position");
    messages
        .into_iter()
        .enumerate()
        .filter(|(index, message)| *index == last || !is_retrieved_tools_announcement(message))
        .map(|(_, message)| message)
        .collect()
}

pub fn shape_history(messages: Vec<LLMMessage>, supports_dynamic_loading: bool) -> Vec<LLMMessage> {
    if supports_dynamic_loading {
        strip_stale_retrieved_tools_announcements(messages)
    } else {
        strip_dynamic_tool_context(messages)
    }
}

pub fn latest_user_query(messages: &[LLMMessage]) -> Option<String> {
    for message in messages.iter().rev() {
        if message.role != "user" {
            continue;
        }
        if let Some(origin) = &message.origin {
            if origin.get("kind").and_then(|kind| kind.as_str()) != Some("user") {
                continue;
            }
        }
        let text = message.content.trim();
        if !text.is_empty() {
            return Some(text.to_string());
        }
    }
    None
}

pub fn last_retrieved_announcement_text(messages: &[LLMMessage]) -> Option<String> {
    messages
        .iter()
        .rev()
        .find(|message| is_retrieved_tools_announcement(message))
        .map(|message| message.content.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::tool_retriever::ToolSearchHit;

    fn announcement(variant: &str, text: &str) -> LLMMessage {
        LLMMessage {
            role: "user".into(),
            content: crate::injection::wrap_system_reminder(text),
            origin: Some(serde_json::json!({ "kind": "injection", "variant": variant })),
            ..Default::default()
        }
    }

    fn hit(name: &str, description: &str) -> ToolSearchHit {
        ToolSearchHit {
            name: name.into(),
            description: description.into(),
            score: 1.0,
        }
    }

    #[test]
    fn fold_applies_removed_then_added_in_order() {
        let messages = vec![
            announcement(
                LOADABLE_TOOLS_VARIANT,
                "<tools_added>\nA\nB\n</tools_added>",
            ),
            announcement(
                LOADABLE_TOOLS_VARIANT,
                "<tools_removed>\nA\n</tools_removed>",
            ),
            announcement(LOADABLE_TOOLS_VARIANT, "<tools_added>\nC\n</tools_added>"),
        ];
        let announced = fold_announced_tool_names(&messages);
        assert!(announced.contains("B"));
        assert!(announced.contains("C"));
        assert!(!announced.contains("A"));
    }

    #[test]
    fn a_retrieved_block_is_recognized_by_content_and_stripped_to_the_last() {
        let messages = vec![
            announcement(
                RETRIEVED_TOOLS_VARIANT,
                "<tools_relevant>\nold — first\n</tools_relevant>",
            ),
            LLMMessage::user("continue"),
            announcement(
                RETRIEVED_TOOLS_VARIANT,
                "<tools_relevant>\nnew — second\n</tools_relevant>",
            ),
        ];
        assert!(is_retrieved_tools_announcement(&messages[0]));
        let stripped = strip_stale_retrieved_tools_announcements(messages);
        assert_eq!(stripped.len(), 2);
        assert!(stripped[1].content.contains("new — second"));
        assert_eq!(stripped[0].content, "continue");
    }

    #[test]
    fn a_variant_keyed_retrieval_block_is_recognized_without_the_marker() {
        let message = LLMMessage {
            role: "user".into(),
            content: "anything".into(),
            origin: Some(
                serde_json::json!({ "kind": "system_trigger", "name": RETRIEVED_TOOLS_VARIANT }),
            ),
            ..Default::default()
        };
        assert!(is_retrieved_tools_announcement(&message));
    }

    #[test]
    fn a_plain_user_message_is_not_an_announcement_even_when_it_quotes_one() {
        let message = LLMMessage::user("<tools_added>\nA\n</tools_added>");
        assert!(!is_loadable_tools_announcement(&message));
    }

    #[test]
    fn an_unknown_tool_select_variant_is_ignored() {
        let messages = vec![
            LLMMessage::user("hi"),
            announcement(
                LEGACY_TOOL_SELECT_VARIANT,
                "<tools_added>\nA\n</tools_added>",
            ),
        ];
        assert!(!fold_announced_tool_names(&messages).contains("A"));
        let stripped = strip_dynamic_tool_context(messages);
        assert_eq!(stripped.len(), 2);
        assert_eq!(stripped[0].content, "hi");
        assert!(stripped[1].content.contains("<tools_added>"));
    }

    #[test]
    fn closed_disclosure_converts_deltas_and_hides_select_tools_exchanges() {
        let select_call = crate::turn_loop::types::ToolCall {
            id: "select-1".to_string(),
            name: crate::tools::select_tools::SELECT_TOOLS_TOOL_NAME.to_string(),
            arguments: serde_json::json!({"names": ["mcp__s__alpha"]}),
            extras: None,
            args_parse_error: None,
        };
        let read_call = crate::turn_loop::types::ToolCall {
            id: "read-1".to_string(),
            name: "Read".to_string(),
            arguments: serde_json::json!({}),
            extras: None,
            args_parse_error: None,
        };
        let snapshot = LLMMessage {
            origin: Some(serde_json::json!({
                "kind": "injection",
                "variant": LOADABLE_TOOLS_VARIANT,
                "disclosure": {"kind": "snapshot", "revision": 0, "tools": []}
            })),
            ..announcement(LOADABLE_TOOLS_VARIANT, "snapshot")
        };
        let delta = LLMMessage {
            origin: Some(serde_json::json!({
                "kind": "injection",
                "variant": LOADABLE_TOOLS_VARIANT
            })),
            ..announcement(
                LOADABLE_TOOLS_VARIANT,
                "<tools_added>\nmcp__s__alpha\n</tools_added>\n\n<tools_removed>\nmcp__s__old\n</tools_removed>",
            )
        };
        let schema = LLMMessage {
            role: "system".into(),
            content: String::new(),
            tools: vec![kimi_core::types::ToolDescription {
                name: "mcp__s__alpha".into(),
                description: "alpha".into(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
                deferred: None,
            }],
            origin: Some(serde_json::json!({
                "kind": "injection",
                "variant": "dynamic_tool_schema"
            })),
            ..Default::default()
        };
        let messages = vec![
            snapshot,
            delta,
            schema,
            LLMMessage {
                role: "assistant".into(),
                tool_calls: vec![select_call, read_call],
                ..Default::default()
            },
            LLMMessage::tool_result("select-1", "Loaded"),
            LLMMessage::tool_result("read-1", "contents"),
        ];

        let stripped = strip_dynamic_tool_context(messages);
        assert_eq!(stripped.len(), 3);
        assert!(stripped[0].content.contains(
            "The available tools changed during this session. The current model cannot load runtime tool additions with select_tools."
        ));
        assert!(
            stripped[0]
                .content
                .contains("<tools_added>\nmcp__s__alpha\n</tools_added>")
        );
        assert!(
            stripped[0]
                .content
                .contains("<tools_removed>\nmcp__s__old\n</tools_removed>")
        );
        assert!(!stripped[0].content.contains("Use the select_tools tool"));
        assert_eq!(stripped[1].tool_calls.len(), 1);
        assert_eq!(stripped[1].tool_calls[0].id, "read-1");
        assert_eq!(stripped[2].tool_call_id.as_deref(), Some("read-1"));
    }

    #[test]
    fn retrieval_identity_uses_only_the_v2_variant_not_text_or_legacy_names() {
        let loadable = announcement(
            LOADABLE_TOOLS_VARIANT,
            "<tools_relevant>\nmcp__s__alpha\n</tools_relevant>",
        );
        assert!(!is_retrieved_tools_announcement(&loadable));
        assert!(is_loadable_tools_announcement(&loadable));

        let legacy = announcement(
            LEGACY_TOOL_SELECT_VARIANT,
            "<tools_added>\nmcp__s__alpha\n</tools_added>",
        );
        assert!(!is_loadable_tools_announcement(&legacy));
        assert!(fold_announced_tool_names(&[legacy]).is_empty());
    }

    #[test]
    fn rendering_matches_v2s_retrieval_text() {
        let rendered = render_retrieved_tools_announcement(
            &[hit("mcp__gh__search", "Search  issues\nand PRs")],
            42,
        );
        assert!(rendered.contains(
            "<tools_relevant>\nmcp__gh__search — Search issues and PRs\n</tools_relevant>"
        ));
        assert!(rendered.contains("These are the 1 most relevant of 42 loadable tools."));
        assert!(rendered.ends_with("A newer <tools_relevant> block replaces older ones."));

        let empty = render_retrieved_tools_announcement(&[], 3);
        assert!(empty.contains("No tool descriptions matched the current request."));
        assert!(empty.contains(
            "3 tools are loadable. Call select_tools with a query to search by capability"
        ));
    }

    #[test]
    fn latest_user_query_skips_injections_and_blank_prompts() {
        let messages = vec![
            LLMMessage::user("first"),
            announcement(LOADABLE_TOOLS_VARIANT, "<tools_added>\nA\n</tools_added>"),
            LLMMessage::user("second"),
            LLMMessage::user("   "),
            LLMMessage {
                role: "user".into(),
                content: "skill body".into(),
                origin: Some(serde_json::json!({ "kind": "skill_activation" })),
                ..Default::default()
            },
        ];
        assert_eq!(latest_user_query(&messages).as_deref(), Some("second"));
    }

    #[test]
    fn shaping_keeps_only_the_newest_retrieval_block_when_enabled() {
        let messages = vec![
            announcement(
                RETRIEVED_TOOLS_VARIANT,
                "<tools_relevant>\nold\n</tools_relevant>",
            ),
            LLMMessage::user("hi"),
            announcement(
                RETRIEVED_TOOLS_VARIANT,
                "<tools_relevant>\nnew\n</tools_relevant>",
            ),
        ];
        let shaped = shape_history(messages.clone(), true);
        assert_eq!(shaped.len(), 2);
        assert!(shaped.last().unwrap().content.contains("new"));

        let stripped = shape_history(messages, false);
        assert_eq!(stripped.len(), 3);
        assert!(
            stripped[0]
                .content
                .contains("The available tools changed during this session.")
        );
        assert_eq!(stripped[1].content, "hi");
        assert!(
            stripped[2]
                .content
                .contains("The available tools changed during this session.")
        );
    }
}
