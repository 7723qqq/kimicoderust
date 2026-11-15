use std::collections::HashMap;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::_base::utils::abort::abortable;
use crate::agent::profile::profile::IAgentProfileService;
use crate::agent::scope_context::scope_context::IAgentScopeContext;
use crate::agent::state::agent_state::IAgentStateService;
use crate::agent::tool_executor::tool_executor::{
    ExecutableTool, ExecutableToolContext, ExecutableToolOutput, ExecutableToolResult,
    RunnableToolExecution, ToolExecution,
};
use crate::agent::tool_registry::tool_registry::{
    IAgentToolRegistryService, ToolRegistrationOptions,
};
use crate::hooks::HookRegisterOptions;
use crate::human::interaction::facade::interactions;
use crate::human::interaction::interaction::{
    INTERACTION_TAG_AGENT_ID, INTERACTION_TAG_SESSION_ID, INTERACTION_TAG_TOOL_CALL_ID,
    INTERACTION_TAG_TURN_ID, InteractionKind, InteractionRequest, InteractionTags,
};
use crate::human::llm::message::ContentPart;
use crate::session::session_context::session_context::ISessionContext;
use crate::state::event_dispatcher::IEventDispatcher;
use crate::tool::tool_contract::{ToolDelivery, ToolResultSpill, ToolSource};
use crate::wire::record::WireAsyncError;

use super::user_tool::{IAgentUserToolService, UserToolRegistration};
use super::user_tool_ops::{
    ToolsRegisterUserTool, ToolsRegisterUserToolPayload, ToolsUnregisterUserTool,
    ToolsUnregisterUserToolPayload, UserToolModelState, user_tool_key,
};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UserToolExecutionRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    turn_id: Option<u64>,
    tool_call_id: String,
    name: String,
    args: Value,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum UserToolExecutionOutput {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UserToolResultSpill {
    output_path: Option<String>,
    total_chars: Option<usize>,
    suffix: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UserToolExecutionResponse {
    output: UserToolExecutionOutput,
    is_error: Option<bool>,
    stop_turn: Option<bool>,
    stop_turn_reason: Option<String>,
    truncated: Option<bool>,
    note: Option<String>,
    delivery: Option<ToolDelivery>,
    spill: Option<UserToolResultSpill>,
    spill_exempt: Option<bool>,
}

impl From<UserToolExecutionResponse> for ExecutableToolResult {
    fn from(response: UserToolExecutionResponse) -> Self {
        Self {
            output: match response.output {
                UserToolExecutionOutput::Text(output) => ExecutableToolOutput::Text(output),
                UserToolExecutionOutput::Parts(output) => ExecutableToolOutput::Parts(output),
            },
            is_error: response.is_error,
            stop_turn: response.stop_turn,
            stop_turn_reason: response.stop_turn_reason,
            truncated: response.truncated,
            note: response.note,
            delivery: response.delivery,
            spill: response.spill.map(|spill| ToolResultSpill {
                output_path: spill.output_path,
                total_chars: spill.total_chars,
                suffix: spill.suffix,
            }),
            spill_exempt: response.spill_exempt,
        }
    }
}

fn execute_user_tool(
    scope_context: Arc<dyn IAgentScopeContext>,
    session: Arc<dyn ISessionContext>,
    context: ExecutableToolContext,
    name: String,
    args: Value,
) -> BoxFuture<'static, Result<ExecutableToolResult, WireAsyncError>> {
    let id = format!("user_tool_{}", uuid::Uuid::new_v4());
    let mut tags = InteractionTags::new();
    tags.insert(
        INTERACTION_TAG_AGENT_ID.to_string(),
        scope_context.agent_id().into(),
    );
    tags.insert(
        INTERACTION_TAG_SESSION_ID.to_string(),
        session.session_id().into(),
    );
    tags.insert(
        INTERACTION_TAG_TOOL_CALL_ID.to_string(),
        context.tool_call_id.clone().into(),
    );
    tags.insert(
        INTERACTION_TAG_TURN_ID.to_string(),
        context.turn_id.into(),
    );
    let request = InteractionRequest {
        id: Some(id.clone()),
        kind: InteractionKind::UserTool,
        payload: serde_json::to_value(UserToolExecutionRequest {
            turn_id: Some(context.turn_id),
            tool_call_id: context.tool_call_id,
            name: name.clone(),
            args,
        })
        .expect("user tool execution request is serializable"),
        tags: Some(tags),
    };
    let interactions = interactions();
    if let Err(error) = interactions.enqueue(request) {
        return Box::pin(async move { Err(error) });
    }
    let signal = context.signal;
    Box::pin(async move {
        let wait_interactions = interactions.clone();
        let wait_id = id.clone();
        let wait = async move { wait_interactions.wait(&wait_id, None).await };
        let result = abortable(wait, signal.clone()).await;
        match result {
            Ok(response) => serde_json::from_value::<UserToolExecutionResponse>(response)
                .map(ExecutableToolResult::from)
                .map_err(|error| Box::new(error) as WireAsyncError),
            Err(error) => {
                if signal.aborted() {
                    interactions.respond(
                        &id,
                        json!({
                            "output": format!("User tool \"{name}\" was aborted."),
                            "isError": true,
                        }),
                    );
                }
                Err(error)
            }
        }
    })
}

struct UserTool {
    scope_context: Arc<dyn IAgentScopeContext>,
    session: Arc<dyn ISessionContext>,
    name: String,
    description: String,
    parameters: Map<String, Value>,
}

impl ExecutableTool for UserTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Map<String, Value> {
        self.parameters.clone()
    }

    fn resolve_execution(
        &self,
        input: Value,
    ) -> BoxFuture<'static, Result<ToolExecution, WireAsyncError>> {
        let scope_context = self.scope_context.clone();
        let session = self.session.clone();
        let name = self.name.clone();
        Box::pin(async move {
            Ok(ToolExecution::Runnable(RunnableToolExecution {
                accesses: None,
                display: None,
                description: None,
                stop_batch_after_this: None,
                approval_rule: name.clone(),
                matches_rule: None,
                execute: Arc::new(move |context| {
                    let scope_context = scope_context.clone();
                    let session = session.clone();
                    let name = name.clone();
                    let args = input.clone();
                    execute_user_tool(scope_context, session, context, name, args)
                }),
            }))
        })
    }
}

