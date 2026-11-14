use std::sync::Arc;

use crate::human::agent::machine::AgentEmitted;
use crate::human::agent::turn::HistoryMessage;
use crate::human::llm::message::{Role, SystemMessage, create_user_message};
use crate::human::plugin::{Plugin, PluginMessage, PluginTarget};
use crate::human::tool_select::state::ToolSelectState;
use crate::human::tool_select::tool::create_select_tools_tool;

pub const LOADABLE_TOOLS_REMINDER_KEY: &str = "loadable-tools";
pub const DYNAMIC_TOOL_SCHEMA_REMINDER_KEY: &str = "dynamic-tool-schemas";

pub struct ToolSelectPlugin {
    state: Arc<ToolSelectState>,
}

impl Plugin for ToolSelectPlugin {
    fn name(&self) -> &str {
        "tool-select"
    }

    fn tools(&self) -> Vec<crate::human::tool::tool::ToolDefinition> {
        vec![
            create_select_tools_tool(self.state.clone())
                .expect("select_tools has a valid tool name"),
        ]
    }

    fn connect(&self, target: PluginTarget) {
        if target.kind != "agent" {
            return;
        }
        let started_state = self.state.clone();
        let started_target = target.clone();
        (target.on)(
            "turn.started".to_string(),
            Arc::new(move |event| {
                if !matches!(event, AgentEmitted::TurnStarted { .. }) || !started_state.enabled() {
                    return;
                }
                let Some(announcement) = started_state.announcement() else {
                    return;
                };
                (started_target.remind)(
                    LOADABLE_TOOLS_REMINDER_KEY.to_string(),
                    PluginMessage::User(create_user_message(&announcement)),
                );
            }),
        );
        let push_schemas = {
            let state = self.state.clone();
            let target = target.clone();
            Arc::new(move || {
                if !state.enabled() {
                    return;
                }
                let tools = state.pending_schemas();
                if tools.is_empty() {
                    return;
                }
                (target.remind)(
                    DYNAMIC_TOOL_SCHEMA_REMINDER_KEY.to_string(),
                    PluginMessage::System(SystemMessage {
                        role: Role::System,
                        content: Vec::new(),
                        tools: Some(tools),
                    }),
                );
            })
        };
        for event_name in ["tool.done", "tool.failed"] {
            let push_schemas = push_schemas.clone();
            (target.on)(
                event_name.to_string(),
                Arc::new(move |event| {
                    if matches!(
                        event,
                        AgentEmitted::ToolDone { .. } | AgentEmitted::ToolFailed { .. }
                    ) {
                        push_schemas();
                    }
                }),
            );
        }
        let consumed_state = self.state.clone();
        (target.on)(
            "turn.reminders_consumed".to_string(),
            Arc::new(move |event| {
                let AgentEmitted::TurnRemindersConsumed { reminders } = event else {
                    return;
                };
                for entry in reminders {
                    match reminder_key(&entry).as_deref() {
                        Some(LOADABLE_TOOLS_REMINDER_KEY) => consumed_state.mark_announced(),
                        Some(DYNAMIC_TOOL_SCHEMA_REMINDER_KEY) => {
                            consumed_state.mark_schemas_landed()
                        }
                        _ => {}
                    }
                }
            }),
        );
        let reset_state = self.state.clone();
        (target.on)(
            "context.reset".to_string(),
            Arc::new(move |event| {
                if matches!(event, AgentEmitted::ContextReset { .. }) {
                    reset_state.reset();
                }
            }),
        );
    }
}

fn reminder_key(entry: &HistoryMessage) -> Option<String> {
    match entry {
        HistoryMessage::System(entry) => entry.meta.as_ref().and_then(|meta| meta.key.clone()),
        HistoryMessage::User(entry) => entry.meta.as_ref().and_then(|meta| meta.key.clone()),
        HistoryMessage::Assistant(entry) => entry.meta.as_ref().and_then(|meta| meta.key.clone()),
        HistoryMessage::Tool(entry) => entry.meta.as_ref().and_then(|meta| meta.key.clone()),
    }
}

pub fn create_tool_select_plugin(state: Arc<ToolSelectState>) -> ToolSelectPlugin {
    ToolSelectPlugin { state }
}
