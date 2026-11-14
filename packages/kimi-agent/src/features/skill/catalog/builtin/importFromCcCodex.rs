pub const IMPORT_FROM_CC_CODEX_PROMPT: &str =
    include_str!("../../../../skills/builtin/import-from-cc-codex.md");

pub fn import_from_cc_codex_skill() -> super::super::types::SkillDefinition {
    let mut skill = super::super::parser::parse_skill_text(&super::super::parser::ParseSkillTextOptions {
        skill_md_path: "/builtin/skills/import-from-cc-codex.md".to_string(),
        skill_dir_name: "import-from-cc-codex".to_string(),
        source: super::super::types::SkillSource::Builtin,
        text: IMPORT_FROM_CC_CODEX_PROMPT.to_string(),
    })
    .unwrap();
    skill.path = "builtin://import-from-cc-codex".to_string();
    skill.dir = skill.path.clone();
    if skill.metadata.get("type").is_none() {
        skill.metadata.insert("type".to_string(), serde_json::Value::String("inline".to_string()));
    }
    skill.metadata.insert("disableModelInvocation".to_string(), serde_json::Value::Bool(true));
    skill.product_specific = Some(true);
    skill
}
