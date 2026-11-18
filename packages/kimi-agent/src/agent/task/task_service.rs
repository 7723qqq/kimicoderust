use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Weak};

use async_trait::async_trait;
use futures_util::future::{BoxFuture, FutureExt, Shared, join_all};
use indexmap::IndexMap;
use parking_lot::Mutex;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::_base::di::lifecycle::IDisposable;
use crate::_base::event::Emitter;
use crate::_base::log::log::ILogService;
use crate::_base::utils::abort::{AbortSignal, user_cancellation_reason};
use crate::_base::utils::xml_escape::{escape_xml, escape_xml_attr, escape_xml_tags};
use crate::agent::actor_service::services::EventBusService;
use crate::agent::context_memory::context_memory::IAgentContextMemoryService;
use crate::agent::context_memory::types::{ContextMessage, PromptOrigin, TaskOrigin};
use crate::agent::tool_executor::tool_executor::IAgentToolRegistryService;
use crate::agent::tool_policy::tool_policy::IAgentToolPolicyService;
use crate::agent::r#loop::r#loop::{IAgentLoopService, LoopNotify, LoopNotifyHandle};
use crate::_base::di::lifecycle::DisposableRef;
use crate::_base::state::state_registry::StateKey;
use crate::agent::state::errors::StateError;
use crate::agent::state::agent_state::IAgentStateService;
use crate::agent::task::task_ops::task_replayable_key;
use crate::agent::tools::task::task_wait::flag::WAIT_FOR_FLAG_ID;
use crate::agent::undo::undo_participants::{UndoParticipant, UndoParticipantRegistry};
use crate::app::config::config::IConfigService;
use crate::app::event::event2::Event2Class;
use crate::app::flag::flag::IFlagService;
use crate::app::task::task::{ITaskHandle, TERMINAL_TASK_STATES, TaskState};
use crate::app::telemetry::context::TelemetryProperties;
use crate::app::telemetry::telemetry::ITelemetryService;
use crate::features::reminder::reminder_service::ReminderService;
use crate::features::reminder::types::{
    ContextInjectionContent, ContextInjectionResult, ReminderNotification,
};
use crate::os::interface::host_clock::IHostClock;
use crate::os::interface::host_timers::IHostTimers;
use crate::session::agent_lifecycle::agent_lifecycle::MAIN_AGENT_ID;
use crate::state::event_dispatcher::IEventDispatcher;
use crate::wire::record::WireAsyncError;
use kimi_core::primitives::errors::{Error2, Error2Options};
use kimi_core::types::ContentPart;

use super::config_section::resolve_agent_task_config;
use super::errors::TaskErrors;
use super::notification_xml::render_notification_xml;
use super::persist::{AgentTaskOutputTail, AgentTaskPersistence};
use super::task::{
    AgentTaskLoadOptions, AgentTaskNotificationContext, AgentTaskNotificationSeverity,
    AgentTaskOutputChunk, AgentTaskOutputSnapshot, AgentTaskTrackOptions, AgentTaskWaitDelivery,
    IAgentTaskEntry, IAgentTaskService, RegisterAgentTaskOptions,
};
use super::task_lifecycle::{
    ForegroundTaskReleaseReason, ManagedTask, SharedManagedTask, TaskLifecycle, detach_entry,
    install_foreground_signal, ready_future, resolve_waiters, terminate_release,
};
use super::task_ops::{
    TaskModelState, TaskNotified, TaskOutput, TaskOutputPayload, TaskStarted, TaskStartedPayload,
    TaskTerminated, TaskTerminatedPayload, TaskWaitDelivered, TaskWaitDeliveredPayload,
};
use super::text_align::align_to_character_start;
use super::tools::format::format_task_record;
use super::types::{
    AgentTask, AgentTaskInfo, AgentTaskInfoBase, AgentTaskKind, AgentTaskSettlement,
    AgentTaskSettlementStatus, AgentTaskSink, AgentTaskStatus,
};
use super::wall_time::format_task_wall_time;

const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_SETTLED_TASK_ENTRIES: usize = 200;
const TERMINAL_RETAINED_OUTPUT_BYTES: usize = 64 * 1024;
const TERMINAL_OUTPUT_TAIL_BYTES: usize = 4 * 1024;
const MAX_TASK_OUTPUT_BYTES: u64 = 16 * 1024 * 1024;
const SIGTERM_GRACE_MS: u64 = 5_000;
const TASK_ID_ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
const SESSION_CLOSED_REASON: &str = "Session closed";
const NOTIFICATION_FALLBACK_PREVIEW_BYTES: i64 = 3_000;
const QUESTION_ANSWER_INLINE_BYTES: i64 = 16_000;
const ACTIVE_BACKGROUND_TASK_INJECTION_VARIANT: &str = "background_task_status";
const TASK_RESUME_TERMINATION_VARIANT: &str = "task_resume_termination";
const ACTIVE_BACKGROUND_TASK_GUIDANCE: &str = "The conversation was compacted, so the earlier messages that started these background tasks are gone — but the tasks are still running from before. Do not start duplicates. Use TaskList to list them, TaskOutput for a non-blocking status/output snapshot, and TaskStop to cancel one — completion arrives via automatic notification.";

pub const SUBAGENT_BACKGROUND_TASK_NOTICE: &str = "You are running as a subagent: ending your turn is your final hand-off to the parent agent, and completion notifications that arrive after it reach no one. Do not end your turn while a background task whose result you need is still running. Keep waiting for it with WaitFor, calling it again after a timeout if needed, or run the command in the foreground with a suitable timeout instead. Still use the waiting time for other useful work on your task when you can.";

static WARNING_CLASS: LazyLock<Arc<Event2Class>> = LazyLock::new(|| {
    Arc::new(Event2Class {
        type_: "warning".to_string(),
        durable: false,
        observable: true,
        agent_domain: true,
        schema: None,
    })
});

pub type AgentTaskNotification = Map<String, Value>;

#[derive(Clone)]
struct AgentTaskNotificationBuildContext {
    message: ContextMessage,
    origin: TaskNotificationOrigin,
    notification: AgentTaskNotification,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TaskNotificationOrigin {
    task_id: String,
    status: String,
    notification_id: String,
}

struct PendingNotificationRequest {
    id: u64,
    handle: Arc<Mutex<LoopNotifyHandle>>,
}

#[derive(Debug)]
struct MessageError(String);

impl fmt::Display for MessageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for MessageError {}

fn output_limit_reason() -> String {
    let mib = MAX_TASK_OUTPUT_BYTES / (1024 * 1024);
    format!(
        "Output limit exceeded: the command produced more than {mib} MiB and was terminated. Redirect large output to a file (e.g. `command > out.txt`) and inspect it in slices instead."
    )
}

pub fn is_agent_task_terminal(status: AgentTaskStatus) -> bool {
    status.is_terminal()
}

fn coerce_timeout_settlement(
    entry: &ManagedTask,
    mut settlement: AgentTaskSettlement,
) -> AgentTaskSettlement {
    if entry.timed_out && settlement.status == AgentTaskSettlementStatus::Killed {
        settlement.status = AgentTaskSettlementStatus::TimedOut;
    }
    settlement
}

fn state_key(name: &str, initial: Value) -> StateKey<Value> {
    StateKey::new(name, move || initial.clone())
}

fn task_state_key() -> StateKey<Value> {
    task_replayable_key().state_key()
}

pub fn task_notification_delivery_key() -> StateKey<Value> {
    state_key("task.notificationDelivery", json!([]))
}

pub fn task_ghosts_key() -> StateKey<Value> {
    state_key("task.ghosts", json!({}))
}

pub fn task_scheduled_notification_keys_key() -> StateKey<Value> {
    state_key("task.scheduledNotificationKeys", json!([]))
}

pub fn task_delivered_notification_keys_key() -> StateKey<Value> {
    state_key("task.deliveredNotificationKeys", json!([]))
}

pub fn task_active_task_reminder_pending_key() -> StateKey<Value> {
    state_key("task.activeTaskReminderPending", json!(false))
}

pub struct AgentTaskServiceDependencies {
    pub telemetry: Arc<dyn ITelemetryService>,
    pub context: Arc<dyn IAgentContextMemoryService>,
    pub config: Arc<dyn IConfigService>,
    pub persistence: Arc<AgentTaskPersistence>,
    pub agent_id: String,
    pub event_bus: Arc<dyn EventBusService>,
    pub dispatcher: Arc<dyn IEventDispatcher>,
    pub reminder: Arc<dyn ReminderService>,
    pub loop_service: Arc<dyn IAgentLoopService>,
    pub undo_participants: Option<Arc<UndoParticipantRegistry>>,
    pub log: Arc<dyn ILogService>,
    pub states: Arc<dyn IAgentStateService>,
    pub flags: Arc<dyn IFlagService>,
    pub tools: Arc<dyn IAgentToolRegistryService>,
    pub policy: Arc<dyn IAgentToolPolicyService>,
    pub lifecycle_active: Arc<dyn Fn() -> bool + Send + Sync>,
    pub clock: Arc<dyn IHostClock>,
    pub timers: Arc<dyn IHostTimers>,
}

pub struct AgentTaskService {
    weak_self: Weak<AgentTaskService>,
    telemetry: Arc<dyn ITelemetryService>,
    context: Arc<dyn IAgentContextMemoryService>,
    config: Arc<dyn IConfigService>,
    persistence: Arc<AgentTaskPersistence>,
    agent_id: String,
    dispatcher: Arc<dyn IEventDispatcher>,
    reminder: Arc<dyn ReminderService>,
    loop_service: Arc<dyn IAgentLoopService>,
    log: Arc<dyn ILogService>,
    states: Arc<dyn IAgentStateService>,
    lifecycle_active_fn: Arc<dyn Fn() -> bool + Send + Sync>,
    clock: Arc<dyn IHostClock>,
    timers: Arc<dyn IHostTimers>,
    on_did_append_output: Emitter<AgentTaskOutputChunk>,
    on_did_change_task: Emitter<AgentTaskInfo>,
    tasks: Mutex<IndexMap<String, SharedManagedTask>>,
    settled_order: Mutex<VecDeque<String>>,
    exit_suppression_armed: AtomicBool,
    building_notification_keys: Mutex<HashSet<String>>,
    pending_notification_requests: Mutex<HashMap<String, PendingNotificationRequest>>,
    next_notification_request_id: AtomicU64,
    notification_restore_queue: tokio::sync::Mutex<()>,
    state_write_lock: Mutex<()>,
    cleanup: Mutex<Vec<Box<dyn FnOnce() + Send>>>,
    disposed: AtomicBool,
    _registrations: Mutex<Vec<DisposableRef>>,
}

impl AgentTaskService {
    pub fn new(dependencies: AgentTaskServiceDependencies) -> Result<Arc<Self>, StateError> {
        let AgentTaskServiceDependencies {
            telemetry,
            context,
            config,
            persistence,
            agent_id,
            event_bus,
            dispatcher,
            reminder,
            loop_service,
            undo_participants,
            log,
            states,
            flags,
            tools,
            policy,
            lifecycle_active,
            clock,
            timers,
        } = dependencies;
        let registrations = vec![
            states.contribute_state(task_state_key())?,
            states.contribute_state(task_notification_delivery_key())?,
            states.contribute_state(task_ghosts_key())?,
            states.contribute_state(task_scheduled_notification_keys_key())?,
            states.contribute_state(task_delivered_notification_keys_key())?,
            states.contribute_state(task_active_task_reminder_pending_key())?,
        ];
        let this = Arc::new_cyclic(|weak_self| Self {
            weak_self: weak_self.clone(),
            telemetry,
            context,
            config,
            persistence,
            agent_id: agent_id.clone(),
            dispatcher,
            reminder: reminder.clone(),
            loop_service,
            log,
            states,
            lifecycle_active_fn: lifecycle_active,
            clock,
            timers,
            on_did_append_output: Emitter::new(),
            on_did_change_task: Emitter::new(),
            tasks: Mutex::new(IndexMap::new()),
            settled_order: Mutex::new(VecDeque::new()),
            exit_suppression_armed: AtomicBool::new(false),
            building_notification_keys: Mutex::new(HashSet::new()),
            pending_notification_requests: Mutex::new(HashMap::new()),
            next_notification_request_id: AtomicU64::new(1),
            notification_restore_queue: tokio::sync::Mutex::new(()),
            state_write_lock: Mutex::new(()),
            cleanup: Mutex::new(Vec::new()),
            disposed: AtomicBool::new(false),
            _registrations: Mutex::new(registrations),
        });
        let weak = Arc::downgrade(&this);
        let unsubscribe = event_bus.subscribe(
            "context.spliced",
            Arc::new(move |event| {
                if let Some(this) = weak.upgrade() {
                    this.handle_context_spliced(&event);
                }
            }),
        );
        this.cleanup.lock().push(unsubscribe);
        let registration = reminder.register(ACTIVE_BACKGROUND_TASK_INJECTION_VARIANT, {
            let weak = Arc::downgrade(&this);
            Arc::new(move |_| {
                let weak = weak.clone();
                Box::pin(async move {
                    let result = weak
                        .upgrade()
                        .and_then(|this| this.active_background_task_reminder());
                    Ok(result.map(|text| ContextInjectionResult {
                        content: ContextInjectionContent::Text(text),
                        disclosure: None,
                    }))
                })
                    as crate::rpc::types::BoxFuture<
                        'static,
                        Result<Option<ContextInjectionResult>, String>,
                    >
            })
        });
        this.cleanup.lock().push(Box::new(move || registration()));
        if this.agent_id != MAIN_AGENT_ID {
            let registration = reminder.register("subagent_background_task", {
                let flags = flags.clone();
                let tools = tools.clone();
                let policy = policy.clone();
                Arc::new(move |context| {
                    let enabled = context.last_injected_at.is_none()
                        && flags.enabled(WAIT_FOR_FLAG_ID)
                        && tools.resolve("WaitFor").is_some()
                        && policy.is_tool_active("WaitFor", None);
                    Box::pin(async move {
                        Ok(enabled.then(|| ContextInjectionResult {
                            content: ContextInjectionContent::Text(
                                SUBAGENT_BACKGROUND_TASK_NOTICE.to_string(),
                            ),
                            disclosure: None,
                        }))
                    })
                        as crate::rpc::types::BoxFuture<
                            'static,
                            Result<Option<ContextInjectionResult>, String>,
                        >
                })
            });
            this.cleanup.lock().push(Box::new(move || registration()));
        }
        if let Some(registry) = undo_participants {
            let participant: Arc<dyn UndoParticipant> = this.clone();
            match registry.register(participant) {
                Ok(cleanup) => this.cleanup.lock().push(cleanup),
                Err(error) => {
                    this.dispose();
                    return Err(StateError::new(error));
                }
            }
        }
        Ok(this)
    }

