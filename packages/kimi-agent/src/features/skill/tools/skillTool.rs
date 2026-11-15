use std::sync::Arc;

use futures_util::future::BoxFuture;
use kimi_core::primitives::errors::{Error2, INTERNAL_CODE};
use serde_json::{Map, Value};

use crate::agent::tool_executor::tool_executor::{
    ExecutableTool, ExecutableToolResult, RunnableToolExecution, ToolExecution,
};
use crate::features::skill::catalog::types::{
    SkillCatalog, is_inline_skill_type, normalize_skill_name,
};
use crate::features::skill::prompt::{
    RenderSkillPromptInput, SkillPromptTrigger, render_model_tool_skill_prompt,
};
use crate::features::skill::session::skillCatalog::ISessionSkillCatalog;
use crate::features::skill::skillOps::SkillActivationTrigger;
use crate::features::skill::skillService::{IAgentSkillService, ModelToolSkillContext, SkillActivationOrigin};
use crate::rpc::types::ContentPart;
use crate::session::session_context::session_context::ISessionContext;
use crate::tool::rule_match::matches_glob_rule_subject;
use crate::tool::tool_input_display::ToolInputDisplay;
use crate::wire::record::WireAsyncError;

use super::skill::{MAX_SKILL_QUERY_DEPTH, SkillToolInput, nested_skill_too_deep_error, skill_tool_input_schema};

pub const SKILL_TOOL_NAME: &str = "Skill";

#[derive(Clone)]
pub struct SkillTool {
    catalog: Arc<dyn ISessionSkillCatalog>,
    skill: Arc<dyn IAgentSkillService>,
    session_context: Arc<dyn ISessionContext>,
    query_depth: u32,
    workspace_dir: Option<String>,
}

impl SkillTool {
    pub fn new(
        catalog: Arc<dyn ISessionSkillCatalog>,
        skill: Arc<dyn IAgentSkillService>,
        session_context: Arc<dyn ISessionContext>,
    ) -> Self {
        Self {
            catalog,
            skill,
            session_context,
            query_depth: 0,
            workspace_dir: None,
        }
    }

    pub fn name(&self) -> &'static str {
        SKILL_TOOL_NAME
    }

    pub fn side_effect(&self) -> &'static str {
        "read"
    }

    pub fn repo_mutation(&self) -> &'static str {
        "none"
    }

    pub fn description(&self) -> &'static str {
        include_str!("skill.md")
    }

    pub fn parameters(&self) -> Map<String, Value> {
        skill_tool_input_schema()
            .as_object()
            .cloned()
            .expect("Skill input schema is an object")
    }

    pub fn resolve_execution_value(&self, input: Value) -> ToolExecution {
        match serde_json::from_value::<SkillToolInput>(input) {
            Ok(args) => self.resolve_execution(&args),
            Err(error) => ToolExecution::Error(ExecutableToolResult::error(format!(
                "Invalid arguments for Skill: {error}"
            ))),
        }
    }

    pub fn resolve_execution(&self, args: &SkillToolInput) -> ToolExecution {
        let context = if self.query_depth == 0 && self.workspace_dir.is_none() {
            self.skill.model_tool_context()
        } else {
            ModelToolSkillContext {
                query_depth: self.query_depth,
                workspace_dir: self.workspace_dir.clone(),
            }
        };
        let skill_name = args.skill.clone();
        let rule_skill_name = args.skill.clone();
        let catalog = self.catalog.clone();
        let skill_service = self.skill.clone();
        let session_id = self.session_context.session_id();
        let call_args = args.clone();
        ToolExecution::Runnable(RunnableToolExecution {
            accesses: None,
            display: Some(ToolInputDisplay::SkillCall {
                skill_name: args.skill.clone(),
                args: args.args.clone(),
            }),
            description: Some(format!("Invoke skill {skill_name}")),
            stop_batch_after_this: None,
            approval_rule: SKILL_TOOL_NAME.to_string(),
            matches_rule: Some(Arc::new(move |rule_args| {
                matches_glob_rule_subject(rule_args, &rule_skill_name)
            })),
            execute: Arc::new(move |_context| {
                let catalog = catalog.clone();
                let skill_service = skill_service.clone();
                let args = call_args.clone();
                let session_id = session_id.clone();
                let query_depth = context.query_depth;
                let workspace_dir = context.workspace_dir.clone();
                Box::pin(async move {
                    execute_model_skill(
                        catalog.as_ref(),
                        skill_service.as_ref(),
                        &args,
                        query_depth,
                        &session_id,
                        workspace_dir.as_deref(),
                    )
                    .await
                })
            }),
        })
    }

    pub fn with_initial_query_depth(
        &self,
        initial_query_depth: u32,
        workspace_dir: Option<String>,
    ) -> SkillTool {
        let mut clone = self.clone();
        clone.query_depth = initial_query_depth;
        clone.workspace_dir = workspace_dir;
        clone
    }
}

impl ExecutableTool for SkillTool {
    fn name(&self) -> &str {
        SkillTool::name(self)
    }

    fn description(&self) -> &str {
        SkillTool::description(self)
    }

    fn parameters(&self) -> Map<String, Value> {
        SkillTool::parameters(self)
    }

