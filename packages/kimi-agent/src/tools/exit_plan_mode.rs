use std::collections::HashSet;

use serde_json::Value;

use super::{arg_error_text, err_result, ok_result};
use crate::callbacks::HostCallbacks;
use crate::permission::PermissionMode;
use crate::rpc::types::{
    AskQuestionItem, AskQuestionOption, AskQuestionRequest, AskQuestionResponse, StateReadRequest,
    StateWriteRequest,
};
use crate::turn_loop::types::ExecutableToolResult;

const PLAN_MODE_NOT_ACTIVE_MESSAGE: &str = "ExitPlanMode can only be called while plan mode is active. Only the user can turn plan mode on.";

const PLAN_APPROVAL_DISMISSED_MESSAGE: &str = "Plan approval dismissed. Plan mode remains active.";

const PLAN_REJECTED_MESSAGE: &str = "Plan rejected by user. Plan mode remains active.";

const PLAN_REVISE_MESSAGE: &str = "User requested revisions. Plan mode remains active.";

const RESERVED_OPTION_LABELS: [&str; 4] = ["approve", "reject", "reject and exit", "revise"];

const STATE_BRIDGE_UNSUPPORTED_FAILURE_MESSAGE: &str = "The connected client does not support the state bridge. Do NOT call this tool again — the host cannot deactivate plan mode.";

const QUESTION_UNSUPPORTED_FAILURE_MESSAGE: &str =
    "The connected client does not support interactive questions. Do NOT call this tool again.";

pub(crate) struct ExitPlanModeOption {
    pub(crate) label: String,
    pub(crate) description: String,
}

pub async fn execute_exit_plan_mode(
    callbacks: &dyn HostCallbacks,
    args: &Value,
) -> ExecutableToolResult {
    execute_exit_plan_mode_with_mode(callbacks, args, permission_mode()).await
}

fn permission_mode() -> PermissionMode {
    crate::config::KimiConfig::discover()
        .map(|(config, _)| config.build_policy_snapshot(None).mode)
        .unwrap_or(PermissionMode::Manual)
}

pub async fn execute_exit_plan_mode_with_mode(
    callbacks: &dyn HostCallbacks,
    args: &Value,
    mode: PermissionMode,
) -> ExecutableToolResult {
    let options = match parse_options(args, "ExitPlanMode") {
        Ok(options) => options,
        Err(message) => return err_result(message),
    };
    let turn_id = args
        .get("turn_id")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();
    let tool_call_id = args
        .get("tool_call_id")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();
    let read = StateReadRequest {
        domain: "plan".into(),
        key: "plan".into(),
        turn_id: turn_id.clone(),
        tool_call_id: tool_call_id.clone(),
    };
    let plan = match callbacks.state_read(read).await {
        Ok(response) => response.value,
        Err(error) => return map_state_error(error),
    };
    if plan.get("active").and_then(|v| v.as_bool()) != Some(true) {
        return err_result(PLAN_MODE_NOT_ACTIVE_MESSAGE.into());
    }
    let path = plan.get("path").and_then(|p| p.as_str());
    if mode == PermissionMode::Auto {
        return exit_plan(callbacks, &turn_id, &tool_call_id, path, None, true).await;
    }
    confirm_and_exit(callbacks, &turn_id, &tool_call_id, path, &options).await
}

