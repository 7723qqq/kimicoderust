use std::sync::Arc;

use async_trait::async_trait;
use kimi_core::primitives::errors::domains::agent_loop::TURN_AGENT_BUSY;
use kimi_core::primitives::errors::domains::prompt::REQUEST_INVALID;
use kimi_core::primitives::errors::domains::skill::{SKILL_NOT_FOUND, SKILL_TYPE_UNSUPPORTED};
use kimi_core::primitives::errors::{Error2, INTERNAL_CODE};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};

use crate::agent::r#loop::r#loop::{
    IAgentLoopService, LoopState, LoopSubmitOptions, PromptLaunchResult, PromptState, Turn,
};
use crate::app::event::event::IEventService;
use crate::app::event::event2::Event2;
use crate::app::telemetry::telemetry::ITelemetryService;
use crate::callbacks::MAIN_AGENT_ID;
use crate::human::agent::origin::{
    BundledSkillActivation, PromptFileAttachment, PromptOrigin, SkillSource as OriginSkillSource,
    skill_activation_part,
};
use crate::human::agent::turn::{UserEntry, UserMeta};
use crate::human::llm::message::{ContentPart, UserMessage};
use crate::session::sessionMetadata::promptMetadata::{PromptMetadataUpdateTarget, apply_prompt_metadata_update};
use crate::session::sessionMetadata::session_metadata::ISessionMetadata;
use crate::state::eventDispatcher::IEventDispatcher;

use super::catalog::types::{
    SkillCatalog, SkillDefinition, SkillSource, is_user_activatable_skill_type,
};
use super::prompt::{
    RenderSkillPromptInput, prompt_metadata_text_from_parts, prompt_metadata_text_from_skill,
    render_user_slash_skill_prompt,
};
use super::session::skillCatalog::ISessionSkillCatalog;
use super::skill::{PromptSkillActivation, PromptWithSkillsInput, PromptWithSkillsResult, SkillActivationInput};
use super::skillOps::{SKILL_ACTIVATED_TYPE, SkillActivatedPayload, SkillActivationTrigger};

pub use crate::skills::*;

#[derive(Debug, Clone)]
pub struct SkillActivationOrigin {
    pub activation_id: String,
    pub skill_name: String,
    pub trigger: SkillActivationTrigger,
    pub skill_type: Option<String>,
    pub skill_path: String,
    pub skill_source: SkillSource,
    pub skill_args: Option<String>,
    pub attachments: Option<Vec<Value>>,
    pub client_metadata: Option<Vec<Map<String, Value>>>,
}

#[derive(Debug, Clone, Default)]
pub struct ModelToolSkillContext {
    pub query_depth: u32,
    pub workspace_dir: Option<String>,
}

#[derive(Clone)]
struct ModelToolParent {
    turn_id: u64,
    query_depth: u32,
    workspace_dir: Option<String>,
}

pub struct AgentSkillServiceDeps {
    pub catalog: Arc<dyn ISessionSkillCatalog>,
    pub loop_service: Arc<dyn IAgentLoopService>,
    pub metadata: Arc<dyn ISessionMetadata>,
    pub event_service: Arc<dyn IEventService>,
    pub telemetry: Arc<dyn ITelemetryService>,
    pub dispatcher: Arc<dyn IEventDispatcher>,
    pub session_id: String,
    pub agent_id: String,
}

pub struct AgentSkillService {
    deps: AgentSkillServiceDeps,
    model_tool_parent: Mutex<Option<ModelToolParent>>,
    ambiguous_model_tool_turn_id: Mutex<Option<u64>>,
    self_weak: std::sync::Weak<Self>,
}

#[async_trait]
pub trait IAgentSkillService: Send + Sync {
    async fn activate(&self, input: SkillActivationInput) -> Result<PromptLaunchResult, Error2>;
    async fn prompt_with_skills(
        &self,
        input: PromptWithSkillsInput,
    ) -> Result<PromptWithSkillsResult, Error2>;
    async fn record_model_tool_activation(
        &self,
        origin: SkillActivationOrigin,
        query_depth: Option<u32>,
    ) -> Result<(), Error2>;
    fn model_tool_context(&self) -> ModelToolSkillContext;
}

