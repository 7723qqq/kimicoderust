//! EngineSession — the turn lifecycle owner (M1a/M1b/M1c).
//!
//! Owns what v2's `loopService.ts` owns today: turn admission (four modes),
//! the pending-turn FIFO, serial turn execution, turn-id assignment, and
//! cancellation (active via the run_turn cancel flag, queued by dropping the
//! entry). Cross-turn conversation history is owned here too: each turn
//! starts from the accumulated history plus the enqueued prompt, and the
//! turn's final messages (assistant/tool turns included) fold back into the
//! history for the next turn.
//!
//! The turn clock is hydrated once from the host's `turn` state domain and
//! then advanced locally; every id assignment is mirrored to the host by a
//! durable [`TurnEvent::Prompt`], so the host's own `turnKey` fold stays the
//! single writer of persisted turn state.
//!
//! M1c adds the quiescence/backpressure half of `loopService.ts`
//! (`tryAcquireQuiescence` / `settled`): while a quiescence guard is held,
//! enqueued turns are held back instead of admitted; releasing the guard
//! replays them in FIFO order; [`EngineSession::settled`] resolves once no
//! turn is active, pending, or held.
//!
//! Deliberately out of scope (see the M1 section of `ROADMAP.md`):
//! telemetry is served by the `host/telemetry` callback (M1c, emitted from
//! `run_turn`), `host/list_tools` is M1d.

pub mod patch;
pub mod sqlite_store;
pub mod title;
pub mod token_counting;
pub mod usage;

pub(crate) mod goal_turn;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{Notify, oneshot};

use crate::callbacks::HostCallbacks;
use crate::rpc::types::{
    AskQuestionRequest, AskQuestionResponse, ListToolsResponse, LlmChatRequest, LlmChatResponse,
    PermissionCheckRequest, PermissionDecision, StateReadRequest, StateReadResponse,
    StateWriteRequest, StateWriteResponse, ToolExecuteRequest, ToolExecuteResponse,
};
use crate::turn_events::{TurnCancelReason, TurnCancelTarget, TurnEndReason, TurnEvent};
use crate::turn_loop::run_turn::run_turn_continued;
use crate::turn_loop::types::{GoalContext, LLM, LLMMessage, RunTurnInput, ToolInfo, TurnResult};

use goal_turn::GoalTurnEnd;

/// How an enqueued prompt joins the turn pipeline (v2 `StepRequest.admission`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Always start a new queued turn.
    NewTurn,
    /// Join the active turn as steering, else start a new turn.
    ActiveOrNewTurn,
    /// Queue behind the active turn, else start a new turn.
    ActiveOrNextTurn,
    /// Join the active turn as steering; error when no turn is active.
    ActiveTurnOnly,
}

/// The host's persisted turn clock, read once at session construction so a
/// resumed session continues its id sequence instead of restarting at zero.
///
/// Read-only by design. The host owns the clock: it advances from the durable
/// [`TurnEvent::Prompt`] the engine emits per turn, and its `turn` state also
/// carries host-only fields (undo anchors, last-end summary) that a whole-value
/// write from the engine would clobber.
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

/// A prompt to enqueue, plus the payload the host wants mirrored back on the
/// durable `turn.prompt` event.
#[derive(Debug, Clone)]
pub struct TurnRequest {
    pub prompt: LLMMessage,
    pub admission: Admission,
    /// The prompt as the host wrote it — a `ContentPart[]` JSON array the
    /// engine echoes back verbatim on the durable `turn.prompt` event, so the
    /// host persists exactly what it sent rather than the engine's re-render.
    pub input: serde_json::Value,
    /// v2 `PromptOrigin` JSON, echoed back verbatim. The engine does not model
    /// origin variants; the host needs them for the undo anchor and transcript.
    pub origin: serde_json::Value,
}

impl TurnRequest {
    /// A plain text prompt from the user — the minimum the host can fold:
    /// v2's undo-anchor check reads `origin.kind`, so a missing origin would
    /// throw inside the fold rather than degrade.
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

/// The outcome of an enqueued turn.
#[derive(Debug, Clone)]
pub enum TurnOutcome {
    /// The turn ran to a stop (mid-turn cancellation included —
    /// `TurnResult.stop_reason` says which).
    Ran(TurnResult),
    /// Cancelled while still queued; the engine never ran it.
    CancelledBeforeStart,
}

/// Handle for one enqueued turn. Cancellation goes through
/// [`EngineSession::cancel_turn`] with the receipt's `turn_id`.
pub struct TurnReceipt {
    /// For steering admissions this is the active turn's id — the steer
    /// joins that turn and its outcome resolves with the turn's.
    pub turn_id: u64,
    outcome: oneshot::Receiver<Result<TurnOutcome, String>>,
}

impl TurnReceipt {
    /// Wait for the turn to finish (or be cancelled while queued). Takes
    /// `&mut self` so the receipt (and its `turn_id`) remains accessible
    /// after the await — callers that only need the outcome can `let _ =
    /// receipt.outcome().await?` to drop the receipt after.
    pub async fn outcome(&mut self) -> Result<TurnOutcome, String> {
        (&mut self.outcome)
            .await
            .map_err(|_| "session dropped".to_string())?
    }

    /// Split the receipt into its turn id and the outcome receiver — the
    /// shape the napi session surface stores to serve `turn_outcome`
    /// promises.
    pub fn into_parts(self) -> (u64, oneshot::Receiver<Result<TurnOutcome, String>>) {
        (self.turn_id, self.outcome)
    }
}

/// Live session shape (v2 `AgentLoopStatus`).
#[derive(Debug, Clone)]
pub struct SessionStatus {
    pub active_turn_id: Option<u64>,
    pub pending_turn_ids: Vec<u64>,
    /// P56 (G-5): execution-path summary of the most recent completed turn
    /// — the cross-process half of `/status`'s engine line. `None` until
    /// the session has run a turn.
    pub engine: Option<crate::rpc::types::EngineExecSummary>,
    /// v2 `UsageStatus`: per-model buckets, their total, and the current turn.
    pub usage: crate::session::usage::UsageStatus,
}

/// Fresh-per-turn tool-table provider (the turn-start snapshot source; the
/// per-step refresh goes through `host/list_tools`).
pub type ToolDefsProvider =
    Arc<dyn Fn() -> futures_util::future::BoxFuture<'static, Vec<ToolInfo>> + Send + Sync>;

/// Fresh-per-turn async goal provider (budget checks + steering). Async like
/// `tool_defs`: the napi session reads it through a host callback.
pub type GoalProvider =
    Arc<dyn Fn() -> futures_util::future::BoxFuture<'static, Option<GoalContext>> + Send + Sync>;

/// Session-level configuration, fixed for the session's lifetime.
pub struct SessionConfig {
    pub llm: Arc<dyn LLM>,
    pub callbacks: Arc<dyn HostCallbacks>,
    /// Step cap for every turn (v2 `maxStepsPerTurn`).
    pub max_steps: u32,
    /// Host `loopControl.maxAttemptsPerStep` (v2); `None` keeps the engine
    /// default.
    pub max_attempts: Option<u32>,
    /// Context window the host resolved for the session's model (v2
    /// `ModelCapability.max_context_tokens`); `None` keeps the engine default.
    pub max_context_tokens: Option<u32>,
    /// Total requests one compaction round may issue (v2 #3750
    /// `loopControl.compactionMaxAttempts`); `None` keeps the engine default.
    pub compaction_max_attempts: Option<u32>,
    /// The permission mode the session's policy snapshot resolved to, for the
    /// permission-mode reminders. `None` leaves them off.
    pub permission_mode: Option<crate::permission::PermissionMode>,
    /// Fresh tool definitions per turn (MCP tools can change mid-session;
    /// M1d replaces this provider with `host/list_tools`).
    pub tool_defs: ToolDefsProvider,
    /// Fresh goal snapshot per turn (budget checks + steering). Async like
    /// `tool_defs`: the napi session reads it through a host callback.
    pub goal: Option<GoalProvider>,
    /// Ran before each turn's first step (REPL: the undo checkpoint).
    pub on_before_turn: Option<Arc<dyn Fn() + Send + Sync>>,
    /// P55: session-wide slot the pump refreshes per turn with that turn's
    /// [`ParentCancel`] signal. The native toolset shares the same Arc, so a
    /// foreground `Agent` spawn sees the live signal even though the toolset
    /// itself is built once per session. `None` = no native agent context.
    pub agent_cancel_slot:
        Option<Arc<std::sync::Mutex<Option<crate::subagent::types::ParentCancel>>>>,
    /// Session-wide slot the pump refreshes per turn with that turn's *steer*
    /// signal (#3697). Fired when a steering message joins the running turn,
    /// refreshed once that turn has drained it: a blocking `WaitFor` ends early
    /// on the fire, without cancelling the turn or stopping its tasks. `None` =
    /// no steer interruption.
    pub steer_slot: Option<Arc<std::sync::Mutex<Option<crate::subagent::types::ParentCancel>>>>,
    /// Turn-lifecycle hook dispatch (`UserPromptSubmit` / `PreCompact` /
    /// `Stop`); `None` skips those dispatches.
    pub hook_guard: Option<Arc<crate::tools::external_hooks::HookGuard>>,
    /// Print-mode (`kimi -p`) background policy the host resolved from
    /// `[background]`. `None` keeps the engine default: a turn receipt
    /// resolves as soon as the turn ends, whatever the background tasks do.
    pub print_background: Option<PrintBackgroundPolicy>,
    /// The host's session id (`params.session_id`), the key the print settle
    /// scopes task notifications by: it drains only this session's
    /// completions, never another session's. `None` = the host attributed no
    /// session, so no session-scoped delivery happens (see
    /// [`crate::storage::TaskRunner::take_pending_notifications`]).
    pub session_id: Option<String>,
    /// The process-wide task runner the print settle waits on. `None` outside
    /// a wired pipeline, where the wait is a no-op.
    pub task_runner: Option<Arc<crate::storage::TaskRunner>>,
    /// The pipeline's native toolset, when native tools are on. Carries the
    /// progressive-tool-disclosure announcement state; `None` outside a
    /// wired pipeline.
    pub toolset: Option<Arc<crate::tools::NativeToolset>>,
    /// Host-injected turn-telemetry context (M1c, v2 #3963): the host knows
    /// the model configuration, the engine contributes the outcome fields and
    /// emits `turn_started` / `turn_ended` / `turn_interrupted` through the
    /// host/telemetry seam. `None` leaves the turn telemetry to the host.
    pub telemetry: Option<crate::turn_loop::types::TelemetryContext>,
}

/// What a print-mode (`kimi -p`) session does once its main turn ends while
/// background tasks are still running (`[background].print_background_mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrintBackgroundMode {
    /// Exit as soon as the turn ends, leaving pending tasks behind.
    Exit,
    /// Keep the session open until the runner's tasks reach a terminal state.
    Drain,
    /// Like [`Self::Drain`]; the host additionally feeds the completions back
    /// as a further turn.
    Steer,
}

