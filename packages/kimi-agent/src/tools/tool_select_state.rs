use crate::tools::tool_retriever::ToolSearchHit;
use crate::turn_loop::types::LLMMessage;

pub const LOADABLE_TOOLS_VARIANT: &str = "loadable-tools";

pub const RETRIEVED_TOOLS_VARIANT: &str = "retrieved-tools";

const LEGACY_TOOL_SELECT_VARIANT: &str = "tool_select";

fn origin_variant(message: &LLMMessage) -> Option<&str> {
    let origin = message.origin.as_ref()?;
    match origin.get("kind")?.as_str()? {
        "injection" => origin.get("variant")?.as_str(),
        "system_trigger" => origin.get("name")?.as_str(),
        _ => None,
    }
}

pub fn is_retrieved_tools_announcement(message: &LLMMessage) -> bool {
    if origin_variant(message) == Some(RETRIEVED_TOOLS_VARIANT) {
        return true;
    }
    matches!(
        origin_variant(message),
        Some(LOADABLE_TOOLS_VARIANT) | Some(LEGACY_TOOL_SELECT_VARIANT)
    ) && message.content.contains("<tools_relevant>")
}

fn is_added_removed_announcement(message: &LLMMessage) -> bool {
    matches!(
        origin_variant(message),
        Some(LOADABLE_TOOLS_VARIANT) | Some(LEGACY_TOOL_SELECT_VARIANT)
    )
}

pub fn is_loadable_tools_announcement(message: &LLMMessage) -> bool {
    is_retrieved_tools_announcement(message) || is_added_removed_announcement(message)
}

pub fn strip_dynamic_tool_context(messages: Vec<LLMMessage>) -> Vec<LLMMessage> {
    if !messages.iter().any(is_loadable_tools_announcement) {
        return messages;
    }
    messages
        .into_iter()
        .filter(|message| !is_loadable_tools_announcement(message))
        .collect()
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
            let description: String = one_line(&entry.description).chars().take(120).collect();
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
    text.split_whitespace().collect::<Vec<_>>().join(" ")
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
        .map(|message| {
            message
                .content
                .strip_prefix(crate::injection::SYSTEM_REMINDER_PREFIX)
                .and_then(|text| text.strip_suffix(crate::injection::SYSTEM_REMINDER_SUFFIX))
                .unwrap_or(&message.content)
                .to_string()
        })
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
                LOADABLE_TOOLS_VARIANT,
                "<tools_relevant>\nold — first\n</tools_relevant>",
            ),
            LLMMessage::user("continue"),
            announcement(
                LOADABLE_TOOLS_VARIANT,
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
    fn the_legacy_tool_select_variant_still_folds_and_strips() {
        let messages = vec![
            LLMMessage::user("hi"),
            announcement(
                LEGACY_TOOL_SELECT_VARIANT,
                "<tools_added>\nA\n</tools_added>",
            ),
        ];
        assert!(fold_announced_tool_names(&messages).contains("A"));
        let stripped = strip_dynamic_tool_context(messages);
        assert_eq!(stripped.len(), 1);
        assert_eq!(stripped[0].content, "hi");
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
                LOADABLE_TOOLS_VARIANT,
                "<tools_relevant>\nold\n</tools_relevant>",
            ),
            LLMMessage::user("hi"),
            announcement(
                LOADABLE_TOOLS_VARIANT,
                "<tools_relevant>\nnew\n</tools_relevant>",
            ),
        ];
        let shaped = shape_history(messages.clone(), true);
        assert_eq!(shaped.len(), 2);
        assert!(shaped.last().unwrap().content.contains("new"));

        let stripped = shape_history(messages, false);
        assert_eq!(stripped.len(), 1);
        assert_eq!(stripped[0].content, "hi");
    }
}
