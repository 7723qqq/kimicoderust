use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::{pending, BoxFuture};
use indexmap::IndexMap;
use kimi_core::primitives::errors::domains::core::VALIDATION_FAILED;
use kimi_core::primitives::errors::{Error2, Error2Options};
use serde_json::{Map, Value};
use tokio::sync::mpsc::{self, UnboundedSender};

use crate::_base::errors::errors::{BugIndicatingError, SharedError};
use crate::_base::utils::abort::{
    abortable, is_user_cancellation, link_abort_signal, AbortController, AbortError, AbortSignal,
};
use crate::human::llm::usage::TokenUsage;
use crate::llm_adapter::contract::errors::is_provider_rate_limit_error;
use crate::os::interface::host_clock::IHostClock;
use crate::os::interface::host_timers::{IHostTimerHandle, IHostTimers};
use crate::session::subagent::spawn::SubagentSpawnPlan;
use crate::wire::record::WireAsyncError;

const INITIAL_LAUNCH_LIMIT: usize = 5;
const INITIAL_LAUNCH_INTERVAL_MS: u64 = 700;
const RATE_LIMIT_RETRY_BASE_MS: i64 = 3000;
const RATE_LIMIT_RETRY_FACTOR: u32 = 2;
const RATE_LIMIT_CAPACITY_SHRINK_INTERVAL_MS: i64 = 2000;
const RATE_LIMIT_CAPACITY_RECOVERY_INTERVAL_MS: i64 = 3 * 60 * 1000;
const RATE_LIMIT_SUSPENDED_REASON: &str = "Provider rate limit; subagent requeued for retry.";

const AGENT_SWARM_MAX_CONCURRENCY_ENV: &str = "KIMI_CODE_AGENT_SWARM_MAX_CONCURRENCY";

const NO_DEADLINE: i64 = i64::MAX;

pub struct AgentRunAttemptOptions {
    pub parent_tool_call_id: String,
    pub parent_tool_call_uuid: Option<String>,
    pub prompt: String,
    pub description: String,
    pub swarm_index: Option<usize>,
    pub run_in_background: bool,
    pub signal: AbortSignal,
    pub on_ready: Option<Arc<dyn Fn() + Send + Sync>>,
    pub suppress_rate_limit_failure_event: bool,
}

pub struct AgentSpawnAttemptOptions {
    pub profile_name: String,
    pub swarm_item: Option<String>,
    pub plan: SubagentSpawnPlan,
    pub run: AgentRunAttemptOptions,
}

#[derive(Clone)]
pub struct AgentRunAttemptCompletion {
    pub result: String,
    pub usage: Option<TokenUsage>,
    pub stop_reason: Option<String>,
}

pub struct AgentRunAttemptHandle {
    pub agent_id: String,
    pub completion: BoxFuture<'static, Result<AgentRunAttemptCompletion, WireAsyncError>>,
}

#[derive(Clone)]
pub enum SessionSwarmTaskKind {
    Spawn { plan: SubagentSpawnPlan },
    Resume { resume_agent_id: String },
}