    fn read_state<T>(&self, key: &StateKey<Value>) -> T
    where
        T: DeserializeOwned + Default,
    {
        serde_json::from_value(self.states.get(key)).unwrap_or_default()
    }

    fn ghosts(&self) -> IndexMap<String, AgentTaskInfo> {
        self.read_state(&task_ghosts_key())
    }

    fn update_ghosts(&self, update: impl FnOnce(&mut IndexMap<String, AgentTaskInfo>)) {
        let _guard = self.state_write_lock.lock();
        let mut ghosts = self.ghosts();
        update(&mut ghosts);
        self.states.set(
            &task_ghosts_key(),
            serde_json::to_value(ghosts).unwrap_or_else(|_| json!({})),
        );
    }

    fn scheduled_notification_keys(&self) -> HashSet<String> {
        self.read_state(&task_scheduled_notification_keys_key())
    }

    fn delivered_notification_keys(&self) -> HashSet<String> {
        self.read_state(&task_delivered_notification_keys_key())
    }

    fn notification_delivery(&self) -> Vec<String> {
        self.read_state(&task_notification_delivery_key())
    }

    fn update_key_set(&self, delivered: bool, update: impl FnOnce(&mut HashSet<String>)) {
        let _guard = self.state_write_lock.lock();
        let key = if delivered {
            task_delivered_notification_keys_key()
        } else {
            task_scheduled_notification_keys_key()
        };
        let mut keys: HashSet<String> = self.read_state(&key);
        update(&mut keys);
        let mut keys = keys.into_iter().collect::<Vec<_>>();
        keys.sort();
        self.states.set(&key, json!(keys));
    }

    fn active_task_reminder_pending(&self) -> bool {
        self.read_state::<bool>(&task_active_task_reminder_pending_key())
    }

    fn set_active_task_reminder_pending(&self, value: bool) {
        let _guard = self.state_write_lock.lock();
        self.states
            .set(&task_active_task_reminder_pending_key(), json!(value));
    }

    fn now_ms(&self) -> f64 {
        self.clock.now().timestamp_millis() as f64
    }

    pub async fn restore_after_replay(&self) -> Result<Vec<AgentTaskInfo>, WireAsyncError> {
        for key in self.notification_delivery() {
            self.update_key_set(true, |keys| {
                keys.insert(key);
            });
        }
        self.restore_ghosts_from_wire();
        self.load_from_disk(Some(AgentTaskLoadOptions {
            replace: Some(false),
        }))
        .await?;
        self.reconcile().await
    }

    fn active_background_task_reminder(&self) -> Option<String> {
        if !self.active_task_reminder_pending() {
            return None;
        }
        self.set_active_task_reminder_pending(false);
        let tasks = self.list(Some(true), None);
        if tasks.is_empty() {
            return None;
        }
        Some(format!(
            "{ACTIVE_BACKGROUND_TASK_GUIDANCE}\n\n{}",
            format_task_list(&tasks, true)
        ))
    }

    fn restore_ghosts_from_wire(&self) {
        let wire: TaskModelState = self.read_state(&task_state_key());
        let live = self.tasks.lock().keys().cloned().collect::<HashSet<_>>();
        self.update_ghosts(|ghosts| {
            for (task_id, info) in wire {
                if !live.contains(&task_id) {
                    ghosts.insert(task_id, info);
                }
            }
        });
    }

    fn handle_context_spliced(&self, event: &Value) {
        if is_compaction_splice(event) {
            self.set_active_task_reminder_pending(true);
        }
        for message in event
            .get("messages")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(origin) = task_origin_from_message(message) {
                self.mark_delivered_notification(&origin);
            }
        }
    }

    pub fn register_task(
        &self,
        task: Arc<dyn AgentTask>,
        options: Option<RegisterAgentTaskOptions>,
    ) -> String {
        match self.try_register_task(task, options) {
            Ok(task_id) => task_id,
            Err(error) => std::panic::panic_any(error),
        }
    }

    pub fn try_register_task(
        &self,
        task: Arc<dyn AgentTask>,
        options: Option<RegisterAgentTaskOptions>,
    ) -> Result<String, WireAsyncError> {
        let options = options.unwrap_or_default();
        let detached = options.detached.unwrap_or(true);
        self.assert_can_register(detached)?;
        let timeout_ms = options.timeout_ms.or_else(|| task.timeout_ms());
        let task_id = generate_task_id(task.id_prefix());
        let entry_options = RegisterAgentTaskOptions {
            detached: Some(detached),
            detachable: options.detachable,
            timeout_ms,
            detach_timeout_ms: options.detach_timeout_ms,
            auto_background_on_timeout: options.auto_background_on_timeout,
            signal: if detached { None } else { options.signal },
        };
        let mut managed = ManagedTask::new(task_id.clone(), entry_options, self.now_ms());
        managed.task = Some(task.clone());
        managed.on_detach_fn = Some({
            let task = task.clone();
            Arc::new(move || task.on_detach())
        });
        let entry = Arc::new(Mutex::new(managed));
        self.tasks.lock().insert(task_id.clone(), entry.clone());
        self.update_ghosts(|ghosts| {
            ghosts.remove(&task_id);
        });
        if let Some(timeout_ms) = timeout_ms.filter(|timeout| *timeout > 0) {
            self.arm_manager_timeout(&entry, timeout_ms);
        }
        let sink = {
            let weak = self.weak_self.clone();
            let sink_entry = entry.clone();
            let append_entry = entry.clone();
            let signal = sink_entry.lock().abort_controller.signal();
            AgentTaskSink::new(
                signal,
                Arc::new(move |chunk| {
                    if let Some(this) = weak.upgrade() {
                        this.append_output(&append_entry, chunk);
                    }
                }),
                Arc::new({
                    let weak = self.weak_self.clone();
                    move |settlement| {
                        let weak = weak.clone();
                        let entry = sink_entry.clone();
                        Box::pin(async move {
                            let Some(this) = weak.upgrade() else {
                                return false;
                            };
                            let settlement = {
                                let entry = entry.lock();
                                coerce_timeout_settlement(&entry, settlement)
                            };
                            this.settle_task(&entry, settlement).await
                        }) as BoxFuture<'static, bool>
                    }
                }),
            )
        };
        let lifecycle_entry = entry.clone();
        let weak = self.weak_self.clone();
        let lifecycle = Box::pin(async move {
            let result = task.start(sink).await;
            if let Err(error) = result {
                let (timed_out, aborted) = {
                    let entry = lifecycle_entry.lock();
                    (entry.timed_out, entry.abort_controller.signal().aborted())
                };
                let status = if timed_out {
                    AgentTaskSettlementStatus::TimedOut
                } else if aborted {
                    AgentTaskSettlementStatus::Killed
                } else {
                    AgentTaskSettlementStatus::Failed
                };
                if let Some(this) = weak.upgrade() {
                    this.settle_task(
                        &lifecycle_entry,
                        AgentTaskSettlement {
                            status,
                            stop_reason: (status == AgentTaskSettlementStatus::Failed)
                                .then(|| error.to_string()),
                        },
                    )
                    .await;
                }
            }
        })
        .boxed()
        .shared();
        entry.lock().lifecycle = lifecycle.clone();
        tokio::spawn(lifecycle);
        self.install_task_foreground_signal(&entry);
        let detached = entry.lock().is_detached();
        if detached {
            self.persist_live_entry(&entry);
            self.record_task_started(self.to_info(&entry));
        }
        self.fire_change(&entry);
        Ok(task_id)
    }

    pub fn track(
        &self,
        handle: Arc<dyn ITaskHandle<Value>>,
        options: AgentTaskTrackOptions,
    ) -> IAgentTaskEntry {
        match self.try_track(handle, options) {
            Ok(entry) => entry,
            Err(error) => std::panic::panic_any(error),
        }
    }

