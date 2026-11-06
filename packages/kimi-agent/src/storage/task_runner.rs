//! Background task runner for the standalone REPL (P32 批 2).
//!
//! Provides a local task runner over the task domain of the state store:
//! [`TaskRunner`] registers spawned tokio tasks by id, snapshots their
//! output on completion, and mirrors the task status (`running` /
//! `completed` / `killed`) back into `<workspace>/.kimi/state/task.json`
//! so the state bridge's TaskList / TaskOutput / TaskStop / TaskWait
//! renderers see the same wire shapes as the v2 host.
//!
//! Cancellation is cooperative: [`TaskRunner::stop`] sets a per-task
//! cancel flag and wakes the runner's wrapper, which races the task
//! future against the flag at the task's next yield point (the future is
//! dropped, cancelling its pending work). The wrapper settles the entry
//! (`killed` + `stopReason` + `endedAt`) and notifies waiters. A task
//! that finishes before the stop lands settles as `completed`.
//!
//! Waiters are woken through a shared completion future
//! (`futures_util::future::Shared` over a oneshot), which resolves for
//! every waiter — including waiters that register after the task already
//! settled — so a wait can never miss the completion.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::{BoxFuture, FutureExt, Shared};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Notify, oneshot};
use tokio::task::JoinHandle;

use crate::storage::StateStore;
use crate::turn_loop::types::LLMMessage;

/// The grace period [`TaskRunner::stop`] waits for a task to settle at
/// its next cooperative point before returning (v2 `SIGTERM_GRACE_MS`).
const STOP_GRACE: Duration = Duration::from_secs(5);

const PARTIAL_OUTPUT_MAX_CHARS: usize = 32 * 1024;
const PARTIAL_OUTPUT_NOTICE: &str = "[partial output] The subagent did not finish this run. Below is the text it wrote before it ended; it is incomplete and unverified.";
const PARTIAL_OUTPUT_TRUNCATED_MARKER: &str = "[earlier text truncated]";

/// Public assistant text streamed by one subagent run. Retried attempts are
/// discarded and only the most recent 32K Unicode code points are retained.
#[derive(Default)]
struct RunOutput {
    turn_id: Option<String>,
    steps: Vec<String>,
    length: usize,
    truncated: bool,
}

impl RunOutput {
    fn accepts_turn(&self, turn_id: &str) -> bool {
        self.turn_id
            .as_deref()
            .is_none_or(|current| current == turn_id)
    }

    fn bind_turn(&mut self, turn_id: &str) {
        if self.turn_id.is_none() {
            self.turn_id = Some(turn_id.to_string());
        }
    }

    fn start_step(&mut self, turn_id: &str) {
        if !self.accepts_turn(turn_id) {
            return;
        }
        self.bind_turn(turn_id);
        self.steps.push(String::new());
    }

    fn append(&mut self, turn_id: &str, delta: &str) {
        if !self.accepts_turn(turn_id) {
            return;
        }
        self.bind_turn(turn_id);
        if self.steps.is_empty() {
            self.steps.push(String::new());
        }
        self.steps.last_mut().unwrap().push_str(delta);
        self.length += delta.chars().count();
        if self.length > PARTIAL_OUTPUT_MAX_CHARS * 2 {
            self.drop_head(self.length - PARTIAL_OUTPUT_MAX_CHARS);
        }
    }

    fn discard_step(&mut self, turn_id: &str) {
        if !self.accepts_turn(turn_id) {
            return;
        }
        if let Some(last) = self.steps.last_mut() {
            self.length = self.length.saturating_sub(last.chars().count());
            last.clear();
        }
    }

    fn drop_head(&mut self, excess: usize) {
        self.truncated = true;
        let mut remaining = excess;
        while self.steps.len() > 1 {
            let first_len = self.steps[0].chars().count();
            if first_len > remaining {
                break;
            }
            self.steps.remove(0);
            self.length = self.length.saturating_sub(first_len);
            remaining -= first_len;
        }
        if remaining == 0 || self.steps.is_empty() {
            return;
        }
        let first_len = self.steps[0].chars().count();
        let dropped = remaining.min(first_len);
        let kept = self.steps[0].chars().skip(dropped).collect();
        self.steps[0] = kept;
        self.length = self.length.saturating_sub(dropped);
    }

    fn render(&self) -> String {
        let text = self
            .steps
            .iter()
            .map(|step| step.trim())
            .filter(|step| !step.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        let length = text.chars().count();
        let dropped = length.saturating_sub(PARTIAL_OUTPUT_MAX_CHARS);
        let kept = text.chars().skip(dropped).collect::<String>();
        if kept.is_empty() || (!self.truncated && dropped == 0) {
            kept
        } else {
            format!("{PARTIAL_OUTPUT_TRUNCATED_MARKER}\n{kept}")
        }
    }
}

/// The runner's view of a task's status; the wire strings match the v2
/// task domain (`running` / `completed` / `killed` / `lost`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskStatus {
    Running,
    Completed,
    /// The task's work reported a failure. Distinct from `Killed`, which means
    /// a `stop()` landed before completion (v2 `AgentTaskStatus.failed`).
    Failed,
    Killed,
    /// A task the previous process left mid-flight: the entry was persisted
    /// as running when the app exited, so this process has no handle to it
    /// (v2 `AgentTaskStatus`'s `lost`).
    Lost,
}

impl TaskStatus {
    /// The task-domain wire string. The counterpart on the TypeScript side is
    /// `TaskLifecycleStatus` (`packages/node-sdk/src/types.ts`), which spans
    /// six values: `running` / `completed` / `failed` / `timed_out` /
    /// `killed` / `lost`.
    ///
    /// `timed_out` has no arm here on purpose. v2 reaches it through
    /// `coerceTimeoutSettlement` (`agent/task/taskService.ts:178-186`), which
    /// rewrites a `killed` settlement when the task had a timeout — and the
    /// per-task timeout mechanism that sets that flag is not ported yet (it
    /// rides the same gap as the `keepAliveOnExit` flag). Adding the variant
    /// before its producer would put a status on the wire that nothing can
    /// ever emit.
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskStatus::Running => "running",
            TaskStatus::Completed => "completed",
            TaskStatus::Failed => "failed",
            TaskStatus::Killed => "killed",
            TaskStatus::Lost => "lost",
        }
    }
}

/// What a task's future produced — v2 `AgentTaskSettlement`.
///
/// The output used to be a bare `String`, which made failure **inexpressible**:
/// a subagent that errored returned its message like any other output, so the
/// runner booked it `completed` and the parent session was told a failed agent
/// had succeeded. Every production caller already knew which arm it was in —
/// they emitted `subagent.failed` from it — so the verdict had to travel with
/// the text rather than be inferred from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskOutcome {
    /// The work finished; the string is its output.
    Completed(String),
    /// The work failed; the string is the error text, kept as the task output
    /// so the reader sees why.
    Failed(String),
}

impl From<String> for TaskOutcome {
    fn from(output: String) -> Self {
        TaskOutcome::Completed(output)
    }
}

impl TaskOutcome {
    /// The status this outcome settles as, and the output to record.
    fn settle(self) -> (TaskStatus, Option<String>) {
        match self {
            TaskOutcome::Completed(output) => (TaskStatus::Completed, Some(output)),
            TaskOutcome::Failed(output) => (TaskStatus::Failed, Some(output)),
        }
    }
}

/// The outcome of [`TaskRunner::wait`]: the task settled, the wait
/// elapsed while the task was still running, a signal cut it short, or no
/// such task exists.
#[derive(Debug)]
pub enum TaskWaitResult {
    /// The task reached a terminal state; carries the settled entry wire.
    Completed(Value),
    /// The wait elapsed before the task finished; carries the current
    /// (still-running) entry wire.
    TimedOut(Value),
    /// An interrupt signal (a steering message) ended the wait early; the
    /// waited task is untouched and still running.
    Interrupted,
    /// No task with this id is registered.
    NotFound,
}

