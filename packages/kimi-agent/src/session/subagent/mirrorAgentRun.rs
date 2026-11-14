use std::ops::Deref;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::_base::di::scope::IAgentScopeHandle;
use crate::_base::utils::abort::{
    AbortSignal, is_abort_error, is_user_cancellation,
};
use crate::_base::utils::error_message::error_message;
use crate::agent::profile::profile::IAgentProfileService;
use crate::agent::scope_context::scope_context::try_agent_context_of;
use crate::app::event::event2::{Event2, Event2Class, register_event2_class};
use crate::app::telemetry::telemetry::ITelemetryService;
use crate::human::llm::usage::TokenUsage;
use crate::llm_adapter::contract::errors::is_provider_rate_limit_error;
use crate::session::agent_lifecycle::agent_lifecycle::IAgentLifecycleService;
use crate::session::session_token_counting::session_token_counting::ISessionTokenCountingService;
use crate::state::event_dispatcher::IEventDispatcher;
use crate::wire::record::WireAsyncError;

use super::config_section::SubagentModelSource;
use super::subagent::{
    AgentRunCompletion, AgentRunHandle, ISessionSubagentService, SharedAgentRunError,
};

macro_rules! subagent_event {
    ($name:ident, $payload:ident, $type:literal, $( $(#[$meta:meta])* $field:ident : $ty:ty ),* $(,)?) => {
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
        #[serde(rename_all = "camelCase")]
        pub struct $payload {
            $( $(#[$meta])* pub $field: $ty, )*
        }

        #[derive(Debug, Clone, PartialEq)]
        pub struct $name {
            event: Event2,
            pub payload: $payload,
        }

        impl $name {
            pub const TYPE: &'static str = $type;
            pub const DURABLE: bool = true;
            pub const OBSERVABLE: bool = true;

            pub fn new(payload: $payload) -> Self {
                let class = Event2Class {
                    type_: $type.to_string(),
                    durable: true,
                    observable: true,
                    agent_domain: false,
                    schema: None,
                };
                let event = Event2::new(
                    &class,
                    serde_json::to_value(&payload).expect("subagent event is serializable"),
                    None,
                );
                Self { event, payload }
            }

            pub fn as_event(&self) -> &Event2 {
                &self.event
            }
        }

        impl Deref for $name {
            type Target = $payload;

            fn deref(&self) -> &Self::Target {
                &self.payload
            }
        }

        impl From<$name> for Event2 {
            fn from(event: $name) -> Self {
                event.event
            }
        }
    };
}

subagent_event!(SubagentSpawned, SubagentSpawnedPayload, "subagent.spawned",
    subagent_id: String,
    subagent_name: String,
    parent_tool_call_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent_tool_call_uuid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent_agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    caller_agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    swarm_index: Option<f64>,
    run_in_background: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    thinking_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    task_id: Option<String>);
subagent_event!(SubagentStarted, SubagentStartedPayload, "subagent.started",
    subagent_id: String);
subagent_event!(SubagentCompleted, SubagentCompletedPayload, "subagent.completed",
    subagent_id: String,
    result_summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    usage: Option<TokenUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    context_tokens: Option<u64>);
subagent_event!(SubagentFailed, SubagentFailedPayload, "subagent.failed",
    subagent_id: String,
    error: String);
subagent_event!(SubagentCancelled, SubagentCancelledPayload, "subagent.cancelled",
    subagent_id: String);

pub fn register_mirror_agent_run_events() {
    for type_ in [
        SubagentSpawned::TYPE,
        SubagentStarted::TYPE,
        SubagentCompleted::TYPE,
        SubagentFailed::TYPE,
        SubagentCancelled::TYPE,
    ] {
        register_event2_class(Event2Class {
            type_: type_.to_string(),
            durable: true,
            observable: true,
            agent_domain: false,
            schema: None,
        });
    }
}

#[derive(Debug, Clone, Default)]
pub struct AgentRunSpawnedMeta {
    pub profile_name: String,
    pub parent_tool_call_id: Option<String>,
    pub parent_tool_call_uuid: Option<String>,
    pub description: Option<String>,
    pub swarm_index: Option<f64>,
    pub run_in_background: Option<bool>,
    pub fork: Option<bool>,
    pub model: Option<String>,
    pub model_source: Option<SubagentModelSource>,
    pub task_id: Option<String>,
}

#[derive(Clone)]
pub struct MirrorAgentRunOptions {
    pub profile_name: String,
    pub prompt: Option<String>,
    pub suppress_rate_limit_failure_event: Option<bool>,
    pub signal: AbortSignal,
    pub cancel: Option<Arc<dyn Fn(Option<WireAsyncError>) + Send + Sync>>,
    pub defer_started: Option<bool>,
    pub terminalize: Option<Arc<dyn Fn(String, Event2) + Send + Sync>>,
}

pub fn emit_agent_run_spawned(
    requester: &Arc<IAgentScopeHandle>,
    target_agent_id: &str,
    meta: AgentRunSpawnedMeta,
) {
    let child_profile = requester
        .accessor()
        .get::<IAgentLifecycleService>()
        .handle_of(target_agent_id)
        .map(|handle| handle.accessor().get::<IAgentProfileService>());
    let dispatcher = requester.accessor().get::<IEventDispatcher>();
    let event = SubagentSpawned::new(SubagentSpawnedPayload {
        subagent_id: target_agent_id.to_string(),
        subagent_name: meta.profile_name.clone(),
        parent_tool_call_id: meta.parent_tool_call_id.clone().unwrap_or_default(),
        parent_tool_call_uuid: meta.parent_tool_call_uuid.clone(),
        parent_agent_id: Some(requester.id().to_string()),
        caller_agent_id: Some(requester.id().to_string()),
        description: meta.description.clone(),
        swarm_index: meta.swarm_index,
        run_in_background: meta.run_in_background.unwrap_or(false),
        model: meta.model.clone(),
        thinking_effort: child_profile.as_ref().map(|profile| profile.get_effective_thinking_level()),
        task_id: meta.task_id.clone(),
    });
    tokio::spawn(async move {
        let _ = dispatcher.dispatch(event.into()).await;
    });
    if let Some(child_profile) = child_profile {
        child_profile.republish_status();
    }
    requester.accessor().get::<ITelemetryService>().track2(
        "subagent_created",
        json!({
            "subagent_name": meta.profile_name,
            "run_in_background": meta.run_in_background.unwrap_or(false),
            "fork": meta.fork.unwrap_or(false),
            "agent_id": target_agent_id,
            "parent_agent_id": requester.id(),
            "parent_tool_call_id": meta.parent_tool_call_id.unwrap_or_default(),
            "model": meta.model,
            "model_source": model_source_name(meta.model_source.as_ref()),
        }),
    );
}

pub async fn mirror_agent_run(
    requester: &Arc<IAgentScopeHandle>,
    run: AgentRunHandle,
    options: MirrorAgentRunOptions,
) -> Result<AgentRunCompletion, WireAsyncError> {
    let dispatcher = requester.accessor().get::<IEventDispatcher>();
    let subagents = requester.accessor().get::<ISessionSubagentService>();
    let agent_lifecycle = requester.accessor().get::<IAgentLifecycleService>();
    if options.defer_started != Some(true) {
        let dispatcher = dispatcher.clone();
        let agent_id = run.agent_id.clone();
        tokio::spawn(async move {
            let _ = dispatcher
                .dispatch(SubagentStarted::new(SubagentStartedPayload { subagent_id: agent_id }).into())
                .await;
        });
    }
    if let Some(prompt) = options.prompt.clone() {
        let hook_result = subagents
            .hooks()
            .on_will_start_agent_task
            .run(super::subagent::AgentTaskStartHookContext {
                agent_name: options.profile_name.clone(),
                prompt,
                signal: options.signal.clone(),
            })
            .await;
        if let Err(error) = hook_result {
            let error = SharedAgentRunError::new(error);
            return Err(cancel_and_rethrow(&dispatcher, &run, &options, error));
        }
        if options.signal.aborted() {
            let reason = options.signal.reason();
            let error = SharedAgentRunError::new(reason);
            return Err(cancel_and_rethrow(&dispatcher, &run, &options, error));
        }
    }
    match run.completion.clone().await {
        Ok(result) => {
            let context_tokens = child_context_tokens(&agent_lifecycle, &run.agent_id);
            let dispatcher = dispatcher.clone();
            let event = SubagentCompleted::new(SubagentCompletedPayload {
                subagent_id: run.agent_id.clone(),
                result_summary: result.summary.clone(),
                usage: result.usage.clone(),
                context_tokens,
            });
            tokio::spawn(async move {
                let _ = dispatcher.dispatch(event.into()).await;
            });
            subagents.notify_agent_task_stopped(super::subagent::AgentTaskStopHookContext {
                agent_name: options.profile_name.clone(),
                response: result.summary.clone(),
            });
            Ok(result)
        }
        Err(error) => {
            let boxed = error.clone().boxed();
            if let Some(event) = terminal_event_for(&run.agent_id, &boxed, &options) {
                emit_terminal(&dispatcher, &options, &run.agent_id, event);
            }
            Err(error.boxed())
        }
    }
}

fn cancel_and_rethrow(
    dispatcher: &Arc<dyn IEventDispatcher>,
    run: &AgentRunHandle,
    options: &MirrorAgentRunOptions,
    error: SharedAgentRunError,
) -> WireAsyncError {
    let boxed = error.clone().boxed();
    if let Some(cancel) = &options.cancel {
        cancel(Some(error.clone().boxed()));
    }
    let completion = run.completion.clone();
    tokio::spawn(async move {
        let _ = completion.await;
    });
    if let Some(event) = terminal_event_for(&run.agent_id, &boxed, options) {
        emit_terminal(dispatcher, options, &run.agent_id, event);
    }
    boxed
}

fn emit_terminal(
    dispatcher: &Arc<dyn IEventDispatcher>,
    options: &MirrorAgentRunOptions,
    agent_id: &str,
    event: Event2,
) {
    if let Some(terminalize) = &options.terminalize {
        terminalize(agent_id.to_string(), event);
        return;
    }
    let dispatcher = dispatcher.clone();
    tokio::spawn(async move {
        let _ = dispatcher.dispatch(event).await;
    });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunTermination {
    Cancelled,
    Failed,
}

pub fn classify_run_termination(error: &WireAsyncError, signal: &AbortSignal) -> RunTermination {
    if !signal.aborted() {
        return if is_abort_error(error.as_ref()) {
            RunTermination::Cancelled
        } else {
            RunTermination::Failed
        };
    }
    let reason = signal.reason();
    if is_user_cancellation(reason.as_ref()) {
        return RunTermination::Cancelled;
    }
    if !is_abort_error(reason.as_ref()) {
        RunTermination::Failed
    } else {
        RunTermination::Cancelled
    }
}

fn terminal_event_for(
    agent_id: &str,
    error: &WireAsyncError,
    options: &MirrorAgentRunOptions,
) -> Option<Event2> {
    if classify_run_termination(error, &options.signal) == RunTermination::Cancelled {
        return Some(SubagentCancelled::new(SubagentCancelledPayload {
            subagent_id: agent_id.to_string(),
        }).into());
    }
    if options.suppress_rate_limit_failure_event == Some(true)
        && is_provider_rate_limit_error(error.as_ref())
    {
        return None;
    }
    Some(SubagentFailed::new(SubagentFailedPayload {
        subagent_id: agent_id.to_string(),
        error: error_message(error.as_ref()),
    }).into())
}

fn child_context_tokens(
    agent_lifecycle: &Arc<dyn IAgentLifecycleService>,
    agent_id: &str,
) -> Option<u64> {
    let child = agent_lifecycle.handle_of(agent_id)?;
    let context = try_agent_context_of(&child)?;
    Some(
        child
            .accessor()
            .get::<ISessionTokenCountingService>()
            .status_size(&context),
    )
}

fn model_source_name(source: Option<&SubagentModelSource>) -> Option<&'static str> {
    match source {
        Some(SubagentModelSource::Inherited) => Some("inherited"),
        Some(SubagentModelSource::SecondaryPool) => Some("secondary_pool"),
        None => None,
    }
}