    pub fn try_track(
        &self,
        handle: Arc<dyn ITaskHandle<Value>>,
        options: AgentTaskTrackOptions,
    ) -> Result<IAgentTaskEntry, WireAsyncError> {
        let detached = options.detached.unwrap_or(true);
        self.assert_can_register(detached)?;
        let task_id = generate_task_id(options.id_prefix.as_deref().unwrap_or("task"));
        let entry_options = RegisterAgentTaskOptions {
            detached: Some(detached),
            detachable: None,
            timeout_ms: options.timeout_ms,
            detach_timeout_ms: options.detach_timeout_ms,
            auto_background_on_timeout: None,
            signal: if detached {
                None
            } else {
                options.signal.clone()
            },
        };
        let mut managed = ManagedTask::new(task_id.clone(), entry_options, self.now_ms());
        managed.handle = Some(handle.clone());
        managed.to_info_fn = Some(options.to_info.clone());
        managed.force_stop_fn = options.force_stop.clone();
        managed.on_detach_fn = options.on_detach.clone();
        managed.description = Some(options.description.clone());
        let entry = Arc::new(Mutex::new(managed));
        let on_did_detach = entry
            .lock()
            .foreground_release
            .as_ref()
            .map(|release| release.future())
            .unwrap_or_else(|| ready_future(ForegroundTaskReleaseReason::Terminal));
        self.tasks.lock().insert(task_id.clone(), entry.clone());
        self.update_ghosts(|ghosts| {
            ghosts.remove(&task_id);
        });
        if let Some(timeout_ms) = options.timeout_ms.filter(|timeout| *timeout > 0) {
            self.arm_manager_timeout(&entry, timeout_ms);
        }
        let subscriptions = CancellationToken::new();
        entry.lock().handle_subscription = Some(subscriptions.clone());
        let mut output = handle.on_did_output();
        tokio::spawn({
            let weak = self.weak_self.clone();
            let entry = entry.clone();
            let subscriptions = subscriptions.clone();
            async move {
                loop {
                    let chunk = tokio::select! {
                        _ = subscriptions.cancelled() => break,
                        chunk = output.recv() => chunk,
                    };
                    match chunk {
                        Ok(chunk) => {
                            if let Some(this) = weak.upgrade() {
                                this.append_output(&entry, chunk);
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        });
        let mut states = handle.on_did_change_state();
        tokio::spawn({
            let weak = self.weak_self.clone();
            let entry = entry.clone();
            async move {
                loop {
                    let state = tokio::select! {
                        _ = subscriptions.cancelled() => break,
                        state = states.recv() => state,
                    };
                    match state {
                        Ok(state) if TERMINAL_TASK_STATES.contains(&state) => {
                            let (timed_out, stop_reason) = {
                                let entry = entry.lock();
                                (entry.timed_out, entry.stop_reason.clone())
                            };
                            let status = if timed_out {
                                AgentTaskSettlementStatus::TimedOut
                            } else {
                                match state {
                                    TaskState::Cancelled => AgentTaskSettlementStatus::Killed,
                                    TaskState::Failed => AgentTaskSettlementStatus::Failed,
                                    _ => AgentTaskSettlementStatus::Completed,
                                }
                            };
                            if let Some(this) = weak.upgrade() {
                                this.settle_task(
                                    &entry,
                                    AgentTaskSettlement {
                                        status,
                                        stop_reason,
                                    },
                                )
                                .await;
                            }
                        }
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        });
        let lifecycle = Box::pin(async move {
            let _ = handle.result().await;
        })
        .boxed()
        .shared();
        entry.lock().lifecycle = lifecycle;
        self.install_task_foreground_signal(&entry);
        let detached = entry.lock().is_detached();
        if detached {
            self.persist_live_entry(&entry);
            self.record_task_started(self.to_info(&entry));
        }
        self.fire_change(&entry);
        Ok(IAgentTaskEntry {
            task_id,
            on_did_detach,
        })
    }

    pub fn on_did_append_output(&self) -> crate::_base::event::Event<AgentTaskOutputChunk> {
        self.on_did_append_output.event()
    }

    pub fn on_did_change_task(&self) -> crate::_base::event::Event<AgentTaskInfo> {
        self.on_did_change_task.event()
    }

    pub fn refresh(&self, task_id: &str) {
        let entry = self.tasks.lock().get(task_id).cloned();
        let Some(entry) = entry else {
            return;
        };
        if entry.lock().status.is_terminal() {
            return;
        }
        self.fire_change(&entry);
    }

    fn fire_change(&self, entry: &SharedManagedTask) {
        self.on_did_change_task.fire(self.to_info(entry));
    }

    pub fn get_task(&self, task_id: &str) -> Option<AgentTaskInfo> {
        let entry = self.tasks.lock().get(task_id).cloned();
        if let Some(entry) = entry {
            return Some(self.to_info(&entry));
        }
        self.ghosts().get(task_id).cloned()
    }

    pub fn list(&self, active_only: Option<bool>, limit: Option<usize>) -> Vec<AgentTaskInfo> {
        let active_only = active_only.unwrap_or(true);
        let mut result = Vec::new();
        let entries = self
            .tasks
            .lock()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for entry in entries {
            let info = self.to_info(&entry);
            if !should_list_task(&info, active_only) {
                continue;
            }
            result.push(info);
            if limit.is_some_and(|limit| result.len() >= limit) {
                return result;
            }
        }
        if !active_only {
            for ghost in self.ghosts().into_values() {
                if !should_list_task(&ghost, active_only) {
                    continue;
                }
                result.push(ghost);
                if limit.is_some_and(|limit| result.len() >= limit) {
                    return result;
                }
            }
        }
        result
    }

    pub async fn reconcile_notification_delivery_after_undo(
        &self,
    ) -> Result<(), WireAsyncError> {
        let restored = self
            .notification_delivery()
            .into_iter()
            .collect::<HashSet<_>>();
        let dropped = self
            .pending_notification_requests
            .lock()
            .iter()
            .filter(|(_, request)| request.handle.lock().dropped)
            .map(|(key, request)| (key.clone(), request.id))
            .collect::<Vec<_>>();
        for (key, id) in dropped {
            self.clear_pending_notification(&key, id);
        }
        self.update_key_set(true, |keys| {
            *keys = restored.clone();
        });
        let scheduled = self.scheduled_notification_keys();
        let pending = self.pending_notification_requests.lock();
        let remove = scheduled
            .into_iter()
            .filter(|key| restored.contains(key) || !pending.contains_key(key))
            .collect::<Vec<_>>();
        drop(pending);
        self.update_key_set(false, |keys| {
            for key in remove {
                keys.remove(&key);
            }
        });
        self.restore_agent_task_notifications().await
    }

    pub fn persist_output(&self, task_id: &str) {
        let entry = self.tasks.lock().get(task_id).cloned();
        if let Some(entry) = entry {
            self.start_output_persist_entry(&entry);
        }
    }

    pub async fn load_from_disk(
        &self,
        options: Option<AgentTaskLoadOptions>,
    ) -> Result<(), WireAsyncError> {
        let options = options.unwrap_or_default();
        if options.replace != Some(false) {
            self.update_ghosts(|ghosts| ghosts.clear());
        }
        let tasks = self.persistence.list_tasks().await?;
        for task in tasks {
            if self.tasks.lock().contains_key(task.task_id()) {
                continue;
            }
            self.update_ghosts(|ghosts| {
                let task_id = task.task_id().to_string();
                if let Some(existing) = ghosts.get(&task_id) {
                    let merged = newer_restored_task(existing, &task);
                    ghosts.insert(task_id, merged);
                } else {
                    ghosts.insert(task_id, task);
                }
            });
        }
        Ok(())
    }

    pub async fn reconcile(&self) -> Result<Vec<AgentTaskInfo>, WireAsyncError> {
        let lost_tasks = self.mark_loaded_tasks_lost().await?;
        for info in &lost_tasks {
            self.record_task_terminated(info.clone(), None);
        }
        self.append_previous_session_tasks_reminder();
        self.restore_agent_task_notifications().await?;
        Ok(lost_tasks)
    }

    pub async fn get_output_snapshot(
        &self,
        task_id: &str,
        max_preview_bytes: i64,
    ) -> Result<AgentTaskOutputSnapshot, WireAsyncError> {
        if self.get_task(task_id).is_none() {
            return Ok(empty_output_snapshot());
        }
        let output_queue = self
            .tasks
            .lock()
            .get(task_id)
            .map(|entry| entry.lock().output_write_queue.future());
        if let Some(output_queue) = output_queue {
            output_queue.await;
        }
        let preview_limit = max_preview_bytes.max(0) as u64;
        if let Some(persisted) = self
            .persistence
            .read_task_output_snapshot(task_id, preview_limit as i64)
            .await?
        {
            let full_output_available = self
                .tasks
                .lock()
                .get(task_id)
                .is_none_or(|entry| entry.lock().persistence_error.lock().is_none());
            return Ok(AgentTaskOutputSnapshot {
                output_path: Some(persisted.output_path),
                output_size_bytes: persisted.output_size_bytes,
                preview_bytes: persisted.preview_bytes,
                truncated: persisted.truncated,
                full_output_available,
                preview: persisted.preview,
            });
        }
        let entry = self.tasks.lock().get(task_id).cloned();
        let Some(entry) = entry else {
            return Ok(empty_output_snapshot());
        };
        let entry = entry.lock();
        let available = entry.output_chunks.iter().cloned().collect::<String>().into_bytes();
        let preview_bytes = preview_limit
            .min(available.len() as u64)
            .min(entry.output_size_bytes);
        let preview_offset = align_to_character_start(
            &available,
            available.len().saturating_sub(preview_bytes as usize),
        );
        Ok(AgentTaskOutputSnapshot {
            output_path: None,
            output_size_bytes: entry.output_size_bytes,
            preview_bytes,
            truncated: entry.output_size_bytes > preview_bytes,
            full_output_available: false,
            preview: String::from_utf8_lossy(&available[preview_offset..]).into_owned(),
        })
    }

    pub async fn read_output_tail(
        &self,
        task_id: &str,
        max_bytes: i64,
    ) -> Result<AgentTaskOutputTail, WireAsyncError> {
        if let Some(tail) = self
            .persistence
            .read_task_output_tail_bytes(task_id, max_bytes)
            .await?
        {
            return Ok(tail);
        }
        let snapshot = self.get_output_snapshot(task_id, max_bytes.max(0)).await?;
        Ok(AgentTaskOutputTail {
            offset: 0,
            data: snapshot.preview.into_bytes(),
        })
    }

    pub async fn read_output(
        &self,
        task_id: &str,
        tail: Option<i64>,
    ) -> Result<String, WireAsyncError> {
        let limit = tail.unwrap_or(i64::MAX).max(0);
        Ok(self.get_output_snapshot(task_id, limit).await?.preview)
    }

    pub async fn suppress_terminal_notification(
        &self,
        task_id: &str,
    ) -> Result<(), WireAsyncError> {
        let entry = self.tasks.lock().get(task_id).cloned();
        if let Some(entry) = entry {
            let persist = {
                let mut entry = entry.lock();
                if entry.terminal_notification_suppressed == Some(true) {
                    None
                } else {
                    entry.terminal_notification_suppressed = Some(true);
                    Some(self.persist_live_entry_locked(&mut entry))
                }
            };
            if let Some(persist) = persist {
                persist.await;
            }
            return Ok(());
        }
        Ok(())
    }

    pub fn mark_tasks_delivered_via_wait(&self, tasks: &[AgentTaskWaitDelivery]) {
        if tasks.is_empty() {
            return;
        }
        let mut keys = Vec::new();
        for task in tasks {
            let origin = task_notification_origin(task.task_id.clone(), task.status);
            let key = notification_key(&origin);
            let request = self
                .pending_notification_requests
                .lock()
                .get(&key)
                .map(|request| request.handle.clone());
            if let Some(request) = request {
                drop(request.lock());
            }
            self.mark_delivered_notification(&origin);
            keys.push(key);
        }
        let dispatcher = self.dispatcher.clone();
        let event = TaskWaitDelivered::new(TaskWaitDeliveredPayload {
            agent_id: self.agent_id.clone(),
            keys,
        });
        tokio::spawn(async move {
            let _ = dispatcher.dispatch(event.into()).await;
        });
    }

    pub fn detach(&self, task_id: &str) -> Option<AgentTaskInfo> {
        let entry = self.tasks.lock().get(task_id).cloned();
        let Some(entry) = entry else {
            return self.ghosts().get(task_id).cloned();
        };
        detach_entry(&entry, false, self);
        Some(self.to_info(&entry))
    }

    fn apply_detach_timeout(&self, entry: &SharedManagedTask) {
        let timeout_ms = {
            let mut entry = entry.lock();
            let Some(timeout_ms) = entry.options.detach_timeout_ms else {
                return;
            };
            entry.options.timeout_ms = Some(timeout_ms);
            if let Some(handle) = entry.timeout_handle.take() {
                self.timers.clear_timeout(handle);
            }
            timeout_ms
        };
        if timeout_ms > 0 {
            self.arm_manager_timeout(entry, timeout_ms);
        }
    }

    fn arm_manager_timeout(&self, entry: &SharedManagedTask, timeout_ms: u64) {
        let weak = self.weak_self.clone();
        let entry = entry.clone();
        let handle = self.timers.set_clamped_timeout(
            Arc::new(move || {
                entry.lock().timeout_handle = None;
                let Some(this) = weak.upgrade() else {
                    return;
                };
                if this.can_auto_background_on_timeout(&entry) {
                    detach_entry(&entry, true, this.as_ref());
                    return;
                }
                let weak = this.weak_self.clone();
                tokio::spawn(async move {
                    if let Some(this) = weak.upgrade() {
                        this.terminate_with_grace(
                            &entry,
                            TerminateOptions {
                                stop_reason: None,
                                abort_reason: Arc::new(MessageError("Timed out".to_string())),
                                final_status: AgentTaskSettlementStatus::TimedOut,
                            },
                        )
                        .await;
                    }
                });
            }),
            timeout_ms,
        );
        entry.lock().timeout_handle = Some(handle);
    }

    fn can_auto_background_on_timeout(&self, entry: &SharedManagedTask) -> bool {
        let entry = entry.lock();
        entry.options.auto_background_on_timeout == Some(true) && !entry.is_detached()
    }

    pub async fn stop(
        &self,
        task_id: &str,
        reason: Option<String>,
    ) -> Result<Option<AgentTaskInfo>, WireAsyncError> {
        let entry = self.tasks.lock().get(task_id).cloned();
        let Some(entry) = entry else {
            return Ok(None);
        };
        let normalized = normalize_reason(reason);
        let abort_reason = normalized
            .clone()
            .unwrap_or_else(|| "Aborted".to_string());
        self.terminate_with_grace(
            &entry,
            TerminateOptions {
                stop_reason: normalized,
                abort_reason: Arc::new(MessageError(abort_reason)),
                final_status: AgentTaskSettlementStatus::Killed,
            },
        )
        .await
        .map(Some)
    }

    pub async fn stop_by_user(
        &self,
        task_id: &str,
    ) -> Result<Option<AgentTaskInfo>, WireAsyncError> {
        let entry = self.tasks.lock().get(task_id).cloned();
        let Some(entry) = entry else {
            return Ok(None);
        };
        let reason = user_cancellation_reason();
        self.terminate_with_grace(
            &entry,
            TerminateOptions {
                stop_reason: Some(reason.to_string()),
                abort_reason: Arc::new(reason),
                final_status: AgentTaskSettlementStatus::Killed,
            },
        )
        .await
        .map(Some)
    }

    async fn terminate_with_grace(
        &self,
        entry: &SharedManagedTask,
        options: TerminateOptions,
    ) -> Result<AgentTaskInfo, WireAsyncError> {
        let terminal_persist = {
            let managed = entry.lock();
            managed
                .status
                .is_terminal()
                .then(|| managed.persist_write_queue.future())
        };
        if let Some(persist) = terminal_persist {
            persist.await;
            return Ok(self.to_info(entry));
        }
        let (lifecycle, force_stop) = {
            let mut managed = entry.lock();
            if let Some(handle) = managed.timeout_handle.take() {
                self.timers.clear_timeout(handle);
            }
            if options.final_status == AgentTaskSettlementStatus::TimedOut {
                managed.timed_out = true;
            }
            managed.stop_reason = options.stop_reason.clone();
            if let Some(handle) = &managed.handle {
                handle.cancel();
            } else {
                managed
                    .abort_controller
                    .abort(Some(options.abort_reason.clone()));
            }
            (managed.lifecycle.clone(), force_stop_callback(&managed))
        };
        let grace_ms = resolve_agent_task_config(self.config.as_ref())
            .and_then(|config| config.kill_grace_period_ms)
            .unwrap_or(SIGTERM_GRACE_MS);
        let (sender, receiver) = oneshot::channel();
        let sender = Arc::new(Mutex::new(Some(sender)));
        let grace_timer = self.timers.set_timeout(
            Arc::new(move || {
                if let Some(sender) = sender.lock().take() {
                    let _ = sender.send(());
                }
            }),
            grace_ms,
        );
        let graceful = tokio::select! {
            _ = lifecycle => true,
            _ = receiver => false,
        };
        self.timers.clear_timeout(grace_timer);
        let terminal_persist = {
            let managed = entry.lock();
            managed
                .status
                .is_terminal()
                .then(|| managed.persist_write_queue.future())
        };
        if let Some(persist_queue) = terminal_persist {
            persist_queue.await;
            return Ok(self.to_info(entry));
        }
        if !graceful && let Some(force_stop) = force_stop {
            run_force_stop(force_stop).await;
        }
        let was_terminal = entry.lock().status.is_terminal();
        if !was_terminal {
            self.settle_task(
                entry,
                AgentTaskSettlement {
                    status: options.final_status,
                    stop_reason: options.stop_reason,
                },
            )
            .await;
        }
        let persist_queue = entry.lock().persist_write_queue.future();
        persist_queue.await;
        Ok(self.to_info(entry))
    }

    pub async fn stop_all(
        &self,
        reason: Option<String>,
    ) -> Result<Vec<AgentTaskInfo>, WireAsyncError> {
        let task_ids = self.tasks.lock().keys().cloned().collect::<Vec<_>>();
        let results = join_all(
            task_ids
                .into_iter()
                .map(|task_id| {
                    let task_id = task_id.clone();
                    let reason = reason.clone();
                    async move { self.stop(&task_id, reason).await }
                }),
        )
        .await;
        let mut output = Vec::new();
        for result in results {
            if let Some(info) = result? {
                output.push(info);
            }
        }
        Ok(output)
    }

    pub async fn suppress_all_terminal_notifications(&self) -> Result<(), WireAsyncError> {
        self.exit_suppression_armed.store(true, Ordering::SeqCst);
        let requests = self
            .pending_notification_requests
            .lock()
            .values()
            .map(|request| request.handle.clone())
            .collect::<Vec<_>>();
        for request in requests {
            drop(request.lock());
        }
        Ok(())
    }

    pub async fn stop_all_on_exit(
        &self,
        reason: String,
    ) -> Result<Vec<AgentTaskInfo>, WireAsyncError> {
        self.suppress_all_terminal_notifications().await?;
        if self.keep_alive_on_exit() {
            return Ok(Vec::new());
        }
        self.stop_all(Some(reason)).await
    }

    pub fn dispose(&self) {
        if self.disposed.swap(true, Ordering::SeqCst) {
            return;
        }
        if !self.keep_alive_on_exit() {
            let entries = self
                .tasks
                .lock()
                .values()
                .cloned()
                .collect::<Vec<_>>();
            for entry in entries {
                let force_stop = {
                    let mut entry = entry.lock();
                    if entry.status.is_terminal() {
                        None
                    } else {
                        if let Some(handle) = entry.timeout_handle.take() {
                            self.timers.clear_timeout(handle);
                        }
                        if let Some(handle) = &entry.handle {
                            handle.cancel();
                        } else {
                            entry.abort_controller.abort(Some(Arc::new(MessageError(
                                SESSION_CLOSED_REASON.to_string(),
                            ))));
                        }
                        force_stop_callback(&entry)
                    }
                };
                if let Some(force_stop) = force_stop {
                    tokio::spawn(run_force_stop(force_stop));
                }
            }
        }
        for cleanup in self.cleanup.lock().drain(..) {
            cleanup();
        }
        self.on_did_append_output.dispose();
        self.on_did_change_task.dispose();
    }

    fn keep_alive_on_exit(&self) -> bool {
        resolve_agent_task_config(self.config.as_ref())
            .and_then(|config| config.keep_alive_on_exit)
            == Some(true)
    }

    fn lifecycle_active(&self) -> bool {
        (self.lifecycle_active_fn)()
    }

    fn marks_terminal_notification_suppressed(&self, entry: &ManagedTask) -> bool {
        self.exit_suppression_armed.load(Ordering::SeqCst)
            && !self.keep_alive_on_exit()
            && entry.is_detached()
    }

    pub async fn wait(
        &self,
        task_id: &str,
        timeout_ms: Option<u64>,
        signal: Option<AbortSignal>,
    ) -> Result<Option<AgentTaskInfo>, WireAsyncError> {
        let entry = self.tasks.lock().get(task_id).cloned();
        let Some(entry) = entry else {
            return Ok(self.ghosts().get(task_id).cloned());
        };
        let timeout_ms = timeout_ms.unwrap_or(30_000);
        let (waiter_id, receiver) = {
            let mut managed = entry.lock();
            if managed.status.is_terminal() {
                let persist = managed.persist_write_queue.future();
                drop(managed);
                persist.await;
                return Ok(Some(self.to_info(&entry)));
            }
            if timeout_ms == 0 {
                drop(managed);
                return Ok(Some(self.to_info(&entry)));
            }
            let id = managed.next_waiter_id;
            managed.next_waiter_id += 1;
            let (sender, receiver) = oneshot::channel();
            managed.waiters.push((id, sender));
            (id, receiver)
        };
        let (timeout_sender, timeout_receiver) = oneshot::channel();
        let timeout_sender = Arc::new(Mutex::new(Some(timeout_sender)));
        let timeout = self.timers.set_clamped_timeout(
            Arc::new(move || {
                if let Some(sender) = timeout_sender.lock().take() {
                    let _ = sender.send(());
                }
            }),
            timeout_ms,
        );
        let wait = async move {
            let _ = receiver.await;
        };
        let pending = async {
            tokio::select! {
                _ = wait => {},
                _ = timeout_receiver => {},
            }
        };
        let result = if let Some(signal) = signal {
            crate::_base::utils::abort::abortable(pending, signal).await
        } else {
            Ok(pending.await)
        };
        self.timers.clear_timeout(timeout);
        entry.lock().waiters.retain(|(id, _)| *id != waiter_id);
        result?;
        let persist = {
            let entry = entry.lock();
            entry
                .status
                .is_terminal()
                .then(|| entry.persist_write_queue.future())
        };
        if let Some(persist) = persist {
            persist.await;
        }
        Ok(Some(self.to_info(&entry)))
    }

    pub async fn wait_for_foreground_release(
        &self,
        task_id: &str,
    ) -> Result<Option<ForegroundTaskReleaseReason>, WireAsyncError> {
        let entry = self.tasks.lock().get(task_id).cloned();
        let Some(entry) = entry else {
            return Ok(None);
        };
        let (terminal, persist, release, lifecycle) = {
            let entry = entry.lock();
            (
                entry.status.is_terminal(),
                entry.persist_write_queue.future(),
                entry
                    .foreground_release
                    .as_ref()
                    .map(|release| release.future()),
                entry.lifecycle.clone(),
            )
        };
        if terminal {
            persist.await;
            return Ok(Some(ForegroundTaskReleaseReason::Terminal));
        }
        let Some(release) = release else {
            return Ok(Some(ForegroundTaskReleaseReason::Detached));
        };
        let reason = tokio::select! {
            reason = release => reason,
            _ = lifecycle => ForegroundTaskReleaseReason::Terminal,
        };
        if reason == ForegroundTaskReleaseReason::Terminal {
            let persist = entry.lock().persist_write_queue.future();
            persist.await;
        }
        Ok(Some(reason))
    }

    pub async fn drain_writes(&self) -> Result<(), WireAsyncError> {
        let writes = self
            .tasks
            .lock()
            .values()
            .filter(|entry| !entry.lock().status.is_terminal())
            .map(|entry| entry.lock().output_write_queue.future())
            .collect::<Vec<_>>();
        join_all(writes).await;
        Ok(())
    }

    fn assert_can_register(&self, detached: bool) -> Result<(), WireAsyncError> {
        let Some(max_running_tasks) = resolve_agent_task_config(self.config.as_ref())
            .and_then(|config| config.max_running_tasks)
        else {
            return Ok(());
        };
        if !detached {
            return Ok(());
        }
        let running = self.active_task_count();
        if running < max_running_tasks as usize {
            return Ok(());
        }
        Err(Box::new(Error2::with_options(
            TaskErrors::TASK_LIMIT_EXCEEDED,
            "Too many background tasks are already running.",
            Error2Options {
                details: Some(Map::from_iter([
                    ("running".to_string(), json!(running)),
                    ("max".to_string(), json!(max_running_tasks)),
                ])),
                ..Default::default()
            },
        )))
    }

    fn active_task_count(&self) -> usize {
        self.tasks
            .lock()
            .values()
            .filter(|entry| {
                let entry = entry.lock();
                !entry.status.is_terminal() && entry.starts_detached()
            })
            .count()
    }

    async fn mark_loaded_tasks_lost(&self) -> Result<Vec<AgentTaskInfo>, WireAsyncError> {
        let ghosts = self.ghosts();
        let mut lost_tasks = Vec::new();
        for (task_id, info) in ghosts {
            if info.status().is_terminal() {
                continue;
            }
            let mut updated = info;
            updated.set_status(AgentTaskStatus::Lost);
            if updated.ended_at().is_none() {
                updated.set_ended_at(Some(self.now_ms()));
            }
            self.persistence.write_task(&updated).await?;
            self.update_ghosts(|ghosts| {
                ghosts.insert(task_id, updated.clone());
            });
            lost_tasks.push(updated);
        }
        Ok(lost_tasks)
    }

    fn persist_live_entry_locked(
        &self,
        entry: &mut ManagedTask,
    ) -> Shared<BoxFuture<'static, ()>> {
        let info = self.to_info_locked(entry);
        let persistence = self.persistence.clone();
        let queue = entry.persist_write_queue.push(Box::pin(async move {
            let _ = persistence.write_task(&info).await;
        }));
        tokio::spawn(queue.clone());
        queue
    }

    fn persist_live_entry(&self, entry: &SharedManagedTask) -> Shared<BoxFuture<'static, ()>> {
        self.persist_live_entry_locked(&mut entry.lock())
    }

    fn append_output(&self, entry: &SharedManagedTask, chunk: String) {
        let chunk_bytes = chunk.len();
        let (event, output_event, should_stop) = {
            let mut entry = entry.lock();
            entry.output_size_bytes = entry
                .output_size_bytes
                .saturating_add(chunk_bytes as u64);
            let event = (!chunk.is_empty()).then(|| AgentTaskOutputChunk {
                task_id: entry.task_id.clone(),
                offset: entry.output_size_bytes - chunk_bytes as u64,
                text: chunk.clone(),
            });
            self.append_retained_output(&mut entry, &chunk, chunk_bytes);
            let output_event = self.task_output_event(&entry, &chunk);
            let should_stop = !entry.output_limit_tripped
                && entry.task.as_ref().is_some_and(|task| task.kind() == AgentTaskKind::Process)
                && entry.output_size_bytes > MAX_TASK_OUTPUT_BYTES;
            if should_stop {
                entry.output_limit_tripped = true;
            }
            if !entry.output_limit_tripped {
                if !entry.output_persist_started {
                    entry.pending_output.push(chunk);
                    entry.pending_output_bytes += chunk_bytes;
                    if entry.pending_output_bytes > MAX_OUTPUT_BYTES {
                        self.start_output_persist_locked(&mut entry);
                    }
                } else {
                    self.append_task_output(&mut entry, chunk);
                }
            }
            (event, output_event, should_stop)
        };
        if let Some(event) = event {
            self.on_did_append_output.fire(event);
        }
        if let Some(event) = output_event {
            let dispatcher = self.dispatcher.clone();
            tokio::spawn(async move {
                let _ = dispatcher.dispatch(event.into()).await;
            });
        }
        if should_stop {
            let task_id = entry.lock().task_id.clone();
            let weak = self.weak_self.clone();
            tokio::spawn(async move {
                if let Some(this) = weak.upgrade() {
                    let _ = this.stop(&task_id, Some(output_limit_reason())).await;
                }
            });
        }
    }

    fn task_output_event(&self, entry: &ManagedTask, chunk: &str) -> Option<TaskOutput> {
        if chunk.is_empty() || !entry.is_detached() {
            return None;
        }
        Some(TaskOutput::new(TaskOutputPayload {
            agent_id: self.agent_id.clone(),
            task_id: entry.task_id.clone(),
            chunk: Some(chunk.to_string()),
            output_size_bytes: entry.output_size_bytes,
        }))
    }

    fn append_task_output(&self, entry: &mut ManagedTask, chunk: String) {
        let persistence = self.persistence.clone();
        let task_id = entry.task_id.clone();
        let persistence_error = entry.persistence_error.clone();
        let log = self.log.clone();
        let queue = entry.output_write_queue.push(Box::pin(async move {
            if let Err(error) = persistence.append_task_output(&task_id, &chunk).await {
                let mut slot = persistence_error.lock();
                if slot.is_none() {
                    *slot = Some(error.to_string());
                }
                log.error(
                    "task output write failed",
                    Some(json!({"taskId": task_id, "error": error.to_string()})),
                );
            }
        }));
        tokio::spawn(queue);
    }

    fn start_output_persist_entry(&self, entry: &SharedManagedTask) {
        self.start_output_persist_locked(&mut entry.lock());
    }

    fn start_output_persist_locked(&self, entry: &mut ManagedTask) {
        if entry.output_persist_started {
            return;
        }
        entry.output_persist_started = true;
        if !entry.pending_output.is_empty() {
            self.append_task_output(entry, entry.pending_output.join(""));
        }
        entry.pending_output.clear();
        entry.pending_output_bytes = 0;
    }

    fn append_retained_output(&self, entry: &mut ManagedTask, chunk: &str, chunk_bytes: usize) {
        if chunk_bytes >= MAX_OUTPUT_BYTES {
            let bytes = chunk.as_bytes();
            let retained = String::from_utf8_lossy(&bytes[bytes.len() - MAX_OUTPUT_BYTES..]).into_owned();
            entry.retained_output_bytes = retained.len();
            entry.output_chunks.clear();
            entry.output_chunks.push_back(retained);
            return;
        }
        entry.output_chunks.push_back(chunk.to_string());
        entry.retained_output_bytes += chunk_bytes;
        while entry.retained_output_bytes > MAX_OUTPUT_BYTES {
            let Some(removed) = entry.output_chunks.pop_front() else {
                break;
            };
            entry.retained_output_bytes = entry.retained_output_bytes.saturating_sub(removed.len());
        }
    }

    fn trim_terminal_retained_output(&self, entry: &mut ManagedTask) {
        if entry.persistence_error.lock().is_some()
            || entry.retained_output_bytes <= TERMINAL_RETAINED_OUTPUT_BYTES
        {
            return;
        }
        let retained = entry.output_chunks.iter().cloned().collect::<String>().into_bytes();
        let offset = retained.len().saturating_sub(TERMINAL_RETAINED_OUTPUT_BYTES);
        let offset = align_to_character_start(&retained, offset);
        let tail = String::from_utf8_lossy(&retained[offset..]).into_owned();
        entry.retained_output_bytes = tail.len();
        entry.output_chunks.clear();
        entry.output_chunks.push_back(tail);
    }

    fn record_settled_task(&self, task_id: &str) {
        let max_settled = resolve_agent_task_config(self.config.as_ref())
            .and_then(|config| config.max_settled_tasks)
            .map(|value| value as usize)
            .unwrap_or(MAX_SETTLED_TASK_ENTRIES);
        let evict = {
            let mut order = self.settled_order.lock();
            order.push_back(task_id.to_string());
            let mut evict = Vec::new();
            while order.len() > max_settled {
                let Some(oldest) = order.pop_front() else {
                    break;
                };
                evict.push(oldest);
            }
            evict
        };
        for task_id in evict {
            self.evict_settled_task(&task_id);
        }
    }

    fn evict_settled_task(&self, task_id: &str) {
        let entry = self.tasks.lock().get(task_id).cloned();
        let Some(entry) = entry else {
            return;
        };
        if !entry.lock().status.is_terminal() {
            return;
        }
        let info = self.to_info(&entry);
        if self.tasks.lock().shift_remove(task_id).is_none() {
            return;
        }
        self.update_ghosts(|ghosts| {
            ghosts.insert(task_id.to_string(), info);
        });
    }

    async fn settle_task(
        &self,
        entry: &SharedManagedTask,
        settlement: AgentTaskSettlement,
    ) -> bool {
        {
            let mut entry = entry.lock();
            if entry.status.is_terminal() {
                return false;
            }
            entry.status = settlement.status.into();
            entry.ended_at = Some(self.now_ms());
        }
        self.fire_change(entry);
        let output_persist_started = {
            let mut entry = entry.lock();
            entry.stop_reason = settlement.stop_reason.or_else(|| {
                (settlement.status == AgentTaskSettlementStatus::Killed)
                    .then(|| entry.stop_reason.clone())
                    .flatten()
            });
            if let Some(cleanup) = entry.foreground_signal_cleanup.take() {
                cleanup();
            }
            if let Some(subscriptions) = entry.handle_subscription.take() {
                subscriptions.cancel();
            }
            if let Some(handle) = entry.timeout_handle.take() {
                self.timers.clear_timeout(handle);
            }
            if self.marks_terminal_notification_suppressed(&entry) {
                entry.terminal_notification_suppressed = Some(true);
            }
            entry.output_persist_started
        };
        if output_persist_started {
            let persist = self.persist_live_entry(entry);
            persist.await;
            let output = entry.lock().output_write_queue.future();
            output.await;
            self.trim_terminal_retained_output(&mut entry.lock());
        } else {
            let mut entry = entry.lock();
            entry.pending_output.clear();
            entry.pending_output_bytes = 0;
        }
        let persistence_error = entry.lock().persistence_error.lock().clone();
        if let Some(error) = persistence_error {
            self.warn_output_persistence_failed(entry, &error);
        }
        self.fire_terminal_effects(entry);
        let task_id = entry.lock().task_id.clone();
        self.record_settled_task(&task_id);
        terminate_release(&mut entry.lock());
        resolve_waiters(&mut entry.lock());
        true
    }

    fn warn_output_persistence_failed(&self, entry: &SharedManagedTask, error: &str) {
        let task_id = entry.lock().task_id.clone();
        let event = crate::app::event::event2::Event2::new(
            &WARNING_CLASS,
            json!({
                "agentId": self.agent_id,
                "code": "task_output_persistence_failed",
                "message": format!("Output of task \"{task_id}\" could not be fully written to disk; the stored log may be incomplete ({error})."),
            }),
            None,
        );
        let dispatcher = self.dispatcher.clone();
        let log = self.log.clone();
        tokio::spawn(async move {
            if let Err(error) = dispatcher.dispatch(event).await {
                log.error(
                    "task output warning dispatch failed",
                    Some(json!({"taskId": task_id, "error": error.to_string()})),
                );
            }
        });
    }

    fn fire_terminal_effects(&self, entry: &SharedManagedTask) {
        {
            let mut entry = entry.lock();
            if entry.terminal_fired || !entry.is_detached() {
                return;
            }
            entry.terminal_fired = true;
        }
        let info = self.to_info(entry);
        let weak = self.weak_self.clone();
        let notify_info = info.clone();
        tokio::spawn(async move {
            if let Some(this) = weak.upgrade() {
                if let Err(error) = this.notify_agent_task(notify_info.clone()).await {
                    this.log.error(
                        "task notification delivery failed",
                        Some(json!({"taskId": notify_info.task_id(), "error": error.to_string()})),
                    );
                }
            }
        });
        self.record_task_terminated(info, self.retained_output_tail(entry));
    }

    fn retained_output_tail(&self, entry: &SharedManagedTask) -> Option<String> {
        let entry = entry.lock();
        if entry.output_chunks.is_empty() {
            return None;
        }
        let retained = entry.output_chunks.iter().cloned().collect::<String>().into_bytes();
        let offset = retained.len().saturating_sub(TERMINAL_OUTPUT_TAIL_BYTES);
        Some(String::from_utf8_lossy(&retained[offset..]).into_owned())
    }

    fn record_task_started(&self, info: AgentTaskInfo) {
        if self.lifecycle_active() {
            let dispatcher = self.dispatcher.clone();
            let event = TaskStarted::new(TaskStartedPayload {
                agent_id: self.agent_id.clone(),
                info: info.clone(),
            });
            tokio::spawn(async move {
                let _ = dispatcher.dispatch(event.into()).await;
            });
        }
        self.telemetry.track2(
            "background_task_created",
            Some(telemetry_properties(json!({
                "task_id": info.task_id(),
                "kind": if info.kind() == AgentTaskKind::Process { "bash" } else { info.kind().as_str() },
            }))),
        );
    }

    fn record_task_terminated(&self, info: AgentTaskInfo, output_tail: Option<String>) {
        if self.lifecycle_active() {
            let dispatcher = self.dispatcher.clone();
            let event = TaskTerminated::new(TaskTerminatedPayload {
                agent_id: self.agent_id.clone(),
                info: info.clone(),
                output_tail,
            });
            tokio::spawn(async move {
                let _ = dispatcher.dispatch(event.into()).await;
            });
        } else {
            self.log.warn(
                "task terminal event dropped: agent lifecycle inactive",
                Some(json!({"taskId": info.task_id(), "status": info.status().as_str()})),
            );
        }
        self.telemetry.track2(
            "background_task_completed",
            Some(telemetry_properties(json!({
                "task_id": info.task_id(),
                "kind": info.kind().as_str(),
                "duration_ms": info.ended_at().map(|ended| ended - info.started_at()),
                "status": info.status().as_str(),
            }))),
        );
    }

    async fn notify_agent_task(&self, info: AgentTaskInfo) -> Result<(), WireAsyncError> {
        if !self.lifecycle_active() {
            self.log.warn(
                "task notification skipped: agent lifecycle inactive",
                Some(json!({"taskId": info.task_id(), "status": info.status().as_str()})),
            );
            return Ok(());
        }
        let Some(context) = self.build_agent_task_notification_context(&info).await? else {
            return Ok(());
        };
        if !self.lifecycle_active() || self.is_terminal_notification_suppressed(info.task_id()) {
            self.log.warn(
                "task notification dropped before scheduling",
                Some(json!({"taskId": info.task_id()})),
            );
            return Ok(());
        }
        let key = notification_key(&context.origin);
        if self.delivered_notification_keys().contains(&key) {
            return Ok(());
        }
        let id = self
            .next_notification_request_id
            .fetch_add(1, Ordering::Relaxed);
        let weak = self.weak_self.clone();
        let consume_key = key.clone();
        let consume_notification = context.notification.clone();
        let drop_key = key.clone();
        let handle = self.loop_service.notify(Some(LoopNotify {
            message: Some(context.message.clone()),
            turn_scoped: Some(false),
            on_consume: Some(Arc::new(move || {
                if let Some(this) = weak.upgrade() {
                    this.complete_notification_request(
                        &consume_key,
                        id,
                        &consume_notification,
                    );
                }
            })),
            on_drop: Some(Arc::new({
                let weak = self.weak_self.clone();
                move || {
                    if let Some(this) = weak.upgrade() {
                        this.clear_pending_notification(&drop_key, id);
                    }
                }
            })),
            ..Default::default()
        }));
        self.pending_notification_requests.lock().insert(
            key,
            PendingNotificationRequest {
                id,
                handle: Arc::new(Mutex::new(handle)),
            },
        );
        Ok(())
    }

    async fn restore_agent_task_notifications(&self) -> Result<(), WireAsyncError> {
        let _guard = self.notification_restore_queue.lock().await;
        self.restore_agent_task_notifications_now().await
    }

    async fn restore_agent_task_notifications_now(&self) -> Result<(), WireAsyncError> {
        for info in self.list(Some(false), None) {
            if !is_agent_task_terminal(info.status()) || info.status() == AgentTaskStatus::Lost {
                continue;
            }
            self.restore_agent_task_notification(info).await?;
        }
        Ok(())
    }

    fn append_previous_session_tasks_reminder(&self) {
        let mut tasks = Vec::new();
        for info in self.ghosts().into_values() {
            if info.resume_reminded() == Some(true) || !is_previous_session_termination(&info) {
                continue;
            }
            if self.has_previous_session_reminder(info.task_id())
                || (info.status() == AgentTaskStatus::Lost && self.has_delivered_task_origin(&info))
            {
                self.persist_previous_session_reminder_marker(info);
                continue;
            }
            tasks.push(info);
        }
        if tasks.is_empty() {
            return;
        }
        let lines = tasks
            .iter()
            .map(previous_session_task_line)
            .collect::<Vec<_>>();
        self.reminder.notify(
            &[
                "The user exited the application after your last turn, so your background tasks from the previous session lost contact:".to_string(),
                lines.join("\n"),
                "Don't assume any of them completed; check current state (they may still be running), then re-run or resume only what you still need.".to_string(),
            ]
            .join("\n"),
            &ReminderNotification {
                variant: TASK_RESUME_TERMINATION_VARIANT.to_string(),
                owner_prompt_id: None,
            },
        );
        for info in tasks {
            self.fire_previous_session_lost_task_notification_hook(&info);
            self.persist_previous_session_reminder_marker(info);
        }
    }

    fn has_previous_session_reminder(&self, task_id: &str) -> bool {
        let prefix = format!("- {task_id} \"");
        self.context.get().iter().any(|message| {
            let matches_origin = message.origin.as_ref().is_some_and(|origin| {
                origin.injection_variant() == Some(TASK_RESUME_TERMINATION_VARIANT)
            });
            if !matches_origin {
                return false;
            }
            message.content.iter().any(|part| match part {
                ContentPart::Text { text, .. } => text.lines().any(|line| line.starts_with(&prefix)),
                _ => false,
            })
        })
    }

    fn has_delivered_task_origin(&self, info: &AgentTaskInfo) -> bool {
        let origin = task_notification_origin(info.task_id().to_string(), info.status());
        let key = notification_key(&origin);
        self.notification_delivery().contains(&key)
            || self.delivered_notification_keys().contains(&key)
            || self.has_delivered_notification(&key)
    }

    fn persist_previous_session_reminder_marker(&self, mut info: AgentTaskInfo) {
        info.set_resume_reminded(true);
        let task_id = info.task_id().to_string();
        self.update_ghosts(|ghosts| {
            ghosts.insert(task_id, info.clone());
        });
        let persistence = self.persistence.clone();
        let log = self.log.clone();
        tokio::spawn(async move {
            if let Err(error) = persistence.write_task(&info).await {
                log.error(
                    "previous-session task reminder marker write failed",
                    Some(json!({"taskId": info.task_id(), "error": error.to_string()})),
                );
            }
        });
    }

    fn fire_previous_session_lost_task_notification_hook(&self, info: &AgentTaskInfo) {
        if info.status() != AgentTaskStatus::Lost
            || info.detached() == Some(false)
            || info.terminal_notification_suppressed() == Some(true)
        {
            return;
        }
        let origin = task_notification_origin(info.task_id().to_string(), info.status());
        let key = notification_key(&origin);
        if self.scheduled_notification_keys().contains(&key)
            || self.delivered_notification_keys().contains(&key)
            || self.has_delivered_notification(&key)
        {
            return;
        }
        self.fire_notification_hook(&build_agent_task_notification(
            info,
            self.now_ms(),
            None,
        ));
    }

    async fn restore_agent_task_notification(
        &self,
        info: AgentTaskInfo,
    ) -> Result<(), WireAsyncError> {
        let Some(context) = self.build_agent_task_notification_context(&info).await? else {
            return Ok(());
        };
        self.context.append(&[context.message]);
        self.fire_notification_hook(&context.notification);
        Ok(())
    }

    async fn build_agent_task_notification_context(
        &self,
        info: &AgentTaskInfo,
    ) -> Result<Option<AgentTaskNotificationBuildContext>, WireAsyncError> {
        if info.detached() == Some(false) || info.terminal_notification_suppressed() == Some(true) {
            return Ok(None);
        }
        let origin = task_notification_origin(info.task_id().to_string(), info.status());
        let key = notification_key(&origin);
        if self.building_notification_keys.lock().contains(&key) {
            return Ok(None);
        }
        if self.scheduled_notification_keys().contains(&key) {
            self.log.debug(
                "task notification already scheduled; in-process redelivery suppressed",
                Some(json!({"taskId": info.task_id()})),
            );
            return Ok(None);
        }
        if self.delivered_notification_keys().contains(&key) || self.has_delivered_notification(&key) {
            return Ok(None);
        }
        self.building_notification_keys.lock().insert(key.clone());
        let result: Result<Option<AgentTaskNotificationBuildContext>, WireAsyncError> = async {
            let output = match self.notification_output_snapshot(info).await {
                Ok(output) => output,
                Err(error) => {
                    self.log.error(
                        "task notification output read failed; delivering without output",
                        Some(json!({"taskId": info.task_id(), "error": error.to_string()})),
                    );
                    empty_output_snapshot()
                }
            };
            if self.is_terminal_notification_suppressed(info.task_id())
                || self.scheduled_notification_keys().contains(&key)
                || self.delivered_notification_keys().contains(&key)
                || self.has_delivered_notification(&key)
            {
                return Ok(None);
            }
            self.update_key_set(false, |keys| {
                keys.insert(key.clone());
            });
            let notification = build_agent_task_notification(info, self.now_ms(), Some(&output));
            let text = render_notification_xml(&notification);
            let message = ContextMessage {
                role: crate::human::llm::message::Role::User,
                content: vec![ContentPart::Text { text, meta: None }],
                origin: Some(PromptOrigin::Task(TaskOrigin {
                    kind: "task".to_string(),
                    task_id: origin.task_id.clone(),
                    status: info.status(),
                    notification_id: origin.notification_id.clone(),
                })),
                ..Default::default()
            };
            Ok(Some(AgentTaskNotificationBuildContext {
                message,
                origin,
                notification,
            }))
        }
        .await;
        self.building_notification_keys.lock().remove(&key);
        result
    }

    async fn notification_output_snapshot(
        &self,
        info: &AgentTaskInfo,
    ) -> Result<AgentTaskOutputSnapshot, WireAsyncError> {
        if info.kind() == AgentTaskKind::Question {
            return self
                .get_output_snapshot(info.task_id(), QUESTION_ANSWER_INLINE_BYTES)
                .await;
        }
        let persisted = self.get_output_snapshot(info.task_id(), 0).await?;
        if persisted.full_output_available {
            return Ok(persisted);
        }
        self.get_output_snapshot(info.task_id(), NOTIFICATION_FALLBACK_PREVIEW_BYTES)
            .await
    }

    fn complete_notification_request(
        &self,
        key: &str,
        id: u64,
        notification: &AgentTaskNotification,
    ) {
        let mut requests = self.pending_notification_requests.lock();
        if requests.get(key).is_some_and(|request| request.id == id) {
            requests.remove(key);
            drop(requests);
            self.fire_notification_hook(notification);
        }
    }

    fn fire_notification_hook(&self, notification: &AgentTaskNotification) {
        if !self.lifecycle_active() {
            self.log.warn(
                "task notification hook skipped: agent lifecycle inactive",
                Some(json!({"sourceId": notification_string(notification, "source_id")})),
            );
            return;
        }
        let severity = match notification_string(notification, "severity").as_str() {
            "warning" => AgentTaskNotificationSeverity::Warning,
            _ => AgentTaskNotificationSeverity::Info,
        };
        let event = TaskNotified::new(AgentTaskNotificationContext {
            agent_id: self.agent_id.clone(),
            notification_type: notification_string(notification, "type"),
            title: notification_string(notification, "title"),
            body: notification_string(notification, "body"),
            severity,
            source_kind: notification_string(notification, "source_kind"),
            source_id: notification_string(notification, "source_id"),
        });
        let dispatcher = self.dispatcher.clone();
        tokio::spawn(async move {
            let _ = dispatcher.dispatch(event.into()).await;
        });
    }

    fn is_terminal_notification_suppressed(&self, task_id: &str) -> bool {
        self.exit_suppression_armed.load(Ordering::SeqCst)
            || self
                .tasks
                .lock()
                .get(task_id)
                .is_some_and(|entry| entry.lock().terminal_notification_suppressed == Some(true))
            || self
                .ghosts()
                .get(task_id)
                .is_some_and(|info| info.terminal_notification_suppressed() == Some(true))
    }

    fn mark_delivered_notification(&self, origin: &TaskNotificationOrigin) {
        let key = notification_key(origin);
        self.update_key_set(false, |keys| {
            keys.remove(&key);
        });
        self.pending_notification_requests.lock().remove(&key);
        self.update_key_set(true, |keys| {
            keys.insert(key);
        });
    }

    fn clear_pending_notification(&self, key: &str, id: u64) {
        let removed = {
            let mut requests = self.pending_notification_requests.lock();
            if requests.get(key).is_some_and(|request| request.id == id) {
                requests.remove(key);
                true
            } else {
                false
            }
        };
        if !removed {
            return;
        }
        if !self.delivered_notification_keys().contains(key) && !self.has_delivered_notification(key) {
            self.update_key_set(false, |keys| {
                keys.remove(key);
            });
        }
    }

    fn has_delivered_notification(&self, key: &str) -> bool {
        self.context.get().iter().any(|message| {
            message
                .origin
                .as_ref()
                .and_then(task_notification_origin_from_prompt)
                .is_some_and(|origin| notification_key(&origin) == key)
        })
    }

    fn install_task_foreground_signal(&self, entry: &SharedManagedTask) {
        let weak = self.weak_self.clone();
        let abort_entry = entry.clone();
        install_foreground_signal(
            &mut entry.lock(),
            Arc::new(move || {
                let Some(this) = weak.upgrade() else {
                    return;
                };
                if abort_entry.lock().is_detached() {
                    return;
                }
                let entry = abort_entry.clone();
                tokio::spawn(async move {
                    let reason = user_cancellation_reason();
                    let _ = this
                        .terminate_with_grace(
                            &entry,
                            TerminateOptions {
                                stop_reason: Some(reason.to_string()),
                                abort_reason: Arc::new(reason),
                                final_status: AgentTaskSettlementStatus::Killed,
                            },
                        )
                        .await;
                });
            }),
        );
    }

    fn to_info(&self, entry: &SharedManagedTask) -> AgentTaskInfo {
        self.to_info_locked(&entry.lock())
    }

    fn to_info_locked(&self, entry: &ManagedTask) -> AgentTaskInfo {
        let detached = entry.is_detached();
        let base = AgentTaskInfoBase {
            task_id: entry.task_id.clone(),
            description: entry
                .task
                .as_ref()
                .map(|task| task.description().to_string())
                .or_else(|| entry.description.clone())
                .unwrap_or_default(),
            status: entry.status,
            detached: Some(detached),
            started_at: entry.started_at,
            ended_at: entry.ended_at,
            stop_reason: entry.stop_reason.clone(),
            terminal_notification_suppressed: entry.terminal_notification_suppressed,
            resume_reminded: None,
            timeout_ms: entry.options.timeout_ms.map(|value| value as f64),
            output_path: detached
                .then(|| self.persistence.task_output_file(&entry.task_id).ok())
                .flatten(),
        };
        if let Some(to_info) = &entry.to_info_fn {
            return to_info(base);
        }
        entry
            .task
            .as_ref()
            .expect("registered task must provide to_info")
            .to_info(base)
    }
}

struct TerminateOptions {
    stop_reason: Option<String>,
    abort_reason: crate::_base::errors::errors::SharedError,
    final_status: AgentTaskSettlementStatus,
}

async fn run_force_stop(
    force_stop: Arc<dyn Fn() -> BoxFuture<'static, Result<(), WireAsyncError>> + Send + Sync>,
) {
    let Ok(future) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| force_stop())) else {
        return;
    };
    let _ = std::panic::AssertUnwindSafe(future).catch_unwind().await;
}

fn force_stop_callback(
    entry: &ManagedTask,
) -> Option<Arc<dyn Fn() -> BoxFuture<'static, Result<(), WireAsyncError>> + Send + Sync>> {
    if let Some(force_stop) = &entry.force_stop_fn {
        return Some(force_stop.clone());
    }
    let task = entry.task.clone()?;
    Some(Arc::new(move || {
        let task = task.clone();
        Box::pin(async move { task.force_stop().await })
    }))
}

impl TaskLifecycle for AgentTaskService {
    fn apply_detach_timeout(&self, entry: &SharedManagedTask) {
        AgentTaskService::apply_detach_timeout(self, entry);
    }

    fn start_output_persist(&self, entry: &SharedManagedTask) {
        self.start_output_persist_entry(entry);
    }

    fn persist_live(&self, entry: &SharedManagedTask) {
        self.persist_live_entry(entry);
    }

    fn record_task_started(&self, entry: &SharedManagedTask) {
        self.record_task_started(self.to_info(entry));
    }

    fn fire_change(&self, entry: &SharedManagedTask) {
        AgentTaskService::fire_change(self, entry);
    }
}

impl UndoParticipant for AgentTaskService {
    fn id(&self) -> &str {
        "task.notificationDelivery"
    }

    fn phase(&self) -> Option<&'static str> {
        None
    }

    fn reconcile_after_undo(&self) -> BoxFuture<'static, Result<(), String>> {
        let this = self.weak_self.clone();
        Box::pin(async move {
            let Some(this) = this.upgrade() else {
                return Ok(());
            };
            this.reconcile_notification_delivery_after_undo()
                .await
                .map_err(|error| error.to_string())
        })
    }
}

impl IDisposable for AgentTaskService {
    fn dispose(&self) {
        AgentTaskService::dispose(self);
    }
}

#[async_trait]
impl IAgentTaskService for AgentTaskService {
    fn on_did_append_output(&self) -> crate::_base::event::Event<AgentTaskOutputChunk> {
        AgentTaskService::on_did_append_output(self)
    }