impl PrintBackgroundMode {
    /// Wire string (`exit` / `drain` / `steer`). An unknown value resolves to
    /// [`Self::Exit`] so a typo cannot silently stall a run behind the
    /// ceiling.
    #[must_use]
    pub fn from_wire(value: &str) -> Self {
        match value {
            "drain" => Self::Drain,
            "steer" => Self::Steer,
            _ => Self::Exit,
        }
    }

    /// Whether the session holds its turn receipt until the runner drains.
    #[must_use]
    pub fn waits(&self) -> bool {
        matches!(self, Self::Drain | Self::Steer)
    }
}

/// The `[background]` print policy the host passed with the session.
#[derive(Debug, Clone, Copy)]
pub struct PrintBackgroundPolicy {
    pub mode: PrintBackgroundMode,
    /// Wall-clock ceiling for the run's whole settle phase (drain + steer
    /// turns), in seconds.
    pub ceiling_s: u64,
    /// Cap on the steer turns the engine may add for background completions
    /// (`[background].print_max_turns`).
    pub max_turns: u32,
}

/// Ceiling used when the host passes none: the documented
/// `print_wait_ceiling_s` default (~24.8 days), i.e. effectively unbounded.
pub const PRINT_WAIT_CEILING_S_DEFAULT: u64 = 2_147_483;

/// Steer-turn budget used when the host passes none: the documented
/// `print_max_turns` default.
pub const PRINT_MAX_TURNS_DEFAULT: u32 = 100_000;

impl PrintBackgroundPolicy {
    /// From the raw knobs the host resolved (`print_background_mode` /
    /// `print_wait_ceiling_s` / `print_max_turns`). `None` (no mode) keeps the
    /// engine's exit-on-turn-end default, so non-print entries are unaffected;
    /// a non-positive bound keeps the documented default, since `0` cannot
    /// mean "never wait".
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

/// Cross-turn budget of one print run's settle phase. The ceiling deadline and
/// the steer-turn count span every turn of the run — `[background]` bounds the
/// run, not each turn's share of it.
#[derive(Default)]
struct PrintRunState {
    /// Wall-clock deadline, anchored at the first settle of the run.
    deadline: Option<std::time::Instant>,
    /// Steer turns enqueued so far.
    steer_turns: usize,
}

/// How often the settle loop re-checks the runner. The interval only bounds
/// how late a drain is noticed, never how long a wait may run.
const PRINT_SETTLE_POLL: std::time::Duration = std::time::Duration::from_millis(250);

/// How long after a cron's fire time the settle still accepts it (v2
/// `CRON_FIRE_GRACE_MS`): the wake is never exactly on the minute.
const CRON_FIRE_GRACE_MS: i64 = 2_000;

struct PendingTurn {
    turn_id: u64,
    prompt: LLMMessage,
    input: serde_json::Value,
    origin: serde_json::Value,
    cancel: Arc<AtomicBool>,
    /// `Some(sender)` until the pump claims it to resolve the receipt; the
    /// cancel path takes it out to deliver `CancelledBeforeStart`. `None`
    /// after one of those has run.
    outcome: Option<oneshot::Sender<Result<TurnOutcome, String>>>,
}

/// State shared between the public API and the pump. Guarded by one mutex;
/// the pump never holds it across an await.
struct Core {
    next_turn_id: u64,
    pending: Vec<PendingTurn>,
    active_turn_id: Option<u64>,
    active_cancel: Option<Arc<AtomicBool>>,
    /// Steer receipts waiting on the active turn's outcome.
    steer_waiters: Vec<(u64, oneshot::Sender<Result<TurnOutcome, String>>)>,
    /// Cross-turn conversation history (system message excluded — run_turn
    /// rebuilds it per turn from the LLM's system prompt).
    history: Vec<LLMMessage>,
    /// M1c: while > 0 (a [`QuiescenceGuard`] is alive) enqueued turns are
    /// parked in `held` instead of admitted.
    quiescence_depth: usize,
    /// Turns enqueued during quiescence, replayed in FIFO order on release.
    held: Vec<HeldTurn>,
    /// Waiters resolved by [`EngineSession::settled`] once nothing is
    /// active, pending, or held.
    settle_waiters: Vec<oneshot::Sender<()>>,
    /// Set when the pump task has exited and dropped its `Arc<Mutex<Core>>` —
    /// the fact `EngineSession::released` waits on. `settled` says "idle";
    /// this says "the pump is gone", which is what a host about to delete the
    /// session's directory actually needs (ROADMAP §15 D1).
    released: bool,
    /// Waiters resolved by the pump's [`ReleaseGuard`] drop.
    release_waiters: Vec<oneshot::Sender<()>>,
    /// P56 (G-5): execution-path summary of the last completed turn.
    last_engine: Option<crate::rpc::types::EngineExecSummary>,
    /// v2 `UsageAgentModel` equivalent: per-model buckets accumulated across
    /// this session's turns. Seeded from the host's persisted snapshot when a
    /// handle is rebuilt or a session resumed, so the totals survive restarts.
    usage: crate::session::usage::UsageStatus,
}

/// A turn parked by quiescence: the id was allocated at enqueue time (the
/// receipt is already in the caller's hands), the turn itself starts only
/// when the guard is released.
struct HeldTurn {
    turn_id: u64,
    request: TurnRequest,
    cancel: Arc<AtomicBool>,
    outcome: Option<oneshot::Sender<Result<TurnOutcome, String>>>,
}

/// Resolve every [`Core::settle_waiters`] when the session is fully idle.
fn maybe_settle_locked(core: &mut Core) {
    if core.active_turn_id.is_none() && core.pending.is_empty() && core.held.is_empty() {
        for tx in std::mem::take(&mut core.settle_waiters) {
            let _ = tx.send(());
        }
    }
}

/// Publishes "the pump is gone" when the pump task ends, by any path.
///
/// Held for the whole pump body, so its `Drop` runs on the normal `return`,
/// on an early `return`, and on a panic unwind (the crate sets no
/// `panic = "abort"`). That is the fact `dispose()` promises: the conversation
/// has been dropped and the session's background tasks are stopped. Marking it
/// at one exit path by hand would silently mislead every caller if a second
/// exit path were ever added.
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
            let _ = tx.send(());
        }
    }
}

