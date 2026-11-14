use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Map, Value};

use crate::agent::prompt::prompt_metadata_text::prompt_metadata_text_from_content_parts;
use crate::agent::r#loop::r#loop::{
    IAgentLoopService, LoopState, LoopSubmitOptions, PromptLaunchResult, PromptPayload,
    PromptState, PromptTerminalState, SteerPayload, SteerResult, SteerState,
};
use crate::agent::scope_context::scope_context::IAgentScopeContext;
use crate::app::event::event::IEventService;
use crate::app::telemetry::telemetry::ITelemetryService;
use crate::human::agent::origin::USER_PROMPT_ORIGIN;
use crate::human::agent::turn::{UserMeta, create_user_entry};
use crate::human::llm::message::{Role, UserMessage};
use crate::session::agentLifecycle::agentLifecycle::MAIN_AGENT_ID;
use crate::session::sessionContext::sessionContext::ISessionContext;
use crate::session::sessionMetadata::promptMetadata::{
    PromptMetadataUpdateTarget, apply_prompt_metadata_update,
};
use crate::session::sessionMetadata::sessionMetadata::ISessionMetadata;
use crate::wire::record::WireAsyncError;
use kimi_core::primitives::errors::Error2;
use kimi_core::primitives::errors::domains::core::INTERNAL;

#[async_trait]
pub trait IAgentPromptChannel: Send + Sync {
    async fn submit(&self, payload: PromptPayload) -> Result<Option<PromptLaunchResult>, WireAsyncError>;
    async fn submit_steer(&self, payload: SteerPayload) -> Result<SteerResult, WireAsyncError>;
}

pub struct AgentPromptChannel {
    loop_: Arc<dyn IAgentLoopService>,
    scope_context: Arc<dyn IAgentScopeContext>,
    metadata: Arc<dyn ISessionMetadata>,
    event_service: Arc<dyn IEventService>,
    session_context: Arc<dyn ISessionContext>,
    telemetry: Arc<dyn ITelemetryService>,
}

impl AgentPromptChannel {
    pub fn new(
        loop_: Arc<dyn IAgentLoopService>,
        scope_context: Arc<dyn IAgentScopeContext>,
        metadata: Arc<dyn ISessionMetadata>,
        event_service: Arc<dyn IEventService>,
        session_context: Arc<dyn ISessionContext>,
        telemetry: Arc<dyn ITelemetryService>,
    ) -> Self {
        Self {
            loop_,
            scope_context,
            metadata,
            event_service,
            session_context,
            telemetry,
        }
    }

    async fn update_metadata(&self, text: Option<&str>) -> Result<(), WireAsyncError> {
        if self.scope_context.agent_id() != MAIN_AGENT_ID {
            return Ok(());
        }
        apply_prompt_metadata_update(
            &PromptMetadataUpdateTarget {
                metadata: self.metadata.clone(),
                event_service: self.event_service.clone(),
                session_id: self.session_context.session_id(),
            },
            text,
        )
        .await
    }
}

#[async_trait]
impl IAgentPromptChannel for AgentPromptChannel {
    async fn submit(&self, payload: PromptPayload) -> Result<Option<PromptLaunchResult>, WireAsyncError> {
        let metadata_text = prompt_metadata_text_from_content_parts(&payload.input, None);
        self.update_metadata(metadata_text.as_deref()).await?;
        let status = self.loop_.snapshot();
        let submitted = self.loop_.submit(
            create_user_entry(
                UserMessage {
                    role: Role::User,
                    content: payload.input,
                },
                UserMeta {
                    prompt_id: payload.prompt_id,
                    origin: Some(USER_PROMPT_ORIGIN.clone()),
                    tracked: Some(true),
                    ..Default::default()
                },
            ),
            None,
        );
        if status.state == LoopState::Running || status.paused || !status.queue.is_empty() {
            return Ok(None);
        }
        Ok(launch_result(&self.loop_, &submitted.id).await)
    }

    async fn submit_steer(&self, payload: SteerPayload) -> Result<SteerResult, WireAsyncError> {
        self.telemetry.track2(
            "input_steer",
            Some(Map::from_iter([(
                "parts".to_string(),
                Value::from(payload.input.len()),
            )])),
        );
        let metadata_text = prompt_metadata_text_from_content_parts(&payload.input, None);
        self.update_metadata(metadata_text.as_deref()).await?;
        let status = self.loop_.snapshot();
        let steerable = status.active_turn_id.is_some();
        let submitted = self.loop_.submit(
            create_user_entry(
                UserMessage {
                    role: Role::User,
                    content: payload.input,
                },
                UserMeta {
                    origin: Some(USER_PROMPT_ORIGIN.clone()),
                    tracked: Some(true),
                    ..Default::default()
                },
            ),
            Some(LoopSubmitOptions {
                steer_if_active: Some(true),
                on_materialize: None,
            }),
        );
        let handle = self.loop_.prompt_handle(&submitted.id).ok_or_else(|| {
            Box::new(Error2::new(INTERNAL, "Steer failed to launch a turn")) as WireAsyncError
        })?;
        if !steerable && (status.state == LoopState::Running || status.paused || !status.queue.is_empty()) {
            return Ok(SteerResult {
                turn_id: None,
                prompt_id: submitted.id,
                created_at: handle.created_at,
                state: SteerState::Queued,
                steered: Some(false),
            });
        }
        let launched = handle.launched.clone();
        let completion = handle.completion.clone();
        let completed = tokio::select! {
            _ = launched => None,
            completion = completion => Some(completion),
        };
        let turn = handle.launched.clone().await;
        let Some(turn) = turn else {
            let terminal = completed.map(|completion| completion.state);
            if terminal == Some(PromptTerminalState::Blocked) || handle.state == PromptState::Blocked {
                return Ok(SteerResult {
                    turn_id: None,
                    prompt_id: submitted.id,
                    created_at: handle.created_at,
                    state: SteerState::Blocked,
                    steered: Some(false),
                });
            }
            return Err(Box::new(Error2::new(INTERNAL, "Steer failed to launch a turn")));
        };
        let _ = turn.ready.clone().await;
        Ok(SteerResult {
            turn_id: turn.id,
            prompt_id: submitted.id,
            created_at: handle.created_at,
            state: SteerState::Running,
            steered: Some(steerable && turn.id == status.active_turn_id),
        })
    }
}

async fn launch_result(
    loop_: &Arc<dyn IAgentLoopService>,
    id: &str,
) -> Option<PromptLaunchResult> {
    let turn = loop_.prompt_handle(id)?.launched.clone().await?;
    let _ = turn.ready.clone().await;
    turn.id.map(|turn_id| PromptLaunchResult { turn_id })
}