    fn on_did_change_task(&self) -> crate::_base::event::Event<AgentTaskInfo> {
        AgentTaskService::on_did_change_task(self)
    }

    fn refresh(&self, task_id: &str) {
        AgentTaskService::refresh(self, task_id);
    }

    fn track(
        &self,
        handle: Arc<dyn ITaskHandle<Value>>,
        options: AgentTaskTrackOptions,
    ) -> IAgentTaskEntry {
        AgentTaskService::track(self, handle, options)
    }

    fn register_task(
        &self,
        task: Arc<dyn AgentTask>,
        options: Option<RegisterAgentTaskOptions>,
    ) -> String {
        AgentTaskService::register_task(self, task, options)
    }

    fn get_task(&self, task_id: &str) -> Option<AgentTaskInfo> {
        AgentTaskService::get_task(self, task_id)
    }

    fn list(&self, active_only: Option<bool>, limit: Option<usize>) -> Vec<AgentTaskInfo> {
        AgentTaskService::list(self, active_only, limit)
    }

    fn persist_output(&self, task_id: &str) {
        AgentTaskService::persist_output(self, task_id);
    }

    async fn get_output_snapshot(
        &self,
        task_id: &str,
        max_preview_bytes: i64,
    ) -> Result<AgentTaskOutputSnapshot, WireAsyncError> {
        AgentTaskService::get_output_snapshot(self, task_id, max_preview_bytes).await
    }

