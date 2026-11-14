use std::collections::{HashMap, HashSet};
use std::ops::Deref;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use kimi_core::primitives::errors::{Error2, Error2Options};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::_base::di::instantiation::ServicesAccessorExt;
use crate::_base::di::scope::IAgentScopeHandle;
use crate::_base::utils::abort::{AbortController, AbortError, AbortSignal, link_abort_signal};
use crate::_base::utils::error_message::error_message;
use crate::agent::r#loop::r#loop::{IAgentLoopService, LoopState};
use crate::agent::permission_mode::permission_mode::IAgentPermissionModeService;
use crate::agent::profile::profile::IAgentProfileService;
use crate::agent::scope_context::scope_context::agent_context_of;
use crate::app::event::event2::{Event2, Event2Class};
use crate::features::tower::tower::has_pinned_permission_mode;
use crate::os::interface::host_clock::IHostClock;
use crate::os::interface::host_timers::IHostTimers;
use crate::session::agent_lifecycle::agent_lifecycle::{
    CreateAgentOptions, IAgentLifecycleService,
};
use crate::session::agent_lifecycle::create_awaiting_close::create_agent_awaiting_close;
use crate::session::agent_lifecycle::errors::{
    AGENT_ALREADY_RUNNING, AGENT_NOT_A_SUBAGENT, AGENT_NOT_FOUND, AGENT_NOT_OWNED,
};
use crate::session::agent_lifecycle::subagent_metadata::{
    SubagentLabelOptions, is_subagent_meta, labels_from_agent_meta, subagent_labels,
    subagent_parent_agent_id, subagent_swarm_item,
};
use crate::session::session_metadata::session_metadata::{AgentMeta, ISessionMetadata};
use crate::session::subagent::mirror_agent_run::{
    AgentRunSpawnedMeta, MirrorAgentRunOptions, RunTermination, SubagentCancelled,
    SubagentCancelledPayload, SubagentFailed, SubagentFailedPayload, classify_run_termination,
    emit_agent_run_spawned, mirror_agent_run,
};
use crate::session::subagent::spawn::SpawnSubagentOptions;
use crate::session::subagent::subagent::{
    AgentRunCompletion, AgentRunRequest, ISessionSubagentService, RunAgentOptions,
};
use crate::state::event_dispatcher::IEventDispatcher;
use crate::wire::record::WireAsyncError;

pub use super::agentRunBatch::{
    SessionSwarmRunResult, SessionSwarmTask, SessionSwarmTaskKind,
};
use super::agentRunBatch::{
    AgentRunAbandonOutcome, AgentRunAbandonedEvent, AgentRunAttemptCompletion,
    AgentRunAttemptHandle, AgentRunAttemptOptions, AgentRunBatch, AgentRunBatchLauncher,
    AgentRunBatchOptions, AgentRunSuspendedEvent, AgentSpawnAttemptOptions,
    resolve_swarm_max_concurrency,
};

