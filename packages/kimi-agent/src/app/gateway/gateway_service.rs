use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use kimi_core::primitives::errors::{Error2, Error2Options};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};

use crate::_base::di::scope::{IAgentScopeHandle, ScopeActivation, register_scoped_service};
use crate::_base::log::log::ILogService;
use crate::agent::context_memory::types::PromptOrigin;
use crate::agent::r#loop::r#loop::{IAgentLoopService, LoopSubmitOptions};
use crate::app::scopes::LifecycleScope;
use crate::app::session_manager::session_manager::ISessionManager;
use crate::human::agent::turn::{UserMeta, create_user_entry};
use crate::human::llm::message::{ContentPart, UserMessage};
use crate::agent::r#loop::r#loop::TextPart;
use crate::session::agent_lifecycle::agent_lifecycle::IAgentLifecycleService;
use crate::session::agent_lifecycle::errors::AGENT_NOT_FOUND;
use crate::session::errors::SESSION_NOT_FOUND;
use crate::wire::record::WireAsyncError;

use super::gateway::{IWSGateway, IRestGateway, PromptLaunchResult};

pub struct RestGateway {
    sessions: Arc<dyn ISessionManager>,
    log: Arc<dyn ILogService>,
}

impl RestGateway {
    pub fn new(sessions: Arc<dyn ISessionManager>, log: Arc<dyn ILogService>) -> Self {
        Self { sessions, log }
    }

    fn agent(
        &self,
        session_id: &str,
        agent_id: &str,
    ) -> Result<Arc<IAgentScopeHandle>, WireAsyncError> {
        let session = self.sessions.get(session_id).ok_or_else(|| {
            let mut details = Map::new();
            details.insert("sessionId".to_string(), Value::String(session_id.to_string()));
            let error: WireAsyncError = Box::new(Error2::with_options(
                SESSION_NOT_FOUND,
                format!("unknown session '{session_id}'"),
                Error2Options {
                    details: Some(details),
                    ..Default::default()
                },
            ));
            error
        })?;
        let agents = session.accessor().get::<IAgentLifecycleService>();
        agents.handle_of(agent_id).ok_or_else(|| {
            let mut details = Map::new();
            details.insert("agentId".to_string(), Value::String(agent_id.to_string()));
            details.insert("sessionId".to_string(), Value::String(session_id.to_string()));
            let error: WireAsyncError = Box::new(Error2::with_options(
                AGENT_NOT_FOUND,
                format!("unknown agent '{agent_id}'"),
                Error2Options {
                    details: Some(details),
                    ..Default::default()
                },
            ));
            error
        })
    }

    pub async fn prompt(
        &self,
        session_id: &str,
        agent_id: &str,
        input: &str,
    ) -> Result<Option<PromptLaunchResult>, WireAsyncError> {
        let loop_ = self.agent(session_id, agent_id)?.accessor().get::<IAgentLoopService>();
        let submitted = loop_.submit(user_entry(input)?, None);
        await_launch(&loop_, submitted.id).await
    }

    pub async fn steer(
        &self,
        session_id: &str,
        agent_id: &str,
        content: &str,
    ) -> Result<Option<PromptLaunchResult>, WireAsyncError> {
        let loop_ = self.agent(session_id, agent_id)?.accessor().get::<IAgentLoopService>();
        let submitted = loop_.submit(
            user_entry(content)?,
            Some(LoopSubmitOptions {
                steer_if_active: Some(true),
                on_materialize: None,
            }),
        );
        await_launch(&loop_, submitted.id).await
    }

    pub async fn cancel(
        &self,
        session_id: &str,
        agent_id: &str,
        reason: Option<String>,
    ) -> Result<(), WireAsyncError> {
        let loop_ = self.agent(session_id, agent_id)?.accessor().get::<IAgentLoopService>();
        loop_.cancel(
            None,
            reason.map(|reason| -> WireAsyncError {
                Box::new(std::io::Error::new(std::io::ErrorKind::Interrupted, reason))
            }),
        );
        Ok(())
    }

