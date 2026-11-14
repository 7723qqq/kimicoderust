use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};

use super::todoList::{TodoListInput, TodoListInputItem, todo_list_input_schema};
use super::super::super::todoItem::{
    TODO_LIST_HEADER, TodoItem, TodoKind, TodoStatus, render_todo_list, todo_progress,
};
use super::super::super::todoListReminder::todo_list_write_stats_before;
use crate::features::todo::todo_service::AgentTodoService;
use crate::agent::actor_service::services::TelemetryService;
use crate::agent::context_memory::context_memory::IAgentContextMemoryService;
use crate::agent::tool_executor::tool_executor::{
    ExecutableTool, ExecutableToolResult, RunnableToolExecution, ToolExecution,
};
use crate::tool::input_schema::to_input_json_schema;
use crate::tool::tool_input_display::{
    TodoItemKind as DisplayTodoItemKind, TodoListItem as DisplayTodoItem,
};
use crate::wire::record::WireAsyncError;

pub const TODO_LIST_DESCRIPTION: &str = include_str!("todo-list.md");

#[derive(Clone)]
pub struct TodoListTool {
    todo: Arc<AgentTodoService>,
    context: Arc<dyn IAgentContextMemoryService>,
    telemetry: Arc<dyn TelemetryService>,
}

impl TodoListTool {
    pub fn new(
        todo: Arc<AgentTodoService>,
        context: Arc<dyn IAgentContextMemoryService>,
        telemetry: Arc<dyn TelemetryService>,
    ) -> Self {
        Self {
            todo,
            context,
            telemetry,
        }
    }

    pub fn name(&self) -> &'static str {
        "TodoList"
    }

    pub fn side_effect(&self) -> &'static str {
        "read"
    }

    pub fn repo_mutation(&self) -> &'static str {
        "none"
    }

    pub fn description(&self) -> &'static str {
        TODO_LIST_DESCRIPTION
    }

    pub fn parameters(&self) -> Map<String, Value> {
        to_input_json_schema(todo_list_input_schema())
            .as_object()
            .cloned()
            .expect("TodoList input schema is an object")
    }

    pub fn resolve_execution_value(&self, input: Value) -> ToolExecution {
        match serde_json::from_value::<TodoListInput>(input) {
            Ok(args) => self.resolve_execution(&args),
            Err(error) => ToolExecution::Error(ExecutableToolResult::error(format!(
                "Invalid arguments for TodoList: {error}"
            ))),
        }
    }

    pub fn resolve_execution(&self, args: &TodoListInput) -> ToolExecution {
        let description = match &args.todos {
            None => "Reading todo list",
            Some(todos) if todos.is_empty() => "Clearing todo list",
            Some(_) => "Updating todo list",
        };
        let items = match &args.todos {
            Some(todos) => display_items_from_input(todos),
            None => display_items_from_stored(&self.todo.get()),
        };
        let tool = self.clone();
        let args = args.clone();
        ToolExecution::Runnable(RunnableToolExecution {
            accesses: None,
            display: Some(crate::tool::tool_input_display::ToolInputDisplay::TodoList {
                items,
            }),
            description: Some(description.to_string()),
            stop_batch_after_this: None,
            approval_rule: "TodoList".to_string(),
            matches_rule: None,
            execute: Arc::new(move |context| {
                let tool = tool.clone();
                let args = args.clone();
                Box::pin(async move { tool.execution(&args, &context).await })
            }),
        })
    }

    async fn execution(
        &self,
        args: &TodoListInput,
        context: &crate::agent::tool_executor::tool_executor::ExecutableToolContext,
    ) -> Result<ExecutableToolResult, WireAsyncError> {
        let Some(todos) = &args.todos else {
            return Ok(ExecutableToolResult {
                output: render_todo_list(&self.todo.get(), TODO_LIST_HEADER).into(),
                is_error: Some(false),
                ..Default::default()
            });
        };

        let next: Vec<TodoItem> = todos
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, item)| item.into_item(index))
            .collect();
        self.todo
            .replace(next)
            .await
            .map_err(|error| wire_error(error))?;
        let stored = self.todo.get();
        let progress = todo_progress(&stored);
        let stats = todo_list_write_stats_before(&self.context.get(), &context.tool_call_id);
        self.telemetry.track2(
            "todo_list_written",
            json!({
                "turn_id": context.turn_id,
                "item_count": progress.total,
                "open_count": progress.open,
                "steps_since_write": stats.steps_since_last_write,
                "after_reminder": stats.reminded_since_last_write,
            }),
        );
        if stored.is_empty() {
            return Ok(ExecutableToolResult {
                output: "Todo list cleared.".into(),
                is_error: Some(false),
                ..Default::default()
            });
        }
        let updated = format!(
            "Todo list updated.\n{}",
            render_todo_list(&stored, TODO_LIST_HEADER)
        );
        let output = match next_step_hint(&progress) {
            Some(hint) => format!("{updated}\n\n{hint}"),
            None => updated,
        };
        Ok(ExecutableToolResult {
            output: output.into(),
            is_error: Some(false),
            ..Default::default()
        })
    }
}

