use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};

use super::notifyOutput::{NOTIFY_USER_DELIVERED_OUTPUT, NOTIFY_USER_SUPPRESSED_OUTPUT};
use crate::features::notify::notify_user_availability::notify_user_available;
use crate::features::notify::notify_user_nudge::notify_streak_before;
use super::notifyUser::NOTIFY_USER_TOOL_NAME;
use crate::agent::actor_service::services::{BootstrapService, FlagService, TelemetryService};
use crate::agent::context_memory::context_memory::IAgentContextMemoryService;
use crate::agent::tool_executor::tool_executor::{
    ExecutableTool, ExecutableToolResult, RunnableToolExecution, ToolExecution,
};
use crate::tool::tool_contract::{RepoMutation, ToolSideEffect};
use crate::tool::input_schema::to_input_json_schema;
use crate::tool::tool_contract::ToolAccesses;
use crate::tools::notify_user::NotifyUserInput;
use crate::wire::record::WireAsyncError;

pub const NOTIFY_USER_EMPTY_TITLE: &str = "title must not be empty.";
pub const NOTIFY_USER_MULTILINE_TITLE: &str = "title must be a single line.";
pub const NOTIFY_USER_INVALID_TITLE: &str = "title must be a string.";
pub const NOTIFY_USER_EMPTY_MESSAGE: &str = "message must not be empty.";
pub const NOTIFY_USER_DESCRIPTION: &str = include_str!("notify-user.md");

#[derive(Clone)]
pub struct NotifyUserTool {
    flags: Arc<dyn FlagService>,
    bootstrap: Arc<dyn BootstrapService>,
    context: Arc<dyn IAgentContextMemoryService>,
    telemetry: Arc<dyn TelemetryService>,
}

impl NotifyUserTool {
    pub fn new(
        flags: Arc<dyn FlagService>,
        bootstrap: Arc<dyn BootstrapService>,
        context: Arc<dyn IAgentContextMemoryService>,
        telemetry: Arc<dyn TelemetryService>,
    ) -> Self {
        Self {
            flags,
            bootstrap,
            context,
            telemetry,
        }
    }

    pub fn name(&self) -> &'static str {
        NOTIFY_USER_TOOL_NAME
    }

    pub fn description(&self) -> &'static str {
        NOTIFY_USER_DESCRIPTION
    }

    pub fn parameters(&self) -> Map<String, Value> {
        to_input_json_schema(crate::tools::notify_user::notify_user_tool_def().input_schema)
            .as_object()
            .cloned()
            .expect("NotifyUser input schema is an object")
    }

    pub fn validate_input(input: &NotifyUserInput) -> Result<(), &'static str> {
        if let Some(title) = &input.title {
            if trim_js(title).is_empty() {
                return Err(NOTIFY_USER_EMPTY_TITLE);
            }
            if title.contains(['\r', '\n']) {
                return Err(NOTIFY_USER_MULTILINE_TITLE);
            }
        }
        if trim_js(&input.message).is_empty() {
            return Err(NOTIFY_USER_EMPTY_MESSAGE);
        }
        Ok(())
    }

    pub fn output(displayed: bool) -> &'static str {
        if displayed {
            NOTIFY_USER_DELIVERED_OUTPUT
        } else {
            NOTIFY_USER_SUPPRESSED_OUTPUT
        }
    }

    pub fn resolve_execution_value(&self, input: Value) -> ToolExecution {
        if let Some(title) = input.get("title")
            && !title.is_string()
        {
            return ToolExecution::Error(ExecutableToolResult::error(NOTIFY_USER_INVALID_TITLE));
        }
        match serde_json::from_value::<NotifyUserInput>(input) {
            Ok(args) => self.resolve_execution(&args),
            Err(error) => ToolExecution::Error(ExecutableToolResult::error(format!(
                "Invalid arguments for NotifyUser: {error}"
            ))),
        }
    }

    pub fn resolve_execution(&self, args: &NotifyUserInput) -> ToolExecution {
        if let Err(message) = Self::validate_input(args) {
            return ToolExecution::Error(ExecutableToolResult::error(message));
        }
        let args = args.clone();
        let flags = self.flags.clone();
        let bootstrap = self.bootstrap.clone();
        let context = self.context.clone();
        let telemetry = self.telemetry.clone();
        ToolExecution::Runnable(RunnableToolExecution {
            accesses: Some(ToolAccesses::none()),
            display: None,
            description: Some("Notifying the user".to_string()),
            stop_batch_after_this: None,
            approval_rule: NOTIFY_USER_TOOL_NAME.to_string(),
            matches_rule: None,
            execute: Arc::new(move |execution_context| {
                let flags = flags.clone();
                let bootstrap = bootstrap.clone();
                let context = context.clone();
                let telemetry = telemetry.clone();
                let args = args.clone();
                Box::pin(async move {
                    let displayed = notify_user_available(flags.as_ref(), bootstrap.as_ref());
                    let streak = notify_streak_before(&context.get(), &execution_context.tool_call_id);
                    let title_chars = args
                        .title
                        .as_ref()
                        .map(|title| title.encode_utf16().count())
                        .unwrap_or(0);
                    telemetry.track2(
                        "notify_user_sent",
                        json!({
                            "turn_id": execution_context.turn_id,
                            "rounds_since_notify": streak.rounds,
                            "after_nudge": streak.nudges > 0,
                            "title_chars": title_chars,
                            "message_chars": args.message.encode_utf16().count(),
                            "displayed": displayed,
                        }),
                    );
                    Ok(ExecutableToolResult {
                        output: Self::output(displayed).into(),
                        is_error: Some(false),
                        ..Default::default()
                    })
                })
            }),
        })
    }
}

fn trim_js(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

fn is_js_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'
            | '\u{000a}'
            | '\u{000b}'
            | '\u{000c}'
            | '\u{000d}'
            | '\u{0020}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'
            | '\u{2001}'
            | '\u{2002}'
            | '\u{2003}'
            | '\u{2004}'
            | '\u{2005}'
            | '\u{2006}'
            | '\u{2007}'
            | '\u{2008}'
            | '\u{2009}'
            | '\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

impl ExecutableTool for NotifyUserTool {
    fn side_effect(&self) -> Option<ToolSideEffect> {
        Some(ToolSideEffect::Read)
    }

    fn repo_mutation(&self) -> Option<RepoMutation> {
        Some(RepoMutation::None)
    }

    fn name(&self) -> &str {
        NotifyUserTool::name(self)
    }

    fn description(&self) -> &str {
        NotifyUserTool::description(self)
    }

    fn parameters(&self) -> Map<String, Value> {
        NotifyUserTool::parameters(self)
    }

    fn resolve_execution(
        &self,
        input: Value,
    ) -> BoxFuture<'static, Result<ToolExecution, WireAsyncError>> {
        let execution = self.resolve_execution_value(input);
        Box::pin(async move { Ok(execution) })
    }
}