pub const RESUMED_PROFILE_FALLBACK: &str = "subagent";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentSuspendedPayload {
    pub subagent_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SubagentSuspended {
    event: Event2,
    pub payload: SubagentSuspendedPayload,
}

impl SubagentSuspended {
    pub const TYPE: &'static str = "subagent.suspended";

    pub fn new(payload: SubagentSuspendedPayload) -> Self {
        let class = Event2Class {
            type_: Self::TYPE.to_string(),
            durable: false,
            observable: true,
            agent_domain: false,
            schema: None,
        };
        let event = Event2::new(
            &class,
            serde_json::to_value(&payload).expect("subagent suspended payload is serializable"),
            None,
        );
        Self { event, payload }
    }

    pub fn as_event(&self) -> &Event2 {
        &self.event
    }
}

impl Deref for SubagentSuspended {
    type Target = SubagentSuspendedPayload;

    fn deref(&self) -> &Self::Target {
        &self.payload
    }
}

impl From<SubagentSuspended> for Event2 {
    fn from(event: SubagentSuspended) -> Self {
        event.event
    }
}

pub struct SessionSwarmRunArgs<T> {
    pub caller_agent_id: String,
    pub tasks: Vec<SessionSwarmTask<T>>,
}

pub struct SessionSwarmService {
    agent_lifecycle: Arc<dyn IAgentLifecycleService>,
    subagents: Arc<dyn ISessionSubagentService>,
    metadata: Arc<dyn ISessionMetadata>,
    clock: Arc<dyn IHostClock>,
    timers: Arc<dyn IHostTimers>,
    in_flight: Mutex<HashMap<String, Arc<AbortController>>>,
}

impl SessionSwarmService {
    pub fn new(
        agent_lifecycle: Arc<dyn IAgentLifecycleService>,
        subagents: Arc<dyn ISessionSubagentService>,
        metadata: Arc<dyn ISessionMetadata>,
        clock: Arc<dyn IHostClock>,
        timers: Arc<dyn IHostTimers>,
    ) -> Self {
        Self {
            agent_lifecycle,
            subagents,
            metadata,
            clock,
            timers,
            in_flight: Mutex::new(HashMap::new()),
        }
    }

    pub async fn get_swarm_item(
        &self,
        caller_agent_id: &str,
        agent_id: &str,
    ) -> Result<Option<String>, WireAsyncError> {
        let meta = read_agent_meta(&self.metadata, agent_id).await?;
        let Some(meta) = meta else {
            return Ok(None);
        };
        if !is_subagent_meta(Some(&meta)) {
            return Ok(None);
        }
        if subagent_parent_agent_id(Some(&meta)).as_deref() != Some(caller_agent_id) {
            return Ok(None);
        }
        Ok(subagent_swarm_item(Some(&meta)))
    }

    pub async fn run<T: Clone + Send + Sync + 'static>(
        &self,
        args: SessionSwarmRunArgs<T>,
    ) -> Result<Vec<SessionSwarmRunResult<T>>, WireAsyncError> {
        let SessionSwarmRunArgs {
            caller_agent_id,
            tasks,
        } = args;
        let controller = Arc::new(AbortController::new());
        self.in_flight
            .lock()
            .insert(caller_agent_id.clone(), controller.clone());

        let mut unlinks = Vec::new();
        let linked_tasks = tasks
            .into_iter()
            .map(|mut task| {
                if let Some(signal) = &task.signal {
                    unlinks.push(link_abort_signal(signal, controller.clone()));
                }
                task.signal = Some(controller.signal());
                task
            })
            .collect::<Vec<_>>();

        let terminalized = Arc::new(Mutex::new(HashSet::new()));
        let terminalize: Arc<dyn Fn(String, Event2) + Send + Sync> = {
            let terminalized = terminalized.clone();
            let agent_lifecycle = self.agent_lifecycle.clone();
            let caller_agent_id = caller_agent_id.clone();
            Arc::new(move |agent_id: String, event: Event2| {
                if !terminalized.lock().insert(agent_id) {
                    return;
                }
                dispatch_subagent_event(&agent_lifecycle, &caller_agent_id, event);
            })
        };

        let launcher = Arc::new(SwarmLauncher {
            agent_lifecycle: self.agent_lifecycle.clone(),
            subagents: self.subagents.clone(),
            metadata: self.metadata.clone(),
            caller_agent_id: caller_agent_id.clone(),
            terminalize,
        });

        let max_concurrency = resolve_swarm_max_concurrency()?;
        let outcome = AgentRunBatch::new(
            launcher,
            linked_tasks,
            AgentRunBatchOptions {
                max_concurrency,
                clock: self.clock.clone(),
                timers: self.timers.clone(),
            },
        )
        .run()
        .await;

        for unlink in unlinks {
            unlink();
        }
        {
            let mut in_flight = self.in_flight.lock();
            let is_current = in_flight
                .get(&caller_agent_id)
                .is_some_and(|current| Arc::ptr_eq(current, &controller));
            if is_current {
                in_flight.remove(&caller_agent_id);
            }
        }

        outcome
    }

    pub fn cancel(&self, caller_agent_id: &str) {
        if let Some(controller) = self.in_flight.lock().get(caller_agent_id) {
            controller.abort(None);
        }
    }
}

#[derive(Clone)]
struct SwarmLauncher {
    agent_lifecycle: Arc<dyn IAgentLifecycleService>,
    subagents: Arc<dyn ISessionSubagentService>,
    metadata: Arc<dyn ISessionMetadata>,
    caller_agent_id: String,
    terminalize: Arc<dyn Fn(String, Event2) + Send + Sync>,
}

