use std::collections::HashSet;
use std::sync::{Arc, LazyLock};

use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::_base::utils::abort::{
    AbortController, abort_error, is_user_cancellation, user_cancellation_reason,
};
use crate::agent::tool_executor::tool_executor::{
    ExecutableToolContext, ExecutableToolResult, RunnableToolExecution, ToolExecution,
};
use crate::callbacks::HostCallbacks;
use crate::rpc::types::{
    AskQuestionItem, AskQuestionOption, AskQuestionRequest, AskQuestionResponse,
};
use crate::storage::TaskRunner;
use crate::storage::task_runner::{TaskOutcome, generate_task_id};
use crate::tool::input_schema::to_input_json_schema;
use crate::wire::record::WireAsyncError;

pub const QUESTION_DISMISSED_MESSAGE: &str = "User dismissed the question without answering.";
pub const QUESTION_UNSUPPORTED_FAILURE_MESSAGE: &str = "The connected client does not support interactive questions. Do NOT call this tool again. Ask the user directly in your text response instead.";
pub const QUESTION_NON_INTERACTIVE_MESSAGE: &str = "Nobody can answer questions in a non-interactive run.";
pub const BACKGROUND_UNAVAILABLE_MESSAGE: &str = "Background questions are not available for this agent because TaskList, TaskOutput, and TaskStop are not enabled.";

const BACKGROUND_DESCRIPTION: &str = "- Set background=true when you can keep working without the answer. This starts a background question task and returns a task_id immediately. The answer arrives automatically in a later turn — you do not need to poll, sleep, or check on it. Continue with other work; never fabricate or predict the answer.";
const QUESTION_UNIQUENESS_MESSAGE: &str = "Question texts must be unique across questions, and option labels must be unique within each question.";
const BASE_DESCRIPTION: &str = r#"Use this tool when you need to ask the user questions with structured options during execution. This allows you to:
1. Collect user preferences or requirements before proceeding
2. Resolve ambiguous or underspecified instructions
3. Let the user decide between implementation approaches as you work
4. Present concrete options when multiple valid directions exist

**When NOT to use:**
- When you can infer the answer from context — be decisive and proceed
- Trivial decisions that don't materially affect the outcome

Overusing this tool interrupts the user's flow. Only use it when the user's input genuinely changes your next action.

**Usage notes:**
- Question texts must be unique across the call, and option labels must be unique within each question
- Group related questions into one call to minimize interruptions
- List a recommended option first
- The result is JSON with an `answers` object keyed by question text; each value is the chosen option's label (comma-separated labels for multi_select, or the user's own words if they picked "Other"); if `answers` is empty and a `note` says the user dismissed it, they chose not to answer — do not treat this as selecting the recommended option; decide based on context and do not re-ask the same question"#;

static PARAMETERS_WITH_BACKGROUND: LazyLock<Value> =
    LazyLock::new(|| ask_user_question_input_schema(true));
static PARAMETERS_FOREGROUND_ONLY: LazyLock<Value> =
    LazyLock::new(|| ask_user_question_input_schema(false));

#[derive(Debug, Clone, Deserialize)]
pub struct AskUserQuestionInput {
    #[serde(default)]
    pub background: bool,
    pub questions: Vec<AskUserQuestionItemInput>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AskUserQuestionItemInput {
    pub question: String,
    #[serde(default)]
    pub header: String,
    pub options: Vec<AskUserQuestionOptionInput>,
    #[serde(default)]
    pub multi_select: Option<bool>,
    #[serde(rename = "multiSelect", default)]
    pub multi_select_camel: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AskUserQuestionOptionInput {
    pub label: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Clone)]
pub struct AskUserQuestionToolDependencies {
    pub callbacks: Arc<dyn HostCallbacks>,
    pub tasks: Option<Arc<TaskRunner>>,
    pub session_id: Option<String>,
    pub agent_id: Option<String>,
    pub non_interactive: bool,
    pub background_allowed: bool,
}

#[derive(Clone)]
pub struct AskUserQuestionTool {
    inner: Arc<AskUserQuestionToolDependencies>,
}

impl AskUserQuestionTool {
    pub fn new(dependencies: AskUserQuestionToolDependencies) -> Self {
        Self {
            inner: Arc::new(dependencies),
        }
    }

    pub fn name(&self) -> &'static str {
        "AskUserQuestion"
    }

