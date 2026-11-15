use std::io;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::future::BoxFuture;
use serde_json::{Map, Value};

use crate::human::llm::message::{Deferred, ToolDescription};
use crate::human::tool::executor::{ToolExecuteInput, ToolExecutor, ToolResult};
use crate::wire::record::WireAsyncError;

pub type ToolExecute = Arc<
    dyn Fn(ToolExecuteInput) -> BoxFuture<'static, Result<ToolResult, WireAsyncError>>
        + Send
        + Sync,
>;

#[derive(Clone)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Map<String, Value>,
    pub deferred: Option<Deferred>,
    pub execute: ToolExecute,
}

impl ToolDefinition {
    pub fn description(&self) -> ToolDescription {
        ToolDescription {
            name: self.name.clone(),
            description: self.description.clone(),
            parameters: self.parameters.clone(),
            deferred: self.deferred,
        }
    }

    pub async fn execute(&self, input: ToolExecuteInput) -> Result<ToolResult, WireAsyncError> {
        (self.execute)(input).await
    }
}

#[async_trait]
impl ToolExecutor for ToolDefinition {
    async fn execute(&self, input: ToolExecuteInput) -> Result<ToolResult, WireAsyncError> {
        ToolDefinition::execute(self, input).await
    }
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

pub fn define_tool(tool: ToolDefinition) -> Result<ToolDefinition, WireAsyncError> {
    if js_trim(&tool.name).is_empty() {
        return Err(Box::new(io::Error::new(
            io::ErrorKind::InvalidInput,
            "tool name must not be empty",
        )));
    }
    Ok(tool)
}
