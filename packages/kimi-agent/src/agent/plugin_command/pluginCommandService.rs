use std::sync::Arc;

use async_trait::async_trait;
use kimi_core::primitives::errors::domains::prompt::REQUEST_INVALID;
use kimi_core::primitives::errors::Error2;

use crate::agent::r#loop::r#loop::IAgentLoopService;
use crate::agent::plugin_command::plugin_command::{
    ActivatePluginCommandPayload, IAgentPluginCommandService, PluginCommandActivated,
    PluginCommandTrigger,
};
use crate::agent::prompt::prompt_metadata_text::prompt_metadata_text_from_text;
use crate::agent::scope_context::scope_context::IAgentScopeContext;
use crate::app::event::event::IEventService;
use crate::app::plugin::commands::expand_command_arguments;
use crate::app::plugin::plugin::IPluginService;
use crate::human::agent::origin::PromptOrigin;
use crate::human::agent::turn::{create_user_entry, UserMeta};
use crate::human::llm::message::{ContentPart, Role, UserMessage};
use crate::session::agentLifecycle::agentLifecycle::MAIN_AGENT_ID;
use crate::session::sessionContext::sessionContext::ISessionContext;
use crate::session::sessionMetadata::prompt_metadata::{
    apply_prompt_metadata_update, PromptMetadataUpdateTarget,
};
use crate::session::sessionMetadata::session_metadata::ISessionMetadata;
use crate::state::event_dispatcher::IEventDispatcher;
use crate::wire::record::WireAsyncError;

pub struct AgentPluginCommandService {
    plugins: Arc<dyn IPluginService>,
    loop_: Arc<dyn IAgentLoopService>,
    dispatcher: Arc<dyn IEventDispatcher>,
    metadata: Arc<dyn ISessionMetadata>,
    event_service: Arc<dyn IEventService>,
    session_context: Arc<dyn ISessionContext>,
    scope_context: Arc<dyn IAgentScopeContext>,
}

impl AgentPluginCommandService {
    pub fn new(
        plugins: Arc<dyn IPluginService>,
        loop_: Arc<dyn IAgentLoopService>,
        dispatcher: Arc<dyn IEventDispatcher>,
        metadata: Arc<dyn ISessionMetadata>,
        event_service: Arc<dyn IEventService>,
        session_context: Arc<dyn ISessionContext>,
        scope_context: Arc<dyn IAgentScopeContext>,
    ) -> Self {
        Self {
            plugins,
            loop_,
            dispatcher,
            metadata,
            event_service,
            session_context,
            scope_context,
        }
    }

    async fn activate_inner(&self, payload: ActivatePluginCommandPayload) -> Result<(), WireAsyncError> {
        let commands = self.plugins.list_plugin_commands().await?;
        let Some(definition) = commands.into_iter().find(|command| {
            command.plugin_id == payload.plugin_id && command.name == payload.command_name
        }) else {
            return Err(Error2::new(
                REQUEST_INVALID,
                format!(
                    "Plugin command \"{}:{}\" was not found",
                    payload.plugin_id, payload.command_name
                ),
            )
            .into());
        };
        let command_args = payload.args.clone();
        let expanded = expand_command_arguments(
            &definition.body,
            payload.args.as_deref().unwrap_or(""),
        );
        let activation_id = uuid::Uuid::new_v4().to_string();
        self.dispatcher
            .dispatch(
                PluginCommandActivated {
                    agent_id: self.scope_context.agent_id(),
                    activation_id: activation_id.clone(),
                    plugin_id: payload.plugin_id.clone(),
                    command_name: payload.command_name.clone(),
                    command_args: command_args.clone(),
                    trigger: PluginCommandTrigger::UserSlash,
                }
                .into(),
            )
            .await?;
        self.loop_.submit(
            create_user_entry(
                UserMessage {
                    role: Role::User,
                    content: vec![ContentPart::Text {
                        text: expanded,
                        meta: None,
                    }],
                },
                UserMeta {
                    origin: Some(PromptOrigin {
                        kind: "plugin_command".to_string(),
                        ..Default::default()
                    }),
                    tracked: Some(true),
                    ..Default::default()
                },
            ),
            None,
        );
        if self.scope_context.agent_id() == MAIN_AGENT_ID {
            let text = prompt_metadata_text_from_plugin_command(&payload);
            apply_prompt_metadata_update(
                &PromptMetadataUpdateTarget {
                    metadata: self.metadata.clone(),
                    event_service: self.event_service.clone(),
                    session_id: self.session_context.session_id(),
                },
                text.as_deref(),
            )
            .await?;
        }
        Ok(())
    }
}

#[async_trait]
impl IAgentPluginCommandService for AgentPluginCommandService {
    async fn activate(&self, payload: ActivatePluginCommandPayload) -> Result<(), WireAsyncError> {
        self.activate_inner(payload).await
    }
}

fn prompt_metadata_text_from_plugin_command(
    payload: &ActivatePluginCommandPayload,
) -> Option<String> {
    let args = payload.args.as_deref().map(str::trim);
    let command = format!("/{}:{}", payload.plugin_id, payload.command_name);
    prompt_metadata_text_from_text(&match args {
        Some(args) if !args.is_empty() => format!("{command} {args}"),
        _ => command,
    })
}