    pub fn description(&self) -> String {
        let mut description = format!("{BASE_DESCRIPTION}\n");
        if self.allow_background() {
            description.push_str(BACKGROUND_DESCRIPTION);
        }
        description
    }
    pub fn side_effect(&self) -> &'static str {
        "read"
    }
    pub fn repo_mutation(&self) -> &'static str {
        "none"
    }

    pub fn parameters(&self) -> Map<String, Value> {
        if self.allow_background() {
            &PARAMETERS_WITH_BACKGROUND
        } else {
            &PARAMETERS_FOREGROUND_ONLY
        }
        .as_object()
        .cloned()
        .expect("AskUserQuestion input schema is an object")
    }

    pub fn resolve_execution_value(&self, input: Value) -> ToolExecution {
        match parse_ask_user_question_input(input) {
            Ok(args) => self.resolve_execution(args),
            Err(error) => ToolExecution::Error(ExecutableToolResult::error(error)),
        }
    }

    pub fn resolve_execution(&self, args: AskUserQuestionInput) -> ToolExecution {
        let is_background = args.background;
        let description = if is_background {
            format!(
                "Starting background question: {}",
                question_description(&args.questions)
            )
        } else {
            "Asking user questions".to_string()
        };
        let this = self.clone();
        ToolExecution::Runnable(RunnableToolExecution {
            accesses: None,
            display: None,
            description: Some(description),
            stop_batch_after_this: None,
            approval_rule: self.name().to_string(),
            matches_rule: None,
            execute: Arc::new(move |context| {
                let this = this.clone();
                let args = args.clone();
                Box::pin(async move { this.execution(args, context).await })
            }),
        })
    }

    async fn execution(
        &self,
        args: AskUserQuestionInput,
        context: ExecutableToolContext,
    ) -> Result<ExecutableToolResult, WireAsyncError> {
        if self.inner.non_interactive {
            return Ok(ExecutableToolResult::error(QUESTION_NON_INTERACTIVE_MESSAGE));
        }
        if args.background && !self.allow_background() {
            return Ok(ExecutableToolResult::error(BACKGROUND_UNAVAILABLE_MESSAGE));
        }
        if let Some(error) = question_uniqueness_error(&args.questions) {
            return Ok(ExecutableToolResult::error(error));
        }
        if args.background {
            self.execute_in_background(args, context)
        } else {
            self.execute_question(args, context).await
        }
    }

    fn allow_background(&self) -> bool {
        self.inner.background_allowed
    }

    fn execute_in_background(
        &self,
        args: AskUserQuestionInput,
        context: ExecutableToolContext,
    ) -> Result<ExecutableToolResult, WireAsyncError> {
        if context.signal.aborted() {
            return Err(abort_reason(&context.signal));
        }
        let Some(runner) = self.inner.tasks.as_ref() else {
            return Ok(ExecutableToolResult::error(BACKGROUND_UNAVAILABLE_MESSAGE));
        };
        let description = question_description(&args.questions);
        let task_id = generate_task_id("question");
        let question_count = args.questions.len();
        let tool_call_id = context.tool_call_id.clone();
        let this = self.clone();
        let mut task_context = context.clone();
        task_context.signal = AbortController::new().signal();
        let spawned = runner.spawn_question_task(
            self.inner.session_id.as_deref(),
            task_id.clone(),
            description,
            question_count,
            Some(tool_call_id.as_str()),
            async move {
                match this.execute_question(args, task_context).await {
                    Ok(result) if result.is_error == Some(true) => {
                        TaskOutcome::Failed(output_text(&result))
                    }
                    Ok(result) => TaskOutcome::Completed(output_text(&result)),
                    Err(error) => TaskOutcome::Failed(error.to_string()),
                }
            },
        );
        if let Err(error) = spawned {
            return Ok(ExecutableToolResult::error(error));
        }
        let status = runner
            .entry(&task_id)
            .and_then(|entry| {
                entry
                    .get("status")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "running".to_string());
        Ok(ExecutableToolResult {
            output: format!(
                "task_id: {task_id}\nstatus: {status}\nnext_step: Continue your work; the answer arrives automatically in a later message. Use TaskStop only to cancel the question."
            )
            .into(),
            is_error: Some(false),
            ..Default::default()
        })
    }

    async fn execute_question(
        &self,
        args: AskUserQuestionInput,
        context: ExecutableToolContext,
    ) -> Result<ExecutableToolResult, WireAsyncError> {
        let request = self.request(&args, &context);
        let question_id = request.question_id.clone();
        let mut guard = QuestionCancelGuard::new(self.inner.callbacks.clone(), question_id);
        let response = tokio::select! {
            biased;
            response = self.inner.callbacks.ask_question(request) => response,
            () = context.signal.cancelled() => {
                drop(guard);
                self.track_question_dismissed(&context);
                return Ok(dismissed_question_result());
            }
        };
        guard.disarm();
        match response {
            Ok(response) => {
                if response.cancelled == Some(true)
                    && response.reason.as_deref() == Some("unsupported")
                {
                    return Ok(ExecutableToolResult::error(QUESTION_UNSUPPORTED_FAILURE_MESSAGE));
                }
                if response.cancelled == Some(true) || response.answers.is_empty() {
                    self.track_question_dismissed(&context);
                    return Ok(dismissed_question_result());
                }
                self.track_question_answered(&context, &response);
                Ok(ExecutableToolResult {
                    output: json!({ "answers": response.answers }).to_string().into(),
                    is_error: Some(false),
                    ..Default::default()
                })
            }
            Err(error) => {
                if context.signal.aborted() {
                    return Err(abort_reason(&context.signal));
                }
                if error.contains("does not support interactive questions") {
                    Ok(ExecutableToolResult::error(
                        QUESTION_UNSUPPORTED_FAILURE_MESSAGE,
                    ))
                } else {
                    Ok(dismissed_question_result())
                }
            }
        }
    }

    fn request(
        &self,
        args: &AskUserQuestionInput,
        context: &ExecutableToolContext,
    ) -> AskQuestionRequest {
        AskQuestionRequest {
            question_id: generate_question_id(),
            turn_id: context.turn_id.to_string(),
            tool_call_id: context.tool_call_id.clone(),
            agent_id: self.inner.agent_id.clone(),
            session_id: self.inner.session_id.clone(),
            background: args.background,
            timeout_ms: None,
            questions: args
                .questions
                .iter()
                .map(|question| AskQuestionItem {
                    question: question.question.clone(),
                    header: Some(question.header.clone()),
                    options: question
                        .options
                        .iter()
                        .map(|option| AskQuestionOption {
                            label: option.label.clone(),
                            description: Some(option.description.clone()),
                        })
                        .collect(),
                    multi_select: question_multi_select(question),
                })
                .collect(),
        }
    }

    fn track_question_dismissed(&self, context: &ExecutableToolContext) {
        let mut event = json!({ "event": "question_dismissed" });
        if let Some(trace_id) = context.trace.as_ref().and_then(|trace| trace.trace_id.as_ref()) {
            event["trace_id"] = json!(trace_id);
        }
        self.inner.callbacks.telemetry(event);
    }

    fn track_question_answered(
        &self,
        context: &ExecutableToolContext,
        response: &AskQuestionResponse,
    ) {
        let mut event = json!({
            "event": "question_answered",
            "answered": response.answers.len(),
        });
        if let Some(trace_id) = context.trace.as_ref().and_then(|trace| trace.trace_id.as_ref()) {
            event["trace_id"] = json!(trace_id);
        }
        if let Some(method) = &response.method {
            event["method"] = json!(method);
        }
        self.inner.callbacks.telemetry(event);
    }
}