#[derive(Clone)]
pub struct SessionSwarmTask<T> {
    pub data: T,
    pub kind: SessionSwarmTaskKind,
    pub profile_name: String,
    pub parent_tool_call_id: String,
    pub parent_tool_call_uuid: Option<String>,
    pub prompt: String,
    pub description: String,
    pub swarm_index: Option<usize>,
    pub swarm_item: Option<String>,
    pub run_in_background: bool,
    pub timeout: Option<Duration>,
    pub signal: Option<AbortSignal>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentRunStatus {
    Completed,
    Failed,
    Aborted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentRunState {
    Started,
    NotStarted,
}

#[derive(Clone)]
pub struct SessionSwarmRunResult<T> {
    pub task: SessionSwarmTask<T>,
    pub agent_id: Option<String>,
    pub status: AgentRunStatus,
    pub state: Option<AgentRunState>,
    pub result: Option<String>,
    pub usage: Option<TokenUsage>,
    pub stop_reason: Option<String>,
    pub error: Option<String>,
}

pub type QueuedAgentRunTask<T> = SessionSwarmTask<T>;

pub type AgentRunResult<T> = SessionSwarmRunResult<T>;

pub type QueuedAgentRunResult<T> = SessionSwarmRunResult<T>;

pub struct AgentRunSuspendedEvent<T> {
    pub task: QueuedAgentRunTask<T>,
    pub agent_id: String,
    pub reason: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentRunAbandonOutcome {
    Cancelled,
    Failed,
}

pub struct AgentRunAbandonedEvent<T> {
    pub task: QueuedAgentRunTask<T>,
    pub agent_id: String,
    pub outcome: AgentRunAbandonOutcome,
    pub error: Option<String>,
}

pub trait AgentRunBatchLauncher<T>: Send + Sync {
    fn spawn(
        &self,
        options: AgentSpawnAttemptOptions,
    ) -> BoxFuture<'static, Result<AgentRunAttemptHandle, WireAsyncError>>;
    fn resume(
        &self,
        agent_id: String,
        options: AgentRunAttemptOptions,
    ) -> BoxFuture<'static, Result<AgentRunAttemptHandle, WireAsyncError>>;
    fn retry(
        &self,
        agent_id: String,
        options: AgentRunAttemptOptions,
    ) -> BoxFuture<'static, Result<AgentRunAttemptHandle, WireAsyncError>>;
    fn suspended(&self, _event: AgentRunSuspendedEvent<T>) {}
    fn abandoned(&self, _event: AgentRunAbandonedEvent<T>) {}
}

pub struct AgentRunBatchOptions {
    pub max_concurrency: Option<usize>,
    pub clock: Arc<dyn IHostClock>,
    pub timers: Arc<dyn IHostTimers>,
}

enum AttemptOutcome<T> {
    Result(AgentRunResult<T>),
    RateLimited { agent_id: String, error: String },
}

struct TaskState<T> {
    index: usize,
    task: QueuedAgentRunTask<T>,
    agent_id: Option<String>,
    retry_agent_id: Option<String>,
    retry_count: usize,
    retry_ready_at: i64,
    started: bool,
}

struct ActiveAttempt {
    generation: u64,
    ready: bool,
    cleanup: Option<Box<dyn FnOnce() + Send>>,
}

enum BatchEvent<T> {
    Ready(usize, u64),
    Spawned(usize, String),
    Done(usize, AttemptOutcome<T>),
    NormalTimer(u64),
    RateLimitTimer(u64),
}

pub struct AgentRunBatch<T> {
    launcher: Arc<dyn AgentRunBatchLauncher<T>>,
    states: Vec<TaskState<T>>,
    pending: Vec<usize>,
    results: Vec<Option<AgentRunResult<T>>>,
    active: IndexMap<usize, ActiveAttempt>,
    controller: Arc<AbortController>,
    batch_signal: Option<AbortSignal>,
    max_concurrency: Option<usize>,
    clock: Arc<dyn IHostClock>,
    timers: Arc<dyn IHostTimers>,
    tx: Option<UnboundedSender<BatchEvent<T>>>,
    normal_launch_count: usize,
    normal_timer: Option<IHostTimerHandle>,
    normal_timer_generation: u64,
    rate_limit_timer: Option<IHostTimerHandle>,
    rate_limit_timer_generation: u64,
    attempt_generation: u64,
    finished: bool,
    rate_limit_mode: bool,
    started_success_count: usize,
    rate_limit_capacity: usize,
    last_rate_limit_at: Option<i64>,
    last_capacity_shrink_at: Option<i64>,
    last_capacity_recovery_at: Option<i64>,
    global_retry_interval_ms: i64,
    next_rate_limit_launch_at: i64,
    finish_results: Option<Vec<AgentRunResult<T>>>,
    failure: Option<WireAsyncError>,
}

impl<T: Clone + Send + Sync + 'static> AgentRunBatch<T> {
    pub fn new(
        launcher: Arc<dyn AgentRunBatchLauncher<T>>,
        tasks: Vec<QueuedAgentRunTask<T>>,
        options: AgentRunBatchOptions,
    ) -> Self {
        let states = tasks
            .into_iter()
            .enumerate()
            .map(|(index, task)| TaskState {
                index,
                task,
                agent_id: None,
                retry_agent_id: None,
                retry_count: 0,
                retry_ready_at: 0,
                started: false,
            })
            .collect::<Vec<_>>();
        let batch_signal = states.iter().find_map(|state| state.task.signal.clone());
        let pending = (0..states.len()).collect();
        let results = (0..states.len()).map(|_| None).collect();
        Self {
            launcher,
            states,
            pending,
            results,
            active: IndexMap::new(),
            controller: Arc::new(AbortController::new()),
            batch_signal,
            max_concurrency: options.max_concurrency,
            clock: options.clock,
            timers: options.timers,
            tx: None,
            normal_launch_count: 0,
            normal_timer: None,
            normal_timer_generation: 0,
            rate_limit_timer: None,
            rate_limit_timer_generation: 0,
            attempt_generation: 0,
            finished: false,
            rate_limit_mode: false,
            started_success_count: 0,
            rate_limit_capacity: 1,
            last_rate_limit_at: None,
            last_capacity_shrink_at: None,
            last_capacity_recovery_at: None,
            global_retry_interval_ms: RATE_LIMIT_RETRY_BASE_MS,
            next_rate_limit_launch_at: 0,
            finish_results: None,
            failure: None,
        }
    }

    pub async fn run(mut self) -> Result<Vec<AgentRunResult<T>>, WireAsyncError> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        self.tx = Some(tx);

        if self.states.is_empty() {
            self.finish(Vec::new());
            return self.take_terminal();
        }

        if self
            .batch_signal
            .as_ref()
            .is_some_and(|signal| signal.aborted())
        {
            return self.handle_batch_abort();
        }

        let mut batch_abort: Option<BoxFuture<'static, Result<(), SharedError>>> = self
            .batch_signal
            .clone()
            .map(|signal| abortable(pending::<()>(), signal));

        self.schedule();
        if self.finished {
            return self.take_terminal();
        }

        loop {
            let event = match batch_abort.as_mut() {
                Some(abort) => {
                    tokio::select! {
                        biased;
                        _ = abort => return self.handle_batch_abort(),
                        event = rx.recv() => event,
                    }
                }
                None => rx.recv().await,
            };
            let Some(event) = event else {
                return Err(Box::new(BugIndicatingError::new(
                    "AgentRunBatch event channel closed unexpectedly",
                )));
            };
            if self
                .batch_signal
                .as_ref()
                .is_some_and(|signal| signal.aborted())
            {
                return self.handle_batch_abort();
            }
            self.handle_event(event);
            if self.finished {
                return self.take_terminal();
            }
        }
    }

    fn schedule(&mut self) {
        if self.finished {
            return;
        }
        if self.finish_if_complete() {
            return;
        }
        if self.controller.signal().aborted() {
            return;
        }
        if self.rate_limit_mode {
            self.schedule_rate_limit_launch();
        } else {
            self.schedule_normal_launch();
        }
    }

    fn schedule_normal_launch(&mut self) {
        while self.normal_launch_count < INITIAL_LAUNCH_LIMIT
            && !self.pending.is_empty()
            && !self.rate_limit_mode
            && !self.is_at_concurrency_limit()
        {
            let state_index = self.pending.remove(0);
            self.start_attempt(state_index);
            self.normal_launch_count += 1;
        }

        if self.pending.is_empty()
            || self.rate_limit_mode
            || self.normal_timer.is_some()
            || self.is_at_concurrency_limit()
        {
            return;
        }

        let generation = self.normal_timer_generation.wrapping_add(1);
        self.normal_timer_generation = generation;
        let tx = self.sender();
        self.normal_timer = Some(self.timers.set_timeout(
            Arc::new(move || {
                let _ = tx.send(BatchEvent::NormalTimer(generation));
            }),
            INITIAL_LAUNCH_INTERVAL_MS,
        ));
    }

    fn is_at_concurrency_limit(&self) -> bool {
        self.max_concurrency
            .is_some_and(|max_concurrency| self.active.len() >= max_concurrency)
    }

    fn schedule_rate_limit_launch(&mut self) {
        self.clear_rate_limit_timer();
        if self.pending.is_empty() {
            return;
        }

        let now = self.now();
        self.recover_rate_limit_capacity(now);
        if self.active.len() >= self.rate_limit_capacity {
            self.schedule_rate_limit_wakeup(self.next_rate_limit_capacity_recovery_at(), now);
            return;
        }

        let next_allowed_at = self.next_rate_limit_launch_at.max(self.next_pending_ready_at());
        let next_wakeup_at = next_allowed_at.min(self.next_rate_limit_capacity_recovery_at());
        if next_wakeup_at > now {
            self.schedule_rate_limit_wakeup(next_wakeup_at, now);
            return;
        }

        let Some(pending_index) = self
            .pending
            .iter()
            .position(|state_index| self.states[*state_index].retry_ready_at <= now)
        else {
            return;
        };

        let state_index = self.pending.remove(pending_index);
        self.start_attempt(state_index);
        self.next_rate_limit_launch_at = now + self.global_retry_interval_ms;
        self.schedule_next_rate_limit_wakeup(now);
    }

    fn start_attempt(&mut self, state_index: usize) {
        if self.finished || self.controller.signal().aborted() {
            return;
        }

        let task = self.states[state_index].task.clone();
        let retry_agent_id = self.states[state_index].retry_agent_id.clone();
        let initial_agent_id = self.states[state_index].agent_id.clone();
        let controller = Arc::new(AbortController::new());
        let timed_out = Arc::new(AtomicBool::new(false));
        let generation = self.attempt_generation.wrapping_add(1);
        self.attempt_generation = generation;
        let cleanup = self.link_attempt_signals(&controller, &task, timed_out.clone());
        self.active.insert(
            state_index,
            ActiveAttempt {
                generation,
                ready: false,
                cleanup: Some(cleanup),
            },
        );

        let launcher = self.launcher.clone();
        let tx = self.sender();
        tokio::spawn(async move {
            let outcome = run_attempt(
                launcher,
                tx.clone(),
                state_index,
                generation,
                task,
                retry_agent_id,
                initial_agent_id,
                controller,
                timed_out,
            )
            .await;
            let _ = tx.send(BatchEvent::Done(state_index, outcome));
        });
    }

    fn mark_attempt_ready(&mut self, state_index: usize, generation: u64) {
        {
            let Some(attempt) = self.active.get_mut(&state_index) else {
                return;
            };
            if self.finished || attempt.ready || attempt.generation != generation {
                return;
            }
            attempt.ready = true;
        }

        self.states[state_index].started = true;
        if !self.rate_limit_mode {
            self.started_success_count += 1;
        }

        if self.rate_limit_mode {
            self.global_retry_interval_ms = RATE_LIMIT_RETRY_BASE_MS;
            self.next_rate_limit_launch_at = self.now() + self.global_retry_interval_ms;
            self.schedule();
        }
    }

    fn handle_attempt_outcome(&mut self, state_index: usize, outcome: AttemptOutcome<T>) {
        let Some(attempt) = self.release_attempt(state_index) else {
            return;
        };
        if self.finished {
            return;
        }

        match outcome {
            AttemptOutcome::Result(result) => {
                self.results[state_index] = Some(result);
            }
            AttemptOutcome::RateLimited { agent_id, error } => {
                if self.is_only_unfinished_task(state_index) {
                    self.launcher.abandoned(AgentRunAbandonedEvent {
                        task: self.states[state_index].task.clone(),
                        agent_id: agent_id.clone(),
                        outcome: AgentRunAbandonOutcome::Failed,
                        error: Some(error.clone()),
                    });
                    self.results[state_index] = Some(AgentRunResult {
                        task: self.states[state_index].task.clone(),
                        agent_id: Some(agent_id),
                        status: AgentRunStatus::Failed,
                        state: Some(AgentRunState::Started),
                        result: None,
                        usage: None,
                        stop_reason: None,
                        error: Some(error),
                    });
                } else {
                    self.requeue_rate_limited(state_index, agent_id, attempt.ready);
                }
            }
        }
        self.schedule();
    }

    fn release_attempt(&mut self, state_index: usize) -> Option<ActiveAttempt> {
        let mut attempt = self.active.shift_remove(&state_index)?;
        if let Some(cleanup) = attempt.cleanup.take() {
            cleanup();
        }
        Some(attempt)
    }

    fn requeue_rate_limited(&mut self, state_index: usize, agent_id: String, attempt_ready: bool) {
        {
            let state = &mut self.states[state_index];
            state.agent_id = Some(agent_id.clone());
            state.retry_agent_id = Some(agent_id.clone());
            self.launcher.suspended(AgentRunSuspendedEvent {
                task: state.task.clone(),
                agent_id,
                reason: RATE_LIMIT_SUSPENDED_REASON.to_string(),
            });
        }

        let now = self.now();
        self.last_rate_limit_at = Some(now);
        self.states[state_index].retry_count += 1;
        let retry_delay = rate_limit_retry_delay(self.states[state_index].retry_count);
        self.states[state_index].retry_ready_at = now + retry_delay;
        self.pending.insert(0, state_index);
        self.enter_rate_limit_mode(now);

        if !attempt_ready {
            self.global_retry_interval_ms = self
                .global_retry_interval_ms
                .saturating_mul(i64::from(RATE_LIMIT_RETRY_FACTOR))
                .max(retry_delay);
            self.next_rate_limit_launch_at = self
                .next_rate_limit_launch_at
                .max(now + self.global_retry_interval_ms);
        } else {
            self.next_rate_limit_launch_at = self
                .next_rate_limit_launch_at
                .max(now + RATE_LIMIT_RETRY_BASE_MS);
        }
    }

    fn enter_rate_limit_mode(&mut self, now: i64) {
        if !self.rate_limit_mode {
            self.rate_limit_mode = true;
            self.clear_normal_timer();
            self.rate_limit_capacity = self.started_success_count.max(1);
            self.next_rate_limit_launch_at = self
                .next_rate_limit_launch_at
                .max(now + RATE_LIMIT_RETRY_BASE_MS);
            self.shrink_rate_limit_capacity(now, true);
            return;
        }

        self.shrink_rate_limit_capacity(now, false);
    }

    fn shrink_rate_limit_capacity(&mut self, now: i64, force: bool) {
        if !force
            && self
                .last_capacity_shrink_at
                .is_some_and(|at| now - at < RATE_LIMIT_CAPACITY_SHRINK_INTERVAL_MS)
        {
            return;
        }

        self.rate_limit_capacity = self.rate_limit_capacity.saturating_sub(1).max(1);
        self.last_capacity_shrink_at = Some(now);
    }

    fn recover_rate_limit_capacity(&mut self, now: i64) {
        let next_recovery_at = self.next_rate_limit_capacity_recovery_at();
        if next_recovery_at > now {
            return;
        }

        self.rate_limit_capacity += 1;
        self.last_capacity_recovery_at = Some(now);
        self.next_rate_limit_launch_at = self.next_rate_limit_launch_at.min(now);
    }

    fn next_rate_limit_capacity_recovery_at(&self) -> i64 {
        if self.pending.is_empty() || self.last_rate_limit_at.is_none() {
            return NO_DEADLINE;
        }

        let latest_capacity_change_at = self
            .last_rate_limit_at
            .unwrap_or(NO_DEADLINE)
            .max(self.last_capacity_recovery_at.unwrap_or(0));
        latest_capacity_change_at + RATE_LIMIT_CAPACITY_RECOVERY_INTERVAL_MS
    }

    fn schedule_rate_limit_wakeup(&mut self, wakeup_at: i64, now: i64) {
        if wakeup_at == NO_DEADLINE || wakeup_at <= now {
            return;
        }
        let generation = self.rate_limit_timer_generation.wrapping_add(1);
        self.rate_limit_timer_generation = generation;
        let tx = self.sender();
        self.rate_limit_timer = Some(self.timers.set_timeout(
            Arc::new(move || {
                let _ = tx.send(BatchEvent::RateLimitTimer(generation));
            }),
            (wakeup_at - now) as u64,
        ));
    }

    fn schedule_next_rate_limit_wakeup(&mut self, now: i64) {
        if self.pending.is_empty() {
            return;
        }

        let next_wakeup_at = if self.active.len() >= self.rate_limit_capacity {
            self.next_rate_limit_capacity_recovery_at()
        } else {
            self.next_rate_limit_launch_at
                .max(self.next_pending_ready_at())
                .min(self.next_rate_limit_capacity_recovery_at())
        };

        self.schedule_rate_limit_wakeup(next_wakeup_at, now);
    }

    fn next_pending_ready_at(&self) -> i64 {
        self.pending
            .iter()
            .map(|state_index| self.states[*state_index].retry_ready_at)
            .min()
            .unwrap_or(NO_DEADLINE)
    }

    fn finish_if_complete(&mut self) -> bool {
        if self.results.iter().all(Option::is_some) {
            let results = self
                .results
                .iter()
                .map(|result| {
                    result
                        .clone()
                        .expect("complete batch carries every task result")
                })
                .collect();
            self.finish(results);
            true
        } else {
            false
        }
    }

    fn is_only_unfinished_task(&self, state_index: usize) -> bool {
        self.results
            .iter()
            .enumerate()
            .all(|(index, result)| index == state_index || result.is_some())
    }

    fn finish_with_user_cancellation(&mut self) -> Result<Vec<AgentRunResult<T>>, WireAsyncError> {
        if self.finished {
            return self.take_terminal();
        }
        self.abandon_suspended();

        let results = self
            .states
            .iter()
            .map(|state| {
                if let Some(result) = &self.results[state.index] {
                    return result.clone();
                }

                if state.started || state.agent_id.is_some() {
                    return AgentRunResult {
                        task: state.task.clone(),
                        agent_id: state.agent_id.clone(),
                        status: AgentRunStatus::Aborted,
                        state: Some(AgentRunState::Started),
                        result: None,
                        usage: None,
                        stop_reason: None,
                        error: Some(
                            "The user manually interrupted this subagent batch before this subagent finished."
                                .to_string(),
                        ),
                    };
                }

                AgentRunResult {
                    task: state.task.clone(),
                    agent_id: None,
                    status: AgentRunStatus::Aborted,
                    state: Some(AgentRunState::NotStarted),
                    result: None,
                    usage: None,
                    stop_reason: None,
                    error: Some(
                        "The user manually interrupted this subagent batch before this subagent was started."
                            .to_string(),
                    ),
                }
            })
            .collect();
        self.finish(results);
        self.take_terminal()
    }

    fn finish(&mut self, results: Vec<AgentRunResult<T>>) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.cleanup();
        self.finish_results = Some(results);
    }

