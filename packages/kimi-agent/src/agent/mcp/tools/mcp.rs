use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::_base::errors::errors::SharedError;
use crate::_base::errors::error_message::to_error_message;
use crate::_base::errors::serialize::UnknownError;
use crate::_base::utils::abort::{AbortSignal, is_abort_error};
use crate::agent::mcp::output::{
    McpOriginalsTarget, McpOutputOptions, mcp_result_to_executable_output,
};
use crate::agent::media::session_media_store::ISessionMediaStore;
use crate::agent::tool_executor::tool_executor::{
    ExecutableTool, ExecutableToolContext, ExecutableToolResult, RunnableToolExecution, ToolExecution,
};
use crate::agent::tool_executor::tool_executor_events::ToolUpdate;
use crate::app::telemetry::telemetry::ITelemetryService;
use crate::human::llm::message::ToolDescription;
use crate::mcp_core::client_shared::{
    is_mcp_connection_closed_error, is_mcp_malformed_result_error, is_mcp_transport_failure,
    probe_mcp_liveness,
};
use crate::mcp_core::errors::{MCP_OAUTH_FAILED, MCP_STARTUP_FAILED};
use crate::mcp_core::tool_naming::qualify_mcp_tool_name;
use crate::mcp_core::types::{MCPClient, MCPToolResult};
use crate::rpc::types::BoxFuture;
use crate::wire::record::WireAsyncError;
use kimi_core::primitives::errors::{Error2, Error2Options};

pub type McpReconnect = Arc<
    dyn Fn(
            Option<AbortSignal>,
        ) -> BoxFuture<'static, Result<Option<Arc<dyn MCPClient>>, WireAsyncError>>
        + Send
        + Sync,
>;
pub type McpUnauthorizedHandler = Arc<
    dyn for<'a> Fn(&'a WireAsyncError, Arc<dyn MCPClient>) -> BoxFuture<'static, bool>
        + Send
        + Sync,
>;

#[derive(Clone, Default)]
pub struct McpToolOptions {
    pub server_name: Option<String>,
    pub attachment_store: Option<Arc<dyn ISessionMediaStore>>,
    pub originals: Option<Arc<dyn Fn() -> Option<McpOriginalsTarget> + Send + Sync>>,
    pub telemetry: Option<Arc<dyn ITelemetryService>>,
    pub provider_type: Option<Arc<dyn Fn() -> Option<String> + Send + Sync>>,
    pub reconnect: Option<McpReconnect>,
    pub is_removed: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    pub on_unauthorized: Option<McpUnauthorizedHandler>,
}

pub fn create_mcp_tool(
    qualified_name: &str,
    tool: ToolDescription,
    client: Arc<dyn MCPClient>,
    options: McpToolOptions,
) -> Arc<dyn ExecutableTool> {
    Arc::new(McpTool {
        qualified_name: qualified_name.to_string(),
        tool,
        client,
        options: Arc::new(options),
    })
}

struct McpTool {
    qualified_name: String,
    tool: ToolDescription,
    client: Arc<dyn MCPClient>,
    options: Arc<McpToolOptions>,
}

impl ExecutableTool for McpTool {
    fn name(&self) -> &str {
        &self.qualified_name
    }

    fn description(&self) -> &str {
        &self.tool.description
    }

    fn parameters(&self) -> Map<String, Value> {
        self.tool.parameters.clone()
    }

    fn resolve_execution(
        &self,
        input: Value,
    ) -> BoxFuture<'static, Result<ToolExecution, WireAsyncError>> {
        let qualified_name = self.qualified_name.clone();
        let tool_name = self.tool.name.clone();
        let client = self.client.clone();
        let options = self.options.clone();
        Box::pin(async move {
            let execute_options = options.clone();
            let execute_name = qualified_name.clone();
            let execute_tool_name = tool_name.clone();
            let execute_client = client.clone();
            let args = input
                .as_object()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .collect::<HashMap<String, Value>>();
            Ok(ToolExecution::Runnable(RunnableToolExecution {
                accesses: None,
                display: None,
                description: None,
                stop_batch_after_this: None,
                approval_rule: qualified_name,
                matches_rule: None,
                execute: Arc::new(move |context| {
                    let options = execute_options.clone();
                    let qualified_name = execute_name.clone();
                    let tool_name = execute_tool_name.clone();
                    let client = execute_client.clone();
                    let args = args.clone();
                    Box::pin(async move {
                        execute_mcp_tool(
                            qualified_name,
                            tool_name,
                            client,
                            args,
                            options,
                            context,
                        )
                        .await
                    })
                }),
            }))
        })
    }
}