/// Spawn context a caller can attach: which session the task belongs to and
/// what kind of work it is, so the lifecycle events reach the right lane with
/// the Web vocabulary's task shapes.
#[derive(Debug, Clone, Default)]
pub struct TaskSpawnMeta<'a> {
    pub session_id: Option<&'a str>,
    /// `subagent` | `bash` | `tool` (kimi-web `WireTask.kind`).
    pub kind: &'a str,
    pub subagent_type: Option<&'a str>,
    /// The subagent's stable id, for a subagent task. Persisted so the
    /// previous-session reminder can name the `Agent(resume=…)` that picks the
    /// context back up (v2 `previousSessionTaskLine`'s `agentId`).
    pub agent_id: Option<&'a str>,
}

/// One registered background task.
struct TaskEntry {
    id: String,
    description: String,
    started_at: u64,
    status: TaskStatus,
    ended_at: Option<u64>,
    stop_reason: Option<String>,
    output: Option<String>,
    /// Live public text for a running subagent; rendered only if the run ends
    /// without a normal completion.
    partial_output: RunOutput,
    /// Which session spawned the task (`event.task.*` lane routing).
    session_id: Option<String>,
    /// `subagent` | `bash` | `tool`.
    kind: String,
    subagent_type: Option<String>,
    /// The subagent's stable id (`kind == "subagent"`); see
    /// [`TaskSpawnMeta::agent_id`].
    agent_id: Option<String>,
    /// Cooperative cancellation flag, set by `stop()`; the spawned
    /// wrapper checks it at the task's next yield point.
    cancel: Arc<AtomicBool>,
    /// Wakes the wrapper when the cancel flag is set.
    cancel_notify: Arc<Notify>,
    /// Resolves once the wrapper has settled the entry; every waiter
    /// (including late ones) observes the completion.
    done: Shared<BoxFuture<'static, ()>>,
    /// The spawned tokio task.
    handle: Option<JoinHandle<()>>,
}

/// The lifecycle sink signature: the task's session (for lane routing; `None`
/// means the `global` lane) and the event payload.
pub type TaskEventSink = Arc<dyn Fn(Option<&str>, Value) + Send + Sync>;

/// The `[background]` knobs one engine context applies to its own task runner
/// and Bash tool. Every field is optional: `None` keeps the built-in behavior
/// (5s stop grace / unlimited concurrency / auto-background on / 600s
/// background Bash timeout), so an unconfigured file behaves exactly as
/// before.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BackgroundLimits {
    /// `kill_grace_period_ms`: how long `stop` waits for a cooperative exit.
    pub kill_grace_period_ms: Option<u64>,
    /// `max_running_tasks`: cap on concurrently running tasks; `Some(0)` (and
    /// `None`) mean unlimited.
    pub max_running_tasks: Option<u32>,
    /// `bash_auto_background_on_timeout`: migrate a timed-out foreground Bash
    /// call to the background instead of killing it.
    pub bash_auto_background_on_timeout: Option<bool>,
    /// `bash_task_timeout_s`: default timeout for background Bash tasks;
    /// `Some(0)` means "no timeout".
    pub bash_task_timeout_s: Option<u64>,
}

impl BackgroundLimits {
    /// Build from host-resolved wire values (the session params of the napi /
    /// stdio entries). A non-positive grace period or concurrency cap is not
    /// meaningful and is treated as unset; `bash_task_timeout_s` keeps `0`,
    /// which is a real value meaning "no timeout".
    #[must_use]
    pub fn from_wire(
        kill_grace_period_ms: Option<u64>,
        max_running_tasks: Option<u64>,
        bash_auto_background_on_timeout: Option<bool>,
        bash_task_timeout_s: Option<u64>,
    ) -> Self {
        Self {
            kill_grace_period_ms: kill_grace_period_ms.filter(|value| *value > 0),
            max_running_tasks: max_running_tasks
                .filter(|value| *value > 0)
                .and_then(|value| u32::try_from(value).ok()),
            bash_auto_background_on_timeout,
            bash_task_timeout_s,
        }
    }
}

/// Local background task runner: a registry of spawned tasks plus their
/// output snapshots, with the task domain state mirrored into the
/// [`StateStore`] when one is attached.
pub struct TaskRunner {
    tasks: Mutex<HashMap<String, TaskEntry>>,
    /// Serializes task.json read-modify-write cycles so concurrent
    /// settles cannot lose each other's updates.
    persist_lock: Mutex<()>,
    store: Option<StateStore>,
    /// Completion notifications awaiting delivery (v2
    /// `task.notificationDelivery`). Settled tasks push here so a host
    /// or in-process consumer can drain and surface them as
    /// conversation input; the runner itself does not inject — v2
    /// wires the dispatcher, repl, and host to consume.
    pending_notifications: Mutex<Vec<TaskNotification>>,
    /// Optional lifecycle sink (the server's hub): fired as
    /// `event.task.created` / `background.task.started` on spawn and
    /// `event.task.completed` / `background.task.terminated` on settle, with
    /// the task's session for lane routing. Absent = purely local runner.
    event_sink: Mutex<Option<TaskEventSink>>,
    /// Host-injected liveness predicate (v2 `taskService.lifecycleActive`):
    /// `Some(task_session) -> bool`. `false` = the owning session is gone
    /// (pump disposed), so a late settle announces nothing and queues
    /// nothing. Absent = no gating (the legacy always-fire behavior).
    liveness_check: Mutex<Option<TaskLivenessCheck>>,
    /// User-configured external hooks (v2 `agentExternalHooksService`):
    /// `TaskStarted` fires on spawn, `Notification` when a completion
    /// notification is queued. `None`, or unset, skips both dispatches.
    hook_guard: Mutex<Option<Arc<crate::tools::external_hooks::HookGuard>>>,
    /// How long [`Self::stop`] waits for a cooperative exit
    /// (`[background].kill_grace_period_ms`); defaults to [`STOP_GRACE`].
    /// Mutable so a shared runner can pick the value up after construction
    /// (the daemon receives its config after the runner exists).
    kill_grace: Mutex<Duration>,
    /// Cap on concurrently running tasks
    /// (`[background].max_running_tasks`); `0` is unlimited.
    max_running: AtomicUsize,
    /// Wakes idle session pumps when a completion notification is queued:
    /// v2's `loop.notify` starts a turn in a session that is not running one.
    /// A watch channel, not `Notify` — the runner is process-wide, and
    /// `notify_one` would let one session's pump consume another session's
    /// wakeup; a receiver created with the session sees every later bump.
    notification_bell: tokio::sync::watch::Sender<u64>,
}

/// A host-injected liveness predicate: `Some(task session id) -> alive?`.
/// The `None` lane (server-level tasks) is always alive — it has no session
/// lifecycle to outlive.
pub type TaskLivenessCheck = Arc<dyn Fn(Option<&str>) -> bool + Send + Sync>;

/// A task completion event queued by [`TaskRunner::settle_task`] and
/// drained via [`TaskRunner::take_pending_notifications`]. Mirrors v2's
/// `task.notificationDelivery` payload shape: identifier, description,
/// terminal status, optional output preview, and wall-clock start/end times
/// (the rendered notification carries a `Wall time` line, upstream #3966).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskNotification {
    pub task_id: String,
    pub description: String,
    pub status: TaskStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_preview: Option<String>,
    pub started_at: u64,
    pub ended_at: u64,
    /// The session the task was spawned for; `None` = server-level (see
    /// [`TaskRunner::take_pending_notifications`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

