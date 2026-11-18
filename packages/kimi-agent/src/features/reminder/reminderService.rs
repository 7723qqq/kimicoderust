use std::sync::LazyLock;
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
use std::sync::Arc;

use futures_util::FutureExt;
use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::_base::log::log::I_LOG_SERVICE;
use crate::agent::actor_service::agent_actor_runtime::AgentActorHost;
use crate::agent::actor_service::agent_actor_service::{
    AgentActorContext, AgentActorDurable, AgentActorOptions, AgentActorService,
};
use crate::agent::actor_service::services::{
    AgentLoopService, EventBusService, HookContext, LogService,
};
use crate::agent::agent_context::AgentContext;
use crate::agent::context_memory::context_memory::IAgentContextMemoryService;
use crate::agent::context_memory::types::{ContextMessage, InjectionOrigin, PromptOrigin};
use crate::features::reminder::types::{
    ContextInjectionContent, ContextInjectionContext, ContextInjectionResult, ReminderNotification,
};
use crate::human::llm::tokens::estimate_tokens_for_messages;
use kimi_core::types::ContentPart;

#[derive(Clone)]
pub struct ReminderEntry {
    pub provider: Arc<
        dyn Fn(
                ContextInjectionContext,
            ) -> crate::rpc::types::BoxFuture<
                'static,
                Result<Option<ContextInjectionResult>, String>,
            > + Send
            + Sync,
    >,
    pub variant: String,
}
use crate::human::xstate2::{
    Action, ActorLogic, Context, InvokeDef, InvokeInput, Machine, StateNode, Transition, XEvent,
    from_callback,
};

pub const REMINDER_VARIANT_PRIORITY: &[(&str, i32)] = &[("date_change", -1)];

#[derive(Clone)]
pub struct ReminderActorContext {
    pub entries: Arc<Mutex<Vec<Arc<ReminderEntry>>>>,
    pub runtime: Context,
}

pub fn actor_context(runtime: &AgentActorContext<()>) -> ReminderActorContext {
    (runtime.get_logic_state)()
        .downcast::<Mutex<ReminderActorContext>>()
        .unwrap()
        .lock()
        .clone()
}

pub fn reminder_actor_logic() -> Machine {
    Machine {
        context: Arc::new(|input| {
            Arc::new(Mutex::new(ReminderActorContext {
                entries: Arc::new(Mutex::new(Vec::new())),
                runtime: input,
            }))
        }),
        initial: "beforeRestore".to_string(),
        states: std::collections::HashMap::from([
            (
                "beforeRestore".to_string(),
                StateNode {
                    final_: false,
                    on: std::collections::HashMap::from([(
                        "runtime.restore".to_string(),
                        vec![Transition {
                            target: Some("active".to_string()),
                            actions: Vec::new(),
                            ..Default::default()
                        }],
                    )]),
                    invoke: None,
                },
            ),
            (
                "active".to_string(),
                StateNode {
                    final_: false,
                    on: std::collections::HashMap::new(),
                    invoke: Some(InvokeDef {
                        id: "reminderEffects".to_string(),
                        src: "reminderEffects".to_string(),
                        input: Arc::new(|args| {
                            let context = args
                                .context
                                .downcast_ref::<Mutex<ReminderActorContext>>()
                                .unwrap()
                                .lock();
                            InvokeInput {
                                runtime: context.runtime.clone(),
                                restore: args.event.restore.clone(),
                            }
                        }),
                        on_done: Vec::new(),
                        on_error: Vec::new(),
                    }),
                },
            ),
        ]),
        on: std::collections::HashMap::from([
            (
                "reminder.register".to_string(),
                vec![Transition {
                    target: None,
                    actions: vec![Action::Custom(Arc::new(|context, args| {
                        let context = context
                            .downcast_ref::<Mutex<ReminderActorContext>>()
                            .unwrap();
                        let entry = args
                            .event
                            .data
                            .clone()
                            .unwrap()
                            .downcast::<ReminderEntry>()
                            .unwrap();
                        context.lock().entries.lock().push(entry);
                    }))],
                    ..Default::default()
                }],
            ),
            (
                "reminder.unregister".to_string(),
                vec![Transition {
                    target: None,
                    actions: vec![Action::Custom(Arc::new(|context, args| {
                        let context = context
                            .downcast_ref::<Mutex<ReminderActorContext>>()
                            .unwrap();
                        let entry = args
                            .event
                            .data
                            .clone()
                            .unwrap()
                            .downcast::<ReminderEntry>()
                            .unwrap();
                        context
                            .lock()
                            .entries
                            .lock()
                            .retain(|current| !Arc::ptr_eq(current, &entry));
                    }))],
                    ..Default::default()
                }],
            ),
        ]),
        actors: std::collections::HashMap::from([(
            "reminderEffects".to_string(),
            reminder_effects(),
        )]),
        ..Default::default()
    }
}

