use serde::{Deserialize, Serialize};

pub use super::errors::ProfileErrors;

use crate::app::agent_profile_catalog::agent_profile_catalog::{
    AgentProfile, AgentProfileContext, EnvironmentDisclosureSnapshot,
};
use crate::llm_adapter::contract::capability::ModelCapability;
use crate::llm_adapter::model::model_requester::ModelRequestParams;
use crate::rpc::types::BoxFuture;

pub const MODEL_NOT_CONFIGURED: &str = "model.not_configured";
pub const MODEL_CONFIG_INVALID: &str = "model.config_invalid";
pub const THINKING_ALIAS_CONFLICT: &str = "profile.thinking_alias_conflict";
pub const PROFILE_UNKNOWN: &str = "profile.unknown";
pub const PROFILE_ALREADY_BOUND: &str = "profile.already_bound";
pub const PROFILE_NOT_BOUND: &str = "profile.not_bound";

pub type ProfileErrorCode = &'static str;

#[derive(Debug, Clone, PartialEq)]
pub struct ProfileError {
    pub code: ProfileErrorCode,
    pub message: String,
    pub details: Option<serde_json::Value>,
}

impl ProfileError {
    pub fn new(
        code: ProfileErrorCode,
        message: impl Into<String>,
        details: Option<serde_json::Value>,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            details,
        }
    }
}

impl std::fmt::Display for ProfileError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ProfileError {}

pub type ThinkingEffort = String;

pub type ResolvedAgentProfile = AgentProfile;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentConfigData {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_alias: Option<String>,
    pub model_capabilities: ModelCapability,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_name: Option<String>,
    pub thinking_level: String,
    pub system_prompt: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentConfigUpdateData {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_alias: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemPromptContext {
    #[serde(flatten)]
    pub base: AgentProfileContext,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents_md_warning: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents_md_paths: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileData {
    #[serde(flatten)]
    pub base: AgentConfigData,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents_md_paths: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_tool_names: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disallowed_tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagents: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_disclosure: Option<EnvironmentDisclosureSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render_generation: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileUpdateData {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_alias: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_disclosure: Option<EnvironmentDisclosureSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents_md_paths: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disallowed_tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_tool_names: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileBindingSnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_alias: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_name: Option<String>,
    pub thinking_level: String,
    pub system_prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_disclosure: Option<EnvironmentDisclosureSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render_generation: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents_md_paths: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_tool_names: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disallowed_tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagents: Option<Vec<String>>,
}

pub struct ProfileServiceOptions {
    pub emit_status_updated: Option<Box<dyn Fn() + Send + Sync>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyProfileOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additional_dirs: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileModelContext {
    pub model_alias: String,
    pub model_capabilities: ModelCapability,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_size: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub always_thinking: Option<bool>,
    pub thinking_level: ThinkingEffort,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserved_context_size: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_trigger_ratio: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_max_attempts: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileSetModelResult {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BindAgentInput {
    pub profile: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict_thinking: Option<bool>,
}

pub trait AgentProfileService: Send + Sync {
    fn register(&self);

    fn configure(&self, options: ProfileServiceOptions);

    fn update(&self, changed: ProfileUpdateData);

    fn apply_binding_snapshot(&self, snapshot: ProfileBindingSnapshot);

    fn bind(&self, input: BindAgentInput) -> BoxFuture<'static, Result<(), ProfileError>>;

    fn set_model(
        &self,
        model: &str,
    ) -> BoxFuture<'static, Result<ProfileSetModelResult, ProfileError>>;

    fn set_thinking(&self, level: &str);

    fn republish_status(&self);

    fn get_model(&self) -> String;

    fn use_profile(&self, profile: &ResolvedAgentProfile, context: SystemPromptContext);

    fn apply_profile(
        &self,
        profile: &ResolvedAgentProfile,
        options: Option<ApplyProfileOptions>,
    ) -> BoxFuture<'static, Result<(), ProfileError>>;

    fn get_agents_md_warning(&self) -> Option<String>;

    fn data(&self) -> ProfileData;

    fn get_effective_thinking_level(&self) -> ThinkingEffort;

    fn resolve_model_context(&self) -> ProfileModelContext;

    fn resolve_request_params(&self) -> ModelRequestParams;

    fn get_model_capabilities(&self) -> ModelCapability;

    fn get_model_wire_name(&self) -> Option<String>;

    fn get_model_provider_type(&self, alias: Option<&str>) -> Option<String>;

    fn get_max_output_size(&self) -> Option<u32>;

    fn has_model(&self) -> bool;

    fn is_runnable(&self) -> bool;

    fn has_provider(&self) -> bool;

    fn get_system_prompt(&self) -> String;

    fn get_active_tool_names(&self) -> Option<Vec<String>>;

    fn add_active_tool(&self, name: &str);

    fn remove_active_tool(&self, name: &str);

    fn refresh_active_tools(&self) -> BoxFuture<'static, Result<(), ProfileError>>;
}

pub use self::AgentProfileService as IAgentProfileService;
