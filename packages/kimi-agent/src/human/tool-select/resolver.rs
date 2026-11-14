use std::sync::Arc;

use async_trait::async_trait;

use crate::human::llm::message::Message;
use crate::human::llm::requester::actor::{MessageResolveContext, MessageResolver};
use crate::human::tool_select::state::ToolSelectState;
use crate::wire::record::WireAsyncError;

pub struct ToolSelectMessageResolver {
    state: Arc<ToolSelectState>,
}

#[async_trait]
impl MessageResolver for ToolSelectMessageResolver {
    fn id(&self) -> &str {
        "tool-select"
    }

    async fn resolve(
        &self,
        messages: Vec<Message>,
        _ctx: MessageResolveContext,
    ) -> Result<Vec<Message>, WireAsyncError> {
        let mut shaped: Option<Vec<Message>> = None;
        for (index, message) in messages.iter().enumerate() {
            match shape_message(message, &self.state) {
                Shape::Unchanged => {
                    if let Some(shaped) = shaped.as_mut() {
                        shaped.push(message.clone());
                    }
                }
                Shape::Replace(next) => {
                    if shaped.is_none() {
                        shaped = Some(messages[..index].to_vec());
                    }
                    shaped.as_mut().unwrap().push(next);
                }
                Shape::Remove => {
                    if shaped.is_none() {
                        shaped = Some(messages[..index].to_vec());
                    }
                }
            }
        }
        Ok(shaped.unwrap_or(messages))
    }
}

enum Shape {
    Unchanged,
    Replace(Message),
    Remove,
}

fn shape_message(message: &Message, state: &ToolSelectState) -> Shape {
    let Message::System(system) = message else {
        return Shape::Unchanged;
    };
    let Some(tools) = system.tools.as_ref().filter(|tools| !tools.is_empty()) else {
        return Shape::Unchanged;
    };
    let kept = if state.enabled() {
        tools
            .iter()
            .filter(|tool| state.is_loadable(&tool.name))
            .cloned()
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    if kept.len() == tools.len() {
        return Shape::Unchanged;
    }
    if !kept.is_empty() {
        let mut next = system.clone();
        next.tools = Some(kept);
        return Shape::Replace(Message::System(next));
    }
    if system.content.is_empty() {
        return Shape::Remove;
    }
    let mut next = system.clone();
    next.tools = None;
    Shape::Replace(Message::System(next))
}

pub fn create_tool_select_message_resolver(
    state: Arc<ToolSelectState>,
) -> ToolSelectMessageResolver {
    ToolSelectMessageResolver { state }
}
