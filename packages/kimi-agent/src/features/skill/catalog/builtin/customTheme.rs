pub const CUSTOM_THEME_PROMPT: &str = include_str!("../../../../skills/builtin/custom-theme.md");

pub fn custom_theme_skill() -> super::super::types::SkillDefinition {
    let mut skill = super::super::parser::parse_skill_text(&super::super::parser::ParseSkillTextOptions {
        skill_md_path: "/builtin/skills/custom-theme.md".to_string(),
        skill_dir_name: "custom-theme".to_string(),
        source: super::super::types::SkillSource::Builtin,
        text: CUSTOM_THEME_PROMPT.to_string(),
    })
    .unwrap();
    skill.path = "builtin://custom-theme".to_string();
    skill.dir = skill.path.clone();
    if skill.metadata.get("type").is_none() {
        skill.metadata.insert("type".to_string(), serde_json::Value::String("inline".to_string()));
    }
    skill.metadata.insert("disableModelInvocation".to_string(), serde_json::Value::Bool(true));
    skill.product_specific = Some(true);
    skill.scopes = Some(vec![super::super::types::SkillScope::Tui]);
    skill
}
