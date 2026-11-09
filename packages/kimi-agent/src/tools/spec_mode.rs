use std::path::{Path, PathBuf};

use serde_json::Value;

use super::exit_plan_mode::{
    Answer, ExitPlanModeOption, interpret_answer, parse_options, question_options,
};
use super::mode_mutex::{
    emit_spec_mode_exit, mode_switch_lock, pause_tower_for_mode_enter, read_mode_state,
    write_mode_state,
};
use super::{err_result, ok_result};
use crate::callbacks::HostCallbacks;
use crate::native::path_access::{PathClass, is_within_directory};
use crate::native::shell_path_bridge::ShellPathBridge;
use crate::permission::PermissionMode;
use crate::rpc::types::{AskQuestionItem, AskQuestionRequest, StateReadRequest, StateWriteRequest};
use crate::turn_loop::types::{
    ExecutableToolResult, FileOperation, ToolFileAccess, ToolResourceAccess,
};

pub const SPEC_DIR_NAME: &str = "specs";

pub const SPEC_REQUIRED_FILES: [&str; 3] = ["requirements.md", "design.md", "tasks.md"];

pub const SPEC_MODE_INVALID: &str = "spec.mode_invalid";

pub const SPEC_INCOMPLETE: &str = "spec.incomplete";

pub const SPEC_STATE_UNAVAILABLE: &str = "spec.state_unavailable";

const SPEC_WRITE_DENIED: &str = "Spec mode is active. You may only write inside the current spec directory. Call ExitSpecMode to exit spec mode before editing other files.";

const SPEC_MODE_ALREADY_ACTIVE_MESSAGE: &str =
    "Already in spec mode. Use ExitSpecMode when the spec is ready.";

const SPEC_MODE_NOT_ACTIVE_MESSAGE: &str = "ExitSpecMode can only be called while spec mode is active. Use EnterSpecMode (or /spec) first.";

const SPEC_APPROVAL_DISMISSED_MESSAGE: &str = "Spec approval dismissed. Spec mode remains active.";

const SPEC_REJECTED_MESSAGE: &str = "Spec rejected by user. Spec mode remains active.";

const SPEC_REJECTED_AND_EXITED_MESSAGE: &str = "Spec rejected by user. Spec mode deactivated.";

const SPEC_REVISE_MESSAGE: &str = "User requested revisions. Spec mode remains active.";

const STATE_BRIDGE_UNSUPPORTED_FAILURE_MESSAGE: &str = "The connected client does not support the state bridge. Do NOT call this tool again — the host cannot activate spec mode.";

const QUESTION_UNSUPPORTED_FAILURE_MESSAGE: &str =
    "The connected client does not support interactive questions. Do NOT call this tool again.";

pub fn active_spec_id(state: &Value) -> Option<&str> {
    if state.get("active").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    state
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
}

pub fn spec_dir(workspace_root: &Path, id: &str) -> PathBuf {
    workspace_root.join(SPEC_DIR_NAME).join(id)
}

pub fn spec_state_unavailable_message(tool_name: &str) -> String {
    format!(
        "{SPEC_STATE_UNAVAILABLE}: the spec mode state could not be read, so {tool_name} is refused. The engine does not treat an unreadable spec state as inactive. Repair the session's spec state and retry."
    )
}

fn invalid_transition(detail: &str) -> String {
    format!("{SPEC_MODE_INVALID}: {detail}")
}

pub async fn execute_enter_spec_mode(
    callbacks: &dyn HostCallbacks,
    workspace_root: &Path,
    agent_id: &str,
    args: &Value,
) -> ExecutableToolResult {
    if !args.as_object().is_some_and(|obj| obj.is_empty()) {
        return err_result("Invalid EnterSpecMode arguments: the tool takes no arguments.".into());
    }

    let _switch = mode_switch_lock().lock().await;

    let spec = match read_mode_state(callbacks, "spec").await {
        Ok(value) => value,
        Err(error) => {
            return err_result(format!(
                "{} ({error})",
                spec_state_unavailable_message("EnterSpecMode")
            ));
        }
    };
    if spec.get("active").and_then(Value::as_bool) == Some(true) {
        return err_result(invalid_transition(SPEC_MODE_ALREADY_ACTIVE_MESSAGE));
    }

    let plan = match read_mode_state(callbacks, "plan").await {
        Ok(value) => value,
        Err(error) => {
            return err_result(invalid_transition(&format!(
                "plan mode state could not be read, so spec mode cannot be entered atomically ({error})."
            )));
        }
    };

    let id = format!("spec-{:016x}", fastrand::u64(..));
    let dir = spec_dir(workspace_root, &id);
    if let Err(error) = std::fs::create_dir_all(&dir) {
        return err_result(format!(
            "Failed to enter spec mode: could not create the spec directory {}: {error}",
            dir.display()
        ));
    }

    let spec_write = serde_json::json!({ "active": true, "id": id });
    if let Err(error) = write_mode_state(callbacks, "spec", spec_write.clone()).await {
        return err_result(error);
    }

    let mut evicted: Vec<&str> = Vec::new();
    let plan_was_active = plan.get("active").and_then(Value::as_bool) == Some(true);
    if plan_was_active
        && let Err(error) =
            write_mode_state(callbacks, "plan", serde_json::json!({ "active": false })).await
    {
        let _ = write_mode_state(
            callbacks,
            "spec",
            serde_json::json!({ "active": false, "lastTransition": "cancel" }),
        )
        .await;
        return err_result(invalid_transition(&format!(
            "plan mode could not be exited, so spec mode was not entered ({error})."
        )));
    }
    if plan_was_active {
        evicted.push("plan");
    }
    if crate::swarm::mode::set_swarm_mode(callbacks, agent_id, None) {
        evicted.push("swarm");
    }
    if agent_id == "main"
        && !pause_tower_for_mode_enter(workspace_root, "spec mode")
            .await
            .is_empty()
    {
        evicted.push("tower");
    }

    emit_spec_mode_enter(callbacks, agent_id, &id);

    ok_result(entered_spec_mode_message(&dir, &evicted))
}

