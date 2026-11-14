pub const MCP_CONFIG_PROMPT: &str = include_str!("../../../../skills/builtin/mcp-config.md");

pub fn mcp_config_skill() -> super::super::types::SkillDefinition {
    let mut skill = super::super::parser::parse_skill_text(&super::super::parser::ParseSkillTextOptions {
        skill_md_path: "/builtin/skills/mcp-config.md".to_string(),
        skill_dir_name: "mcp-config".to_string(),
        source: super::super::types::SkillSource::Builtin,
        text: MCP_CONFIG_PROMPT.to_string(),
    })
    .unwrap();
    skill.path = "builtin://mcp-config".to_string();
    skill.dir = skill.path.clone();
    if skill.metadata.get("type").is_none() {
        skill.metadata.insert("type".to_string(), serde_json::Value::String("inline".to_string()));
    }
    skill.metadata.insert("disableModelInvocation".to_string(), serde_json::Value::Bool(true));
    skill.product_specific = Some(true);
    skill
}