struct AgentUserToolServiceInner {
    scope_context: Arc<dyn IAgentScopeContext>,
    registry: Arc<dyn IAgentToolRegistryService>,
    profile: Arc<dyn IAgentProfileService>,
    session: Arc<dyn ISessionContext>,
    dispatcher: Arc<dyn IEventDispatcher>,
    agent_state: Arc<dyn IAgentStateService>,
    registrations: Mutex<HashMap<String, DisposableRef>>,
    subscriptions: Mutex<Vec<DisposableRef>>,
}

impl AgentUserToolServiceInner {
    fn registrations(&self) -> UserToolModelState {
        serde_json::from_value(self.agent_state.get(&user_tool_key().state_key()))
            .expect("user tool state must match its schema")
    }

    fn dispatch_detached(&self, event: crate::app::event::event2::Event2) {
        let dispatcher = self.dispatcher.clone();
        tokio::spawn(async move {
            let _ = dispatcher.dispatch(event).await;
        });
    }

    fn apply_register(&self, input: UserToolRegistration, activate: bool) {
        let name = input.name.clone();
        self.apply_unregister(&name);
        let tool = UserTool {
            scope_context: self.scope_context.clone(),
            session: self.session.clone(),
            name: name.clone(),
            description: input.description,
            parameters: input.parameters,
        };
        let registration: DisposableRef = self
            .registry
            .register(
                Arc::new(tool),
                ToolRegistrationOptions {
                    source: Some(ToolSource::User),
                    disclosure: input.disclosure,
                },
            )
            .into();
        self.registrations.lock().insert(name.clone(), registration);
        if activate {
            self.profile.add_active_tool(&name);
        }
    }