fn reminder_effects() -> ActorLogic {
    from_callback(|input| {
        let runtime = input
            .runtime
            .downcast_ref::<AgentActorContext<()>>()
            .unwrap()
            .clone();
        let compaction_rearm_pending = Arc::new(Mutex::new(false));
        let take_compaction_rearm = {
            let pending = compaction_rearm_pending.clone();
            move || {
                let mut pending = pending.lock();
                let value = *pending;
                *pending = false;
                value
            }
        };
        let loop_service = runtime.service::<dyn AgentLoopService>("loopService");
        let reconcile_around_step = {
            let runtime = runtime.clone();
            let take_compaction_rearm = take_compaction_rearm.clone();
            Arc::new(
                move |context: HookContext,
                      next: crate::agent::actor_service::services::NextHook| {
                    let runtime = runtime.clone();
                    let take_compaction_rearm = take_compaction_rearm.clone();
                    Box::pin(async move {
                        let rearmed = take_compaction_rearm();
                        let first_step = context
                            .value
                            .get("firstStepOfTurn")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        inject(&runtime, first_step || rearmed, context.signal.clone()).await;
                        next().await;
                        if take_compaction_rearm() {
                            inject(&runtime, true, context.signal).await;
                        }
                    }) as crate::rpc::types::BoxFuture<'static, ()>
                },
            )
        };
        let hook = loop_service
            .hooks()
            .on_will_begin_step
            .register("context-injector", reconcile_around_step);
        let splice = runtime
            .service::<dyn EventBusService>("eventBus")
            .subscribe("context.spliced", {
                let compaction_rearm_pending = compaction_rearm_pending.clone();
                Arc::new(move |event: Value| {
                    if is_compaction_splice(&event) {
                        *compaction_rearm_pending.lock() = true;
                    }
                })
            });
        Box::new(move || {
            splice();
            hook();
            actor_context(&runtime).entries.lock().clear();
        })
    })
}

pub fn find_injections(history: &[ContextMessage], variant: &str) -> Vec<usize> {
    let mut positions = Vec::new();
    for (index, message) in history.iter().enumerate() {
        let matches = message
            .origin
            .as_ref()
            .and_then(PromptOrigin::injection_variant)
            == Some(variant);
        if matches {
            positions.push(index);
        }
    }
    positions
}

pub fn is_compaction_splice(splice: &Value) -> bool {
    splice
        .get("deleteCount")
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
        > 0.0
        && splice
            .get("messages")
            .and_then(Value::as_array)
            .map(|messages| {
                messages.iter().any(|message| {
                    serde_json::from_value::<ContextMessage>(message.clone())
                        .ok()
                        .as_ref()
                        .is_some_and(is_compaction_summary_message)
                })
            })
            .unwrap_or(false)
}

fn is_compaction_summary_message(message: &ContextMessage) -> bool {
    crate::agent::context_memory::compaction_handoff::is_compaction_summary_message(message)
}