    fn fail(&mut self, error: WireAsyncError) {
        if self.finished {
            return;
        }
        self.abandon_suspended();
        self.finished = true;
        self.cleanup();
        self.failure = Some(error);
    }

    fn abandon_suspended(&mut self) {
        let pending = self.pending.clone();
        for state_index in pending {
            let Some(agent_id) = self.states[state_index].agent_id.clone() else {
                continue;
            };
            self.launcher.abandoned(AgentRunAbandonedEvent {
                task: self.states[state_index].task.clone(),
                agent_id,
                outcome: AgentRunAbandonOutcome::Cancelled,
                error: None,
            });
        }

        let active = self.active.keys().copied().collect::<Vec<_>>();
        for state_index in active {
            let Some(attempt) = self.active.get(&state_index) else {
                continue;
            };
            if attempt.ready {
                continue;
            }
            let Some(agent_id) = self.states[state_index].agent_id.clone() else {
                continue;
            };
            self.launcher.abandoned(AgentRunAbandonedEvent {
                task: self.states[state_index].task.clone(),
                agent_id,
                outcome: AgentRunAbandonOutcome::Cancelled,
                error: None,
            });
        }
    }

    fn cleanup(&mut self) {
        self.clear_normal_timer();
        self.clear_rate_limit_timer();
        let active = std::mem::take(&mut self.active);
        for (_, mut attempt) in active {
            if let Some(cleanup) = attempt.cleanup.take() {
                cleanup();
            }
        }
    }

