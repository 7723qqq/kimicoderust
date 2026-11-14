use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::_base::utils::timer::IntervalTimer;
use crate::agent::actor_service::agent_actor_runtime::AgentActorHost;
use crate::agent::actor_service::agent_actor_service::{
    AgentActorContext, AgentActorDurable, AgentActorOptions, AgentActorService, StateFold,
};
use crate::agent::actor_service::services::{AgentLoopService, ConfigService, TelemetryService};
use crate::agent::agent_context::AgentContext;
use crate::agent::context_memory::types::{ContextMessage, CronJobOrigin, CronMissedOrigin};
use crate::callbacks::MAIN_AGENT_ID;
use crate::features::cron::config_section::{
    CRON_SECTION, CronConfig, DEFAULT_CRON_CONFIG, PollIntervalMs,
};
use crate::features::cron::cron_ops::{
    CronModelState, cron_add_event, cron_cursor_event, cron_delete_event, cron_fired_event,
};
use crate::features::cron::cron_task::{CRON_ID_REGEX, CronTask, CronTaskInit};
use crate::features::cron::internal::clock::{ClockSources, SYSTEM_CLOCKS, resolve_clock_sources};
use crate::features::cron::internal::cron_expr::{
    ParsedCronExpression, compute_next_cron_run, parse_cron_expression,
};
use crate::features::cron::internal::format::render_cron_fire_xml;
use crate::features::cron::internal::jitter::{
    jittered_next_cron_run_ms, one_shot_jittered_next_cron_run_ms,
};
use crate::features::reminder::reminder_service::ReminderService;
use crate::features::reminder::types::ReminderNotification;
use crate::human::xstate2::{
    Action, Context, FromCallback, InvokeDef, InvokeInput, Machine, StateNode, Transition, XEvent,
    from_callback, send_to,
};

const STALE_THRESHOLD_MS: i64 = 7 * 24 * 60 * 60 * 1000;
const DEFAULT_POLL_INTERVAL_MS: i64 = 1_000;
const MAX_COALESCE_ITERATIONS: usize = 10_000;
const MAX_ID_ATTEMPTS: usize = 8;

pub const CRON_SCHEDULED: &str = "cron_scheduled";
pub const CRON_FIRED: &str = "cron_fired";
pub const CRON_MISSED: &str = "cron_missed";
pub const CRON_DELETED: &str = "cron_deleted";
pub const AGENT_CRON_SERVICE_ID: &str = "agentCronService";

const CRON_FORK_CLEARED_REMINDER: &str = "This fork does not have any scheduled cron tasks. Tasks from the source session continue to run in the source session. Create new tasks here if needed.";
const CRON_FORK_CLEARED_REMINDER_NAME: &str = "cron_fork_cleared";

#[derive(Clone)]
pub struct CronActorContext {
    pub model: CronModelState,
    pub runtime: Context,
}

struct CronEffectState {
    clocks: ClockSources,
    parsed_cache: HashMap<String, Arc<ParsedCronExpression>>,
    last_seen_at: HashMap<String, i64>,
    seeded_from_store: HashSet<String>,
    in_flight: HashSet<String>,
}

impl Default for CronEffectState {
    fn default() -> Self {
        Self {
            clocks: SYSTEM_CLOCKS.clone(),
            parsed_cache: HashMap::new(),
            last_seen_at: HashMap::new(),
            seeded_from_store: HashSet::new(),
            in_flight: HashSet::new(),
        }
    }
}

struct TickCompletion {
    sender: Mutex<Option<tokio::sync::oneshot::Sender<Result<(), String>>>>,
}

fn config_of(runtime: &AgentActorContext<CronModelState>) -> Arc<dyn ConfigService> {
    runtime.service("configService")
}

fn cron_config_of(runtime: &AgentActorContext<CronModelState>) -> CronConfig {
    config_of(runtime)
        .get(CRON_SECTION)
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or(DEFAULT_CRON_CONFIG)
}

fn clocks_of(runtime: &AgentActorContext<CronModelState>) -> ClockSources {
    let config = cron_config_of(runtime);
    resolve_clock_sources(config.clock.as_deref(), config.debug)
}

fn telemetry_of(runtime: &AgentActorContext<CronModelState>) -> Arc<dyn TelemetryService> {
    runtime.service("telemetryService")
}

fn debug_log(runtime: &AgentActorContext<CronModelState>, message: &str) {
    if cron_config_of(runtime).debug {
        eprintln!("[cron/session] {message}");
    }
}