pub fn is_injection_result(content: &Value) -> bool {
    content.is_object() && content.get("content").is_some()
}

pub fn is_raw_injection_message(content: &Value) -> bool {
    !content.is_array()
}

fn text_context_message(
    role: crate::human::llm::message::Role,
    text: String,
    origin: PromptOrigin,
) -> ContextMessage {
    ContextMessage {
        role,
        content: vec![ContentPart::Text { text, meta: None }],
        origin: Some(origin),
        ..Default::default()
    }
}

pub fn append_reminder(
    runtime: &AgentActorContext<()>,
    content: &str,
    notification: &ReminderNotification,
) {
    let message = text_context_message(
        crate::human::llm::message::Role::User,
        wrap_system_reminder(content),
        reminder_origin(
            &notification.variant,
            notification.owner_prompt_id.clone(),
            None,
        ),
    );
    runtime
        .service::<dyn IAgentContextMemoryService>("contextMemoryService")
        .append(&[message]);
}

pub fn provider_context(
    runtime: &AgentActorContext<()>,
    entry: &ReminderEntry,
    is_new_turn: bool,
    estimated_tokens: usize,
    signal: Option<tokio_util::sync::CancellationToken>,
) -> ContextInjectionContext {
    let history = runtime
        .service::<dyn IAgentContextMemoryService>("contextMemoryService")
        .get();
    let injected_positions = find_injections(&history, &entry.variant);
    let last_injected_at = injected_positions.last().copied();
    let last_injection = last_injected_at.and_then(|index| history.get(index).cloned());
    let last_disclosure = last_injection
        .as_ref()
        .and_then(|message| message.origin.as_ref())
        .and_then(PromptOrigin::injection_disclosure)
        .cloned();
    ContextInjectionContext {
        injected_positions,
        last_injected_at,
        last_injection,
        last_disclosure,
        is_new_turn,
        estimated_tokens: Some(estimated_tokens),
        signal,
    }
}

pub async fn inject_entry(
    runtime: &AgentActorContext<()>,
    entry: Arc<ReminderEntry>,
    is_new_turn: bool,
    estimated_tokens: usize,
    signal: Option<tokio_util::sync::CancellationToken>,
) {
    let context = provider_context(
        runtime,
        &entry,
        is_new_turn,
        estimated_tokens,
        signal.clone(),
    );
    let future = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        (entry.provider)(context)
    })) {
        Ok(future) => future,
        Err(error) => {
            provider_failed(runtime, &entry, &signal, panic_message(error));
            return;
        }
    };
    let content = match std::panic::AssertUnwindSafe(future).catch_unwind().await {
        Ok(Ok(content)) => content,
        Ok(Err(error)) => {
            provider_failed(runtime, &entry, &signal, error);
            return;
        }
        Err(error) => {
            provider_failed(runtime, &entry, &signal, panic_message(error));
            return;
        }
    };
    let entries = actor_context(runtime).entries.lock().clone();
    if !entries.iter().any(|current| Arc::ptr_eq(current, &entry)) {
        return;
    }
    append_result(runtime, &entry, content);
}

pub fn append_result(
    runtime: &AgentActorContext<()>,
    entry: &ReminderEntry,
    content: Option<ContextInjectionResult>,
) {
    let Some(result) = content else {
        return;
    };
    let origin = reminder_origin(&entry.variant, None, result.disclosure);
    let memory = runtime.service::<dyn IAgentContextMemoryService>("contextMemoryService");
    match result.content {
        ContextInjectionContent::Text(text) => {
            if trim_js(&text).is_empty() {
                return;
            }
            memory.append(&[text_context_message(
                crate::human::llm::message::Role::User,
                wrap_system_reminder(&text),
                origin,
            )]);
        }
        ContextInjectionContent::Message(message) => {
            let empty = message.content.is_empty()
                && message
                    .tools
                    .as_ref()
                    .map(|tools| tools.is_empty())
                    .unwrap_or(true);
            if empty {
                return;
            }
            memory.append(&[ContextMessage {
                role: message.role,
                content: message.content,
                tools: message.tools,
                origin: Some(origin),
                ..Default::default()
            }]);
        }
        ContextInjectionContent::Parts(parts) => {
            if parts.is_empty() {
                return;
            }
            memory.append(&[ContextMessage {
                role: crate::human::llm::message::Role::User,
                content: parts,
                origin: Some(origin),
                ..Default::default()
            }]);
        }
    }
}