    fn clear_normal_timer(&mut self) {
        if let Some(handle) = self.normal_timer.take() {
            self.timers.clear_timeout(handle);
        }
        self.normal_timer_generation = self.normal_timer_generation.wrapping_add(1);
    }

    fn clear_rate_limit_timer(&mut self) {
        if let Some(handle) = self.rate_limit_timer.take() {
            self.timers.clear_timeout(handle);
        }
        self.rate_limit_timer_generation = self.rate_limit_timer_generation.wrapping_add(1);
    }

    fn link_attempt_signals(
        &self,
        attempt: &Arc<AbortController>,
        task: &QueuedAgentRunTask<T>,
        timed_out: Arc<AtomicBool>,
    ) -> Box<dyn FnOnce() + Send> {
        let batch_signal = self.controller.signal();
        let unlink_batch = link_abort_signal(&batch_signal, attempt.clone());
        let unlink_task = task
            .signal
            .as_ref()
            .map(|signal| link_abort_signal(signal, attempt.clone()));
        let timeout = match task.timeout {
            Some(timeout) if timeout > Duration::ZERO => {
                let controller = attempt.clone();
                Some(self.timers.set_clamped_timeout(
                    Arc::new(move || {
                        timed_out.store(true, Ordering::SeqCst);
                        controller.abort(Some(Arc::new(AbortError::new("Subagent timed out."))));
                    }),
                    timeout.as_millis() as u64,
                ))
            }
            _ => None,
        };
        let timers = self.timers.clone();

        Box::new(move || {
            if let Some(handle) = timeout {
                timers.clear_timeout(handle);
            }
            unlink_batch();
            if let Some(unlink) = unlink_task {
                unlink();
            }
        })
    }