fn is_stale_at(runtime: &AgentActorContext<CronModelState>, task: &CronTask, now: i64) -> bool {
    if cron_config_of(runtime).no_stale || task.recurring == Some(false) {
        return false;
    }
    now - task.created_at >= STALE_THRESHOLD_MS
}

fn compute_jittered_next(
    runtime: &AgentActorContext<CronModelState>,
    task: &CronTask,
    parsed: &ParsedCronExpression,
    base_ms: i64,
) -> Option<i64> {
    let ideal = compute_next_cron_run(parsed, base_ms)?;
    let no_jitter = cron_config_of(runtime).no_jitter;
    Some(if task.recurring == Some(false) {
        one_shot_jittered_next_cron_run_ms(task, ideal, None, no_jitter)
    } else {
        jittered_next_cron_run_ms(task, parsed, ideal, None, no_jitter)
    })
}

fn parsed_cron(
    state: &Arc<Mutex<CronEffectState>>,
    expression: &str,
) -> Result<Arc<ParsedCronExpression>, kimi_core::primitives::errors::Error2> {
    if let Some(parsed) = state.lock().parsed_cache.get(expression).cloned() {
        return Ok(parsed);
    }
    let parsed = Arc::new(parse_cron_expression(expression)?);
    state
        .lock()
        .parsed_cache
        .insert(expression.to_string(), parsed.clone());
    Ok(parsed)
}

fn count_coalesced(
    runtime: &AgentActorContext<CronModelState>,
    task: &CronTask,
    parsed: &ParsedCronExpression,
    first_fire_ms: i64,
    now_ms: i64,
) -> (usize, i64) {
    let mut count = 1;
    let mut cursor = first_fire_ms;
    let mut last_due_ms = first_fire_ms;
    let no_jitter = cron_config_of(runtime).no_jitter;
    while count < MAX_COALESCE_ITERATIONS {
        let Some(next) = compute_next_cron_run(parsed, cursor) else {
            break;
        };
        if next > now_ms {
            break;
        }
        let jittered_next = if task.recurring == Some(false) {
            one_shot_jittered_next_cron_run_ms(task, next, None, no_jitter)
        } else {
            jittered_next_cron_run_ms(task, parsed, next, None, no_jitter)
        };
        if jittered_next > now_ms {
            break;
        }
        count += 1;
        cursor = next;
        last_due_ms = next;
    }
    (count, last_due_ms)
}

fn remove_tasks(runtime: &AgentActorContext<CronModelState>, ids: &[String]) -> Vec<String> {
    let tasks = (runtime.get_state)().tasks;
    let removed: Vec<String> = ids
        .iter()
        .filter(|id| tasks.contains_key(*id))
        .cloned()
        .collect();
    if !removed.is_empty() {
        let dispatch = runtime.dispatch.clone();
        let event = cron_delete_event(&removed);
        tokio::spawn(async move {
            let _ = dispatch(event).await;
        });
    }
    removed
}

async fn deliver_fire(
    runtime: &AgentActorContext<CronModelState>,
    task: &CronTask,
    coalesced_count: usize,
    fired_at: i64,
) -> bool {
    let origin = CronJobOrigin {
        kind: "cron_job".to_string(),
        job_id: task.id.clone(),
        cron: task.cron.clone(),
        recurring: task.recurring != Some(false),
        coalesced_count: coalesced_count as u32,
        stale: is_stale_at(runtime, task, fired_at),
    };
    let loop_service = runtime.service::<dyn AgentLoopService>("loopService");
    let buffered = loop_service.snapshot().get("state").and_then(Value::as_str) == Some("running");
    let submitted = loop_service.submit(
        json!({
            "message": {
                "role": "user",
                "content": [{"type": "text", "text": render_cron_fire_xml(&origin, &task.prompt)}],
            },
            "meta": {"origin": origin},
        }),
        Some(json!({"steerIfActive": true})),
    );
    if let Err(error) = submitted {
        debug_log(
            runtime,
            &format!("steer threw for task {}: {error}", task.id),
        );
        return false;
    }
    let dispatch = runtime.dispatch.clone();
    let event = cron_fired_event(&origin, &task.prompt);
    tokio::spawn(async move {
        let _ = dispatch(event).await;
    });
    telemetry_of(runtime).track2(
        CRON_FIRED,
        json!({
            "recurring": task.recurring != Some(false),
            "coalesced_count": coalesced_count,
            "stale": origin.stale,
            "buffered": buffered,
        }),
    );
    true
}

