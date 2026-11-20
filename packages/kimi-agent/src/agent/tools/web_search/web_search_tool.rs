use std::sync::{Arc, LazyLock};

use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::_base::utils::abort::{AbortSignal, abortable, is_abort_error};
use crate::agent::tool_executor::tool_executor::{
    ExecutableTool, ExecutableToolContext, ExecutableToolResult, RunnableToolExecution, ToolExecution,
};
use crate::tool::tool_contract::{RepoMutation, ToolSideEffect};
use crate::app::auth::web_search::web_search::{WebSearchOptions, WebSearchProviderService};
use crate::tool::input_schema::to_input_json_schema;
use crate::tool::output_accumulator::{ToolOutputAccumulator, ToolOutputAccumulatorResult};
use crate::tool::rule_match::{literal_rule_pattern, matches_glob_rule_subject};
use crate::tool::tool_contract::ToolAccesses;
use crate::tool::tool_input_display::ToolInputDisplay;
use crate::wire::record::WireAsyncError;

const DESCRIPTION: &str = r#"Search the web for information. Use this when you need up-to-date information from the internet.

Each result includes its title, its URL, and a snippet, plus its source site and publication date when available. Results are short summaries, not full pages — when a result looks relevant, call the FetchURL tool on its URL to read the full page content. Fetch only the few URLs you actually need. Prefer specific queries, and refine the query if the results don't contain what you need.

When you rely on a result in your answer, cite its source URL so the user can verify it.
"#;

static PARAMETERS: LazyLock<Value> = LazyLock::new(|| {
    to_input_json_schema(json!({
        "type": "object",
        "properties": {
            "query": {
                "type": "string",
                "description": "The query text to search for."
            }
        },
        "required": ["query"]
    }))
});

#[derive(Debug, Clone, Deserialize)]
pub struct WebSearchInput {
    pub query: String,
}

#[derive(Clone)]
pub struct WebSearchTool {
    provider_service: Arc<dyn WebSearchProviderService>,
}

impl WebSearchTool {
    pub fn new(provider_service: Arc<dyn WebSearchProviderService>) -> Self {
        Self { provider_service }
    }

    pub fn name(&self) -> &'static str {
        "WebSearch"
    }

    pub fn description(&self) -> &'static str {
        DESCRIPTION
    }

    pub fn parameters(&self) -> Map<String, Value> {
        PARAMETERS
            .as_object()
            .cloned()
            .expect("WebSearch input schema is an object")
    }

    pub fn resolve_execution_value(&self, input: Value) -> ToolExecution {
        match serde_json::from_value::<WebSearchInput>(input) {
            Ok(args) => self.resolve_execution(args),
            Err(error) => ToolExecution::Error(ExecutableToolResult::error(format!(
                "Invalid arguments for WebSearch: {error}"
            ))),
        }
    }

    pub fn resolve_execution(&self, args: WebSearchInput) -> ToolExecution {
        let preview = if args.query.encode_utf16().count() > 40 {
            format!("{}…", utf16_prefix(&args.query, 40))
        } else {
            args.query.clone()
        };
        let rule_query = args.query.clone();
        let this = self.clone();
        ToolExecution::Runnable(RunnableToolExecution {
            accesses: Some(ToolAccesses::none()),
            display: Some(ToolInputDisplay::Search {
                query: args.query.clone(),
                scope: None,
            }),
            description: Some(format!("Searching: {preview}")),
            stop_batch_after_this: None,
            approval_rule: literal_rule_pattern(self.name(), &args.query),
            matches_rule: Some(Arc::new(move |rule_args| {
                matches_glob_rule_subject(rule_args, &rule_query)
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
        args: WebSearchInput,
        context: ExecutableToolContext,
    ) -> Result<ExecutableToolResult, WireAsyncError> {
        let Some(provider) = self.provider_service.get_web_search_provider() else {
            return Ok(ExecutableToolResult::error(
                "Web search is no longer configured; the provider was removed after this session started.",
            ));
        };
        let signal = AbortTokenBridge::new(context.signal.clone());
        let result = provider
            .search(
                &args.query,
                WebSearchOptions {
                    tool_call_id: Some(context.tool_call_id.clone()),
                    signal: Some(signal.token()),
                },
            )
            .await;
        match result {
            Ok(results) => {
                let mut builder = ToolOutputAccumulator::new();
                if results.is_empty() {
                    builder.write("No search results found.");
                    return Ok(from_accumulator(builder.ok("", None)));
                }
                let mut first = true;
                for result in results {
                    if !first {
                        builder.write("---\n\n");
                    }
                    first = false;
                    builder.write(&format!("Title: {}\n", result.title));
                    if let Some(site_name) = result.site_name.filter(|value| !value.is_empty()) {
                        builder.write(&format!("Site: {site_name}\n"));
                    }
                    if let Some(date) = result.date.filter(|value| !value.is_empty()) {
                        builder.write(&format!("Date: {date}\n"));
                    }
                    builder.write(&format!("URL: {}\n", result.url));
                    builder.write(&format!("Snippet: {}\n\n", result.snippet));
                }
                builder.write("When you rely on a result in your answer, cite it inline as a markdown link, e.g. [title](url).");
                Ok(from_accumulator(builder.ok("", None)))
            }
            Err(error) => {
                if context.signal.aborted() {
                    return Err(error);
                }
                Ok(ExecutableToolResult::error(classify_search_error(
                    error.as_ref(),
                )))
            }
        }
    }
}

impl ExecutableTool for WebSearchTool {
    fn side_effect(&self) -> Option<ToolSideEffect> {
        Some(ToolSideEffect::Read)
    }

    fn repo_mutation(&self) -> Option<RepoMutation> {
        Some(RepoMutation::None)
    }

    fn name(&self) -> &str {
        WebSearchTool::name(self)
    }

    fn description(&self) -> &str {
        WebSearchTool::description(self)
    }

    fn parameters(&self) -> Map<String, Value> {
        WebSearchTool::parameters(self)
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

fn classify_search_error(error: &(dyn std::error::Error + Send + Sync + 'static)) -> String {
    let name = if is_abort_error(error) {
        "AbortError"
    } else if error.is::<tokio::time::error::Elapsed>() {
        "TimeoutError"
    } else if error.is::<reqwest::Error>() {
        "TypeError"
    } else if let Some(error) = error.downcast_ref::<kimi_core::primitives::errors::Error2>() {
        error.name()
    } else {
        ""
    };
    let message = error.to_string();
    let lower = message.to_lowercase();
    if name == "AbortError" || lower.contains("abort") {
        return format!("Search cancelled: {message}");
    }
    if name == "TimeoutError" || lower.contains("timed out") || lower.contains("timeout") {
        return format!("Search timed out: {message}");
    }
    if lower.contains("401") || lower.contains("unauthorized") || lower.contains("auth") {
        return format!("Search failed (authentication): {message}");
    }
    if lower.contains("http ")
        || lower.contains("network")
        || lower.contains("fetch")
        || name == "TypeError"
    {
        return format!("Search failed (network): {message}");
    }
    format!("Search failed: {message}")
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