struct SessionContext {
    llm: Arc<dyn LLM>,
    callbacks: Arc<dyn HostCallbacks>,
    /// The host's session id; see [`SessionConfig::session_id`].
    session_id: Option<String>,
    tool_defs: ToolDefsProvider,
    goal: Option<GoalProvider>,
    on_before_turn: Option<Arc<dyn Fn() + Send + Sync>>,
    max_steps: u32,
    max_attempts: Option<u32>,
    max_context_tokens: Option<u32>,
    compaction_max_attempts: Option<u32>,
    /// Permission mode for the permission-mode reminders; see
    /// [`SessionConfig::permission_mode`].
    permission_mode: Option<crate::permission::PermissionMode>,
    /// P55: see [`SessionConfig::agent_cancel_slot`].
    agent_cancel_slot: Option<Arc<std::sync::Mutex<Option<crate::subagent::types::ParentCancel>>>>,
    /// See [`SessionConfig::steer_slot`].
    steer_slot: Option<Arc<std::sync::Mutex<Option<crate::subagent::types::ParentCancel>>>>,
    /// Turn-lifecycle hook dispatch; see [`SessionConfig::hook_guard`].
    hook_guard: Option<Arc<crate::tools::external_hooks::HookGuard>>,
    /// Print-mode background policy; see [`SessionConfig::print_background`].
    print_background: Option<PrintBackgroundPolicy>,
    /// The runner the print settle polls; see [`SessionConfig::task_runner`].
    task_runner: Option<Arc<crate::storage::TaskRunner>>,
    /// Disclosure announcement state; see [`SessionConfig::toolset`].
    toolset: Option<Arc<crate::tools::NativeToolset>>,
    /// Cross-turn settle budget of the current print run; see [`PrintRunState`].
    print_run: std::sync::Mutex<PrintRunState>,
    /// Resolves the media references a turn carries (v2
    /// `AgentMediaResolverService`).
    media: crate::llm::media_resolver::MediaResolver,
    /// The session's cross-turn record of omitted media (v2
    /// `media.budgetDropped`).
    media_dropped: crate::llm::media_budget::DroppedMedia,
    /// Host-injected turn-telemetry context; see
    /// [`SessionConfig::telemetry`].
    telemetry: Option<crate::turn_loop::types::TelemetryContext>,
    /// Whether the session's previous turn ended aborted — the next turn's
    /// interruption-reminder trigger (v2 filters `TurnEnded` on the cancelled
    /// reason). The pump writes it when a turn settles; `run_session_turn`
    /// reads and clears it.
    last_turn_aborted: Arc<std::sync::atomic::AtomicBool>,
    /// When the session's pump was created — `SessionHeartbeat` uptime.
    created_at: std::time::Instant,
}

/// The turn lifecycle owner. A cloneable handle; the pump task runs turns
/// serially in the background.
#[derive(Clone)]
pub struct EngineSession {
    core: Arc<Mutex<Core>>,
    wakeup: Arc<Notify>,
    /// Set by [`EngineSession::shutdown`]; the pump checks it at the top of its
    /// loop and returns, dropping its `Arc<Mutex<Core>>` — and with it the whole
    /// conversation. Without this the pump parked on `wakeup` forever and kept
    /// the history alive for the life of the process.
    shutdown: Arc<AtomicBool>,
    steer_queue: Arc<Mutex<Vec<LLMMessage>>>,
    /// Steer-queue-decorated callbacks (for event dispatch + state bridge).
    callbacks: Arc<dyn HostCallbacks>,
    /// P55: shared with the pump (per-turn refresh) and `cancel_turn`
    /// (trigger), and with the native toolset's agent context.
    agent_cancel_slot: Option<Arc<std::sync::Mutex<Option<crate::subagent::types::ParentCancel>>>>,
    /// #3697: shared with the pump (per-turn refresh), the steer admission
    /// (trigger) and the native toolset's `WaitFor`.
    steer_slot: Option<Arc<std::sync::Mutex<Option<crate::subagent::types::ParentCancel>>>>,
    /// Turn-lifecycle hook dispatch; mirrors [`SessionConfig::hook_guard`] so
    /// `EngineSession` (the non-pump side) can fire the events the enqueue
    /// path owns, like `UserPromptQueued`.
    hook_guard: Option<Arc<crate::tools::external_hooks::HookGuard>>,
}

impl EngineSession {
    pub async fn new(config: SessionConfig) -> Self {
        Self::new_seeded(config, crate::session::usage::UsageStatus::default()).await
    }