    fn handle_event(&mut self, event: BatchEvent<T>) {
        match event {
            BatchEvent::Ready(state_index, generation) => {
                self.mark_attempt_ready(state_index, generation);
            }
            BatchEvent::Spawned(state_index, agent_id) => {
                if let Some(state) = self.states.get_mut(state_index) {
                    state.agent_id = Some(agent_id);
                }
            }
            BatchEvent::Done(state_index, outcome) => {
                self.handle_attempt_outcome(state_index, outcome);
            }
            BatchEvent::NormalTimer(generation) => self.handle_normal_timer(generation),
            BatchEvent::RateLimitTimer(generation) => self.handle_rate_limit_timer(generation),
        }
    }

    fn handle_normal_timer(&mut self, generation: u64) {
        if generation != self.normal_timer_generation {
            return;
        }
        self.normal_timer = None;
        if self.finished || self.rate_limit_mode || self.pending.is_empty() {
            return;
        }
        if self.is_at_concurrency_limit() {
            return;
        }
        let state_index = self.pending.remove(0);
        self.start_attempt(state_index);
        self.normal_launch_count += 1;
        self.schedule();
    }

    fn handle_rate_limit_timer(&mut self, generation: u64) {
        if generation != self.rate_limit_timer_generation {
            return;
        }
        self.rate_limit_timer = None;
        self.schedule();
    }