impl TaskRunner {
    /// Create a runner; `store` is the state store whose `task` domain
    /// receives the status updates (`None` keeps the runner purely
    /// in-memory).
    pub fn new(store: Option<StateStore>) -> Self {
        Self {
            tasks: Mutex::new(HashMap::new()),
            persist_lock: Mutex::new(()),
            store,
            pending_notifications: Mutex::new(Vec::new()),
            event_sink: Mutex::new(None),
            liveness_check: Mutex::new(None),
            hook_guard: Mutex::new(None),
            kill_grace: Mutex::new(STOP_GRACE),
            max_running: AtomicUsize::new(0),
            notification_bell: tokio::sync::watch::channel(0).0,
        }
    }

    /// Apply the `[background]` knobs: the cooperative-stop grace period and
    /// the cap on concurrently running tasks. Unset values keep the
    /// built-in behavior (5s / unlimited).
    #[must_use]
    pub fn with_background_limits(
        self,
        kill_grace_period_ms: Option<u64>,
        max_running_tasks: Option<u32>,
    ) -> Self {
        self.apply_background_limits(kill_grace_period_ms, max_running_tasks);
        self
    }

    /// [`Self::with_background_limits`] on a shared runner: the daemon
    /// resolves its config after the runner was built.
    pub fn apply_background_limits(
        &self,
        kill_grace_period_ms: Option<u64>,
        max_running_tasks: Option<u32>,
    ) {
        if let Some(ms) = kill_grace_period_ms.filter(|ms| *ms > 0) {
            *self.kill_grace.lock().unwrap() = Duration::from_millis(ms);
        }
        if let Some(Ok(cap)) = max_running_tasks
            .filter(|cap| *cap > 0)
            .map(usize::try_from)
        {
            self.max_running.store(cap, Ordering::Relaxed);
        }
    }

    /// Install the lifecycle event sink (the server does this once, fanning
    /// out to the task's session lane — or `global` when the task has none).
    pub fn set_event_sink(&self, sink: TaskEventSink) {
        *self.event_sink.lock().unwrap() = Some(sink);
    }

    /// Install the external-hook gate so task lifecycle events reach
    /// user-configured `[[hooks]]` commands (`TaskStarted` /
    /// `Notification`). Like the event sink, this is per-runner state the
    /// pipeline installs once it owns the runner.
    pub fn set_hook_guard(&self, guard: Arc<crate::tools::external_hooks::HookGuard>) {
        *self.hook_guard.lock().unwrap() = Some(guard);
    }

    /// Install the liveness predicate (v2 `taskService.lifecycleActive`): the
    /// host answers "is this task's session still alive?" — for the server a
    /// session-row lookup, for the stdio/napi hosts a session-registry
    /// membership test. Late settles for a dead session go fully silent
    /// (no terminal events, no queued notification), matching v2's
    /// `recordTaskTerminated` + `notifyAgentTask` early return.
    pub fn set_liveness_check(&self, check: TaskLivenessCheck) {
        *self.liveness_check.lock().unwrap() = Some(check);
    }

    /// The liveness gate for one task's session: absent predicate = always
    /// alive (legacy behavior), `None` session = server-level (alive).
    fn session_alive(&self, session_id: Option<&str>) -> bool {
        match self.liveness_check.lock().unwrap().as_ref() {
            None => true,
            Some(check) => check(session_id),
        }
    }

    /// Emit one live output chunk for a running task on its session lane
    /// (`event.task.progress`). A no-op for an unknown task or when no sink is
    /// wired, so a streaming caller can report unconditionally.
    pub fn emit_progress(&self, task_id: &str, output_chunk: &str, stream: &str) {
        let session = {
            let tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
            match tasks.get(task_id) {
                Some(entry) => entry.session_id.clone(),
                None => return,
            }
        };
        self.fire_event(
            session.as_deref(),
            serde_json::json!({
                "type": "event.task.progress",
                "task_id": task_id,
                "output_chunk": output_chunk,
                "stream": stream,
            }),
        );
    }

    fn fire_event(&self, session_id: Option<&str>, event: Value) {
        if let Some(sink) = self.event_sink.lock().unwrap().as_ref() {
            sink(session_id, event);
        }
    }

    /// Create a runner backed by the state store of a workspace
    /// (`<workspace>/.kimi/state/`).
    pub fn for_workspace(workspace_root: &Path) -> std::io::Result<Self> {
        Ok(Self::new(Some(StateStore::for_workspace(workspace_root)?)))
    }

    /// Register a background task and spawn it on the current tokio
    /// runtime. The task's output is snapshotted when it completes; a
    /// `stop()` that lands before completion settles the task as `killed`,
    /// preserving any public partial output captured for a subagent. The
    /// runner must be shared (`Arc`) so the
    /// spawned wrapper can settle the entry.
    ///
    /// Takes a plain `String` because this shape has no failure notion: the
    /// future's *output* is the result, whatever it says. Callers that can fail
    /// want [`Self::spawn_task_with_meta`] and a [`TaskOutcome`].
    pub fn spawn_task<F>(
        self: &Arc<Self>,
        id: String,
        description: String,
        future: F,
    ) -> Result<(), String>
    where
        F: Future<Output = String> + Send + 'static,
    {
        self.spawn_task_with_meta(
            TaskSpawnMeta {
                session_id: None,
                kind: "tool",
                subagent_type: None,
                agent_id: None,
            },
            id,
            description,
            async move { TaskOutcome::from(future.await) },
        )
    }

