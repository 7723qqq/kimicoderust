use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::errors::{ErrorEvent, WarningEvent};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultEvent {
    #[serde(rename = "type")]
    pub type_: String,
    pub turn_id: u64,
    pub tool_call_id: String,
    pub output: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synthetic: Option<bool>,
}

impl ToolResultEvent {
    pub fn new(turn_id: u64, tool_call_id: String, output: Value) -> Self {
        Self {
            type_: "tool.result".to_string(),
            turn_id,
            tool_call_id,
            output,
            is_error: None,
            synthetic: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AgentEvent {
    Error(ErrorEvent),
    Warning(WarningEvent),
    AgentStatusUpdated(crate::agent::usage::usage_events::AgentStatusUpdatedEvent),
    SessionMetaUpdated(crate::session::sessionMetadata::session_meta_events::SessionMetaUpdatedEvent),
    SessionCreated(crate::app::session_legacy::session_protocol::SessionCreatedEvent),
    WorkspaceCreated(crate::app::workspace::workspace_protocol::WorkspaceCreatedEvent),
    WorkspaceUpdated(crate::app::workspace::workspace_protocol::WorkspaceUpdatedEvent),
    WorkspaceDeleted(crate::app::workspace::workspace_protocol::WorkspaceDeletedEvent),
    SessionWorkChanged(crate::app::session_legacy::session_protocol::SessionWorkChangedEvent),
    SessionStatusChanged(crate::app::session_legacy::session_protocol::SessionStatusChangedEvent),
    ConfigChanged(crate::app::config::config_events::ConfigChangedEvent),
    ConfigWarning(crate::app::config::config_events::ConfigWarningEvent),
    ModelCatalogChanged(crate::app::kosong_config::discovery::ModelCatalogChangedEvent),
    PluginChanged(crate::app::plugin::plugin_events::PluginChangedEvent),
    CapabilityChanged(crate::app::capability::capability_events::CapabilityChangedEvent),
    GoalUpdated(crate::features::goal::goalOps::GoalUpdatedEvent),
    SkillActivated(crate::features::skill::skillOps::SkillActivatedEvent),
    PluginCommandActivated(crate::agent::plugin_command::plugin_command::PluginCommandActivatedEvent),
    EnvironmentStatusChanged(crate::agent::environment_binding::environment_events::EnvironmentStatusChangedEvent),
    TodoUpdated(crate::features::todo::todoOps::TodoUpdatedEvent),
    TurnStarted(crate::agent::r#loop::turn_events::TurnStartedEvent),
    TurnEnded(crate::agent::r#loop::turn_ops::TurnEndedEvent),
    TurnStepStarted(crate::agent::r#loop::turn_events::TurnStepStartedEvent),
    TurnStepCompleted(crate::agent::r#loop::turn_events::TurnStepCompletedEvent),
    TurnStepRetrying(crate::agent::r#loop::turn_events::TurnStepRetryingEvent),
    TurnStepInterrupted(crate::agent::r#loop::turn_events::TurnStepInterruptedEvent),
    AssistantDelta(crate::agent::r#loop::turn_events::AssistantDeltaEvent),
    HookResult(crate::features::externalHooks::agent::agentExternalHooksService::HookResultEvent),
    ThinkingDelta(crate::agent::r#loop::turn_events::ThinkingDeltaEvent),
    ToolCallDelta(crate::agent::tool_executor::tool_executor_events::ToolCallDeltaEvent),
    ToolCallStarted(crate::agent::tool_executor::tool_executor_events::ToolCallStartedEvent),
    ToolProgress(crate::agent::tool_executor::tool_executor_events::ToolProgressEvent),
    ShellOutput(crate::agent::tool_executor::tool_executor_events::ShellOutputEvent),
    ShellStarted(crate::agent::tool_executor::tool_executor_events::ShellStartedEvent),
    ShellCompleted(crate::agent::tool_executor::tool_executor_events::ShellCompletedEvent),
    ToolResult(ToolResultEvent),
    ToolListUpdated(crate::agent::tool_executor::tool_executor_events::ToolListUpdatedEvent),
    McpServerStatus(crate::agent::tool_executor::tool_executor_events::McpServerStatusEvent),
    SubagentSpawned(crate::session::subagent::mirror_agent_run::SubagentSpawnedEvent),
    SubagentStarted(crate::session::subagent::mirror_agent_run::SubagentStartedEvent),
    SubagentSuspended(crate::features::swarm::session::session_swarm_service::SubagentSuspendedEvent),
    SubagentCompleted(crate::session::subagent::mirror_agent_run::SubagentCompletedEvent),
    SubagentFailed(crate::session::subagent::mirror_agent_run::SubagentFailedEvent),
    SubagentCancelled(crate::session::subagent::mirror_agent_run::SubagentCancelledEvent),
    CompactionStarted(crate::agent::full_compaction::compaction_ops::CompactionStartedEvent),
    CompactionBlocked(crate::agent::full_compaction::compaction_ops::CompactionBlockedEvent),
    CompactionCancelled(crate::agent::full_compaction::compaction_ops::CompactionCancelledEvent),
    CompactionCompleted(crate::agent::full_compaction::compaction_ops::CompactionCompletedEvent),
    TaskStarted(crate::agent::task::types::TaskStartedEvent),
    TaskTerminated(crate::agent::task::types::TaskTerminatedEvent),
    BackgroundTaskStarted(crate::agent::task::types::BackgroundTaskStartedEvent),
    BackgroundTaskTerminated(crate::agent::task::types::BackgroundTaskTerminatedEvent),
    BackgroundTaskOutput(crate::agent::task::types::BackgroundTaskOutputEvent),
    CronFired(crate::features::cron::cron_ops::CronFiredEvent),
    PromptSubmitted(crate::agent::prompt::prompt_events::PromptSubmittedEvent),
    PromptCompleted(crate::agent::prompt::prompt_events::PromptCompletedEvent),
    PromptAborted(crate::agent::prompt::prompt_events::PromptAbortedEvent),
    PromptSteered(crate::agent::prompt::prompt_events::PromptSteeredEvent),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub agent_id: String,
    pub session_id: String,
    #[serde(flatten)]
    pub event: AgentEvent,
}