    async fn read_output(
        &self,
        task_id: &str,
        tail: Option<i64>,
    ) -> Result<String, WireAsyncError> {
        AgentTaskService::read_output(self, task_id, tail).await
    }

    async fn read_output_tail(
        &self,
        task_id: &str,
        max_bytes: i64,
    ) -> Result<AgentTaskOutputTail, WireAsyncError> {
        AgentTaskService::read_output_tail(self, task_id, max_bytes).await
    }

    async fn suppress_terminal_notification(&self, task_id: &str) -> Result<(), WireAsyncError> {
        AgentTaskService::suppress_terminal_notification(self, task_id).await
    }

    async fn suppress_all_terminal_notifications(&self) -> Result<(), WireAsyncError> {
        AgentTaskService::suppress_all_terminal_notifications(self).await
    }

    fn mark_tasks_delivered_via_wait(&self, tasks: &[AgentTaskWaitDelivery]) {
        AgentTaskService::mark_tasks_delivered_via_wait(self, tasks);
    }

    fn detach(&self, task_id: &str) -> Option<AgentTaskInfo> {
        AgentTaskService::detach(self, task_id)
    }

    async fn stop(
        &self,
        task_id: &str,
        reason: Option<String>,
    ) -> Result<Option<AgentTaskInfo>, WireAsyncError> {
        AgentTaskService::stop(self, task_id, reason).await
    }