    fn handle_batch_abort(&mut self) -> Result<Vec<AgentRunResult<T>>, WireAsyncError> {
        let reason = self.batch_signal.as_ref().and_then(|signal| signal.reason());
        self.controller.abort(reason.clone());
        if reason
            .as_deref()
            .is_some_and(|reason| is_user_cancellation(reason))
        {
            return self.finish_with_user_cancellation();
        }
        let error: WireAsyncError = match reason {
            Some(reason) => Box::new(SharedReasonError(reason)),
            None => Box::new(AbortError::new("Aborted")),
        };
        self.fail(error);
        self.take_terminal()
    }

    fn take_terminal(&mut self) -> Result<Vec<AgentRunResult<T>>, WireAsyncError> {
        if let Some(results) = self.finish_results.take() {
            return Ok(results);
        }
        if let Some(error) = self.failure.take() {
            return Err(error);
        }
        Err(Box::new(BugIndicatingError::new(
            "AgentRunBatch finished without a result",
        )))
    }

    fn sender(&self) -> UnboundedSender<BatchEvent<T>> {
        self.tx
            .clone()
            .expect("AgentRunBatch::run() must be called before attempts start")
    }

    fn now(&self) -> i64 {
        self.clock.now().timestamp_millis()
    }
}

async fn run_attempt<T: Clone + Send + Sync + 'static>(
    launcher: Arc<dyn AgentRunBatchLauncher<T>>,
    tx: UnboundedSender<BatchEvent<T>>,
    state_index: usize,
    generation: u64,
    task: QueuedAgentRunTask<T>,
    retry_agent_id: Option<String>,
    initial_agent_id: Option<String>,
    controller: Arc<AbortController>,
    timed_out: Arc<AtomicBool>,
) -> AttemptOutcome<T> {
    let signal = controller.signal();
    let ready_tx = tx.clone();
    let on_ready: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        let _ = ready_tx.send(BatchEvent::Ready(state_index, generation));
    });
    let run_options = AgentRunAttemptOptions {
        parent_tool_call_id: task.parent_tool_call_id.clone(),
        parent_tool_call_uuid: task.parent_tool_call_uuid.clone(),
        prompt: task.prompt.clone(),
        description: task.description.clone(),
        swarm_index: task.swarm_index,
        run_in_background: task.run_in_background,
        signal: signal.clone(),
        on_ready: Some(on_ready),
        suppress_rate_limit_failure_event: true,
    };

    if signal.aborted() {
        return failed_outcome(
            &task,
            initial_agent_id.clone(),
            &signal,
            &timed_out,
            abort_reason_message(&signal),
        );
    }

    let handle = match retry_agent_id {
        Some(agent_id) => launcher.retry(agent_id, run_options).await,
        None => match &task.kind {
            SessionSwarmTaskKind::Resume { resume_agent_id } => {
                launcher.resume(resume_agent_id.clone(), run_options).await
            }
            SessionSwarmTaskKind::Spawn { plan } => {
                launcher
                    .spawn(AgentSpawnAttemptOptions {
                        profile_name: task.profile_name.clone(),
                        swarm_item: task.swarm_item.clone(),
                        plan: plan.clone(),
                        run: run_options,
                    })
                    .await
            }
        },
    };

    let handle = match handle {
        Ok(handle) => handle,
        Err(error) => {
            return failed_outcome(
                &task,
                initial_agent_id.clone(),
                &signal,
                &timed_out,
                error.to_string(),
            );
        }
    };

    let agent_id = Some(handle.agent_id.clone());
    let _ = tx.send(BatchEvent::Spawned(state_index, handle.agent_id.clone()));

    match handle.completion.await {
        Ok(completion) => AttemptOutcome::Result(AgentRunResult {
            task,
            agent_id,
            status: AgentRunStatus::Completed,
            state: None,
            result: Some(completion.result),
            usage: completion.usage,
            stop_reason: completion.stop_reason,
            error: None,
        }),
        Err(error) => {
            if is_provider_rate_limit_error(error.as_ref()) {
                AttemptOutcome::RateLimited {
                    agent_id: handle.agent_id,
                    error: attempt_error_message(
                        &timed_out,
                        &task,
                        AgentRunStatus::Failed,
                        &error.to_string(),
                    ),
                }
            } else {
                failed_outcome(&task, agent_id, &signal, &timed_out, error.to_string())
            }
        }
    }
}

