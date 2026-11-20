use std::sync::Arc;

use crate::_base::utils::paths::join;
use crate::app::agent_profile_catalog::agent_profile_catalog::{
    AgentProfile, AgentProfileContext, AgentProfileRenderSystemPrompt, AgentProfileInput, DEFAULT_AGENT_PROFILE_NAME, normalize_agent_profile,
};
use crate::app::agent_profile_catalog::profile_shared::{
    render_prompt_template_result, skill_active_for,
};
use crate::os::interface::host_file_system::IHostFileSystem;
use crate::os::interface::host_fs_errors::{HostFsError, OS_FS_UNAVAILABLE};
use crate::wire::record::WireAsyncError;

use super::paths::is_file_path;

pub const SYSTEM_MD_FILENAME: &str = "SYSTEM.md";

pub async fn load_system_md_profile(
    fs: &dyn IHostFileSystem,
    brand_home: &str,
    builtin_default: AgentProfile,
    warn: &dyn Fn(&str),
) -> Result<Option<AgentProfile>, WireAsyncError> {
    let path = join(brand_home, &[SYSTEM_MD_FILENAME]);
    let loaded: Result<Option<String>, WireAsyncError> = async {
        if !is_file_path(fs, &path).await? {
            return Ok(None);
        }
        Ok(Some(fs.read_text(&path, None).await?))
    }
    .await;
    let text = match loaded {
        Ok(text) => text,
        Err(error) => {
            if error
                .downcast_ref::<HostFsError>()
                .is_some_and(|error| error.code().as_str() == OS_FS_UNAVAILABLE)
            {
                return Err(error);
            }
            warn(&format!("agent SYSTEM.md load failed: {error} [{path}]"));
            return Ok(None);
        }
    };
    let Some(text) = text else {
        return Ok(None);
    };
    if js_trim(&text).is_empty() {
        return Ok(None);
    }
    let skill_active = builtin_default
        .tools
        .as_ref()
        .is_none_or(|tools| skill_active_for(tools))
        && !builtin_default
            .disallowed_tools
            .as_ref()
            .is_some_and(|tools| tools.iter().any(|tool| tool == "Skill"));
    let render_default = builtin_default.render_system_prompt.clone();
    let input = AgentProfileInput {
        name: DEFAULT_AGENT_PROFILE_NAME.to_string(),
        description: builtin_default.description.clone(),
        when_to_use: None,
        override_: Some(true),
        tools: builtin_default.tools.clone(),
        disallowed_tools: builtin_default.disallowed_tools.clone(),
        subagents: builtin_default.subagents.clone(),
        summary_policy: None,
        system_prompt: None,
        render_system_prompt: Some(Arc::new(move |context| {
            render_prompt_template_result(
                &text,
                context,
                skill_active,
                Arc::new({
                    let render_default = render_default.clone();
                    move |context: &AgentProfileContext| (render_default.as_ref())(context)
                }) as AgentProfileRenderSystemPrompt,
            )
        })),
        prompt_prefix: None,
    };
    Ok(Some(normalize_agent_profile(input)))
}

fn js_trim(value: &str) -> &str {
    value.trim_matches(|character| {
        matches!(
            character,
            '\u{0009}'..='\u{000d}'
                | ' '
                | '\u{00a0}'
                | '\u{1680}'
                | '\u{2000}'..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
        )
    })
}
