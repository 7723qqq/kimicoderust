use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};

use futures_util::FutureExt;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::_base::di::lifecycle::DisposableStore;
use crate::_base::errors::serialize::{KimiErrorPayload, UnknownError, to_kimi_error_payload};
use crate::_base::utils::abort::{AbortSignal, abort_error};
use crate::agent::context_memory::context_memory::IAgentContextMemoryService;
use crate::agent::context_memory::types::{
    ContextMessage, HookResultOrigin, PromptOrigin, SystemTriggerOrigin,
};
use crate::agent::full_compaction::full_compaction::{
    FullCompactionTask, IAgentFullCompactionService,
};
use crate::agent::full_compaction::types::CompactionResult;
use crate::agent::prompt::prompt_events::{PromptQueued, PromptQueuedPayload};
use crate::agent::r#loop::r#loop::{
    AfterStepContext, IAgentLoopService, PromptSubmitContext,
};
use crate::agent::r#loop::turn_events::{TurnStarted, TurnStartedPayload};
use crate::agent::r#loop::turn_ops::{TurnEndReason, TurnEnded, TurnEndedPayload};
use crate::agent::scope_context::scope_context::IAgentScopeContext;
use crate::agent::state::agent_state::IAgentStateService;
use crate::agent::task::task::AgentTaskNotificationContext;
use crate::agent::task::task_ops::{TaskNotified, TaskStarted, TaskStartedPayload};
use crate::agent::task::types::AgentTaskInfo;
use crate::agent::tool_approval::tool_approval_service::{
    PermissionApprovalRequested, PermissionApprovalRequestedPayload, PermissionApprovalResolved,
    PermissionApprovalResolvedPayload,
};
use crate::agent::tool_executor::before_tool_execute_event::deny_tool_execution;
use crate::agent::tool_executor::tool_executor::{
    ExecutableToolOutput, IAgentToolExecutorService,
};
use crate::agent::tool_executor::tool_hooks::{BeforeExecuteDecision, ToolDidExecuteContext};
use crate::app::event::event2::{Event2, Event2Class};
use crate::app::event::event_bus::IEventBus;
use crate::features::externalHooks::app::externalHooksRunner::{
    ExternalHooksRunnerTriggerArgs, HookExecutionError, IExternalHooksRunnerService,
};
use crate::features::externalHooks::internal::types::HookMatcherValue;
use crate::features::externalHooks::internal::userPrompt::{
    render_user_prompt_hook_block_result, render_user_prompt_hook_result,
};
use crate::hooks::{HookNext, HookRegisterOptions};
use crate::human::llm::finish_reason::FinishReason;
use crate::human::llm::message::{ContentPart, Role};
use crate::rpc::types::ContentBlock;
use crate::session::agent_lifecycle::agent_lifecycle::MAIN_AGENT_ID;
use crate::session::session_context::session_context::ISessionContext;
use crate::session::sessionMetadata::session_metadata::{
    ISessionMetadata, SessionMetadataChangedEvent,
};
use crate::state::event_dispatcher::IEventDispatcher;
use crate::state::state::{ReplayableOptions, ReplayableStateKey, define_state};
use crate::wire::record::WireAsyncError;

use super::agentExternalHooks::IAgentExternalHooksService;

pub const HOOK_RESULT_EVENT_TYPE: &str = "hook.result";
pub const EXTERNAL_HOOKS_STOP_HOOK_CONTINUATION_USED_KEY: &str =
    "externalHooks.stopHookContinuationUsed";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookResultPayload {
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u64>,
    pub hook_event: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<bool>,
}

pub static HOOK_RESULT_CLASS: LazyLock<Arc<Event2Class>> = LazyLock::new(|| {
    Arc::new(Event2Class {
        type_: HOOK_RESULT_EVENT_TYPE.to_string(),
        durable: false,
        observable: true,
        agent_domain: true,
        schema: None,
    })
});

#[derive(Debug, Clone, PartialEq)]
pub struct HookResult {
    event: Event2,
    pub payload: HookResultPayload,
}

impl HookResult {
    pub const TYPE: &'static str = HOOK_RESULT_EVENT_TYPE;
    pub const OBSERVABLE: bool = true;

    pub fn new(payload: HookResultPayload) -> Self {
        Self {
            event: Event2::new(
                &HOOK_RESULT_CLASS,
                serde_json::to_value(&payload).expect("hook result is serializable"),
                None,
            ),
            payload,
        }
    }