pub fn question_multi_select(item: &AskUserQuestionItemInput) -> bool {
    item.multi_select.unwrap_or(false)
}

pub fn question_uniqueness_error(questions: &[AskUserQuestionItemInput]) -> Option<String> {
    let mut texts = HashSet::new();
    for question in questions {
        if !texts.insert(&question.question) {
            return Some(format!(
                "Invalid questions: duplicate question text {}. {QUESTION_UNIQUENESS_MESSAGE} Rephrase the duplicates and call the tool again.",
                serde_json::to_string(&question.question).unwrap_or_default()
            ));
        }
        let mut labels = HashSet::new();
        for option in &question.options {
            if !labels.insert(&option.label) {
                return Some(format!(
                    "Invalid questions: duplicate option label {} in question {}. {QUESTION_UNIQUENESS_MESSAGE} Rephrase the duplicates and call the tool again.",
                    serde_json::to_string(&option.label).unwrap_or_default(),
                    serde_json::to_string(&question.question).unwrap_or_default()
                ));
            }
        }
    }
    None
}

pub fn ask_user_question_input_schema(with_background: bool) -> Value {
    let mut properties = json!({
        "questions": {
            "type": "array",
            "minItems": 1,
            "maxItems": 4,
            "description": "The questions to ask the user (1-4 questions).",
            "items": {
                "type": "object",
                "properties": {
                    "question": {
                        "type": "string",
                        "minLength": 1,
                        "description": "A specific, actionable question. End with '?'."
                    },
                    "header": {
                        "type": "string",
                        "default": "",
                        "description": "Short category tag (max 12 chars, e.g. 'Auth', 'Style')."
                    },
                    "options": {
                        "type": "array",
                        "minItems": 2,
                        "maxItems": 4,
                        "description": "2-4 meaningful, distinct options. Do NOT include an 'Other' option — the system adds one automatically.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "label": {
                                    "type": "string",
                                    "minLength": 1,
                                    "description": "Concise display text (1-5 words). If recommended, append '(Recommended)'."
                                },
                                "description": {
                                    "type": "string",
                                    "default": "",
                                    "description": "Brief explanation of trade-offs or implications."
                                }
                            },
                            "required": ["label"]
                        }
                    },
                    "multi_select": {
                        "type": "boolean",
                        "description": "Whether the user can select multiple options."
                    },
                    "multiSelect": {
                        "type": "boolean",
                        "description": "Deprecated camelCase alias of multi_select; prefer multi_select."
                    }
                },
                "required": ["question", "options"]
            }
        }
    });
    if with_background {
        properties.as_object_mut().expect("properties object").insert(
            "background".to_string(),
            json!({
                "type": "boolean",
                "default": false,
                "description": "Set true to ask in the background and return immediately with a background task_id; you are notified automatically when the user answers — do not poll with TaskOutput while the question is pending."
            }),
        );
    }
    to_input_json_schema(json!({
        "type": "object",
        "properties": properties,
        "required": ["questions"]
    }))
}

