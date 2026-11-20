use std::sync::Arc;

use crate::features::reminder::reminder_service::ReminderService;
use crate::features::reminder::types::{ContextInjectionContent, ContextInjectionResult};

use super::dynamic_tools::{
    LOADABLE_TOOLS_VARIANT, is_tool_select_announcement_disclosure,
};
use super::tool_select::IAgentToolSelectService;
use super::tool_select_announcements::IAgentToolSelectAnnouncementsService;

pub struct AgentToolSelectAnnouncementsService {
    _registration: Arc<dyn Fn() + Send + Sync>,
}

impl AgentToolSelectAnnouncementsService {
    pub fn new(
        tool_select: Arc<dyn IAgentToolSelectService>,
        reminder: Arc<dyn ReminderService>,
    ) -> Self {
        let registration = reminder.register(
            LOADABLE_TOOLS_VARIANT,
            Arc::new(move |context| {
                let tool_select = tool_select.clone();
                Box::pin(async move {
                    if !tool_select.feature_enabled() {
                        return Ok(None);
                    }
                    tool_select
                        .clone()
                        .prepare_for_request(context.signal)
                        .await
                        .map_err(|error| error.to_string())?;
                    let last_disclosure = context
                        .last_disclosure
                        .as_ref()
                        .filter(|value| is_tool_select_announcement_disclosure(value))
                        .and_then(|value| {
                            serde_json::from_value(value.clone()).ok()
                        });
                    Ok(tool_select
                        .loadable_tools_announcement(last_disclosure)
                        .map(|announcement| {
                            ContextInjectionResult {
                                content: ContextInjectionContent::Text(announcement.content),
                                disclosure: serde_json::to_value(announcement.disclosure).ok(),
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

impl IAgentToolSelectAnnouncementsService for AgentToolSelectAnnouncementsService {}

impl Drop for AgentToolSelectAnnouncementsService {
    fn drop(&mut self) {
        (self._registration)();
    }
}
