use std::sync::Arc;

use crate::features::reminder::reminder_service::ReminderService;
use crate::features::reminder::types::{
    ContextInjectionContent, ContextInjectionMessage, ContextInjectionResult,
};

use super::dynamic_tools::DYNAMIC_TOOL_SCHEMA_VARIANT;
use super::tool_select::IAgentToolSelectService;
use super::tool_select_schemas::IAgentToolSelectSchemasService;

pub struct AgentToolSelectSchemasService {
    _registration: Arc<dyn Fn() + Send + Sync>,
}

impl AgentToolSelectSchemasService {
    pub fn new(
        tool_select: Arc<dyn IAgentToolSelectService>,
        reminder: Arc<dyn ReminderService>,
    ) -> Self {
        let registration = reminder.register(
            DYNAMIC_TOOL_SCHEMA_VARIANT,
            Arc::new(move |_context| {
                let tool_select = tool_select.clone();
                Box::pin(async move {
                    Ok(tool_select.drain_pending_tool_schemas().map(|tools| {
                        ContextInjectionResult {
                            content: ContextInjectionContent::Message(ContextInjectionMessage {
                                role: "system".to_string(),
                                content: Vec::new(),
                                tools: Some(tools),
                            }),
                            disclosure: None,
                        }
                    }))
                })
            }),
        );
        Self {
            _registration: registration,
        }
    }
}

impl IAgentToolSelectSchemasService for AgentToolSelectSchemasService {}

impl Drop for AgentToolSelectSchemasService {
    fn drop(&mut self) {
        (self._registration)();
    }
}