impl SwarmLauncher {
    async fn spawn_attempt(
        &self,
        options: AgentSpawnAttemptOptions,
    ) -> Result<AgentRunAttemptHandle, WireAsyncError> {
        throw_if_aborted(&options.run.signal)?;
        let caller = self.require_handle(&self.caller_agent_id, "Caller agent")?;
        let plan = options.plan.clone();
        let spawned = self
            .subagents
            .spawn(SpawnSubagentOptions {
                caller_agent_id: self.caller_agent_id.clone(),
                plan: plan.clone(),
                labels: Some(subagent_labels(
                    &self.caller_agent_id,
                    SubagentLabelOptions {
                        swarm_item: options.swarm_item.clone(),
                        depth: None,
                    },
                )),
                prompt: options.run.prompt.clone(),
            })
            .await?;

        emit_agent_run_spawned(
            &caller,
            &spawned.agent_id,
            AgentRunSpawnedMeta {
                profile_name: plan.profile_name.clone(),
                parent_tool_call_id: Some(options.run.parent_tool_call_id.clone()),
                parent_tool_call_uuid: options.run.parent_tool_call_uuid.clone(),
                description: Some(options.run.description.clone()),
                swarm_index: options.run.swarm_index.map(|index| index as f64),
                run_in_background: Some(options.run.run_in_background),
                fork: Some(plan.fork),
                model: Some(plan.model.clone()),
                model_source: plan.model_source.clone(),
                task_id: None,
            },
        );

        let child = self.require_handle(&spawned.agent_id, "Agent instance")?;
        self.observe(
            caller,
            child,
            plan.profile_name.clone(),
            AgentRunRequest::Prompt {
                prompt: spawned.prompt_text,
                helper: None,
            },
            options.run,
        )
        .await
    }

    async fn resume_attempt(
        &self,
        agent_id: String,
        options: AgentRunAttemptOptions,
        retry_turn: bool,
    ) -> Result<AgentRunAttemptHandle, WireAsyncError> {
        throw_if_aborted(&options.signal)?;
        let meta = self.require_owned_subagent(&agent_id).await?;
        let caller = self.require_handle(&self.caller_agent_id, "Caller agent")?;
        let child = match self.agent_lifecycle.handle_of(&agent_id) {
            Some(child) => child,
            None => {
                self.rebuild_subagent(&agent_id, &meta, &caller, &options.signal)
                    .await?
            }
        };
        self.require_idle_subagent(&agent_id, &child)?;

        let profile = child.accessor().get::<IAgentProfileService>();
        let data = profile.data();
        let profile_name = data
            .base
            .profile_name
            .clone()
            .unwrap_or_else(|| RESUMED_PROFILE_FALLBACK.to_string());
        if !retry_turn {
            emit_agent_run_spawned(
                &caller,
                &agent_id,
                AgentRunSpawnedMeta {
                    profile_name: profile_name.clone(),
                    parent_tool_call_id: Some(options.parent_tool_call_id.clone()),
                    parent_tool_call_uuid: options.parent_tool_call_uuid.clone(),
                    description: Some(options.description.clone()),
                    swarm_index: options.swarm_index.map(|index| index as f64),
                    run_in_background: Some(options.run_in_background),
                    fork: None,
                    model: data.base.model_alias.clone(),
                    model_source: None,
                    task_id: None,
                },
            );
        }

        let request = if retry_turn {
            AgentRunRequest::Retry { trigger: None }
        } else {
            AgentRunRequest::Prompt {
                prompt: options.prompt.clone(),
                helper: None,
            }
        };
        self.observe(caller, child, profile_name, request, options)
            .await
    }