    async fn stop_by_user(
        &self,
        task_id: &str,
    ) -> Result<Option<AgentTaskInfo>, WireAsyncError> {
        AgentTaskService::stop_by_user(self, task_id).await
    }

    async fn stop_all(&self, reason: Option<String>) -> Result<Vec<AgentTaskInfo>, WireAsyncError> {
        AgentTaskService::stop_all(self, reason).await
    }

    async fn stop_all_on_exit(&self, reason: String) -> Result<Vec<AgentTaskInfo>, WireAsyncError> {
        AgentTaskService::stop_all_on_exit(self, reason).await
    }

    async fn drain_writes(&self) -> Result<(), WireAsyncError> {
        AgentTaskService::drain_writes(self).await
    }

    async fn wait(
        &self,
        task_id: &str,
        timeout_ms: Option<u64>,
        signal: Option<AbortSignal>,
    ) -> Result<Option<AgentTaskInfo>, WireAsyncError> {
        AgentTaskService::wait(self, task_id, timeout_ms, signal).await
    }

    async fn wait_for_foreground_release(
        &self,
        task_id: &str,
    ) -> Result<Option<ForegroundTaskReleaseReason>, WireAsyncError> {
        AgentTaskService::wait_for_foreground_release(self, task_id).await
    }
}

fn empty_output_snapshot() -> AgentTaskOutputSnapshot {
    AgentTaskOutputSnapshot {
        output_path: None,
        output_size_bytes: 0,
        preview_bytes: 0,
        truncated: false,
        full_output_available: false,
        preview: String::new(),
    }
}

fn agent_task_notification_children(
    info: &AgentTaskInfo,
    output: Option<&AgentTaskOutputSnapshot>,
) -> Option<Vec<String>> {
    let output = output?;
    if inlines_question_answer(info, output) {
        return (!output.preview.is_empty()).then(|| vec![render_answer_block(&output.preview)]);
    }
    if output.full_output_available && let Some(path) = &output.output_path {
        return Some(vec![render_output_file_block(
            path,
            output.output_size_bytes,
        )]);
    }
    if output.preview.is_empty() {
        return None;
    }
    Some(vec![render_output_preview_block(output)])
}

fn inlines_question_answer(info: &AgentTaskInfo, output: &AgentTaskOutputSnapshot) -> bool {
    info.kind() == AgentTaskKind::Question && !output.truncated
}

fn render_answer_block(answer: &str) -> String {
    ["<answer>", &escape_xml_tags(answer), "</answer>"].join("\n")
}

fn question_notification_text(
    info: &AgentTaskInfo,
    output: Option<&AgentTaskOutputSnapshot>,
) -> Option<(String, String)> {
    if info.status() != AgentTaskStatus::Completed
        || !output.is_some_and(|output| inlines_question_answer(info, output))
    {
        return None;
    }
    match question_outcome(&output?.preview) {
        Some(true) => Some((
            "Background question answered".to_string(),
            format!("The user answered \"{}\".", info.description()),
        )),
        Some(false) => Some((
            "Background question dismissed".to_string(),
            format!(
                "The user dismissed \"{}\" without answering.",
                info.description()
            ),
        )),
        None => None,
    }
}

fn question_outcome(output: &str) -> Option<bool> {
    let parsed = serde_json::from_str::<Value>(output).ok()?;
    let answers = parsed.get("answers")?.as_object()?;
    Some(!answers.is_empty())
}

fn render_output_file_block(output_path: &str, output_size_bytes: u64) -> String {
    [
        format!(
            "<output-file path=\"{}\" bytes=\"{output_size_bytes}\">",
            escape_xml_attr(output_path)
        ),
        format!(
            "Read the output file to retrieve the result: {}",
            escape_xml(output_path)
        ),
        "</output-file>".to_string(),
    ]
    .join("\n")
}

fn render_output_preview_block(output: &AgentTaskOutputSnapshot) -> String {
    let guidance = if output.truncated {
        format!(
            "Showing the last {} bytes. No persisted full output is available.",
            output.preview_bytes
        )
    } else {
        "No persisted full output is available; this preview is the currently buffered task output."
            .to_string()
    };
    [
        format!(
            "<output-preview bytes=\"{}\" total_bytes=\"{}\" truncated=\"{}\">",
            output.preview_bytes, output.output_size_bytes, output.truncated
        ),
        guidance,
        escape_xml(&output.preview),
        "</output-preview>".to_string(),
    ]
    .join("\n")
}

fn should_list_task(info: &AgentTaskInfo, active_only: bool) -> bool {
    if !info.status().is_terminal() {
        return true;
    }
    if active_only {
        return false;
    }
    info.detached() != Some(false)
}

fn is_compaction_splice(splice: &Value) -> bool {
    splice.get("deleteCount").and_then(Value::as_u64).unwrap_or(0) > 0
        && splice
            .get("messages")
            .and_then(Value::as_array)
            .is_some_and(|messages| {
                messages.iter().any(|message| {
                    message
                        .get("origin")
                        .and_then(|origin| origin.get("kind"))
                        .and_then(Value::as_str)
                        == Some("compaction_summary")
                })
            })
}

fn newer_restored_task(existing: &AgentTaskInfo, loaded: &AgentTaskInfo) -> AgentTaskInfo {
    let existing_terminal = is_agent_task_terminal(existing.status());
    let loaded_terminal = is_agent_task_terminal(loaded.status());
    if existing_terminal && !loaded_terminal {
        return existing.clone();
    }
    if !existing_terminal && loaded_terminal {
        return loaded.clone();
    }
    match (existing.ended_at(), loaded.ended_at()) {
        (Some(existing_ended), Some(loaded_ended)) if loaded_ended >= existing_ended => loaded.clone(),
        (Some(_), Some(_)) => existing.clone(),
        (Some(_), None) => existing.clone(),
        (None, Some(_)) => loaded.clone(),
        (None, None) => loaded.clone(),
    }
}

fn task_notification_origin_from_prompt(origin: &PromptOrigin) -> Option<TaskNotificationOrigin> {
    match origin {
        PromptOrigin::Task(task) => Some(TaskNotificationOrigin {
            task_id: task.task_id.clone(),
            status: task.status.as_str().to_string(),
            notification_id: task.notification_id.clone(),
        }),
        _ => None,
    }
}

fn task_notification_origin_from_value(origin: &Value) -> Option<TaskNotificationOrigin> {
    let object = origin.as_object()?;
    let kind = object.get("kind")?.as_str()?;
    if kind != "background_task" && kind != "task" {
        return None;
    }
    Some(TaskNotificationOrigin {
        task_id: object.get("taskId")?.as_str()?.to_string(),
        status: object.get("status")?.as_str()?.to_string(),
        notification_id: object.get("notificationId")?.as_str()?.to_string(),
    })
}

fn task_origin_from_message(message: &Value) -> Option<TaskNotificationOrigin> {
    task_notification_origin_from_value(message.get("origin")?)
}

fn task_notification_origin(task_id: String, status: AgentTaskStatus) -> TaskNotificationOrigin {
    TaskNotificationOrigin {
        notification_id: task_notification_id(&task_id, status.as_str()),
        task_id,
        status: status.as_str().to_string(),
    }
}

fn task_notification_id(task_id: &str, status: &str) -> String {
    format!("task:{task_id}:{status}")
}

fn notification_key(origin: &TaskNotificationOrigin) -> String {
    format!(
        "{}\0{}\0{}",
        origin.task_id, origin.status, origin.notification_id
    )
}

fn build_agent_task_notification_body(info: &AgentTaskInfo, now_ms: f64) -> String {
    let base_line = if info.status() == AgentTaskStatus::TimedOut {
        format!("{} timed out.", info.description())
    } else if info.status() == AgentTaskStatus::Killed
        && is_serialized_user_cancellation(info.stop_reason())
    {
        format!("{} was stopped by user.", info.description())
    } else if let Some(reason) = info.stop_reason().filter(|reason| !reason.is_empty()) {
        format!(
            "{} {}. Reason: {reason}",
            info.description(),
            if info.status() == AgentTaskStatus::Killed {
                "was stopped"
            } else {
                info.status().as_str()
            }
        )
    } else {
        format!("{} {}.", info.description(), info.status().as_str())
    };
    let timed = format!(
        "Wall time: {}\n{base_line}",
        format_task_wall_time(info, Some(now_ms))
    );
    if info.kind() != AgentTaskKind::Agent || info.status() == AgentTaskStatus::Completed {
        return timed;
    }
    let Some(agent_id) = info.agent_id() else {
        return timed;
    };
    if agent_id == info.task_id() {
        return timed;
    }
    let recovery = [
        String::new(),
        format!(
            "To recover or continue this subagent, call Agent(resume=\"{agent_id}\", prompt=\"Pick up where you left off; redo the last tool call if its result was never observed.\")."
        ),
        format!(
            "Use agent_id (\"{agent_id}\"), NOT source_id / task_id (\"{}\") — the two look alike but only agent_id is accepted by the resume parameter.",
            info.task_id()
        ),
        "Add run_in_background=true to keep it backgrounded, or omit it to take the result inline in the current turn.".to_string(),
        "The subagent retains its full prior context across the restart, but any in-flight tool call lost its result and may need to be redone.".to_string(),
    ]
    .join("\n");
    format!("{timed}{recovery}")
}

fn build_agent_task_notification(
    info: &AgentTaskInfo,
    now_ms: f64,
    output: Option<&AgentTaskOutputSnapshot>,
) -> AgentTaskNotification {
    let question = question_notification_text(info, output);
    let (title, body) = question.unwrap_or_else(|| {
        (
            format!("Background {} {}", info.kind().as_str(), info.status().as_str()),
            build_agent_task_notification_body(info, now_ms),
        )
    });
    let mut notification = Map::from_iter([
        (
            "id".to_string(),
            json!(task_notification_id(info.task_id(), info.status().as_str())),
        ),
        ("category".to_string(), json!("task")),
        (
            "type".to_string(),
            json!(format!("task.{}", info.status().as_str())),
        ),
        ("source_kind".to_string(), json!("background_task")),
        ("source_id".to_string(), json!(info.task_id())),
        ("title".to_string(), json!(title)),
        (
            "severity".to_string(),
            json!(if info.status() == AgentTaskStatus::Completed {
                "info"
            } else {
                "warning"
            }),
        ),
        ("body".to_string(), json!(body)),
    ]);
    if info.kind() == AgentTaskKind::Agent && let Some(agent_id) = info.agent_id() {
        notification.insert("agent_id".to_string(), json!(agent_id));
    }
    if let Some(children) = agent_task_notification_children(info, output) {
        notification.insert("children".to_string(), json!(children));
    }
    notification
}

fn notification_string(notification: &AgentTaskNotification, key: &str) -> String {
    notification
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn generate_task_id(kind: &str) -> String {
    let mut bytes = [0_u8; 8];
    getrandom::fill(&mut bytes).expect("operating system randomness is unavailable");
    let suffix = bytes
        .iter()
        .map(|byte| TASK_ID_ALPHABET[*byte as usize % TASK_ID_ALPHABET.len()] as char)
        .collect::<String>();
    format!("{kind}-{suffix}")
}

fn normalize_reason(reason: Option<String>) -> Option<String> {
    reason.and_then(|reason| {
        let trimmed = reason.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

fn is_serialized_user_cancellation(reason: Option<&str>) -> bool {
    reason.is_some_and(|reason| reason == user_cancellation_reason().to_string())
}

fn previous_session_task_line(info: &AgentTaskInfo) -> String {
    if info.kind() == AgentTaskKind::Agent && let Some(agent_id) = info.agent_id() {
        return format!(
            "- {} \"{}\" (subagent) — resume it with Agent(resume=\"{}\", prompt=\"Pick up where you left off; redo the last tool call if its result was never observed.\") to continue from its prior context.",
            info.task_id(),
            info.description(),
            agent_id
        );
    }
    format!(
        "- {} \"{}\" ({})",
        info.task_id(),
        info.description(),
        if info.kind() == AgentTaskKind::Process {
            "bash"
        } else {
            info.kind().as_str()
        }
    )
}

fn is_previous_session_termination(info: &AgentTaskInfo) -> bool {
    if info.status() == AgentTaskStatus::Lost {
        return true;
    }
    info.status() == AgentTaskStatus::Killed
        && info.terminal_notification_suppressed() == Some(true)
        && info.stop_reason() == Some(SESSION_CLOSED_REASON)
}

fn format_task_list(tasks: &[AgentTaskInfo], active_only: bool) -> String {
    let label = if active_only {
        "active_background_tasks"
    } else {
        "background_tasks"
    };
    let header = format!("{label}: {}", tasks.len());
    if tasks.is_empty() {
        return format!("{header}\nNo background tasks found.");
    }
    format!(
        "{header}\n{}",
        tasks
            .iter()
            .map(|task| {
                let mut value = serde_json::to_value(task).unwrap_or(Value::Null);
                if let Some(object) = value.as_object_mut() {
                    for key in ["startedAt", "endedAt"] {
                        if let Some(timestamp) = object.get(key).and_then(Value::as_f64) {
                            object.insert(key.to_string(), json!(timestamp as u64));
                        }
                    }
                }
                format_task_record(&value)
            })
            .collect::<Vec<_>>()
            .join("\n---\n")
    )
}

fn telemetry_properties(value: Value) -> TelemetryProperties {
    value.as_object().cloned().unwrap_or_default()
}
