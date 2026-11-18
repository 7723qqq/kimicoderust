use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::_base::utils::abort::AbortSignal;
use crate::agent::tool_executor::tool_executor::{
    ExecutableTool, ExecutableToolContext, ExecutableToolResult, RunnableToolExecution, ToolExecution,
};
use crate::agent::tool_executor::tool_executor_events::ToolUpdate;
use crate::mcp_core::oauth::service::{
    AlreadyAuthorizedError, BeginAuthorizationOptions, CompleteAuthorizationOptions, McpOAuthService,
};
use crate::mcp_core::tool_naming::qualify_mcp_tool_name;
use crate::rpc::types::BoxFuture;
use crate::wire::record::WireAsyncError;

pub const MCP_OAUTH_AUTHORIZATION_URL_TOOL_UPDATE: &str = "mcp.oauth.authorization_url";
const DEFAULT_AUTH_TIMEOUT_MS: u64 = 15 * 60 * 1000;
const AUTH_TOOL_TOOL_NAME: &str = "authenticate";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpOAuthAuthorizationUrlUpdateData {
    pub wait_id: String,
    pub server_name: String,
    pub authorization_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
}

pub type McpAuthReconnect = Arc<
    dyn Fn(Option<AbortSignal>) -> BoxFuture<'static, Result<(), WireAsyncError>> + Send + Sync,
>;

#[derive(Clone)]
pub struct CreateMcpAuthToolOptions {
    pub server_name: String,
    pub server_url: String,
    pub oauth_service: Arc<McpOAuthService>,
    pub reconnect: McpAuthReconnect,
    pub timeout_ms: Option<u64>,
}

pub fn create_mcp_auth_tool(options: CreateMcpAuthToolOptions) -> Arc<dyn ExecutableTool> {
    Arc::new(McpAuthTool {
        name: qualify_mcp_tool_name(&options.server_name, AUTH_TOOL_TOOL_NAME),
        description: description_template(&options.server_name),
        options,
    })
}

struct McpAuthTool {
    name: String,
    description: String,
    options: CreateMcpAuthToolOptions,
}

impl ExecutableTool for McpAuthTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Map<String, Value> {
        json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
        .as_object()
        .cloned()
        .unwrap()
    }

    fn resolve_execution(
        &self,
        _input: Value,
    ) -> BoxFuture<'static, Result<ToolExecution, WireAsyncError>> {
        let name = self.name.clone();
        let options = self.options.clone();
        Box::pin(async move {
            let execution = RunnableToolExecution {
                accesses: None,
                display: None,
                description: Some(format!("Authenticating {}", options.server_name)),
                stop_batch_after_this: None,
                approval_rule: name,
                matches_rule: None,
                execute: Arc::new(move |context| {
                    let options = options.clone();
                    Box::pin(async move { execute_auth(&options, context).await })
                }),
            };
            Ok(ToolExecution::Runnable(execution))
        })
    }
}

async fn execute_auth(
    options: &CreateMcpAuthToolOptions,
    context: ExecutableToolContext,
) -> Result<ExecutableToolResult, WireAsyncError> {
    let server_name = &options.server_name;
    if context.signal.aborted() {
        return Ok(error_result(server_name, "aborted", None));
    }
    emit_status(
        &context,
        &format!("Discovering OAuth metadata for {server_name}…"),
    );
    let flow = match options
        .oauth_service
        .begin_authorization(
            server_name,
            &options.server_url,
            BeginAuthorizationOptions { client_label: None },
        )
        .await
    {
        Ok(flow) => flow,
        Err(error) => {
            if error.downcast_ref::<AlreadyAuthorizedError>().is_some() {
                emit_status(
                    &context,
                    &format!("Already authorized; reconnecting {server_name}…"),
                );
                if let Err(error) = (options.reconnect)(Some(context.signal)).await {
                    return Ok(error_result(server_name, &error.to_string(), None));
                }
                return Ok(ExecutableToolResult {
                    output: format!(
                        "MCP server \"{server_name}\" already had valid OAuth credentials. Reconnected; real tools are available now."
                    )
                    .into(),
                    ..Default::default()
                });
            }
            return Ok(error_result(server_name, &error.to_string(), None));
        }
    };
    let url_text = flow.authorization_url.clone();
    let wait_timeout_ms = options.timeout_ms.unwrap_or(DEFAULT_AUTH_TIMEOUT_MS);
    let custom_data = serde_json::to_value(McpOAuthAuthorizationUrlUpdateData {
        wait_id: flow.wait_id.clone(),
        server_name: server_name.clone(),
        authorization_url: url_text.clone(),
        expires_at: Some(chrono::Utc::now().timestamp_millis() + wait_timeout_ms as i64),
    })?;
    if let Some(on_update) = &context.on_update {
        on_update(ToolUpdate {
            kind: "custom".to_string(),
            custom_kind: Some(MCP_OAUTH_AUTHORIZATION_URL_TOOL_UPDATE.to_string()),
            custom_data: Some(custom_data.clone()),
            ..Default::default()
        });
    }
    emit_status(
        &context,
        &format!(
            "Open this URL in your browser to authorize \"{server_name}\":\n\n{url_text}\n\nWaiting for the OAuth callback (timeout 15 min). If you cancel, call this tool again to restart the flow."
        ),
    );
    if let Err(error) = (flow.complete)(CompleteAuthorizationOptions {
        signal: Some(context.signal.clone()),
        timeout_ms: Some(wait_timeout_ms),
    })
    .await
    {
        return Ok(error_result(
            server_name,
            &error.to_string(),
            Some(&url_text),
        ));
    }
    emit_status(
        &context,
        &format!("Authorized — reconnecting {server_name}…"),
    );
    if let Err(error) = (options.reconnect)(Some(context.signal)).await {
        return Ok(error_result(server_name, &error.to_string(), None));
    }
    Ok(ExecutableToolResult {
        output: format!(
            "MCP server \"{server_name}\" authenticated successfully. The real MCP tools have replaced this synthetic authenticate tool."
        )
        .into(),
        ..Default::default()
    })
}

fn description_template(server_name: &str) -> String {
    format!(
        "Authenticate with MCP server \"{server_name}\" via OAuth.\n\nThis server requires an OAuth login that has not yet been completed. Calling this tool starts the authorization flow:\n\n  1. The tool prints an authorization URL.\n  2. **You must show that URL to the user verbatim** and ask them to open it\n     in a browser, sign in, and approve the client.\n  3. The tool blocks (up to 15 minutes) until the browser redirects back to\n     the local callback listener.\n  4. On success, the client reconnects the MCP server and the real tools\n     replace this synthetic tool.\n\nTake no arguments. Treat the URL as sensitive — do not modify it or strip\nquery parameters."
    )
}

fn emit_status(context: &ExecutableToolContext, text: &str) {
    if let Some(on_update) = &context.on_update {
        on_update(ToolUpdate::text("status", text));
    }
}

fn error_result(
    server_name: &str,
    message: &str,
    authorization_url: Option<&str>,
) -> ExecutableToolResult {
    let suffix = authorization_url
        .map(|url| {
            format!(
                "\n\nAuthorization URL (still valid if the listener has not timed out): {url}"
            )
        })
        .unwrap_or_default();
    ExecutableToolResult {
        output: format!(
            "OAuth flow for MCP server \"{server_name}\" did not complete: {message}{suffix}"
        )
        .into(),
        is_error: Some(true),
        ..Default::default()
    }
}
