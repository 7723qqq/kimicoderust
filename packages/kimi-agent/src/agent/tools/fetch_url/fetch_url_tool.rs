use std::sync::{Arc, LazyLock};

use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::_base::utils::abort::{AbortSignal, abortable};
use crate::_base::utils::proxy::is_proxy_configured;
use crate::agent::tool_executor::tool_executor::{
    ExecutableTool, ExecutableToolContext, ExecutableToolResult, RunnableToolExecution, ToolExecution,
};
use crate::tool::tool_contract::{RepoMutation, ToolSideEffect};
use crate::app::web::web::IWebFetchService;
use crate::app::web::tools::fetch_url_types::UrlFetchOptions;
use crate::app::web::web::{HttpFetchError, UrlFetchKind};
use crate::tool::input_schema::to_input_json_schema;
use crate::tool::output_accumulator::{ToolOutputAccumulator, ToolOutputAccumulatorResult};
use crate::tool::rule_match::{literal_rule_pattern, matches_glob_rule_subject};
use crate::tool::tool_contract::ToolAccesses;
use crate::tool::tool_input_display::ToolInputDisplay;
use crate::wire::record::WireAsyncError;

const DESCRIPTION: &str = r#"Fetch content from a URL. The content is returned either as the main text extracted from the page, or as the full response body verbatim; a note at the top of the result states which of the two you received, so you can judge how complete it is. Use this when you need to read a specific web page.

Only fully-formed public `http`/`https` URLs are supported; other schemes and private or loopback addresses are not fetched. Very large pages may be truncated or refused. The fetch carries no login or session for the target site, so pages behind authentication (private repositories, internal dashboards) return a login page or an error instead of the real content — if the text you get back looks like a generic landing or sign-in page, treat that as the login wall, not the answer, and reach the content through a credentialed route (an authenticated CLI or MCP tool) instead.
"#;

static PARAMETERS: LazyLock<Value> = LazyLock::new(|| {
    to_input_json_schema(json!({
        "type": "object",
        "properties": {
            "url": {
                "type": "string",
                "description": "The URL to fetch content from."
            }
        },
        "required": ["url"]
    }))
});

#[derive(Debug, Clone, Deserialize)]
pub struct FetchURLInput {
    pub url: String,
}

#[derive(Clone)]
pub struct FetchURLTool {
    web_fetch: Arc<dyn IWebFetchService>,
}

impl FetchURLTool {
    pub fn new(web_fetch: Arc<dyn IWebFetchService>) -> Self {
        Self { web_fetch }
    }