fn display_items_from_input(items: &[TodoListInputItem]) -> Vec<DisplayTodoItem> {
    items
        .iter()
        .map(|item| DisplayTodoItem {
            id: item.id.clone(),
            parent_id: item.parent_id.clone(),
            kind: item.kind.map(display_kind),
            title: item.title.clone(),
            status: item.status.as_str().to_string(),
            progress: item.progress,
        })
        .collect()
}

fn display_items_from_stored(items: &[TodoItem]) -> Vec<DisplayTodoItem> {
    items
        .iter()
        .map(|item| DisplayTodoItem {
            id: Some(item.id.clone()),
            parent_id: Some(item.parent_id.clone()),
            kind: Some(display_kind(item.kind)),
            title: item.title.clone(),
            status: item.status.as_str().to_string(),
            progress: item.progress,
        })
        .collect()
}

fn display_kind(kind: TodoKind) -> DisplayTodoItemKind {
    match kind {
        TodoKind::Milestone => DisplayTodoItemKind::Milestone,
        TodoKind::Task => DisplayTodoItemKind::Task,
    }
}

pub fn next_step_hint(progress: &super::super::super::todoItem::TodoProgress) -> Option<String> {
    if progress.open == 0 {
        return None;
    }
    let Some((current, others)) = progress.in_progress.split_first() else {
        return Some(
            "No item is in_progress. Mark the one you are working on as in_progress.".to_string(),
        );
    };
    if !others.is_empty() {
        return Some(
            "Several items are in_progress. Keep only the one you are working on now in_progress."
                .to_string(),
        );
    }
    if progress.open == 1 {
        return Some(format!(
            "\"{}\" is the last open item. Once it is finished, mark it done before you write your final message; if it stays blocked or unfinished, keep it in_progress and say what remains in your reply.",
            current.title
        ));
    }
    Some(format!(
        "When \"{}\" is finished, mark it done and set the next item in_progress; that update can go in the same response as your next tool call.",
        current.title
    ))
}

fn wire_error(message: String) -> WireAsyncError {
    Box::new(kimi_core::primitives::errors::Error2::new(
        kimi_core::primitives::errors::INTERNAL_CODE,
        message,
    ))
}

impl ExecutableTool for TodoListTool {
    fn name(&self) -> &str {
        TodoListTool::name(self)
    }

    fn description(&self) -> &str {
        TodoListTool::description(self)
    }

    fn parameters(&self) -> Map<String, Value> {
        TodoListTool::parameters(self)
    }

    fn resolve_execution(
        &self,
        input: Value,
    ) -> BoxFuture<'static, Result<ToolExecution, WireAsyncError>> {
        let execution = self.resolve_execution_value(input);
        Box::pin(async move { Ok(execution) })
    }
}