pub async fn inject(
    runtime: &AgentActorContext<()>,
    is_new_turn: bool,
    signal: Option<tokio_util::sync::CancellationToken>,
) {
    let mut entries = actor_context(runtime).entries.lock().clone();
    if entries.is_empty() {
        return;
    }
    entries.sort_by_key(|entry| {
        REMINDER_VARIANT_PRIORITY
            .iter()
            .find(|(variant, _)| variant == &entry.variant)
            .map(|(_, priority)| *priority)
            .unwrap_or(0)
    });
    let history = runtime
        .service::<dyn IAgentContextMemoryService>("contextMemoryService")
        .get();
    let estimated_tokens = estimate_tokens_for_messages(
        &history.iter().map(ContextMessage::estimable).collect::<Vec<_>>(),
        None,
    ) as usize;
    for entry in entries {
        inject_entry(
            runtime,
            entry,
            is_new_turn,
            estimated_tokens,
            signal.clone(),
        )
        .await;
        if signal.as_ref().is_some_and(|signal| signal.is_cancelled()) {
            return;
        }
    }
}

pub struct AgentReminderService {
    base: AgentActorService,
    actor: AgentActorContext<()>,
    disposed: Arc<Mutex<bool>>,
}

impl AgentReminderService {
    pub fn new(
        dispatcher: Arc<dyn AgentActorHost>,
        agent: AgentContext,
        get: Arc<dyn Fn(&str) -> Arc<dyn std::any::Any + Send + Sync> + Send + Sync>,
        dispatch: Arc<
            dyn Fn(Value) -> crate::rpc::types::BoxFuture<'static, Result<(), String>>
                + Send
                + Sync,
        >,
    ) -> Self {
        let base = AgentActorService::new(dispatcher, agent, get, dispatch);
        let actor = base.attach_actor(
            reminder_actor_logic(),
            AgentActorOptions {
                id: "reminder".to_string(),
                input: None,
                durable: None,
            },
        );
        Self {
            base,
            actor,
            disposed: Arc::new(Mutex::new(false)),
        }
    }

    pub fn register(
        &self,
        variant: &str,
        provider: Arc<
            dyn Fn(
                    ContextInjectionContext,
                ) -> crate::rpc::types::BoxFuture<
                    'static,
                    Result<Option<ContextInjectionResult>, String>,
                > + Send
                + Sync,
        >,
    ) -> Arc<dyn Fn() + Send + Sync> {
        let entry = Arc::new(ReminderEntry {
            provider,
            variant: variant.to_string(),
        });
        (self.actor.send)(XEvent::with_data(
            "reminder.register",
            Value::Null,
            entry.clone(),
        ));
        let disposed = self.disposed.clone();
        let actor = self.actor.clone();
        Arc::new(move || {
            if *disposed.lock() {
                return;
            }
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                (actor.send)(XEvent::with_data(
                    "reminder.unregister",
                    Value::Null,
                    entry.clone(),
                ));
            }));
        })
    }

    pub fn notify(&self, content: &str, notification: &ReminderNotification) {
        append_reminder(&self.actor, content, notification);
    }

    pub async fn reconcile_when_idle(&self, variant: &str) {
        let loop_service = self.actor.service::<dyn AgentLoopService>("loopService");
        let Some(quiescence) = loop_service.try_acquire_quiescence() else {
            return;
        };
        let history = self
            .actor
            .service::<dyn IAgentContextMemoryService>("contextMemoryService")
            .get();
        let estimated_tokens = estimate_tokens_for_messages(
            &history.iter().map(ContextMessage::estimable).collect::<Vec<_>>(),
            None,
        ) as usize;
        let entries = actor_context(&self.actor).entries.lock().clone();
        for entry in entries {
            if entry.variant == variant {
                inject_entry(&self.actor, entry, false, estimated_tokens, None).await;
            }
        }
        drop(quiescence);
    }

    pub fn dispose(&self) {
        *self.disposed.lock() = true;
        self.base.dispose();
    }
}