    pub fn name(&self) -> &'static str {
        "FetchURL"
    }

    pub fn description(&self) -> &'static str {
        DESCRIPTION
    }

    pub fn parameters(&self) -> Map<String, Value> {
        PARAMETERS
            .as_object()
            .cloned()
            .expect("FetchURL input schema is an object")
    }

    pub fn resolve_execution_value(&self, input: Value) -> ToolExecution {
        match serde_json::from_value::<FetchURLInput>(input) {
            Ok(args) => self.resolve_execution(args),
            Err(error) => ToolExecution::Error(ExecutableToolResult::error(format!(
                "Invalid arguments for FetchURL: {error}"
            ))),
        }
    }

    pub fn resolve_execution(&self, args: FetchURLInput) -> ToolExecution {
        let preview = if args.url.encode_utf16().count() > 50 {
            format!("{}…", utf16_prefix(&args.url, 50))
        } else {
            args.url.clone()
        };
        let rule_url = args.url.clone();
        let this = self.clone();
        ToolExecution::Runnable(RunnableToolExecution {
            accesses: Some(ToolAccesses::none()),
            display: Some(ToolInputDisplay::UrlFetch {
                url: args.url.clone(),
                method: None,
            }),
            description: Some(format!("Fetching: {preview}")),
            stop_batch_after_this: None,
            approval_rule: literal_rule_pattern(self.name(), &args.url),
            matches_rule: Some(Arc::new(move |rule_args| {
                matches_glob_rule_subject(rule_args, &rule_url)
            })),
            execute: Arc::new(move |context| {
                let this = this.clone();
                let args = args.clone();
                Box::pin(async move { this.execution(args, context).await })
            }),
        })
    }

    async fn execution(
        &self,
        args: FetchURLInput,
        context: ExecutableToolContext,
    ) -> Result<ExecutableToolResult, WireAsyncError> {
        let signal = AbortTokenBridge::new(context.signal.clone());
        let result = self
            .web_fetch
            .get_url_fetcher()
            .fetch(
                &args.url,
                UrlFetchOptions {
                    tool_call_id: Some(context.tool_call_id.clone()),
                    signal: Some(signal.token()),
                },
            )
            .await;
        match result {
            Ok(result) => {
                if result.content.is_empty() {
                    return Ok(ExecutableToolResult {
                        output: "The response body is empty.".into(),
                        is_error: Some(false),
                        ..Default::default()
                    });
                }
                let note = match result.kind {
                    UrlFetchKind::Passthrough => {
                        "The returned content is the full response body, returned verbatim."
                    }
                    UrlFetchKind::Extracted => {
                        "The returned content is the main text extracted from the page."
                    }
                };
                let cite_reminder = "If you use it in your answer, cite this page as a markdown link, e.g. [title](url).";
                let mut builder = ToolOutputAccumulator::new();
                builder.write(&format!("{note} {cite_reminder}\n\n{}", result.content));
                Ok(from_accumulator(builder.ok("", None)))
            }
            Err(error) => {
                if context.signal.aborted() {
                    return Err(error);
                }
                let message = error.to_string();
                if let Some(http_error) = error.downcast_ref::<HttpFetchError>() {
                    return Ok(ExecutableToolResult::error(format!(
                        "Failed to fetch URL. Status: {}. {message}",
                        http_error.status
                    )));
                }
                let cause_text = describe_error_cause(error.as_ref());
                let proxy_hint = if is_proxy_configured(&std::env::vars().collect()) {
                    ""
                } else {
                    " No HTTP(S)_PROXY is configured on this machine; if the site is unreachable by direct connection, set one and restart."
                };
                Ok(ExecutableToolResult::error(format!(
                    "Failed to fetch URL due to network error: {}. {message}{cause_text}{proxy_hint}",
                    args.url
                )))
            }
        }
    }
}

impl ExecutableTool for FetchURLTool {
    fn side_effect(&self) -> Option<ToolSideEffect> {
        Some(ToolSideEffect::Read)
    }

    fn repo_mutation(&self) -> Option<RepoMutation> {
        Some(RepoMutation::None)
    }

    fn name(&self) -> &str {
        FetchURLTool::name(self)
    }

    fn description(&self) -> &str {
        FetchURLTool::description(self)
    }

    fn parameters(&self) -> Map<String, Value> {
        FetchURLTool::parameters(self)
    }

    fn resolve_execution(
        &self,
        input: Value,
    ) -> futures_util::future::BoxFuture<'static, Result<ToolExecution, WireAsyncError>> {
        let execution = self.resolve_execution_value(input);
        Box::pin(async move { Ok(execution) })
    }
}

fn from_accumulator(result: ToolOutputAccumulatorResult) -> ExecutableToolResult {
    ExecutableToolResult {
        output: result.content.into(),
        is_error: Some(result.is_error),
        note: result.brief,
        spill: result.spill,
        ..Default::default()
    }
}

fn describe_error_cause(error: &(dyn std::error::Error + Send + Sync + 'static)) -> String {
    let mut parts = Vec::new();
    let mut current = error.source();
    for _ in 0..5 {
        let Some(error) = current else {
            break;
        };
        parts.push(error.to_string());
        current = error.source();
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" Cause: {}.", parts.join(" <- "))
    }
}

struct AbortTokenBridge {
    token: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl AbortTokenBridge {
    fn new(signal: AbortSignal) -> Self {
        let token = CancellationToken::new();
        let task_token = token.clone();
        let task = tokio::spawn(async move {
            let cancelled = task_token.clone().cancelled_owned();
            if abortable(cancelled, signal).await.is_err() {
                task_token.cancel();
            }
        });
        Self { token, task }
    }

    fn token(&self) -> CancellationToken {
        self.token.clone()
    }
}

impl Drop for AbortTokenBridge {
    fn drop(&mut self) {
        self.token.cancel();
        self.task.abort();
    }
}

fn utf16_prefix(value: &str, max_units: usize) -> &str {
    if value.encode_utf16().count() <= max_units {
        return value;
    }
    let mut units = 0;
    let mut end = 0;
    for (index, character) in value.char_indices() {
        let next = units + character.len_utf16();
        if next > max_units {
            break;
        }
        units = next;
        end = index + character.len_utf8();
    }
    &value[..end]
}
