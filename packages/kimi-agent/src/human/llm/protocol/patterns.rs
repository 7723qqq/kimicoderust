use crate::human::llm::message::{ContentPart, Message, ToolMessage};
use crate::human::llm::protocol::rewrite::{Pattern, Rewrite};
use crate::human::llm::requester::bases::tool_result_text::convert_tool_message_to_plain_text;

pub struct MergeUsersPolicy<T> {
    pub is_user: fn(&T) -> bool,
    pub is_tool_result_only: fn(&T) -> bool,
    pub merge: fn(T, T) -> T,
}

pub struct MergeConsecutiveUsers<T> {
    policy: MergeUsersPolicy<T>,
}

pub fn merge_consecutive_users<T>(policy: MergeUsersPolicy<T>) -> MergeConsecutiveUsers<T> {
    MergeConsecutiveUsers { policy }
}

impl<T> Pattern<T> for MergeConsecutiveUsers<T>
where
    T: Clone + Send + Sync,
{
    fn name(&self) -> &str {
        "mergeConsecutiveUsers"
    }

    fn rewrite(&self, items: &[T], index: usize) -> Option<Rewrite<T>> {
        let first = items.get(index)?;
        if !(self.policy.is_user)(first) {
            return None;
        }
        let mut acc = first.clone();
        let mut end = index + 1;
        while end < items.len() {
            let next = &items[end];
            if !(self.policy.is_user)(next) {
                break;
            }
            if !(self.policy.is_tool_result_only)(&acc) && (self.policy.is_tool_result_only)(next) {
                break;
            }
            acc = (self.policy.merge)(acc, next.clone());
            end += 1;
        }
        if end == index + 1 {
            return None;
        }
        Some(Rewrite {
            consumed: end - index,
            replacement: vec![acc],
        })
    }
}

pub struct ToolResultToPlainText;

impl Pattern<Message> for ToolResultToPlainText {
    fn name(&self) -> &str {
        "toolResultToPlainText"
    }

    fn rewrite(&self, items: &[Message], index: usize) -> Option<Rewrite<Message>> {
        let Some(Message::Tool(message)) = items.get(index) else {
            return None;
        };
        Some(Rewrite {
            consumed: 1,
            replacement: vec![Message::Tool(ToolMessage {
                role: message.role,
                tool_call_id: message.tool_call_id.clone(),
                content: std::iter::once(ContentPart::Text {
                    text: convert_tool_message_to_plain_text(message),
                    meta: None,
                })
                .chain(
                    message
                        .content
                        .iter()
                        .filter(|part| matches!(part, ContentPart::Think { .. }))
                        .cloned(),
                )
                .collect(),
            })],
        })
    }
}

pub static TOOL_RESULT_TO_PLAIN_TEXT: ToolResultToPlainText = ToolResultToPlainText;
