#[path = "agentLifecycle/mod.rs"]
pub mod agent_lifecycle;
pub mod agent_records;
pub mod cache_observation;
pub mod errors;
pub mod idle_reclaim;
#[path = "mcp/mod.rs"]
pub mod mcp;
#[path = "process/mod.rs"]
pub mod process;
#[path = "sessionActivity/mod.rs"]
pub mod session_activity;
pub use session_activity as sessionActivity;
#[path = "sessionAgentProfileCatalog/mod.rs"]
pub mod session_agent_profile_catalog;
#[path = "sessionContext/mod.rs"]
pub mod session_context;
#[path = "sessionInstructions/mod.rs"]
pub mod session_instructions;
#[path = "sessionLog/mod.rs"]
pub mod session_log_service;
pub mod session_log;
#[path = "sessionMetadata/mod.rs"]
pub mod session_metadata;
#[path = "sessionTitle/mod.rs"]
pub mod session_title;
#[path = "sessionToolPolicy/mod.rs"]
pub mod session_tool_policy;
#[path = "sessionToolPolicyGate/mod.rs"]
pub mod session_tool_policy_gate;
#[path = "state/mod.rs"]
pub mod state;
#[path = "subagent/mod.rs"]
pub mod subagent;
#[path = "tokenCounting/mod.rs"]
pub mod session_token_counting;
#[path = "usage/mod.rs"]
pub mod session_usage;
#[path = "workdir/mod.rs"]
pub mod workdir;
#[path = "workspaceContext/mod.rs"]
pub mod workspace_context;
#[path = "workspaceInfo/mod.rs"]
pub mod workspace_info;
pub use agent_lifecycle as agentLifecycle;
pub use session_context as sessionContext;
pub use session_metadata as sessionMetadata;
pub mod lock;
pub mod patch;
pub mod sqlite_store;
pub mod title;
pub mod token_counting;
#[path = "usage.rs"]
pub mod usage;
pub mod wake;
pub mod wire_journal;
pub(crate) mod wire_tree;

pub(crate) mod goal_turn;
pub(crate) mod origin;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{Notify, oneshot};

use crate::callbacks::HostCallbacks;
use crate::primitives::errors::{ErrorPayload, PayloadOptions, make_error_payload};
use crate::rpc::types::{
    AskQuestionRequest, AskQuestionResponse, ListToolsResponse, LlmChatRequest, LlmChatResponse,
    PermissionCheckRequest, PermissionDecision, StateReadRequest, StateReadResponse,
    StateWriteRequest, StateWriteResponse, ToolExecuteRequest, ToolExecuteResponse,
};
use crate::turn_events::{
    TurnCancelReason, TurnCancelTarget, TurnEndReason, TurnEvent, TurnInterruptReason,
};
use crate::turn_loop::run_turn::run_turn_continued;
use crate::turn_loop::types::{GoalContext, LLM, LLMMessage, RunTurnInput, ToolInfo, TurnResult};

use goal_turn::GoalTurnEnd;
use wake::{
    enqueue_replayed_wake_locked, enqueue_task_notification_turn, spawn_notification_waiter,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    NewTurn,

    ActiveOrNewTurn,

    ActiveOrNextTurn,

    ActiveTurnOnly,
}

async fn read_turn_clock(callbacks: &Arc<dyn HostCallbacks>) -> u64 {
    let read = StateReadRequest {
        domain: "turn".into(),
        key: String::new(),
        turn_id: String::new(),
        tool_call_id: String::new(),
    };
    match callbacks.state_read(read).await {
        Ok(resp) => resp
            .value
            .get("nextTurnId")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        Err(_) => 0,
    }
}

#[derive(Debug, Clone)]
pub struct TurnRequest {
    pub prompt: LLMMessage,
    pub admission: Admission,

    pub input: serde_json::Value,

    pub origin: serde_json::Value,
}

impl TurnRequest {
    pub fn user(prompt: LLMMessage, admission: Admission) -> Self {
        let input = serde_json::json!([{ "type": "text", "text": prompt.content.clone() }]);
        Self {
            input,
            origin: serde_json::json!({ "kind": "user" }),
            prompt,
            admission,
        }
    }
}

#[derive(Debug, Clone)]
pub enum TurnOutcome {
    Ran(TurnResult),

    CancelledBeforeStart,
}

pub struct TurnReceipt {
    pub turn_id: u64,

    pub prompt_id: String,
    pub created_at: String,
    pub state: PromptSubmissionState,
    pub steered: bool,
    outcome: oneshot::Receiver<Result<TurnOutcome, String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptSubmissionState {
    Running,
    Queued,
    Blocked,
}

impl PromptSubmissionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Queued => "queued",
            Self::Blocked => "blocked",
        }
    }
}

impl TurnReceipt {
    pub async fn outcome(&mut self) -> Result<TurnOutcome, String> {
        (&mut self.outcome)
            .await
            .map_err(|_| "session dropped".to_string())?
    }

    pub fn into_parts(self) -> (u64, oneshot::Receiver<Result<TurnOutcome, String>>) {
        (self.turn_id, self.outcome)
    }
}

#[derive(Debug, Clone)]
pub struct SessionStatus {
    pub active_turn_id: Option<u64>,
    pub pending_turn_ids: Vec<u64>,

    pub engine: Option<crate::rpc::types::EngineExecSummary>,

    pub usage: crate::session::usage::UsageStatus,
}

pub type ToolDefsProvider =
    Arc<dyn Fn() -> futures_util::future::BoxFuture<'static, Vec<ToolInfo>> + Send + Sync>;

pub type GoalProvider =
    Arc<dyn Fn() -> futures_util::future::BoxFuture<'static, Option<GoalContext>> + Send + Sync>;

pub struct SessionConfig {
    pub llm: Arc<dyn LLM>,
    pub callbacks: Arc<dyn HostCallbacks>,

    pub max_steps: u32,

    pub max_attempts: Option<u32>,

    pub max_context_tokens: Option<u32>,

    pub compaction_max_attempts: Option<u32>,
    /// `[llm_requester].request_byte_budget` (v2 `resolveLlmRequestByteBudget`).
    pub llm_request_byte_budget: u64,

    pub permission_mode: Option<crate::permission::PermissionMode>,

    pub tool_defs: ToolDefsProvider,

    pub goal: Option<GoalProvider>,

    pub on_before_turn: Option<Arc<dyn Fn() + Send + Sync>>,

    pub agent_cancel_slot:
        Option<Arc<std::sync::Mutex<Option<crate::subagent::types::ParentCancel>>>>,

    pub steer_slot: Option<Arc<std::sync::Mutex<Option<crate::subagent::types::ParentCancel>>>>,

    pub hook_guard: Option<Arc<crate::tools::external_hooks::HookGuard>>,

    pub print_background: Option<PrintBackgroundPolicy>,

    pub session_id: Option<String>,

    pub task_runner: Option<Arc<crate::storage::TaskRunner>>,

    pub toolset: Option<Arc<crate::tools::NativeToolset>>,

    pub telemetry: Option<crate::turn_loop::types::TelemetryContext>,

    pub steer_trace: Option<kimi_ui_core::SteerJournal>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrintBackgroundMode {
    Exit,

    Drain,

    Steer,
}

impl PrintBackgroundMode {
    #[must_use]
    pub fn from_wire(value: &str) -> Self {
        match value {
            "drain" => Self::Drain,
            "steer" => Self::Steer,
            _ => Self::Exit,
        }
    }

