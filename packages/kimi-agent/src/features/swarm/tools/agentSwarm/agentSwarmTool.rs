use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use kimi_core::primitives::errors::domains::core::VALIDATION_FAILED;
use kimi_core::primitives::errors::Error2;
use serde_json::{Map, Value};

use super::super::super::configSection::resolve_swarm_timeout_ms;
use super::super::super::agent::swarmService::{AgentSwarmService, SWARM_MODE_OFF_MESSAGE};
use super::super::super::session::agentRunBatch::{AgentRunState, AgentRunStatus};
use super::super::super::session::sessionSwarmService::{
    SessionSwarmRunArgs, SessionSwarmService, SessionSwarmTask, SessionSwarmTaskKind,
};
use super::agentSwarm::{
    AgentSwarmToolInput, MAX_AGENT_SWARM_SUBAGENTS, PROMPT_TEMPLATE_PLACEHOLDER,
    agent_swarm_input_schema,
};
use crate::_base::utils::abort::AbortSignal;
use crate::agent::profile::profile::IAgentProfileService;
use crate::agent::scope_context::scopeContext::IAgentScopeContext;
use crate::agent::tool_executor::tool_executor::{
    ExecutableTool, ExecutableToolResult, RunnableToolExecution, ToolExecution,
};
use crate::app::config::config::IConfigService;
use crate::app::flag::flag::IFlagService;
use crate::session::subagent::configSection::{
    build_subagent_model_summary, exposes_subagent_model_choice,
    strip_subagent_fork_parameter, strip_subagent_model_parameter,
};
use crate::session::subagent::flag::SUBAGENT_FORK_FLAG_ID;
use crate::subagent::fork::{
    FORK_EXPERIMENTAL_UNAVAILABLE, FORK_WITH_RESUME_UNAVAILABLE, fork_incompatibility,
};
use crate::session::subagent::spawn::{SubagentSpawnPlan, SubagentSpawnPlanInput};
use crate::session::subagent::subagent::ISessionSubagentService;
use crate::tool::input_schema::to_input_json_schema;
use crate::tool::tool_contract::ToolAccesses;
use crate::tool::tool_input_display::ToolInputDisplay;
use crate::wire::record::WireAsyncError;

pub const AGENT_SWARM_TOOL_NAME: &str = "AgentSwarm";
pub const DEFAULT_SUBAGENT_TYPE: &str = "coder";
pub const AGENT_SWARM_FORK_DESCRIPTION: &str = include_str!("agent-swarm-fork.md");
pub const AGENT_SWARM_DESCRIPTION: &str = include_str!("agent-swarm.md");

#[derive(Clone)]
pub struct AgentSwarmTool {
    swarm_service: Arc<SessionSwarmService>,
    scope_context: Arc<dyn IAgentScopeContext>,
    swarm_mode: Arc<AgentSwarmService>,
    config: Arc<dyn IConfigService>,
    flags: Arc<dyn IFlagService>,
    subagents: Arc<dyn ISessionSubagentService>,
    profile: Arc<dyn IAgentProfileService>,
    caller_agent_id: String,
    description: String,
}

