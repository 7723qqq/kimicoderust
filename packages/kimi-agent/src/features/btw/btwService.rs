use crate::_base::di::instantiation::ServicesAccessorExt;
use crate::features::reminder::reminder_service::I_AGENT_REMINDER_SERVICE;
use crate::agent::scope_context::scope_context::I_AGENT_SCOPE_CONTEXT;
use crate::agent::tool_approval::tool_approval::I_AGENT_TOOL_APPROVAL_SERVICE;
use crate::agent::tool_executor::tool_executor::I_AGENT_TOOL_EXECUTOR_SERVICE;
use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use kimi_core::primitives::errors::Error2;
use parking_lot::Mutex;

use super::btw::{
    BTW_AGENT_LABEL, BTW_READONLY_TOOLS, ISessionBtwService, SIDE_QUESTION_SYSTEM_REMINDER,
    TOOL_CALL_DISABLED_MESSAGE,
};
use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::agent::scope_context::scopeContext::IAgentScopeContext;
use crate::agent::tool_approval::tool_approval::IAgentToolApprovalService;
use crate::agent::tool_executor::before_tool_execute_event::deny_tool_execution;
use crate::agent::tool_executor::tool_executor::IAgentToolExecutorService;
use crate::features::reminder::reminderService::ReminderService;
use crate::features::reminder::types::ReminderNotification;
use crate::session::agentLifecycle::agentLifecycle::{
    ForkAgentOptions, IAgentLifecycleService, MAIN_AGENT_ID,
};
use crate::session::agentLifecycle::errors::AGENT_NOT_FOUND;
use crate::wire::record::WireAsyncError;

pub struct SessionBtwService {
    agent_lifecycle: Arc<dyn IAgentLifecycleService>,
    agent_subscriptions: Arc<Mutex<HashMap<String, Vec<DisposableRef>>>>,
    subscriptions: Mutex<Vec<DisposableRef>>,
}

impl SessionBtwService {
    pub fn new(agent_lifecycle: Arc<dyn IAgentLifecycleService>) -> Self {
        let agent_subscriptions: Arc<Mutex<HashMap<String, Vec<DisposableRef>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let service = Self {
            agent_lifecycle: agent_lifecycle.clone(),
            agent_subscriptions: agent_subscriptions.clone(),
            subscriptions: Mutex::new(Vec::new()),
        };
        let registry = agent_subscriptions.clone();
        let create_subscription = agent_lifecycle.on_did_create_scope(Arc::new(move |event| {
            let is_btw = event
                .meta
                .as_ref()
                .and_then(|meta| meta.labels.as_ref())
                .and_then(|labels| labels.get(BTW_AGENT_LABEL))
                .is_some_and(|value| value == "true");
            if !is_btw {
                return;
            }
            let reason = event
                .handle
                .accessor()
                .get(&I_AGENT_TOOL_APPROVAL_SERVICE)
                .format_deny_message(TOOL_CALL_DISABLED_MESSAGE);
            let tool_executor = event.handle.accessor().get(&I_AGENT_TOOL_EXECUTOR_SERVICE);
            let subscription = tool_executor.on_before_execute_tool().subscribe(
                move |event: Arc<crate::agent::tool_executor::before_tool_execute_event::BeforeToolExecuteEventImpl>| {
                    if !BTW_READONLY_TOOLS.contains(&event.context.base.tool_call.name.as_str()) {
                        event.veto(deny_tool_execution(&reason));
                    }
                },
            );
            registry
                .lock()
                .entry(event.handle.id().to_string())
                .or_default()
                .push(Box::new(subscription));
        }));
        let close_registry = agent_subscriptions;
        let close_subscription = service.agent_lifecycle.on_did_close(Arc::new(move |agent| {
            close_registry.lock().remove(&agent.agent_id);
        }));
        service.subscriptions.lock().push(create_subscription);
        service.subscriptions.lock().push(close_subscription);
        service
    }

    pub async fn start(&self) -> Result<String, WireAsyncError> {
        let main = self
            .agent_lifecycle
            .handle_of(MAIN_AGENT_ID)
            .ok_or_else(|| {
                Box::new(Error2::new(AGENT_NOT_FOUND, "Main agent was not found"))
                    as WireAsyncError
            })?;
        let source = main
            .accessor()
            .get(&I_AGENT_SCOPE_CONTEXT)
            .agent_context();
        let child_context = self
            .agent_lifecycle
            .clone()
            .fork(
                source,
                Some(ForkAgentOptions {
                    labels: Some(HashMap::from([(
                        BTW_AGENT_LABEL.to_string(),
                        "true".to_string(),
                    )])),
                    ..Default::default()
                }),
            )
            .await?;
        if let Some(child) = self.agent_lifecycle.handle_of(&child_context.agent_id) {
            child
                .accessor()
                .get(&I_AGENT_REMINDER_SERVICE)
                .notify(
                    SIDE_QUESTION_SYSTEM_REMINDER,
                    &ReminderNotification {
                        variant: BTW_AGENT_LABEL.to_string(),
                        owner_prompt_id: None,
                    },
                );
        }
        Ok(child_context.agent_id.clone())
    }
}

#[async_trait]
impl ISessionBtwService for SessionBtwService {
    async fn start(&self) -> Result<String, WireAsyncError> {
        SessionBtwService::start(self).await
    }
}
