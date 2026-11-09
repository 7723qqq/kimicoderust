use std::collections::HashSet;

use serde_json::Value;

use super::{err_result, ok_result};
use crate::callbacks::HostCallbacks;
use crate::rpc::types::{
    AskQuestionItem, AskQuestionOption, AskQuestionRequest, AskQuestionResponse,
};
use crate::turn_loop::types::ExecutableToolResult;

pub const QUESTION_DISMISSED_MESSAGE: &str = "User dismissed the question without answering.";

const QUESTION_UNSUPPORTED_FAILURE_MESSAGE: &str = "The connected client does not support interactive questions. Do NOT call this tool again. Ask the user directly in your text response instead.";

const BACKGROUND_UNAVAILABLE_MESSAGE: &str = "Background questions are not available for this agent because TaskList, TaskOutput, and TaskStop are not enabled.";

pub async fn execute_ask_user_question(
    callbacks: &dyn HostCallbacks,
    args: &Value,
) -> ExecutableToolResult {
    if args.get("background").and_then(Value::as_bool) == Some(true) {
        let available = match callbacks.list_tools().await {
            Ok(response) => {
                let names: HashSet<&str> = response
                    .tools
                    .iter()
                    .map(|tool| tool.name.as_str())
                    .collect();
                ["TaskList", "TaskOutput", "TaskStop"]
                    .iter()
                    .all(|name| names.contains(name))
            }
            Err(_) => false,
        };
        if !available {
            return err_result(BACKGROUND_UNAVAILABLE_MESSAGE.into());
        }
    }

    let Some(request) = parse_request(args) else {
        return err_result(
            "Invalid AskUserQuestion arguments: expected 1-4 questions, each with 2-4 options; question texts must be unique and option labels unique within each question."
                .into(),
        );
    };

    match callbacks.ask_question(request).await {
        Ok(response) => map_response(response),
        Err(error) => {
            if error.contains("does not support interactive questions") {
                err_result(QUESTION_UNSUPPORTED_FAILURE_MESSAGE.into())
            } else {
                dismissed_result()
            }
        }
    }
}

fn parse_request(args: &Value) -> Option<AskQuestionRequest> {
    let questions = args.get("questions")?.as_array()?;
    if questions.is_empty() || questions.len() > 4 {
        return None;
    }
    let mut items = Vec::with_capacity(questions.len());
    let mut seen_texts = HashSet::new();
    for q in questions {
        let question = q.get("question")?.as_str()?.to_string();

        if question.is_empty() {
            return None;
        }
        if !seen_texts.insert(question.clone()) {
            return None;
        }
        let options = q.get("options")?.as_array()?;
        if options.len() < 2 || options.len() > 4 {
            return None;
        }
        let mut parsed_options = Vec::with_capacity(options.len());
        let mut seen_labels = HashSet::new();
        for o in options {
            let label = o.get("label")?.as_str()?.to_string();

            if label.is_empty() {
                return None;
            }
            if !seen_labels.insert(label.clone()) {
                return None;
            }
            parsed_options.push(AskQuestionOption {
                label,
                description: o
                    .get("description")
                    .and_then(|d| d.as_str())
                    .map(str::to_string),
            });
        }
        items.push(AskQuestionItem {
            question,
            header: q.get("header").and_then(|h| h.as_str()).map(str::to_string),
            options: parsed_options,

            multi_select: q
                .get("multi_select")
                .or_else(|| q.get("multiSelect"))
                .and_then(|m| m.as_bool())
                .unwrap_or(false),
        });
    }
    Some(AskQuestionRequest {
        question_id: format!("question_{:016x}", fastrand::u64(..)),
        turn_id: args
            .get("turn_id")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string(),
        tool_call_id: args
            .get("tool_call_id")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string(),
        background: args
            .get("background")
            .and_then(|b| b.as_bool())
            .unwrap_or(false),
        timeout_ms: None,
        questions: items,
    })
}

fn map_response(response: AskQuestionResponse) -> ExecutableToolResult {
    if response.cancelled == Some(true) {
        let reason = response.reason.as_deref().unwrap_or("unknown");
        return err_result(format!(
            "The question was cancelled before the user answered (reason: {reason})."
        ));
    }
    if response.answers.is_empty() {
        return match response.note.as_deref() {
            Some(note) if note != QUESTION_DISMISSED_MESSAGE => ok_result(note.to_string()),
            _ => dismissed_result(),
        };
    }
    ok_result(serde_json::json!({ "answers": response.answers }).to_string())
}

