use std::sync::{LazyLock, Mutex};

use crate::features::skill::catalog::types::SkillDefinition;

static BUILTIN_SKILL_CONTRIBUTIONS: LazyLock<Mutex<Vec<SkillDefinition>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

pub fn register_builtin_skill(skill: SkillDefinition) {
    let mut contributions = BUILTIN_SKILL_CONTRIBUTIONS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(existing_index) = contributions
        .iter()
        .position(|candidate| candidate.name == skill.name)
    {
        contributions.remove(existing_index);
    }
    contributions.push(skill);
}

pub fn get_builtin_skill_contributions() -> Vec<SkillDefinition> {
    BUILTIN_SKILL_CONTRIBUTIONS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

pub fn _clear_builtin_skill_contributions_for_tests() {
    BUILTIN_SKILL_CONTRIBUTIONS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
}