pub fn emit_spec_mode_enter(callbacks: &dyn HostCallbacks, agent_id: &str, id: &str) {
    callbacks.emit_event(serde_json::json!({
        "type": "spec_mode.enter",
        "agentId": agent_id,
        "id": id,
    }));
    callbacks.emit_event(serde_json::json!({
        "type": "agent.status.updated",
        "agentId": agent_id,
        "specMode": true,
    }));
}

fn entered_spec_mode_message(dir: &Path, evicted: &[&str]) -> String {
    let dir = dir.to_string_lossy();
    let mut lines: Vec<String> = Vec::new();
    if !evicted.is_empty() {
        lines.push(format!(
            "Left {} mode to enter spec mode.",
            evicted.join(", ")
        ));
        lines.push(String::new());
    }
    lines.extend([
        "Spec mode is now active. Your workflow:".into(),
        String::new(),
        format!("Spec directory: {dir}"),
        String::new(),
        "1. Investigate with read-only tools (Read, Grep, Glob). Use Bash only when needed.".into(),
        "2. Write the requirements document — what to build, and what is out of scope.".into(),
        "3. Write the design document — how to build it, and which alternatives you rejected."
            .into(),
        "4. Write the tasks document — each task with acceptance criteria that can be checked."
            .into(),
        "5. Call ExitSpecMode for approval.".into(),
        String::new(),
        format!(
            "Only these files may be written: {}.",
            SPEC_REQUIRED_FILES.join(", ")
        ),
        "Every other write is rejected until the user approves the spec.".into(),
    ]);
    lines.join("\n")
}

pub async fn execute_exit_spec_mode(
    callbacks: &dyn HostCallbacks,
    workspace_root: &Path,
    agent_id: &str,
    args: &Value,
    mode: PermissionMode,
) -> ExecutableToolResult {
    let options = match parse_options(args, "ExitSpecMode") {
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
        domain: "spec".into(),
        key: "spec".into(),
        turn_id: turn_id.clone(),
        tool_call_id: tool_call_id.clone(),
    };
    let spec = match callbacks.state_read(read).await {
        Ok(response) => response.value,
        Err(error) => return map_state_error(error),
    };
    if spec.get("active").and_then(Value::as_bool) != Some(true) {
        return err_result(invalid_transition(SPEC_MODE_NOT_ACTIVE_MESSAGE));
    }
    let Some(id) = active_spec_id(&spec) else {
        return err_result(format!(
            "{}: spec mode is active but its state carries no spec id, so the spec directory cannot be resolved.",
            SPEC_STATE_UNAVAILABLE
        ));
    };
    let dir = spec_dir(workspace_root, id);

    let missing: Vec<&str> = SPEC_REQUIRED_FILES
        .iter()
        .filter(|name| {
            std::fs::read_to_string(dir.join(name))
                .map(|content| content.trim().is_empty())
                .unwrap_or(true)
        })
        .copied()
        .collect();
    if !missing.is_empty() {
        return err_result(format!(
            "{SPEC_INCOMPLETE}: The spec is incomplete. Write {} in {} before calling ExitSpecMode.",
            missing.join(", "),
            dir.display()
        ));
    }

    if mode == PermissionMode::Auto {
        return exit_spec(
            callbacks,
            agent_id,
            &turn_id,
            &tool_call_id,
            id,
            &dir,
            None,
            true,
        )
        .await;
    }
    confirm_and_exit(
        callbacks,
        agent_id,
        &turn_id,
        &tool_call_id,
        id,
        &dir,
        &options,
    )
    .await
}