async fn confirm_and_exit(
    callbacks: &dyn HostCallbacks,
    turn_id: &str,
    tool_call_id: &str,
    path: Option<&str>,
    options: &[ExitPlanModeOption],
) -> ExecutableToolResult {
    let question = match path {
        Some(path) => format!("Approve the plan (plan file: {path}) and exit plan mode?"),
        None => "Approve the plan and exit plan mode?".to_string(),
    };
    let request = AskQuestionRequest {
        question_id: format!("question_{:016x}", fastrand::u64(..)),
        turn_id: turn_id.to_string(),
        tool_call_id: tool_call_id.to_string(),
        background: false,
        timeout_ms: None,
        questions: vec![AskQuestionItem {
            question: question.clone(),
            header: Some("Plan Review".into()),
            options: question_options(options),
            multi_select: false,
        }],
    };
    let response = match callbacks.ask_question(request).await {
        Ok(response) => response,
        Err(error) => {
            if error.contains("does not support interactive questions") {
                return err_result(QUESTION_UNSUPPORTED_FAILURE_MESSAGE.into());
            }
            return ok_result(PLAN_APPROVAL_DISMISSED_MESSAGE.into());
        }
    };
    match interpret_answer(&response, &question, options) {
        Answer::Approved(selected) => {
            exit_plan(
                callbacks,
                turn_id,
                tool_call_id,
                path,
                selected.as_deref(),
                false,
            )
            .await
        }

        Answer::Rejected(Some(feedback)) => {
            ok_result(format!("User rejected the plan. Feedback:\n\n{feedback}"))
        }

        Answer::Rejected(None) => ExecutableToolResult {
            delivery: None,
            stop_turn: true,
            ..err_result(PLAN_REJECTED_MESSAGE.into())
        },

        Answer::RejectAndExit => {
            exit_plan(callbacks, turn_id, tool_call_id, path, None, true).await
        }
        Answer::Revise => ok_result(PLAN_REVISE_MESSAGE.into()),
        Answer::Dismissed => ok_result(PLAN_APPROVAL_DISMISSED_MESSAGE.into()),
    }
}

pub(crate) fn question_options(options: &[ExitPlanModeOption]) -> Vec<AskQuestionOption> {
    if options.is_empty() {
        return vec![
            AskQuestionOption {
                label: "Approve (Recommended)".into(),
                description: Some("Exit plan mode and start executing the plan.".into()),
            },
            AskQuestionOption {
                label: "Reject".into(),
                description: Some("Keep plan mode active; the plan is rejected.".into()),
            },
            AskQuestionOption {
                label: "Reject and Exit".into(),
                description: Some(
                    "Reject the plan AND deactivate plan mode so the next turn is unconstrained."
                        .into(),
                ),
            },
            AskQuestionOption {
                label: "Revise".into(),
                description: Some("Keep plan mode active; revise the plan first.".into()),
            },
        ];
    }
    options
        .iter()
        .map(|option| AskQuestionOption {
            label: option.label.clone(),
            description: (!option.description.is_empty()).then(|| option.description.clone()),
        })
        .collect()
}

pub(crate) enum Answer {
    Approved(Option<String>),

    Rejected(Option<String>),

    RejectAndExit,
    Revise,
    Dismissed,
}

pub(crate) fn interpret_answer(
    response: &AskQuestionResponse,
    question: &str,
    options: &[ExitPlanModeOption],
) -> Answer {
    if response.cancelled == Some(true) {
        return Answer::Dismissed;
    }
    let Some(answer) = response.answers.get(question) else {
        return Answer::Dismissed;
    };
    if options.is_empty() {
        return match answer.as_str() {
            "Approve (Recommended)" => Answer::Approved(None),
            "Reject and Exit" => Answer::RejectAndExit,
            "Revise" => Answer::Revise,

            "Reject" => Answer::Rejected(None),
            _ => Answer::Rejected(Some(answer.clone())),
        };
    }

    if let Some(option) = options.iter().find(|option| option.label == *answer) {
        let _ = option;
        Answer::Approved(Some(answer.clone()))
    } else {
        Answer::Rejected(Some(answer.clone()))
    }
}

async fn exit_plan(
    callbacks: &dyn HostCallbacks,
    turn_id: &str,
    tool_call_id: &str,
    path: Option<&str>,
    selected: Option<&str>,
    auto: bool,
) -> ExecutableToolResult {
    let write = StateWriteRequest {
        domain: "plan".into(),
        key: "plan".into(),
        value: serde_json::json!({ "active": false }),
        undoable: true,
        turn_id: turn_id.to_string(),
        tool_call_id: tool_call_id.to_string(),
    };
    match callbacks.state_write(write).await {
        Ok(_) => ok_result(if auto {
            auto_approved_output(path)
        } else {
            approved_output(path, selected)
        }),
        Err(error) => map_state_error(error),
    }
}

