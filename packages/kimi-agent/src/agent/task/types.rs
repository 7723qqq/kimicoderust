use std::collections::HashSet;
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};

use crate::_base::utils::abort::AbortSignal;
use crate::wire::record::WireAsyncError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentTaskStatus {
    Running,
    Completed,
    Failed,
    TimedOut,
    Killed,
    Lost,
}

impl AgentTaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
            Self::Killed => "killed",
            Self::Lost => "lost",
        }
    }

    pub fn is_terminal(self) -> bool {
        TERMINAL_STATUSES.contains(&self)
    }
}

pub static TERMINAL_STATUSES: LazyLock<HashSet<AgentTaskStatus>> = LazyLock::new(|| {
    HashSet::from([
        AgentTaskStatus::Completed,
        AgentTaskStatus::Failed,
        AgentTaskStatus::TimedOut,
        AgentTaskStatus::Killed,
        AgentTaskStatus::Lost,
    ])
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentTaskSettlementStatus {
    Completed,
    Failed,
    TimedOut,
    Killed,
}

impl From<AgentTaskSettlementStatus> for AgentTaskStatus {
    fn from(value: AgentTaskSettlementStatus) -> Self {
        match value {
            AgentTaskSettlementStatus::Completed => Self::Completed,
            AgentTaskSettlementStatus::Failed => Self::Failed,
            AgentTaskSettlementStatus::TimedOut => Self::TimedOut,
            AgentTaskSettlementStatus::Killed => Self::Killed,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentTaskSettlement {
    pub status: AgentTaskSettlementStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentTaskInfoBase {
    pub task_id: String,
    pub description: String,
    pub status: AgentTaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detached: Option<bool>,
    pub started_at: f64,
    pub ended_at: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_notification_suppressed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_reminded: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentTaskKind {
    Process,
    Agent,
    Question,
}

impl AgentTaskKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Process => "process",
            Self::Agent => "agent",
            Self::Question => "question",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessTaskInfo {
    #[serde(flatten)]
    pub base: AgentTaskInfoBase,
    pub command: String,
    pub pid: u32,
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_tool_call_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentTaskInfo {
    #[serde(flatten)]
    pub base: AgentTaskInfoBase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swarm_index: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_summary: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionTaskInfo {
    #[serde(flatten)]
    pub base: AgentTaskInfoBase,
    pub question_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum AgentTaskInfo {
    Process(ProcessTaskInfo),
    Agent(SubagentTaskInfo),
    Question(QuestionTaskInfo),
}

impl AgentTaskInfo {
    pub fn base(&self) -> &AgentTaskInfoBase {
        match self {
            Self::Process(info) => &info.base,
            Self::Agent(info) => &info.base,
            Self::Question(info) => &info.base,
        }
    }

    pub fn base_mut(&mut self) -> &mut AgentTaskInfoBase {
        match self {
            Self::Process(info) => &mut info.base,
            Self::Agent(info) => &mut info.base,
            Self::Question(info) => &mut info.base,
        }
    }

    pub fn kind(&self) -> AgentTaskKind {
        match self {
            Self::Process(_) => AgentTaskKind::Process,
            Self::Agent(_) => AgentTaskKind::Agent,
            Self::Question(_) => AgentTaskKind::Question,
        }
    }

    pub fn task_id(&self) -> &str {
        &self.base().task_id
    }

    pub fn description(&self) -> &str {
        &self.base().description
    }

    pub fn status(&self) -> AgentTaskStatus {
        self.base().status
    }

    pub fn detached(&self) -> Option<bool> {
        self.base().detached
    }

    pub fn started_at(&self) -> f64 {
        self.base().started_at
    }

    pub fn ended_at(&self) -> Option<f64> {
        self.base().ended_at
    }

    pub fn stop_reason(&self) -> Option<&str> {
        self.base().stop_reason.as_deref()
    }

    pub fn timeout_ms(&self) -> Option<f64> {
        self.base().timeout_ms
    }

    pub fn output_path(&self) -> Option<&str> {
        self.base().output_path.as_deref()
    }

    pub fn terminal_notification_suppressed(&self) -> Option<bool> {
        self.base().terminal_notification_suppressed
    }

    pub fn resume_reminded(&self) -> Option<bool> {
        self.base().resume_reminded
    }

    pub fn agent_id(&self) -> Option<&str> {
        match self {
            Self::Agent(info) => info.agent_id.as_deref(),
            _ => None,
        }
    }

    pub fn set_status(&mut self, status: AgentTaskStatus) {
        self.base_mut().status = status;
    }

    pub fn set_ended_at(&mut self, ended_at: Option<f64>) {
        self.base_mut().ended_at = ended_at;
    }

    pub fn set_terminal_notification_suppressed(&mut self, suppressed: bool) {
        self.base_mut().terminal_notification_suppressed = Some(suppressed);
    }

    pub fn set_resume_reminded(&mut self, reminded: bool) {
        self.base_mut().resume_reminded = Some(reminded);
    }
}

#[derive(Clone)]
pub struct AgentTaskSink {
    pub signal: AbortSignal,
    append_output_fn: Arc<dyn Fn(String) + Send + Sync>,
    settle_fn: Arc<dyn Fn(AgentTaskSettlement) -> BoxFuture<'static, bool> + Send + Sync>,
}

impl AgentTaskSink {
    pub fn new(
        signal: AbortSignal,
        append_output: Arc<dyn Fn(String) + Send + Sync>,
        settle: Arc<dyn Fn(AgentTaskSettlement) -> BoxFuture<'static, bool> + Send + Sync>,
    ) -> Self {
        Self {
            signal,
            append_output_fn: append_output,
            settle_fn: settle,
        }
    }

    pub fn append_output(&self, chunk: String) {
        (self.append_output_fn)(chunk);
    }

    pub fn settle(&self, settlement: AgentTaskSettlement) -> BoxFuture<'static, bool> {
        (self.settle_fn)(settlement)
    }
}

#[async_trait]
pub trait AgentTask: Send + Sync {
    fn id_prefix(&self) -> &str;
    fn kind(&self) -> AgentTaskKind;
    fn description(&self) -> &str;
    fn timeout_ms(&self) -> Option<u64> {
        None
    }
    async fn start(&self, sink: AgentTaskSink) -> Result<(), WireAsyncError>;
    fn on_detach(&self) {}
    async fn force_stop(&self) -> Result<(), WireAsyncError> {
        Ok(())
    }
    fn to_info(&self, base: AgentTaskInfoBase) -> AgentTaskInfo;
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskInfoBase {
    pub task_id: String,
    pub description: String,
    pub status: AgentTaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detached: Option<bool>,
    pub started_at: f64,
    pub ended_at: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_notification_suppressed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum TaskInfo {
    #[serde(rename_all = "camelCase")]
    Process {
        #[serde(flatten)]
        base: TaskInfoBase,
        command: String,
        pid: u32,
        exit_code: Option<i32>,
    },
    #[serde(rename_all = "camelCase")]
    Agent {
        #[serde(flatten)]
        base: TaskInfoBase,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subagent_type: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thinking_effort: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Question {
        #[serde(flatten)]
        base: TaskInfoBase,
        question_count: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskStartedEvent {
    #[serde(rename = "type")]
    pub type_: String,
    pub info: TaskInfo,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskTerminatedEvent {
    #[serde(rename = "type")]
    pub type_: String,
    pub info: TaskInfo,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackgroundTaskStartedEvent {
    #[serde(rename = "type")]
    pub type_: String,
    pub info: TaskInfo,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackgroundTaskTerminatedEvent {
    #[serde(rename = "type")]
    pub type_: String,
    pub info: TaskInfo,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundTaskOutputEvent {
    #[serde(rename = "type")]
    pub type_: String,
    pub task_id: String,
    pub chunk: String,
    pub output_size_bytes: u64,
}