fn parse_ask_user_question_input(input: Value) -> Result<AskUserQuestionInput, String> {
    validate_ask_user_question_input(&input)?;
    serde_json::from_value(input).map_err(|error| format!("Invalid AskUserQuestion arguments: {error}"))
}

fn validate_ask_user_question_input(input: &Value) -> Result<(), String> {
    let object = input.as_object().ok_or_else(invalid_arguments)?;
    let questions = object
        .get("questions")
        .and_then(Value::as_array)
        .filter(|questions| !questions.is_empty() && questions.len() <= 4)
        .ok_or_else(invalid_arguments)?;
    if object
        .get("background")
        .is_some_and(|value| !value.is_boolean())
    {
        return Err(invalid_arguments());
    }
    for question in questions {
        let question = question.as_object().ok_or_else(invalid_arguments)?;
        if !question
            .get("question")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
        {
            return Err(invalid_arguments());
        }
        if question
            .get("header")
            .is_some_and(|value| !value.is_string())
            || question
                .get("multi_select")
                .is_some_and(|value| !value.is_boolean())
            || question
                .get("multiSelect")
                .is_some_and(|value| !value.is_boolean())
        {
            return Err(invalid_arguments());
        }
        let options = question
            .get("options")
            .and_then(Value::as_array)
            .filter(|options| options.len() >= 2 && options.len() <= 4)
            .ok_or_else(invalid_arguments)?;
        for option in options {
            let option = option.as_object().ok_or_else(invalid_arguments)?;
            if !option
                .get("label")
                .and_then(Value::as_str)
                .is_some_and(|value| !value.is_empty())
                || option
                    .get("description")
                    .is_some_and(|value| !value.is_string())
            {
                return Err(invalid_arguments());
            }
        }
    }
    Ok(())
}

fn invalid_arguments() -> String {
    "Invalid AskUserQuestion arguments: expected 1-4 questions, each with 2-4 options; question texts must be unique and option labels unique within each question.".to_string()
}

fn question_description(questions: &[AskUserQuestionItemInput]) -> String {
    let label = questions
        .first()
        .map(|question| question.question.trim())
        .filter(|question| !question.is_empty())
        .unwrap_or("Ask user question");
    if questions.len() <= 1 {
        label.to_string()
    } else {
        format!("{label} (+{} more)", questions.len() - 1)
    }
}

fn dismissed_question_result() -> ExecutableToolResult {
    ExecutableToolResult {
        output: json!({
            "answers": {},
            "note": QUESTION_DISMISSED_MESSAGE,
        })
        .to_string()
        .into(),
        is_error: Some(false),
        ..Default::default()
    }
}