impl AgentSkillService {
    pub fn new(deps: AgentSkillServiceDeps) -> Arc<Self> {
        Arc::new_cyclic(|weak| Self {
            deps,
            model_tool_parent: Mutex::new(None),
            ambiguous_model_tool_turn_id: Mutex::new(None),
            self_weak: weak.clone(),
        })
    }

    pub async fn activate(&self, input: SkillActivationInput) -> Result<PromptLaunchResult, Error2> {
        <Self as IAgentSkillService>::activate(self, input).await
    }

    pub async fn prompt_with_skills(
        &self,
        input: PromptWithSkillsInput,
    ) -> Result<PromptWithSkillsResult, Error2> {
        <Self as IAgentSkillService>::prompt_with_skills(self, input).await
    }

    pub fn model_tool_context(&self) -> ModelToolSkillContext {
        <Self as IAgentSkillService>::model_tool_context(self)
    }

    fn render_skill_prompt(&self, skill: &SkillDefinition, raw_args: &str) -> String {
        self.deps.catalog.catalog().render_skill_prompt(
            skill,
            raw_args,
            Some(&self.deps.session_id),
        )
    }

    fn skill_by_input(&self, name: &str, path: Option<&str>) -> Result<SkillDefinition, Error2> {
        let skill = self
            .deps
            .catalog
            .catalog()
            .get_skill(name, path)
            .ok_or_else(|| Error2::new(SKILL_NOT_FOUND, format!("Skill \"{name}\" was not found")))?;
        if !is_user_activatable_skill_type(skill.skill_type()) {
            return Err(Error2::new(
                SKILL_TYPE_UNSUPPORTED,
                format!("Skill \"{}\" cannot be activated by the user", skill.name),
            ));
        }
        Ok(skill)
    }

    fn origin_for(
        &self,
        skill: &SkillDefinition,
        args: Option<String>,
        attachments: Option<Vec<Value>>,
        client_metadata: Option<Vec<Map<String, Value>>>,
    ) -> SkillActivationOrigin {
        SkillActivationOrigin {
            activation_id: uuid::Uuid::new_v4().to_string(),
            skill_name: skill.name.clone(),
            trigger: SkillActivationTrigger::UserSlash,
            skill_type: skill.skill_type().map(str::to_owned),
            skill_path: skill.path.clone(),
            skill_source: skill.source,
            skill_args: args,
            attachments,
            client_metadata,
        }
    }

    fn loaded_prompt(&self, skill: &SkillDefinition, origin: &SkillActivationOrigin) -> String {
        let args = origin.skill_args.as_deref().unwrap_or("");
        let content = self.render_skill_prompt(skill, args);
        render_user_slash_skill_prompt(&RenderSkillPromptInput {
            skill_name: skill.name.clone(),
            skill_args: args.to_string(),
            skill_content: content,
            skill_source: Some(skill.source.as_str().to_string()),
            skill_dir: Some(skill.dir.clone()),
        })
    }

    async fn record_activation(
        &self,
        origin: &SkillActivationOrigin,
        input: Option<Vec<ContentPart>>,
    ) -> Result<Option<Arc<Turn>>, Error2> {
        self.deps
            .dispatcher
            .dispatch(Event2 {
                type_: SKILL_ACTIVATED_TYPE.to_string(),
                time: chrono::Utc::now().timestamp_millis(),
                payload: serde_json::to_value(SkillActivatedPayload {
                    agent_id: self.deps.agent_id.clone(),
                    activation_id: origin.activation_id.clone(),
                    skill_name: origin.skill_name.clone(),
                    trigger: origin.trigger,
                    skill_args: origin.skill_args.clone(),
                    skill_path: Some(origin.skill_path.clone()),
                    skill_source: serde_json::to_value(origin.skill_source).ok(),
                })
                .map_err(|error| Error2::new(INTERNAL_CODE, error.to_string()))?,
            })
            .await
            .map_err(|error| Error2::new(INTERNAL_CODE, error.to_string()))?;
        self.publish_activation(origin);
        let Some(input) = input else {
            return Ok(None);
        };
        let steer = self.deps.loop_service.snapshot().state == LoopState::Running;
        let prompt_origin = PromptOrigin {
            kind: "skill_activation".to_string(),
            in_turn: None,
            client_metadata: origin.client_metadata.clone(),
            skill_activations: None,
            attachments: parse_attachments(origin.attachments.clone()),
            ..Default::default()
        };
        let result = self.deps.loop_service.submit(
            UserEntry {
                message: UserMessage {
                    role: crate::human::llm::message::Role::User,
                    content: input,
                },
                meta: Some(UserMeta {
                    origin: Some(prompt_origin),
                    tracked: Some(!steer),
                    ..UserMeta::default()
                }),
            },
            Some(LoopSubmitOptions {
                steer_if_active: Some(steer),
                on_materialize: None,
            }),
        );
        let handle = self
            .deps
            .loop_service
            .prompt_handle(&result.id)
            .ok_or_else(|| Error2::new(INTERNAL_CODE, "skill activation prompt is missing"))?;
        Ok(handle.launched.clone().await)
    }