    pub fn class() -> Arc<Event2Class> {
        HOOK_RESULT_CLASS.clone()
    }

    pub fn as_event(&self) -> &Event2 {
        &self.event
    }
}

impl From<HookResult> for Event2 {
    fn from(event: HookResult) -> Self {
        event.event
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookResultEvent {
    #[serde(default = "hook_result_event_type")]
    pub r#type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u64>,
    pub hook_event: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<bool>,
}

fn hook_result_event_type() -> String {
    HOOK_RESULT_EVENT_TYPE.to_string()
}

pub fn external_hooks_stop_hook_continuation_used_key() -> Arc<ReplayableStateKey> {
    static KEY: LazyLock<Arc<ReplayableStateKey>> = LazyLock::new(|| {
        define_state(
            EXTERNAL_HOOKS_STOP_HOOK_CONTINUATION_USED_KEY,
            Arc::new(|| Value::Bool(false)),
        )
        .replayable(ReplayableOptions {
            schema: json!({"type": "boolean"}),
            durable: Some(false),
            blobs: None,
            mutable_replay: None,
        })
    });
    KEY.clone()
}

#[derive(Clone)]
pub struct AgentExternalHooksService {
    runner: Arc<dyn IExternalHooksRunnerService>,
    context: Arc<dyn IAgentContextMemoryService>,
    event_bus: Arc<dyn IEventBus>,
    session_context: Arc<dyn ISessionContext>,
    session_metadata: Arc<dyn ISessionMetadata>,
    states: Arc<dyn IAgentStateService>,
    scope_context: Arc<dyn IAgentScopeContext>,
    dispatcher: Arc<dyn IEventDispatcher>,
    session_title: Arc<Mutex<Option<String>>>,
    hook_failure_warned: Arc<AtomicBool>,
    registrations: DisposableStore,
}

impl AgentExternalHooksService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        runner: Arc<dyn IExternalHooksRunnerService>,
        context: Arc<dyn IAgentContextMemoryService>,
        event_bus: Arc<dyn IEventBus>,
        session_context: Arc<dyn ISessionContext>,
        session_metadata: Arc<dyn ISessionMetadata>,
        states: Arc<dyn IAgentStateService>,
        scope_context: Arc<dyn IAgentScopeContext>,
        dispatcher: Arc<dyn IEventDispatcher>,
        tool_executor: Arc<dyn IAgentToolExecutorService>,
        loop_service: Arc<dyn IAgentLoopService>,
        full_compaction: Arc<dyn IAgentFullCompactionService>,
    ) -> Self {
        let service = Self {
            runner,
            context,
            event_bus,
            session_context,
            session_metadata,
            states,
            scope_context,
            dispatcher,
            session_title: Arc::new(Mutex::new(None)),
            hook_failure_warned: Arc::new(AtomicBool::new(false)),
            registrations: DisposableStore::new(),
        };
        let registration = service
            .states
            .contribute_state(external_hooks_stop_hook_continuation_used_key().state_key())
            .unwrap_or_else(|error| panic!("{error}"));
        service.registrations.add(registration);
        service.refresh_session_title();
        service.register_session_title_listener();
        service.register_listeners(tool_executor, loop_service, full_compaction);
        service
    }

    fn with_session_facts(&self, input_data: Map<String, Value>) -> Map<String, Value> {
        let mut out = Map::new();
        out.insert("sessionTitle".to_string(), self.session_title_value());
        out.extend(input_data);
        out
    }

    fn session_title_value(&self) -> Value {
        self.session_title
            .lock()
            .clone()
            .map(Value::String)
            .unwrap_or(Value::Null)
    }

    fn stop_hook_continuation_used(&self) -> bool {
        self.states
            .get(&external_hooks_stop_hook_continuation_used_key().state_key())
            .as_bool()
            == Some(true)
    }

    fn set_stop_hook_continuation_used(&self, value: bool) {
        self.states.set(
            &external_hooks_stop_hook_continuation_used_key().state_key(),
            Value::Bool(value),
        );
    }

    async fn hook_cwd(&self) -> String {
        let _ = self.session_metadata.ready().await;
        self.session_metadata.effective_cwd()
    }

    fn refresh_session_title(&self) {
        let metadata = self.session_metadata.clone();
        let session_title = self.session_title.clone();
        spawn_detached(async move {
            if let Ok(meta) = metadata.read().await {
                *session_title.lock() = meta.title;
            }
        });
    }