pub fn ask_user_question_tool_def() -> crate::turn_loop::types::ToolInfo {
    crate::turn_loop::types::ToolInfo {
        name: "AskUserQuestion".into(),
        description: r#"Use this tool when you need to ask the user questions with structured options during execution. This allows you to:
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
- The result is JSON with an `answers` object keyed by question text; each value is the chosen option's label (comma-separated labels for multi_select, or the user's own words if they picked "Other"); if `answers` is empty and a `note` says the user dismissed it, they chose not to answer — do not treat this as selecting the recommended option; decide based on context and do not re-ask the same question"#
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
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
                                "description": "A specific, actionable question. End with '?'."
                            },
                            "header": {
                                "type": "string",
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
                                            "description": "Concise display text (1-5 words). If recommended, append '(Recommended)'."
                                        },
                                        "description": {
                                            "type": "string",
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
                },
                "background": {
                    "type": "boolean",
                    "description": "Set true to ask in the background and return immediately with a background task_id; you are notified automatically when the user answers — do not poll with TaskOutput while the question is pending."
                }
            },
            "required": ["questions"]
        }),
    }
}

fn dismissed_result() -> ExecutableToolResult {
    ok_result(
        serde_json::json!({
            "answers": {},
            "note": QUESTION_DISMISSED_MESSAGE,
        })
        .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;

    struct ScriptedCallbacks {
        response: Result<AskQuestionResponse, String>,
        received: Arc<std::sync::Mutex<Option<AskQuestionRequest>>>,
        task_tools_enabled: bool,
    }

    impl HostCallbacks for ScriptedCallbacks {
        fn list_tools(
            &self,
        ) -> crate::rpc::types::BoxFuture<
            'static,
            Result<crate::rpc::types::ListToolsResponse, String>,
        > {
            let names: Vec<String> = if self.task_tools_enabled {
                ["TaskList", "TaskOutput", "TaskStop"]
                    .iter()
                    .map(|name| name.to_string())
                    .collect()
            } else {
                vec!["Read".to_string()]
            };
            Box::pin(async move {
                Ok(crate::rpc::types::ListToolsResponse {
                    tools: names
                        .into_iter()
                        .map(|name| crate::rpc::types::ToolInfo {
                            name,
                            description: String::new(),
                            input_schema: serde_json::json!({}),
                        })
                        .collect(),
                })
            })
        }
        fn llm_chat(
            &self,
            _: crate::rpc::types::LlmChatRequest,
        ) -> crate::rpc::types::BoxFuture<'static, Result<crate::rpc::types::LlmChatResponse, String>>
        {
            Box::pin(async { Err("not used".into()) })
        }

        fn execute_tool(
            &self,
            _: crate::rpc::types::ToolExecuteRequest,
        ) -> crate::rpc::types::BoxFuture<
            'static,
            Result<crate::rpc::types::ToolExecuteResponse, String>,
        > {
            Box::pin(async { Err("not used".into()) })
        }

        fn check_permission(
            &self,
            _: crate::rpc::types::PermissionCheckRequest,
        ) -> crate::rpc::types::BoxFuture<
            'static,
            Result<crate::rpc::types::PermissionDecision, String>,
        > {
            Box::pin(async { Ok(crate::rpc::types::PermissionDecision::allow()) })
        }

        fn ask_question(
            &self,
            request: AskQuestionRequest,
        ) -> crate::rpc::types::BoxFuture<'static, Result<AskQuestionResponse, String>> {
            *self.received.lock().unwrap() = Some(request);
            let response = self.response.clone();
            Box::pin(async move { response })
        }
    }

    fn scripted(
        response: Result<AskQuestionResponse, String>,
    ) -> (
        ScriptedCallbacks,
        Arc<std::sync::Mutex<Option<AskQuestionRequest>>>,
    ) {
        scripted_with_task_tools(response, true)
    }

    fn scripted_with_task_tools(
        response: Result<AskQuestionResponse, String>,
        task_tools_enabled: bool,
    ) -> (
        ScriptedCallbacks,
        Arc<std::sync::Mutex<Option<AskQuestionRequest>>>,
    ) {
        let received = Arc::new(std::sync::Mutex::new(None));
        (
            ScriptedCallbacks {
                response,
                received: received.clone(),
                task_tools_enabled,
            },
            received,
        )
    }

    fn empty_response() -> AskQuestionResponse {
        AskQuestionResponse {
            answers: HashMap::new(),
            method: None,
            note: None,
            cancelled: None,
            reason: None,
        }
    }

    fn sample_args() -> Value {
        serde_json::json!({
            "questions": [
                {
                    "question": "Which approach should I take?",
                    "header": "Style",
                    "options": [
                        { "label": "Option A (Recommended)", "description": "Fast, less flexible" },
                        { "label": "Option B", "description": "Slower, more flexible" }
                    ],
                    "multi_select": false
                }
            ],
            "background": false,
            "turn_id": "turn-42",
            "tool_call_id": "call_abc"
        })
    }

    #[tokio::test]
    async fn test_request_parsing_builds_wire_request() {
        let (callbacks, received) = scripted(Ok(empty_response()));
        let result = execute_ask_user_question(&callbacks, &sample_args()).await;
        assert!(!result.is_error);
        let request = received.lock().unwrap().clone().expect("request recorded");
        assert!(request.question_id.starts_with("question_"));
        assert_eq!(request.turn_id, "turn-42");
        assert_eq!(request.tool_call_id, "call_abc");
        assert!(!request.background);
        assert_eq!(request.questions.len(), 1);
        let item = &request.questions[0];
        assert_eq!(item.question, "Which approach should I take?");
        assert_eq!(item.header.as_deref(), Some("Style"));
        assert_eq!(item.options.len(), 2);
        assert_eq!(item.options[0].label, "Option A (Recommended)");
        assert_eq!(
            item.options[0].description.as_deref(),
            Some("Fast, less flexible")
        );
        assert!(!item.multi_select);
    }

    #[tokio::test]
    async fn test_background_and_multi_select_are_forwarded() {
        let args = serde_json::json!({
            "questions": [
                {
                    "question": "Pick languages?",
                    "options": [
                        { "label": "Rust" },
                        { "label": "TypeScript" }
                    ],
                    "multi_select": true
                }
            ],
            "background": true
        });
        let (callbacks, received) = scripted(Ok(empty_response()));
        let _ = execute_ask_user_question(&callbacks, &args).await;
        let request = received.lock().unwrap().clone().unwrap();
        assert!(request.background);
        assert!(request.questions[0].multi_select);
        assert!(request.questions[0].header.is_none());
        assert!(request.questions[0].options[1].description.is_none());
        assert_eq!(request.turn_id, "");
        assert_eq!(request.tool_call_id, "");
    }

    #[tokio::test]
    async fn test_background_requires_the_three_task_tools() {
        let args = serde_json::json!({
            "questions": [
                { "question": "Pick one?", "options": [{ "label": "a" }, { "label": "b" }] }
            ],
            "background": true
        });
        let (callbacks, received) = scripted_with_task_tools(Ok(empty_response()), false);
        let result = execute_ask_user_question(&callbacks, &args).await;
        assert!(result.is_error);
        assert_eq!(result.content, BACKGROUND_UNAVAILABLE_MESSAGE);
        assert!(
            received.lock().unwrap().is_none(),
            "the host must not be asked"
        );
    }

    #[tokio::test]
    async fn test_invalid_args_return_error_without_calling_host() {
        let (callbacks, received) = scripted(Ok(empty_response()));
        for bad in [
            serde_json::json!({ "questions": [] }),
            serde_json::json!({}),
            serde_json::json!({ "questions": [{ "question": "q", "options": [{ "label": "a" }] }] }),
            serde_json::json!({
                "questions": [
                    { "question": "q", "options": [{ "label": "a" }, { "label": "b" }] },
                    { "question": "q", "options": [{ "label": "c" }, { "label": "d" }] }
                ]
            }),
            serde_json::json!({
                "questions": [
                    { "question": "q", "options": [{ "label": "a" }, { "label": "a" }] }
                ]
            }),
            serde_json::json!({
                "questions": [
                    { "question": "", "options": [{ "label": "a" }, { "label": "b" }] }
                ]
            }),
            serde_json::json!({
                "questions": [
                    { "question": "q", "options": [{ "label": "" }, { "label": "b" }] }
                ]
            }),
        ] {
            let result = execute_ask_user_question(&callbacks, &bad).await;
            assert!(result.is_error, "args: {bad}");
            assert!(result.content.contains("Invalid AskUserQuestion arguments"));
        }
        assert!(
            received.lock().unwrap().is_none(),
            "invalid args must not reach the host"
        );
    }

    #[tokio::test]
    async fn test_answered_response_formats_answers() {
        let mut answers = HashMap::new();
        answers.insert(
            "Which approach should I take?".to_string(),
            "Option A (Recommended)".to_string(),
        );
        let (callbacks, _) = scripted(Ok(AskQuestionResponse {
            answers,
            method: Some("enter".into()),
            note: None,
            cancelled: None,
            reason: None,
        }));
        let result = execute_ask_user_question(&callbacks, &sample_args()).await;
        assert!(!result.is_error);
        let parsed: Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(
            parsed["answers"]["Which approach should I take?"],
            "Option A (Recommended)"
        );
    }

    #[tokio::test]
    async fn test_dismissed_response_notes_no_answer() {
        let (callbacks, _) = scripted(Ok(AskQuestionResponse {
            answers: HashMap::new(),
            method: None,
            note: Some(QUESTION_DISMISSED_MESSAGE.into()),
            cancelled: None,
            reason: None,
        }));
        let result = execute_ask_user_question(&callbacks, &sample_args()).await;
        assert!(!result.is_error);
        let parsed: Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed["answers"], serde_json::json!({}));
        assert_eq!(parsed["note"], QUESTION_DISMISSED_MESSAGE);
    }

    #[tokio::test]
    async fn test_cancelled_response_is_error() {
        let (callbacks, _) = scripted(Ok(AskQuestionResponse {
            answers: HashMap::new(),
            method: None,
            note: None,
            cancelled: Some(true),
            reason: Some("turn_ended".into()),
        }));
        let result = execute_ask_user_question(&callbacks, &sample_args()).await;
        assert!(result.is_error);
        assert!(result.content.contains("cancelled"));
        assert!(result.content.contains("turn_ended"));
    }

    #[tokio::test]
    async fn test_background_output_passes_through_verbatim() {
        let (callbacks, _) = scripted(Ok(AskQuestionResponse {
            answers: HashMap::new(),
            method: None,
            note: Some(
                "task_id: question_abc\ndescription: Which approach should I take?\nstatus: running\nautomatic_notification: true".into(),
            ),
            cancelled: None,
            reason: None,
        }));
        let result = execute_ask_user_question(&callbacks, &sample_args()).await;
        assert!(!result.is_error);
        assert!(result.content.contains("task_id: question_abc"));
        assert!(result.content.contains("status: running"));
    }

    #[tokio::test]
    async fn test_unsupported_host_returns_failure_message() {
        let (callbacks, _) = scripted(Err("host does not support interactive questions".into()));
        let result = execute_ask_user_question(&callbacks, &sample_args()).await;
        assert!(result.is_error);
        assert_eq!(result.content, QUESTION_UNSUPPORTED_FAILURE_MESSAGE);
    }

    #[tokio::test]
    async fn test_other_host_error_maps_to_dismissed() {
        let (callbacks, _) = scripted(Err("connection reset".into()));
        let result = execute_ask_user_question(&callbacks, &sample_args()).await;
        assert!(!result.is_error);
        let parsed: Value =
            serde_json::from_str(&result.content).expect("content must be valid JSON");
        assert_eq!(parsed["answers"], serde_json::json!({}));
        assert_eq!(parsed["note"], QUESTION_DISMISSED_MESSAGE);
    }

    #[test]
    fn test_tool_def_matches_v2_schema() {
        let def = ask_user_question_tool_def();
        assert_eq!(def.name, "AskUserQuestion");
        assert_eq!(def.input_schema["type"], "object");
        assert_eq!(def.input_schema["required"][0], "questions");
        assert!(def.input_schema["properties"]["questions"].is_object());
        assert!(def.input_schema["properties"]["background"].is_object());
        assert!(
            def.input_schema["properties"]["background"]["description"]
                .as_str()
                .is_some_and(|text| text.contains("background task_id")),
            "the background parameter's own description carries the contract"
        );
    }
}
