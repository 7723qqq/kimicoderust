use std::sync::Arc;

use futures_util::future::BoxFuture;
use indexmap::IndexMap;
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use super::super::super::lsp::{ILspService, LspOperation, LspQueryRequest, LspQueryResult};
use super::super::super::protocol::{
    LspHoverContentPart, LspHoverContents, LspLocation, LspMarkedString, LspPosition,
};
use super::super::super::translate::uri_to_path;
use super::lsp::{LspInput, lsp_input_schema};
use crate::_base::utils::abort::{AbortSignal, abortable};
use crate::agent::tool_executor::tool_executor::{
    ExecutableTool, ExecutableToolResult, RunnableToolExecution, ToolExecution,
};
use crate::tool::tool_contract::{RepoMutation, ToolSideEffect};
use crate::session::sessionContext::sessionContext::ISessionContext;
use crate::tool::input_schema::to_input_json_schema;
use crate::wire::record::WireAsyncError;

pub const MAX_LOCATIONS: usize = 100;
pub const MAX_RESULT_CHARS: usize = 16_000;
pub const LSP_TOOL_DESCRIPTION: &str = include_str!("lsp.md");
pub const LSP_WORKSPACE_REQUIRED_MESSAGE: &str = "the session has no workspace root; the lsp tool needs a workspace to start a language server";

#[derive(Clone)]
pub struct LspTool {
    lsp: Arc<dyn ILspService>,
    session_context: Arc<dyn ISessionContext>,
}

impl LspTool {
    pub fn new(lsp: Arc<dyn ILspService>, session_context: Arc<dyn ISessionContext>) -> Self {
        Self {
            lsp,
            session_context,
        }
    }

    pub fn name(&self) -> &'static str {
        "lsp"
    }

    pub fn description(&self) -> &'static str {
        LSP_TOOL_DESCRIPTION
    }

    pub fn parameters(&self) -> Map<String, Value> {
        to_input_json_schema(lsp_input_schema())
            .as_object()
            .cloned()
            .expect("lsp input schema is an object")
    }

    pub fn resolve_execution_value(&self, input: Value) -> ToolExecution {
        match serde_json::from_value::<LspInput>(input) {
            Ok(args) => self.resolve_execution(&args),
            Err(error) => ToolExecution::Error(ExecutableToolResult::error(format!(
                "Invalid arguments for lsp: {error}"
            ))),
        }
    }

    pub fn resolve_execution(&self, args: &LspInput) -> ToolExecution {
        let operation = args.operation;
        let file_path = args.file_path.clone();
        let line = args.line;
        let character = args.character;
        let lsp = self.lsp.clone();
        let workspace_root = self.session_context.cwd();
        ToolExecution::Runnable(RunnableToolExecution {
            accesses: None,
            display: None,
            description: Some(format!(
                "Running LSP {} at {file_path}:{line}:{character}",
                operation_name(operation)
            )),
            stop_batch_after_this: None,
            approval_rule: "lsp".to_string(),
            matches_rule: None,
            execute: Arc::new(move |context| {
                let lsp = lsp.clone();
                let workspace_root = workspace_root.clone();
                let file_path = file_path.clone();
                Box::pin(async move {
                    if workspace_root.is_empty() {
                        return Ok(ExecutableToolResult::error(LSP_WORKSPACE_REQUIRED_MESSAGE));
                    }
                    let bridge = AbortTokenBridge::new(context.signal.clone());
                    let request = LspQueryRequest {
                        operation,
                        file_path,
                        position: LspPosition {
                            line: line.saturating_sub(1),
                            character: character.saturating_sub(1),
                        },
                        workspace_root,
                    };
                    match lsp.query(request, Some(bridge.token())).await {
                        Ok(result) => Ok(ExecutableToolResult {
                            output: render_result(&result).into(),
                            is_error: Some(false),
                            ..Default::default()
                        }),
                        Err(error) => {
                            Ok(ExecutableToolResult::error(error.message().to_string()))
                        }
                    }
                })
            }),
        })
    }
}

pub fn render_result(result: &LspQueryResult) -> String {
    match result {
        LspQueryResult::Hover { hover } => render_hover(hover.as_ref()),
        LspQueryResult::Locations { locations } => render_locations(locations),
    }
}

fn operation_name(operation: LspOperation) -> &'static str {
    match operation {
        LspOperation::GoToDefinition => "goToDefinition",
        LspOperation::FindReferences => "findReferences",
        LspOperation::GoToImplementation => "goToImplementation",
        LspOperation::Hover => "hover",
    }
}

fn render_locations(locations: &[LspLocation]) -> String {
    if locations.is_empty() {
        return "No locations found.".to_string();
    }
    let mut by_file: IndexMap<String, Vec<String>> = IndexMap::new();
    for location in locations.iter().take(MAX_LOCATIONS) {
        let path = uri_to_path(&location.uri);
        let line = location.range.start.line + 1;
        let character = location.range.start.character + 1;
        by_file
            .entry(path.clone())
            .or_default()
            .push(format!("{path}:{line}:{character}"));
    }
    let mut lines = Vec::new();
    for (path, entries) in by_file {
        lines.push(format!("{path}:"));
        lines.extend(entries.into_iter().map(|entry| format!("  {entry}")));
    }
    if locations.len() > MAX_LOCATIONS {
        lines.push(format!(
            "… {} more locations omitted",
            locations.len() - MAX_LOCATIONS
        ));
    }
    truncate(&lines.join("\n"))
}

fn render_hover(hover: Option<&super::super::super::protocol::LspHover>) -> String {
    let Some(hover) = hover else {
        return "No hover information available at this position.".to_string();
    };
    truncate(&hover_text(&hover.contents))
}

fn hover_text(contents: &LspHoverContents) -> String {
    match contents {
        LspHoverContents::Part(part) => hover_part_text(part),
        LspHoverContents::Parts(parts) => parts
            .iter()
            .map(hover_part_text)
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn hover_part_text(part: &LspHoverContentPart) -> String {
    match part {
        LspHoverContentPart::Markup(markup) => markup.value.clone(),
        LspHoverContentPart::Marked(LspMarkedString::Text(text)) => text.clone(),
        LspHoverContentPart::Marked(LspMarkedString::LanguageValue(value)) => value.value.clone(),
    }
}

fn truncate(text: &str) -> String {
    let units = text.encode_utf16().collect::<Vec<_>>();
    if units.len() <= MAX_RESULT_CHARS {
        return text.to_string();
    }
    format!(
        "{}\n… output truncated ({} more characters)",
        String::from_utf16_lossy(&units[..MAX_RESULT_CHARS]),
        units.len() - MAX_RESULT_CHARS
    )
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

impl ExecutableTool for LspTool {
    fn side_effect(&self) -> Option<ToolSideEffect> {
        Some(ToolSideEffect::Read)
    }

    fn repo_mutation(&self) -> Option<RepoMutation> {
        Some(RepoMutation::None)
    }

    fn name(&self) -> &str {
        LspTool::name(self)
    }

    fn description(&self) -> &str {
        LspTool::description(self)
    }

    fn parameters(&self) -> Map<String, Value> {
        LspTool::parameters(self)
    }

    fn resolve_execution(
        &self,
        input: Value,
    ) -> BoxFuture<'static, Result<ToolExecution, WireAsyncError>> {
        let execution = self.resolve_execution_value(input);
        Box::pin(async move { Ok(execution) })
    }
}
