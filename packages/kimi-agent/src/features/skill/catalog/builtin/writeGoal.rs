pub const WRITE_GOAL_PROMPT: &str = include_str!("../../../../skills/builtin/write-goal.md");

pub fn write_goal_skill() -> super::super::types::SkillDefinition {
    let mut skill = super::super::parser::parse_skill_text(&super::super::parser::ParseSkillTextOptions {
        skill_md_path: "/builtin/skills/write-goal.md".to_string(),
        skill_dir_name: "write-goal".to_string(),
        source: super::super::types::SkillSource::Builtin,
        text: WRITE_GOAL_PROMPT.to_string(),
    })
    .unwrap();
    skill.path = "builtin://write-goal".to_string();
    skill.dir = skill.path.clone();
    if skill.metadata.get("type").is_none() {
        skill.metadata.insert("type".to_string(), serde_json::Value::String("inline".to_string()));
    }
    skill
}