async fn confirm_and_exit(
    callbacks: &dyn HostCallbacks,
    agent_id: &str,
    turn_id: &str,
    tool_call_id: &str,
    id: &str,
    dir: &Path,
    options: &[ExitPlanModeOption],
) -> ExecutableToolResult {
    let dir_display = dir.to_string_lossy();
    let question = format!("Approve the spec in {dir_display} and exit spec mode?");
    let request = AskQuestionRequest {
        question_id: format!("question_{:016x}", fastrand::u64(..)),
        turn_id: turn_id.to_string(),
        tool_call_id: tool_call_id.to_string(),
        background: false,
        timeout_ms: None,
        questions: vec![AskQuestionItem {
            question: question.clone(),
            header: Some("Spec Review".into()),
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

            return ok_result(SPEC_APPROVAL_DISMISSED_MESSAGE.into());
        }
    };
    match interpret_answer(&response, &question, options) {
        Answer::Approved(selected) => {
            exit_spec(
                callbacks,
                agent_id,
                turn_id,
                tool_call_id,
                id,
                dir,
                selected.as_deref(),
                false,
            )
            .await
        }
        Answer::Rejected(Some(feedback)) => {
            ok_result(format!("User rejected the spec. Feedback:\n\n{feedback}"))
        }
        Answer::Rejected(None) => ExecutableToolResult {
            delivery: None,
            stop_turn: true,
            ..err_result(SPEC_REJECTED_MESSAGE.into())
        },
        Answer::RejectAndExit => {
            match deactivate_spec(callbacks, agent_id, turn_id, tool_call_id, id).await {
                Ok(()) => ExecutableToolResult {
                    delivery: None,
                    stop_turn: true,
                    ..err_result(SPEC_REJECTED_AND_EXITED_MESSAGE.into())
                },
                Err(result) => result,
            }
        }
        Answer::Revise => ok_result(SPEC_REVISE_MESSAGE.into()),
        Answer::Dismissed => ok_result(SPEC_APPROVAL_DISMISSED_MESSAGE.into()),
    }
}

async fn exit_spec(
    callbacks: &dyn HostCallbacks,
    agent_id: &str,
    turn_id: &str,
    tool_call_id: &str,
    id: &str,
    dir: &Path,
    selected: Option<&str>,
    auto: bool,
) -> ExecutableToolResult {
    match deactivate_spec(callbacks, agent_id, turn_id, tool_call_id, id).await {
        Ok(()) => ok_result(spec_exited_output(dir, selected, auto)),
        Err(result) => result,
    }
}

async fn deactivate_spec(
    callbacks: &dyn HostCallbacks,
    agent_id: &str,
    turn_id: &str,
    tool_call_id: &str,
    expected_id: &str,
) -> Result<(), ExecutableToolResult> {
    let _switch = mode_switch_lock().lock().await;
    match read_mode_state(callbacks, "spec").await {
        Ok(value) if active_spec_id(&value) == Some(expected_id) => {}
        Ok(_) => {
            return Err(err_result(invalid_transition(
                "spec mode changed while the approval was pending; call ExitSpecMode again.",
            )));
        }
        Err(error) => return Err(map_state_error(error)),
    }
    let write = StateWriteRequest {
        domain: "spec".into(),
        key: "spec".into(),
        value: serde_json::json!({ "active": false, "lastTransition": "exit" }),
        undoable: true,
        turn_id: turn_id.to_string(),
        tool_call_id: tool_call_id.to_string(),
    };
    match callbacks.state_write(write).await {
        Ok(_) => {
            emit_spec_mode_exit(callbacks, agent_id);
            Ok(())
        }
        Err(error) => Err(map_state_error(error)),
    }
}

fn spec_exited_output(dir: &Path, selected: Option<&str>, auto: bool) -> String {
    let dir = dir.to_string_lossy();
    if auto {
        return format!(
            "Exited spec mode. Spec saved to: {dir} — work through {dir}/tasks.md, recording progress in {dir}/progress.md as you go.\nNote: this spec was auto-approved without user review — the user has NOT explicitly approved it. Follow the user's original instructions on whether to proceed with execution; if they asked you to stop, wait, or only summarize, do not start executing."
        );
    }
    let option_prefix = match selected {
        Some(label) => format!(
            "Selected approach: {label}\nExecute ONLY the selected approach. Do not execute any unselected alternatives.\n\n"
        ),
        None => String::new(),
    };
    format!(
        "Exited spec mode. {option_prefix}Spec saved to: {dir}\n\nWork through {dir}/tasks.md, recording progress in {dir}/progress.md as you go."
    )
}

pub fn spec_denial(
    spec: &Value,
    tool_name: &str,
    args: &Value,
    workspace_root: Option<&Path>,
    bridge: &ShellPathBridge,
) -> Option<String> {
    if spec.get("active").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let name = tool_name.to_ascii_lowercase();

    if matches!(
        name.as_str(),
        "exitspecmode" | "exit_spec_mode" | "enterspecmode" | "enter_spec_mode"
    ) {
        return None;
    }
    match name.as_str() {
        "taskstop" | "task_stop" => {
            return Some(
                "TaskStop is not available in spec mode because it would mutate work that outlives the spec. Call ExitSpecMode first."
                    .into(),
            );
        }
        "croncreate" | "cron_create" | "crondelete" | "cron_delete" => {
            return Some(format!(
                "{tool_name} is not available in spec mode because it would mutate work that outlives the spec. Call ExitSpecMode first."
            ));
        }
        _ => {}
    }

    let Some(id) = active_spec_id(spec) else {
        return Some(format!(
            "{SPEC_STATE_UNAVAILABLE}: spec mode is active but its state carries no spec id, so no write can be authorised."
        ));
    };

    let accesses = declared_accesses(tool_name, args);
    if accesses
        .iter()
        .any(|access| !matches!(access, ToolResourceAccess::File(_)))
    {
        return Some(SPEC_WRITE_DENIED.into());
    }
    let writes: Vec<&ToolFileAccess> = accesses
        .iter()
        .filter_map(|access| match access {
            ToolResourceAccess::File(file)
                if matches!(
                    file.operation,
                    FileOperation::Write | FileOperation::ReadWrite
                ) =>
            {
                Some(file)
            }
            _ => None,
        })
        .collect();
    if writes.is_empty() {
        return None;
    }
    let Some(workspace_root) = workspace_root else {
        return Some(SPEC_WRITE_DENIED.into());
    };
    let dir = spec_dir(workspace_root, id).to_string_lossy().into_owned();
    for file in writes {
        let resolved = super::NativeToolset::candidate_path(workspace_root, bridge, &file.path)
            .to_string_lossy()
            .into_owned();
        if !is_within_directory(&resolved, &dir, path_class()) {
            return Some(SPEC_WRITE_DENIED.into());
        }
    }
    None
}

fn declared_accesses(tool_name: &str, args: &Value) -> Vec<ToolResourceAccess> {
    use crate::turn_loop::types::{read_file_access, read_tree_access, write_file_access};

    let name = tool_name.to_ascii_lowercase();
    let path = args.get("path").and_then(|p| p.as_str());
    match name.as_str() {
        "read" => path.map(read_file_access).into_iter().collect(),
        "write" => path.map(write_file_access).into_iter().collect(),
        "edit" => path
            .map(|p| {
                ToolResourceAccess::File(ToolFileAccess {
                    operation: FileOperation::ReadWrite,
                    path: p.to_string(),
                    recursive: false,
                })
            })
            .into_iter()
            .collect(),
        "grep" | "glob" | "listdirectory" | "list_directory" => {
            path.map(read_tree_access).into_iter().collect()
        }
        "fetchurl" | "fetch_url" | "websearch" | "web_search" | "agent" => Vec::new(),
        _ if crate::tools::github::is_github_tool(tool_name) => Vec::new(),

        "towermerge" | "tower_merge" | "towerteardown" | "tower_teardown" => {
            vec![ToolResourceAccess::File(ToolFileAccess {
                operation: FileOperation::Write,
                path: "/".into(),
                recursive: true,
            })]
        }

        _ => Vec::new(),
    }
}

fn path_class() -> PathClass {
    if cfg!(windows) {
        PathClass::Win32
    } else {
        PathClass::Posix
    }
}

fn map_state_error(error: String) -> ExecutableToolResult {
    if error.contains("does not support state bridge") {
        err_result(STATE_BRIDGE_UNSUPPORTED_FAILURE_MESSAGE.into())
    } else {
        err_result(error)
    }
}

pub fn enter_spec_mode_tool_def() -> crate::turn_loop::types::ToolInfo {
    crate::turn_loop::types::ToolInfo {
        name: "EnterSpecMode".into(),
        description: "Use this tool when the user asks for a spec, or when a change is large enough that agreeing on the contract before writing code is worth a detour.\n\nGetting sign-off on a written spec before implementation prevents wasted effort: the user reviews requirements, design and tasks once, then you execute against that agreed contract instead of guessing.\n\n**What spec mode is for.** Plan mode produces a throwaway plan for one session. Spec mode writes documents that live in the repository, get committed alongside the code, and are read again by later sessions and teammates. Reach for it when the artifact should outlive this conversation.\n\n**Your workflow — write all three required documents before calling ExitSpecMode:**\n\n1. Investigate with read-only tools (Read, Grep, Glob). Use Bash only when needed.\n2. Write `requirements.md` — what to build. State the goal, who it is for, the boundaries, and explicitly what is *out of scope*. A requirement the user did not imply is a defect, not initiative.\n3. Write `design.md` — how to build it. Name the concrete approach (files, functions, contracts), the technical constraints, and the trade-offs you rejected along with why.\n4. Write `tasks.md` — how to verify it. Split the work into tasks small enough to finish and to check independently.\n5. Call ExitSpecMode for approval. Nothing outside the spec directory can be written until the user approves.\n\n**Tasks must carry acceptance criteria.** A task that says \"implement the export endpoint\" cannot be verified. Write what must hold when it is done — the specific error, status, field, threshold or boundary that proves it. \"Handles errors\" is not a criterion; \"returns 409 when the email already exists\" is. Prefer criteria you can check by running something over criteria that need judgement.\n\n**Do not restate the project's global rules.** Conventions that already live in AGENTS.md / DEVELOP.md apply everywhere and belong there, not in a spec about one change. Reference them; do not copy them.\n\n**Keep the user in the loop on genuinely open questions.** If the requirements leave a decision that materially changes the design, ask before writing.\n\n**Constraints:**\n- Writes are restricted to the spec directory until the user approves the spec; other edits are rejected. Call ExitSpecMode when the documents are ready.\n- Do not create `progress.md` while in spec mode — it records implementation progress later, and this mode produces the three required documents above. Writes are not limited to those three: any file inside the spec directory is writable, so `progress.md` is accepted if the spec or the user calls for it.\n- Plan, spec, swarm and tower modes are mutually exclusive. Entering spec mode leaves whichever of the others was active; the result names it, if any. You do not need to exit the other mode yourself.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
    }
}

pub fn exit_spec_mode_tool_def() -> crate::turn_loop::types::ToolInfo {
    crate::turn_loop::types::ToolInfo {
        name: "ExitSpecMode".into(),
        description: "Use this tool when you are in spec mode and have finished writing the spec documents, and are ready for the user to review them.\n\n## How This Tool Works\n- The requirements, design and tasks documents are read from the spec directory, not passed as parameters.\n- The user sees all three when they review. In auto permission mode the tool exits spec mode without asking.\n- All three must be non-empty. A missing or empty document is rejected, naming the file you still have to write.\n\n## When to Use\nUse it only when the spec is genuinely ready to be approved. It does not verify the quality of what you wrote — it checks that the three documents exist and hands them to the user.\n\n## What a good spec contains\n- **Requirements**: the goal, who it is for, the boundaries, and what is explicitly out of scope. A requirement the user did not imply is a defect.\n- **Design**: the concrete approach — real files, functions and contracts — plus the trade-offs you rejected and why.\n- **Tasks**: work split small enough to finish and to check, each with acceptance criteria. \"Handles errors\" is not a criterion; \"returns 409 when the email already exists\" is.\n\n## Multiple Approaches\nIf the design document leaves a genuine choice between approaches, pass them via the `options` parameter so the user can pick which one to execute. Each option must correspond to an approach the design document actually describes.\n\n## After Approval\nSpec mode ends and normal tool permissions return. Work through the tasks document, and record progress in the spec's progress file as you go.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "options": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 3,
                    "description": "When the design document leaves a genuine choice between approaches, list them here so the user can pick which one to execute. Provide up to 3 options; 2-3 distinct approaches work best. Each option must correspond to an approach the design document actually describes. Do not use \"Reject\", \"Revise\", \"Approve\", or \"Reject and Exit\" as labels.",
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
    use crate::rpc::types::{
        AskQuestionResponse, BoxFuture, PermissionDecision, StateReadResponse, StateWriteResponse,
    };
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;

    struct ScriptedCallbacks {
        reads: StdMutex<HashMap<String, Result<Value, String>>>,
        writes: StdMutex<Vec<StateWriteRequest>>,
        events: StdMutex<Vec<Value>>,
        ask_response: Result<AskQuestionResponse, String>,
        writes_forbidden: StdMutex<bool>,
    }

    impl ScriptedCallbacks {
        fn new(reads: HashMap<String, Result<Value, String>>) -> Self {
            Self {
                reads: StdMutex::new(reads),
                writes: StdMutex::new(Vec::new()),
                events: StdMutex::new(Vec::new()),
                ask_response: Ok(AskQuestionResponse {
                    answers: HashMap::new(),
                    method: None,
                    note: None,
                    cancelled: None,
                    reason: None,
                }),
                writes_forbidden: StdMutex::new(false),
            }
        }

        fn writes(&self) -> Vec<StateWriteRequest> {
            self.writes.lock().unwrap().clone()
        }
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
            let response = self
                .reads
                .lock()
                .unwrap()
                .get(&request.domain)
                .cloned()
                .unwrap_or_else(|| Ok(serde_json::json!({ "active": false })));
            Box::pin(async move { response.map(|value| StateReadResponse { value }) })
        }

        fn state_write(
            &self,
            request: StateWriteRequest,
        ) -> BoxFuture<'static, Result<StateWriteResponse, String>> {
            if *self.writes_forbidden.lock().unwrap() {
                return Box::pin(async { Err("write forbidden by test".into()) });
            }
            self.writes.lock().unwrap().push(request.clone());
            let value = request.value.clone();
            Box::pin(async move { Ok(StateWriteResponse { ok: true, value }) })
        }

        fn ask_question(
            &self,
            _: AskQuestionRequest,
        ) -> BoxFuture<'static, Result<AskQuestionResponse, String>> {
            let response = self.ask_response.clone();
            Box::pin(async move { response })
        }

        fn emit_event(&self, event: Value) {
            self.events.lock().unwrap().push(event);
        }
    }

    fn bridge() -> ShellPathBridge {
        ShellPathBridge::new(&crate::native::shell::resolve_shell(None).program)
    }

    fn workspace() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kimi-spec-{:016x}", fastrand::u64(..)));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn active_spec(id: &str) -> Value {
        serde_json::json!({ "active": true, "id": id })
    }

    fn read_ok(value: Value) -> Result<Value, String> {
        Ok(value)
    }

    #[tokio::test]
    async fn test_enter_creates_dir_writes_state_and_exits_plan() {
        let ws = workspace();
        let callbacks = ScriptedCallbacks::new(HashMap::from([
            (
                "spec".into(),
                read_ok(serde_json::json!({ "active": false })),
            ),
            (
                "plan".into(),
                read_ok(serde_json::json!({ "active": true, "id": "plan-1", "path": "/p.md" })),
            ),
        ]));

        let result = execute_enter_spec_mode(&callbacks, &ws, "main", &serde_json::json!({})).await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("Spec mode is now active"));
        assert!(
            result
                .content
                .contains("Left plan mode to enter spec mode.")
        );
        assert!(result.content.contains(SPEC_DIR_NAME));

        let writes = callbacks.writes();
        let spec_write = writes.iter().find(|w| w.domain == "spec").unwrap();
        assert_eq!(spec_write.value["active"], true);
        let id = spec_write.value["id"].as_str().unwrap();
        assert!(spec_dir(&ws, id).is_dir(), "the spec directory must exist");
        let plan_write = writes.iter().find(|w| w.domain == "plan").unwrap();
        assert_eq!(plan_write.value, serde_json::json!({ "active": false }));

        let events = callbacks.events.lock().unwrap().clone();
        assert!(events.iter().any(|e| e["type"] == "spec_mode.enter"));
        assert!(
            events
                .iter()
                .any(|e| e["type"] == "agent.status.updated" && e["specMode"] == true)
        );
    }

    #[tokio::test]
    async fn test_enter_already_active_returns_stable_error_code() {
        let ws = workspace();
        let callbacks = ScriptedCallbacks::new(HashMap::from([(
            "spec".into(),
            read_ok(active_spec("spec-1")),
        )]));
        let result = execute_enter_spec_mode(&callbacks, &ws, "main", &serde_json::json!({})).await;
        assert!(result.is_error);
        assert!(result.content.starts_with(SPEC_MODE_INVALID));
        assert!(result.content.contains("Already in spec mode"));
        assert!(callbacks.writes().is_empty());
    }

    #[tokio::test]
    async fn test_enter_refuses_when_plan_state_is_unreadable() {
        let ws = workspace();
        let callbacks = ScriptedCallbacks::new(HashMap::from([
            (
                "spec".into(),
                read_ok(serde_json::json!({ "active": false })),
            ),
            ("plan".into(), Err("spec/plan state unreadable".into())),
        ]));
        let result = execute_enter_spec_mode(&callbacks, &ws, "main", &serde_json::json!({})).await;
        assert!(result.is_error);
        assert!(result.content.starts_with(SPEC_MODE_INVALID));
        assert!(result.content.contains("plan mode state could not be read"));
        assert!(
            callbacks.writes().is_empty(),
            "an unreadable plan state must not be treated as inactive"
        );
    }

    #[tokio::test]
    async fn test_enter_switches_plan_to_spec_in_one_transition() {
        let ws = workspace();
        let callbacks = ScriptedCallbacks::new(HashMap::from([
            (
                "spec".into(),
                read_ok(serde_json::json!({ "active": false })),
            ),
            (
                "plan".into(),
                read_ok(serde_json::json!({ "active": true, "id": "plan-1" })),
            ),
        ]));
        let result = execute_enter_spec_mode(&callbacks, &ws, "main", &serde_json::json!({})).await;
        assert!(!result.is_error);
        let writes = callbacks.writes();
        assert_eq!(
            writes.iter().filter(|w| w.domain == "spec").count(),
            1,
            "one spec write on the happy path"
        );
        assert_eq!(writes.iter().filter(|w| w.domain == "plan").count(), 1);
    }

    #[tokio::test]
    async fn test_exit_when_inactive_returns_stable_error_code() {
        let ws = workspace();
        let callbacks = ScriptedCallbacks::new(HashMap::from([(
            "spec".into(),
            read_ok(serde_json::json!({ "active": false })),
        )]));
        let result = execute_exit_spec_mode(
            &callbacks,
            &ws,
            "main",
            &serde_json::json!({}),
            PermissionMode::Manual,
        )
        .await;
        assert!(result.is_error);
        assert!(result.content.starts_with(SPEC_MODE_INVALID));
        assert!(result.content.contains("ExitSpecMode can only be called"));
        assert!(callbacks.writes().is_empty());
    }

    #[tokio::test]
    async fn test_exit_incomplete_names_missing_files() {
        let ws = workspace();
        let dir = spec_dir(&ws, "spec-1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("requirements.md"), "REQ").unwrap();

        let callbacks = ScriptedCallbacks::new(HashMap::from([(
            "spec".into(),
            read_ok(active_spec("spec-1")),
        )]));
        let result = execute_exit_spec_mode(
            &callbacks,
            &ws,
            "main",
            &serde_json::json!({}),
            PermissionMode::Manual,
        )
        .await;
        assert!(result.is_error);
        assert!(result.content.starts_with(SPEC_INCOMPLETE));
        assert!(result.content.contains("design.md"));
        assert!(result.content.contains("tasks.md"));
        assert!(!result.content.contains("requirements.md"));
        assert!(callbacks.writes().is_empty());
    }

    #[tokio::test]
    async fn test_exit_auto_does_not_ask_and_deactivates() {
        let ws = workspace();
        let dir = spec_dir(&ws, "spec-1");
        std::fs::create_dir_all(&dir).unwrap();
        for name in SPEC_REQUIRED_FILES {
            std::fs::write(dir.join(name), "content").unwrap();
        }
        let callbacks = ScriptedCallbacks::new(HashMap::from([(
            "spec".into(),
            read_ok(active_spec("spec-1")),
        )]));
        let result = execute_exit_spec_mode(
            &callbacks,
            &ws,
            "main",
            &serde_json::json!({}),
            PermissionMode::Auto,
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("auto-approved without user review"));
        let writes = callbacks.writes();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].domain, "spec");
        assert_eq!(writes[0].value["active"], false);
        assert_eq!(writes[0].value["lastTransition"], "exit");
        let events = callbacks.events.lock().unwrap().clone();
        assert!(events.iter().any(|e| e["type"] == "spec_mode.exit"));
        assert!(
            events
                .iter()
                .any(|e| e["type"] == "agent.status.updated" && e["specMode"] == false)
        );
    }

    #[tokio::test]
    async fn test_exit_manual_approval_with_selected_option() {
        let ws = workspace();
        let dir = spec_dir(&ws, "spec-1");
        std::fs::create_dir_all(&dir).unwrap();
        for name in SPEC_REQUIRED_FILES {
            std::fs::write(dir.join(name), "content").unwrap();
        }
        let question = format!(
            "Approve the spec in {} and exit spec mode?",
            dir.to_string_lossy()
        );
        let mut answers = HashMap::new();
        answers.insert(question, "Option A".to_string());
        let mut callbacks = ScriptedCallbacks::new(HashMap::from([(
            "spec".into(),
            read_ok(active_spec("spec-1")),
        )]));
        callbacks.ask_response = Ok(AskQuestionResponse {
            answers,
            method: Some("enter".into()),
            note: None,
            cancelled: None,
            reason: None,
        });
        let result = execute_exit_spec_mode(
            &callbacks,
            &ws,
            "main",
            &serde_json::json!({ "options": [{ "label": "Option A" }, { "label": "Option B" }] }),
            PermissionMode::Manual,
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("Selected approach: Option A"));
        assert_eq!(callbacks.writes().len(), 1);
    }

    #[tokio::test]
    async fn test_exit_manual_reject_keeps_spec_active() {
        let ws = workspace();
        let dir = spec_dir(&ws, "spec-1");
        std::fs::create_dir_all(&dir).unwrap();
        for name in SPEC_REQUIRED_FILES {
            std::fs::write(dir.join(name), "content").unwrap();
        }
        let question = format!(
            "Approve the spec in {} and exit spec mode?",
            dir.to_string_lossy()
        );
        let mut answers = HashMap::new();
        answers.insert(question, "Reject".to_string());
        let mut callbacks = ScriptedCallbacks::new(HashMap::from([(
            "spec".into(),
            read_ok(active_spec("spec-1")),
        )]));
        callbacks.ask_response = Ok(AskQuestionResponse {
            answers,
            method: Some("enter".into()),
            note: None,
            cancelled: None,
            reason: None,
        });
        let result = execute_exit_spec_mode(
            &callbacks,
            &ws,
            "main",
            &serde_json::json!({}),
            PermissionMode::Manual,
        )
        .await;
        assert!(result.is_error);
        assert!(result.stop_turn);
        assert_eq!(result.content, SPEC_REJECTED_MESSAGE);
        assert!(callbacks.writes().is_empty());
    }

    #[test]
    fn test_guard_allows_reads_denies_writes_outside_and_non_file_tools() {
        let ws = workspace();
        let dir = spec_dir(&ws, "spec-1");
        std::fs::create_dir_all(&dir).unwrap();
        let bridge = bridge();
        let spec = active_spec("spec-1");
        let root = Some(ws.as_path());

        assert_eq!(
            spec_denial(
                &spec,
                "Read",
                &serde_json::json!({ "path": "../Cargo.toml" }),
                root,
                &bridge
            ),
            None
        );
        assert_eq!(
            spec_denial(
                &spec,
                "Grep",
                &serde_json::json!({ "path": "src" }),
                root,
                &bridge
            ),
            None
        );

        let inside = dir.join("requirements.md").to_string_lossy().into_owned();
        assert_eq!(
            spec_denial(
                &spec,
                "Write",
                &serde_json::json!({ "path": inside }),
                root,
                &bridge
            ),
            None
        );

        let outside = ws.join("src").join("lib.rs").to_string_lossy().into_owned();
        assert_eq!(
            spec_denial(
                &spec,
                "Write",
                &serde_json::json!({ "path": outside }),
                root,
                &bridge
            ),
            Some(SPEC_WRITE_DENIED.to_string())
        );
        assert_eq!(
            spec_denial(
                &spec,
                "Edit",
                &serde_json::json!({ "path": ws.join("README.md").to_string_lossy() }),
                root,
                &bridge
            ),
            Some(SPEC_WRITE_DENIED.to_string())
        );

        assert_eq!(
            spec_denial(
                &spec,
                "Bash",
                &serde_json::json!({ "command": "ls" }),
                root,
                &bridge
            ),
            None
        );

        assert!(spec_denial(&spec, "TaskStop", &serde_json::json!({}), root, &bridge).is_some());
        assert!(spec_denial(&spec, "CronCreate", &serde_json::json!({}), root, &bridge).is_some());

        assert_eq!(
            spec_denial(&spec, "ExitSpecMode", &serde_json::json!({}), root, &bridge),
            None
        );
        assert_eq!(
            spec_denial(
                &spec,
                "EnterSpecMode",
                &serde_json::json!({}),
                root,
                &bridge
            ),
            None
        );

        assert_eq!(
            spec_denial(
                &serde_json::json!({ "active": false }),
                "Write",
                &serde_json::json!({ "path": outside }),
                root,
                &bridge
            ),
            None
        );
    }

    #[test]
    fn test_guard_fails_closed_when_active_without_id() {
        let ws = workspace();
        let deny = spec_denial(
            &serde_json::json!({ "active": true }),
            "Read",
            &serde_json::json!({ "path": "a.txt" }),
            Some(ws.as_path()),
            &bridge(),
        )
        .expect("active without id must be refused");
        assert!(deny.starts_with(SPEC_STATE_UNAVAILABLE));
    }

    #[test]
    fn test_state_persists_across_resume_and_corruption_is_not_default() {
        let dir = workspace();
        let store = crate::storage::state_store::StateStore::for_dir(dir.join("state")).unwrap();
        let outcome = store
            .apply_write(
                "spec",
                &serde_json::json!({ "active": true, "id": "spec-9" }),
            )
            .unwrap();
        store.write_domain("spec", &outcome.stored).unwrap();

        let resumed = crate::storage::state_store::StateStore::for_dir(dir.join("state")).unwrap();
        match resumed.read_domain_state("spec") {
            crate::primitives::read::ReadOutcome::Found(value) => {
                assert_eq!(value["active"], true);
                assert_eq!(value["id"], "spec-9");
            }
            other => panic!("expected Found, got {other:?}"),
        }

        std::fs::write(dir.join("state").join("spec.json"), "{not json").unwrap();
        match resumed.read_domain_state("spec") {
            crate::primitives::read::ReadOutcome::Unreadable(_) => {}
            other => panic!("expected Unreadable, got {other:?}"),
        }
    }

    #[test]
    fn test_tool_defs_match_v2_schema() {
        let enter = enter_spec_mode_tool_def();
        assert_eq!(enter.name, "EnterSpecMode");
        assert_eq!(enter.input_schema["additionalProperties"], false);
        assert!(
            enter.input_schema["properties"]
                .as_object()
                .unwrap()
                .is_empty()
        );

        let exit = exit_spec_mode_tool_def();
        assert_eq!(exit.name, "ExitSpecMode");
        assert_eq!(exit.input_schema["additionalProperties"], false);
        assert_eq!(exit.input_schema["properties"]["options"]["maxItems"], 3);
        assert!(exit.description.contains("auto permission mode"));
    }
}