async fn execute_mcp_tool(
    qualified_name: String,
    tool_name: String,
    client: Arc<dyn MCPClient>,
    args: HashMap<String, Value>,
    options: Arc<McpToolOptions>,
    context: ExecutableToolContext,
) -> Result<ExecutableToolResult, WireAsyncError> {
    if options.is_removed.as_ref().is_some_and(|is_removed| is_removed()) {
        return Ok(ExecutableToolResult {
            output: format!(
                "MCP server for tool \"{qualified_name}\" has been removed (plugin uninstalled or config deleted). Do not call this tool again."
            )
            .into(),
            is_error: Some(true),
            ..Default::default()
        });
    }
    let result = match call_tool(
        client.clone(),
        &tool_name,
        args.clone(),
        context.signal.clone(),
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            let error = unauthorized_error(&options, &qualified_name, error, client.clone()).await?;
            retry_after_reconnect(
                error,
                client,
                &tool_name,
                args,
                &context,
                &options,
                &qualified_name,
            )
            .await?
        }
    };
    mcp_result_to_executable_output(
        &result,
        &qualified_name,
        &McpOutputOptions {
            signal: Some(context.signal),
            attachment_store: options.attachment_store.clone(),
            originals: options.originals.as_ref().and_then(|originals| originals()),
            telemetry: options.telemetry.clone(),
            provider_type: options
                .provider_type
                .as_ref()
                .and_then(|provider_type| provider_type()),
        },
    )
    .await
}

async fn call_tool(
    client: Arc<dyn MCPClient>,
    tool_name: &str,
    args: HashMap<String, Value>,
    signal: AbortSignal,
) -> Result<MCPToolResult, WireAsyncError> {
    client.call_tool(tool_name, args, Some(signal)).await
}

async fn unauthorized_error(
    options: &McpToolOptions,
    qualified_name: &str,
    error: WireAsyncError,
    client: Arc<dyn MCPClient>,
) -> Result<WireAsyncError, WireAsyncError> {
    let unauthorized = match &options.on_unauthorized {
        Some(handler) => handler(&error, client).await,
        None => false,
    };
    if !unauthorized {
        return Ok(error);
    }
    let server_name = options.server_name.as_deref().unwrap_or(qualified_name);
    Err(Box::new(Error2::with_options(
        MCP_OAUTH_FAILED,
        format!(
            "MCP server \"{server_name}\" rejected the call with 401 Unauthorized and is now marked needs-auth. Call the {} tool to complete the OAuth login, then retry the original call.",
            qualify_mcp_tool_name(server_name, "authenticate")
        ),
        Error2Options {
            cause: Some(error),
            ..Default::default()
        },
    )))
}

async fn retry_after_reconnect(
    error: WireAsyncError,
    client: Arc<dyn MCPClient>,
    tool_name: &str,
    args: HashMap<String, Value>,
    context: &ExecutableToolContext,
    options: &McpToolOptions,
    qualified_name: &str,
) -> Result<MCPToolResult, WireAsyncError> {
    let reconnect = options.reconnect.clone();
    let is_unrecoverable = |error: &WireAsyncError| {
        context.signal.aborted()
            || is_abort_error(error.as_ref())
            || !is_mcp_transport_failure(error.as_ref())
            || is_mcp_malformed_result_error(error.as_ref())
    };
    if reconnect.is_none() || is_unrecoverable(&error) {
        return Err(error);
    }
    let mut failure = error;
    if !is_mcp_connection_closed_error(failure.as_ref()) {
        let alive = probe_mcp_liveness(client.as_ref(), context.signal.clone()).await;
        if context.signal.aborted() {
            let reason = context
                .signal
                .reason()
                .unwrap_or_else(|| Arc::new(crate::_base::utils::abort::abort_error()));
            return Err(Box::new(reason) as WireAsyncError);
        }
        if alive {
            match call_tool(
                client.clone(),
                tool_name,
                args.clone(),
                context.signal.clone(),
            )
            .await
            {
                Ok(result) => return Ok(result),
                Err(retry_error) => {
                    let retry_error = unauthorized_error(
                        options,
                        qualified_name,
                        retry_error,
                        client.clone(),
                    )
                    .await?;
                    if is_unrecoverable(&retry_error) {
                        return Err(retry_error);
                    }
                    failure = retry_error;
                }
            }
        }
    }
    if let Some(on_update) = &context.on_update {
        on_update(ToolUpdate::text(
            "status",
            "MCP connection lost — reconnecting…",
        ));
    }
    let fresh_client = match reconnect.unwrap()(Some(context.signal.clone())).await {
        Ok(client) => client,
        Err(reconnect_error) => {
            if context.signal.aborted() || is_abort_error(reconnect_error.as_ref()) {
                return Err(reconnect_error);
            }
            let failure: SharedError = failure.into();
            let reconnect_error: SharedError = reconnect_error.into();
            let failure_message = to_error_message(&UnknownError::Error(failure), false);
            let reconnect_message =
                to_error_message(&UnknownError::Error(reconnect_error.clone()), false);
            return Err(Box::new(Error2::with_options(
                MCP_STARTUP_FAILED,
                format!(
                    "{failure_message} (reconnecting the MCP server also failed: {reconnect_message})"
                ),
                Error2Options {
                    cause: Some(Box::new(reconnect_error)),
                    ..Default::default()
                },
            )));
        }
    };
    let Some(fresh_client) = fresh_client else {
        return Err(failure);
    };
    match call_tool(
        fresh_client.clone(),
        tool_name,
        args,
        context.signal.clone(),
    )
    .await
    {
        Ok(result) => Ok(result),
        Err(final_error) => {
            let final_error = unauthorized_error(
                options,
                qualified_name,
                final_error,
                fresh_client,
            )
            .await?;
            Err(final_error)
        }
    }
}