fn output_text(result: &ExecutableToolResult) -> String {
    match &result.output {
        crate::agent::tool_executor::tool_executor::ExecutableToolOutput::Text(text) => text.clone(),
        crate::agent::tool_executor::tool_executor::ExecutableToolOutput::Parts(parts) => {
            serde_json::to_string(parts).unwrap_or_default()
        }
    }
}

fn abort_reason(signal: &crate::_base::utils::abort::AbortSignal) -> WireAsyncError {
    if signal
        .reason()
        .as_ref()
        .is_some_and(|reason| is_user_cancellation(reason.as_ref()))
    {
        Box::new(user_cancellation_reason())
    } else {
        Box::new(abort_error())
    }
}

fn generate_question_id() -> String {
    let mut bytes = [0u8; 16];
    for byte in &mut bytes {
        *byte = fastrand::u8(..);
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "question_{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

struct QuestionCancelGuard {
    callbacks: Arc<dyn HostCallbacks>,
    question_id: String,
    armed: bool,
}

impl QuestionCancelGuard {
    fn new(callbacks: Arc<dyn HostCallbacks>, question_id: String) -> Self {
        Self {
            callbacks,
            question_id,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for QuestionCancelGuard {
    fn drop(&mut self) {
        if self.armed {
            self.callbacks
                .cancel_question(&self.question_id, "turn_ended");
        }
    }
}

pub fn ask_user_question_tool_def(
    allow_background: bool,
) -> crate::turn_loop::types::ToolInfo {
    let description = if allow_background {
        format!("{BASE_DESCRIPTION}\n{BACKGROUND_DESCRIPTION}")
    } else {
        BASE_DESCRIPTION.to_string()
    };
    crate::turn_loop::types::ToolInfo {
        name: "AskUserQuestion".to_string(),
        description,
        input_schema: ask_user_question_input_schema(allow_background),
    }
}

pub async fn execute_ask_user_question(
    callbacks: Arc<dyn HostCallbacks>,
    tasks: Option<&Arc<TaskRunner>>,
    session_id: Option<&str>,
    agent_id: Option<&str>,
    llm_trace_id: Option<&str>,
    turn_id: u64,
    tool_call_id: Option<&str>,
    non_interactive: bool,
    background_allowed: bool,
    cancel: Option<&crate::subagent::types::ParentCancel>,
    args: &Value,
) -> crate::turn_loop::types::ExecutableToolResult {
    let tool = AskUserQuestionTool::new(AskUserQuestionToolDependencies {
        callbacks,
        tasks: tasks.cloned(),
        session_id: session_id.map(str::to_string),
        agent_id: agent_id.map(str::to_string),
        non_interactive,
        background_allowed,
    });
    let execution = tool.resolve_execution_value(args.clone());
    match execution {
        ToolExecution::Error(result) => legacy_result(result),
        ToolExecution::Runnable(run) => {
            let controller = AbortController::new();
            if cancel.is_some_and(|cancel| cancel.triggered()) {
                controller.abort(Some(Arc::new(user_cancellation_reason())));
            }
            let context = ExecutableToolContext {
                turn_id,
                tool_call_id: tool_call_id.unwrap_or_default().to_string(),
                trace: llm_trace_id.map(|trace_id| {
                    crate::llm_adapter::contract::request_trace::LLMRequestTrace {
                        trace_id: Some(trace_id.to_string()),
                    }
                }),
                metadata: None,
                signal: controller.signal(),
                steer_signal: None,
                on_update: None,
                on_foreground_task_start: None,
            };
            let executing = (run.execute)(context);
            tokio::pin!(executing);
            let cancelled = async {
                match cancel {
                    Some(cancel) => cancel.wait().await,
                    None => std::future::pending::<()>().await,
                }
            };
            let result = tokio::select! {
                biased;
                () = cancelled => {
                    controller.abort(Some(Arc::new(user_cancellation_reason())));
                    executing.await
                }
                result = &mut executing => result,
            };
            match result {
                Ok(result) => legacy_result(result),
                Err(error) => crate::tools::err_result(error.to_string()),
            }
        }
    }
}

fn legacy_result(
    result: ExecutableToolResult,
) -> crate::turn_loop::types::ExecutableToolResult {
    let content = match result.output {
        crate::agent::tool_executor::tool_executor::ExecutableToolOutput::Text(text) => text,
        crate::agent::tool_executor::tool_executor::ExecutableToolOutput::Parts(parts) => {
            serde_json::to_string(&parts).unwrap_or_default()
        }
    };
    crate::turn_loop::types::ExecutableToolResult {
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
