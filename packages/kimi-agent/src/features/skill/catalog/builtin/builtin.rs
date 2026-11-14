pub use crate::skills::BuiltinSkillDef;

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

pub fn builtin_skill_defs() -> Vec<BuiltinSkillDef> {
    let mut definitions = crate::skills::builtin_skill_defs();
    definitions.sort_by_key(|definition| {
        BUILTIN_SKILL_ORDER
            .iter()
            .position(|name| *name == definition.descriptor.name)
            .unwrap_or(usize::MAX)
    });
    definitions
}

pub fn visible_builtin_skill_defs(product_skills_enabled: bool) -> Vec<BuiltinSkillDef> {
    builtin_skill_defs()
        .into_iter()
        .filter(|definition| product_skills_enabled || !definition.product_specific)
        .collect()
}

pub fn builtin_skills() -> Vec<crate::skills::SkillDescriptor> {
    visible_builtin_skill_defs(crate::config::KimiConfig::resolved_builtin_product_skills())
        .into_iter()
        .map(|definition| definition.descriptor)
        .collect()
}