    /// [`Self::spawn_task`] with spawn context: the task's session (event
    /// lane routing) and work kind (the Web task vocabulary's shapes), and a
    /// future that can report whether it succeeded — see [`TaskOutcome`].
    pub fn spawn_task_with_meta<F>(
        self: &Arc<Self>,
        meta: TaskSpawnMeta<'_>,
        id: String,
        description: String,
        future: F,
    ) -> Result<(), String>
    where
        F: Future<Output = TaskOutcome> + Send + 'static,
    {
        let mut tasks = self.tasks.lock().unwrap();
        if tasks.contains_key(&id) {
            return Err(format!("task already exists: {id}"));
        }
        // `[background].max_running_tasks`: refuse rather than queue, so the
        // caller sees the limit immediately (v2 `TASK_LIMIT_EXCEEDED`).
        let cap = self.max_running.load(Ordering::Relaxed);
        if cap > 0 {
            let running = tasks
                .values()
                .filter(|entry| entry.status == TaskStatus::Running)
                .count();
            if running >= cap {
                return Err(format!(
                    "Too many background tasks are already running (limit {cap})."
                ));
            }
        }
        let (done_tx, done_rx) = oneshot::channel::<()>();
        let done: Shared<BoxFuture<'static, ()>> = Box::pin(async move {
            let _ = done_rx.await;
        })
        .boxed()
        .shared();
        let started_at = now_ms();
        let entry = TaskEntry {
            id: id.clone(),
            description: description.clone(),
            started_at,
            status: TaskStatus::Running,
            ended_at: None,
            stop_reason: None,
            output: None,
            partial_output: RunOutput::default(),
            session_id: meta.session_id.map(str::to_string),
            kind: meta.kind.to_string(),
            subagent_type: meta.subagent_type.map(str::to_string),
            agent_id: meta.agent_id.map(str::to_string),
            cancel: Arc::new(AtomicBool::new(false)),
            cancel_notify: Arc::new(Notify::new()),
            done,
            handle: None,
        };
        self.persist_wire(&self.entry_wire(&entry));
        let cancel = Arc::clone(&entry.cancel);
        let cancel_notify = Arc::clone(&entry.cancel_notify);
        let spawn_session = entry.session_id.clone();
        let spawn_kind = entry.kind.clone();
        let spawn_subagent_type = entry.subagent_type.clone();
        let spawn_agent_id = entry.agent_id.clone();
        tasks.insert(id.clone(), entry);
        let runner = Arc::clone(self);
        let task_id = id.clone();
        let handle = tokio::spawn(async move {
            let output = tokio::select! {
                biased;
                output = future => Some(output),
                _ = cancelled(cancel, cancel_notify) => None,
            };
            let (status, stop_reason, output) = match output {
                // The work finished and said whether it succeeded. This is the
                // arm that used to collapse both verdicts into `completed`.
                Some(outcome) => {
                    let (status, output) = outcome.settle();
                    (status, None, output)
                }
                None => {
                    // Killed via the cancel flag, which only `stop()` sets —
                    // and `stop()` records the reason on the entry up front —
                    // so the wrapper settles with `None` and never overwrites
                    // it. Nothing else produces a kill, which is why a
                    // subagent that was cancelled upstream settles `Failed`
                    // rather than reaching this arm.
                    (TaskStatus::Killed, None, None)
                }
            };
            runner.settle_task(&task_id, status, output, stop_reason);
            let _ = done_tx.send(());
        });
        tasks.get_mut(&id).unwrap().handle = Some(handle);
        drop(tasks);

        // Announce the task in both vocabularies the Web client consumes:
        // the protocol shape (mappers `event.task.created`) and the legacy
        // agent-event name (projector allowlist).
        let session = spawn_session.as_deref();
        self.fire_event(
            session,
            serde_json::json!({
                "type": "event.task.created",
                "task": {
                    "id": id,
                    "session_id": session,
                    "kind": spawn_kind,
                    "description": description,
                    "status": "running",
                    "created_at": chrono::Utc::now().to_rfc3339(),
                    "started_at": chrono::Utc::now().to_rfc3339(),
                    "subagent_type": spawn_subagent_type,
                    // The agent↔task association (upstream #3970): the
                    // transcript fold adopts the spawn placeholder keyed by
                    // this id instead of duplicating the task.
                    "agent_id": spawn_agent_id,
                    "run_in_background": true,
                },
            }),
        );
        self.fire_event(
            session,
            serde_json::json!({
                "type": "background.task.started",
                "task_id": id,
                "description": description,
            }),
        );
        // v2 `TaskStarted` hook event (agentExternalHooksService): hooks
        // match against the task kind.
        let guard = self.hook_guard.lock().unwrap().clone();
        if let Some(guard) = guard {
            let kind = spawn_kind.clone();
            let description = description.clone();
            let task_id = id.clone();
            tokio::spawn(async move {
                guard
                    .notify_task_started(&task_id, &kind, &description, "running", started_at)
                    .await;
            });
        }
        Ok(())
    }

    /// Ids of the tasks still `running`, oldest first.
    ///
    /// The print-mode settle loop (`session/mod.rs`) polls this until the
    /// list drains or its ceiling elapses: the runner owns no completion
    /// signal a caller can await on, so a poll is the only way to observe
    /// "the background work finished".
    pub fn running_ids(&self) -> Vec<String> {
        let tasks = self.tasks.lock().unwrap();
        let mut running: Vec<(&u64, &String)> = tasks
            .values()
            .filter(|entry| entry.status == TaskStatus::Running)
            .map(|entry| (&entry.started_at, &entry.id))
            .collect();
        running.sort_by_key(|(started_at, _)| **started_at);
        running.into_iter().map(|(_, id)| id.clone()).collect()
    }

    /// Reconcile tasks the previous process left behind, returning the
    /// previous-session reminder text when there is anything to report.
    ///
    /// A previous process wrote its in-flight tasks into the `task` domain but
    /// died before settling them, so this process has no handle to them: they
    /// are marked `lost` and written back, and the model is told they lost
    /// contact rather than being left to assume they completed. Ports v2
    /// `AgentTaskService::reconcile` — `markLoadedTasksLost` then
    /// `appendPreviousSessionTasksReminder` — including the one-shot
    /// `resumeReminded` marker that keeps a task out of later reminders.
    ///
    /// Two gates decide whether a loss is *reported*: an id this process
    /// still holds a handle to (an in-process task is not a previous-session
    /// loss — v2's `ghosts` distinction), and an id `already` contains —
    /// the ids an earlier reminder in this conversation named (v2
    /// `hasPreviousSessionReminder`). Either way the entry itself is still
    /// marked `lost` and persisted with its marker. Each newly reported
    /// loss also queues a [`TaskNotification`] behind the settle path's
    /// session-liveness gate.
    ///
    /// `None` when no store is attached, or when nothing needs reporting.
    pub fn reconcile_previous_session(&self, already: &HashSet<String>) -> Option<String> {
        let store = self.store.as_ref()?;
        let (reported, notifications) = {
            // A task *this process* spawned live sits in the domain as
            // `running` (`spawn_task_with_meta` persists it) with an
            // in-process handle behind it, so it is not a previous-session
            // loss. The registry is held across the domain read — a spawn
            // landing between a snapshot and the read would otherwise be
            // misreported. Lock order is `tasks` before `persist_lock`:
            // spawn and settle take the two in that order, and reversing it
            // deadlocks.
            let live_tasks = self.tasks.lock().unwrap();
            let _guard = self.persist_lock.lock().unwrap();
            let tasks = store
                .read_domain("task")
                .and_then(|v| v.as_array().cloned())
                .unwrap_or_default();

            let mut reported: Vec<String> = Vec::new();
            let mut notifications: Vec<TaskNotification> = Vec::new();
            let mut updated: Vec<Value> = Vec::with_capacity(tasks.len());
            let mut changed = false;
            for task in tasks {
                let status = task
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                // Only a still-running entry is a previous-session loss; a settled
                // one already told its story.
                if status != TaskStatus::Running.as_str() {
                    updated.push(task);
                    continue;
                }
                let task_id = task
                    .get("taskId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if live_tasks.contains_key(&task_id) {
                    updated.push(task);
                    continue;
                }
                let remindable = task
                    .get("resumeReminded")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                // A task an earlier reminder already announced is never
                // announced again — but its loss is still persisted, and the
                // marker keeps later reconciles silent too.
                let report = !remindable && !already.contains(&task_id);
                let mut task = task;
                if let Some(object) = task.as_object_mut() {
                    object.insert("status".into(), json!(TaskStatus::Lost.as_str()));
                    if !object.contains_key("endedAt") {
                        object.insert("endedAt".into(), json!(now_ms()));
                    }
                    // The marker is persisted with the loss so a later reconcile
                    // stays silent (v2 `persistPreviousSessionReminderMarker`).
                    if !remindable {
                        object.insert("resumeReminded".into(), json!(true));
                        changed = true;
                    }
                }
                if report {
                    notifications.push(previous_session_notification(&task));
                    reported.push(previous_session_task_line(&task));
                }
                updated.push(task);
            }

            if changed && store.write_domain("task", &Value::Array(updated)).is_err() {
                // A failed write means the reminder would fire again next time;
                // reporting it now is still the honest outcome.
            }
            (reported, notifications)
        };
        // Enqueue only now that `persist_lock` is released (the settle path
        // takes the two in that order too), behind the same session-liveness
        // gate and deduped by `(task_id, status)` — a marker write that
        // failed re-reports the loss, but never re-queues its notification.
        let mut queued = false;
        for notification in notifications {
            if !self.session_alive(notification.session_id.as_deref()) {
                // v2 `notifyAgentTask`'s `lifecycleActive` guard, surfaced
                // (upstream #4280).
                tracing::warn!(
                    task_id = %notification.task_id,
                    status = %notification.status.as_str(),
                    "task notification skipped: agent lifecycle inactive"
                );
                continue;
            }
            let mut queue = self.pending_notifications.lock().unwrap();
            let duplicate = queue.iter().any(|queued| {
                queued.task_id == notification.task_id && queued.status == notification.status
            });
            if duplicate {
                // v2 `scheduledNotificationKeys` redelivery suppression,
                // surfaced at debug (upstream #4280).
                tracing::debug!(
                    task_id = %notification.task_id,
                    "task notification already scheduled; in-process redelivery suppressed"
                );
            } else {
                queue.push(notification);
                queued = true;
            }
        }
        if queued {
            self.ring_notification_bell();
        }
        if reported.is_empty() {
            return None;
        }
        Some(format!(
            "{}\n{}\n{}",
            PREVIOUS_SESSION_REMINDER_HEADER,
            reported.join("\n"),
            "Don't assume any of them completed; check current state (they may still be running), then re-run or resume only what you still need.",
        ))
    }

    /// Capture public stream events for the running subagent task that owns
    /// `agent_id`. Main-agent events and already-settled tasks are ignored.
    pub(crate) fn observe_subagent_event(&self, event: &Value) {
        let Some(agent_id) = event
            .get("agent_id")
            .or_else(|| event.get("agentId"))
            .and_then(Value::as_str)
        else {
            return;
        };
        let event_type = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let turn_id = event
            .get("turn_id")
            .or_else(|| event.get("turnId"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if turn_id.is_empty() {
            return;
        }
        let mut tasks = self.tasks.lock().unwrap();
        for entry in tasks.values_mut().filter(|entry| {
            entry.status == TaskStatus::Running
                && entry.kind == "subagent"
                && entry.agent_id.as_deref() == Some(agent_id)
        }) {
            match event_type {
                "llm.step.begin" => entry.partial_output.start_step(turn_id),
                "llm.delta" => {
                    let part = event.get("part");
                    if part
                        .and_then(|part| part.get("type"))
                        .and_then(Value::as_str)
                        == Some("text")
                        && let Some(text) = part
                            .and_then(|part| part.get("text"))
                            .and_then(Value::as_str)
                    {
                        entry.partial_output.append(turn_id, text);
                    }
                }
                "assistant.delta" => {
                    if let Some(text) = event.get("delta").and_then(Value::as_str) {
                        entry.partial_output.append(turn_id, text);
                    }
                }
                _ => {}
            }
        }
    }

    /// A retry invalidates only the current step attempt of the matching run.
    pub(crate) fn observe_subagent_telemetry(&self, event: &Value) {
        if event.get("event").and_then(Value::as_str) != Some("TurnStepRetrying") {
            return;
        }
        let Some(turn_id) = event
            .get("turn_id")
            .or_else(|| event.get("turnId"))
            .and_then(Value::as_str)
        else {
            return;
        };
        let mut tasks = self.tasks.lock().unwrap();
        for entry in tasks
            .values_mut()
            .filter(|entry| entry.status == TaskStatus::Running && entry.kind == "subagent")
        {
            entry.partial_output.discard_step(turn_id);
        }
    }

    /// The output snapshot of a settled task; `None` while the task is
    /// still running (or unknown), or when a stopped subagent produced no
    /// public assistant text.
    pub fn get_output(&self, id: &str) -> Option<String> {
        let tasks = self.tasks.lock().unwrap();
        let entry = tasks.get(id)?;
        if entry.status == TaskStatus::Running {
            return None;
        }
        entry.output.clone()
    }

    /// Request a cooperative stop: sets the task's cancel flag and wakes
    /// its wrapper, then waits up to [`STOP_GRACE`] for the task to
    /// settle at its next yield point. Returns the current entry wire —
    /// `killed` once the task settled, the still-running entry if it
    /// never yields (the wrapper settles it later). Stopping a terminal
    /// task returns its current entry unchanged.
    ///
    /// The `reason` becomes the entry's `stopReason` (blank/`None` falls
    /// back to `"Stopped by TaskStop"`); a reason already on the entry is
    /// never overwritten.
    pub async fn stop(&self, id: &str, reason: Option<&str>) -> Result<Value, String> {
        let done = {
            let mut tasks = self.tasks.lock().unwrap();
            let Some(entry) = tasks.get_mut(id) else {
                return Err(format!("Task not found: {id}"));
            };
            if entry.status != TaskStatus::Running {
                let mut wire = self.entry_wire(entry);
                if entry.kind == "subagent"
                    && let Some(object) = wire.as_object_mut()
                {
                    // A repeated subagent TaskStop does not deliver the same
                    // output again; TaskOutput remains the persisted read path.
                    object.remove("output");
                    object.remove("outputPath");
                }
                return Ok(wire);
            }
            if entry.stop_reason.is_none() {
                let reason = reason
                    .map(str::trim)
                    .filter(|r| !r.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| "Stopped by TaskStop".to_string());
                entry.stop_reason = Some(reason);
            }
            entry.cancel.store(true, Ordering::Relaxed);
            entry.cancel_notify.notify_waiters();
            entry.done.clone()
        };
        let grace = *self.kill_grace.lock().unwrap();
        let _ = tokio::time::timeout(grace, done).await;
        let tasks = self.tasks.lock().unwrap();
        let entry = tasks.get(id).unwrap();
        Ok(self.entry_wire(entry))
    }

    /// Cooperatively stop EVERY running task spawned for `session_id`.
    ///
    /// v2 stops a session's tasks as an explicit step of closing it
    /// (`IAgentTaskService.stopAllOnExit('Session closed')` —
    /// `session/agentLifecycle/agentLifecycleService.ts:629`). This runner is
    /// process-wide and shared across sessions, so a session's teardown has no
    /// way to reach its tasks by dropping state: it has to name them (ROADMAP
    /// §15 D2 — closing a session used to leave its background bash and
    /// subagents running).
    ///
    /// Every owned running task is flagged under ONE lock and only then
    /// awaited, so a task that never yields cannot hold up stopping the rest,
    /// and the whole wait is bounded by a single grace period rather than one
    /// per task. `reason` defaults to `"Session closed"`, v2's wording.
    ///
    /// Returns the ids it flagged (unordered). Already-terminal tasks are left
    /// alone — their `stopReason` is never overwritten — and a session with
    /// nothing running returns an empty list. As with [`Self::stop`], a task
    /// that does not yield stays `running` in the entry until its wrapper
    /// settles it; the flag and the reason are what this guarantees.
    pub async fn stop_session(&self, session_id: &str, reason: Option<&str>) -> Vec<String> {
        let reason = reason
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .unwrap_or("Session closed")
            .to_string();
        let mut ids = Vec::new();
        let mut waits: Vec<Shared<BoxFuture<'static, ()>>> = Vec::new();
        {
            let mut tasks = self.tasks.lock().unwrap();
            for (id, entry) in tasks.iter_mut() {
                if entry.session_id.as_deref() != Some(session_id) {
                    continue;
                }
                if entry.status != TaskStatus::Running {
                    continue;
                }
                if entry.stop_reason.is_none() {
                    entry.stop_reason = Some(reason.clone());
                }
                entry.cancel.store(true, Ordering::Relaxed);
                entry.cancel_notify.notify_waiters();
                ids.push(id.clone());
                waits.push(entry.done.clone());
            }
        }
        if waits.is_empty() {
            return ids;
        }
        let grace = *self.kill_grace.lock().unwrap();
        let settle = async {
            for done in waits {
                done.await;
            }
        };
        let _ = tokio::time::timeout(grace, settle).await;
        ids
    }

    /// Wait for a task to settle, up to `timeout_ms` (v2 `wait`
    /// semantics: a terminal task returns immediately, `timeout_ms == 0`
    /// returns the current entry without waiting, and a timeout is not
    /// an error — the caller decides whether to wait again).
    pub async fn wait(&self, id: &str, timeout_ms: u64) -> TaskWaitResult {
        self.wait_interruptible(id, timeout_ms, None).await
    }

    /// [`Self::wait`] with an interrupt signal: the wait ends early, as
    /// [`TaskWaitResult::Interrupted`], the moment `signal` fires — leaving
    /// the waited task running. A signal that already fired ends the wait
    /// before it parks, so a trigger racing the call is never missed.
    pub async fn wait_interruptible(
        &self,
        id: &str,
        timeout_ms: u64,
        signal: Option<&crate::subagent::types::ParentCancel>,
    ) -> TaskWaitResult {
        let done = {
            let tasks = self.tasks.lock().unwrap();
            let Some(entry) = tasks.get(id) else {
                return TaskWaitResult::NotFound;
            };
            if entry.status != TaskStatus::Running {
                return TaskWaitResult::Completed(self.entry_wire(entry));
            }
            entry.done.clone()
        };
        if timeout_ms == 0 {
            let tasks = self.tasks.lock().unwrap();
            let entry = tasks.get(id).unwrap();
            return TaskWaitResult::TimedOut(self.entry_wire(entry));
        }
        if signal.is_some_and(|signal| signal.triggered()) {
            return TaskWaitResult::Interrupted;
        }
        let deadline = tokio::time::sleep(Duration::from_millis(timeout_ms));
        tokio::pin!(deadline);
        let interrupted = async {
            match signal {
                Some(signal) => signal.wait().await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            _ = done => {
                let tasks = self.tasks.lock().unwrap();
                let entry = tasks.get(id).unwrap();
                TaskWaitResult::Completed(self.entry_wire(entry))
            }
            _ = &mut deadline => {
                let tasks = self.tasks.lock().unwrap();
                let entry = tasks.get(id).unwrap();
                let wire = self.entry_wire(entry);
                // A task that settled just as the timeout fired reports the
                // terminal state, matching v2's post-race status check.
                if entry.status != TaskStatus::Running {
                    TaskWaitResult::Completed(wire)
                } else {
                    TaskWaitResult::TimedOut(wire)
                }
            }
            _ = interrupted => TaskWaitResult::Interrupted,
        }
    }

    /// Wait for the first of `ids` to settle, up to `timeout_ms` (v2
    /// `WaitFor.waitAny`). Every id is raced under the same deadline and
    /// signal, so an interrupt or a timeout applies to the whole wait.
    /// `NotFound` when `ids` is empty.
    pub async fn wait_any(
        &self,
        ids: &[String],
        timeout_ms: u64,
        signal: Option<&crate::subagent::types::ParentCancel>,
    ) -> TaskWaitResult {
        if ids.is_empty() {
            return TaskWaitResult::NotFound;
        }
        let waits = ids
            .iter()
            .map(|id| self.wait_interruptible(id, timeout_ms, signal).boxed());
        let (first, _index, _rest) = futures_util::future::select_all(waits).await;
        first
    }

    /// Every registered task's entry wire, oldest first; the output
    /// snapshot is omitted (TaskList does not carry output).
    pub fn list(&self) -> Vec<Value> {
        let tasks = self.tasks.lock().unwrap();
        let mut entries: Vec<Value> = tasks.values().map(|entry| self.entry_wire(entry)).collect();
        for entry in &mut entries {
            if let Some(obj) = entry.as_object_mut() {
                obj.remove("output");
            }
        }
        entries.sort_by(|a, b| {
            let a_started = a.get("startedAt").and_then(|v| v.as_u64()).unwrap_or(0);
            let b_started = b.get("startedAt").and_then(|v| v.as_u64()).unwrap_or(0);
            a_started
                .cmp(&b_started)
                .then_with(|| a["taskId"].as_str().cmp(&b["taskId"].as_str()))
        });
        entries
    }

    /// One task's entry wire (with the output snapshot when settled), or
    /// `None` for an unknown id.
    pub fn entry(&self, id: &str) -> Option<Value> {
        let tasks = self.tasks.lock().unwrap();
        tasks.get(id).map(|entry| self.entry_wire(entry))
    }

    /// Settle a task from its spawned wrapper: record the terminal
    /// status, the end time, the output snapshot and the stop reason,
    /// then mirror the entry into the task domain.
    fn settle_task(
        &self,
        id: &str,
        status: TaskStatus,
        output: Option<String>,
        stop_reason: Option<String>,
    ) {
        let (
            description,
            started_at,
            ended_at,
            output_preview,
            wire,
            session_id,
            kind,
            output_bytes,
        ) = {
            let mut tasks = self.tasks.lock().unwrap();
            let Some(entry) = tasks.get_mut(id) else {
                return;
            };
            entry.status = status;
            entry.ended_at = Some(now_ms());
            let partial = if entry.kind == "subagent" && status != TaskStatus::Completed {
                entry.partial_output.render()
            } else {
                String::new()
            };
            let partial =
                (!partial.is_empty()).then(|| format!("{PARTIAL_OUTPUT_NOTICE}\n\n{partial}"));
            entry.output = match (output, partial) {
                (Some(output), _) if output.contains("[partial output]") => Some(output),
                (Some(output), Some(partial)) => Some(format!("{output}\n\n{partial}")),
                (Some(output), None) => Some(output),
                (None, partial) => partial,
            };
            entry.partial_output = RunOutput::default();
            if let Some(reason) = stop_reason {
                entry.stop_reason = Some(reason);
            }
            let ended_at = entry.ended_at.unwrap_or(now_ms());
            let preview = entry.output.as_deref().and_then(truncate_preview);
            let description = entry.description.clone();
            let wire = self.entry_wire(entry);
            (
                description,
                entry.started_at,
                ended_at,
                preview,
                wire,
                entry.session_id.clone(),
                entry.kind.clone(),
                entry.output.as_ref().map(|o| o.len()),
            )
        };
        if let Some(output) = wire.get("output").and_then(|v| v.as_str())
            && let Some(store) = &self.store
        {
            store.write_task_output(id, output);
        }
        self.persist_wire(&wire);
        // Queue a completion notification for delivery (v2
        // `task.notificationDelivery`). Consumers (host, repl, engine
        // injection) drain via `take_pending_notifications`.
        // #3717 late-settle silence: the drain happens on the owning
        // session's pump, so when the host reports that session gone the
        // notification has no future reader — queueing it just leaks. v2
        // gates `notifyAgentTask` on `lifecycleActive()` the same way.
        if self.session_alive(session_id.as_deref()) {
            // v2 `Notification` hook event (agentExternalHooksService,
            // delivered-as-context task notifications): hooks match against
            // the terminal status.
            let guard = self.hook_guard.lock().unwrap().clone();
            if let Some(guard) = guard {
                let (task_id, description, status, started_at) = (
                    id.to_string(),
                    description.clone(),
                    status.as_str(),
                    started_at,
                );
                tokio::spawn(async move {
                    guard
                        .notify_task_notification(
                            &task_id,
                            &description,
                            status,
                            started_at,
                            ended_at,
                        )
                        .await;
                });
            }
            let pending = {
                let mut queue = self.pending_notifications.lock().unwrap();
                queue.push(TaskNotification {
                    task_id: id.to_string(),
                    description,
                    status,
                    output_preview: output_preview.clone(),
                    started_at,
                    ended_at,
                    session_id: session_id.clone(),
                });
                queue.len()
            };
            // v2 `loop.notify`'s buffered-arm debug (upstream #4280): the
            // notification sits in the queue until the owning session's pump
            // drains it into a turn.
            tracing::debug!(
                task_id = %id,
                pending,
                "loop notify buffered before reaching the machine"
            );
            self.ring_notification_bell();
        } else {
            // v2 `notifyAgentTask`'s `lifecycleActive` guard, surfaced
            // (upstream #4280): the drop itself must be visible.
            tracing::warn!(
                task_id = %id,
                status = %status.as_str(),
                "task notification skipped: agent lifecycle inactive"
            );
        }
        // Terminal facts, both vocabularies (mappers + projector). Same
        // liveness gate as v2's `recordTaskTerminated`: a session teardown
        // must not be answered by terminal events for tasks it will never
        // see.
        if self.session_alive(session_id.as_deref()) {
            let session = session_id.as_deref();
            self.fire_event(
                session,
                serde_json::json!({
                    "type": "event.task.completed",
                    "task_id": id,
                    "status": status.as_str(),
                    "output_preview": output_preview,
                    "output_bytes": output_bytes,
                }),
            );
            self.fire_event(
                session,
                serde_json::json!({
                    "type": "background.task.terminated",
                    "task_id": id,
                    "status": status.as_str(),
                    "kind": kind,
                }),
            );
        } else {
            // v2 `recordTaskTerminated`'s guard, surfaced (upstream #4280).
            tracing::warn!(
                task_id = %id,
                status = %status.as_str(),
                "task terminal event dropped: agent lifecycle inactive"
            );
        }
    }

    /// Drain the completion notifications **of one session**: only
    /// notifications whose task was spawned for `session_id` are returned, and
    /// they leave the queue. Another session's notification stays queued for
    /// its own drain — the runner is server-scoped and shared across sessions,
    /// so an unscoped drain lets one session's print turn consume another
    /// session's completion and turn it into a follow-up turn.
    ///
    /// `None` (a caller with no session id) drains nothing: a task spawned
    /// without a session id is server-level, and handing it to whichever
    /// session drains first is exactly the cross-session leak this scoping
    /// exists to prevent. Such notifications stay queued for a server-level
    /// consumer.
    pub fn take_pending_notifications(&self, session_id: Option<&str>) -> Vec<TaskNotification> {
        let Some(session_id) = session_id else {
            return Vec::new();
        };
        let mut queue = self.pending_notifications.lock().unwrap();
        let mut taken = Vec::new();
        let mut kept = Vec::with_capacity(queue.len());
        for notification in queue.drain(..) {
            if notification.session_id.as_deref() == Some(session_id) {
                taken.push(notification);
            } else {
                kept.push(notification);
            }
        }
        *queue = kept;
        if !taken.is_empty() {
            // v2 `consumeNudges`'s drained-arm debug (upstream #4280).
            tracing::debug!(
                live = taken.len(),
                remaining = queue.len(),
                "loop consumed drained notifications"
            );
        }
        taken
    }

    /// Pending notification count for one session (for hosts that want to know
    /// whether to poll without consuming the queue). Scoped like
    /// [`Self::take_pending_notifications`], so a `None` caller counts nothing.
    pub fn pending_notification_count(&self, session_id: Option<&str>) -> usize {
        let Some(session_id) = session_id else {
            return 0;
        };
        self.pending_notifications
            .lock()
            .unwrap()
            .iter()
            .filter(|notification| notification.session_id.as_deref() == Some(session_id))
            .count()
    }

    /// A receiver that fires whenever a completion notification is queued.
    ///
    /// Each idle session pump watches this to start the notification turn
    /// v2's `loop.notify` would have started; the watch's version counter
    /// means a bump that arrives while the pump is busy is still observed on
    /// its next wait, and one session cannot consume another's wakeup.
    pub fn notification_bell(&self) -> tokio::sync::watch::Receiver<u64> {
        self.notification_bell.subscribe()
    }

    /// The sessions with at least one queued completion notification. The
    /// server's wake service drains each one into its own notification turn;
    /// `None`-scoped (server-level) notifications are not listed.
    pub fn sessions_with_pending_notifications(&self) -> Vec<String> {
        let queue = self
            .pending_notifications
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut sessions: Vec<String> = queue
            .iter()
            .filter_map(|notification| notification.session_id.clone())
            .collect();
        sessions.sort();
        sessions.dedup();
        sessions
    }

    fn ring_notification_bell(&self) {
        self.notification_bell.send_modify(|version| *version += 1);
    }

    /// Inject a synthetic wake notification for `session_id`, **coalesced**:
    /// a pending wake with the same task id is replaced in place (text and
    /// timestamp refreshed) so a burst of sends becomes one notification per
    /// drain — v2 `towerService`'s coalesced tower wake. The session pump
    /// drains it like any task notification and turns it into a follow-up
    /// turn, which is what actually wakes the tower.
    ///
    /// Mirrors `settle_task`'s two channels: the terminal events fire on the
    /// session lane (gated on liveness, so a dead session is not announced
    /// to) and the notification queues for the print/steer drain. No task
    /// entry is created — the wake is not a task and stays out of
    /// TaskList/TaskOutput.
    pub fn enqueue_wake(&self, session_id: &str, task_id: &str, description: String) {
        if !self.session_alive(Some(session_id)) {
            // Same lifecycle guard as `settle_task`'s notification arm,
            // surfaced (upstream #4280).
            tracing::warn!(
                task_id = %task_id,
                "task notification skipped: agent lifecycle inactive"
            );
            return;
        }
        let mut queue = self.pending_notifications.lock().unwrap();
        let now = now_ms();
        let description = match queue.iter_mut().find(|notification| {
            notification.task_id == task_id
                && notification.session_id.as_deref() == Some(session_id)
        }) {
            Some(existing) => {
                existing.description = description;
                existing.ended_at = now;
                existing.description.clone()
            }
            None => {
                queue.push(TaskNotification {
                    task_id: task_id.to_string(),
                    description: description.clone(),
                    status: TaskStatus::Completed,
                    output_preview: None,
                    started_at: now,
                    ended_at: now,
                    session_id: Some(session_id.to_string()),
                });
                description
            }
        };
        drop(queue);
        self.ring_notification_bell();
        self.fire_event(
            Some(session_id),
            serde_json::json!({
                "type": "event.task.completed",
                "task_id": task_id,
                "status": "completed",
                "output_preview": description,
                "output_bytes": 0,
            }),
        );
        self.fire_event(
            Some(session_id),
            serde_json::json!({
                "type": "background.task.terminated",
                "task_id": task_id,
                "status": "completed",
                "kind": "other",
            }),
        );
    }

    /// Drop a pending wake: leaving tower mode must not answer the session
    /// with a stale "check your inbox" notification (v2 drops queued tower
    /// inbox wakes on exit).
    pub fn cancel_wake(&self, session_id: &str, task_id: &str) {
        let mut queue = self.pending_notifications.lock().unwrap();
        queue.retain(|notification| {
            !(notification.task_id == task_id
                && notification.session_id.as_deref() == Some(session_id))
        });
    }

    /// The task entry as the state bridge wire value.
    ///
    /// Two consumers read this one document: the in-tree REST/napi clients
    /// (and the persisted `task` state domain) use the camelCase pair
    /// `taskId` / `startedAt`, while the official Web bundle's task mapper
    /// requires the protocol shape — a snake_case `id`, matching
    /// `event.task.created`, and a boolean `run_in_background` it throws on
    /// when absent. Carry both rather than picking a side.
    fn entry_wire(&self, entry: &TaskEntry) -> Value {
        let started_iso = chrono::DateTime::from_timestamp_millis(entry.started_at as i64)
            .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
        let ended_iso = entry
            .ended_at
            .and_then(|ended| chrono::DateTime::from_timestamp_millis(ended as i64))
            .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));

        let mut obj = serde_json::Map::new();
        obj.insert("taskId".into(), json!(entry.id));
        obj.insert("description".into(), json!(entry.description));
        obj.insert("status".into(), json!(entry.status.as_str()));
        obj.insert("startedAt".into(), json!(entry.started_at));
        if let Some(ended_at) = entry.ended_at {
            obj.insert("endedAt".into(), json!(ended_at));
        }
        if let Some(reason) = &entry.stop_reason {
            obj.insert("stopReason".into(), json!(reason));
        }
        if let Some(output) = &entry.output {
            obj.insert("output".into(), json!(output));
            if entry.kind == "subagent"
                && let Some(store) = &self.store
            {
                obj.insert(
                    "outputPath".into(),
                    json!(store.task_output_path(&entry.id).to_string_lossy()),
                );
            }
        }
        // The spawning session: the task domain is workspace-scoped, so
        // per-session history pages filter on this field.
        if let Some(session) = &entry.session_id {
            obj.insert("sessionId".into(), json!(session));
        }
        // Protocol vocabulary (`event.task.created` / the Web task mapper).
        obj.insert("id".into(), json!(entry.id));
        obj.insert("kind".into(), json!(entry.kind));
        if let Some(session) = &entry.session_id {
            obj.insert("session_id".into(), json!(session));
        }
        if let Some(started_iso) = started_iso {
            obj.insert("started_at".into(), json!(started_iso));
        }
        if let Some(ended_iso) = &ended_iso {
            obj.insert("completed_at".into(), json!(ended_iso));
            obj.insert("ended_at".into(), json!(ended_iso));
        }
        if let Some(subagent_type) = &entry.subagent_type {
            obj.insert("subagent_type".into(), json!(subagent_type));
        }
        // The subagent's stable id: the previous-session reminder names it in
        // an `Agent(resume=…)` hint, so it has to survive the process that
        // spawned the task.
        if let Some(agent_id) = &entry.agent_id {
            obj.insert("agentId".into(), json!(agent_id));
        }
        // Every task the runner tracks outlives its spawning tool call, so the
        // protocol flag is definitionally true (the event producer hardcodes
        // the same value).
        obj.insert("run_in_background".into(), json!(true));
        Value::Object(obj)
    }

    /// Mirror an entry wire into the `task` domain of the state store
    /// (best-effort: a missing store or a failed write only logs). The
    /// task domain file carries the v2 task entry shape only; the output
    /// log is persisted separately by `settle_task` via
    /// `StateStore::write_task_output`.
    fn persist_wire(&self, wire: &Value) {
        let Some(store) = &self.store else {
            return;
        };
        let _guard = self.persist_lock.lock().unwrap();
        let mut tasks = store
            .read_domain("task")
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default();
        let id = wire
            .get("taskId")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let mut stored = wire.clone();
        if let Some(obj) = stored.as_object_mut() {
            obj.remove("output");
        }
        match tasks
            .iter_mut()
            .find(|t| t.get("taskId").and_then(|v| v.as_str()) == Some(id))
        {
            Some(entry) => *entry = stored,
            None => tasks.push(stored),
        }
        if let Err(error) = store.write_domain("task", &Value::Array(tasks)) {
            tracing::warn!(target: "kimi_agent::storage::task_runner", "persist task state: {error}");
        }
    }
}