async fn process_due(
    runtime: &AgentActorContext<CronModelState>,
    state: &Arc<Mutex<CronEffectState>>,
    task: &CronTask,
    now: i64,
) {
    if state.lock().in_flight.contains(&task.id) {
        return;
    }
    let parsed = match parsed_cron(state, &task.cron) {
        Ok(parsed) => parsed,
        Err(error) => {
            debug_log(
                runtime,
                &format!("tick failed to parse cron for task {}: {error}", task.id),
            );
            return;
        }
    };
    {
        let mut state = state.lock();
        if !state.seeded_from_store.contains(&task.id)
            && task.last_fired_at.is_some_and(|last| last <= now)
            && !state.last_seen_at.contains_key(&task.id)
            && let Some(last_fired_at) = task.last_fired_at
        {
            state.last_seen_at.insert(task.id.clone(), last_fired_at);
        }
        state.seeded_from_store.insert(task.id.clone());
    }
    let seen = state.lock().last_seen_at.get(&task.id).copied();
    let base_from_ms = seen
        .filter(|seen| *seen > task.created_at)
        .unwrap_or(task.created_at);
    let Some(next_fire_at) = compute_jittered_next(runtime, task, &parsed, base_from_ms) else {
        return;
    };
    if now < next_fire_at {
        return;
    }
    let ideal = compute_next_cron_run(&parsed, base_from_ms);
    let (coalesced_count, last_due_ms) = if task.recurring != Some(false) && ideal.is_some() {
        let (count, last_due) = count_coalesced(runtime, task, &parsed, ideal.unwrap(), now);
        (count.max(1), Some(last_due))
    } else {
        (1, None)
    };
    state.lock().in_flight.insert(task.id.clone());
    let fired_at = state.lock().clocks.wall_now();
    let delivered = deliver_fire(runtime, task, coalesced_count, fired_at).await;
    state.lock().in_flight.remove(&task.id);
    if !delivered {
        return;
    }
    if task.recurring == Some(false) || is_stale_at(runtime, task, fired_at) {
        let removed = remove_tasks(runtime, &[task.id.clone()]);
        let mut state = state.lock();
        state.last_seen_at.remove(&task.id);
        state.seeded_from_store.remove(&task.id);
        drop(state);
        if task.recurring != Some(false) && !removed.is_empty() {
            telemetry_of(runtime).track2(
                CRON_DELETED,
                json!({"task_id": task.id, "agent_id": Option::<String>::None}),
            );
        }
        return;
    }
    let advanced_to = last_due_ms.unwrap_or(now);
    state
        .lock()
        .last_seen_at
        .insert(task.id.clone(), advanced_to);
    if (runtime.get_state)().tasks.contains_key(&task.id) {
        let dispatch = runtime.dispatch.clone();
        let event = cron_cursor_event(&task.id, advanced_to);
        tokio::spawn(async move {
            let _ = dispatch(event).await;
        });
    }
}

async fn tick_cron(
    runtime: &AgentActorContext<CronModelState>,
    state: &Arc<Mutex<CronEffectState>>,
) -> Result<(), String> {
    if config_of(runtime).ready().await.is_err() {
        return Ok(());
    }
    let tasks: Vec<CronTask> = (runtime.get_state)().tasks.values().cloned().collect();
    if cron_config_of(runtime).disabled || tasks.is_empty() {
        return Ok(());
    }
    if runtime
        .service::<dyn AgentLoopService>("loopService")
        .snapshot()
        .get("state")
        .and_then(Value::as_str)
        == Some("running")
    {
        return Ok(());
    }
    let now = state.lock().clocks.wall_now();
    futures_util::future::join_all(tasks.into_iter().map(|task| {
        let state = state.clone();
        async move { process_due(runtime, &state, &task, now).await }
    }))
    .await;
    Ok(())
}