    fn apply_unregister(&self, name: &str) {
        let Some(registration) = self.registrations.lock().remove(name) else {
            return;
        };
        registration.dispose();
        self.profile.remove_active_tool(name);
    }

    fn restore_registered_tools(&self) {
        let persisted_active = self.profile.get_active_tool_names();
        for registration in self.registrations().values() {
            let activate = persisted_active.as_ref().is_none_or(|active| {
                active.iter().any(|name| name == &registration.name)
            });
            self.apply_register(registration.clone(), activate);
        }
    }
}

impl Drop for AgentUserToolServiceInner {
    fn drop(&mut self) {
        for subscription in self.subscriptions.get_mut().drain(..) {
            subscription.dispose();
        }
        for (_, registration) in self.registrations.get_mut().drain() {
            registration.dispose();
        }
    }
}

#[derive(Clone)]
pub struct AgentUserToolService {
    inner: Arc<AgentUserToolServiceInner>,
}

impl AgentUserToolService {
    pub fn new(
        scope_context: Arc<dyn IAgentScopeContext>,
        registry: Arc<dyn IAgentToolRegistryService>,
        profile: Arc<dyn IAgentProfileService>,
        session: Arc<dyn ISessionContext>,
        dispatcher: Arc<dyn IEventDispatcher>,
        agent_state: Arc<dyn IAgentStateService>,
    ) -> Self {
        let _ = agent_state.contribute_state(user_tool_key().state_key());
        let service = Self {
            inner: Arc::new(AgentUserToolServiceInner {
                scope_context,
                registry,
                profile,
                session,
                dispatcher,
                agent_state,
                registrations: Mutex::new(HashMap::new()),
                subscriptions: Mutex::new(Vec::new()),
            }),
        };
        let weak = Arc::downgrade(&service.inner);
        let restore = service
            .inner
            .dispatcher
            .hooks()
            .on_did_restore
            .register(
                "user-tool",
                Arc::new(move |_context, next| {
                    let weak = weak.clone();
                    Box::pin(async move {
                        if let Some(service) = weak.upgrade() {
                            service.restore_registered_tools();
                        }
                        next().await
                    }) as BoxFuture<'static, Result<(), WireAsyncError>>
                }),
                HookRegisterOptions::default(),
            )
            .expect("user tool restore hook can be registered");
        service.inner.subscriptions.lock().push(Arc::new(restore));
        service
    }
}

impl IAgentUserToolService for AgentUserToolService {
    fn list(&self) -> Vec<UserToolRegistration> {
        self.inner.registrations().values().cloned().collect()
    }

    fn inherit_user_tools(
        &self,
        parent: &dyn IAgentUserToolService,
        active_tool_names: Option<Vec<String>>,
    ) {
        for registration in parent.list() {
            self.inner.dispatch_detached(
                ToolsRegisterUserTool::new(ToolsRegisterUserToolPayload {
                    agent_id: self.inner.scope_context.agent_id(),
                    name: registration.name.clone(),
                    description: registration.description.clone(),
                    parameters: registration.parameters.clone(),
                    disclosure: registration.disclosure,
                })
                .into(),
            );
            let activate = active_tool_names.as_ref().is_none_or(|active| {
                active.iter().any(|name| name == &registration.name)
            });
            self.inner.apply_register(registration, activate);
        }
    }

    fn register(&self, input: UserToolRegistration) {
        self.inner.dispatch_detached(
            ToolsRegisterUserTool::new(ToolsRegisterUserToolPayload {
                agent_id: self.inner.scope_context.agent_id(),
                name: input.name.clone(),
                description: input.description.clone(),
                parameters: input.parameters.clone(),
                disclosure: input.disclosure,
            })
            .into(),
        );
        self.inner.apply_register(input, true);
    }

    fn unregister(&self, name: &str) {
        self.inner.dispatch_detached(
            ToolsUnregisterUserTool::new(ToolsUnregisterUserToolPayload {
                agent_id: self.inner.scope_context.agent_id(),
                name: name.to_string(),
            })
            .into(),
        );
        self.inner.apply_unregister(name);
    }
}