    #[must_use]
    pub fn waits(&self) -> bool {
        matches!(self, Self::Drain | Self::Steer)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PrintBackgroundPolicy {
    pub mode: PrintBackgroundMode,

    pub ceiling_s: u64,

    pub max_turns: u32,
}

pub const PRINT_WAIT_CEILING_S_DEFAULT: u64 = 2_147_483;

pub const PRINT_MAX_TURNS_DEFAULT: u32 = 100_000;

impl PrintBackgroundPolicy {
    #[must_use]
    pub fn from_wire(
        mode: Option<&str>,
        ceiling_s: Option<u64>,
        max_turns: Option<u32>,
    ) -> Option<Self> {
        Some(Self {
            mode: PrintBackgroundMode::from_wire(mode?),
            ceiling_s: ceiling_s
                .filter(|ceiling| *ceiling > 0)
                .unwrap_or(PRINT_WAIT_CEILING_S_DEFAULT),
            max_turns: max_turns
                .filter(|max_turns| *max_turns > 0)
                .unwrap_or(PRINT_MAX_TURNS_DEFAULT),
        })
    }
}

#[derive(Default)]
struct PrintRunState {
    deadline: Option<std::time::Instant>,

    steer_turns: usize,
}

const PRINT_SETTLE_POLL: std::time::Duration = std::time::Duration::from_millis(250);

const CRON_FIRE_GRACE_MS: i64 = 2_000;

struct PendingTurn {
    turn_id: u64,

    prompt_id: Option<String>,
    prompt: LLMMessage,
    input: serde_json::Value,
    origin: serde_json::Value,
    cancel: Arc<AtomicBool>,

    outcome: Option<oneshot::Sender<Result<TurnOutcome, String>>>,

    wake: Option<wake::WakePayload>,
}

struct Core {
    next_turn_id: u64,
    pending: Vec<PendingTurn>,
    active_turn_id: Option<u64>,
    active_cancel: Option<Arc<AtomicBool>>,

    active_wake: Option<wake::WakePayload>,

    deferred_wakes: Vec<wake::WakePayload>,

    wake_waiting_for_user: bool,

    wake_replay_handlers: std::collections::HashMap<String, wake::WakeReplayHandler>,

    wake_replay_inflight: std::collections::HashMap<String, usize>,

    wake_replay_errors: std::collections::HashMap<String, String>,

    steer_waiters: Vec<(u64, oneshot::Sender<Result<TurnOutcome, String>>)>,

    history: Vec<LLMMessage>,

    quiescence_depth: usize,

    held: Vec<HeldTurn>,

    settle_waiters: Vec<oneshot::Sender<()>>,

    released: bool,

    release_waiters: Vec<oneshot::Sender<()>>,

    last_engine: Option<crate::rpc::types::EngineExecSummary>,

    usage: crate::session::usage::UsageStatus,
}

struct HeldTurn {
    turn_id: u64,

    prompt_id: Option<String>,
    request: TurnRequest,
    cancel: Arc<AtomicBool>,
    outcome: Option<oneshot::Sender<Result<TurnOutcome, String>>>,
    wake: Option<wake::WakePayload>,
}

#[derive(Default)]
struct WakeInterruption {
    active: Option<u64>,
    queued: Vec<u64>,
}

#[derive(Default)]
struct TurnPlacement {
    preallocated_id: Option<u64>,
    wake: Option<wake::WakePayload>,
}

struct AdmissionHandles<'a> {
    steer_queue: &'a Mutex<Vec<LLMMessage>>,
    steer_slot: &'a Option<Arc<Mutex<Option<crate::subagent::types::ParentCancel>>>>,
    wakeup: &'a Notify,

    journal: Option<&'a kimi_ui_core::SteerJournal>,
}

fn maybe_settle_locked(core: &mut Core) {
    if core.active_turn_id.is_none()
        && core.pending.is_empty()
        && core.held.is_empty()
        && core.deferred_wakes.is_empty()
        && core.wake_replay_inflight.is_empty()
    {
        for tx in std::mem::take(&mut core.settle_waiters) {
            if tx.send(()).is_err() {
                tracing::debug!("a session settle waiter had already given up");
            }
        }
    }
}

struct ReleaseGuard {
    core: Arc<Mutex<Core>>,
}

impl ReleaseGuard {
    fn new(core: Arc<Mutex<Core>>) -> Self {
        Self { core }
    }
}

impl Drop for ReleaseGuard {
    fn drop(&mut self) {
        let mut core = self.core.lock().unwrap_or_else(|e| e.into_inner());
        core.released = true;
        for tx in std::mem::take(&mut core.release_waiters) {
            if tx.send(()).is_err() {
                tracing::debug!("a session release waiter had already given up");
            }
        }
    }
}

struct SessionContext {
    llm: Arc<dyn LLM>,
    callbacks: Arc<dyn HostCallbacks>,

    steer_trace: Option<kimi_ui_core::SteerJournal>,

    session_id: Option<String>,
    tool_defs: ToolDefsProvider,
    goal: Option<GoalProvider>,
    on_before_turn: Option<Arc<dyn Fn() + Send + Sync>>,
    max_steps: u32,
    max_attempts: Option<u32>,
    max_context_tokens: Option<u32>,
    compaction_max_attempts: Option<u32>,

    llm_request_byte_budget: u64,

    permission_mode: Option<crate::permission::PermissionMode>,

    agent_cancel_slot: Option<Arc<std::sync::Mutex<Option<crate::subagent::types::ParentCancel>>>>,

    steer_slot: Option<Arc<std::sync::Mutex<Option<crate::subagent::types::ParentCancel>>>>,

    hook_guard: Option<Arc<crate::tools::external_hooks::HookGuard>>,

    print_background: Option<PrintBackgroundPolicy>,

    task_runner: Option<Arc<crate::storage::TaskRunner>>,

    toolset: Option<Arc<crate::tools::NativeToolset>>,
    change_notifier: std::sync::Mutex<crate::injection::change_notifier::ChangeNotifierState>,
    agents_md_watch:
        std::sync::Mutex<Option<(std::path::PathBuf, crate::environment::AgentsMdWatch)>>,

    print_run: std::sync::Mutex<PrintRunState>,

    media: crate::llm::media_resolver::MediaResolver,

    media_dropped: crate::llm::media_budget::DroppedMedia,

    media_recovery: crate::llm::media_budget::SharedMediaRecovery,

    telemetry: Option<crate::turn_loop::types::TelemetryContext>,

    last_turn_aborted: Arc<std::sync::atomic::AtomicBool>,

    created_at: std::time::Instant,
}

#[derive(Clone)]
pub struct EngineSession {
    core: Arc<Mutex<Core>>,
    wakeup: Arc<Notify>,

    shutdown: Arc<AtomicBool>,
    steer_queue: Arc<Mutex<Vec<LLMMessage>>>,

    callbacks: Arc<dyn HostCallbacks>,

    agent_cancel_slot: Option<Arc<std::sync::Mutex<Option<crate::subagent::types::ParentCancel>>>>,

    steer_slot: Option<Arc<std::sync::Mutex<Option<crate::subagent::types::ParentCancel>>>>,

    hook_guard: Option<Arc<crate::tools::external_hooks::HookGuard>>,

    media_recovery: crate::llm::media_budget::SharedMediaRecovery,

    steer_trace: Option<kimi_ui_core::SteerJournal>,
}

impl EngineSession {
    pub async fn new(config: SessionConfig) -> Self {
        Self::new_seeded(config, crate::session::usage::UsageStatus::default()).await
    }

    pub async fn new_seeded(
        config: SessionConfig,
        initial_usage: crate::session::usage::UsageStatus,
    ) -> Self {
        let initial_clock = read_turn_clock(&config.callbacks).await;

        let notification_bell = config
            .task_runner
            .as_ref()
            .map(|runner| runner.notification_bell());
        let core = Arc::new(Mutex::new(Core {
            next_turn_id: initial_clock,
            pending: Vec::new(),
            active_turn_id: None,
            active_cancel: None,
            active_wake: None,
            deferred_wakes: Vec::new(),
            wake_waiting_for_user: false,
            wake_replay_handlers: std::collections::HashMap::new(),
            wake_replay_inflight: std::collections::HashMap::new(),
            wake_replay_errors: std::collections::HashMap::new(),
            steer_waiters: Vec::new(),
            history: Vec::new(),
            quiescence_depth: 0,
            held: Vec::new(),
            settle_waiters: Vec::new(),
            released: false,
            release_waiters: Vec::new(),
            last_engine: None,
            usage: initial_usage,
        }));
        let steer_queue = Arc::new(Mutex::new(Vec::new()));

        let media_recovery: crate::llm::media_budget::SharedMediaRecovery = Default::default();
        let ctx = Arc::new(SessionContext {
            llm: config.llm,
            callbacks: Arc::new(
                SteerQueueCallbacks::new(
                    config.callbacks,
                    steer_queue.clone(),
                    config.steer_trace.clone(),
                )
                .with_steer_slot(config.steer_slot.clone()),
            ),
            steer_trace: config.steer_trace.clone(),
            tool_defs: config.tool_defs,
            goal: config.goal,
            on_before_turn: config.on_before_turn,
            max_steps: config.max_steps,
            max_attempts: config.max_attempts,
            max_context_tokens: config.max_context_tokens,
            compaction_max_attempts: config.compaction_max_attempts,
            llm_request_byte_budget: config.llm_request_byte_budget,
            permission_mode: config.permission_mode,
            agent_cancel_slot: config.agent_cancel_slot.clone(),
            steer_slot: config.steer_slot.clone(),
            hook_guard: config.hook_guard.clone(),
            print_background: config.print_background,
            session_id: config.session_id,
            task_runner: config.task_runner.clone(),
            toolset: config.toolset.clone(),
            change_notifier: std::sync::Mutex::new(Default::default()),
            agents_md_watch: std::sync::Mutex::new(None),
            print_run: std::sync::Mutex::new(PrintRunState::default()),
            media: crate::llm::media_resolver::MediaResolver::new(),
            media_dropped: Default::default(),
            media_recovery: media_recovery.clone(),
            telemetry: config.telemetry,
            last_turn_aborted: Arc::new(AtomicBool::new(false)),
            created_at: std::time::Instant::now(),
        });
        let wakeup = Arc::new(Notify::new());
        let shutdown = Arc::new(AtomicBool::new(false));
        let callbacks = ctx.callbacks.clone();
        tokio::spawn(pump(
            core.clone(),
            ctx.clone(),
            wakeup.clone(),
            shutdown.clone(),
            notification_bell,
        ));
        tokio::spawn(heartbeat(ctx, shutdown.clone()));
        Self {
            core,
            wakeup,
            shutdown,
            steer_queue,
            callbacks,
            agent_cancel_slot: config.agent_cancel_slot,
            steer_slot: config.steer_slot,
            hook_guard: config.hook_guard.clone(),
            media_recovery,
            steer_trace: config.steer_trace.clone(),
        }
    }

    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);

        if let Some(flag) = &self
            .core
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .active_cancel
        {
            flag.store(true, Ordering::SeqCst);
        }

        self.wakeup.notify_one();
    }

    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    pub fn enqueue_turn(&self, request: TurnRequest) -> Result<TurnReceipt, String> {
        self.enqueue_turn_with_id(request, None)
    }

