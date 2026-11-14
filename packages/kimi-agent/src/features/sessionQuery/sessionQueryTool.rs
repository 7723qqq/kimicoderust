use std::sync::Arc;

use futures_util::future::BoxFuture;
use kimi_core::primitives::errors::Error2;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use super::events::{SessionEventSearchRequest, SessionSearchRequest};
use super::sessionQueryService::SessionQueryService;
use super::toolContract::{
    SessionQueryOperation, SessionQueryToolInput, format_validation_error,
    session_query_tool_input_schema,
};
use super::toolInput::{
    EventFilterInput, EventSearchInput, SessionSearchInput, SessionTraceInput, build_event_filters,
    build_session_filters, normalize_query,
};
use super::toolPresentation::{format_event_search, format_session_search, format_session_trace};
use super::types::SessionResultFilter;
use crate::_base::utils::abort::abort_error;
use crate::agent::scope_context::scopeContext::IAgentScopeContext;
use crate::agent::tool_executor::tool_executor::{
    ExecutableTool, ExecutableToolResult, RunnableToolExecution, ToolExecution,
};
use crate::i18n::LocalizedText;
use crate::session::sessionContext::sessionContext::ISessionContext;
use crate::tool::input_schema::to_input_json_schema;
use crate::wire::record::WireAsyncError;

pub const MAX_SEARCH_RESULTS: usize = 20;
pub const SESSION_QUERY_TOOL_NAME: &str = "session_query";
pub const SESSION_QUERY_DESCRIPTION: &str = include_str!("session-query.md");

#[derive(Clone)]
pub struct SessionQueryTool {
    query: Arc<SessionQueryService>,
    session_context: Arc<dyn ISessionContext>,
    scope_context: Arc<dyn IAgentScopeContext>,
}

impl SessionQueryTool {
    pub fn new(
        query: Arc<SessionQueryService>,
        session_context: Arc<dyn ISessionContext>,
        scope_context: Arc<dyn IAgentScopeContext>,
    ) -> Self {
        Self {
            query,
            session_context,
            scope_context,
        }
    }

