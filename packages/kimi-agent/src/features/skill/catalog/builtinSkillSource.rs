use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::_base::di::lifecycle::IDisposable;
use crate::_base::event::{Emitter, Event};
use crate::app::config::config_service::ConfigService;

use super::builtin::builtin::builtin_skill_defs;
use super::configSection::{BUILTIN_PRODUCT_SKILLS_SECTION, builtin_product_skills_enabled};
use super::skillSource::{
    BUILTIN_SKILL_SOURCE_ID, ISkillSource, SkillContribution, skill_source_priority,
};
use super::types::{SkillDefinition, SkillMetadata, SkillScope, SkillSource};

pub use crate::skills::{BuiltinSkillDef, builtin_skills};

const BUILTIN_SKILL_ORDER: &[&str] = &[
    "mcp-config",
    "import-from-cc-codex",
    "update-config",
    "custom-theme",
    "write-goal",
    "check-kimi-code-docs",
    "sub-skill",
    "sub-skill.review",
    "sub-skill.consolidate",
];

pub struct BuiltinSkillSource {
    config: Arc<ConfigService>,
    on_did_change_emitter: Arc<Emitter<()>>,
    _config_subscription: Box<dyn IDisposable>,
}

impl BuiltinSkillSource {
    pub fn new(config: Arc<ConfigService>) -> Arc<Self> {
        let on_did_change_emitter = Arc::new(Emitter::new());
        let emitter = on_did_change_emitter.clone();
        let subscription = config.on_did_section_change(move |event| {
            if event.domain == BUILTIN_PRODUCT_SKILLS_SECTION {
                emitter.fire(());
            }
        });
        Arc::new(Self {
            config,
            on_did_change_emitter,
            _config_subscription: subscription,
        })
    }
}

#[async_trait]
pub trait IBuiltinSkillSource: ISkillSource {}

impl IBuiltinSkillSource for BuiltinSkillSource {}

impl ISkillSource for BuiltinSkillSource {
    fn id(&self) -> &str {
        BUILTIN_SKILL_SOURCE_ID
    }

    fn priority(&self) -> i32 {
        skill_source_priority::BUILTIN
    }

    fn on_did_change(&self) -> Option<Event<()>> {
        Some(self.on_did_change_emitter.event())
    }

    async fn load(&self) -> Result<SkillContribution, String> {
        self.config.ready().await.map_err(|error| error.to_string())?;
        let product_enabled = builtin_product_skills_enabled(self.config.as_ref());
        let mut definitions = builtin_skill_defs();
        definitions.sort_by_key(|definition| {
            BUILTIN_SKILL_ORDER
                .iter()
                .position(|name| *name == definition.descriptor.name)
                .unwrap_or(usize::MAX)
        });
        Ok(SkillContribution {
            skills: definitions
                .into_iter()
                .filter(|definition| product_enabled || !definition.product_specific)
                .map(builtin_definition)
                .collect(),
            ..SkillContribution::default()
        })
    }
}

fn builtin_definition(definition: BuiltinSkillDef) -> SkillDefinition {
    let descriptor = definition.descriptor;
    let parsed = crate::skills::parse_skill_frontmatter(definition.body, &descriptor.name);
    let content = crate::_base::text::frontmatter::parse_frontmatter(definition.body)
        .map(|parsed| trim_js(&parsed.body).to_string())
        .unwrap_or_else(|_| trim_js(definition.body).to_string());
    let mut metadata = SkillMetadata::new();
    metadata.insert("name".to_string(), Value::String(descriptor.name.clone()));
    metadata.insert("description".to_string(), Value::String(descriptor.description.clone()));
    if let Some(value) = &descriptor.skill_type {
        metadata.insert("type".to_string(), Value::String(value.clone()));
    }
    if let Some(value) = &descriptor.when_to_use {
        metadata.insert("whenToUse".to_string(), Value::String(value.clone()));
    }
    if descriptor.disable_model_invocation {
        metadata.insert("disableModelInvocation".to_string(), Value::Bool(true));
    }
    if let Some(value) = descriptor.is_sub_skill {
        metadata.insert("isSubSkill".to_string(), Value::Bool(value));
    }
    if !parsed.argument_names.is_empty() {
        metadata.insert(
            "arguments".to_string(),
            Value::Array(parsed.argument_names.into_iter().map(Value::String).collect()),
        );
    }
    let dir = Path::new(&descriptor.path)
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .to_string_lossy()
        .replace('\\', "/");
    SkillDefinition {
        name: descriptor.name,
        description: descriptor.description,
        path: descriptor.path,
        dir,
        content: content.clone(),
        metadata,
        source: SkillSource::Builtin,
        workspace_dir: None,
        plugin: None,
        mermaid: super::parser::parse_mermaid_flowchart(&content),
        d2: super::parser::parse_d2_flowchart(&content),
        product_specific: definition.product_specific.then_some(true),
        scopes: descriptor.scopes.map(|scopes| {
            scopes
                .into_iter()
                .filter_map(|scope| match scope.as_str() {
                    "tui" => Some(SkillScope::Tui),
                    "web" => Some(SkillScope::Web),
                    _ => None,
                })
                .collect()
        }),
        experimental_flag: None,
    }
}

fn trim_js(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

fn is_js_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'
            | '\u{000a}'
            | '\u{000b}'
            | '\u{000c}'
            | '\u{000d}'
            | '\u{0020}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'
            | '\u{2001}'
            | '\u{2002}'
            | '\u{2003}'
            | '\u{2004}'
            | '\u{2005}'
            | '\u{2006}'
            | '\u{2007}'
            | '\u{2008}'
            | '\u{2009}'
            | '\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}