fn failed_outcome<T: Clone>(
    task: &QueuedAgentRunTask<T>,
    agent_id: Option<String>,
    signal: &AbortSignal,
    timed_out: &AtomicBool,
    error_message: String,
) -> AttemptOutcome<T> {
    let status = if signal.aborted() && is_user_cancel_reason(signal) {
        AgentRunStatus::Aborted
    } else {
        AgentRunStatus::Failed
    };
    let state = if agent_id.is_some() {
        AgentRunState::Started
    } else {
        AgentRunState::NotStarted
    };
    AttemptOutcome::Result(AgentRunResult {
        task: task.clone(),
        agent_id,
        status,
        state: Some(state),
        result: None,
        usage: None,
        stop_reason: None,
        error: Some(attempt_error_message(
            timed_out,
            task,
            status,
            &error_message,
        )),
    })
}

fn attempt_error_message<T>(
    timed_out: &AtomicBool,
    task: &QueuedAgentRunTask<T>,
    status: AgentRunStatus,
    error_message: &str,
) -> String {
    if timed_out.load(Ordering::SeqCst) && task.timeout.is_some() {
        return "Subagent timed out.".to_string();
    }
    if status == AgentRunStatus::Aborted {
        return "The user manually interrupted this subagent batch.".to_string();
    }
    error_message.to_string()
}