/// Await the cooperative cancellation flag: returns as soon as `stop()`
/// has been called for the task. The flag is re-checked around the
/// notification wait so a stop that lands between the check and the wait
/// is never missed.
async fn cancelled(cancel: Arc<AtomicBool>, cancel_notify: Arc<Notify>) {
    loop {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let notified = cancel_notify.notified();
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        notified.await;
    }
}

/// Truncate a task output for inclusion in the completion notification
/// payload (v2 `renderNotificationXml` keeps a head/tail preview).
const NOTIFICATION_PREVIEW_CHARS: usize = 500;

fn truncate_preview(output: &str) -> Option<String> {
    let total = output.chars().count();
    if total <= NOTIFICATION_PREVIEW_CHARS {
        return Some(output.to_string());
    }
    // For moderate lengths, keep head and tail so the model can still see
    // what the task started and finished with.
    let head: String = output.chars().take(NOTIFICATION_PREVIEW_CHARS).collect();
    let tail: String = output
        .chars()
        .rev()
        .take(NOTIFICATION_PREVIEW_CHARS)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    Some(format!("{head}\n…\n{tail}"))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// One reminder line for a lost task (v2 `previousSessionTaskLine`).
///
/// A subagent entry names the `Agent(resume=…)` that picks its context back
/// up; a process entry renders as `bash`, matching the tool the user ran.
///
/// The engine's wire kind for a subagent is `subagent` where v2 said `agent`
/// (`sdk-rpc-client-native.ts` maps the two the same way). `agentId` stays a
/// separate field because the two ids are not always equal: the foreground
/// spawn uses the agent id as the task id, while a background resume mints a
/// fresh task id and carries the agent id alongside it.
fn previous_session_task_line(task: &Value) -> String {
    let task_id = task
        .get("taskId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let description = task
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let kind = task.get("kind").and_then(Value::as_str).unwrap_or_default();
    if kind == "subagent"
        && let Some(agent_id) = task.get("agentId").and_then(Value::as_str)
    {
        return format!(
            "- {task_id} \"{description}\" (subagent) — resume it with Agent(resume=\"{agent_id}\", prompt=\"Pick up where you left off; redo the last tool call if its result was never observed.\") to continue from its prior context."
        );
    }
    let label = if kind == "bash" { "bash" } else { kind };
    format!("- {task_id} \"{description}\" ({label})")
}

/// The opening line of the previous-session reminder (v2
/// `appendPreviousSessionTasksReminder`'s verbatim first line). Shared by
/// [`TaskRunner::reconcile_previous_session`], which emits it, and
/// [`scan_previous_session_reminders`], which recognises a reminder by it,
/// so the two cannot drift.
const PREVIOUS_SESSION_REMINDER_HEADER: &str = "The user exited the application after your last turn, so your background tasks from the previous session lost contact:";

/// The notification for one newly reported previous-session loss, in the
/// same payload shape [`TaskRunner::settle_task`] queues (v2
/// `task.notificationDelivery`): the ids off the persisted entry, an
/// optional output preview, and wall-clock start/end — `startedAt` falls
/// back to `endedAt` for an entry persisted without a start time.
fn previous_session_notification(task: &Value) -> TaskNotification {
    let ended_at = task
        .get("endedAt")
        .and_then(Value::as_u64)
        .unwrap_or_else(now_ms);
    TaskNotification {
        task_id: task
            .get("taskId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        description: task
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        status: TaskStatus::Lost,
        output_preview: task
            .get("output")
            .and_then(Value::as_str)
            .and_then(truncate_preview),
        started_at: task
            .get("startedAt")
            .and_then(Value::as_u64)
            .unwrap_or(ended_at),
        ended_at,
        session_id: task
            .get("sessionId")
            .or_else(|| task.get("session_id"))
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

/// The task ids an earlier previous-session reminder already named in this
/// conversation, scanned off the injection history (v2
/// `hasPreviousSessionReminder`'s transcript prefix lookup). Feeds
/// [`TaskRunner::reconcile_previous_session`], which reports a loss only
/// when the id is absent here — so a lost `resumeReminded` write cannot
/// repeat a reminder the model has already seen.
pub fn scan_previous_session_reminders(messages: &[LLMMessage]) -> HashSet<String> {
    let mut seen = HashSet::new();
    for message in messages {
        if !crate::injection::is_system_reminder(&message.content)
            || !message.content.contains(PREVIOUS_SESSION_REMINDER_HEADER)
        {
            continue;
        }
        for line in message.content.lines() {
            let Some(rest) = line.strip_prefix("- ") else {
                continue;
            };
            // `- <taskId> "<description>" (<kind>)` — v2 looks the id up by
            // the same `- <taskId> "` prefix.
            if let Some((id, _)) = rest.split_once(" \"")
                && !id.is_empty()
            {
                seen.insert(id.to_string());
            }
        }
    }
    seen
}

#[cfg(test)]
mod tests;