    fn register_session_title_listener(&self) {
        let this = self.clone();
        let subscription = self
            .session_metadata
            .on_did_change_metadata(Arc::new(move |event: SessionMetadataChangedEvent| {
                if !event.changed.iter().any(|changed| changed == "title") {
                    return;
                }
                this.refresh_session_title();
            }));
        self.registrations.add(Arc::from(subscription));
    }

    fn fire_and_forget(
        &self,
        event: &str,
        input_data: Map<String, Value>,
        matcher_value: Option<HookMatcherValue>,
        signal: Option<CancellationToken>,
    ) {
        let this = self.clone();
        let event = event.to_string();
        spawn_detached(async move {
            let cwd = this.hook_cwd().await;
            let _ = this
                .runner
                .fire_and_forget_trigger(
                    &event,
                    ExternalHooksRunnerTriggerArgs {
                        matcher_value,
                        signal,
                        cwd: Some(cwd),
                        session_id: Some(this.session_context.session_id()),
                        input_data: this.with_session_facts(input_data),
                    },
                )
                .await;
        });
    }

    fn warn_hook_failure(&self, failure: &HookExecutionError) {
        if self.scope_context.agent_id() != MAIN_AGENT_ID {
            return;
        }
        if failure
            .session_id
            .as_deref()
            .is_some_and(|session_id| session_id != self.session_context.session_id())
        {
            return;
        }
        if self.hook_failure_warned.swap(true, Ordering::SeqCst) {
            return;
        }
        self.dispatch_hook_result(
            failure.event.clone(),
            format!(
                "A {} hook failed to execute: {}. Hooks always run on this local machine; check that the hook command works locally. Later hook failures in this session are logged without another warning.",
                failure.event, failure.message
            ),
            None,
        );
    }

    fn dispatch_hook_result(&self, hook_event: String, content: String, blocked: Option<bool>) {
        let dispatcher = self.dispatcher.clone();
        let payload = HookResultPayload {
            agent_id: self.scope_context.agent_id(),
            turn_id: None,
            hook_event,
            content,
            blocked,
        };
        spawn_detached(async move {
            let _ = dispatcher.dispatch(HookResult::new(payload).into()).await;
        });
    }

    fn register_listeners(
        &self,
        tool_executor: Arc<dyn IAgentToolExecutorService>,
        loop_service: Arc<dyn IAgentLoopService>,
        full_compaction: Arc<dyn IAgentFullCompactionService>,
    ) {
        self.register_hook_error_listener();
        self.register_permission_hooks();
        self.register_tool_hooks(tool_executor);
        self.register_prompt_hooks(loop_service.clone());
        self.register_turn_hooks();
        self.register_loop_hooks(loop_service);
        self.register_full_compaction_hooks(full_compaction);
        self.register_task_hooks();
    }