    pub fn name(&self) -> &'static str {
        SESSION_QUERY_TOOL_NAME
    }

    pub fn side_effect(&self) -> &'static str {
        "read"
    }

    pub fn repo_mutation(&self) -> &'static str {
        "none"
    }

    pub fn description(&self) -> &'static str {
        SESSION_QUERY_DESCRIPTION
    }

    pub fn parameters(&self) -> Map<String, Value> {
        to_input_json_schema(session_query_tool_input_schema())
            .as_object()
            .cloned()
            .expect("session_query input schema is an object")
    }

    pub fn resolve_execution_value(&self, input: Value) -> ToolExecution {
        match serde_json::from_value::<SessionQueryToolInput>(input) {
            Ok(args) => self.resolve_execution(&args),
            Err(error) => ToolExecution::Error(ExecutableToolResult::error(format!(
                "Invalid arguments for session_query: {error}"
            ))),
        }
    }

    pub fn resolve_execution(&self, args: &SessionQueryToolInput) -> ToolExecution {
        let tool = self.clone();
        let args = args.clone();
        ToolExecution::Runnable(RunnableToolExecution {
            accesses: None,
            display: None,
            description: Some(format!("Session query: {}", operation_name(args.operation))),
            stop_batch_after_this: None,
            approval_rule: SESSION_QUERY_TOOL_NAME.to_string(),
            matches_rule: None,
            execute: Arc::new(move |context| {
                let tool = tool.clone();
                let args = args.clone();
                Box::pin(async move {
                    if context.signal.aborted() {
                        return Err(Box::new(abort_error()) as WireAsyncError);
                    }
                    match tokio::task::spawn_blocking(move || tool.execution(&args)).await {
                        Ok(result) => Ok(result),
                        Err(error) => Err(Box::new(error) as WireAsyncError),
                    }
                })
            }),
        })
    }

    fn execution(&self, args: &SessionQueryToolInput) -> ExecutableToolResult {
        if self.scope_context.agent_id() != crate::callbacks::MAIN_AGENT_ID {
            return ExecutableToolResult::error(
                LocalizedText::new("toolsV2.sessionQuery.mainAgentOnly").render(),
            );
        }
        match args.operation {
            SessionQueryOperation::SessionSearch => finish(self.session_search(args)),
            SessionQueryOperation::EventSearch => finish(self.event_search(args)),
            SessionQueryOperation::SessionTrace => finish(self.session_trace(args)),
        }
    }

    fn session_search(&self, args: &SessionQueryToolInput) -> Result<ExecutableToolResult, Error2> {
        let parsed = parse_operation_args::<SessionSearchInput>(args)?;
        let cwd = self.session_context.cwd();
        let query = normalize_query(&parsed.query)?;
        let mut session_filters = build_session_filters(&parsed)?;
        session_filters.push(SessionResultFilter::Cwd {
            values: vec![(!cwd.is_empty()).then_some(cwd)],
        });
        let event_filters = build_event_filters(&EventFilterInput {
            seq_from: parsed.event_seq_from,
            seq_to: parsed.event_seq_to,
            time_from: parsed.event_time_from,
            time_to: parsed.event_time_to,
            event_types: parsed.event_types,
        })?;
        let page = self.query.search_sessions(SessionSearchRequest {
            query,
            session_filters,
            event_filters,
            limit: Some(MAX_SEARCH_RESULTS),
            cursor: None,
        })?;
        let capped = page.next_cursor.is_some();
        Ok(ExecutableToolResult {
            output: format_session_search(&page.items, capped).into(),
            is_error: Some(false),
            ..Default::default()
        })
    }

    fn event_search(&self, args: &SessionQueryToolInput) -> Result<ExecutableToolResult, Error2> {
        let parsed = parse_operation_args::<EventSearchInput>(args)?;
        let session_id = parsed
            .session_id
            .clone()
            .unwrap_or_else(|| self.session_context.session_id());
        let query = normalize_query(&parsed.query)?;
        let filters = build_event_filters(&EventFilterInput {
            seq_from: parsed.seq_from,
            seq_to: parsed.seq_to,
            time_from: parsed.time_from,
            time_to: parsed.time_to,
            event_types: parsed.event_types,
        })?;
        let page = self.query.search_events(SessionEventSearchRequest {
            session_id: session_id.clone(),
            query,
            filters,
            limit: Some(MAX_SEARCH_RESULTS),
            cursor: None,
        })?;
        let capped = page.next_cursor.is_some();
        Ok(ExecutableToolResult {
            output: format_event_search(&session_id, &page.items, capped).into(),
            is_error: Some(false),
            ..Default::default()
        })
    }

    fn session_trace(&self, args: &SessionQueryToolInput) -> Result<ExecutableToolResult, Error2> {
        let parsed = parse_operation_args::<SessionTraceInput>(args)?;
        let session_id = parsed
            .session_id
            .unwrap_or_else(|| self.session_context.session_id());
        let trace = self.query.trace_session_lineage(&session_id)?;
        Ok(ExecutableToolResult {
            output: format_session_trace(&trace).into(),
            is_error: Some(false),
            ..Default::default()
        })
    }
}

fn parse_operation_args<T: DeserializeOwned>(
    args: &SessionQueryToolInput,
) -> Result<T, Error2> {
    let value = Value::Object(args.operation_args.clone().unwrap_or_default());
    serde_json::from_value(value).map_err(|error| {
        Error2::new(
            super::errors::SESSION_QUERY_INVALID_FILTER,
            format_validation_error(&error.to_string()),
        )
    })
}

fn finish(outcome: Result<ExecutableToolResult, Error2>) -> ExecutableToolResult {
    match outcome {
        Ok(result) => result,
        Err(error) => ExecutableToolResult::error(error.message().to_string()),
    }
}

fn operation_name(operation: SessionQueryOperation) -> &'static str {
    match operation {
        SessionQueryOperation::SessionSearch => "session_search",
        SessionQueryOperation::EventSearch => "event_search",
        SessionQueryOperation::SessionTrace => "session_trace",
    }
}

impl ExecutableTool for SessionQueryTool {
    fn name(&self) -> &str {
        SessionQueryTool::name(self)
    }

    fn description(&self) -> &str {
        SessionQueryTool::description(self)
    }

    fn parameters(&self) -> Map<String, Value> {
        SessionQueryTool::parameters(self)
    }

    fn resolve_execution(
        &self,
        input: Value,
    ) -> BoxFuture<'static, Result<ToolExecution, WireAsyncError>> {
        let execution = self.resolve_execution_value(input);
        Box::pin(async move { Ok(execution) })
    }
}