fn auto_approved_output(path: Option<&str>) -> String {
    let mut out = "Exited plan mode. Plan mode deactivated. All tools are now available.\nNote: this plan was auto-approved without user review — the user has NOT explicitly approved it. Follow the user's original instructions on whether to proceed with execution; if they asked you to stop, wait, or only summarize after planning, do not start executing."
        .to_string();
    if let Some(path) = path {
        out.push_str(&format!("\nPlan saved to: {path}"));
    }
    out
}

fn approved_output(path: Option<&str>, selected: Option<&str>) -> String {
    let mut out =
        "Exited plan mode. Plan mode deactivated. All tools are now available.".to_string();
    if let Some(selected) = selected {
        out = format!(
            "Exited plan mode. Selected approach: {selected}\nExecute ONLY the selected approach. Do not execute any unselected alternatives.\n\nPlan mode deactivated. All tools are now available."
        );
    }
    if let Some(path) = path {
        out.push_str(&format!("\nPlan saved to: {path}"));
    }
    out
}

pub(crate) fn parse_options(args: &Value, tool: &str) -> Result<Vec<ExitPlanModeOption>, String> {
    let Some(options) = args.get("options") else {
        return Ok(Vec::new());
    };
    let Some(array) = options.as_array() else {
        return Err(arg_error_text(
            tool,
            "options",
            "engine.tools.argMustBeArray",
        ));
    };
    if array.is_empty() || array.len() > 3 {
        return Err(format!(
            "Invalid {tool} arguments: `options` must contain 1-3 items."
        ));
    }
    let mut parsed = Vec::with_capacity(array.len());
    let mut seen = HashSet::new();
    for item in array {
        let Some(label) = item.get("label").and_then(|l| l.as_str()) else {
            return Err(format!(
                "Invalid {tool} arguments: each option needs a `label` string."
            ));
        };
        if label.is_empty() || label.chars().count() > 80 {
            return Err(format!(
                "Invalid {tool} arguments: option labels must be 1-80 characters."
            ));
        }
        let normalized = label.trim().to_lowercase();
        if RESERVED_OPTION_LABELS.iter().any(|r| *r == normalized) {
            return Err(format!(
                "Invalid {tool} arguments: option labels must not use reserved approval labels (Approve, Reject, Reject and Exit, Revise)."
            ));
        }
        if !seen.insert(normalized) {
            return Err(format!(
                "Invalid {tool} arguments: option labels must be unique."
            ));
        }
        parsed.push(ExitPlanModeOption {
            label: label.to_string(),
            description: item
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("")
                .to_string(),
        });
    }
    Ok(parsed)
}

fn map_state_error(error: String) -> ExecutableToolResult {
    if error.contains("does not support state bridge") {
        err_result(STATE_BRIDGE_UNSUPPORTED_FAILURE_MESSAGE.into())
    } else {
        err_result(error)
    }
}