    pub async fn get_status(&self, session_id: &str) -> Value {
        Value::Bool(self.sessions.get(session_id).is_some())
    }

    pub async fn flush_logs(&self, session_id: &str) -> Result<(), WireAsyncError> {
        let Some(session) = self.sessions.get(session_id) else {
            return Ok(());
        };
        session.accessor().get::<ILogService>().flush().await
    }

    pub async fn flush_global_logs(&self) -> Result<(), WireAsyncError> {
        self.log.flush().await
    }
}

#[async_trait]
impl IRestGateway for RestGateway {
    async fn prompt(
        &self,
        session_id: &str,
        agent_id: &str,
        input: &str,
    ) -> Result<Option<PromptLaunchResult>, WireAsyncError> {
        RestGateway::prompt(self, session_id, agent_id, input).await
    }

    async fn steer(
        &self,
        session_id: &str,
        agent_id: &str,
        content: &str,
    ) -> Result<Option<PromptLaunchResult>, WireAsyncError> {
        RestGateway::steer(self, session_id, agent_id, content).await
    }

    async fn cancel(
        &self,
        session_id: &str,
        agent_id: &str,
        reason: Option<String>,
    ) -> Result<(), WireAsyncError> {
        RestGateway::cancel(self, session_id, agent_id, reason).await
    }

    async fn get_status(&self, session_id: &str) -> Value {
        RestGateway::get_status(self, session_id).await
    }

    async fn flush_logs(&self, session_id: &str) -> Result<(), WireAsyncError> {
        RestGateway::flush_logs(self, session_id).await
    }

    async fn flush_global_logs(&self) -> Result<(), WireAsyncError> {
        RestGateway::flush_global_logs(self).await
    }
}

async fn await_launch(
    loop_: &Arc<dyn IAgentLoopService>,
    id: String,
) -> Result<Option<PromptLaunchResult>, WireAsyncError> {
    let turn = match loop_.prompt_handle(&id) {
        Some(handle) => handle.launched.clone().await,
        None => None,
    };
    let Some(turn) = turn else {
        return Ok(None);
    };
    let _ = turn.ready.clone().await;
    Ok(turn.id.map(|turn_id| PromptLaunchResult { turn_id }))
}

fn user_entry(input: &str) -> Result<crate::agent::r#loop::machine::UserEntry, WireAsyncError> {
    let origin = serde_json::from_value::<PromptOrigin>(json!({"kind": "user"}))
        .map_err(|error| -> WireAsyncError { Box::new(error) })?;
    Ok(create_user_entry(
        UserMessage {
            role: "user".to_string(),
            content: vec![ContentPart::Text(TextPart {
                type_: "text".to_string(),
                text: input.to_string(),
            })],
        },
        UserMeta {
            origin: Some(origin),
            tracked: Some(true),
            ..Default::default()
        },
    ))
}

#[derive(Default)]
pub struct WSGateway {
    connections: Mutex<HashSet<String>>,
}

impl WSGateway {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn connect(&self, connection_id: &str) {
        self.connections.lock().insert(connection_id.to_string());
    }

    pub fn broadcast(&self, session_id: &str, event: Value) {
        std::hint::black_box((session_id, event));
    }
}

impl IWSGateway for WSGateway {
    fn connect(&self, connection_id: &str) {
        WSGateway::connect(self, connection_id)
    }

    fn broadcast(&self, session_id: &str, event: Value) {
        WSGateway::broadcast(self, session_id, event)
    }
}

pub fn register_gateway_services() {
    register_scoped_service(
        LifecycleScope::App,
        ScopeActivation::OnScopeCreated,
        "gateway",
    );
    register_scoped_service(
        LifecycleScope::App,
        ScopeActivation::OnScopeCreated,
        "gateway",
    );
}