fn cron_effects() -> FromCallback {
    from_callback(|input| {
        let runtime = input
            .runtime
            .downcast_ref::<AgentActorContext<CronModelState>>()
            .unwrap()
            .clone();
        if runtime.agent.agent_id != MAIN_AGENT_ID {
            return Box::new(|| {});
        }
        if (runtime.get_state)().fork_notice.reminder_pending {
            runtime
                .service::<dyn ReminderService>("reminderService")
                .notify(
                    CRON_FORK_CLEARED_REMINDER,
                    &ReminderNotification {
                        variant: CRON_FORK_CLEARED_REMINDER_NAME.to_string(),
                        owner_prompt_id: None,
                    },
                );
        }
        let timer = IntervalTimer::new(true);
        let state = Arc::new(Mutex::new(CronEffectState::default()));
        let disposed = Arc::new(Mutex::new(false));
        let signal_task = Arc::new(Mutex::new(None::<tokio::task::JoinHandle<()>>));
        (input.receive)({
            let runtime = runtime.clone();
            let state = state.clone();
            Arc::new(move |event: XEvent| {
                let runtime = runtime.clone();
                let state = state.clone();
                tokio::spawn(async move {
                    let outcome = tick_cron(&runtime, &state).await;
                    if let Some(completion) = event
                        .data
                        .and_then(|data| data.downcast::<TickCompletion>().ok())
                    {
                        if let Some(sender) = completion.sender.lock().take() {
                            let _ = sender.send(outcome);
                        }
                    }
                });
            })
        });
        if let Some(restore) = &input.restore {
            let disposed = disposed.clone();
            let timer = timer.clone();
            let state = state.clone();
            let signal_task = signal_task.clone();
            let send_back = input.send_back.clone();
            restore.wait_until(Box::pin(async move {
                if config_of(&runtime).ready().await.is_err() || *disposed.lock() {
                    return;
                }
                let config = cron_config_of(&runtime);
                state.lock().clocks = resolve_clock_sources(config.clock.as_deref(), config.debug);
                let interval = if config.manual_tick {
                    None
                } else {
                    match config.poll_interval_ms {
                        PollIntervalMs::Unspecified => Some(DEFAULT_POLL_INTERVAL_MS),
                        PollIntervalMs::Null => None,
                        PollIntervalMs::Value(value) => Some(value),
                    }
                };
                if let Some(interval) = interval.filter(|interval| *interval != 0) {
                    let send_back = send_back.clone();
                    timer.cancel_and_set(
                        move || {
                            (send_back)(XEvent::new("cron.tick", Value::Null));
                        },
                        interval,
                    );
                }
                #[cfg(unix)]
                if config.manual_tick {
                    let send_back = send_back.clone();
                    let task = tokio::spawn(async move {
                        if let Ok(mut signal) = tokio::signal::unix::signal(
                            tokio::signal::unix::SignalKind::user_defined1(),
                        ) {
                            while signal.recv().await.is_some() {
                                (send_back)(XEvent::new("cron.tick", Value::Null));
                            }
                        }
                    });
                    *signal_task.lock() = Some(task);
                }
            }));
        }
        Box::new(move || {
            *disposed.lock() = true;
            timer.dispose();
            if let Some(task) = signal_task.lock().take() {
                task.abort();
            }
            let mut state = state.lock();
            state.in_flight.clear();
            state.last_seen_at.clear();
            state.seeded_from_store.clear();
            state.parsed_cache.clear();
        })
    })
}

pub fn next_fire_for(runtime: &AgentActorContext<CronModelState>, task: &CronTask) -> Option<i64> {
    let clocks = clocks_of(runtime);
    let parsed = match parse_cron_expression(&task.cron) {
        Ok(parsed) => parsed,
        Err(error) => {
            debug_log(
                runtime,
                &format!("nextFireFor skipping task {}: {error}", task.id),
            );
            return None;
        }
    };
    let persisted_cursor = task.last_fired_at.filter(|last| *last <= clocks.wall_now());
    let base_from_ms = match persisted_cursor {
        Some(cursor) if cursor > task.created_at => cursor,
        _ => task.created_at,
    };
    compute_jittered_next(runtime, task, &parsed, base_from_ms)
}