    fn record_activation_detached(&self, origin: SkillActivationOrigin) {
        let weak = self.self_weak.clone();
        tokio::spawn(async move {
            if let Some(service) = weak.upgrade() {
                let _ = service.record_activation(&origin, None).await;
            }
        });
    }

    fn publish_activation(&self, origin: &SkillActivationOrigin) {
        self.deps.telemetry.track2(
            "skill_invoked",
            Some(Map::from_iter([
                ("skill_name".to_string(), Value::String(origin.skill_name.clone())),
                ("trigger".to_string(), Value::String(trigger_name(origin.trigger).to_string())),
            ])),
        );
        if origin.skill_type.as_deref() == Some("flow") {
            self.deps.telemetry.track2(
                "flow_invoked",
                Some(Map::from_iter([(
                    "flow_name".to_string(),
                    Value::String(origin.skill_name.clone()),
                )])),
            );
        }
    }

    fn remember_model_tool_parent(
        &self,
        origin: &SkillActivationOrigin,
        turn_id: u64,
        resolved_query_depth: Option<u32>,
    ) {
        if *self.ambiguous_model_tool_turn_id.lock() == Some(turn_id) {
            return;
        }
        let parent = self
            .model_tool_parent
            .lock()
            .clone()
            .filter(|parent| parent.turn_id == turn_id);
        let workspace_dir = self
            .deps
            .catalog
            .catalog()
            .list_skills()
            .into_iter()
            .find(|skill| skill.path == origin.skill_path)
            .and_then(|skill| skill.workspace_dir);
        if origin.trigger == SkillActivationTrigger::ModelTool
            && parent.is_some()
            && parent.as_ref().and_then(|parent| parent.workspace_dir.clone()) != workspace_dir
        {
            *self.model_tool_parent.lock() = None;
            *self.ambiguous_model_tool_turn_id.lock() = Some(turn_id);
            return;
        }
        let query_depth = resolved_query_depth.unwrap_or_else(|| {
            if origin.trigger == SkillActivationTrigger::NestedSkill {
                parent.map_or(1, |parent| parent.query_depth + 1)
            } else {
                1
            }
        });
        *self.model_tool_parent.lock() = Some(ModelToolParent {
            turn_id,
            query_depth,
            workspace_dir,
        });
    }

    fn prepare_bundled(
        &self,
        input: &PromptSkillActivation,
    ) -> Result<(SkillActivationOrigin, ContentPart, BundledSkillActivation), Error2> {
        let skill = self.skill_by_input(&input.name, input.path.as_deref())?;
        let origin = self.origin_for(&skill, input.args.clone(), None, None);
        let text = self.loaded_prompt(&skill, &origin);
        let entry = BundledSkillActivation {
            activation_id: origin.activation_id.clone(),
            skill_name: origin.skill_name.clone(),
            skill_args: origin.skill_args.clone(),
            skill_type: origin.skill_type.clone(),
            skill_path: Some(origin.skill_path.clone()),
            skill_source: Some(origin_skill_source(origin.skill_source)),
        };
        Ok((origin, skill_activation_part(&text, &entry.activation_id), entry))
    }