    async fn observe(
        &self,
        caller: Arc<IAgentScopeHandle>,
        child: Arc<IAgentScopeHandle>,
        profile_name: String,
        request: AgentRunRequest,
        options: AgentRunAttemptOptions,
    ) -> Result<AgentRunAttemptHandle, WireAsyncError> {
        let agent_id = child.id().to_string();
        let prompt = match &request {
            AgentRunRequest::Prompt { prompt, .. } => Some(prompt.clone()),
            AgentRunRequest::Retry { .. } => None,
        };
        let run = match self
            .subagents
            .run(
                agent_context_of(&child),
                request,
                RunAgentOptions {
                    signal: options.signal.clone(),
                    on_ready: options.on_ready.clone(),
                },
            )
            .await
        {
            Ok(run) => run,
            Err(error) => {
                (self.terminalize)(
                    agent_id.clone(),
                    run_start_terminal_event(&agent_id, &error, &options.signal),
                );
                return Err(error);
            }
        };

        let mirror_options = MirrorAgentRunOptions {
            profile_name,
            prompt,
            suppress_rate_limit_failure_event: Some(options.suppress_rate_limit_failure_event),
            signal: options.signal.clone(),
            cancel: None,
            defer_started: None,
            terminalize: Some(self.terminalize.clone()),
        };
        let completion: BoxFuture<'static, Result<AgentRunAttemptCompletion, WireAsyncError>> =
            Box::pin(async move {
                let completion = mirror_agent_run(&caller, run, mirror_options).await?;
                Ok(completion_into_attempt(completion))
            });
        Ok(AgentRunAttemptHandle {
            agent_id,
            completion,
        })
    }

    fn require_handle(
        &self,
        agent_id: &str,
        label: &str,
    ) -> Result<Arc<IAgentScopeHandle>, WireAsyncError> {
        self.agent_lifecycle.handle_of(agent_id).ok_or_else(|| {
            coded_error(
                AGENT_NOT_FOUND,
                format!("{label} \"{agent_id}\" does not exist"),
                [("agentId", json!(agent_id))],
            )
        })
    }

    fn require_idle_subagent(
        &self,
        agent_id: &str,
        child: &Arc<IAgentScopeHandle>,
    ) -> Result<(), WireAsyncError> {
        if child.accessor().get::<IAgentLoopService>().snapshot().state == LoopState::Running {
            return Err(coded_error(
                AGENT_ALREADY_RUNNING,
                format!("Agent instance \"{agent_id}\" is already running and cannot run concurrently"),
                [("agentId", json!(agent_id))],
            ));
        }
        Ok(())
    }

    async fn rebuild_subagent(
        &self,
        agent_id: &str,
        meta: &AgentMeta,
        caller: &Arc<IAgentScopeHandle>,
        signal: &AbortSignal,
    ) -> Result<Arc<IAgentScopeHandle>, WireAsyncError> {
        create_agent_awaiting_close(
            &self.agent_lifecycle,
            CreateAgentOptions {
                agent_id: Some(agent_id.to_string()),
                labels: labels_from_agent_meta(meta),
                forked_from: meta.forked_from.clone(),
                ..Default::default()
            },
            Some(signal),
        )
        .await?;

        let Some(rebuilt) = self.agent_lifecycle.handle_of(agent_id) else {
            return Err(coded_error(
                AGENT_NOT_FOUND,
                format!("Agent instance \"{agent_id}\" does not exist"),
                [("agentId", json!(agent_id))],
            ));
        };

        let profile_name = rebuilt
            .accessor()
            .get::<IAgentProfileService>()
            .data()
            .base
            .profile_name;
        if !has_pinned_permission_mode(profile_name.as_deref()) {
            let mode = caller.accessor().get::<IAgentPermissionModeService>().mode();
            rebuilt
                .accessor()
                .get::<IAgentPermissionModeService>()
                .set_mode(mode);
        }

        Ok(rebuilt)
    }

    async fn require_owned_subagent(&self, agent_id: &str) -> Result<AgentMeta, WireAsyncError> {
        let meta = read_agent_meta(&self.metadata, agent_id).await?;
        let Some(meta) = meta.filter(|meta| is_subagent_meta(Some(meta))) else {
            return Err(coded_error(
                AGENT_NOT_A_SUBAGENT,
                format!("Agent instance \"{agent_id}\" is not a subagent"),
                [("agentId", json!(agent_id))],
            ));
        };
        if subagent_parent_agent_id(Some(&meta)).as_deref() != Some(self.caller_agent_id.as_str()) {
            return Err(coded_error(
                AGENT_NOT_OWNED,
                format!("Agent instance \"{agent_id}\" does not belong to this parent agent"),
                [
                    ("agentId", json!(agent_id)),
                    ("callerAgentId", json!(self.caller_agent_id)),
                ],
            ));
        }
        Ok(meta)
    }
}