pub fn cron_actor_logic() -> Machine {
    Machine {
        context: Arc::new(|input| {
            Arc::new(Mutex::new(CronActorContext {
                model: CronModelState::default(),
                runtime: input,
            }))
        }),
        initial: "beforeRestore".to_string(),
        states: std::collections::HashMap::from([
            (
                "beforeRestore".to_string(),
                StateNode {
                    on: std::collections::HashMap::from([
                        (
                            "runtime.restore".to_string(),
                            Transition {
                                target: Some("active".to_string()),
                                actions: Vec::new(),
                            },
                        ),
                        (
                            "cron.tick".to_string(),
                            Transition {
                                target: None,
                                actions: vec![Action::Custom(Arc::new(|_, event| {
                                    if let Some(completion) = event
                                        .data
                                        .as_ref()
                                        .and_then(|data| data.downcast_ref::<TickCompletion>())
                                        && let Some(sender) = completion.sender.lock().take()
                                    {
                                        let _ = sender
                                            .send(Err("Cron runtime is not restored".to_string()));
                                    }
                                }))],
                            },
                        ),
                    ]),
                    invoke: None,
                },
            ),
            (
                "active".to_string(),
                StateNode {
                    on: std::collections::HashMap::from([(
                        "cron.tick".to_string(),
                        Transition {
                            target: None,
                            actions: vec![send_to("cronEffects", |event| event.clone())],
                        },
                    )]),
                    invoke: Some(InvokeDef {
                        id: "cronEffects".to_string(),
                        src: "cronEffects".to_string(),
                        input: Arc::new(|context, event| {
                            let context = context
                                .downcast_ref::<Mutex<CronActorContext>>()
                                .unwrap()
                                .lock();
                            InvokeInput {
                                runtime: context.runtime.clone(),
                                restore: event.restore.clone(),
                            }
                        }),
                    }),
                },
            ),
        ]),
        on: std::collections::HashMap::from([(
            "cron.commit".to_string(),
            Transition {
                target: None,
                actions: vec![Action::Custom(Arc::new(|context, event| {
                    let context = context.downcast_ref::<Mutex<CronActorContext>>().unwrap();
                    context.lock().model = event
                        .payload
                        .get("model")
                        .cloned()
                        .and_then(|model| serde_json::from_value(model).ok())
                        .unwrap_or_default();
                }))],
            },
        )]),
        actors: std::collections::HashMap::from([("cronEffects".to_string(), cron_effects())]),
    }
}

pub struct AgentCronService {
    base: AgentActorService,
    actor: AgentActorContext<CronModelState>,
}

impl AgentCronService {
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
            cron_actor_logic(),
            AgentActorOptions {
                id: "cron".to_string(),
                input: None,
                durable: Some(AgentActorDurable {
                    events: vec![
                        "cron.add".to_string(),
                        "cron.delete".to_string(),
                        "cron.cursor".to_string(),
                        "session.forked".to_string(),
                        "context.append_message".to_string(),
                    ],
                    undoable: false,
                    transition: cron_transition(),
                    read: Arc::new(|context| {
                        context
                            .downcast_ref::<Mutex<CronActorContext>>()
                            .unwrap()
                            .lock()
                            .model
                            .clone()
                    }),
                    commit: Arc::new(|actor, model, _| {
                        actor.send(XEvent::new("cron.commit", json!({"model": model})));
                    }),
                }),
            },
        );
        Self { base, actor }
    }

    pub fn now(&self) -> i64 {
        clocks_of(&self.actor).wall_now()
    }

    pub fn is_disabled(&self) -> bool {
        cron_config_of(&self.actor).disabled
    }

    pub fn add_task(&self, init: CronTaskInit) -> Result<CronTask, String> {
        let tasks = (self.actor.get_state)().tasks;
        let mut id = None;
        for _ in 0..MAX_ID_ATTEMPTS {
            let candidate = ulid::Ulid::new().to_string();
            if CRON_ID_REGEX.is_match(&candidate) && !tasks.contains_key(&candidate) {
                id = Some(candidate);
                break;
            }
        }
        let Some(id) = id else {
            return Err(format!(
                "SessionCronService: failed to generate a unique ULID after {MAX_ID_ATTEMPTS} attempts"
            ));
        };
        let task = init.into_task(id, self.now());
        let dispatch = self.actor.dispatch.clone();
        let event = cron_add_event(&task);
        tokio::spawn(async move {
            let _ = dispatch(event).await;
        });
        Ok(task)
    }

    pub fn remove_tasks(&self, ids: &[String]) -> Vec<String> {
        remove_tasks(&self.actor, ids)
    }

    pub fn get_task(&self, id: &str) -> Option<CronTask> {
        (self.actor.get_state)().tasks.get(id).cloned()
    }

    pub fn list(&self) -> Vec<CronTask> {
        (self.actor.get_state)().tasks.values().cloned().collect()
    }

    pub fn is_stale(&self, task: &CronTask) -> bool {
        is_stale_at(&self.actor, task, self.now())
    }

    pub fn get_next_fire_time(&self) -> Option<i64> {
        let mut min = None;
        for task in (self.actor.get_state)().tasks.values() {
            if let Some(next) = next_fire_for(&self.actor, task)
                && min.is_none_or(|current| next < current)
            {
                min = Some(next);
            }
        }
        min
    }

    pub fn get_next_fire_for_task(&self, task_id: &str) -> Option<i64> {
        let task = (self.actor.get_state)().tasks.get(task_id).cloned()?;
        next_fire_for(&self.actor, &task)
    }

    pub fn compute_display_next_fire(
        &self,
        task: &CronTask,
        parsed: &ParsedCronExpression,
        ideal_ms: i64,
    ) -> Option<i64> {
        let no_jitter = cron_config_of(&self.actor).no_jitter;
        Some(if task.recurring == Some(false) {
            one_shot_jittered_next_cron_run_ms(task, ideal_ms, None, no_jitter)
        } else {
            jittered_next_cron_run_ms(task, parsed, ideal_ms, None, no_jitter)
        })
    }

    pub fn handle_missed(
        &self,
        tasks: &[CronTask],
        render_missed_notification: &dyn Fn(&[CronTask]) -> Vec<Value>,
    ) -> Option<Value> {
        if tasks.is_empty() {
            return None;
        }
        let origin = CronMissedOrigin {
            kind: "cron_missed".to_string(),
            count: tasks.len(),
        };
        self.actor
            .service::<dyn AgentLoopService>("loopService")
            .submit(
                json!({
                    "message": {"role": "user", "content": render_missed_notification(tasks)},
                    "meta": {"origin": origin},
                }),
                Some(json!({"steerIfActive": true})),
            )
            .expect("cron missed submit failed");
        telemetry_of(&self.actor).track2(CRON_MISSED, json!({"count": tasks.len()}));
        None
    }

    pub fn emit_scheduled(&self, task: &CronTask, agent_id: Option<&str>) {
        telemetry_of(&self.actor).track2(
            CRON_SCHEDULED,
            json!({"recurring": task.recurring != Some(false), "agent_id": agent_id}),
        );
    }

    pub fn emit_deleted(&self, task_id: &str, agent_id: Option<&str>) {
        telemetry_of(&self.actor).track2(
            CRON_DELETED,
            json!({"task_id": task_id, "agent_id": agent_id}),
        );
    }

    pub fn tick(&self) -> crate::rpc::types::BoxFuture<'static, Result<(), String>> {
        let actor = self.actor.clone();
        Box::pin(async move {
            let (sender, receiver) = tokio::sync::oneshot::channel();
            (actor.send)(XEvent::with_data(
                "cron.tick",
                Value::Null,
                Arc::new(TickCompletion {
                    sender: Mutex::new(Some(sender)),
                }),
            ));
            match receiver.await {
                Ok(outcome) => outcome,
                Err(error) => Err(error.to_string()),
            }
        })
    }
}