    fn resolve_execution(&self, input: Value) -> BoxFuture<'static, Result<ToolExecution, WireAsyncError>> {
        let execution = self.resolve_execution_value(input);
        Box::pin(async move { Ok(execution) })
    }
}

pub async fn execute_model_skill(
    catalog: &dyn ISessionSkillCatalog,
    skill_service: &dyn IAgentSkillService,
    args: &SkillToolInput,
    query_depth: u32,
    session_id: &str,
    parent_workspace_dir: Option<&str>,
) -> Result<ExecutableToolResult, WireAsyncError> {
    let current_depth = query_depth;
    if current_depth >= MAX_SKILL_QUERY_DEPTH {
        return Err(Box::new(Error2::new(
            INTERNAL_CODE,
            nested_skill_too_deep_error(MAX_SKILL_QUERY_DEPTH, Some(&args.skill)),
        )));
    }

    catalog
        .ready()
        .await
        .map_err(|error| Box::new(Error2::new(INTERNAL_CODE, error)) as WireAsyncError)?;
    let skill_catalog = catalog.catalog();
    let skill = match (args.path.as_deref(), parent_workspace_dir) {
        (None, Some(parent_workspace_dir)) => skill_catalog
            .list_skills()
            .into_iter()
            .find(|candidate| {
                candidate.workspace_dir.as_deref() == Some(parent_workspace_dir)
                    && normalize_skill_name(&candidate.name) == normalize_skill_name(&args.skill)
            })
            .or_else(|| skill_catalog.get_skill(&args.skill, None)),
        (path, _) => skill_catalog.get_skill(&args.skill, path),
    };
    let Some(skill) = skill else {
        return Ok(error_result(format!(
            "Skill \"{}\" not found in the current skill listing.",
            args.skill
        )));
    };
    if skill.disable_model_invocation() == Some(true) {
        return Ok(error_result(format!(
            "Skill \"{}\" can only be triggered by the user (model invocation is disabled).",
            args.skill
        )));
    }
    if !is_inline_skill_type(skill.skill_type()) {
        return Ok(error_result(format!(
            "Skill \"{}\" is not an inline skill and cannot be invoked by the model in v1.",
            skill.name
        )));
    }

    let skill_args = args.args.clone().unwrap_or_default();
    let trigger = if current_depth > 0 {
        "nested-skill"
    } else {
        "model-tool"
    };
    let activation_trigger = if current_depth > 0 {
        SkillActivationTrigger::NestedSkill
    } else {
        SkillActivationTrigger::ModelTool
    };
    let origin = SkillActivationOrigin {
        activation_id: uuid::Uuid::new_v4().to_string(),
        skill_name: skill.name.clone(),
        trigger: activation_trigger,
        skill_type: skill.skill_type().map(str::to_owned),
        skill_path: skill.path.clone(),
        skill_source: skill.source,
        skill_args: (!skill_args.is_empty()).then(|| skill_args.clone()),
        attachments: None,
        client_metadata: None,
    };
    let skill_content = skill_catalog.render_skill_prompt(&skill, &skill_args, Some(session_id));
    let prompt = render_model_tool_skill_prompt(
        &RenderSkillPromptInput {
            skill_name: skill.name.clone(),
            skill_args: skill_args.clone(),
            skill_content,
            skill_source: Some(skill.source.as_str().to_string()),
            skill_dir: Some(skill.dir.clone()),
        },
        if current_depth > 0 {
            SkillPromptTrigger::NestedSkill
        } else {
            SkillPromptTrigger::ModelTool
        },
    );
    let mut delivery_origin = Map::new();
    delivery_origin.insert("kind".to_string(), Value::String("skill_activation".to_string()));
    delivery_origin.insert(
        "activationId".to_string(),
        Value::String(origin.activation_id.clone()),
    );
    delivery_origin.insert(
        "skillName".to_string(),
        Value::String(origin.skill_name.clone()),
    );
    if let Some(skill_args) = &origin.skill_args {
        delivery_origin.insert("skillArgs".to_string(), Value::String(skill_args.clone()));
    }
    delivery_origin.insert("trigger".to_string(), Value::String(trigger.to_string()));
    if let Some(skill_type) = &origin.skill_type {
        delivery_origin.insert("skillType".to_string(), Value::String(skill_type.clone()));
    }
    delivery_origin.insert(
        "skillPath".to_string(),
        Value::String(origin.skill_path.clone()),
    );
    delivery_origin.insert(
        "skillSource".to_string(),
        Value::String(origin.skill_source.as_str().to_string()),
    );

    skill_service
        .record_model_tool_activation(origin, Some(current_depth + 1))
        .await
        .map_err(|error| Box::new(error) as WireAsyncError)?;

    Ok(ExecutableToolResult {
        output: format!(
            "Skill \"{}\" loaded inline. Follow its instructions.",
            skill.name
        )
        .into(),
        delivery: Some(crate::tool::tool_contract::ToolDelivery {
            blocks: vec![ContentPart::Text {
                text: prompt,
                meta: None,
            }],
            origin: Some(Value::Object(delivery_origin)),
        }),
        ..Default::default()
    })
}

fn error_result(message: String) -> ExecutableToolResult {
    ExecutableToolResult {
        output: message.into(),
        is_error: Some(true),
        ..Default::default()
    }
}