    pub fn enqueue_turn_with_id(
        &self,
        mut request: TurnRequest,
        prompt_id: Option<String>,
    ) -> Result<TurnReceipt, String> {
        let (outcome_tx, outcome_rx) = oneshot::channel();
        let mut core = self.core.lock().unwrap_or_else(|e| e.into_inner());

        let user_prompt = request.origin.get("kind").and_then(|kind| kind.as_str()) == Some("user");
        let interruption = if user_prompt {
            Self::interrupt_wakes_locked(&mut core)
        } else {
            WakeInterruption::default()
        };

        if interruption.active.is_some() && request.admission == Admission::ActiveOrNewTurn {
            request.admission = Admission::NewTurn;
        }
        let queued_behind_active = core.active_turn_id.is_some();
        let active_turn_id = core.active_turn_id;
        let admission = request.admission;
        let prompt_text = request.prompt.content.clone();
        let prompt_id = prompt_id
            .unwrap_or_else(|| format!("prompt-{}-{:016x}", core.next_turn_id, fastrand::u64(..)));
        let created_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        if core.quiescence_depth > 0 {
            let turn_id = core.next_turn_id;
            core.next_turn_id += 1;
            core.held.push(HeldTurn {
                turn_id,
                prompt_id: Some(prompt_id.clone()),
                request,
                cancel: Arc::new(AtomicBool::new(false)),
                outcome: Some(outcome_tx),
                wake: None,
            });
            let queue_length = core.held.len();
            drop(core);
            if let Some(journal) = &self.steer_trace {
                journal.record(
                    prompt_id.clone(),
                    kimi_ui_core::SteerStage::Parked { turn_id },
                );
            }
            self.emit_wake_interruption(interruption);
            if user_prompt {
                self.fire_user_prompt_queued(turn_id, &prompt_text, queue_length);
            }
            return Ok(TurnReceipt {
                turn_id,
                prompt_id,
                created_at,
                state: PromptSubmissionState::Queued,
                steered: false,
                outcome: outcome_rx,
            });
        }
        let turn_id = Self::admit_locked(
            &mut core,
            request,
            outcome_tx,
            Some(prompt_id.clone()),
            TurnPlacement::default(),
            AdmissionHandles {
                steer_queue: &self.steer_queue,
                steer_slot: &self.steer_slot,
                wakeup: &self.wakeup,
                journal: self.steer_trace.as_ref(),
            },
        )?;
        let queue_length = core.pending.len();
        let joined_active = active_turn_id.is_some()
            && matches!(
                admission,
                Admission::ActiveTurnOnly | Admission::ActiveOrNewTurn
            )
            && core
                .pending
                .iter()
                .all(|pending| pending.turn_id != turn_id);
        drop(core);
        self.emit_wake_interruption(interruption);
        if user_prompt && queued_behind_active {
            self.fire_user_prompt_queued(turn_id, &prompt_text, queue_length);
        }
        let state = if queued_behind_active && !joined_active {
            PromptSubmissionState::Queued
        } else {
            PromptSubmissionState::Running
        };
        if let Some(journal) = &self.steer_trace {
            journal.record(
                prompt_id.clone(),
                kimi_ui_core::SteerStage::Admitted {
                    turn_id,
                    steered: joined_active,
                },
            );
        }
        Ok(TurnReceipt {
            turn_id,
            prompt_id,
            created_at,
            state,
            steered: joined_active,
            outcome: outcome_rx,
        })
    }

    fn fire_user_prompt_queued(&self, turn_id: u64, prompt: &str, queue_length: usize) {
        let Some(ref guard) = self.hook_guard else {
            return;
        };
        let guard = guard.clone();
        let prompt = prompt.to_string();
        tokio::spawn(async move {
            guard
                .notify_user_prompt_queued(turn_id, &prompt, queue_length)
                .await;
        });
    }

    fn admit_locked(
        core: &mut Core,
        request: TurnRequest,
        outcome_tx: oneshot::Sender<Result<TurnOutcome, String>>,
        prompt_id: Option<String>,
        placement: TurnPlacement,
        handles: AdmissionHandles<'_>,
    ) -> Result<u64, String> {
        let AdmissionHandles {
            steer_queue,
            steer_slot,
            wakeup,
            journal,
        } = handles;
        match request.admission {
            Admission::NewTurn | Admission::ActiveOrNextTurn => {
                let turn_id = match placement.preallocated_id {
                    Some(id) => id,
                    None => {
                        let id = core.next_turn_id;
                        core.next_turn_id += 1;
                        id
                    }
                };
                core.pending.push(PendingTurn {
                    turn_id,
                    prompt_id: prompt_id.clone(),
                    prompt: request.prompt,
                    input: request.input,
                    origin: request.origin,
                    cancel: Arc::new(AtomicBool::new(false)),
                    outcome: Some(outcome_tx),
                    wake: placement.wake,
                });
                wakeup.notify_one();
                Ok(turn_id)
            }
            Admission::ActiveOrNewTurn | Admission::ActiveTurnOnly => {
                if let Some(active) = core.active_turn_id {
                    core.steer_waiters.push((active, outcome_tx));
                    let mut prompt = request.prompt;
                    prompt.prompt_id = prompt_id.clone();
                    prompt.origin = Some(request.origin.clone());
                    steer_queue
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(prompt);
                    if let (Some(journal), Some(id)) = (journal, prompt_id.as_deref()) {
                        journal.record(id, kimi_ui_core::SteerStage::QueuePushed);
                    }

                    if let Some(slot) = steer_slot
                        && let Some(signal) =
                            slot.lock().unwrap_or_else(|e| e.into_inner()).as_ref()
                    {
                        signal.trigger();
                    }
                    Ok(active)
                } else if request.admission == Admission::ActiveTurnOnly {
                    Err("Step request requires an active turn".to_string())
                } else {
                    let turn_id = match placement.preallocated_id {
                        Some(id) => id,
                        None => {
                            let id = core.next_turn_id;
                            core.next_turn_id += 1;
                            id
                        }
                    };
                    core.pending.push(PendingTurn {
                        turn_id,
                        prompt_id: prompt_id.clone(),
                        prompt: request.prompt,
                        input: request.input,
                        origin: request.origin,
                        cancel: Arc::new(AtomicBool::new(false)),
                        outcome: Some(outcome_tx),
                        wake: placement.wake,
                    });
                    wakeup.notify_one();
                    Ok(turn_id)
                }
            }
        }
    }

    pub fn cancel_turn(&self, turn_id: Option<u64>) -> bool {
        enum Decision {
            CancelActive { turn_id: Option<u64> },
            CancelQueued { turn_id: u64 },
            Nothing,
        }
        let decision = {
            let mut core = self.core.lock().unwrap_or_else(|e| e.into_inner());
            let decision = match turn_id {
                Some(id) if core.active_turn_id == Some(id) => match &core.active_cancel {
                    Some(flag) => {
                        flag.store(true, Ordering::SeqCst);
                        core.active_wake = None;
                        Decision::CancelActive { turn_id: Some(id) }
                    }
                    None => Decision::Nothing,
                },
                Some(id) => match core.pending.iter().position(|t| t.turn_id == id) {
                    Some(pos) => {
                        let mut entry = core.pending.remove(pos);
                        entry.cancel.store(true, Ordering::SeqCst);
                        if let Some(tx) = entry.outcome.take() {
                            let _ = tx.send(Ok(TurnOutcome::CancelledBeforeStart));
                        }
                        Decision::CancelQueued { turn_id: id }
                    }
                    None => match core.held.iter().position(|t| t.turn_id == id) {
                        Some(pos) => {
                            let mut entry = core.held.remove(pos);
                            entry.cancel.store(true, Ordering::SeqCst);
                            if let Some(tx) = entry.outcome.take() {
                                let _ = tx.send(Ok(TurnOutcome::CancelledBeforeStart));
                            }
                            Decision::CancelQueued { turn_id: id }
                        }
                        None => Decision::Nothing,
                    },
                },
                None => match &core.active_cancel {
                    Some(flag) => {
                        flag.store(true, Ordering::SeqCst);
                        core.active_wake = None;

                        Decision::CancelActive {
                            turn_id: core.active_turn_id,
                        }
                    }
                    None => Decision::Nothing,
                },
            };
            maybe_settle_locked(&mut core);
            decision
        };
        match decision {
            Decision::CancelActive { turn_id } => {
                if let Some(slot) = &self.agent_cancel_slot
                    && let Some(signal) = slot.lock().unwrap_or_else(|e| e.into_inner()).as_ref()
                {
                    signal.trigger();
                }
                self.callbacks.turn_event(TurnEvent::Cancel {
                    turn_id,
                    target: Some(TurnCancelTarget::Active),
                    reason: Some(TurnCancelReason::UserCancelled),
                });
                true
            }
            Decision::CancelQueued { turn_id } => {
                self.callbacks.turn_event(TurnEvent::Cancel {
                    turn_id: Some(turn_id),
                    target: Some(TurnCancelTarget::Queued),
                    reason: Some(TurnCancelReason::UserCancelled),
                });
                true
            }
            Decision::Nothing => false,
        }
    }

    pub fn status(&self) -> SessionStatus {
        let core = self.core.lock().unwrap_or_else(|e| e.into_inner());
        SessionStatus {
            active_turn_id: core.active_turn_id,
            pending_turn_ids: core.pending.iter().map(|t| t.turn_id).collect(),
            engine: core.last_engine.clone(),
            usage: core.usage.clone(),
        }
    }

    pub fn record_compaction_usage(&self, model: &str, usage: &kimi_core::types::TokenUsage) {
        let mut core = self.core.lock().unwrap_or_else(|e| e.into_inner());
        core.usage.record_compaction(model, usage);
    }

    pub fn try_acquire_quiescence(&self) -> Option<QuiescenceGuard> {
        let mut core = self.core.lock().unwrap_or_else(|e| e.into_inner());
        if core.quiescence_depth > 0
            || core.active_turn_id.is_some()
            || !core.pending.is_empty()
            || !core.held.is_empty()
            || !core.deferred_wakes.is_empty()
            || !core.wake_replay_inflight.is_empty()
        {
            return None;
        }
        core.quiescence_depth += 1;
        Some(QuiescenceGuard {
            core: self.core.clone(),
            steer_queue: self.steer_queue.clone(),
            steer_slot: self.steer_slot.clone(),
            wakeup: self.wakeup.clone(),
            steer_trace: self.steer_trace.clone(),
        })
    }