impl AgentSwarmTool {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        swarm_service: Arc<SessionSwarmService>,
        scope_context: Arc<dyn IAgentScopeContext>,
        swarm_mode: Arc<AgentSwarmService>,
        config: Arc<dyn IConfigService>,
        flags: Arc<dyn IFlagService>,
        subagents: Arc<dyn ISessionSubagentService>,
        profile: Arc<dyn IAgentProfileService>,
    ) -> Self {
        let caller_agent_id = scope_context.agent_id();
        let mut description = AGENT_SWARM_DESCRIPTION.to_string();
        if flags.enabled(SUBAGENT_FORK_FLAG_ID) {
            description.push_str("\n\n");
            description.push_str(AGENT_SWARM_FORK_DESCRIPTION);
        }
        if let Some(model_lines) = build_subagent_model_summary(config.as_ref()) {
            description.push_str("\n\n");
            description.push_str(&model_lines);
        }
        Self {
            swarm_service,
            scope_context,
            swarm_mode,
            config,
            flags,
            subagents,
            profile,
            caller_agent_id,
            description,
        }
    }

    pub fn name(&self) -> &'static str {
        AGENT_SWARM_TOOL_NAME
    }

    pub fn side_effect(&self) -> &'static str {
        "read"
    }

    pub fn repo_mutation(&self) -> &'static str {
        "none"
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn parameters(&self) -> Map<String, Value> {
        let mut parameters = to_input_json_schema(agent_swarm_input_schema())
            .as_object()
            .cloned()
            .expect("AgentSwarm input schema is an object");
        if !exposes_subagent_model_choice(self.config.as_ref()) {
            parameters = strip_subagent_model_parameter(&parameters);
        }
        if !self.flags.enabled(SUBAGENT_FORK_FLAG_ID) {
            parameters = strip_subagent_fork_parameter(&parameters);
        }
        parameters
    }

    pub fn resolve_execution_value(&self, input: Value) -> ToolExecution {
        match serde_json::from_value::<AgentSwarmToolInput>(input) {
            Ok(args) => self.resolve_execution(&args),
            Err(error) => ToolExecution::Error(ExecutableToolResult::error(format!(
                "Invalid arguments for AgentSwarm: {error}"
            ))),
        }
    }

    pub fn resolve_execution(&self, args: &AgentSwarmToolInput) -> ToolExecution {
        if !self.swarm_mode.is_active() {
            return ToolExecution::Error(ExecutableToolResult::error(SWARM_MODE_OFF_MESSAGE));
        }
        let agent_count = args.items.as_ref().map(Vec::len).unwrap_or(0)
            + args
                .resume_agent_ids
                .as_ref()
                .map(|entries| entries.len())
                .unwrap_or(0);
        let tool = self.clone();
        let args = args.clone();
        ToolExecution::Runnable(RunnableToolExecution {
            accesses: Some(ToolAccesses::all()),
            display: Some(ToolInputDisplay::AgentCall {
                agent_name: format!("swarm ({agent_count} subagents)"),
                prompt: args.description.clone(),
                background: None,
            }),
            description: Some(format!("Launching agent swarm: {}", args.description)),
            stop_batch_after_this: None,
            approval_rule: AGENT_SWARM_TOOL_NAME.to_string(),
            matches_rule: None,
            execute: Arc::new(move |context| {
                let tool = tool.clone();
                let args = args.clone();
                Box::pin(async move { tool.execution(&args, &context).await })
            }),
        })
    }

    async fn execution(
        &self,
        args: &AgentSwarmToolInput,
        context: &crate::agent::tool_executor::tool_executor::ExecutableToolContext,
    ) -> Result<ExecutableToolResult, WireAsyncError> {
        if !self.swarm_mode.is_active() {
            return Ok(ExecutableToolResult::error(SWARM_MODE_OFF_MESSAGE));
        }
        let signal = context.signal.clone();
        let tool_call_id = context.tool_call_id.clone();
        match self.run_swarm(args, signal, &tool_call_id).await {
            Ok(output) => Ok(ExecutableToolResult {
                output: output.into(),
                is_error: Some(false),
                ..Default::default()
            }),
            Err(error) => Ok(ExecutableToolResult::error(error.message().to_string())),
        }
    }

    async fn run_swarm(
        &self,
        args: &AgentSwarmToolInput,
        signal: AbortSignal,
        tool_call_id: &str,
    ) -> Result<String, Error2> {
        let fork = args.fork == Some(true);
        if fork && !self.flags.enabled(SUBAGENT_FORK_FLAG_ID) {
            return Err(validation(FORK_EXPERIMENTAL_UNAVAILABLE));
        }
        if fork
            && args
                .resume_agent_ids
                .as_ref()
                .is_some_and(|entries| !entries.is_empty())
        {
            return Err(validation(FORK_WITH_RESUME_UNAVAILABLE));
        }

        let item_count = args.items.as_ref().map(Vec::len).unwrap_or(0);
        let mut plan: Option<SubagentSpawnPlan> = None;
        if item_count > 0 {
            if fork {
                let profile = self.profile.data();
                let caller_profile = profile.base.profile_name.as_deref().unwrap_or(DEFAULT_SUBAGENT_TYPE);
                let caller_model = profile.base.model_alias.clone();
                if let Some(incompatible) = fork_incompatibility(
                    None,
                    args.subagent_type.as_deref(),
                    args.model.as_deref(),
                    caller_profile,
                    caller_model.as_deref(),
                ) {
                    return Err(validation(incompatible));
                }
            }
            plan = Some(
                self.subagents
                    .plan_spawn(SubagentSpawnPlanInput {
                        caller_agent_id: self.caller_agent_id.clone(),
                        profile_name: args.subagent_type.clone(),
                        model: args.model.clone(),
                        fork: Some(fork),
                    })
                    .await
                    .map_err(|error| {
                        Error2::new(VALIDATION_FAILED, error.to_string())
                    })?,
            );
        }
        let profile_name = plan
            .as_ref()
            .map(|plan| plan.profile_name.clone())
            .unwrap_or_else(|| DEFAULT_SUBAGENT_TYPE.to_string());
        let timeout_ms = resolve_swarm_timeout_ms(self.config.as_ref());

        let specs = self.create_specs(args).await?;
        let tasks: Vec<SessionSwarmTask<AgentSwarmSpec>> = specs
            .iter()
            .map(|spec| {
                let description_name = if matches!(spec, AgentSwarmSpec::Resume { .. }) {
                    "resume".to_string()
                } else {
                    profile_name.clone()
                };
                let task_profile_name = if matches!(spec, AgentSwarmSpec::Resume { .. }) {
                    "subagent".to_string()
                } else {
                    profile_name.clone()
                };
                let kind = match spec {
                    AgentSwarmSpec::Resume { agent_id, .. } => SessionSwarmTaskKind::Resume {
                        resume_agent_id: agent_id.clone(),
                    },
                    AgentSwarmSpec::Spawn { .. } => SessionSwarmTaskKind::Spawn {
                        plan: plan.clone().expect("spawn specs have a plan"),
                    },
                };
                SessionSwarmTask {
                    data: spec.clone(),
                    kind,
                    profile_name: task_profile_name,
                    parent_tool_call_id: tool_call_id.to_string(),
                    parent_tool_call_uuid: None,
                    prompt: spec.prompt().to_string(),
                    description: format!(
                        "{} #{} ({})",
                        args.description,
                        spec.index(),
                        description_name
                    ),
                    swarm_index: Some(spec.index()),
                    swarm_item: spec.item().map(str::to_string),
                    run_in_background: false,
                    timeout: Some(Duration::from_millis(timeout_ms)),
                    signal: Some(signal.clone()),
                }
            })
            .collect();

        let results = self
            .swarm_service
            .run(SessionSwarmRunArgs {
                caller_agent_id: self.caller_agent_id.clone(),
                tasks,
            })
            .await
            .map_err(|error| Error2::new(VALIDATION_FAILED, error.to_string()))?;

        let rendered: Vec<SwarmRunResult> = results
            .into_iter()
            .map(|result| SwarmRunResult {
                spec: result.task.data,
                agent_id: result.agent_id,
                status: result.status,
                state: result.state,
                result: result.result,
                stop_reason: result.stop_reason,
                error: result.error,
            })
            .collect();
        Ok(render_swarm_results(&rendered))
    }

    async fn create_specs(
        &self,
        args: &AgentSwarmToolInput,
    ) -> Result<Vec<AgentSwarmSpec>, Error2> {
        let resume_entries: Vec<(String, String)> = args
            .resume_agent_ids
            .as_ref()
            .map(|entries| {
                entries
                    .iter()
                    .map(|(agent_id, prompt)| (agent_id.trim().to_string(), prompt.trim().to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let items: Vec<String> = args
            .items
            .as_ref()
            .map(|items| items.iter().map(|item| item.trim().to_string()).collect())
            .unwrap_or_default();
        let item_count = items.len();
        let resume_count = resume_entries.len();
        if resume_count == 0 && item_count < 2 {
            return Err(validation(
                "AgentSwarm requires at least 2 items unless resume_agent_ids is provided.",
            ));
        }
        let total_count = resume_count + item_count;
        if total_count > MAX_AGENT_SWARM_SUBAGENTS {
            return Err(validation(format!(
                "AgentSwarm supports at most {MAX_AGENT_SWARM_SUBAGENTS} subagents."
            )));
        }
        let prompt_template = normalize_optional_string(args.prompt_template.as_deref());
        if item_count > 0 && prompt_template.is_none() {
            return Err(validation(
                "prompt_template is required when items are provided.",
            ));
        }
        if let Some(template) = prompt_template
            && !template.contains(PROMPT_TEMPLATE_PLACEHOLDER)
        {
            return Err(validation(format!(
                "prompt_template must include the {PROMPT_TEMPLATE_PLACEHOLDER} placeholder."
            )));
        }

        let mut seen_prompts: HashMap<String, usize> = HashMap::new();
        let mut specs: Vec<AgentSwarmSpec> = Vec::new();
        for (agent_id, prompt) in resume_entries {
            let item = self
                .swarm_service
                .get_swarm_item(&self.caller_agent_id, &agent_id)
                .await
                .map_err(|error| Error2::new(VALIDATION_FAILED, error.to_string()))?;
            specs.push(AgentSwarmSpec::Resume {
                index: specs.len() + 1,
                agent_id,
                item,
                prompt,
            });
        }
        if let Some(template) = prompt_template {
            for (index, item) in items.into_iter().enumerate() {
                let prompt = template.replace(PROMPT_TEMPLATE_PLACEHOLDER, &item);
                if let Some(previous_index) = seen_prompts.get(&prompt) {
                    return Err(validation(format!(
                        "Duplicate subagent prompts from items {previous_index} and {}. AgentSwarm requires distinct subagents.",
                        index + 1
                    )));
                }
                seen_prompts.insert(prompt.clone(), index + 1);
                specs.push(AgentSwarmSpec::Spawn {
                    index: specs.len() + 1,
                    item,
                    prompt,
                });
            }
        }
        Ok(specs)
    }
}

#[derive(Clone)]
enum AgentSwarmSpec {
    Spawn {
        index: usize,
        item: String,
        prompt: String,
    },
    Resume {
        index: usize,
        agent_id: String,
        item: Option<String>,
        prompt: String,
    },
}

impl AgentSwarmSpec {
    fn index(&self) -> usize {
        match self {
            Self::Spawn { index, .. } | Self::Resume { index, .. } => *index,
        }
    }

    fn item(&self) -> Option<&str> {
        match self {
            Self::Spawn { item, .. } => Some(item),
            Self::Resume { item, .. } => item.as_deref(),
        }
    }

    fn prompt(&self) -> &str {
        match self {
            Self::Spawn { prompt, .. } | Self::Resume { prompt, .. } => prompt,
        }
    }
}

struct SwarmRunResult {
    spec: AgentSwarmSpec,
    agent_id: Option<String>,
    status: AgentRunStatus,
    state: Option<AgentRunState>,
    result: Option<String>,
    stop_reason: Option<String>,
    error: Option<String>,
}

fn render_swarm_results(results: &[SwarmRunResult]) -> String {
    let completed = results
        .iter()
        .filter(|result| result.status == AgentRunStatus::Completed)
        .count();
    let failed = results
        .iter()
        .filter(|result| result.status == AgentRunStatus::Failed)
        .count();
    let aborted = results
        .iter()
        .filter(|result| result.status == AgentRunStatus::Aborted)
        .count();
    let should_render_resume_hint = results
        .iter()
        .any(|result| result.status != AgentRunStatus::Completed || result.stop_reason.is_some())
        && results.iter().any(|result| result.agent_id.is_some());
    let mut lines = vec![
        "<agent_swarm_result>".to_string(),
        format!(
            "<summary>{}</summary>",
            render_swarm_summary(completed, failed, aborted)
        ),
    ];
    if should_render_resume_hint {
        lines.push(
            "<resume_hint>Call AgentSwarm with resume_agent_ids using the agent_id values in this result to continue unfinished work.</resume_hint>".to_string(),
        );
    }
    for result in results {
        let agent_id = result
            .agent_id
            .as_ref()
            .map(|agent_id| format!(" agent_id=\"{agent_id}\""))
            .unwrap_or_default();
        let mode = match result.spec {
            AgentSwarmSpec::Resume { .. } => " mode=\"resume\"",
            AgentSwarmSpec::Spawn { .. } => "",
        };
        let item = result
            .spec
            .item()
            .map(|item| format!(" item=\"{}\"", escape_xml_attribute(item)))
            .unwrap_or_default();
        let state = result
            .state
            .map(|state| format!(" state=\"{}\"", run_state_name(state)))
            .unwrap_or_default();
        let stop_reason = result
            .stop_reason
            .as_ref()
            .map(|reason| format!(" stop_reason=\"{}\"", escape_xml_attribute(reason)))
            .unwrap_or_default();
        let body = match result.status {
            AgentRunStatus::Completed => result.result.as_deref().unwrap_or("").to_string(),
            _ => result
                .error
                .as_deref()
                .unwrap_or("unknown error")
                .to_string(),
        };
        lines.push(format!(
            "<subagent{mode}{agent_id}{item}{state} outcome=\"{}\"{stop_reason}>{body}</subagent>",
            run_status_name(result.status)
        ));
    }
    lines.push("</agent_swarm_result>".to_string());
    lines.join("\n")
}

fn render_swarm_summary(completed: usize, failed: usize, aborted: usize) -> String {
    let mut parts = Vec::new();
    if completed > 0 {
        parts.push(format!("completed: {completed}"));
    }
    if failed > 0 {
        parts.push(format!("failed: {failed}"));
    }
    if aborted > 0 {
        parts.push(format!("aborted: {aborted}"));
    }
    parts.join(", ")
}

fn run_status_name(status: AgentRunStatus) -> &'static str {
    match status {
        AgentRunStatus::Completed => "completed",
        AgentRunStatus::Failed => "failed",
        AgentRunStatus::Aborted => "aborted",
    }
}

fn run_state_name(state: AgentRunState) -> &'static str {
    match state {
        AgentRunState::Started => "started",
        AgentRunState::NotStarted => "not_started",
    }
}

fn normalize_optional_string(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn escape_xml_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn validation(message: impl Into<String>) -> Error2 {
    Error2::new(VALIDATION_FAILED, message)
}

impl ExecutableTool for AgentSwarmTool {
    fn name(&self) -> &str {
        AgentSwarmTool::name(self)
    }

    fn description(&self) -> &str {
        AgentSwarmTool::description(self)
    }

    fn parameters(&self) -> Map<String, Value> {
        AgentSwarmTool::parameters(self)
    }

    fn resolve_execution(
        &self,
        input: Value,
    ) -> BoxFuture<'static, Result<ToolExecution, WireAsyncError>> {
        let execution = self.resolve_execution_value(input);
        Box::pin(async move { Ok(execution) })
    }
}
