use std::collections::{HashMap, HashSet};

use crate::human::llm::message::{Message, ToolMessage};
use crate::human::llm::protocol::rewrite::{Pattern, Rewrite};
use crate::human::llm::syntax_errors::SyntaxRequestFormatError;

#[derive(Debug, Clone, Copy)]
pub struct SortToolRunByCallOrder;

pub const SORT_TOOL_RUN_BY_CALL_ORDER: SortToolRunByCallOrder = SortToolRunByCallOrder;

impl Pattern<Message> for SortToolRunByCallOrder {
    fn name(&self) -> &str {
        "sortToolRunByCallOrder"
    }

    fn rewrite(&self, items: &[Message], index: usize) -> Option<Rewrite<Message>> {
        let Some(Message::Assistant(assistant)) = items.get(index) else {
            return None;
        };
        if assistant.tool_calls.is_empty() {
            return None;
        }
        let mut end = index + 1;
        while end < items.len() && matches!(items.get(end), Some(Message::Tool(_))) {
            end += 1;
        }
        if end == index + 1 {
            return None;
        }
        let run = items[index + 1..end]
            .iter()
            .map(|message| match message {
                Message::Tool(message) => message.clone(),
                _ => unreachable!(),
            })
            .collect::<Vec<ToolMessage>>();
        let run_ids = run
            .iter()
            .map(|message| message.tool_call_id.clone())
            .collect::<Vec<_>>();
        let mut tool_msg_by_id = HashMap::new();
        let mut seen_tool_call_ids = HashSet::new();
        for tool_msg in &run {
            if !seen_tool_call_ids.insert(tool_msg.tool_call_id.clone()) {
                let error = SyntaxRequestFormatError::new(format!(
                    "Duplicate tool response for id: {}",
                    tool_msg.tool_call_id
                ));
                std::panic::panic_any(error);
            }
            tool_msg_by_id.insert(tool_msg.tool_call_id.clone(), tool_msg.clone());
        }
        let mut sorted = Vec::new();
        for tool_call in &assistant.tool_calls {
            let Some(msg) = tool_msg_by_id.get(&tool_call.id).cloned() else {
                let error = SyntaxRequestFormatError::new(format!(
                    "Missing tool responses for ids: {}",
                    tool_call.id
                ));
                std::panic::panic_any(error);
            };
            sorted.push(msg);
            tool_msg_by_id.remove(&tool_call.id);
        }
        if !tool_msg_by_id.is_empty() {
            let unexpected = run_ids
                .iter()
                .filter(|id| tool_msg_by_id.contains_key(*id))
                .cloned()
                .collect::<Vec<_>>();
            let error = SyntaxRequestFormatError::new(format!(
                "Unexpected tool responses for ids: {}",
                serde_json::to_string(&unexpected).unwrap()
            ));
            std::panic::panic_any(error);
        }
        let sorted_ids = sorted
            .iter()
            .map(|message| message.tool_call_id.clone())
            .collect::<Vec<_>>();
        if run_ids == sorted_ids {
            return None;
        }
        let mut replacement = Vec::with_capacity(1 + sorted.len());
        replacement.push(Message::Assistant(assistant.clone()));
        replacement.extend(sorted.into_iter().map(Message::Tool));
        Some(Rewrite {
            consumed: 1 + run.len(),
            replacement,
        })
    }
}