    pub fn is_settled(&self) -> bool {
        let core = self.core.lock().unwrap_or_else(|e| e.into_inner());
        core.active_turn_id.is_none()
            && core.pending.is_empty()
            && core.held.is_empty()
            && core.deferred_wakes.is_empty()
            && core.wake_replay_inflight.is_empty()
    }

    pub async fn settled(&self) -> Result<(), crate::primitives::deadline::WaitTimeout> {
        let (rx, started) = {
            let mut core = self.core.lock().unwrap_or_else(|e| e.into_inner());
            if core.active_turn_id.is_none()
                && core.pending.is_empty()
                && core.held.is_empty()
                && core.deferred_wakes.is_empty()
                && core.wake_replay_inflight.is_empty()
            {
                return Ok(());
            }
            let (tx, rx) = oneshot::channel();
            core.settle_waiters.push(tx);
            (rx, std::time::Instant::now())
        };
        Self::drain("session settle", rx, started).await
    }

    pub fn is_released(&self) -> bool {
        self.core.lock().unwrap_or_else(|e| e.into_inner()).released
    }

    pub async fn released(&self) -> Result<(), crate::primitives::deadline::WaitTimeout> {
        let (rx, started) = {
            let mut core = self.core.lock().unwrap_or_else(|e| e.into_inner());
            if core.released {
                return Ok(());
            }
            let (tx, rx) = oneshot::channel();
            core.release_waiters.push(tx);
            (rx, std::time::Instant::now())
        };
        Self::drain("session release", rx, started).await
    }

    async fn drain(
        operation: &str,
        rx: oneshot::Receiver<()>,
        started: std::time::Instant,
    ) -> Result<(), crate::primitives::deadline::WaitTimeout> {
        match rx.await {
            Ok(()) => Ok(()),
            Err(_) => Err(crate::primitives::deadline::WaitTimeout {
                operation: operation.to_string(),
                stage: "the session was dropped before it reached that state".to_string(),
                limit: std::time::Duration::MAX,
                waited: started.elapsed(),
            }),
        }
    }

    pub fn set_history(&self, history: Vec<LLMMessage>) {
        self.core.lock().unwrap_or_else(|e| e.into_inner()).history = history;

        self.media_recovery
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .advance_revision();
    }

    pub fn clear_history(&self) {
        self.core
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .history
            .clear();
        self.media_recovery
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .advance_revision();
    }

    pub fn extend_history(&self, msgs: Vec<LLMMessage>) {
        self.core
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .history
            .extend(msgs);
    }

    pub fn history_len(&self) -> usize {
        self.core
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .history
            .len()
    }

    pub fn snapshot_history(&self) -> Vec<LLMMessage> {
        self.core
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .history
            .clone()
    }
}

pub struct QuiescenceGuard {
    core: Arc<Mutex<Core>>,
    steer_queue: Arc<Mutex<Vec<LLMMessage>>>,
    steer_slot: Option<Arc<Mutex<Option<crate::subagent::types::ParentCancel>>>>,
    wakeup: Arc<Notify>,
    steer_trace: Option<kimi_ui_core::SteerJournal>,
}

impl Drop for QuiescenceGuard {
    fn drop(&mut self) {
        let held = {
            let mut core = self.core.lock().unwrap_or_else(|e| e.into_inner());
            core.quiescence_depth = core.quiescence_depth.saturating_sub(1);
            if core.quiescence_depth > 0 {
                return;
            }
            std::mem::take(&mut core.held)
        };

        {
            let mut core = self.core.lock().unwrap_or_else(|e| e.into_inner());
            for entry in held {
                let HeldTurn {
                    turn_id,
                    prompt_id,
                    request,
                    cancel: _,
                    outcome,
                    wake,
                } = entry;

                let outcome = outcome.unwrap_or_else(|| {
                    let (tx, _rx) = oneshot::channel();
                    tx
                });
                let _ = EngineSession::admit_locked(
                    &mut core,
                    request,
                    outcome,
                    prompt_id,
                    TurnPlacement {
                        preallocated_id: Some(turn_id),
                        wake,
                    },
                    AdmissionHandles {
                        steer_queue: &self.steer_queue,
                        steer_slot: &self.steer_slot,
                        wakeup: &self.wakeup,
                        journal: self.steer_trace.as_ref(),
                    },
                );
            }
        }
        self.wakeup.notify_one();
    }
}

async fn heartbeat(ctx: Arc<SessionContext>, shutdown: Arc<AtomicBool>) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        heartbeat_tick(
            ctx.hook_guard.as_deref(),
            ctx.created_at.elapsed().as_millis() as u64,
        )
        .await;
    }
}

async fn heartbeat_tick(guard: Option<&crate::tools::external_hooks::HookGuard>, uptime_ms: u64) {
    let Some(guard) = guard else {
        return;
    };
    if !guard.has_hooks_for("SessionHeartbeat") {
        return;
    }
    guard
        .notify_session_lifecycle(
            "SessionHeartbeat",
            "",
            serde_json::json!({
                "session_title": "",
                "uptime_ms": uptime_ms,
            }),
        )
        .await;
}

fn agents_md_change_reminder(changes: &[crate::environment::AgentsMdChange]) -> Option<String> {
    use crate::environment::AgentsMdChange;

    let mut stale = Vec::new();
    let mut fresh = Vec::new();
    for change in changes {
        match change {
            AgentsMdChange::Modified { path } => stale.push(format!("- {}", path.display())),
            AgentsMdChange::Removed { path } => {
                stale.push(format!("- {} (deleted)", path.display()));
            }
            AgentsMdChange::BecameUnreadable { path, .. } => {
                stale.push(format!("- {} (unreadable)", path.display()));
            }
            AgentsMdChange::Added { path } | AgentsMdChange::BecameReadable { path } => {
                fresh.push(format!("- {}", path.display()));
            }
        }
    }
    let mut sections = Vec::new();
    if !stale.is_empty() {
        sections.push(format!(
            "The AGENTS.md instruction file(s) below changed on disk after they were injected into the system prompt:\n{}\nRead the current file(s) and follow the latest contents; the copies injected in the system prompt are stale.",
            stale.join("\n")
        ));
    }
    if !fresh.is_empty() {
        sections.push(format!(
            "The following AGENTS.md file(s) exist in your workspace but were not included in your system prompt:\n{}\nRead them before making changes in those directories.",
            fresh.join("\n")
        ));
    }
    (!sections.is_empty()).then(|| sections.join("\n\n"))
}

async fn inject_change_notifier_reminders(ctx: &SessionContext, history: &mut Vec<LLMMessage>) {
    let Some(toolset) = ctx.toolset.as_ref() else {
        return;
    };
    let listing = toolset.skill_listing_outcome();
    let profiles = match toolset.subagent_manager() {
        Some(manager) => Some(
            manager
                .profile_listing()
                .await
                .into_iter()
                .map(
                    |profile| crate::injection::change_notifier::SubagentProfile {
                        name: profile.name,
                        description: profile.description,
                        when_to_use: profile.when_to_use,
                    },
                )
                .collect::<Vec<_>>(),
        ),
        None => None,
    };
    let pool = toolset.secondary_model().map(|runtime| {
        let config = runtime.config();
        crate::injection::change_notifier::ModelPoolSnapshot {
            default_model: Some(config.default_model.clone()),
            models: config
                .models
                .iter()
                .map(|entry| (entry.alias.clone(), entry.hint.clone()))
                .collect(),
        }
    });
    let workspace = ctx
        .session_id
        .as_deref()
        .and_then(|session_id| {
            toolset
                .subagent_manager()
                .and_then(|manager| manager.session_workspace_roots_sync(session_id))
        })
        .or_else(|| toolset.workspace_roots().ok());
    let workspace_snapshot =
        workspace.as_ref().map(
            |roots| crate::injection::change_notifier::WorkspaceSnapshot {
                additional: roots
                    .additional_dirs()
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect(),
            },
        );
    let cwd = workspace
        .as_ref()
        .map(|roots| roots.work_dir().display().to_string())
        .unwrap_or_else(|| toolset.workspace_root().display().to_string());
    let root = workspace
        .as_ref()
        .map(|roots| roots.work_dir().to_path_buf())
        .unwrap_or_else(|| toolset.workspace_root().to_path_buf());
    let agents_md_reminder = {
        let home = toolset.user_home_dir();
        let mut slot = ctx
            .agents_md_watch
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if slot.as_ref().map_or(true, |(watched, _)| watched != &root) {
            *slot = Some((root.clone(), toolset.start_agents_md_watch(&root)));
        }
        let (_, watch) = slot.as_mut().unwrap();
        let changes = watch.check(&root, None, home.as_deref());
        agents_md_change_reminder(&changes)
    };
    let filter = toolset.tools_filter();
    let is_tool_active = |name: &str| filter.map_or(true, |filter| filter.allows(name));
    let mut state = ctx
        .change_notifier
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let mut reminders = Vec::new();
    if let Some(text) = agents_md_reminder {
        reminders.push((
            text,
            crate::injection::change_notifier::AGENTS_MD_CHANGE_VARIANT,
        ));
    }
    if let Some(text) = state.skills(listing, ctx.llm.system_prompt(), &is_tool_active) {
        reminders.push((
            text,
            crate::injection::change_notifier::SKILLS_CHANGE_VARIANT,
        ));
    }
    if let Some(profiles) = profiles
        && let Some(text) = state.subagents(&profiles, pool, &is_tool_active)
    {
        reminders.push((
            text,
            crate::injection::change_notifier::SUBAGENTS_CHANGE_VARIANT,
        ));
    }
    if let Some(text) = state.workspace_dirs(workspace_snapshot, &cwd) {
        reminders.push((
            text,
            crate::injection::change_notifier::WORKSPACE_DIRS_CHANGE_VARIANT,
        ));
    }
    drop(state);
    for (text, variant) in reminders {
        append_injected_message(
            ctx,
            history,
            crate::injection::injection_message_named(text, variant),
        );
    }
}