    /// Create a session seeded with the usage snapshot the host persisted for
    /// it. The engine owns every subsequent fold; the host only stores and
    /// hands back the opaque snapshot across handle builds (v2's durable
    /// `UsageRecord` buckets surviving a restart).
    pub async fn new_seeded(
        config: SessionConfig,
        initial_usage: crate::session::usage::UsageStatus,
    ) -> Self {
        let initial_clock = read_turn_clock(&config.callbacks).await;
        // Subscribed before any turn runs: the watch version tracks ticks, so
        // a completion that lands while the pump is busy still wakes its next
        // idle wait — no lost wakeup between "checked" and "parked".
        let notification_bell = config
            .task_runner
            .as_ref()
            .map(|runner| runner.notification_bell());
        let core = Arc::new(Mutex::new(Core {
            next_turn_id: initial_clock,
            pending: Vec::new(),
            active_turn_id: None,
            active_cancel: None,
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
        let ctx = Arc::new(SessionContext {
            llm: config.llm,
            callbacks: Arc::new(
                SteerQueueCallbacks::new(config.callbacks, steer_queue.clone())
                    .with_steer_slot(config.steer_slot.clone()),
            ),
            tool_defs: config.tool_defs,
            goal: config.goal,
            on_before_turn: config.on_before_turn,
            max_steps: config.max_steps,
            max_attempts: config.max_attempts,
            max_context_tokens: config.max_context_tokens,
            compaction_max_attempts: config.compaction_max_attempts,
            permission_mode: config.permission_mode,
            agent_cancel_slot: config.agent_cancel_slot.clone(),
            steer_slot: config.steer_slot.clone(),
            hook_guard: config.hook_guard.clone(),
            print_background: config.print_background,
            session_id: config.session_id,
            task_runner: config.task_runner.clone(),
            toolset: config.toolset.clone(),
            print_run: std::sync::Mutex::new(PrintRunState::default()),
            media: crate::llm::media_resolver::MediaResolver::new(),
            media_dropped: Default::default(),
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
        }
    }

    /// Stop the pump task and release what it owns. The pump returns at the top
    /// of its loop — after an in-flight turn finishes — and drops its
    /// `Arc<Mutex<Core>>`, so the conversation history is freed with it instead
    /// of staying alive for the life of the process. Idempotent.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // A closing session must not sit inside an in-flight turn's retry
        // budget — at the default 10 attempts × 32 s cap that is minutes, and
        // `dispose()` awaits the pump's release. Flipping the active turn's
        // flag (no `Cancel` event: this is a teardown, not a user steer) makes
        // the step loop abort at the next boundary or during backoff.
        if let Some(flag) = &self
            .core
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .active_cancel
        {
            flag.store(true, Ordering::SeqCst);
        }
        // The pump may be parked on the wakeup. `notify_one` stores a permit
        // when it is not, so the next `notified()` returns immediately and the
        // loop then sees the flag — no lost wakeup either way.
        self.wakeup.notify_one();
    }

    /// Whether [`Self::shutdown`] was called. The liveness predicate for
    /// background tasks (#3717): a shut-down session's late task settles must
    /// stay silent — its pump, the notification drain, is gone.
    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    /// Enqueue a prompt. The turn id is assigned synchronously (monotonic,
    /// never reused — cancelled queued turns consume their id, matching v2's
    /// reserved-id clock), so the caller can cancel by id immediately.
    ///
    /// While a [`QuiescenceGuard`] is alive the request is parked (M1c
    /// backpressure, v2 `heldAdmissions`) and only admitted on guard release.
    pub fn enqueue_turn(&self, request: TurnRequest) -> Result<TurnReceipt, String> {
        let (outcome_tx, outcome_rx) = oneshot::channel();
        let mut core = self.core.lock().unwrap_or_else(|e| e.into_inner());
        // v2 `publishPromptQueued`: a *user* prompt admitted behind an active
        // turn (or parked by a quiescence guard) gets one hook event. Steer
        // admissions never do — upstream routes them through `PromptSteered`,
        // which has no hook.
        let user_prompt = request.origin.get("kind").and_then(|kind| kind.as_str()) == Some("user");
        let queued_behind_active = core.active_turn_id.is_some();
        let prompt_text = request.prompt.content.clone();
        if core.quiescence_depth > 0 {
            // Quiescence: park the request. The id is allocated now (the
            // receipt is already in the caller's hands); admission happens on
            // guard release. There is never an active turn during
            // quiescence, so steering admissions park too.
            let turn_id = core.next_turn_id;
            core.next_turn_id += 1;
            core.held.push(HeldTurn {
                turn_id,
                request,
                cancel: Arc::new(AtomicBool::new(false)),
                outcome: Some(outcome_tx),
            });
            let queue_length = core.held.len();
            drop(core);
            if user_prompt {
                self.fire_user_prompt_queued(turn_id, &prompt_text, queue_length);
            }
            return Ok(TurnReceipt {
                turn_id,
                outcome: outcome_rx,
            });
        }
        let turn_id = Self::admit_locked(
            &mut core,
            request,
            outcome_tx,
            None,
            &self.steer_queue,
            &self.steer_slot,
            &self.wakeup,
        )?;
        let queue_length = core.pending.len();
        drop(core);
        if user_prompt && queued_behind_active {
            self.fire_user_prompt_queued(turn_id, &prompt_text, queue_length);
        }
        Ok(TurnReceipt {
            turn_id,
            outcome: outcome_rx,
        })
    }

    /// Fire the `UserPromptQueued` hook event from the non-async enqueue
    /// path (the notify method spawns the hook command and only needs a
    /// runtime handle).
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

    /// Shared admission logic (v2 `admit`, :242-265). `preallocated_id` is
    /// set when replaying a quiescence-held turn whose id was already handed
    /// out at enqueue time. Notifies the pump after queueing; the caller may
    /// still hold the core lock — `Notify` is non-blocking and the pump
    /// takes the lock only after waking.
    fn admit_locked(
        core: &mut Core,
        request: TurnRequest,
        outcome_tx: oneshot::Sender<Result<TurnOutcome, String>>,
        preallocated_id: Option<u64>,
        steer_queue: &Mutex<Vec<LLMMessage>>,
        steer_slot: &Option<Arc<Mutex<Option<crate::subagent::types::ParentCancel>>>>,
        wakeup: &Notify,
    ) -> Result<u64, String> {
        match request.admission {
            Admission::NewTurn | Admission::ActiveOrNextTurn => {
                let turn_id = match preallocated_id {
                    Some(id) => id,
                    None => {
                        let id = core.next_turn_id;
                        core.next_turn_id += 1;
                        id
                    }
                };
                core.pending.push(PendingTurn {
                    turn_id,
                    prompt: request.prompt,
                    input: request.input,
                    origin: request.origin,
                    cancel: Arc::new(AtomicBool::new(false)),
                    outcome: Some(outcome_tx),
                });
                wakeup.notify_one();
                Ok(turn_id)
            }
            Admission::ActiveOrNewTurn | Admission::ActiveTurnOnly => {
                if let Some(active) = core.active_turn_id {
                    // Steer: the prompt joins the active turn through the
                    // drain_steers seam; the receipt resolves with the active
                    // turn's outcome.
                    core.steer_waiters.push((active, outcome_tx));
                    steer_queue
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(request.prompt);
                    // Fire the turn's steer signal (#3697, v2
                    // `steerController.abort`): a blocking `WaitFor` ends now
                    // and hands the model the new input, instead of holding the
                    // turn until the waited task finishes or times out.
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
                    let turn_id = match preallocated_id {
                        Some(id) => id,
                        None => {
                            let id = core.next_turn_id;
                            core.next_turn_id += 1;
                            id
                        }
                    };
                    core.pending.push(PendingTurn {
                        turn_id,
                        prompt: request.prompt,
                        input: request.input,
                        origin: request.origin,
                        cancel: Arc::new(AtomicBool::new(false)),
                        outcome: Some(outcome_tx),
                    });
                    wakeup.notify_one();
                    Ok(turn_id)
                }
            }
        }
    }

    /// Cancel a turn by id. An active turn is interrupted at the next step
    /// boundary; a queued or quiescence-held turn is dropped before starting
    /// (its receipt resolves with [`TurnOutcome::CancelledBeforeStart`]).
    /// Without an id the active turn (if any) is cancelled. Returns whether
    /// anything was cancelled.
    pub fn cancel_turn(&self, turn_id: Option<u64>) -> bool {
        // Decide under the lock, dispatch after releasing it: `turn_event`
        // hands the event to the host, which must never run while the core
        // mutex is held.
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
                        // v2 always attributes the cancellation to the active
                        // turn's id, even when the caller said "cancel all".
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
                // P55: wake the foreground `Agent` spawn's event-driven wait
                // immediately (the flag store above covers step tops).
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

    /// Live session shape.
    pub fn status(&self) -> SessionStatus {
        let core = self.core.lock().unwrap_or_else(|e| e.into_inner());
        SessionStatus {
            active_turn_id: core.active_turn_id,
            pending_turn_ids: core.pending.iter().map(|t| t.turn_id).collect(),
            engine: core.last_engine.clone(),
            usage: core.usage.clone(),
        }
    }

    /// Try to acquire quiescence (v2 `tryAcquireQuiescence`): an exclusive
    /// window in which enqueued turns are parked instead of admitted. Fails
    /// (`None`) when a guard is already held or any turn is active, pending,
    /// or held — the caller must wait for [`EngineSession::settled`] and
    /// retry. Undo checkpoints, compaction, and reminder injection run
    /// inside this window (v2 consumers: undoService, fullCompactionService,
    /// reminderAgentRuntime).
    pub fn try_acquire_quiescence(&self) -> Option<QuiescenceGuard> {
        let mut core = self.core.lock().unwrap_or_else(|e| e.into_inner());
        if core.quiescence_depth > 0
            || core.active_turn_id.is_some()
            || !core.pending.is_empty()
            || !core.held.is_empty()
        {
            return None;
        }
        core.quiescence_depth += 1;
        Some(QuiescenceGuard {
            core: self.core.clone(),
            steer_queue: self.steer_queue.clone(),
            steer_slot: self.steer_slot.clone(),
            wakeup: self.wakeup.clone(),
        })
    }

    /// Whether the session is fully idle (nothing active, pending, or held)
    /// right now — the non-blocking probe behind [`EngineSession::settled`].
    pub fn is_settled(&self) -> bool {
        let core = self.core.lock().unwrap_or_else(|e| e.into_inner());
        core.active_turn_id.is_none() && core.pending.is_empty() && core.held.is_empty()
    }

    /// Resolves once the session is fully idle: no active turn, no pending
    /// or held turns (v2 `settled`). Session teardown awaits this before
    /// disposing engine-owned resources.
    pub async fn settled(&self) {
        let rx = {
            let mut core = self.core.lock().unwrap_or_else(|e| e.into_inner());
            if core.active_turn_id.is_none() && core.pending.is_empty() && core.held.is_empty() {
                return;
            }
            let (tx, rx) = oneshot::channel();
            core.settle_waiters.push(tx);
            rx
        };
        let _ = rx.await;
    }

    /// Whether the pump has exited and dropped the conversation — the
    /// non-blocking probe behind [`EngineSession::released`].
    pub fn is_released(&self) -> bool {
        self.core.lock().unwrap_or_else(|e| e.into_inner()).released
    }

    /// Resolves once the pump task has exited and its `Arc<Mutex<Core>>` is
    /// the caller's alone: the conversation history is freed and the session's
    /// background tasks have been stopped. This is what `dispose()` promises
    /// (v2's `disposeAsync` / `waitFor(status==='done')` sequence,
    /// `agentLifecycleService.ts:638-641`) — unlike [`Self::settled`], which
    /// only says the session is idle. Resolves immediately once released.
    pub async fn released(&self) {
        let rx = {
            let mut core = self.core.lock().unwrap_or_else(|e| e.into_inner());
            if core.released {
                return;
            }
            let (tx, rx) = oneshot::channel();
            core.release_waiters.push(tx);
            rx
        };
        let _ = rx.await;
    }

    /// Replace the session's cross-turn history. The next enqueued turn
    /// starts from `history` (with the new prompt appended). Used by the
    /// REPL's `/resume` and `/clear` slash commands between turns.
    pub fn set_history(&self, history: Vec<LLMMessage>) {
        self.core.lock().unwrap_or_else(|e| e.into_inner()).history = history;
    }

    /// Clear the session's history (the next enqueued turn starts fresh).
    pub fn clear_history(&self) {
        self.core
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .history
            .clear();
    }

    /// Append messages to the history without starting a turn (REPL cron path:
    /// cron-fired prompts join the next user turn's context).
    pub fn extend_history(&self, msgs: Vec<LLMMessage>) {
        self.core
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .history
            .extend(msgs);
    }

    /// Current history length, for `/status` display. Counts every message
    /// including the system message and the most recent user prompt.
    pub fn history_len(&self) -> usize {
        self.core
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .history
            .len()
    }

    /// A clone of the current cross-turn history. The napi `session_get_history`
    /// serializes this so the host can carry the conversation across an
    /// engine-session rebuild (a mid-session model / permission change) and
    /// implement undo / fork without losing context — the inverse of
    /// [`Self::set_history`].
    pub fn snapshot_history(&self) -> Vec<LLMMessage> {
        self.core
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .history
            .clone()
    }
}

/// Exclusive quiescence window (v2 `IDisposable` from
/// `tryAcquireQuiescence`). Releasing the guard — explicit `drop` or scope
/// exit — replays every held turn in FIFO order and wakes the pump. RAII
/// replaces v2's manual `dispose()`, so an early return inside the window
/// cannot strand the session in quiescence.
pub struct QuiescenceGuard {
    core: Arc<Mutex<Core>>,
    steer_queue: Arc<Mutex<Vec<LLMMessage>>>,
    steer_slot: Option<Arc<Mutex<Option<crate::subagent::types::ParentCancel>>>>,
    wakeup: Arc<Notify>,
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
        // Replay held turns in FIFO order. Admission modes are preserved; a
        // replayed steer admission with no active turn queues a new turn,
        // matching v2's admit-on-replay.
        {
            let mut core = self.core.lock().unwrap_or_else(|e| e.into_inner());
            for entry in held {
                let HeldTurn {
                    turn_id,
                    request,
                    cancel: _,
                    outcome,
                } = entry;
                // The outcome sender is always `Some` here — only the cancel
                // path takes it, and a cancelled held turn never reaches the
                // replay. The fallback keeps the compiler happy without a
                // panic path.
                let outcome = outcome.unwrap_or_else(|| {
                    let (tx, _rx) = oneshot::channel();
                    tx
                });
                let _ = EngineSession::admit_locked(
                    &mut core,
                    request,
                    outcome,
                    Some(turn_id),
                    &self.steer_queue,
                    &self.steer_slot,
                    &self.wakeup,
                );
            }
        }
        self.wakeup.notify_one();
    }
}

/// The 60s `SessionHeartbeat` tick (v2
/// `sessionExternalHooksService.ts:25`): one interval per session, gated on
/// a listener being configured so an unhooked session spends no timer.
/// `uptimeMs` is measured from session creation; like the other session
/// lifecycle payloads, `session_title` is host metadata the engine does not
/// track and stays empty.
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

/// One `SessionHeartbeat` firing — the listener gate (`has_hooks_for`) plus
/// the payload, split from the 60s loop above so the behavior can be tested
/// without waiting out a minute. No guard or no listener = silence.
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

async fn pump(
    core: Arc<Mutex<Core>>,
    ctx: Arc<SessionContext>,
    wakeup: Arc<Notify>,
    shutdown: Arc<AtomicBool>,
    mut notification_bell: Option<tokio::sync::watch::Receiver<u64>>,
) {
    // Held for the pump's whole body: on any exit — the `return` below, an
    // early return added later, or a panic unwind — its `Drop` publishes
    // "released" and wakes `EngineSession::released` waiters.
    let _release = ReleaseGuard::new(core.clone());
    loop {
        // A disposed session's pump must not outlive it: parked on `wakeup` it
        // held `core` — the whole conversation — for the life of the process.
        if shutdown.load(Ordering::SeqCst) {
            // …and neither may its background work. v2 stops a session's tasks
            // as an explicit step of closing it (`IAgentTaskService.stopAllOnExit
            // ('Session closed')`, `agentLifecycleService.ts:629`). The runner is
            // process-wide, so the tasks have to be named here: without this a
            // disposed session's background bash and subagents keep running
            // (ROADMAP §15 D2). The pump is the right place for it — it is the
            // engine's own teardown point and already runs on the runtime.
            if let (Some(runner), Some(session_id)) = (&ctx.task_runner, &ctx.session_id) {
                runner
                    .stop_session(session_id, Some("Session closed"))
                    .await;
            }
            return;
        }
        // Start the next runnable turn when idle. Cancelled entries are
        // dropped (their receipts were resolved at cancel time).
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
                    (entry, core.history.clone())
                })
            }
        };

