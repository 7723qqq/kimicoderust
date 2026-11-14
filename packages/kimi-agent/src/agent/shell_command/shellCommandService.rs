use std::collections::HashMap;
use std::ops::Deref;
use std::sync::{Arc, LazyLock};
use std::time::Instant;

use async_trait::async_trait;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::_base::utils::abort::{AbortController, user_cancellation_reason};
use crate::_base::utils::xml_escape::escape_xml;
use crate::agent::context_memory::context_memory::IAgentContextMemoryService;
use crate::agent::context_memory::types::{
    ContextMessage, PromptOrigin, ShellCommandOrigin, ShellCommandPhase,
};
use crate::agent::r#loop::r#loop::{IAgentLoopService, LoopSubmitOptions};
use crate::agent::scope_context::scope_context::IAgentScopeContext;
use crate::agent::state::agent_state::{StateError, StateKey};
use crate::agent::state::agent_state_service::{AgentStateService, StateRegistration};
use crate::agent::tool_executor::tool_executor::{
    ExecutableTool, ExecutableToolContext, ExecutableToolOutput, IAgentToolRegistryService,
    ToolExecution,
};
use crate::tools::ToolUpdate;
use crate::app::event::event2::{Event2, Event2Class};
use crate::app::telemetry::telemetry::ITelemetryService;
use crate::human::agent::origin::PromptOrigin as MachinePromptOrigin;
use crate::human::agent::turn::{UserMessage, UserMeta, create_user_entry};
use crate::human::llm::message::{ContentPart, Role};
use crate::state::eventDispatcher::IEventDispatcher;
use crate::wire::record::WireAsyncError;
use kimi_core::primitives::errors::{Error2, domains::core::INTERNAL};

use super::shell_command::{IAgentShellCommandService, RunShellCommandInput, RunShellCommandResult};

const SHELL_FOREGROUND_TIMEOUT_S: u64 = 2 * 60;

pub static SHELL_COMMAND_TASKS_KEY: LazyLock<StateKey> = LazyLock::new(|| {
    StateKey::new(
        "shellCommand.tasks",
        Arc::new(|| Value::Object(Map::new())),
    )
});

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellToolUpdate {
    pub kind: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percent: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_data: Option<Value>,
    pub replace: bool,
}