fn append_injected_message(
    ctx: &SessionContext,
    history: &mut Vec<LLMMessage>,
    message: LLMMessage,
) {
    if let Some(context_message) = crate::session::agent_records::context_message(&message) {
        ctx.callbacks.emit_event(serde_json::json!({
            "type": "context.append_message",
            "agentId": crate::callbacks::MAIN_AGENT_ID,
            "message": context_message,
        }));
    }
    history.push(message);
}

fn agent_turn_outcome(reason: TurnEndReason) -> Option<&'static str> {
    match reason {
        TurnEndReason::Completed => Some("done"),
        TurnEndReason::Cancelled => Some("aborted"),
        TurnEndReason::Failed => Some("failed"),
        TurnEndReason::Blocked => None,
    }
}

fn inject_plugin_reminders(ctx: &SessionContext, history: &mut Vec<LLMMessage>) {
    let Some(session_id) = ctx.session_id.as_deref() else {
        return;
    };
    if let Some(text) = crate::injection::plugin::take_plugin_change_notices(session_id) {
        append_injected_message(
            ctx,
            history,
            crate::injection::injection_message_named(
                text,
                crate::injection::plugin::PLUGIN_CHANGE_VARIANT,
            ),
        );
    }
    let Some(toolset) = ctx.toolset.as_ref() else {
        return;
    };
    let dirs = toolset.plugin_skill_dirs();
    if dirs.is_empty() {
        return;
    }
    if let Some(text) =
        crate::injection::plugin::session_start_reminder(dirs, session_id, history, false)
    {
        append_injected_message(
            ctx,
            history,
            crate::injection::injection_message_named(
                text,
                crate::injection::plugin::PLUGIN_SESSION_START_VARIANT,
            ),
        );
    }
}

async fn pump(
    core: Arc<Mutex<Core>>,
    ctx: Arc<SessionContext>,
    wakeup: Arc<Notify>,
    shutdown: Arc<AtomicBool>,
    mut notification_bell: Option<tokio::sync::watch::Receiver<u64>>,
) {
    let _release = ReleaseGuard::new(core.clone());
    loop {
        if shutdown.load(Ordering::SeqCst) {
            if let (Some(runner), Some(session_id)) = (&ctx.task_runner, &ctx.session_id) {
                runner
                    .stop_session(session_id, Some("Session closed"))
                    .await;
            }
            return;
        }

        let next = {
            let mut core = core.lock().unwrap_or_else(|e| e.into_inner());
            if core.active_turn_id.is_some() {
                None
            } else {
                let pos = core
                    .pending
                    .iter()
                    .position(|t| !t.cancel.load(Ordering::SeqCst));
                pos.map(|pos| {
                    let entry = core.pending.remove(pos);
                    core.active_turn_id = Some(entry.turn_id);
                    core.active_cancel = Some(entry.cancel.clone());
                    core.active_wake = entry.wake.clone();
                    (entry, core.history.clone())
                })
            }
        };

        let (mut entry, mut history) = match next {
            Some(pair) => pair,
            None => {
                if ctx.print_background.is_none() {
                    let mut core = core.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(receipt) = enqueue_task_notification_turn(&ctx, &mut core, &wakeup)
                    {
                        spawn_notification_waiter(receipt);
                        continue;
                    }
                }

                match &mut notification_bell {
                    Some(bell) => {
                        tokio::select! {
                            _ = wakeup.notified() => {}
                            changed = bell.changed() => {
                                let _ = changed;
                            }
                        }
                    }
                    None => wakeup.notified().await,
                }
                continue;
            }
        };

        inject_plugin_reminders(&ctx, &mut history);
        inject_change_notifier_reminders(&ctx, &mut history).await;

        let history_len = history.len();

        let mut entry_outcome = entry.outcome.take();
        let PendingTurn {
            turn_id,
            prompt_id,
            prompt,
            input,
            origin,
            cancel,
            wake,
            ..
        } = entry;
        let started = std::time::Instant::now();
        if let Some(slot) = &ctx.agent_cancel_slot {
            *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(
                crate::subagent::types::ParentCancel::from_flag(cancel.clone()),
            );
        }

        if let Some(slot) = &ctx.steer_slot {
            *slot.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(crate::subagent::types::ParentCancel::new());
        }
        if let (Some(journal), Some(id)) = (&ctx.steer_trace, &prompt_id) {
            journal.record(
                id.clone(),
                kimi_ui_core::SteerStage::TurnClaimed { turn_id },
            );
        }
        let (display_prompt, prompt_attachments) =
            crate::turn_events::started_display(&input, &origin);
        let user_entry = {
            let mut meta = serde_json::Map::new();
            meta.insert(
                "source".to_string(),
                serde_json::json!(crate::session::agent_records::origin_source(&origin)),
            );
            if let Some(id) = &prompt_id {
                meta.insert("promptId".to_string(), serde_json::json!(id));
            }
            if origin.is_object() {
                meta.insert("origin".to_string(), origin.clone());
            }
            serde_json::json!({
                "message": { "role": "user", "content": input.clone() },
                "meta": meta,
            })
        };
        ctx.callbacks.turn_event(TurnEvent::Prompt {
            turn_id,
            prompt_id: prompt_id.clone(),
            input,
            origin: origin.clone(),
        });
        let queue_item_id = prompt_id.clone();
        ctx.callbacks.turn_event(TurnEvent::Started {
            turn_id,
            prompt_id,
            origin: origin.clone(),
            prompt: display_prompt,
            prompt_attachments,
        });
        let mut turn_started = serde_json::json!({
            "type": "agent.turn.started",
            "agent_id": crate::callbacks::MAIN_AGENT_ID,
            "kind": "event",
            "turnId": turn_id,
        });
        if let Some(queue_item_id) = &queue_item_id {
            turn_started["queueItemId"] = serde_json::json!(queue_item_id);
        }
        ctx.callbacks.emit_event(turn_started);
        let mut appended = user_entry;
        appended["type"] = serde_json::json!("agent.message.appended");
        appended["agent_id"] = serde_json::json!(crate::callbacks::MAIN_AGENT_ID);
        appended["kind"] = serde_json::json!("event");
        ctx.callbacks.emit_event(appended);
        if let Some(ref guard) = ctx.hook_guard {
            guard
                .notify_turn_started(turn_id, &origin, &prompt.content)
                .await;
        }

        let turn_cancel = cancel.clone();

        let occupancy = ctx
            .session_id
            .as_deref()
            .map(|session_id| crate::session::lock::SESSION_LOCKS.hold_guard(session_id));
        let outcome = match &occupancy {
            Some(Err(error)) => Err(error.to_string()),
            _ => run_session_turn(&ctx, turn_id, prompt, origin.clone(), cancel, history).await,
        };

        let turn_duration_ms = started.elapsed().as_millis() as u64;

        let goal_after_turn =
            goal_turn::settle_goal_after_turn(&*ctx.callbacks, GoalTurnEnd::from(&outcome)).await;

        let mut print_warnings = Vec::new();
        if outcome.is_ok() && !settle_print_background(&ctx, &turn_cancel).await {
            if !turn_cancel.load(Ordering::SeqCst)
                && let Some(policy) = ctx.print_background
            {
                print_warnings.push(settle_warning(
                    "print.settle_ceiling",
                    format!(
                        "print settle ceiling reached ({}s), finishing",
                        policy.ceiling_s
                    ),
                ));
            }
        }

        let mut goal_continuation = None;
        let mut cron_followups = Vec::new();
        if outcome.is_ok() && ctx.print_background.is_some() {
            goal_continuation = goal_after_turn.continuation.clone();
            if goal_continuation.is_none() {
                cron_followups = pending_cron_followups(&ctx, &turn_cancel).await;
            }
        }

        let (steer_receipts, delegated_wake_replays) = {
            let mut core = core.lock().unwrap_or_else(|e| e.into_inner());
            core.active_turn_id = None;
            core.active_cancel = None;
            core.active_wake = None;
            if wake.is_none() {
                core.wake_waiting_for_user = false;
            }
            if let Some(slot) = &ctx.agent_cancel_slot {
                *slot.lock().unwrap_or_else(|e| e.into_inner()) = None;
            }
            if let Some(slot) = &ctx.steer_slot {
                *slot.lock().unwrap_or_else(|e| e.into_inner()) = None;
            }
            if let Ok(TurnOutcome::Ran(result)) = &outcome {
                core.usage.record(ctx.llm.model_name(), &result.usage);

                if let Some(compaction_usage) = &result.compaction_usage {
                    core.usage
                        .record_compaction(ctx.llm.model_name(), compaction_usage);
                }

                ctx.last_turn_aborted.store(
                    result.stop_reason == crate::turn_loop::types::LoopTurnStopReason::Aborted,
                    std::sync::atomic::Ordering::Relaxed,
                );
                core.history
                    .extend(result.messages.iter().skip(1 + history_len).cloned());

                core.last_engine = Some(crate::rpc::types::EngineExecSummary {
                    transport: Some(result.llm_transport.clone()),
                    native_tool_calls: Some(result.native_tool_calls),
                    steps: Some(result.steps),
                    stop_reason: Some(format!("{:?}", result.stop_reason)),
                });
            } else if outcome.is_err() {
                core.last_engine = Some(crate::rpc::types::EngineExecSummary {
                    transport: None,
                    native_tool_calls: None,
                    steps: None,
                    stop_reason: Some("failed".into()),
                });
            }

            if ctx.print_background.is_none()
                && ctx.session_id.is_some()
                && let Some(text) = goal_after_turn.continuation.clone()
            {
                let mut no_receipt = None;
                enqueue_print_followup_turn(
                    &mut core,
                    &mut no_receipt,
                    text,
                    serde_json::json!({ "kind": "system_trigger", "name": "goal_continuation" }),
                );
            }

            print_warnings.extend(maybe_enqueue_print_followup(
                &ctx,
                &mut core,
                &mut entry_outcome,
                goal_continuation,
                cron_followups,
            ));
            let mut delegated_wake_replays = Vec::new();
            if !core.wake_waiting_for_user
                && core.pending.is_empty()
                && core.held.is_empty()
                && core.quiescence_depth == 0
            {
                for payload in std::mem::take(&mut core.deferred_wakes) {
                    if let Some(handler) = core.wake_replay_handlers.get(&payload.variant).cloned()
                    {
                        *core
                            .wake_replay_inflight
                            .entry(payload.variant.clone())
                            .or_default() += 1;
                        delegated_wake_replays.push((payload, handler));
                    } else {
                        let receipt = enqueue_replayed_wake_locked(&mut core, &wakeup, payload);
                        spawn_notification_waiter(receipt);
                    }
                }
            }

            maybe_settle_locked(&mut core);
            (
                std::mem::take(&mut core.steer_waiters),
                delegated_wake_replays,
            )
        };

        if let Ok(TurnOutcome::Ran(result)) = &outcome {
            let reason = end_reason_of(&result.stop_reason);
            let error = turn_end_error_payload(&result.stop_reason, result.steps);
            ctx.callbacks.turn_event(TurnEvent::Ended {
                turn_id,
                reason,
                error: error.as_ref().map(payload_value),
                duration_ms: Some(turn_duration_ms),
                interrupt_reason: interrupt_reason_of(&result.stop_reason),
                trace_id: Some(format!("turn-{turn_id}")),
                stop_reason: forced_stop_reason_of(&result.stop_reason),
            });
            for message in result.messages.iter().skip(1 + history_len) {
                if let Some(mut entry) = crate::session::agent_records::history_entry(message) {
                    entry["type"] = serde_json::json!("agent.message.appended");
                    entry["agent_id"] = serde_json::json!(crate::callbacks::MAIN_AGENT_ID);
                    entry["kind"] = serde_json::json!("event");
                    ctx.callbacks.emit_event(entry);
                }
            }
            if let Some(agent_outcome) = agent_turn_outcome(reason) {
                ctx.callbacks.emit_event(serde_json::json!({
                    "type": "agent.turn.ended",
                    "agent_id": crate::callbacks::MAIN_AGENT_ID,
                    "kind": "event",
                    "turnId": turn_id,
                    "outcome": agent_outcome,
                }));
            }
            if let Some(ref guard) = ctx.hook_guard {
                notify_agent_turn_end(guard, turn_id, AgentTurnEnd::Ran(result)).await;
            }
        } else if let Err(e) = &outcome {
            let payload = turn_failure_payload(e);
            ctx.callbacks.turn_event(TurnEvent::Ended {
                turn_id,
                reason: TurnEndReason::Failed,
                error: Some(payload_value(&payload)),
                duration_ms: Some(turn_duration_ms),
                interrupt_reason: Some(TurnInterruptReason::Error),
                trace_id: Some(format!("turn-{turn_id}")),
                stop_reason: None,
            });
            ctx.callbacks.emit_event(serde_json::json!({
                "type": "agent.turn.ended",
                "agent_id": crate::callbacks::MAIN_AGENT_ID,
                "kind": "event",
                "turnId": turn_id,
                "outcome": "failed",
                "errorMessage": e,
            }));
            if let Some(ref guard) = ctx.hook_guard {
                notify_agent_turn_end(guard, turn_id, AgentTurnEnd::Failed(e.as_str())).await;
            }

            ctx.callbacks.emit_event(error_event(&payload));
        }
        for warning in print_warnings {
            ctx.callbacks.emit_event(warning);
        }
        if let Some(tx) = entry_outcome {
            let _ = tx.send(outcome.clone());
        }
        for (_, receipt) in steer_receipts {
            let _ = receipt.send(outcome.clone());
        }
        for (payload, handler) in delegated_wake_replays {
            let variant = payload.variant.clone();
            let result = handler(payload.clone()).await;
            let mut core = core.lock().unwrap_or_else(|e| e.into_inner());
            let remove_inflight =
                core.wake_replay_inflight
                    .get_mut(&variant)
                    .is_some_and(|count| {
                        *count = count.saturating_sub(1);
                        *count == 0
                    });
            if remove_inflight {
                core.wake_replay_inflight.remove(&variant);
            }
            match result {
                Ok(()) => {
                    core.wake_replay_errors.remove(&variant);
                }
                Err(error) => {
                    core.deferred_wakes.insert(0, payload);
                    core.wake_replay_errors.insert(variant, error);
                }
            }
            maybe_settle_locked(&mut core);
        }
    }
}