        // P55: publish this turn's cancel signal into the session-wide slot
        // so the native `Agent` tool's foreground spawn awaits a signal the
        // host's `cancel_turn` can trip immediately (not just at step tops).
        // (Written after the entry destructure below, where `cancel` lives.)

        let (mut entry, history) = match next {
            Some(pair) => pair,
            None => {
                // A completed background task is input the model must see
                // (v2 `taskService.notifyAgentTask` → `loop.notify`): an
                // interactive session turns it into a synthetic turn even
                // though no user prompt is pending. Print runs keep their own
                // settle loop — feeding these back here would surface a turn
                // after `prompt()` already resolved.
                if ctx.print_background.is_none() {
                    let mut core = core.lock().unwrap_or_else(|e| e.into_inner());
                    if enqueue_task_notification_turn(&ctx, &mut core) {
                        continue;
                    }
                }
                // Idle: wait for the next enqueue. A stored permit (an
                // enqueue that raced ahead of this await) wakes immediately;
                // a queued task notification wakes it too, so a task that
                // finishes while the session sits idle starts its own turn.
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
        // The turn is handed `history + prompt`; its result echoes all of it
        // back. The fold below must skip the system message AND the handed
        // history — re-folding the history would duplicate it from the third
        // turn on.
        let history_len = history.len();
        // `mut`: a print steer turn can inherit it — the receipt rides along
        // to the follow-up turn instead of resolving here.
        let mut entry_outcome = entry.outcome.take();
        let PendingTurn {
            turn_id,
            prompt,
            input,
            origin,
            cancel,
            ..
        } = entry;
        let started = std::time::Instant::now();
        if let Some(slot) = &ctx.agent_cancel_slot {
            *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(
                crate::subagent::types::ParentCancel::from_flag(cancel.clone()),
            );
        }
        // A fresh steer signal per turn (#3697): the previous turn's fired
        // signal must not end this turn's first `WaitFor`.
        if let Some(slot) = &ctx.steer_slot {
            *slot.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(crate::subagent::types::ParentCancel::new());
        }
        ctx.callbacks.turn_event(TurnEvent::Prompt {
            turn_id,
            input,
            origin: origin.clone(),
        });
        ctx.callbacks.turn_event(TurnEvent::Started {
            turn_id,
            origin: origin.clone(),
        });
        if let Some(ref guard) = ctx.hook_guard {
            guard
                .notify_turn_started(turn_id, &origin, &prompt.content)
                .await;
        }
        // Kept past the move into `run_session_turn` so the print settle can
        // observe a cancel that arrives while it holds the receipt.
        let turn_cancel = cancel.clone();
        let outcome =
            run_session_turn(&ctx, turn_id, prompt, origin.clone(), cancel, history).await;
        // Captured before the print settle: a drain can hold the receipt for a
        // long time and must not inflate the turn's own reported duration.
        let turn_duration_ms = started.elapsed().as_millis() as u64;

        // v2 `incrementGoalTurn` + `accountTokenUsage` + `settleAbnormalTurn`:
        // fold the finished turn into the session's goal, settle a pause /
        // budget block, and use the post-settle snapshot — not the host's
        // (possibly stale) provider — to decide whether another autonomous
        // turn is owed.
        let goal_after_turn =
            goal_turn::settle_goal_after_turn(&*ctx.callbacks, GoalTurnEnd::from(&outcome)).await;

        // `[background]` print policy (`kimi -p`): drain the session's
        // background tasks while the turn slot is still held, so the session
        // never reads as settled mid-drain and the follow-up decisions below
        // see the final task state. Resolving the receipt is what ends the
        // host's `prompt()`, so holding it here is what keeps the host free
        // of a settle loop of its own.
        let mut print_warnings = Vec::new();
        if outcome.is_ok() && !settle_print_background(&ctx, &turn_cancel).await {
            // Cut short by the ceiling (a cancel stays silent — the host asked
            // for it): v2 warned before finishing.
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
        // Follow-up producers, in v2's settle order (goal → cron → tasks) and
        // only for print runs: interactive sessions keep their per-turn
        // round-trips unchanged (the server prompt loop drives the equivalent
        // continuation for REST / ACP turns).
        let mut goal_continuation = None;
        let mut cron_followups = Vec::new();
        if outcome.is_ok() && ctx.print_background.is_some() {
            // `goal.continuation`: the continuation the store's post-settle
            // snapshot owes. A print run reports it as telemetry and owes the
            // turn itself — no host consumes that event — while a session
            // with cron work falls back to the due jobs only when no goal turn
            // is owed.
            goal_continuation = goal_after_turn.continuation.clone();
            if goal_continuation.is_none() {
                cron_followups = pending_cron_followups(&ctx, &turn_cancel).await;
            }
        }

        // Fold the turn's final messages into the session history (system
        // message excluded — run_turn rebuilds it per turn), release the
        // active slot, and resolve steer receipts with the turn's outcome.
        let steer_receipts = {
            let mut core = core.lock().unwrap_or_else(|e| e.into_inner());
            core.active_turn_id = None;
            core.active_cancel = None;
            if let Some(slot) = &ctx.agent_cancel_slot {
                *slot.lock().unwrap_or_else(|e| e.into_inner()) = None;
            }
            if let Some(slot) = &ctx.steer_slot {
                *slot.lock().unwrap_or_else(|e| e.into_inner()) = None;
            }
            if let Ok(TurnOutcome::Ran(result)) = &outcome {
                // v2 `UsageAgentModel.record`: the engine owns the fold; the
                // host only persists the snapshot this status exposes.
                core.usage.record(ctx.llm.model_name(), &result.usage);
                // Record the interruption-reminder trigger for the next turn
                // (v2 filters `TurnEnded` on the cancelled reason).
                ctx.last_turn_aborted.store(
                    result.stop_reason == crate::turn_loop::types::LoopTurnStopReason::Aborted,
                    std::sync::atomic::Ordering::Relaxed,
                );
                core.history
                    .extend(result.messages.iter().skip(1 + history_len).cloned());
                // P56 (G-5): remember how this turn executed for the
                // cross-process status surface.
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
            // Interactive goal continuation (v2 `launchContinuationTurn`): a
            // goal still active after a settle queues the next turn itself.
            // It carries no prompt receipt — the host's `prompt()` belongs to
            // the user's turn, the way v2's continuation is a separate prompt.
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
            // `steer` / goal / cron: hand what the model is still owed back as
            // follow-up turns, before the idle gate below sees an empty queue.
            print_warnings.extend(maybe_enqueue_print_followup(
                &ctx,
                &mut core,
                &mut entry_outcome,
                goal_continuation,
                cron_followups,
            ));
            // Wake `settled()` waiters when nothing else is queued (M1c).
            maybe_settle_locked(&mut core);
            std::mem::take(&mut core.steer_waiters)
        };
        // Durable: the host folds this into `turnKey.lastEnded` and drives any
        // terminal-state display from it.
        if let Ok(TurnOutcome::Ran(result)) = &outcome {
            let reason = end_reason_of(&result.stop_reason);
            let error = turn_end_error_payload(&result.stop_reason, result.steps);
            ctx.callbacks.turn_event(TurnEvent::Ended {
                turn_id,
                reason,
                error: error.clone(),
                duration_ms: Some(turn_duration_ms),
            });
            if let Some(ref guard) = ctx.hook_guard {
                match reason {
                    // v2 `TurnEnded` filters: Interrupt on cancelled,
                    // StopFailure carries the turn's error payload.
                    TurnEndReason::Cancelled => guard.notify_interrupt(turn_id).await,
                    TurnEndReason::Failed => {
                        let (error_type, error_message) = error
                            .as_ref()
                            .and_then(|payload| {
                                Some((
                                    payload.get("code")?.as_str()?.to_string(),
                                    payload.get("message")?.as_str()?.to_string(),
                                ))
                            })
                            .unwrap_or_else(|| {
                                ("internal".to_string(), format!("{:?}", result.stop_reason))
                            });
                        guard.notify_stop_failure(&error_type, &error_message).await;
                    }
                    _ => {}
                }
            }
        } else if let Err(e) = &outcome {
            let payload = turn_failure_payload(e);
            ctx.callbacks.turn_event(TurnEvent::Ended {
                turn_id,
                reason: TurnEndReason::Failed,
                error: Some(payload.clone()),
                duration_ms: Some(turn_duration_ms),
            });
            if let Some(ref guard) = ctx.hook_guard {
                let error_type = payload
                    .get("code")
                    .and_then(|code| code.as_str())
                    .unwrap_or("internal");
                let error_message = payload
                    .get("message")
                    .and_then(|message| message.as_str())
                    .unwrap_or(e);
                guard.notify_stop_failure(error_type, error_message).await;
            }
            // v2 dispatches a separate `AgentErrorEvent` alongside the failed
            // `turn.ended` (loopService.ts:1533-1537); the host renders that
            // one, so a turn that dies before producing any output is not
            // silent.
            ctx.callbacks.emit_event(payload);
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
    }
}

/// Hold a print-mode session open after a turn while background tasks are still
/// running, bounded by the run's `[background].print_wait_ceiling_s`.
///
/// Runs while the finished turn's slot is still held, before the history fold:
/// the session therefore never reads as settled mid-drain, and the follow-up
/// decision that follows sees the final task state. Returns whether the drain
/// completed — `false` means the ceiling (or a cancel) cut it short with tasks
/// still running. A no-op for `exit`, for an absent policy, and when the
/// process has no task runner wired.
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
    // One deadline for the whole run, anchored at the first settle: v2's
    // ceiling bounds the entire settle phase (drain + steer turns), not each
    // turn's share of it.
    let deadline = {
        let mut state = ctx.print_run.lock().unwrap_or_else(|e| e.into_inner());
        *state.deadline.get_or_insert_with(|| {
            std::time::Instant::now() + std::time::Duration::from_secs(policy.ceiling_s)
        })
    };
    loop {
        // A cancelled turn ends the wait too: the host asked to stop, so
        // holding its receipt longer would only delay the stop.
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

/// The cron jobs a print run owes the model, due within the run's remaining
/// ceiling, as `(prompt, origin)` pairs.
///
/// The native host has no cron dispatcher — `cron.fired` has producers only in
/// the daemon lineage, and the cron tools merely read and write the host's
/// registry — so a print run owns the firing itself: it sleeps until the
/// earliest fire within the ceiling, renders the fired prompts in the
/// documented `<cron-fire>` envelope (docs/reference/tools.md), and deletes
/// one-shot jobs from the registry after firing. Jobs whose fire time already
/// passed are the daemon's missed-fire territory (coalescing) and stay out of
/// scope; `KIMI_DISABLE_CRON=1` disables the whole feature.
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
    // One deadline for the whole run (shared with the drain; see
    // `settle_print_background`). No point sleeping for a fire the budget
    // will not allow.
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
    // A fire past the ceiling will never run inside this run: leave it for
    // whatever ticks the registry next.
    let remaining_ms = deadline
        .checked_duration_since(std::time::Instant::now())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    if fire_at - now > remaining_ms {
        return Vec::new();
    }

    // Sleep until the fire (plus a small grace so a just-due job is caught),
    // in poll-sized chunks so a cancel is observed promptly.
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

    // Fresh registry: jobs may have appeared or vanished while waiting.
    let Some(tasks) = read_cron_registry(ctx).await else {
        return Vec::new();
    };
    let mut scheduler =
        crate::cron::scheduler::CronScheduler::new(tasks, local_utc_offset_minutes());
    let fired = scheduler.tick(fire_at - CRON_FIRE_GRACE_MS, now_ms_epoch());
    let mut followups = Vec::with_capacity(fired.len());
    for fired_entry in fired {
        let entry = &fired_entry.entry;
        // One-shot jobs auto-delete after firing (docs/reference/tools.md);
        // best-effort — the host owns the registry and may reject. Stale
        // recurring jobs go the same way (v2 removes them after firing).
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

/// The host's cron registry, as scheduler entries. `None` on an unwired state
/// bridge; entries missing `id` / `cron` / `prompt` are skipped (the create
/// path validates before storing, so this is defensive only).
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

/// The documented `<cron-fire>` envelope (docs/reference/tools.md): the fired
/// prompt wrapped in attributes the renderers strip before display.
/// `coalesced_count` / `stale` are the real delivery metadata from the
/// scheduler tick — v2 reports how many ideal fires one delivery covers and
/// whether the task outlived the 7-day stale rule.
///
/// Public because the daemon binary (`main.rs`) is a separate crate and wraps
/// its own fired prompts with it.
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

/// `CronJobOrigin` (protocol `events.ts`) — the transcript folds these as
/// cron cards rather than user prompts, exactly what a daemon-fired turn
/// looked like.
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

/// A `WarningEvent` (protocol `events.ts`) for a print run's settle-side
/// warnings; the host renders it wherever its own warnings go.
fn settle_warning(code: &str, message: String) -> serde_json::Value {
    serde_json::json!({ "type": "warning", "message": message, "code": code })
}

/// The process's local UTC offset in minutes east of UTC — the cron module's
/// `tz_offset_minutes`. std has no local-time API; chrono reads the system
/// zone on every platform the crate builds for.
pub(crate) fn local_utc_offset_minutes() -> i32 {
    // `DateTime::offset()` is inherent on chrono's `DateTime<Local>` — no
    // trait import needed.
    chrono::Local::now().offset().local_minus_utc() / 60
}

pub(crate) fn now_ms_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Enqueue the print run's follow-up turns, if the model is owed any, and
/// return the settle warnings the host should see.
///
/// Called under the core lock, before the idle gate: follow-up turns enter
/// `pending` first, so `settled()` waiters and session teardown see a session
/// that is still working. The first follow-up turn inherits the finished
/// turn's receipt (`entry_outcome`), which is what keeps the host's `prompt()`
/// pending until the last follow-up has run — the host stays free of any wait
/// loop.
///
/// Priority follows v2's settle order: the goal continuation, then due cron
/// jobs, then completed background tasks (only in `steer` mode — `drain`
/// waits without feeding back). A pending notification keeps for the
/// follow-up turn's own settle round. Notifications are consumed only once
/// their turn is actually admitted, so a budgeted-out run leaves them queued
/// rather than dropping them silently. Only this session's completions are
/// consumed; another session's stay queued for its own settle.
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

    // Is the model owed anything? The budget warnings only fire when a
    // pending follow-up is actually refused.
    let tasks_pending = policy.mode == PrintBackgroundMode::Steer
        && ctx
            .task_runner
            .as_ref()
            .is_some_and(|runner| runner.pending_notification_count(ctx.session_id.as_deref()) > 0);
    if goal_continuation.is_none() && cron_followups.is_empty() && !tasks_pending {
        return Vec::new();
    }

    // One ceiling and one turn budget for the whole run: v2 bounded the goal,
    // cron, and task turns of a settle phase alike, and `print_max_turns` is
    // documented as the cap on *triggered* turns, not per producer.
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
            // `SystemTriggerOrigin(name: goal_continuation)`: the transcript
            // folds these as their own turn-opening system trigger, exactly
            // what a v2 goal continuation looked like.
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
    // This session's completions only: the runner is shared across sessions, so
    // an unscoped drain would fold another session's task into this turn.
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
        // `TaskOrigin` (protocol `events.ts`): the transcript folds these as
        // task-notification turns instead of user prompts. The first task
        // names the turn; the rest ride in the prompt text.
        serde_json::json!({
            "kind": "task",
            "taskId": first.task_id,
            "status": first.status.as_str(),
            "notificationId": format!("{}-{}", first.task_id, first.ended_at),
        }),
    );
    warnings
}

/// Push the follow-up turn: the model sees the rendered text as a user
/// message, the host sees the same text echoed as the `turn.prompt` input, and
/// the finished turn's receipt rides along so `prompt()` resolves only when
/// this turn ends.
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
        prompt: LLMMessage {
            role: "user".to_string(),
            content: text.clone(),
            ..LLMMessage::default()
        },
        // The `ContentPart[]` echo the host renders for `turn.prompt`; a
        // single text part is the whole story here.
        input: serde_json::json!([{ "type": "text", "text": text }]),
        origin,
        cancel: Arc::new(AtomicBool::new(false)),
        outcome: entry_outcome.take(),
    });
}

/// Queue this session's completed background tasks as a synthetic turn
/// (v2 `taskService.notifyAgentTask` → `loop.notify`). Returns whether a turn
/// was queued.
///
/// The turn is ordinary on the host side — `turn.started` with a `task`
/// origin, a model response, the notification text in the transcript — but
/// it has no prompt receipt: nothing is waiting on it, the way v2's
/// `notify` never resolves a prompt.
fn enqueue_task_notification_turn(ctx: &SessionContext, core: &mut Core) -> bool {
    let Some(runner) = &ctx.task_runner else {
        return false;
    };
    let session_id = ctx.session_id.as_deref();
    if runner.pending_notification_count(session_id) == 0 {
        return false;
    }
    let notifications = runner.take_pending_notifications(session_id);
    let Some(first) = notifications.first() else {
        return false;
    };
    let text = render_task_notifications(&notifications);
    let mut no_receipt = None;
    enqueue_print_followup_turn(
        core,
        &mut no_receipt,
        text,
        serde_json::json!({
            "kind": "task",
            "taskId": &first.task_id,
            "status": first.status.as_str(),
            "notificationId": format!("{}-{}", first.task_id, first.ended_at),
        }),
    );
    true
}

/// The user-role text a steer turn hands the model: one block per completed
/// background task, plus its output preview when the runner kept one.
pub(crate) fn render_task_notifications(
    notifications: &[crate::storage::TaskNotification],
) -> String {
    notifications
        .iter()
        .map(|notification| {
            // The tower wake is already a self-contained instruction (v2's
            // coalesced tower wake): rendering it as a "background task"
            // would tell the model a task finished when none did.
            if notification.task_id == crate::tools::tower::TOWER_WAKE_TASK_ID {
                return notification.description.clone();
            }
            // Upstream #3966: the notification body opens with the task's
            // wall time (v2 `buildAgentTaskNotificationBody`).
            let header = crate::turn_loop::wall_time::wall_time_header(
                notification
                    .ended_at
                    .saturating_sub(notification.started_at),
            );
            let mut block = format!(
                "{header}\nBackground task {} ({}) finished: {}",
                notification.task_id,
                notification.status.as_str(),
                notification.description
            );
            if let Some(preview) = &notification.output_preview {
                block.push('\n');
                block.push_str(preview);
            }
            block
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Map the engine's step-level stop reason onto v2's four-value turn end
/// reason. `MaxTokens` is a finish reason on a response the model did
/// produce, so the turn completed; `Filtered` and the step-budget
/// exhaustion (`max_steps`) fail the turn in v2 (loopService.ts:877-882 and
/// :956-962); `Paused` / `BudgetLimited` stop because the goal cannot
/// progress right now, which v2 reports as blocked; `Aborted` is the
/// cancellation flag (also how a blocked goal stops).
fn end_reason_of(stop: &crate::turn_loop::types::LoopTurnStopReason) -> TurnEndReason {
    use crate::turn_loop::types::LoopTurnStopReason as Stop;
    match stop {
        Stop::EndTurn | Stop::MaxTokens | Stop::RepeatBreaker => TurnEndReason::Completed,
        Stop::Paused | Stop::BudgetLimited => TurnEndReason::Blocked,
        Stop::Aborted => TurnEndReason::Cancelled,
        Stop::Filtered | Stop::MaxSteps | Stop::Unknown => TurnEndReason::Failed,
    }
}

/// The `error` payload v2 puts on `turn.ended` for failing reasons
/// (loop.ts:20-27 `createMaxStepsExceededError`, loopService.ts:877-882
/// `PROVIDER_FILTERED`). `None` for reasons that end the turn cleanly.
fn turn_end_error_payload(
    stop: &crate::turn_loop::types::LoopTurnStopReason,
    steps: u32,
) -> Option<serde_json::Value> {
    use crate::turn_loop::types::LoopTurnStopReason as Stop;
    match stop {
        Stop::MaxSteps => Some(error_payload(
            "loop.max_steps_exceeded",
            format!(
                "Turn exceeded maxSteps={steps}. If max_steps_per_turn is too small, raise it in config.toml (loop_control.max_steps_per_turn), or run \"/update-config\" to update it, then \"/reload\"."
            ),
        )),
        Stop::Filtered => Some(error_payload(
            "provider.filtered",
            "Provider safety policy blocked the response.".to_string(),
        )),
        _ => None,
    }
}

/// A protocol `KimiErrorPayload` (events.ts) — the shape `turn.ended.error`
/// and the `error` event both carry. `retryable` is false throughout: the
/// engine has already exhausted its own retry budget by the time a turn
/// fails, so the host must not offer another attempt.
fn error_payload(code: &str, message: String) -> serde_json::Value {
    serde_json::json!({
        "type": "error",
        "code": code,
        "message": message,
        "retryable": false,
    })
}

/// Classify a failed turn's error string onto a protocol error code. The
/// engine's LLM layer reports transport failures as `llm http status <code>
/// <reason>: <body>` (llm/http.rs:287) and everything else as a plain
/// message, so the status line is the only structured signal available.
fn turn_failure_code(message: &str) -> &'static str {
    // The transport wraps its own error (`LLM call failed after N attempts:
    // llm transport error connect: …`), so the marker is searched for rather
    // than stripped from the head.
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
            // The endpoint never answered: unreachable, refused, or too slow.
            "connect" | "timeout" | "transport" => "provider.connection_error",
            // The endpoint answered with something unparseable, or the request
            // could not be built — the provider's answer either way, not ours.
            _ => "provider.api_error",
        };
    }
    if message.contains("connection") || message.contains("timed out") {
        "provider.connection_error"
    } else {
        "internal"
    }
}

/// The `error` payload for a turn that failed outright (the engine returned
/// `Err`). The message is the engine's own error text, kept verbatim so the
/// provider's response body reaches the user.
fn turn_failure_payload(message: &str) -> serde_json::Value {
    error_payload(turn_failure_code(message), message.to_string())
}

/// Whether a turn's origin is one an undo may cut back to (v2 undo anchor).
///
/// Mirrors the host's `isUndoAnchorPrompt` (`sdk-client/helpers.ts`): a plain
/// user prompt or a user-slash skill activation / plugin command, and never
/// an in-turn prompt. Only these turns push a durable-state checkpoint, so
/// the engine's stack stays one-to-one with the turns the host's
/// `computeUndoCut` removes.
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
    // The undo anchor for this turn: snapshot todo/plan/goal before the model
    // can change them, so undo can restore the state the turn started from.
    // Continuations (goal follow-up, cron fire, task wake) are not anchors —
    // the host's undo never cuts there.
    if origin_is_undo_anchor(&origin)
        && let Err(error) = ctx.callbacks.checkpoint_state().await
    {
        tracing::warn!(%error, "state checkpoint failed; undo cannot restore this turn's state");
    }
    let tool_defs = (ctx.tool_defs)().await;
    // Read-and-clear the previous turn's abort outcome for the interruption
    // reminder (v2 `interruptionReminderService`).
    let previous_turn_aborted = ctx
        .last_turn_aborted
        .swap(false, std::sync::atomic::Ordering::Relaxed);
    let goal = match ctx.goal.as_ref() {
        Some(provider) => provider().await,
        None => None,
    };
    // v2 #3832: the turn's origin rides the opening user message, so the
    // cross-turn history (and the host's replay of it) can tell a skill
    // activation from a typed prompt. The engine does not model origin
    // variants — the value is opaque and echoed verbatim.
    prompt.origin = Some(origin);
    let mut messages = history;
    messages.push(prompt);
    let input = RunTurnInput {
        // Use the value read above: `swap` is read-and-clear, so re-reading
        // `last_turn_aborted` here would consume the flag a second time and
        // always observe `false` — the interruption reminder would never fire.
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
        toolset: ctx.toolset.clone(),
    };
    let result = match &ctx.telemetry {
        // M1c (v2 #3963): the host injected a telemetry context, so the turn
        // lifecycle rides the host/telemetry seam. The goal-domain events stay
        // with `run_turn_with_telemetry` — this pump drives its own goal
        // follow-ups (see the print settle below).
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

/// Callbacks decorator: serves the session's steer queue through the
/// `drain_steers` seam the turn loop already consumes (native transports
/// only — in host-proxy mode the host owns steering). Everything else
/// delegates unchanged.
pub(crate) struct SteerQueueCallbacks {
    inner: Arc<dyn HostCallbacks>,
    steer_queue: Arc<Mutex<Vec<LLMMessage>>>,
    /// #3697: the turn's steer slot. Draining a steer refreshes it, so the
    /// turn that consumed the message can wait again instead of tripping over
    /// the interrupt that just ended its previous wait.
    steer_slot: Option<Arc<Mutex<Option<crate::subagent::types::ParentCancel>>>>,
}

impl SteerQueueCallbacks {
    pub(crate) fn new(
        inner: Arc<dyn HostCallbacks>,
        steer_queue: Arc<Mutex<Vec<LLMMessage>>>,
    ) -> Self {
        Self {
            inner,
            steer_queue,
            steer_slot: None,
        }
    }

    /// [`Self::new`] with the session's steer slot, so draining refreshes the
    /// turn's steer signal.
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
        Box::pin(async move {
            let drained = std::mem::take(&mut *queue.lock().unwrap_or_else(|e| e.into_inner()));
            // The turn has now consumed the steering messages, so refresh its
            // steer signal (v2 `loopService`: a fresh `steerController` once no
            // undropped steer nudge remains). Without this the next `WaitFor`
            // would observe the already-fired signal and abort instantly.
            if !drained.is_empty()
                && let Some(slot) = &slot
            {
                *slot.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(crate::subagent::types::ParentCancel::new());
            }
            Ok(drained)
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