impl<T: Clone + Send + Sync + 'static> AgentRunBatchLauncher<T> for SwarmLauncher {
    fn spawn(
        &self,
        options: AgentSpawnAttemptOptions,
    ) -> BoxFuture<'static, Result<AgentRunAttemptHandle, WireAsyncError>> {
        let this = self.clone();
        Box::pin(async move { this.spawn_attempt(options).await })
    }

    fn resume(
        &self,
        agent_id: String,
        options: AgentRunAttemptOptions,
    ) -> BoxFuture<'static, Result<AgentRunAttemptHandle, WireAsyncError>> {
        let this = self.clone();
        Box::pin(async move { this.resume_attempt(agent_id, options, false).await })
    }

    fn retry(
        &self,
        agent_id: String,
        options: AgentRunAttemptOptions,
    ) -> BoxFuture<'static, Result<AgentRunAttemptHandle, WireAsyncError>> {
        let this = self.clone();
        Box::pin(async move { this.resume_attempt(agent_id, options, true).await })
    }

    fn suspended(&self, event: AgentRunSuspendedEvent<T>) {
        dispatch_subagent_event(
            &self.agent_lifecycle,
            &self.caller_agent_id,
            SubagentSuspended::new(SubagentSuspendedPayload {
                subagent_id: event.agent_id,
                reason: event.reason,
            })
            .into(),
        );
    }

    fn abandoned(&self, event: AgentRunAbandonedEvent<T>) {
        let terminal_event: Event2 = match event.outcome {
            AgentRunAbandonOutcome::Failed => {
                SubagentFailed::new(SubagentFailedPayload {
                    subagent_id: event.agent_id.clone(),
                    error: event
                        .error
                        .unwrap_or_else(|| "Provider rate limit".to_string()),
                })
                .into()
            }
            AgentRunAbandonOutcome::Cancelled => SubagentCancelled::new(SubagentCancelledPayload {
                subagent_id: event.agent_id.clone(),
            })
            .into(),
        };
        (self.terminalize)(event.agent_id, terminal_event);
    }
}

async fn read_agent_meta(
    metadata: &Arc<dyn ISessionMetadata>,
    agent_id: &str,
) -> Result<Option<AgentMeta>, WireAsyncError> {
    let meta = metadata.read().await?;
    Ok(meta
        .agents
        .and_then(|agents| agents.get(agent_id).cloned()))
}

fn dispatch_subagent_event(
    agent_lifecycle: &Arc<dyn IAgentLifecycleService>,
    caller_agent_id: &str,
    event: Event2,
) {
    let Some(caller) = agent_lifecycle.handle_of(caller_agent_id) else {
        return;
    };
    let dispatcher = caller.accessor().get::<IEventDispatcher>();
    tokio::spawn(async move {
        let _ = dispatcher.dispatch(event).await;
    });
}

fn run_start_terminal_event(agent_id: &str, error: &WireAsyncError, signal: &AbortSignal) -> Event2 {
    if classify_run_termination(error, signal) == RunTermination::Cancelled {
        return SubagentCancelled::new(SubagentCancelledPayload {
            subagent_id: agent_id.to_string(),
        })
        .into();
    }
    SubagentFailed::new(SubagentFailedPayload {
        subagent_id: agent_id.to_string(),
        error: error_message(error.as_ref()),
    })
    .into()
}

fn completion_into_attempt(completion: AgentRunCompletion) -> AgentRunAttemptCompletion {
    AgentRunAttemptCompletion {
        result: completion.summary,
        usage: completion.usage,
        stop_reason: completion.stop_reason,
    }
}

fn throw_if_aborted(signal: &AbortSignal) -> Result<(), WireAsyncError> {
    if !signal.aborted() {
        return Ok(());
    }
    Err(match signal.reason() {
        Some(reason) => Box::new(AbortError::new(reason.to_string())),
        None => Box::new(AbortError::new("Aborted")),
    })
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
