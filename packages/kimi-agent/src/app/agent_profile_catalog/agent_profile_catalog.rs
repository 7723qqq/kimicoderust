use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};

use crate::_base::log::log::ILogger;
use crate::os::interface::host_environment::JsRuntimeInfo;
use crate::os::interface::host_process::IHostProcessService;

use super::behavior_contract::ContractDelivery;

pub const DEFAULT_AGENT_PROFILE_NAME: &str = "agent";

#[derive(Clone)]
pub struct AgentProfilePromptPrefixContext {
    pub cwd: String,
    pub process: Arc<dyn IHostProcessService>,
    pub log: Option<Arc<dyn ILogger>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentProfileSummaryPolicy {
    pub min_chars: usize,
    pub continuation_prompt: String,
    pub retries: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentProfileContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd_listing: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents_md: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_memory: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additional_dirs_info: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub js_runtimes: Option<Vec<JsRuntimeInfo>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub now: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_zone: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_active: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_sections: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_adaptation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub product_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_style_guide: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify_user_active: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub todo_list_active: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_mode_available: Option<bool>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentDisclosureSnapshot {
    pub cwd: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptValueChannel {
    System,
    Reminder,
    IntentionallyOmitted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptValueDelivery {
    pub name: String,
    pub channel: PromptValueChannel,
    pub source_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptRenderWarningCode {
    UnresolvedVariable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptRenderWarning {
    pub code: PromptRenderWarningCode,
    pub name: String,
    pub message: String,
}

#[derive(Clone)]
pub struct SystemPromptRenderResult {
    pub text: String,
    pub environment: EnvironmentDisclosureSnapshot,
    pub dropped_vars: Option<Vec<String>>,
    pub contract_delivery: Option<Vec<ContractDelivery>>,
    pub value_delivery: Option<Vec<PromptValueDelivery>>,
    pub warnings: Option<Vec<PromptRenderWarning>>,
}

pub type AgentProfileSystemPrompt =
    Arc<dyn Fn(&AgentProfileContext) -> String + Send + Sync>;
pub type AgentProfileRenderSystemPrompt =
    Arc<dyn Fn(&AgentProfileContext) -> SystemPromptRenderResult + Send + Sync>;
pub type AgentProfilePromptPrefix = Arc<
    dyn Fn(AgentProfilePromptPrefixContext) -> BoxFuture<'static, String> + Send + Sync,
>;

#[derive(Clone)]
pub struct AgentProfile {
    pub name: String,
    pub description: Option<String>,
    pub when_to_use: Option<String>,
    pub override_: Option<bool>,
    pub tools: Option<Vec<String>>,
    pub disallowed_tools: Option<Vec<String>>,
    pub subagents: Option<Vec<String>>,
    pub summary_policy: Option<AgentProfileSummaryPolicy>,
    pub system_prompt: AgentProfileSystemPrompt,
    pub render_system_prompt: AgentProfileRenderSystemPrompt,
    pub prompt_prefix: Option<AgentProfilePromptPrefix>,
}

impl AsRef<AgentProfile> for AgentProfile {
    fn as_ref(&self) -> &AgentProfile {
        self
    }
}

#[derive(Clone)]
pub struct AgentProfileInput {
    pub name: String,
    pub description: Option<String>,
    pub when_to_use: Option<String>,
    pub override_: Option<bool>,
    pub tools: Option<Vec<String>>,
    pub disallowed_tools: Option<Vec<String>>,
    pub subagents: Option<Vec<String>>,
    pub summary_policy: Option<AgentProfileSummaryPolicy>,
    pub system_prompt: Option<AgentProfileSystemPrompt>,
    pub render_system_prompt: Option<AgentProfileRenderSystemPrompt>,
    pub prompt_prefix: Option<AgentProfilePromptPrefix>,
}

pub fn normalize_agent_profile(input: AgentProfileInput) -> AgentProfile {
    let AgentProfileInput {
        name,
        description,
        when_to_use,
        override_,
        tools,
        disallowed_tools,
        subagents,
        summary_policy,
        system_prompt,
        render_system_prompt,
        prompt_prefix,
    } = input;
    if let Some(render) = render_system_prompt {
        let system_render = render.clone();
        let system_prompt: AgentProfileSystemPrompt =
            Arc::new(move |context| system_render(context).text);
        return AgentProfile {
            name,
            description,
            when_to_use,
            override_,
            tools,
            disallowed_tools,
            subagents,
            summary_policy,
            system_prompt,
            render_system_prompt: render,
            prompt_prefix,
        };
    }
    if let Some(system_prompt) = system_prompt {
        let render = system_prompt.clone();
        let render_system_prompt: AgentProfileRenderSystemPrompt = Arc::new(move |context| {
            SystemPromptRenderResult {
                text: render(context),
                environment: EnvironmentDisclosureSnapshot {
                    cwd: context.cwd.clone().unwrap_or_default(),
                },
                dropped_vars: None,
                contract_delivery: None,
                value_delivery: None,
                warnings: None,
            }
        });
        return AgentProfile {
            name,
            description,
            when_to_use,
            override_,
            tools,
            disallowed_tools,
            subagents,
            summary_policy,
            system_prompt,
            render_system_prompt,
            prompt_prefix,
        };
    }
    panic!("Agent profile \"{name}\" must define systemPrompt or renderSystemPrompt.");
}
