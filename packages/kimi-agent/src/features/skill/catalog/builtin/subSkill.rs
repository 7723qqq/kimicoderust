pub use crate::skills::{BuiltinSkillDef, builtin_skill_defs};

pub const SUB_SKILL_ROOT: &str = "sub-skill";

pub fn sub_skill_parent() -> super::super::types::SkillDefinition {
    make_builtin(
        include_str!("../../../../skills/builtin/sub-skill/SKILL.md"),
        "sub-skill",
        "builtin://sub-skill",
        &[
            ("disableModelInvocation", serde_json::Value::Bool(true)),
            ("has-sub-skill", serde_json::Value::Bool(true)),
        ],
    )
}

pub fn sub_skill_review() -> super::super::types::SkillDefinition {
    make_builtin(
        include_str!("../../../../skills/builtin/sub-skill/review/SKILL.md"),
        "sub-skill.review",
        "builtin://sub-skill/review",
        &[
            ("disableModelInvocation", serde_json::Value::Bool(true)),
            ("isSubSkill", serde_json::Value::Bool(true)),
        ],
    )
}

pub fn sub_skill_consolidate() -> super::super::types::SkillDefinition {
    make_builtin(
        include_str!("../../../../skills/builtin/sub-skill/consolidate/SKILL.md"),
        "sub-skill.consolidate",
        "builtin://sub-skill/consolidate",
        &[
            ("disableModelInvocation", serde_json::Value::Bool(true)),
            ("isSubSkill", serde_json::Value::Bool(true)),
        ],
    )
}

fn make_builtin(
    body: &str,
    dir_name: &str,
    pseudo_path: &str,
    extra_metadata: &[(&str, serde_json::Value)],
) -> super::super::types::SkillDefinition {
    let mut skill = super::super::parser::parse_skill_text(&super::super::parser::ParseSkillTextOptions {
        skill_md_path: format!("/builtin/skills/{dir_name}/SKILL.md"),
        skill_dir_name: dir_name.to_string(),
        source: super::super::types::SkillSource::Builtin,
        text: body.to_string(),
    })
    .unwrap();
    skill.name = dir_name.to_string();
    skill.path = pseudo_path.to_string();
    skill.dir = skill.path.clone();
    if skill.metadata.get("type").is_none() {
        skill.metadata.insert("type".to_string(), serde_json::Value::String("inline".to_string()));
    }
    for (key, value) in extra_metadata {
        skill.metadata.insert(key.to_string(), value.clone());
    }
    skill
}