async fn settle_print_background(ctx: &SessionContext, cancel: &AtomicBool) -> bool {
    let Some(policy) = ctx.print_background else {
        return true;
    };
    if !policy.mode.waits() {
        return true;
    }
    let Some(runner) = &ctx.task_runner else {
        return true;
    };

    let deadline = {
        let mut state = ctx.print_run.lock().unwrap_or_else(|e| e.into_inner());
        *state.deadline.get_or_insert_with(|| {
            std::time::Instant::now() + std::time::Duration::from_secs(policy.ceiling_s)
        })
    };
    loop {
        if cancel.load(Ordering::SeqCst) {
            return false;
        }
        if runner.running_ids().is_empty() {
            return true;
        }
        let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) else {
            return false;
        };
        tokio::time::sleep(remaining.min(PRINT_SETTLE_POLL)).await;
    }
}

async fn pending_cron_followups(
    ctx: &SessionContext,
    cancel: &AtomicBool,
) -> Vec<(String, serde_json::Value)> {
    if crate::turn_loop::retry::parse_truthy_env("KIMI_DISABLE_CRON") {
        return Vec::new();
    }
    let Some(policy) = ctx.print_background else {
        return Vec::new();
    };

    let deadline = {
        let mut state = ctx.print_run.lock().unwrap_or_else(|e| e.into_inner());
        *state.deadline.get_or_insert_with(|| {
            std::time::Instant::now() + std::time::Duration::from_secs(policy.ceiling_s)
        })
    };
    let Some(tasks) = read_cron_registry(ctx).await else {
        return Vec::new();
    };
    if tasks.is_empty() {
        return Vec::new();
    }
    let scheduler = crate::cron::scheduler::CronScheduler::new(tasks, local_utc_offset_minutes());
    let now = now_ms_epoch();
    let Some(fire_at) = scheduler.next_fire_at(now) else {
        return Vec::new();
    };

    let remaining_ms = deadline
        .checked_duration_since(std::time::Instant::now())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    if fire_at - now > remaining_ms {
        return Vec::new();
    }

    let wake_at = fire_at + CRON_FIRE_GRACE_MS;
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Vec::new();
        }
        let now = now_ms_epoch();
        if now >= wake_at {
            break;
        }
        let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) else {
            return Vec::new();
        };
        let until_wake = std::time::Duration::from_millis(
            (wake_at - now).min(remaining.as_millis() as i64) as u64,
        );
        tokio::time::sleep(
            until_wake
                .min(PRINT_SETTLE_POLL)
                .max(std::time::Duration::from_millis(1)),
        )
        .await;
    }

    let Some(tasks) = read_cron_registry(ctx).await else {
        return Vec::new();
    };
    let mut scheduler =
        crate::cron::scheduler::CronScheduler::new(tasks, local_utc_offset_minutes());
    let fired = scheduler.tick(fire_at - CRON_FIRE_GRACE_MS, now_ms_epoch());
    let mut followups = Vec::with_capacity(fired.len());
    for fired_entry in fired {
        let entry = &fired_entry.entry;

        if !entry.recurring || fired_entry.stale {
            let _ = ctx
                .callbacks
                .state_write(crate::rpc::types::StateWriteRequest {
                    domain: "cron".into(),
                    key: "cron".into(),
                    value: serde_json::json!({ "action": "delete", "id": entry.id }),
                    undoable: false,
                    turn_id: String::new(),
                    tool_call_id: String::new(),
                })
                .await;
        }
        followups.push((
            render_cron_fire(entry, fired_entry.coalesced_count, fired_entry.stale),
            cron_fire_origin(entry, fired_entry.coalesced_count, fired_entry.stale),
        ));
    }
    followups
}

async fn read_cron_registry(
    ctx: &SessionContext,
) -> Option<Vec<crate::cron::scheduler::CronEntry>> {
    let response = ctx
        .callbacks
        .state_read(crate::rpc::types::StateReadRequest {
            domain: "cron".into(),
            key: "cron".into(),
            turn_id: String::new(),
            tool_call_id: String::new(),
        })
        .await
        .ok()?;
    Some(
        response
            .value
            .as_array()?
            .iter()
            .filter_map(|task| {
                Some(crate::cron::scheduler::CronEntry {
                    id: task.get("id")?.as_str()?.to_string(),
                    cron: task.get("cron")?.as_str()?.to_string(),
                    prompt: task.get("prompt")?.as_str()?.to_string(),
                    recurring: task
                        .get("recurring")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(true),
                    session_id: None,
                    created_at: task.get("createdAt").and_then(serde_json::Value::as_i64),
                })
            })
            .collect(),
    )
}

