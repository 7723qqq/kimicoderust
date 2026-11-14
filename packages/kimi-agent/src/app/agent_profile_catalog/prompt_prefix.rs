use std::panic::AssertUnwindSafe;

use futures_util::FutureExt;

use super::agent_profile_catalog::{AgentProfile, AgentProfilePromptPrefixContext};

pub async fn apply_profile_prompt_prefix(
    profile: &AgentProfile,
    prompt: &str,
    ctx: AgentProfilePromptPrefixContext,
) -> String {
    apply_profile_prompt_prefix_with_context(profile, prompt, ctx).await
}

pub(crate) async fn apply_profile_prompt_prefix_with_context(
    profile: &AgentProfile,
    prompt: &str,
    context: AgentProfilePromptPrefixContext,
) -> String {
    let Some(prompt_prefix) = profile.prompt_prefix.clone() else {
        return prompt.to_string();
    };
    let future = match std::panic::catch_unwind(AssertUnwindSafe(|| prompt_prefix(context))) {
        Ok(future) => future,
        Err(_) => return prompt.to_string(),
    };
    match AssertUnwindSafe(future).catch_unwind().await {
        Ok(prefix) if !prefix.is_empty() => format!("{prefix}\n\n{prompt}"),
        _ => prompt.to_string(),
    }
}