    async fn apply_metadata(&self, text: Option<String>) -> Result<(), Error2> {
        if self.deps.agent_id != MAIN_AGENT_ID {
            return Ok(());
        }
        apply_prompt_metadata_update(
            &PromptMetadataUpdateTarget {
                metadata: self.deps.metadata.clone(),
                event_service: self.deps.event_service.clone(),
                session_id: self.deps.session_id.clone(),
            },
            text.as_deref(),
        )
        .await
        .map_err(|error| Error2::new(INTERNAL_CODE, error.to_string()))
    }
}

#[async_trait]
impl IAgentSkillService for AgentSkillService {
    async fn activate(&self, input: SkillActivationInput) -> Result<PromptLaunchResult, Error2> {
        self.deps.catalog.ready().await.map_err(|error| Error2::new(INTERNAL_CODE, error))?;
        let skill = self.skill_by_input(&input.name, input.path.as_deref())?;
        let origin = self.origin_for(
            &skill,
            input.args.clone(),
            input.attachments.clone(),
            input.client_metadata.clone(),
        );
        let loaded = self.loaded_prompt(&skill, &origin);
        let mut content = vec![ContentPart::Text {
            text: loaded,
            meta: None,
        }];
        if let Some(extra) = input.content {
            content.extend(parse_content_parts(extra)?);
        }
        let turn = self.record_activation(&origin, Some(content)).await?;
        if let Some(turn) = &turn
            && let Some(turn_id) = turn.id
        {
            self.remember_model_tool_parent(&origin, turn_id, None);
        }
        let Some(turn) = turn else {
            return Err(Error2::new(
                TURN_AGENT_BUSY,
                "Cannot activate skill while another turn is active",
            ));
        };
        turn.ready.clone().await;
        let turn_id = turn.id.ok_or_else(|| {
            Error2::new(INTERNAL_CODE, "Skill activation turn ended before it started")
        })?;
        self.apply_metadata(prompt_metadata_text_from_skill(&input)).await?;
        Ok(PromptLaunchResult { turn_id })
    }

    async fn prompt_with_skills(
        &self,
        input: PromptWithSkillsInput,
    ) -> Result<PromptWithSkillsResult, Error2> {
        if input.input.is_empty() {
            return Err(Error2::new(REQUEST_INVALID, "promptWithSkills requires a non-empty prompt"));
        }
        if input.skills.is_empty() {
            return Err(Error2::new(REQUEST_INVALID, "promptWithSkills requires at least one skill"));
        }
        self.deps.catalog.ready().await.map_err(|error| Error2::new(INTERNAL_CODE, error))?;
        let mut prepared = Vec::new();
        for activation in &input.skills {
            prepared.push(self.prepare_bundled(activation)?);
        }
        let user_input = parse_content_parts(input.input.clone())?;
        self.apply_metadata(prompt_metadata_text_from_parts(
            &user_input,
            input.client_metadata.as_deref(),
        ))
        .await?;
        for (origin, _, _) in &prepared {
            self.record_activation_detached(origin.clone());
        }
        let status = self.deps.loop_service.snapshot();
        let steerable = input.steer_if_active == Some(true) && status.active_turn_id.is_some();
        let mut parts: Vec<_> = prepared.iter().map(|(_, part, _)| part.clone()).collect();
        parts.extend(user_input);
        let entries: Vec<_> = prepared.iter().map(|(_, _, entry)| entry.clone()).collect();
        let result = self.deps.loop_service.submit(
            UserEntry {
                message: UserMessage {
                    role: crate::human::llm::message::Role::User,
                    content: parts,
                },
                meta: Some(UserMeta {
                    origin: Some(PromptOrigin {
                        kind: "user".to_string(),
                        in_turn: None,
                        client_metadata: input.client_metadata.clone(),
                        skill_activations: Some(entries),
                        attachments: parse_attachments(input.attachments.clone()),
                        ..Default::default()
                    }),
                    tracked: Some(true),
                    ..UserMeta::default()
                }),
            },
            Some(LoopSubmitOptions {
                steer_if_active: input.steer_if_active,
                on_materialize: None,
            }),
        );
        let handle = self
            .deps
            .loop_service
            .prompt_handle(&result.id)
            .ok_or_else(|| Error2::new(INTERNAL_CODE, "promptWithSkills prompt is missing"))?;
        if !steerable
            && (status.state == LoopState::Running || status.paused || !status.queue.is_empty())
        {
            if prepared.len() == 1 {
                let launched = handle.launched.clone();
                let origin = prepared[0].0.clone();
                let weak = self.self_weak.clone();
                tokio::spawn(async move {
                    if let Some(turn) = launched.await
                        && let Some(turn_id) = turn.id
                        && let Some(service) = weak.upgrade()
                    {
                        service.remember_model_tool_parent(&origin, turn_id, None);
                    }
                });
            }
            return Ok(PromptWithSkillsResult {
                turn_id: None,
                prompt_id: result.id,
                created_at: handle.created_at.clone(),
                state: "queued".to_string(),
                steered: Some(false),
            });
        }
        tokio::select! {
            _ = handle.launched.clone() => {}
            _ = handle.completion.clone() => {}
        }
        let turn = handle.launched.clone().await;
        if turn.is_none() && handle.state != PromptState::Blocked {
            return Err(Error2::new(INTERNAL_CODE, "promptWithSkills failed to launch a turn"));
        }
        if let Some(turn) = &turn
            && let Some(turn_id) = turn.id
        {
            if prepared.len() == 1 {
                self.remember_model_tool_parent(&prepared[0].0, turn_id, None);
            } else if self.model_tool_parent.lock().as_ref().is_some_and(|parent| parent.turn_id == turn_id) {
                *self.model_tool_parent.lock() = None;
            }
        }
        if let Some(turn) = &turn {
            turn.ready.clone().await;
        }
        let launched_turn_id = turn.as_ref().and_then(|turn| turn.id);
        Ok(PromptWithSkillsResult {
            turn_id: launched_turn_id,
            prompt_id: result.id,
            created_at: handle.created_at.clone(),
            state: if handle.state == PromptState::Blocked {
                "blocked".to_string()
            } else {
                "running".to_string()
            },
            steered: Some(steerable && turn_id_matches(turn.as_ref(), status.active_turn_id)),
        })
    }

