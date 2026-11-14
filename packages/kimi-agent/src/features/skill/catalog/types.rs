use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillSource {
    Project,
    User,
    Extra,
    Builtin,
}

impl SkillSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::User => "user",
            Self::Extra => "extra",
            Self::Builtin => "builtin",
        }
    }
}

pub fn skill_source_rank(source: SkillSource) -> u16 {
    match source {
        SkillSource::Project => 400,
        SkillSource::User => 300,
        SkillSource::Extra => 200,
        SkillSource::Builtin => 100,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillScope {
    Tui,
    Web,
}

pub type SkillMetadata = BTreeMap<String, Value>;

#[derive(Debug, Clone, PartialEq)]
pub struct SkillPluginContext {
    pub id: String,
    pub instructions: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillScanMode {
    Directory,
    RootSkillOnly,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SkillDefinition {
    pub name: String,
    pub description: String,
    pub path: String,
    pub dir: String,
    pub content: String,
    pub metadata: SkillMetadata,
    pub source: SkillSource,
    pub workspace_dir: Option<String>,
    pub plugin: Option<SkillPluginContext>,
    pub mermaid: Option<String>,
    pub d2: Option<String>,
    pub product_specific: Option<bool>,
    pub scopes: Option<Vec<SkillScope>>,
    pub experimental_flag: Option<String>,
}

impl SkillDefinition {
    pub fn skill_type(&self) -> Option<&str> {
        self.metadata.get("type").and_then(Value::as_str)
    }

    pub fn disable_model_invocation(&self) -> Option<bool> {
        self.metadata
            .get("disableModelInvocation")
            .and_then(Value::as_bool)
    }

    pub fn is_sub_skill(&self) -> Option<bool> {
        self.metadata.get("isSubSkill").and_then(Value::as_bool)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SkillSummary {
    pub name: String,
    pub description: String,
    pub path: String,
    pub source: SkillSource,
    pub workspace_dir: Option<String>,
    pub skill_type: Option<String>,
    pub disable_model_invocation: Option<bool>,
    pub is_sub_skill: Option<bool>,
    pub scopes: Option<Vec<SkillScope>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SkillRoot {
    pub path: String,
    pub source: String,
    pub workspace_dir: Option<String>,
    pub plugin: Option<SkillPluginContext>,
    pub scan_mode: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedSkill {
    pub path: String,
    pub skill_type: String,
    pub reason: String,
}

pub trait SkillCatalog {
    fn get_skill(&self, name: &str, path: Option<&str>) -> Option<SkillDefinition>;
    fn get_plugin_skill(&self, plugin_id: &str, name: &str) -> Option<SkillDefinition>;
    fn render_skill_prompt(
        &self,
        skill: &SkillDefinition,
        raw_args: &str,
        session_id: Option<&str>,
    ) -> String;
    fn list_default_skills(&self) -> Vec<SkillDefinition>;
    fn list_skills(&self) -> Vec<SkillDefinition>;
    fn list_invocable_skills(&self) -> Vec<SkillDefinition>;
    fn get_skill_roots(&self) -> Vec<String>;
    fn get_skipped_by_policy(&self) -> Vec<SkippedSkill>;
    fn get_model_skill_listing(&self) -> String;
}

pub fn normalize_skill_name(name: &str) -> String {
    name.to_lowercase()
}

pub fn is_inline_skill_type(skill_type: Option<&str>) -> bool {
    matches!(skill_type, None | Some("prompt") | Some("inline"))
}

pub fn is_user_activatable_skill_type(skill_type: Option<&str>) -> bool {
    is_inline_skill_type(skill_type) || skill_type == Some("flow")
}

pub fn is_supported_skill_type(skill_type: Option<&str>) -> bool {
    is_user_activatable_skill_type(skill_type) || skill_type == Some("reference")
}

pub fn summarize_skill(skill: &SkillDefinition) -> SkillSummary {
    SkillSummary {
        name: skill.name.clone(),
        description: skill.description.clone(),
        path: skill.path.clone(),
        source: skill.source,
        workspace_dir: skill.workspace_dir.clone(),
        skill_type: skill.skill_type().map(str::to_owned),
        disable_model_invocation: skill.disable_model_invocation(),
        is_sub_skill: skill.is_sub_skill(),
        scopes: skill.scopes.clone(),
    }
}