pub fn exit_plan_mode_tool_def() -> crate::turn_loop::types::ToolInfo {
    crate::turn_loop::types::ToolInfo {
        name: "ExitPlanMode".into(),
        description: "Use this tool when you are in plan mode and have finished writing your plan to the plan file and are ready for user approval.\n\n## How This Tool Works\n- You should have already written your plan to the plan file specified in the plan mode reminder.\n- This tool does NOT take the plan content as a parameter - it reads the plan from the file you wrote.\n- The user will see the contents of your plan file when they review it. In auto permission mode, the tool reads the file and exits plan mode without asking the user.\n\n## When to Use\nOnly use this tool for tasks that require planning implementation steps. For research tasks (searching files, reading code, understanding the codebase), do NOT use this tool.\n\n## What a good plan contains\nList specific, verifiable steps grounded in the actual codebase — real files, functions, and commands, in a sensible order. Each step should be concrete enough to act on and to check. Avoid vague filler like \"improve performance\" or \"add tests\"; say what to change and where.\n\n## Multiple Approaches\nIf your plan offers multiple alternative approaches, pass them via the `options` parameter so the user can choose which one to execute — see the `options` parameter for the format, count, and reserved labels. In yolo and manual modes the user sees all options alongside the host's Reject and Revise controls.\n\n## Before Using\n- In auto permission mode, do NOT use AskUserQuestion; make the best decision from available context.\n- In auto permission mode, this tool exits plan mode without asking the user.\n- In yolo and manual modes, this tool still presents the plan to the user for approval.\n- If auto permission mode is not active and you have unresolved questions, use AskUserQuestion first.\n- If auto permission mode is not active and you have multiple approaches and haven't narrowed down yet, consider using AskUserQuestion first to let the user choose, then write a plan for the chosen approach only.\n- Once your plan is finalized, use THIS tool to request approval.\n- Do NOT use AskUserQuestion to ask \"Is this plan OK?\" or \"Should I proceed?\" - that is exactly what ExitPlanMode does.\n- If rejected, revise based on feedback and call ExitPlanMode again.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "options": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 3,
                    "description": "When the plan contains multiple alternative approaches, list them here so the user can choose which one to execute. Provide up to 3 options; 2-3 distinct approaches work best when the plan offers a real choice. Passing a single option is allowed and is equivalent to a plain plan approval. Each option represents a distinct approach from the plan. Do not use \"Reject\", \"Revise\", \"Approve\", or \"Reject and Exit\" as labels.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "label": {
                                "type": "string",
                                "minLength": 1,
                                "maxLength": 80,
                                "description": "Short name for this option (1-8 words). Append \"(Recommended)\" if you recommend this option."
                            },
                            "description": {
                                "type": "string",
                                "description": "Brief summary of this approach and its trade-offs."
                            }
                        },
                        "required": ["label"],
                        "additionalProperties": false
                    }
                }
            },
            "additionalProperties": false
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::types::{BoxFuture, PermissionDecision, StateReadResponse, StateWriteResponse};
    use std::collections::HashMap;
    use std::sync::Arc;

    struct ScriptedCallbacks {
        read_response: Result<StateReadResponse, String>,
        write_response: Result<StateWriteResponse, String>,
        ask_response: Result<AskQuestionResponse, String>,
        read_received: Arc<std::sync::Mutex<Option<StateReadRequest>>>,
        write_received: Arc<std::sync::Mutex<Option<StateWriteRequest>>>,
        ask_received: Arc<std::sync::Mutex<Option<AskQuestionRequest>>>,
    }

    impl HostCallbacks for ScriptedCallbacks {
        fn llm_chat(
            &self,
            _: crate::rpc::types::LlmChatRequest,
        ) -> BoxFuture<'static, Result<crate::rpc::types::LlmChatResponse, String>> {
            Box::pin(async { Err("not used".into()) })
        }

        fn execute_tool(
            &self,
            _: crate::rpc::types::ToolExecuteRequest,
        ) -> BoxFuture<'static, Result<crate::rpc::types::ToolExecuteResponse, String>> {
            Box::pin(async { Err("not used".into()) })
        }

        fn check_permission(
            &self,
            _: crate::rpc::types::PermissionCheckRequest,
        ) -> BoxFuture<'static, Result<PermissionDecision, String>> {
            Box::pin(async { Ok(PermissionDecision::allow()) })
        }

        fn state_read(
            &self,
            request: StateReadRequest,
        ) -> BoxFuture<'static, Result<StateReadResponse, String>> {
            *self.read_received.lock().unwrap() = Some(request);
            let response = self.read_response.clone();
            Box::pin(async move { response })
        }

        fn state_write(
            &self,
            request: StateWriteRequest,
        ) -> BoxFuture<'static, Result<StateWriteResponse, String>> {
            *self.write_received.lock().unwrap() = Some(request);
            let response = self.write_response.clone();
            Box::pin(async move { response })
        }

        fn ask_question(
            &self,
            request: AskQuestionRequest,
        ) -> BoxFuture<'static, Result<AskQuestionResponse, String>> {
            *self.ask_received.lock().unwrap() = Some(request);
            let response = self.ask_response.clone();
            Box::pin(async move { response })
        }
    }

    #[allow(clippy::type_complexity)]
    fn scripted(
        read_response: Result<StateReadResponse, String>,
        write_response: Result<StateWriteResponse, String>,
        ask_response: Result<AskQuestionResponse, String>,
    ) -> (
        ScriptedCallbacks,
        Arc<std::sync::Mutex<Option<StateReadRequest>>>,
        Arc<std::sync::Mutex<Option<StateWriteRequest>>>,
        Arc<std::sync::Mutex<Option<AskQuestionRequest>>>,
    ) {
        let read_received = Arc::new(std::sync::Mutex::new(None));
        let write_received = Arc::new(std::sync::Mutex::new(None));
        let ask_received = Arc::new(std::sync::Mutex::new(None));
        (
            ScriptedCallbacks {
                read_response,
                write_response,
                ask_response,
                read_received: read_received.clone(),
                write_received: write_received.clone(),
                ask_received: ask_received.clone(),
            },
            read_received,
            write_received,
            ask_received,
        )
    }

    fn read_ok(value: Value) -> Result<StateReadResponse, String> {
        Ok(StateReadResponse { value })
    }

    fn write_ok(value: Value) -> Result<StateWriteResponse, String> {
        Ok(StateWriteResponse { ok: true, value })
    }

    fn answered(question: &str, label: &str) -> Result<AskQuestionResponse, String> {
        let mut answers = HashMap::new();
        answers.insert(question.to_string(), label.to_string());
        Ok(AskQuestionResponse {
            answers,
            method: Some("enter".into()),
            note: None,
            cancelled: None,
            reason: None,
        })
    }

    fn dismissed() -> Result<AskQuestionResponse, String> {
        Ok(AskQuestionResponse {
            answers: HashMap::new(),
            method: None,
            note: None,
            cancelled: None,
            reason: None,
        })
    }

    fn active_plan() -> Value {
        serde_json::json!({
            "active": true,
            "id": "plan-7f3a",
            "path": "/session/agents/agent-1/plans/plan-7f3a.md"
        })
    }

    #[tokio::test]
    async fn test_not_active_returns_v2_error_without_writing() {
        let (callbacks, read_received, write_received, ask_received) = scripted(
            read_ok(serde_json::json!({ "active": false })),
            write_ok(Value::Null),
            dismissed(),
        );
        let result = execute_exit_plan_mode_with_mode(
            &callbacks,
            &serde_json::json!({}),
            PermissionMode::Manual,
        )
        .await;
        assert!(result.is_error);
        assert_eq!(result.content, PLAN_MODE_NOT_ACTIVE_MESSAGE);
        let request = read_received.lock().unwrap().clone().unwrap();
        assert_eq!(request.domain, "plan");
        assert_eq!(request.key, "plan");
        assert!(write_received.lock().unwrap().is_none());
        assert!(ask_received.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn test_auto_mode_exits_without_asking() {
        let (callbacks, _, write_received, ask_received) = scripted(
            read_ok(active_plan()),
            write_ok(serde_json::json!({ "active": false })),
            dismissed(),
        );
        let result = execute_exit_plan_mode_with_mode(
            &callbacks,
            &serde_json::json!({}),
            PermissionMode::Auto,
        )
        .await;
        assert!(!result.is_error);
        assert_eq!(
            result.content,
            "Exited plan mode. Plan mode deactivated. All tools are now available.\nNote: this plan was auto-approved without user review — the user has NOT explicitly approved it. Follow the user's original instructions on whether to proceed with execution; if they asked you to stop, wait, or only summarize after planning, do not start executing.\nPlan saved to: /session/agents/agent-1/plans/plan-7f3a.md"
        );
        assert!(
            ask_received.lock().unwrap().is_none(),
            "auto mode must not ask the user"
        );
        let request = write_received.lock().unwrap().clone().unwrap();
        assert_eq!(request.domain, "plan");
        assert_eq!(request.key, "plan");
        assert_eq!(request.value, serde_json::json!({ "active": false }));
        assert!(request.undoable);
    }

    #[tokio::test]
    async fn test_auto_mode_without_path_omits_saved_to() {
        let (callbacks, _, _, _) = scripted(
            read_ok(serde_json::json!({ "active": true, "id": "plan-1" })),
            write_ok(serde_json::json!({ "active": false })),
            dismissed(),
        );
        let result = execute_exit_plan_mode_with_mode(
            &callbacks,
            &serde_json::json!({}),
            PermissionMode::Auto,
        )
        .await;
        assert!(!result.is_error);
        assert!(!result.content.contains("Plan saved to:"));
        assert!(result.content.contains("auto-approved without user review"));
    }

    #[tokio::test]
    async fn test_manual_mode_asks_and_exits_on_approval() {
        let (callbacks, _, write_received, ask_received) = scripted(
            read_ok(active_plan()),
            write_ok(serde_json::json!({ "active": false })),
            answered(
                "Approve the plan (plan file: /session/agents/agent-1/plans/plan-7f3a.md) and exit plan mode?",
                "Approve (Recommended)",
            ),
        );
        let result = execute_exit_plan_mode_with_mode(
            &callbacks,
            &serde_json::json!({}),
            PermissionMode::Manual,
        )
        .await;
        assert!(!result.is_error);
        assert_eq!(
            result.content,
            "Exited plan mode. Plan mode deactivated. All tools are now available.\nPlan saved to: /session/agents/agent-1/plans/plan-7f3a.md"
        );
        let question = ask_received.lock().unwrap().clone().unwrap();
        assert_eq!(question.questions.len(), 1);
        assert_eq!(question.questions[0].header.as_deref(), Some("Plan Review"));
        assert_eq!(question.questions[0].options.len(), 4);
        assert_eq!(
            question.questions[0].options[0].label,
            "Approve (Recommended)"
        );
        assert_eq!(question.questions[0].options[1].label, "Reject");
        assert_eq!(question.questions[0].options[2].label, "Reject and Exit");
        assert_eq!(question.questions[0].options[3].label, "Revise");
        let request = write_received.lock().unwrap().clone().unwrap();
        assert_eq!(request.value, serde_json::json!({ "active": false }));
    }

    #[tokio::test]
    async fn test_manual_mode_reject_keeps_plan_active() {
        let (callbacks, _, write_received, _) = scripted(
            read_ok(active_plan()),
            write_ok(Value::Null),
            answered(
                "Approve the plan (plan file: /session/agents/agent-1/plans/plan-7f3a.md) and exit plan mode?",
                "Reject",
            ),
        );
        let result = execute_exit_plan_mode_with_mode(
            &callbacks,
            &serde_json::json!({}),
            PermissionMode::Manual,
        )
        .await;
        assert!(result.is_error);
        assert!(result.stop_turn);
        assert_eq!(result.content, PLAN_REJECTED_MESSAGE);
        assert!(write_received.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn test_manual_mode_revise_keeps_plan_active() {
        let (callbacks, _, write_received, _) = scripted(
            read_ok(active_plan()),
            write_ok(Value::Null),
            answered(
                "Approve the plan (plan file: /session/agents/agent-1/plans/plan-7f3a.md) and exit plan mode?",
                "Revise",
            ),
        );
        let result = execute_exit_plan_mode_with_mode(
            &callbacks,
            &serde_json::json!({}),
            PermissionMode::Manual,
        )
        .await;
        assert!(!result.is_error);
        assert!(!result.stop_turn);
        assert_eq!(result.content, PLAN_REVISE_MESSAGE);
        assert!(write_received.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn test_manual_mode_dismissed_keeps_plan_active() {
        let (callbacks, _, write_received, _) =
            scripted(read_ok(active_plan()), write_ok(Value::Null), dismissed());
        let result = execute_exit_plan_mode_with_mode(
            &callbacks,
            &serde_json::json!({}),
            PermissionMode::Manual,
        )
        .await;
        assert!(!result.is_error);
        assert_eq!(result.content, PLAN_APPROVAL_DISMISSED_MESSAGE);
        assert!(write_received.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn test_manual_mode_cancelled_keeps_plan_active() {
        let (callbacks, _, write_received, _) = scripted(
            read_ok(active_plan()),
            write_ok(Value::Null),
            Ok(AskQuestionResponse {
                answers: HashMap::new(),
                method: None,
                note: None,
                cancelled: Some(true),
                reason: Some("turn_ended".into()),
            }),
        );
        let result = execute_exit_plan_mode_with_mode(
            &callbacks,
            &serde_json::json!({}),
            PermissionMode::Manual,
        )
        .await;
        assert!(!result.is_error);
        assert_eq!(result.content, PLAN_APPROVAL_DISMISSED_MESSAGE);
        assert!(write_received.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn test_question_failure_keeps_plan_active() {
        let (callbacks, _, write_received, _) = scripted(
            read_ok(active_plan()),
            write_ok(Value::Null),
            Err("host does not support interactive questions".into()),
        );
        let result = execute_exit_plan_mode_with_mode(
            &callbacks,
            &serde_json::json!({}),
            PermissionMode::Manual,
        )
        .await;
        assert!(result.is_error);
        assert_eq!(result.content, QUESTION_UNSUPPORTED_FAILURE_MESSAGE);
        assert!(write_received.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn test_transient_question_error_maps_to_dismissed() {
        let (callbacks, _, write_received, _) = scripted(
            read_ok(active_plan()),
            write_ok(Value::Null),
            Err("connection reset".into()),
        );
        let result = execute_exit_plan_mode_with_mode(
            &callbacks,
            &serde_json::json!({}),
            PermissionMode::Manual,
        )
        .await;
        assert!(!result.is_error);
        assert_eq!(result.content, PLAN_APPROVAL_DISMISSED_MESSAGE);
        assert!(write_received.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn test_model_options_are_offered_and_selection_approves() {
        let (callbacks, _, write_received, ask_received) = scripted(
            read_ok(active_plan()),
            write_ok(serde_json::json!({ "active": false })),
            answered(
                "Approve the plan (plan file: /session/agents/agent-1/plans/plan-7f3a.md) and exit plan mode?",
                "Option A",
            ),
        );
        let result = execute_exit_plan_mode_with_mode(
            &callbacks,
            &serde_json::json!({
                "options": [
                    { "label": "Option A", "description": "Fast, less flexible" },
                    { "label": "Option B", "description": "Slower, more flexible" }
                ]
            }),
            PermissionMode::Manual,
        )
        .await;
        assert!(!result.is_error);
        assert_eq!(
            result.content,
            "Exited plan mode. Selected approach: Option A\nExecute ONLY the selected approach. Do not execute any unselected alternatives.\n\nPlan mode deactivated. All tools are now available.\nPlan saved to: /session/agents/agent-1/plans/plan-7f3a.md"
        );
        let question = ask_received.lock().unwrap().clone().unwrap();
        assert_eq!(question.questions[0].options.len(), 2);
        assert_eq!(question.questions[0].options[0].label, "Option A");
        assert_eq!(
            question.questions[0].options[0].description.as_deref(),
            Some("Fast, less flexible")
        );
        assert!(write_received.lock().unwrap().is_some());
    }

    #[tokio::test]
    async fn test_model_option_other_answer_is_rejected() {
        let (callbacks, _, write_received, _) = scripted(
            read_ok(active_plan()),
            write_ok(Value::Null),
            answered(
                "Approve the plan (plan file: /session/agents/agent-1/plans/plan-7f3a.md) and exit plan mode?",
                "Something else entirely",
            ),
        );
        let result = execute_exit_plan_mode_with_mode(
            &callbacks,
            &serde_json::json!({
                "options": [{ "label": "Option A" }]
            }),
            PermissionMode::Manual,
        )
        .await;
        assert!(!result.is_error);
        assert!(!result.stop_turn);
        assert_eq!(
            result.content,
            "User rejected the plan. Feedback:\n\nSomething else entirely"
        );
        assert!(write_received.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn test_invalid_options_return_error_without_calling_host() {
        let (callbacks, read_received, write_received, ask_received) =
            scripted(read_ok(active_plan()), write_ok(Value::Null), dismissed());
        for bad in [
            serde_json::json!({ "options": [] }),
            serde_json::json!({ "options": [{ "label": "a" }, { "label": "b" }, { "label": "c" }, { "label": "d" }] }),
            serde_json::json!({ "options": [{ "label": "a" }, { "label": "a" }] }),
            serde_json::json!({ "options": [{ "label": "Approve" }] }),
            serde_json::json!({ "options": [{ "label": "reject and exit" }] }),
            serde_json::json!({ "options": [{ "label": 42 }] }),
            serde_json::json!({ "options": "nope" }),
        ] {
            let result =
                execute_exit_plan_mode_with_mode(&callbacks, &bad, PermissionMode::Manual).await;
            assert!(result.is_error, "args: {bad}");
            assert!(result.content.contains("Invalid ExitPlanMode arguments"));
        }
        assert!(read_received.lock().unwrap().is_none());
        assert!(write_received.lock().unwrap().is_none());
        assert!(ask_received.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn test_unsupported_host_on_read_returns_failure_message() {
        let (callbacks, _, _, _) = scripted(
            Err("host does not support state bridge".into()),
            write_ok(Value::Null),
            dismissed(),
        );
        let result = execute_exit_plan_mode_with_mode(
            &callbacks,
            &serde_json::json!({}),
            PermissionMode::Auto,
        )
        .await;
        assert!(result.is_error);
        assert_eq!(result.content, STATE_BRIDGE_UNSUPPORTED_FAILURE_MESSAGE);
    }

    #[tokio::test]
    async fn test_unsupported_host_on_write_returns_failure_message() {
        let (callbacks, _, _, _) = scripted(
            read_ok(active_plan()),
            Err("State write error: [-32603] host does not support state bridge".into()),
            dismissed(),
        );
        let result = execute_exit_plan_mode_with_mode(
            &callbacks,
            &serde_json::json!({}),
            PermissionMode::Auto,
        )
        .await;
        assert!(result.is_error);
        assert_eq!(result.content, STATE_BRIDGE_UNSUPPORTED_FAILURE_MESSAGE);
    }

    #[tokio::test]
    async fn test_other_host_error_passes_through() {
        let (callbacks, _, _, _) = scripted(
            Err("State read error: [-32001] unknown domain: goal".into()),
            write_ok(Value::Null),
            dismissed(),
        );
        let result = execute_exit_plan_mode_with_mode(
            &callbacks,
            &serde_json::json!({}),
            PermissionMode::Auto,
        )
        .await;
        assert!(result.is_error);
        assert!(result.content.contains("-32001"));
        assert!(result.content.contains("unknown domain"));
    }

    #[tokio::test]
    async fn test_turn_and_tool_call_ids_are_forwarded() {
        let (callbacks, read_received, write_received, ask_received) = scripted(
            read_ok(active_plan()),
            write_ok(serde_json::json!({ "active": false })),
            answered(
                "Approve the plan (plan file: /session/agents/agent-1/plans/plan-7f3a.md) and exit plan mode?",
                "Approve (Recommended)",
            ),
        );
        let args = serde_json::json!({
            "turn_id": "turn-42",
            "tool_call_id": "call_abc"
        });
        let result =
            execute_exit_plan_mode_with_mode(&callbacks, &args, PermissionMode::Manual).await;
        assert!(!result.is_error);
        assert_eq!(
            read_received.lock().unwrap().clone().unwrap().turn_id,
            "turn-42"
        );
        assert_eq!(
            read_received.lock().unwrap().clone().unwrap().tool_call_id,
            "call_abc"
        );
        assert_eq!(
            write_received.lock().unwrap().clone().unwrap().turn_id,
            "turn-42"
        );
        assert_eq!(
            ask_received.lock().unwrap().clone().unwrap().turn_id,
            "turn-42"
        );
        assert_eq!(
            ask_received.lock().unwrap().clone().unwrap().tool_call_id,
            "call_abc"
        );
    }

    #[test]
    fn test_tool_def_matches_v2_schema() {
        let def = exit_plan_mode_tool_def();
        assert_eq!(def.name, "ExitPlanMode");
        assert_eq!(def.input_schema["type"], "object");
        assert_eq!(def.input_schema["additionalProperties"], false);
        let options = &def.input_schema["properties"]["options"];
        assert_eq!(options["minItems"], 1);
        assert_eq!(options["maxItems"], 3);
        assert_eq!(options["items"]["required"][0], "label");
        assert!(def.description.contains("ready for user approval"));
        assert!(def.description.contains("auto permission mode"));
    }
}
