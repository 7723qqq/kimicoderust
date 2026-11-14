pub const CHECK_KIMI_CODE_DOC_PROMPT: &str =
    include_str!("../../../../skills/builtin/check-kimi-code-docs.md");

pub fn check_kimi_code_docs_skill() -> super::super::types::SkillDefinition {
    let mut skill = super::super::parser::parse_skill_text(&super::super::parser::ParseSkillTextOptions {
        skill_md_path: "/builtin/skills/check-kimi-code-docs.md".to_string(),
        skill_dir_name: "check-kimi-code-docs".to_string(),
        source: super::super::types::SkillSource::Builtin,
        text: CHECK_KIMI_CODE_DOC_PROMPT.to_string(),
    })
    .unwrap();
    skill.path = "builtin://check-kimi-code-docs".to_string();
    skill.dir = skill.path.clone();
    if skill.metadata.get("type").is_none() {
        skill.metadata.insert("type".to_string(), serde_json::Value::String("inline".to_string()));
    }
    skill.product_specific = Some(true);
    skill
}