pub fn render_cron_fire(
    entry: &crate::cron::scheduler::CronEntry,
    coalesced_count: u32,
    stale: bool,
) -> String {
    format!(
        "<cron-fire jobId=\"{}\" cron=\"{}\" recurring=\"{}\" coalescedCount=\"{coalesced_count}\" stale=\"{stale}\">\n<prompt>\n{}\n</prompt>\n</cron-fire>",
        entry.id, entry.cron, entry.recurring, entry.prompt
    )
}

pub(crate) fn cron_fire_origin(
    entry: &crate::cron::scheduler::CronEntry,
    coalesced_count: u32,
    stale: bool,
) -> serde_json::Value {
    serde_json::json!({
        "kind": "cron_job",
        "jobId": entry.id,
        "cron": entry.cron,
        "recurring": entry.recurring,
        "coalescedCount": coalesced_count,
        "stale": stale,
    })
}

fn settle_warning(code: &str, message: String) -> serde_json::Value {
    serde_json::json!({ "type": "warning", "message": message, "code": code })
}

pub(crate) fn local_utc_offset_minutes() -> i32 {
    chrono::Local::now().offset().local_minus_utc() / 60
}

pub(crate) fn now_ms_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn maybe_enqueue_print_followup(
    ctx: &SessionContext,
    core: &mut Core,
    entry_outcome: &mut Option<oneshot::Sender<Result<TurnOutcome, String>>>,
    goal_continuation: Option<String>,
    cron_followups: Vec<(String, serde_json::Value)>,
) -> Vec<serde_json::Value> {
    let Some(policy) = ctx.print_background else {
        return Vec::new();
    };

    let tasks_pending = policy.mode == PrintBackgroundMode::Steer
        && ctx
            .task_runner
            .as_ref()
            .is_some_and(|runner| runner.pending_notification_count(ctx.session_id.as_deref()) > 0);
    if goal_continuation.is_none() && cron_followups.is_empty() && !tasks_pending {
        return Vec::new();
    }

    let mut state = ctx.print_run.lock().unwrap_or_else(|e| e.into_inner());
    let deadline = state.deadline.get_or_insert_with(|| {
        std::time::Instant::now() + std::time::Duration::from_secs(policy.ceiling_s)
    });
    let mut warnings = Vec::new();
    if std::time::Instant::now() >= *deadline {
        warnings.push(settle_warning(
            "print.settle_ceiling",
            format!(
                "print settle ceiling reached ({}s), finishing",
                policy.ceiling_s
            ),
        ));
        return warnings;
    }
    if state.steer_turns >= policy.max_turns as usize {
        warnings.push(settle_warning(
            "print.settle_max_turns",
            format!(
                "print steer max turns reached ({}), finishing",
                policy.max_turns
            ),
        ));
        return warnings;
    }

    if let Some(text) = goal_continuation {
        state.steer_turns += 1;
        enqueue_print_followup_turn(
            core,
            entry_outcome,
            text,
            serde_json::json!({ "kind": "system_trigger", "name": "goal_continuation" }),
        );
        return warnings;
    }

    let mut enqueued = false;
    for (text, origin) in cron_followups {
        if state.steer_turns >= policy.max_turns as usize {
            break;
        }
        state.steer_turns += 1;
        enqueue_print_followup_turn(core, entry_outcome, text, origin);
        enqueued = true;
    }
    if enqueued {
        return warnings;
    }

    if policy.mode != PrintBackgroundMode::Steer {
        return warnings;
    }
    let Some(runner) = &ctx.task_runner else {
        return warnings;
    };

    let session_id = ctx.session_id.as_deref();
    if runner.pending_notification_count(session_id) == 0 {
        return warnings;
    }
    let notifications = runner.take_pending_notifications(session_id);
    let Some(first) = notifications.first() else {
        return warnings;
    };
    state.steer_turns += 1;
    let text = render_task_notifications(&notifications);
    enqueue_print_followup_turn(
        core,
        entry_outcome,
        text,
        serde_json::json!({
            "kind": "task",
            "taskId": first.task_id,
            "status": first.status.as_str(),
            "notificationId": format!("{}-{}", first.task_id, first.ended_at),
        }),
    );
    warnings
}

fn enqueue_print_followup_turn(
    core: &mut Core,
    entry_outcome: &mut Option<oneshot::Sender<Result<TurnOutcome, String>>>,
    text: String,
    origin: serde_json::Value,
) {
    let turn_id = core.next_turn_id;
    core.next_turn_id += 1;
    core.pending.push(PendingTurn {
        turn_id,
        prompt_id: None,
        prompt: LLMMessage {
            role: kimi_core::types::Role::User,
            content: text.clone(),
            ..LLMMessage::default()
        },

        input: serde_json::json!([{ "type": "text", "text": text }]),
        origin,
        cancel: Arc::new(AtomicBool::new(false)),
        outcome: entry_outcome.take(),
        wake: None,
    });
}