    fn register_hook_error_listener(&self) {
        let mut errors = self.runner.on_did_hook_error();
        let this = self.clone();
        spawn_detached(async move {
            loop {
                match errors.recv().await {
                    Ok(failure) => this.warn_hook_failure(&failure),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    fn register_permission_hooks(&self) {
        let this = self.clone();
        let subscription = self.event_bus.subscribe_class(
            PermissionApprovalRequested::class(),
            Arc::new(move |event| {
                let Ok(payload) =
                    serde_json::from_value::<PermissionApprovalRequestedPayload>(event.payload)
                else {
                    return;
                };
                let input_data = payload_object(&payload);
                this.fire_and_forget(
                    "PermissionRequest",
                    input_data,
                    Some(HookMatcherValue::Text(payload.tool_name)),
                    None,
                );
            }),
        );
        self.registrations.add(Arc::from(subscription));

        let this = self.clone();
        let subscription = self.event_bus.subscribe_class(
            PermissionApprovalResolved::class(),
            Arc::new(move |event| {
                let Ok(payload) =
                    serde_json::from_value::<PermissionApprovalResolvedPayload>(event.payload)
                else {
                    return;
                };
                let input_data = payload_object(&payload);
                this.fire_and_forget(
                    "PermissionResult",
                    input_data,
                    Some(HookMatcherValue::Text(payload.request.tool_name)),
                    None,
                );
            }),
        );
        self.registrations.add(Arc::from(subscription));
    }

    fn register_tool_hooks(&self, tool_executor: Arc<dyn IAgentToolExecutorService>) {
        let this = self.clone();
        let subscription = tool_executor.on_before_execute_tool().subscribe(move |event| {
            let this = this.clone();
            let signal = event.context.base.signal.clone();
            let tool_name = event.context.base.tool_call.name.clone();
            let tool_call_id = event.context.base.tool_call.id.clone();
            let args = event.context.base.args.clone();
            event.wait_until(Arc::new(move || {
                let this = this.clone();
                let signal = signal.clone();
                let tool_name = tool_name.clone();
                let tool_call_id = tool_call_id.clone();
                let args = args.clone();
                async move {
                    let reason = this
                        .run_pre_tool_use(&signal, &tool_name, &tool_call_id, &args)
                        .await?;
                    Ok(reason.map(|reason| BeforeExecuteDecision {
                        veto: Some(deny_tool_execution(&reason)),
                        execution_metadata: None,
                    }))
                }
                .boxed()
            }));
        });
        self.registrations.add(Arc::new(subscription));

        let this = self.clone();
        let slot = tool_executor.hooks().on_did_execute_tool;
        let _ = slot.register(
            "externalHooks",
            Arc::new(move |context: ToolDidExecuteContext, next: HookNext<ToolDidExecuteContext>| {
                let this = this.clone();
                Box::pin(async move {
                    this.notify_post_tool_use(&context);
                    next.proceed(Some(context)).await;
                })
            }),
            HookRegisterOptions::default(),
        );
        self.registrations.add(slot.as_disposable("externalHooks"));
    }

    fn register_prompt_hooks(&self, loop_service: Arc<dyn IAgentLoopService>) {
        let this = self.clone();
        let slot = loop_service.hooks().on_before_submit_prompt;
        let _ = slot.register(
            "externalHooks",
            Arc::new(move |mut context: PromptSubmitContext, next: HookNext<PromptSubmitContext>| {
                let this = this.clone();
                Box::pin(async move {
                    if this.run_prompt_submit_hook(&mut context).await {
                        context.block = true;
                        return;
                    }
                    next.proceed(Some(context)).await;
                })
            }),
            HookRegisterOptions::default(),
        );
        self.registrations.add(slot.as_disposable("externalHooks"));

        let this = self.clone();
        let subscription = self.event_bus.subscribe_class(
            PromptQueued::class(),
            Arc::new(move |event| {
                let Ok(payload) = serde_json::from_value::<PromptQueuedPayload>(event.payload)
                else {
                    return;
                };
                let mut input_data = Map::new();
                input_data.insert("promptId".to_string(), Value::String(payload.prompt_id));
                input_data.insert(
                    "prompt".to_string(),
                    serde_json::to_value(&payload.content).unwrap_or(Value::Null),
                );
                input_data.insert("queueLength".to_string(), Value::from(payload.queue_length));
                this.fire_and_forget(
                    "UserPromptQueued",
                    input_data,
                    Some(HookMatcherValue::Parts(payload.content)),
                    None,
                );
            }),
        );
        self.registrations.add(Arc::from(subscription));
    }

    fn register_turn_hooks(&self) {
        let this = self.clone();
        let subscription = self
            .event_bus
            .subscribe_class(TurnStarted::class(), Arc::new(move |event| {
                let Ok(payload) = serde_json::from_value::<TurnStartedPayload>(event.payload) else {
                    return;
                };
                this.notify_turn_started(&payload);
            }));
        self.registrations.add(Arc::from(subscription));

        let this = self.clone();
        let subscription = self
            .event_bus
            .subscribe_class(TurnEnded::class(), Arc::new(move |event| {
                let Ok(payload) = serde_json::from_value::<TurnEndedPayload>(event.payload) else {
                    return;
                };
                this.notify_turn_ended(&payload);
            }));
        self.registrations.add(Arc::from(subscription));
    }

    fn notify_turn_started(&self, event: &TurnStartedPayload) {
        let origin_kind = event
            .origin
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut input_data = Map::new();
        input_data.insert("turnId".to_string(), Value::from(event.turn_id));
        input_data.insert(
            "originKind".to_string(),
            Value::String(origin_kind.to_string()),
        );
        if let Some(name) = event.origin.get("name").and_then(Value::as_str) {
            input_data.insert("originName".to_string(), Value::String(name.to_string()));
        }
        if let Some(prompt) = &event.prompt {
            input_data.insert("prompt".to_string(), Value::String(prompt.clone()));
        }
        self.fire_and_forget(
            "TurnStarted",
            input_data,
            Some(HookMatcherValue::Text(origin_kind.to_string())),
            None,
        );
    }

    fn register_loop_hooks(&self, loop_service: Arc<dyn IAgentLoopService>) {
        let this = self.clone();
        let loop_for_handler = loop_service.clone();
        let slot = loop_service.hooks().on_did_finish_step;
        let _ = slot.register(
            "externalHooks",
            Arc::new(move |context: AfterStepContext, next: HookNext<AfterStepContext>| {
                let this = this.clone();
                let loop_for_handler = loop_for_handler.clone();
                Box::pin(async move {
                    next.proceed(Some(context.clone())).await;
                    if context.finish_reason == FinishReason::ToolCalls
                        || context.finish_reason == FinishReason::Filtered
                        || loop_for_handler.snapshot().has_pending_requests
                    {
                        return;
                    }
                    let Some(reason) = this.run_stop(&context).await else {
                        return;
                    };
                    this.set_stop_hook_continuation_used(true);
                    let mut message = ContextMessage::new(
                        Role::User,
                        vec![ContentPart::Text {
                            text: reason,
                            meta: None,
                        }],
                    );
                    message.origin = Some(PromptOrigin::SystemTrigger(SystemTriggerOrigin {
                        kind: "system_trigger".to_string(),
                        name: "stop_hook".to_string(),
                        background_helper: None,
                    }));
                    this.context.append(&[message]);
                    loop_for_handler.notify(None);
                })
            }),
            HookRegisterOptions::default(),
        );
        self.registrations.add(slot.as_disposable("externalHooks"));
    }

    fn register_full_compaction_hooks(
        &self,
        full_compaction: Arc<dyn IAgentFullCompactionService>,
    ) {
        let this = self.clone();
        let slot = full_compaction.hooks().on_will_compact;
        let _ = slot.register(
            "externalHooks",
            Arc::new(move |context: FullCompactionTask, next: HookNext<FullCompactionTask>| {
                let this = this.clone();
                Box::pin(async move {
                    this.run_pre_compact(&context).await;
                    let promise = context.promise.clone();
                    let this_for_post = this.clone();
                    let task = context.clone();
                    spawn_detached(async move {
                        if let Ok(result) = promise.await {
                            this_for_post.notify_post_compact(&task, &result);
                        }
                    });
                    next.proceed(Some(context)).await;
                })
            }),
            HookRegisterOptions::default(),
        );
        self.registrations.add(slot.as_disposable("externalHooks"));
    }

    fn register_task_hooks(&self) {
        let this = self.clone();
        let subscription = self
            .event_bus
            .subscribe_class(TaskNotified::class(), Arc::new(move |event| {
                let Ok(context) =
                    serde_json::from_value::<AgentTaskNotificationContext>(event.payload)
                else {
                    return;
                };
                this.notify_task_notification(&context);
            }));
        self.registrations.add(Arc::from(subscription));

        let this = self.clone();
        let subscription = self
            .event_bus
            .subscribe_class(TaskStarted::class(), Arc::new(move |event| {
                let Ok(payload) = serde_json::from_value::<TaskStartedPayload>(event.payload) else {
                    return;
                };
                this.notify_task_started(&payload.info);
            }));
        self.registrations.add(Arc::from(subscription));
    }

    fn notify_task_started(&self, info: &AgentTaskInfo) {
        let mut input_data = Map::new();
        input_data.insert(
            "taskId".to_string(),
            Value::String(info.base().task_id.clone()),
        );
        input_data.insert(
            "kind".to_string(),
            Value::String(info.kind().as_str().to_string()),
        );
        input_data.insert(
            "description".to_string(),
            Value::String(info.base().description.clone()),
        );
        input_data.insert(
            "status".to_string(),
            Value::String(info.base().status.as_str().to_string()),
        );
        if let Some(detached) = info.base().detached {
            input_data.insert("detached".to_string(), Value::Bool(detached));
        }
        input_data.insert(
            "startedAt".to_string(),
            json!(info.base().started_at),
        );
        self.fire_and_forget(
            "TaskStarted",
            input_data,
            Some(HookMatcherValue::Text(info.kind().as_str().to_string())),
            None,
        );
    }

    async fn run_pre_tool_use(
        &self,
        signal: &AbortSignal,
        tool_name: &str,
        tool_call_id: &str,
        args: &Value,
    ) -> Result<Option<String>, WireAsyncError> {
        if signal.aborted() {
            return Err(Box::new(abort_error()));
        }
        let tool_input = if args.is_object() {
            args.clone()
        } else {
            Value::Object(Map::new())
        };
        let mut input_data = Map::new();
        input_data.insert("toolName".to_string(), Value::String(tool_name.to_string()));
        input_data.insert("toolInput".to_string(), tool_input);
        input_data.insert(
            "toolCallId".to_string(),
            Value::String(tool_call_id.to_string()),
        );
        let block = self
            .runner
            .trigger_block(
                "PreToolUse",
                ExternalHooksRunnerTriggerArgs {
                    matcher_value: Some(HookMatcherValue::Text(tool_name.to_string())),
                    signal: None,
                    cwd: Some(self.hook_cwd().await),
                    session_id: Some(self.session_context.session_id()),
                    input_data: self.with_session_facts(input_data),
                },
            )
            .await;
        if signal.aborted() {
            return Err(Box::new(abort_error()));
        }
        Ok(block.map(|decision| decision.reason))
    }

    fn notify_post_tool_use(&self, context: &ToolDidExecuteContext) {
        let output = tool_output_text(&context.result.output);
        let is_error = context.result.is_error == Some(true);
        let tool_input = if context.base.args.is_object() {
            context.base.args.clone()
        } else {
            Value::Object(Map::new())
        };
        let mut input_data = Map::new();
        input_data.insert(
            "toolName".to_string(),
            Value::String(context.base.tool_call.name.clone()),
        );
        input_data.insert("toolInput".to_string(), tool_input);
        input_data.insert(
            "toolCallId".to_string(),
            Value::String(context.base.tool_call.id.clone()),
        );
        if is_error {
            input_data.insert(
                "error".to_string(),
                serde_json::to_value(to_kimi_error_payload(&UnknownError::String(output)))
                    .unwrap_or(Value::Null),
            );
        } else {
            input_data.insert(
                "toolOutput".to_string(),
                Value::String(output.chars().take(2000).collect()),
            );
        }
        self.fire_and_forget(
            if is_error {
                "PostToolUseFailure"
            } else {
                "PostToolUse"
            },
            input_data,
            Some(HookMatcherValue::Text(
                context.base.tool_call.name.clone(),
            )),
            None,
        );
    }

    async fn run_prompt_submit_hook(&self, context: &mut PromptSubmitContext) -> bool {
        let origin_kind = context
            .prompt_message
            .origin
            .as_ref()
            .map(PromptOrigin::kind)
            .unwrap_or("user");
        if origin_kind != "user" {
            return false;
        }

        let input = context.prompt_message.content.clone();
        let mut input_data = Map::new();
        input_data.insert(
            "prompt".to_string(),
            serde_json::to_value(&input).unwrap_or(Value::Null),
        );
        input_data.insert("isSteer".to_string(), Value::Bool(context.is_steer));
        let results = self
            .runner
            .trigger(
                "UserPromptSubmit",
                ExternalHooksRunnerTriggerArgs {
                    matcher_value: Some(HookMatcherValue::Parts(input)),
                    signal: Some(CancellationToken::new()),
                    cwd: Some(self.hook_cwd().await),
                    session_id: Some(self.session_context.session_id()),
                    input_data: self.with_session_facts(input_data),
                },
            )
            .await;

        if let Some(block) = render_user_prompt_hook_block_result(&results) {
            let mut message = ContextMessage::new(
                Role::Assistant,
                vec![ContentPart::Text {
                    text: block.text.clone(),
                    meta: None,
                }],
            );
            message.origin = Some(PromptOrigin::HookResult(HookResultOrigin {
                kind: "hook_result".to_string(),
                event: block.event.clone(),
                blocked: Some(true),
            }));
            self.context.append(&[message]);
            self.dispatch_hook_result(block.event, block.message, Some(true));
            return true;
        }

        if let Some(append) = render_user_prompt_hook_result(&results) {
            for part in &append.parts {
                if let ContentBlock::Text { text, meta } = part {
                    context.hook_parts.push(crate::agent::r#loop::r#loop::TextPart {
                        type_: "text".to_string(),
                        text: text.clone(),
                        meta: meta.clone(),
                    });
                }
            }
            for message in &append.messages {
                self.dispatch_hook_result(append.event.clone(), message.clone(), None);
            }
        }
        false
    }

    fn notify_turn_ended(&self, event: &TurnEndedPayload) {
        self.set_stop_hook_continuation_used(false);
        if event.reason == TurnEndReason::Failed
            && let Some(error) = &event.error
        {
            self.notify_stop_failure(error);
        }
        if event.reason == TurnEndReason::Cancelled {
            let mut input_data = Map::new();
            input_data.insert("turnId".to_string(), Value::from(event.turn_id));
            input_data.insert(
                "reason".to_string(),
                Value::String("cancelled".to_string()),
            );
            self.fire_and_forget("Interrupt", input_data, None, None);
        }
    }

    fn notify_stop_failure(&self, error: &KimiErrorPayload) {
        let mut input_data = Map::new();
        if let Some(name) = &error.name {
            input_data.insert("errorType".to_string(), Value::String(name.clone()));
        }
        input_data.insert(
            "errorMessage".to_string(),
            Value::String(error.message.clone()),
        );
        self.fire_and_forget(
            "StopFailure",
            input_data,
            error
                .name
                .as_ref()
                .map(|name| HookMatcherValue::Text(name.clone())),
            Some(CancellationToken::new()),
        );
    }

    async fn run_stop(&self, context: &AfterStepContext) -> Option<String> {
        if context.signal.aborted() {
            return None;
        }
        if self.stop_hook_continuation_used() {
            return None;
        }
        let mut input_data = Map::new();
        input_data.insert("stopHookActive".to_string(), Value::Bool(false));
        let block = self
            .runner
            .trigger_block(
                "Stop",
                ExternalHooksRunnerTriggerArgs {
                    matcher_value: None,
                    signal: None,
                    cwd: Some(self.hook_cwd().await),
                    session_id: Some(self.session_context.session_id()),
                    input_data: self.with_session_facts(input_data),
                },
            )
            .await;
        if context.signal.aborted() {
            return None;
        }
        block.map(|decision| decision.reason)
    }

    async fn run_pre_compact(&self, context: &FullCompactionTask) {
        let signal = context.abort_controller.signal();
        if signal.aborted() {
            return;
        }
        let trigger = context.trigger.as_str().to_string();
        let mut input_data = Map::new();
        input_data.insert("trigger".to_string(), Value::String(trigger.clone()));
        input_data.insert("tokenCount".to_string(), Value::from(context.token_count));
        self.runner
            .trigger(
                "PreCompact",
                ExternalHooksRunnerTriggerArgs {
                    matcher_value: Some(HookMatcherValue::Text(trigger)),
                    signal: None,
                    cwd: Some(self.hook_cwd().await),
                    session_id: Some(self.session_context.session_id()),
                    input_data: self.with_session_facts(input_data),
                },
            )
            .await;
    }

    fn notify_post_compact(&self, context: &FullCompactionTask, result: &CompactionResult) {
        let trigger = context.trigger.as_str().to_string();
        let mut input_data = Map::new();
        input_data.insert("trigger".to_string(), Value::String(trigger.clone()));
        input_data.insert(
            "estimatedTokenCount".to_string(),
            Value::from(result.tokens_after),
        );
        self.fire_and_forget(
            "PostCompact",
            input_data,
            Some(HookMatcherValue::Text(trigger)),
            None,
        );
    }

    fn notify_task_notification(&self, context: &AgentTaskNotificationContext) {
        let mut input_data = Map::new();
        input_data.insert("sink".to_string(), Value::String("context".to_string()));
        if let Value::Object(fields) = serde_json::to_value(context).unwrap_or(Value::Null) {
            input_data.extend(fields);
        }
        self.fire_and_forget(
            "Notification",
            input_data,
            Some(HookMatcherValue::Text(
                context.notification_type.clone(),
            )),
            Some(CancellationToken::new()),
        );
    }
}

impl IAgentExternalHooksService for AgentExternalHooksService {}

fn payload_object<T: Serialize>(payload: &T) -> Map<String, Value> {
    match serde_json::to_value(payload) {
        Ok(Value::Object(object)) => object,
        _ => Map::new(),
    }
}

fn tool_output_text(output: &ExecutableToolOutput) -> String {
    match output {
        ExecutableToolOutput::Text(text) => text.clone(),
        ExecutableToolOutput::Parts(parts) => parts
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""),
    }
}

fn spawn_detached<F>(future: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(future);
    }
}
