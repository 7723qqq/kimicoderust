use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const SKILL_ACTIVATED_TYPE: &str = "skill.activated";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SkillActivationTrigger {
    UserSlash,
    ModelTool,
    NestedSkill,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillActivatedPayload {
    pub agent_id: String,
    pub activation_id: String,
    pub skill_name: String,
    pub trigger: SkillActivationTrigger,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_args: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_source: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillActivatedEvent {
    pub r#type: String,
    pub activation_id: String,
    pub skill_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_args: Option<String>,
    pub trigger: SkillActivationTrigger,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_source: Option<Value>,
}