    async fn record_model_tool_activation(
        &self,
        origin: SkillActivationOrigin,
        query_depth: Option<u32>,
    ) -> Result<(), Error2> {
        if let Some(turn_id) = self.deps.loop_service.snapshot().active_turn_id {
            self.remember_model_tool_parent(&origin, turn_id, query_depth);
        }
        self.record_activation_detached(origin);
        Ok(())
    }

    fn model_tool_context(&self) -> ModelToolSkillContext {
        let status = self.deps.loop_service.snapshot();
        if status.state != LoopState::Running {
            return ModelToolSkillContext::default();
        }
        let Some(active_turn_id) = status.active_turn_id else {
            return ModelToolSkillContext::default();
        };
        let parent = self.model_tool_parent.lock().clone();
        let Some(parent) = parent.filter(|parent| parent.turn_id == active_turn_id) else {
            return ModelToolSkillContext::default();
        };
        ModelToolSkillContext {
            query_depth: parent.query_depth,
            workspace_dir: parent.workspace_dir,
        }
    }
}

fn trigger_name(trigger: SkillActivationTrigger) -> &'static str {
    match trigger {
        SkillActivationTrigger::UserSlash => "user-slash",
        SkillActivationTrigger::ModelTool => "model-tool",
        SkillActivationTrigger::NestedSkill => "nested-skill",
    }
}

fn origin_skill_source(source: SkillSource) -> OriginSkillSource {
    match source {
        SkillSource::Project => OriginSkillSource::Project,
        SkillSource::User => OriginSkillSource::User,
        SkillSource::Extra => OriginSkillSource::Extra,
        SkillSource::Builtin => OriginSkillSource::Builtin,
    }
}

fn parse_content_parts(values: Vec<Value>) -> Result<Vec<ContentPart>, Error2> {
    serde_json::from_value(Value::Array(values)).map_err(|error| Error2::new(REQUEST_INVALID, error.to_string()))
}

fn parse_attachments(values: Option<Vec<Value>>) -> Option<Vec<PromptFileAttachment>> {
    values.and_then(|values| serde_json::from_value(Value::Array(values)).ok())
}

fn turn_id_matches(turn: Option<&Arc<Turn>>, active: Option<u64>) -> bool {
    turn.and_then(|turn| turn.id) == active && active.is_some()
}