impl From<&ToolUpdate<'_>> for ShellToolUpdate {
    fn from(update: &ToolUpdate<'_>) -> Self {
        Self {
            kind: update.kind.to_string(),
            text: update.text.to_string(),
            percent: update.percent,
            custom_kind: update.custom_kind.map(str::to_string),
            custom_data: update.custom_data.cloned(),
            replace: update.replace,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellOutputPayload {
    pub agent_id: String,
    pub command_id: String,
    pub update: ShellToolUpdate,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellStartedPayload {
    pub agent_id: String,
    pub command_id: String,
    pub task_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellCompletedPayload {
    pub agent_id: String,
    pub command_id: String,
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
}

macro_rules! shell_event {
    ($name:ident, $payload:ident, $class:ident, $type:literal) => {
        pub static $class: LazyLock<Arc<Event2Class>> = LazyLock::new(|| {
            Arc::new(Event2Class {
                type_: $type.to_string(),
                durable: false,
                observable: true,
                agent_domain: true,
                schema: None,
            })
        });

        #[derive(Debug, Clone, PartialEq)]
        pub struct $name {
            event: Event2,
            pub payload: $payload,
        }

        impl $name {
            pub const TYPE: &'static str = $type;

            pub fn new(payload: $payload) -> Self {
                let event = Event2::new(
                    &$class,
                    serde_json::to_value(&payload).expect("shell event is serializable"),
                    None,
                );
                Self { event, payload }
            }

            pub fn class() -> Arc<Event2Class> {
                $class.clone()
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

shell_event!(ShellOutput, ShellOutputPayload, SHELL_OUTPUT_CLASS, "shell.output");
shell_event!(ShellStarted, ShellStartedPayload, SHELL_STARTED_CLASS, "shell.started");
shell_event!(ShellCompleted, ShellCompletedPayload, SHELL_COMPLETED_CLASS, "shell.completed");

pub struct AgentShellCommandService {
    tool_registry: Arc<dyn IAgentToolRegistryService>,
    context: Arc<dyn IAgentContextMemoryService>,
    loop_: Arc<dyn IAgentLoopService>,
    dispatcher: Arc<dyn IEventDispatcher>,
    scope_context: Arc<dyn IAgentScopeContext>,
    states: Arc<AgentStateService>,
    telemetry: Arc<dyn ITelemetryService>,
    controllers: Mutex<HashMap<String, Arc<AbortController>>>,
    _registration: StateRegistration,
}

impl AgentShellCommandService {
    pub fn new(
        tool_registry: Arc<dyn IAgentToolRegistryService>,
        context: Arc<dyn IAgentContextMemoryService>,
        loop_: Arc<dyn IAgentLoopService>,
        dispatcher: Arc<dyn IEventDispatcher>,
        scope_context: Arc<dyn IAgentScopeContext>,
        states: Arc<AgentStateService>,
        telemetry: Arc<dyn ITelemetryService>,
    ) -> Result<Self, StateError> {
        let registration = states.contribute_state(SHELL_COMMAND_TASKS_KEY.clone())?;
        Ok(Self {
            tool_registry,
            context,
            loop_,
            dispatcher,
            scope_context,
            states,
            telemetry,
            controllers: Mutex::new(HashMap::new()),
            _registration: registration,
        })
    }

    fn tasks(&self) -> HashMap<String, String> {
        self.states
            .get(&SHELL_COMMAND_TASKS_KEY)
            .ok()
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default()
    }

    fn task_id(&self, command_id: &str) -> Option<String> {
        self.tasks().get(command_id).cloned()
    }

    fn remove_task_id(&self, command_id: &str) {
        let mut tasks = self.tasks();
        tasks.remove(command_id);
        let _ = self
            .states
            .set(&SHELL_COMMAND_TASKS_KEY, serde_json::to_value(tasks).unwrap());
    }

    fn dispatch_detached(&self, event: impl Into<Event2>) {
        let dispatcher = self.dispatcher.clone();
        let event = event.into();
        tokio::spawn(async move {
            let _ = dispatcher.dispatch(event).await;
        });
    }

    fn ensure_bash_tool(&self) -> Result<Arc<dyn ExecutableTool>, WireAsyncError> {
        self.tool_registry.resolve("Bash").ok_or_else(|| {
            Box::new(Error2::new(INTERNAL, "Bash tool is not registered.")) as WireAsyncError
        })
    }

    fn append_shell_input(&self, command: &str) {
        let mut message = ContextMessage::new(
            Role::User,
            vec![ContentPart::Text {
                text: format!("<bash-input>\n{}\n</bash-input>", escape_xml(command)),
                meta: None,
            }],
        );
        message.origin = Some(PromptOrigin::ShellCommand(ShellCommandOrigin {
            kind: "shell_command".to_string(),
            phase: ShellCommandPhase::Input,
            is_error: None,
        }));
        self.context.append(&[message]);
    }

    fn append_shell_output(&self, stdout: &str, stderr: &str, is_error: bool) {
        let mut message = ContextMessage::new(
            Role::User,
            vec![ContentPart::Text {
                text: format!(
                    "<bash-stdout>{}</bash-stdout><bash-stderr>{}</bash-stderr>",
                    escape_xml(stdout),
                    escape_xml(stderr)
                ),
                meta: None,
            }],
        );
        message.origin = Some(PromptOrigin::ShellCommand(ShellCommandOrigin {
            kind: "shell_command".to_string(),
            phase: ShellCommandPhase::Output,
            is_error: is_error.then_some(true),
        }));
        self.context.append(&[message]);
    }

    fn notify_backgrounded(&self, output: String) {
        self.loop_.submit(
            create_user_entry(
                UserMessage {
                    role: Role::User,
                    content: vec![ContentPart::Text {
                        text: output,
                        meta: None,
                    }],
                },
                UserMeta {
                    origin: Some(MachinePromptOrigin {
                        kind: "injection".to_string(),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            ),
            Some(LoopSubmitOptions {
                steer_if_active: Some(true),
                on_materialize: None,
            }),
        );
    }

    async fn execute_input(
        &self,
        input: &RunShellCommandInput,
        controller: &Arc<AbortController>,
        stdout: Arc<Mutex<String>>,
        stderr: Arc<Mutex<String>>,
    ) -> Result<RunShellCommandResult, WireAsyncError> {
        let execution = self.ensure_bash_tool()?.resolve_execution(json!({
            "command": input.command,
            "timeout": SHELL_FOREGROUND_TIMEOUT_S,
        })).await?;
        let execution = match execution {
            ToolExecution::Runnable(execution) => execution,
            ToolExecution::Error(result) => {
                let output = match result.output {
                    ExecutableToolOutput::Text(output) => output,
                    ExecutableToolOutput::Parts(_) => "Command failed.".to_string(),
                };
                self.append_shell_output("", &output, true);
                return Ok(RunShellCommandResult {
                    stdout: String::new(),
                    stderr: output,
                    is_error: Some(true),
                    backgrounded: None,
                });
            }
        };
        let command_id = input.command_id.clone();
        let on_update = {
            let command_id = command_id.clone();
            let states = self.states.clone();
            let dispatcher = self.dispatcher.clone();
            let agent_id = self.scope_context.agent_id();
            let stdout = stdout.clone();
            let stderr = stderr.clone();
            Arc::new(move |update: ToolUpdate<'_>| {
                match update.kind {
                    "stdout" => stdout.lock().push_str(update.text),
                    "stderr" => stderr.lock().push_str(update.text),
                    _ => return,
                }
                if let Some(command_id) = &command_id {
                    let task_id = task_id_for(&states, command_id);
                    dispatch_detached(&dispatcher, ShellOutput::new(ShellOutputPayload {
                        agent_id: agent_id.clone(),
                        command_id: command_id.clone(),
                        update: ShellToolUpdate::from(&update),
                        task_id,
                    }));
                }
            })
        };
        let on_foreground_task_start = command_id.clone().map(|command_id| {
            let states = self.states.clone();
            let dispatcher = self.dispatcher.clone();
            let agent_id = self.scope_context.agent_id();
            Arc::new(move |task_id: String| {
                let mut tasks = read_tasks(&states);
                tasks.insert(command_id.clone(), task_id.clone());
                let _ = states.set(&SHELL_COMMAND_TASKS_KEY, serde_json::to_value(tasks).unwrap());
                dispatch_detached(&dispatcher, ShellStarted::new(ShellStartedPayload {
                    agent_id: agent_id.clone(),
                    command_id: command_id.clone(),
                    task_id,
                }));
            })
        });
        let result = (execution.execute)(ExecutableToolContext {
            turn_id: u64::MAX,
            tool_call_id: "shell-command".to_string(),
            trace: None,
            metadata: None,
            signal: controller.signal(),
            steer_signal: None,
            on_update: Some(on_update),
            on_foreground_task_start,
        })
        .await?;
        let is_error = result.is_error == Some(true);
        if let ExecutableToolOutput::Text(output) = &result.output {
            if output.starts_with("task_id: ") {
                self.notify_backgrounded(output.clone());
                return Ok(RunShellCommandResult {
                    stdout: output.clone(),
                    stderr: String::new(),
                    is_error: Some(false),
                    backgrounded: Some(true),
                });
            }
        }
        let mut stderr_text = stderr.lock().clone();
        if is_error && stdout.lock().is_empty() && stderr_text.is_empty() {
            stderr_text = match result.output {
                ExecutableToolOutput::Text(output) => output,
                ExecutableToolOutput::Parts(_) => "Command failed.".to_string(),
            };
            *stderr.lock() = stderr_text.clone();
            if let Some(command_id) = &command_id {
                if !stderr_text.is_empty() {
                    self.dispatch_detached(ShellOutput::new(ShellOutputPayload {
                        agent_id: self.scope_context.agent_id(),
                        command_id: command_id.clone(),
                        update: ShellToolUpdate {
                            kind: "stderr".to_string(),
                            text: stderr_text.clone(),
                            percent: None,
                            custom_kind: None,
                            custom_data: None,
                            replace: false,
                        },
                        task_id: self.task_id(command_id),
                    }));
                }
            }
        }
        if let Some(command_id) = &command_id {
            self.dispatch_detached(ShellCompleted::new(ShellCompletedPayload {
                agent_id: self.scope_context.agent_id(),
                command_id: command_id.clone(),
                is_error,
                task_id: self.task_id(command_id),
            }));
        }
        let stdout_text = stdout.lock().clone();
        self.append_shell_output(&stdout_text, &stderr_text, is_error);
        Ok(RunShellCommandResult {
            stdout: stdout_text,
            stderr: stderr_text,
            is_error: Some(is_error),
            backgrounded: None,
        })
    }
}

#[async_trait]
impl IAgentShellCommandService for AgentShellCommandService {
    async fn run(&self, input: RunShellCommandInput) -> Result<RunShellCommandResult, WireAsyncError> {
        self.append_shell_input(&input.command);
        let controller = Arc::new(AbortController::new());
        if let Some(command_id) = &input.command_id {
            self.controllers
                .lock()
                .insert(command_id.clone(), controller.clone());
        }
        let started_at = Instant::now();
        let stdout = Arc::new(Mutex::new(String::new()));
        let stderr = Arc::new(Mutex::new(String::new()));
        let outcome = self
            .execute_input(&input, &controller, stdout.clone(), stderr.clone())
            .await;
        let (result, is_error, backgrounded) = match outcome {
            Ok(result) => {
                let is_error = result.is_error == Some(true);
                let backgrounded = result.backgrounded == Some(true);
                (result, is_error, backgrounded)
            }
            Err(error) => {
                let message = error.to_string();
                stderr.lock().push_str(&message);
                if let Some(command_id) = &input.command_id {
                    if !message.is_empty() {
                        self.dispatch_detached(ShellOutput::new(ShellOutputPayload {
                            agent_id: self.scope_context.agent_id(),
                            command_id: command_id.clone(),
                            update: ShellToolUpdate {
                                kind: "stderr".to_string(),
                                text: message.clone(),
                                percent: None,
                                custom_kind: None,
                                custom_data: None,
                                replace: false,
                            },
                            task_id: self.task_id(command_id),
                        }));
                    }
                    self.dispatch_detached(ShellCompleted::new(ShellCompletedPayload {
                        agent_id: self.scope_context.agent_id(),
                        command_id: command_id.clone(),
                        is_error: true,
                        task_id: self.task_id(command_id),
                    }));
                }
                let stdout_text = stdout.lock().clone();
                let stderr_text = stderr.lock().clone();
                self.append_shell_output(&stdout_text, &stderr_text, true);
                (
                    RunShellCommandResult {
                        stdout: stdout_text,
                        stderr: stderr_text,
                        is_error: Some(true),
                        backgrounded: None,
                    },
                    true,
                    false,
                )
            }
        };
        if let Some(command_id) = &input.command_id {
            self.controllers.lock().remove(command_id);
            self.remove_task_id(command_id);
        }
        self.telemetry.track2(
            "shell_command_finished",
            Some(Map::from_iter([
                (
                    "duration_ms".to_string(),
                    Value::from(started_at.elapsed().as_millis() as u64),
                ),
                ("is_error".to_string(), Value::from(is_error)),
                ("backgrounded".to_string(), Value::from(backgrounded)),
            ])),
        );
        Ok(result)
    }

    fn cancel(&self, command_id: &str) {
        if let Some(controller) = self.controllers.lock().get(command_id).cloned() {
            controller.abort(Some(Arc::new(user_cancellation_reason())));
        }
    }
}

fn read_tasks(states: &Arc<AgentStateService>) -> HashMap<String, String> {
    states
        .get(&SHELL_COMMAND_TASKS_KEY)
        .ok()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default()
}

fn task_id_for(states: &Arc<AgentStateService>, command_id: &str) -> Option<String> {
    read_tasks(states).get(command_id).cloned()
}

fn dispatch_detached(dispatcher: &Arc<dyn IEventDispatcher>, event: impl Into<Event2>) {
    let dispatcher = dispatcher.clone();
    let event = event.into();
    tokio::spawn(async move {
        let _ = dispatcher.dispatch(event).await;
    });
}