pub(crate) fn render_task_notifications(
    notifications: &[crate::storage::TaskNotification],
) -> String {
    notifications
        .iter()
        .map(|notification| {
            if notification.task_id == crate::features::tower::wake::TOWER_WAKE_TASK_ID {
                return notification.description.clone();
            }

            notification.to_xml()
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn end_reason_of(stop: &crate::turn_loop::types::LoopTurnStopReason) -> TurnEndReason {
    use crate::turn_loop::types::LoopTurnStopReason as Stop;
    match stop {
        Stop::EndTurn | Stop::MaxTokens | Stop::RepeatBreaker => TurnEndReason::Completed,
        Stop::Paused | Stop::BudgetLimited => TurnEndReason::Blocked,
        Stop::Aborted => TurnEndReason::Cancelled,
        Stop::Filtered | Stop::MaxSteps | Stop::Unknown => TurnEndReason::Failed,
    }
}

fn interrupt_reason_of(
    stop: &crate::turn_loop::types::LoopTurnStopReason,
) -> Option<TurnInterruptReason> {
    use crate::turn_loop::types::LoopTurnStopReason as Stop;
    match stop {
        Stop::EndTurn
        | Stop::MaxTokens
        | Stop::RepeatBreaker
        | Stop::Paused
        | Stop::BudgetLimited => None,
        Stop::Aborted => Some(TurnInterruptReason::Aborted),
        Stop::MaxSteps => Some(TurnInterruptReason::MaxSteps),
        Stop::Filtered => Some(TurnInterruptReason::Filtered),
        Stop::Unknown => Some(TurnInterruptReason::Error),
    }
}

fn forced_stop_reason_of(stop: &crate::turn_loop::types::LoopTurnStopReason) -> Option<String> {
    use crate::turn_loop::types::LoopTurnStopReason as Stop;
    match stop {
        Stop::RepeatBreaker => Some("repeat_breaker".to_string()),
        _ => None,
    }
}

fn turn_end_error_payload(
    stop: &crate::turn_loop::types::LoopTurnStopReason,
    steps: u32,
) -> Option<ErrorPayload> {
    use crate::turn_loop::types::LoopTurnStopReason as Stop;
    match stop {
        Stop::MaxSteps => Some(make_error_payload(
            "loop.max_steps_exceeded",
            format!(
                "Turn exceeded maxSteps={steps}. If max_steps_per_turn is too small, raise it in config.toml (loop_control.max_steps_per_turn), or run \"/update-config\" to update it, then \"/reload\"."
            ),
            PayloadOptions {
                name: Some("LoopError".to_string()),
                details: Some(serde_json::Map::from_iter([(
                    "maxSteps".to_string(),
                    serde_json::json!(steps),
                )])),
            },
        )),

        Stop::Filtered => Some(make_error_payload(
            "provider.filtered",
            "Provider safety policy blocked the response.".to_string(),
            PayloadOptions {
                name: Some("ProviderFilteredError".to_string()),
                details: Some(serde_json::Map::from_iter([(
                    "finishReason".to_string(),
                    serde_json::json!("filtered"),
                )])),
            },
        )),
        _ => None,
    }
}

fn error_payload(code: &str, message: String) -> ErrorPayload {
    make_error_payload(code, message, PayloadOptions::default())
}

fn payload_value(payload: &ErrorPayload) -> serde_json::Value {
    serde_json::to_value(payload).expect("ErrorPayload is serializable")
}

fn error_event(payload: &ErrorPayload) -> serde_json::Value {
    let mut value = payload_value(payload);
    value["type"] = serde_json::json!("error");
    value
}

fn turn_failure_code(message: &str) -> &'static str {
    if let Some(rest) = message.split("llm http status ").nth(1) {
        let status: u16 = rest
            .split_whitespace()
            .next()
            .and_then(|code| code.parse().ok())
            .unwrap_or(0);
        return match status {
            401 | 403 => "provider.auth_error",
            404 => "provider.not_found",
            429 => "provider.rate_limit",
            500..=599 => "provider.overloaded",
            _ => "provider.api_error",
        };
    }
    if let Some(rest) = message.split("llm transport error ").nth(1) {
        let kind = rest
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_end_matches(':');
        return match kind {
            "connect" | "timeout" | "transport" => "provider.connection_error",

            _ => "provider.api_error",
        };
    }
    if message.contains("connection") || message.contains("timed out") {
        "provider.connection_error"
    } else {
        "internal"
    }
}

fn turn_failure_payload(message: &str) -> ErrorPayload {
    error_payload(turn_failure_code(message), message.to_string())
}

pub(crate) enum AgentTurnEnd<'a> {
    Ran(&'a TurnResult),
    Failed(&'a str),

    Cancelled,
}

pub(crate) async fn notify_agent_turn_end(
    guard: &crate::tools::external_hooks::HookGuard,
    turn_id: u64,
    end: AgentTurnEnd<'_>,
) {
    match end {
        AgentTurnEnd::Ran(result) => match end_reason_of(&result.stop_reason) {
            TurnEndReason::Cancelled => guard.notify_interrupt(turn_id).await,
            TurnEndReason::Failed => {
                let error = turn_end_error_payload(&result.stop_reason, result.steps);
                let (error_type, error_message) = error
                    .as_ref()
                    .map(|payload| (payload.code.to_string(), payload.message.clone()))
                    .unwrap_or_else(|| {
                        ("internal".to_string(), format!("{:?}", result.stop_reason))
                    });
                guard.notify_stop_failure(&error_type, &error_message).await;
            }
            TurnEndReason::Completed | TurnEndReason::Blocked => {}
        },
        AgentTurnEnd::Failed(message) => {
            let payload = turn_failure_payload(message);
            guard
                .notify_stop_failure(payload.code.as_str(), &payload.message)
                .await;
        }
        AgentTurnEnd::Cancelled => guard.notify_interrupt(turn_id).await,
    }
}

pub(crate) fn origin_is_undo_anchor(origin: &serde_json::Value) -> bool {
    if origin.get("inTurn").and_then(|v| v.as_bool()) == Some(true) {
        return false;
    }
    match origin.get("kind").and_then(|v| v.as_str()) {
        None | Some("user") => true,
        Some("skill_activation") | Some("plugin_command") => {
            origin.get("trigger").and_then(|v| v.as_str()) == Some("user-slash")
        }
        Some(_) => false,
    }
}

async fn run_session_turn(
    ctx: &Arc<SessionContext>,
    turn_id: u64,
    mut prompt: LLMMessage,
    origin: serde_json::Value,
    cancel: Arc<AtomicBool>,
    history: Vec<LLMMessage>,
) -> Result<TurnOutcome, String> {
    if let Some(hook) = &ctx.on_before_turn {
        hook();
    }

    if origin_is_undo_anchor(&origin)
        && let Err(error) = ctx.callbacks.checkpoint_state().await
    {
        tracing::warn!(%error, "state checkpoint failed; undo cannot restore this turn's state");
    }
    let tool_defs = (ctx.tool_defs)().await;

    let previous_turn_aborted = ctx
        .last_turn_aborted
        .swap(false, std::sync::atomic::Ordering::Relaxed);
    let goal = match ctx.goal.as_ref() {
        Some(provider) => provider().await,
        None => None,
    };

    prompt.origin = Some(origin);
    let mut messages = history;
    messages.push(prompt);
    let input = RunTurnInput {
        llm_request_byte_budget: ctx.llm_request_byte_budget,
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted,
        max_attempts: ctx.max_attempts,
        turn_id: format!("turn-{turn_id}"),
        llm: ctx.llm.as_ref(),
        messages,
        tools: &[],
        tool_defs,
        max_steps: ctx.max_steps,
        max_context_tokens: ctx.max_context_tokens,
        compaction_max_attempts: ctx.compaction_max_attempts,
        permission_mode: ctx.permission_mode,
        goal,
        cancellation: Some(cancel),
        hook_guard: ctx.hook_guard.clone(),
        media: Some(&ctx.media),
        media_dropped: Some(ctx.media_dropped.clone()),
        media_recovery: Some(ctx.media_recovery.clone()),
        toolset: ctx.toolset.clone(),
    };
    let result = match &ctx.telemetry {
        Some(telemetry) => crate::turn_loop::run_turn::run_turn_with_lifecycle_telemetry(
            input,
            telemetry,
            &ctx.callbacks,
        )
        .await
        .map_err(|e| e.to_string())?,
        None => run_turn_continued(input, &ctx.callbacks)
            .await
            .map_err(|e| e.to_string())?,
    };
    Ok(TurnOutcome::Ran(result))
}

pub(crate) struct SteerQueueCallbacks {
    inner: Arc<dyn HostCallbacks>,
    steer_queue: Arc<Mutex<Vec<LLMMessage>>>,

    steer_trace: Option<kimi_ui_core::SteerJournal>,

    steer_slot: Option<Arc<Mutex<Option<crate::subagent::types::ParentCancel>>>>,
}

fn steer_input(message: &LLMMessage) -> serde_json::Value {
    if message.blocks.is_empty() {
        return serde_json::json!([{ "type": "text", "text": message.content }]);
    }
    serde_json::to_value(&message.blocks).unwrap_or_else(|_| serde_json::json!([]))
}

impl SteerQueueCallbacks {
    pub(crate) fn new(
        inner: Arc<dyn HostCallbacks>,
        steer_queue: Arc<Mutex<Vec<LLMMessage>>>,
        steer_trace: Option<kimi_ui_core::SteerJournal>,
    ) -> Self {
        Self {
            inner,
            steer_queue,
            steer_trace,
            steer_slot: None,
        }
    }

    pub(crate) fn with_steer_slot(
        mut self,
        steer_slot: Option<Arc<Mutex<Option<crate::subagent::types::ParentCancel>>>>,
    ) -> Self {
        self.steer_slot = steer_slot;
        self
    }
}

impl HostCallbacks for SteerQueueCallbacks {
    fn hook_guard(&self) -> Option<Arc<crate::tools::external_hooks::HookGuard>> {
        self.inner.hook_guard()
    }

    fn llm_chat(
        &self,
        request: LlmChatRequest,
    ) -> futures_util::future::BoxFuture<'static, Result<LlmChatResponse, String>> {
        self.inner.llm_chat(request)
    }

    fn execute_tool(
        &self,
        request: ToolExecuteRequest,
    ) -> futures_util::future::BoxFuture<'static, Result<ToolExecuteResponse, String>> {
        self.inner.execute_tool(request)
    }

    fn check_permission(
        &self,
        request: PermissionCheckRequest,
    ) -> futures_util::future::BoxFuture<'static, Result<PermissionDecision, String>> {
        self.inner.check_permission(request)
    }

    fn ask_question(
        &self,
        request: AskQuestionRequest,
    ) -> futures_util::future::BoxFuture<'static, Result<AskQuestionResponse, String>> {
        self.inner.ask_question(request)
    }

    fn state_read(
        &self,
        request: StateReadRequest,
    ) -> futures_util::future::BoxFuture<'static, Result<StateReadResponse, String>> {
        self.inner.state_read(request)
    }

    fn state_write(
        &self,
        request: StateWriteRequest,
    ) -> futures_util::future::BoxFuture<'static, Result<StateWriteResponse, String>> {
        self.inner.state_write(request)
    }

    fn checkpoint_state(&self) -> futures_util::future::BoxFuture<'static, Result<(), String>> {
        self.inner.checkpoint_state()
    }

    fn drain_steers(
        &self,
    ) -> futures_util::future::BoxFuture<'static, Result<Vec<LLMMessage>, String>> {
        let queue = self.steer_queue.clone();
        let slot = self.steer_slot.clone();
        let inner = self.inner.clone();
        let steer_trace = self.steer_trace.clone();
        Box::pin(async move {
            let drained = std::mem::take(&mut *queue.lock().unwrap_or_else(|e| e.into_inner()));
            if let Some(journal) = &steer_trace {
                for message in &drained {
                    if let Some(prompt_id) = &message.prompt_id {
                        journal.record(prompt_id.clone(), kimi_ui_core::SteerStage::Drained);
                    }
                }
            }

            for message in drained.iter().filter(|message| message.prompt_id.is_some()) {
                let prompt_id = message.prompt_id.clone().unwrap_or_default();
                inner.turn_event(TurnEvent::Steer {
                    turn_id: None,
                    input: steer_input(message),
                    origin: message
                        .origin
                        .clone()
                        .unwrap_or_else(|| serde_json::json!({ "kind": "user" })),
                    message_id: Some(prompt_id.clone()),
                    prompt_ids: vec![prompt_id],
                });
            }

            if !drained.is_empty()
                && let Some(slot) = &slot
            {
                *slot.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(crate::subagent::types::ParentCancel::new());
            }
            if drained.is_empty() {
                return Ok(Vec::new());
            }
            let prompt_id = if drained.len() == 1 {
                drained[0].prompt_id.clone()
            } else {
                None
            };
            let mut merged = origin::merge_steer_messages(&drained);
            merged.prompt_id = prompt_id;
            if let Some(origin) = merged
                .origin
                .as_mut()
                .and_then(serde_json::Value::as_object_mut)
            {
                origin.insert("inTurn".into(), serde_json::Value::Bool(true));
            }
            Ok(vec![merged])
        })
    }

    fn list_tools(
        &self,
    ) -> futures_util::future::BoxFuture<'static, Result<ListToolsResponse, String>> {
        self.inner.list_tools()
    }

    fn goal(
        &self,
    ) -> futures_util::future::BoxFuture<'static, Result<Option<GoalContext>, String>> {
        self.inner.goal()
    }

    fn parse_bash(
        &self,
        request: crate::rpc::types::ParseBashRequest,
    ) -> futures_util::future::BoxFuture<'static, Result<Option<Vec<String>>, String>> {
        self.inner.parse_bash(request)
    }

    fn auth_token(
        &self,
        provider: String,
        force: bool,
    ) -> futures_util::future::BoxFuture<'static, Result<String, String>> {
        self.inner.auth_token(provider, force)
    }

    fn set_turn_goal(&self, turn_id: &str, goal_id: Option<&str>) {
        self.inner.set_turn_goal(turn_id, goal_id);
    }

    fn emit_event(&self, event: serde_json::Value) {
        self.inner.emit_event(event);
    }

    fn turn_event(&self, event: TurnEvent) {
        self.inner.turn_event(event);
    }

    fn telemetry(&self, event: serde_json::Value) {
        self.inner.telemetry(event);
    }

    fn cancel_llm_chat(&self, request_id: &str) {
        self.inner.cancel_llm_chat(request_id);
    }
}

#[cfg(test)]
mod tests;