fn cron_transition() -> StateFold<CronModelState> {
    Arc::new(|mut state, event| {
        match event.type_.as_str() {
            "cron.add" => {
                if let Some(task) = event.payload.get("task").cloned()
                    && let Ok(task) = serde_json::from_value::<CronTask>(task)
                {
                    state.tasks.insert(task.id.clone(), task);
                }
            }
            "cron.delete" => {
                if let Some(ids) = event.payload.get("ids").and_then(Value::as_array) {
                    for id in ids.iter().filter_map(Value::as_str) {
                        state.tasks.shift_remove(id);
                    }
                }
            }
            "cron.cursor" => {
                if let Some(id) = event.payload.get("id").and_then(Value::as_str)
                    && let Some(last_fired_at) =
                        event.payload.get("lastFiredAt").and_then(Value::as_i64)
                    && let Some(task) = state.tasks.get_mut(id)
                {
                    task.last_fired_at = Some(last_fired_at);
                }
            }
            "session.forked" => {
                state.fork_notice.reminder_pending =
                    !state.tasks.is_empty() || state.fork_notice.reminder_pending;
                state.tasks.clear();
            }
            "context.append_message" => {
                if state.fork_notice.reminder_pending
                    && let Some(message) = event.payload.get("message").cloned()
                    && let Ok(message) = serde_json::from_value::<ContextMessage>(message)
                    && is_cron_fork_cleared_reminder(&message)
                {
                    state.fork_notice.reminder_pending = false;
                }
            }
            _ => {}
        }
        state
    })
}

fn is_cron_fork_cleared_reminder(message: &ContextMessage) -> bool {
    message
        .origin
        .as_ref()
        .and_then(|origin| origin.get("kind"))
        .and_then(Value::as_str)
        == Some("injection")
        && message
            .origin
            .as_ref()
            .and_then(|origin| origin.get("variant"))
            .and_then(Value::as_str)
            == Some(CRON_FORK_CLEARED_REMINDER_NAME)
}