fn rate_limit_retry_delay(retry_count: usize) -> i64 {
    let exponent = retry_count.saturating_sub(1).min(62) as u32;
    let multiplier = u64::from(RATE_LIMIT_RETRY_FACTOR).saturating_pow(exponent);
    RATE_LIMIT_RETRY_BASE_MS.saturating_mul(i64::try_from(multiplier).unwrap_or(i64::MAX))
}

fn is_user_cancel_reason(signal: &AbortSignal) -> bool {
    signal
        .reason()
        .as_deref()
        .is_some_and(|reason| is_user_cancellation(reason))
}

fn abort_reason_message(signal: &AbortSignal) -> String {
    match signal.reason() {
        Some(reason) => reason.to_string(),
        None => "Aborted".to_string(),
    }
}

#[derive(Clone)]
struct SharedReasonError(SharedError);

impl fmt::Display for SharedReasonError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, formatter)
    }
}

impl fmt::Debug for SharedReasonError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0.to_string())
    }
}

impl std::error::Error for SharedReasonError {}

pub fn resolve_swarm_max_concurrency() -> Result<Option<usize>, WireAsyncError> {
    let env = std::env::vars().collect::<HashMap<_, _>>();
    resolve_swarm_max_concurrency_from(&env)
}

pub fn resolve_swarm_max_concurrency_from(
    env: &HashMap<String, String>,
) -> Result<Option<usize>, WireAsyncError> {
    let Some(raw) = env.get(AGENT_SWARM_MAX_CONCURRENCY_ENV) else {
        return Ok(None);
    };
    if raw.trim().is_empty() {
        return Ok(None);
    }

    if let Some(value) = parse_js_number(raw)
        .filter(|value| value.is_finite() && value.fract() == 0.0 && *value > 0.0)
    {
        if let Ok(value) = usize::try_from(value as u128) {
            return Ok(Some(value));
        }
    }

    Err(coded_error(
        VALIDATION_FAILED,
        format!(
            "{AGENT_SWARM_MAX_CONCURRENCY_ENV} must be a positive integer, got {}.",
            Value::from(raw.as_str())
        ),
        [("value", Value::from(raw.as_str()))],
    ))
}

fn parse_js_number(raw: &str) -> Option<f64> {
    let trimmed = raw.trim();
    let (radix, digits) = if let Some(digits) =
        trimmed.strip_prefix("0x").or_else(|| trimmed.strip_prefix("0X"))
    {
        (16, digits)
    } else if let Some(digits) = trimmed
        .strip_prefix("0b")
        .or_else(|| trimmed.strip_prefix("0B"))
    {
        (2, digits)
    } else if let Some(digits) = trimmed
        .strip_prefix("0o")
        .or_else(|| trimmed.strip_prefix("0O"))
    {
        (8, digits)
    } else {
        return trimmed.parse::<f64>().ok();
    };
    u128::from_str_radix(digits, radix)
        .ok()
        .map(|value| value as f64)
}

fn coded_error<const N: usize>(
    code: &'static str,
    message: String,
    details: [(&'static str, Value); N],
) -> WireAsyncError {
    Box::new(Error2::with_options(
        code,
        message,
        Error2Options {
            details: Some(Map::from_iter(
                details
                    .into_iter()
                    .map(|(name, value)| (name.to_string(), value)),
            )),
            ..Default::default()
        },
    ))
}
