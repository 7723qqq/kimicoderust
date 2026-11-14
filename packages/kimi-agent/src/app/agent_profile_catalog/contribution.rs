use std::sync::LazyLock;

use parking_lot::Mutex;

use super::agent_profile_catalog::{AgentProfile, AgentProfileInput, normalize_agent_profile};

static PROFILE_CONTRIBUTIONS: LazyLock<Mutex<Vec<AgentProfile>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

pub fn register_agent_profile(definition: AgentProfileInput) {
    let profile = normalize_agent_profile(definition);
    let mut contributions = PROFILE_CONTRIBUTIONS.lock();
    if let Some(existing_index) = contributions
        .iter()
        .position(|candidate| candidate.name == profile.name)
    {
        contributions.remove(existing_index);
    }
    contributions.push(profile);
}

pub fn get_agent_profile_contributions() -> Vec<AgentProfile> {
    PROFILE_CONTRIBUTIONS.lock().clone()
}

pub fn _clear_agent_profile_contributions_for_tests() {
    PROFILE_CONTRIBUTIONS.lock().clear();
}
