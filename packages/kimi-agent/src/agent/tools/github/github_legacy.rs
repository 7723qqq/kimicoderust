use std::sync::Arc;

use serde_json::Value;

use super::github_tools::{
    GITHUB_MUTATING_TOOL_NAMES, GITHUB_SPECS, GitHubToolBase, GitHubToolDeps,
};
use crate::_base::utils::abort::AbortController;
use crate::agent::tool_executor::tool_executor::{
    ExecutableToolContext, ExecutableToolOutput, ExecutableToolResult, ToolExecution,
};
use crate::config::{GitHubConfig, KimiConfig};
use crate::turn_loop::types::{ExecutableToolResult as LegacyExecutableToolResult, ToolInfo};

#[cfg(test)]
pub(crate) static GITHUB_ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Clone, Debug, Default)]
pub struct GitHubCredentials {
    pub token: Option<String>,
    pub base_url: Option<String>,
}

pub fn github_tool_defs() -> Vec<ToolInfo> {
    GITHUB_SPECS
        .iter()
        .map(|spec| ToolInfo {
            name: spec.name.clone(),
            description: spec.description.clone(),
            input_schema: spec.schema.clone(),
        })
        .collect()
}

pub fn is_github_tool(tool_name: &str) -> bool {
    GITHUB_SPECS
        .iter()
        .any(|spec| spec.name.eq_ignore_ascii_case(tool_name))
}

pub fn is_mutating_tool(tool_name: &str) -> bool {
    GITHUB_MUTATING_TOOL_NAMES
        .iter()
        .any(|name| name.eq_ignore_ascii_case(tool_name))
}

pub fn is_readonly_tool(tool_name: &str) -> bool {
    is_github_tool(tool_name) && !is_mutating_tool(tool_name)
}

pub fn rule_subject(tool_name: &str, args: &Value) -> Option<String> {
    let spec = GITHUB_SPECS
        .iter()
        .find(|spec| spec.name.eq_ignore_ascii_case(tool_name))?;
    if spec.name.eq_ignore_ascii_case("GitHubGetMe") {
        return Some("me".to_string());
    }
    let has_query = spec
        .schema
        .get("properties")
        .and_then(Value::as_object)
        .is_some_and(|properties| properties.contains_key("q"));
    if has_query {
        return args
            .get("q")
            .and_then(Value::as_str)
            .map(str::to_string);
    }
    match (
        args.get("owner").and_then(Value::as_str),
        args.get("repo").and_then(Value::as_str),
    ) {
        (Some(owner), Some(repo)) => Some(format!("{owner}/{repo}")),
        _ => None,
    }
}

pub fn has_token(credentials: Option<&GitHubCredentials>) -> bool {
    config_with_credentials(credentials).github.token.is_some()
}

pub async fn execute_github_tool(
    tool_name: &str,
    args: &Value,
    credentials: Option<&GitHubCredentials>,
) -> Option<LegacyExecutableToolResult> {
    let spec = GITHUB_SPECS
        .iter()
        .find(|spec| spec.name.eq_ignore_ascii_case(tool_name))?
        .clone();
    let tool = GitHubToolBase::new(
        spec,
        GitHubToolDeps {
            config: Arc::new(config_with_credentials(credentials)),
            client: None,
        },
    );
    match tool.resolve_execution(args.clone()) {
        ToolExecution::Error(result) => Some(legacy_result(result)),
        ToolExecution::Runnable(run) => {
            let context = ExecutableToolContext {
                turn_id: 0,
                tool_call_id: String::new(),
                trace: None,
                metadata: None,
                signal: AbortController::new().signal(),
                steer_signal: None,
                on_update: None,
                on_foreground_task_start: None,
            };
            match (run.execute)(context).await {
                Ok(result) => Some(legacy_result(result)),
                Err(error) => Some(crate::tools::err_result(error.to_string())),
            }
        }
    }
}

fn config_with_credentials(credentials: Option<&GitHubCredentials>) -> KimiConfig {
    let token = credentials
        .and_then(|credentials| credentials.token.as_deref())
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .or_else(|| non_empty_env("GITHUB_TOKEN"))
        .or_else(|| non_empty_env("GH_TOKEN"));
    let base_url = credentials
        .and_then(|credentials| credentials.base_url.as_deref())
        .map(str::trim)
        .filter(|base_url| !base_url.is_empty())
        .map(str::to_string)
        .or_else(|| non_empty_env("GITHUB_API_URL"));
    let mut config = KimiConfig::default();
    config.github = GitHubConfig { token, base_url };
    config
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn legacy_result(result: ExecutableToolResult) -> LegacyExecutableToolResult {
    let content = match result.output {
        ExecutableToolOutput::Text(text) => text,
        ExecutableToolOutput::Parts(parts) => serde_json::to_string(&parts).unwrap_or_default(),
    };
    LegacyExecutableToolResult {
        stop_turn: result.stop_turn.unwrap_or(false),
        content,
        is_error: result.is_error.unwrap_or(false),
        note: result.note,
        delivery: result.delivery,
        parts: Vec::new(),
        display: None,
        spill: result.spill,
    }
}