pub trait IAgentReminderService: ReminderService {}

impl IAgentReminderService for AgentReminderService {}

pub trait ReminderService: Send + Sync {
    fn register(
        &self,
        variant: &str,
        provider: Arc<
            dyn Fn(
                    ContextInjectionContext,
                ) -> crate::rpc::types::BoxFuture<
                    'static,
                    Result<Option<ContextInjectionResult>, String>,
                > + Send
                + Sync,
        >,
    ) -> Arc<dyn Fn() + Send + Sync>;

    fn notify(&self, content: &str, notification: &ReminderNotification);

    fn reconcile_when_idle<'a>(&'a self, variant: &'a str) -> crate::rpc::types::BoxFuture<'a, ()>;
}

impl ReminderService for AgentReminderService {
    fn register(
        &self,
        variant: &str,
        provider: Arc<
            dyn Fn(
                    ContextInjectionContext,
                ) -> crate::rpc::types::BoxFuture<
                    'static,
                    Result<Option<ContextInjectionResult>, String>,
                > + Send
                + Sync,
        >,
    ) -> Arc<dyn Fn() + Send + Sync> {
        AgentReminderService::register(self, variant, provider)
    }

    fn notify(&self, content: &str, notification: &ReminderNotification) {
        AgentReminderService::notify(self, content, notification);
    }

    fn reconcile_when_idle<'a>(&'a self, variant: &'a str) -> crate::rpc::types::BoxFuture<'a, ()> {
        Box::pin(async move {
            AgentReminderService::reconcile_when_idle(self, variant).await;
        })
    }
}

fn provider_failed(
    runtime: &AgentActorContext<()>,
    entry: &ReminderEntry,
    signal: &Option<tokio_util::sync::CancellationToken>,
    error: String,
) {
    if signal.as_ref().is_some_and(|signal| signal.is_cancelled()) {
        return;
    }
    runtime
        .service::<dyn LogService>(I_LOG_SERVICE.name())
        .error(
            "context provider failed; skipping it",
            json!({"name": entry.variant, "error": error}),
        );
}

fn panic_message(error: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = error.downcast_ref::<String>() {
        return message.clone();
    }
    if let Some(message) = error.downcast_ref::<&'static str>() {
        return (*message).to_string();
    }
    "unknown error".to_string()
}

fn reminder_origin(
    variant: &str,
    owner_prompt_id: Option<String>,
    disclosure: Option<Value>,
) -> PromptOrigin {
    PromptOrigin::Injection(InjectionOrigin {
        kind: "injection".to_string(),
        variant: variant.to_string(),
        owner_prompt_id,
        disclosure,
    })
}

fn trim_js(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

fn is_js_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'
            | '\u{000a}'
            | '\u{000b}'
            | '\u{000c}'
            | '\u{000d}'
            | '\u{0020}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'
            | '\u{2001}'
            | '\u{2002}'
            | '\u{2003}'
            | '\u{2004}'
            | '\u{2005}'
            | '\u{2006}'
            | '\u{2007}'
            | '\u{2008}'
            | '\u{2009}'
            | '\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

fn wrap_system_reminder(content: &str) -> String {
    super::systemReminder::wrap_system_reminder(content)
}


pub static I_AGENT_REMINDER_SERVICE: LazyLock<ServiceIdentifier<Arc<dyn IAgentReminderService>>> =
    LazyLock::new(|| create_decorator("agentReminderService"));
