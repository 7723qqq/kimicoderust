use serde_json::Value;

use super::{err_result, ok_result};
use crate::callbacks::HostCallbacks;
use crate::rpc::types::{StateReadRequest, StateWriteRequest};
use crate::tools::todo_item::{read_todo_items, render_todo_list};
use crate::turn_loop::types::ExecutableToolResult;

fn next_step_hint(stored: &[crate::tools::todo_item::TodoItem]) -> Option<String> {
    let progress = crate::tools::todo_item::todo_progress(stored);
    if progress.open == 0 {
        return None;
    }
    let mut in_progress = progress.in_progress.iter();
    let Some(current) = in_progress.next() else {
        return Some(
            "No item is in_progress. Mark the one you are working on as in_progress.".into(),
        );
    };
    if in_progress.next().is_some() {
        return Some(
            "Several items are in_progress. Keep only the one you are working on now in_progress."
                .into(),
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

const STATE_BRIDGE_UNSUPPORTED_FAILURE_MESSAGE: &str = "The connected client does not support the state bridge. Do NOT call this tool again — the host cannot persist todo list changes.";

pub async fn execute_todo_list(
    callbacks: &dyn HostCallbacks,
    caller_agent_id: &str,
    args: &Value,
) -> ExecutableToolResult {
    let Some(todos) = args.get("todos") else {
        return read_todo_list(callbacks, args).await;
    };
    let Some(entries) = todos.as_array() else {
        return err_result(
            "Invalid TodoList arguments: `todos` must be an array of { title, status } items."
                .into(),
        );
    };
    let Some(normalized) = normalize_write_items(entries) else {
        return err_result(
            "Invalid TodoList arguments: each todo needs a non-empty `title` and a `status` of \"pending\", \"in_progress\", or \"done\"."
                .into(),
        );
    };
    write_todo_list(callbacks, caller_agent_id, args, normalized).await
}

async fn read_todo_list(callbacks: &dyn HostCallbacks, args: &Value) -> ExecutableToolResult {
    let request = StateReadRequest {
        domain: "todo".into(),
        key: "todo".into(),
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
    };
    match callbacks.state_read(request).await {
        Ok(response) => ok_result(render_todo_list(&read_todo_items(&response.value))),
        Err(error) => map_state_error(error),
    }
}

async fn write_todo_list(
    callbacks: &dyn HostCallbacks,
    caller_agent_id: &str,
    args: &Value,
    normalized: Vec<crate::tools::todo_item::TodoItem>,
) -> ExecutableToolResult {
    let value = match serde_json::to_value(&normalized) {
        Ok(value) => value,
        Err(error) => {
            return err_result(format!(
                "Failed to serialize the todo list ({error}). No changes were written; the existing list is unchanged."
            ));
        }
    };
    let submitted = value.clone();
    let request = StateWriteRequest {
        domain: "todo".into(),
        key: "todo".into(),
        value,
        undoable: true,
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
    };
    match callbacks.state_write(request).await {
        Ok(response) => {
            let stored = match &response.value {
                Value::Array(_) => response.value.clone(),
                _ => submitted,
            };
            callbacks.emit_event(serde_json::json!({
                "type": "tools.update_store",
                "agent_id": caller_agent_id,
                "key": "todo",
                "value": stored,
            }));
            let stored = read_todo_items(&response.value);
            if stored.is_empty() {
                ok_result("Todo list cleared.".into())
            } else {
                let updated = format!("Todo list updated.\n{}", render_todo_list(&stored));
                match next_step_hint(&stored) {
                    Some(hint) => ok_result(format!("{updated}\n\n{hint}")),
                    None => ok_result(updated),
                }
            }
        }
        Err(error) => map_state_error(error),
    }
}

fn normalize_write_items(entries: &[Value]) -> Option<Vec<crate::tools::todo_item::TodoItem>> {
    for entry in entries {
        let obj = entry.as_object()?;
        let title = obj.get("title")?.as_str()?;
        if title.is_empty() {
            return None;
        }
        if !matches!(
            obj.get("status").and_then(|s| s.as_str()),
            Some("pending" | "in_progress" | "done")
        ) {
            return None;
        }
    }

    Some(read_todo_items(&Value::Array(entries.to_vec())))
}

fn map_state_error(error: String) -> ExecutableToolResult {
    if error.contains("does not support state bridge") {
        err_result(STATE_BRIDGE_UNSUPPORTED_FAILURE_MESSAGE.into())
    } else {
        err_result(error)
    }
}

pub fn todo_list_tool_def() -> crate::turn_loop::types::ToolInfo {
    crate::turn_loop::types::ToolInfo {
        name: "TodoList".into(),
        description: "Use this tool to maintain a structured TODO list as you work through a multi-step task. Use it proactively and often when progress tracking helps the current work. The list is shown to the user as a live progress panel, so keep it in step with what you have actually done. This is especially useful in long-running investigations and implementation tasks with several tool calls; in plan mode, write the plan to the plan file rather than tracking it here.\n\n**When to use:**\n- Multi-step tasks that span several tool calls\n- Tracking investigation progress across a large codebase search\n- Planning a sequence of edits before making them\n- After receiving new multi-step instructions, capture the requirements as todos\n\n**When NOT to use:**\n- Single-shot answers that complete in one or two tool calls\n- Trivial requests where tracking adds no clarity\n- Purely conversational or informational replies\n\n**Granularity — split to the smallest verifiable unit:**\n- A leaf task must be one minimal, independently verifiable unit: read one file, change one function, run one command.\n- If finishing an item takes several distinct tool calls, split it further before starting it.\n- Splitting is not busywork: a fine-grained list is your working reference — each turn starts with a digest of the items you are tracking, so the granularity you record is the granularity you plan against. Finer items also make progress reportable (see below), which is how you signal and verify forward motion.\n\n**Milestone structure (tiered):**\n- Work of 3 steps or fewer: a flat list of fine-grained tasks, no milestones.\n- Work of 4 steps or more: first lay out a milestone skeleton — the first milestone is the starting point (confirm context / environment), 1..n middle milestones are coherent phases, the last milestone is the finish line (verify / wrap up) — then attach fine-grained leaf tasks under each milestone.\n- Milestones use `kind: \"milestone\"` with `parentId: null`; leaf tasks reference their milestone via `parentId`. The full list stays a flat array with parent links.\n\n**Progress reporting:**\n- On every meaningful update of an `in_progress` leaf task, include its `progress` (0-100). It is persisted and rendered — it is the signal your work is advancing.\n- Never set `progress` on milestones — it is computed from their children automatically.\n- `done` implies 100; omit `progress` on done items.\n\n**Keeping it current:**\n- Before you start a tracked task, mark it `in_progress`; while work is underway, keep exactly one task `in_progress`.\n- As soon as a task is finished, mark it `done`, before you start the next one. Do not batch completions at the end.\n- Before you write your final message, make sure the list matches what you actually did.\n- An update can go in the same response as your next tool call; it does not need a step of its own.\n- Do not re-send an unchanged list. If you are stuck and no available tool can move any task forward, tell the user instead of re-ordering the same todos.\n\n**How to use:**\n- Call with `todos: [...]` to replace the full list. Statuses: pending / in_progress / done.\n- Call with no `todos` argument to retrieve the current list without changing it.\n- Call with `todos: []` to clear the list.\n- Keep titles short and actionable (e.g. \"Read session-control.ts\", \"Add planMode flag to TurnManager\").".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "description": "The updated todo list. Omit to read the current todo list without making changes. Pass an empty array to clear the list.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {
                                "type": "string",
                                "description": "Stable identifier of the item, as returned by a previous call. Reuse it so an update lands on the same item instead of creating a new one. Omit on first write; missing ids are assigned as T1, T1.1, and so on."
                            },
                            "parentId": {
                                "type": ["string", "null"],
                                "description": "Id of the milestone this item belongs to. Null or omitted for milestones and for a flat list."
                            },
                            "kind": {
                                "type": "string",
                                "enum": ["milestone", "task"],
                                "description": "Milestone groups leaf tasks; task is a leaf unit of work. Defaults to task."
                            },
                            "title": {
                                "type": "string",
                                "minLength": 1,
                                "description": "Short, actionable title for the todo."
                            },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "done"],
                                "description": "Current status of the todo."
                            },
                            "progress": {
                                "type": "number",
                                "minimum": 0,
                                "maximum": 100,
                                "description": "Completion percentage 0-100 of an in_progress leaf task. Omit on milestones (computed from children) and on done items (implies 100)."
                            },
                            "description": {
                                "type": "string",
                                "description": "Optional longer note about the item."
                            }
                        },
                        "required": ["title", "status"]
                    }
                }
            }
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::types::{BoxFuture, PermissionDecision, StateReadResponse, StateWriteResponse};
    use std::sync::Arc;

    struct ScriptedCallbacks {
        read_response: Result<StateReadResponse, String>,
        write_response: Result<StateWriteResponse, String>,
        read_received: Arc<std::sync::Mutex<Option<StateReadRequest>>>,
        write_received: Arc<std::sync::Mutex<Option<StateWriteRequest>>>,
        events: Arc<std::sync::Mutex<Vec<Value>>>,
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

        fn emit_event(&self, event: Value) {
            self.events.lock().unwrap().push(event);
        }
    }

    #[allow(clippy::type_complexity)]
    fn scripted(
        read_response: Result<StateReadResponse, String>,
        write_response: Result<StateWriteResponse, String>,
    ) -> (
        ScriptedCallbacks,
        Arc<std::sync::Mutex<Option<StateReadRequest>>>,
        Arc<std::sync::Mutex<Option<StateWriteRequest>>>,
    ) {
        let read_received = Arc::new(std::sync::Mutex::new(None));
        let write_received = Arc::new(std::sync::Mutex::new(None));
        (
            ScriptedCallbacks {
                read_response,
                write_response,
                read_received: read_received.clone(),
                write_received: write_received.clone(),
                events: Arc::new(std::sync::Mutex::new(Vec::new())),
            },
            read_received,
            write_received,
        )
    }

    fn read_ok(value: Value) -> Result<StateReadResponse, String> {
        Ok(StateReadResponse { value })
    }

    fn write_ok(value: Value) -> Result<StateWriteResponse, String> {
        Ok(StateWriteResponse { ok: true, value })
    }

    #[tokio::test]
    async fn test_read_mode_renders_host_list() {
        let (callbacks, read_received, write_received) = scripted(
            read_ok(serde_json::json!([
                { "id": "T1", "parentId": null, "kind": "task", "title": "Read session-control.ts", "status": "in_progress", "progress": 40 },
                { "id": "T2", "parentId": null, "kind": "task", "title": "Write tests", "status": "done" }
            ])),
            write_ok(Value::Null),
        );
        let result = execute_todo_list(&callbacks, "main", &serde_json::json!({})).await;
        assert!(!result.is_error);
        assert_eq!(
            result.content,
            "Current todo list: (overall 1/2 · 70%)\n  [in_progress] T1: Read session-control.ts (40%)\n  [done] T2: Write tests"
        );
        let request = read_received.lock().unwrap().clone().unwrap();
        assert_eq!(request.domain, "todo");
        assert_eq!(request.key, "todo");
        assert_eq!(request.turn_id, "");
        assert!(write_received.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn test_read_mode_empty_list() {
        let (callbacks, _, _) = scripted(read_ok(serde_json::json!([])), write_ok(Value::Null));
        let result = execute_todo_list(&callbacks, "main", &serde_json::json!({})).await;
        assert!(!result.is_error);
        assert_eq!(result.content, "Todo list is empty.");
    }

    #[tokio::test]
    async fn test_write_mode_replaces_and_renders() {
        let (callbacks, read_received, write_received) = scripted(
            read_ok(Value::Null),
            write_ok(serde_json::json!([
                { "id": "T1", "parentId": null, "kind": "task", "title": "Read session-control.ts", "status": "in_progress", "progress": 40 },
                { "id": "T2", "parentId": null, "kind": "task", "title": "Write tests", "status": "done" }
            ])),
        );
        let result = execute_todo_list(
            &callbacks,
            "main",
            &serde_json::json!({
                "todos": [
                    { "title": "Read session-control.ts", "status": "in_progress", "progress": 40 },
                    { "title": "Write tests", "status": "done" }
                ]
            }),
        )
        .await;
        assert!(!result.is_error);
        assert_eq!(
            result.content,
            "Todo list updated.\nCurrent todo list: (overall 1/2 · 70%)\n  [in_progress] T1: Read session-control.ts (40%)\n  [done] T2: Write tests\n\n\"Read session-control.ts\" is the last open item. Once it is finished, mark it done before you write your final message; if it stays blocked or unfinished, keep it in_progress and say what remains in your reply."
        );
        assert!(read_received.lock().unwrap().is_none());
        let request = write_received.lock().unwrap().clone().unwrap();
        assert_eq!(request.domain, "todo");
        assert_eq!(request.key, "todo");
        assert!(request.undoable);

        assert_eq!(request.value[0]["id"], "T1");
        assert_eq!(request.value[0]["title"], "Read session-control.ts");
        assert_eq!(request.value[0]["status"], "in_progress");
        assert_eq!(request.value[0]["progress"], 40);
        assert_eq!(request.value[1]["id"], "T2");
    }

    #[tokio::test]
    async fn a_todo_write_emits_the_v2_update_store_record() {
        let (callbacks, _, _) = scripted(
            read_ok(Value::Null),
            write_ok(serde_json::json!([
                { "id": "T1", "parentId": null, "kind": "task", "title": "one", "status": "pending" }
            ])),
        );
        let result = execute_todo_list(
            &callbacks,
            "agent-7",
            &serde_json::json!({ "todos": [{ "title": "one", "status": "pending" }] }),
        )
        .await;
        assert!(!result.is_error);
        let events = callbacks.events.lock().unwrap();
        let record = events
            .iter()
            .find(|event| event["type"] == "tools.update_store")
            .expect("the todo write must be recorded");
        assert_eq!(record["agent_id"], "agent-7");
        assert_eq!(record["key"], "todo");
        assert_eq!(record["value"][0]["id"], "T1");
    }

    #[tokio::test]
    async fn test_write_mode_uses_host_response_when_present() {
        let (callbacks, _, _) = scripted(
            read_ok(Value::Null),
            write_ok(serde_json::json!([
                { "id": "H1", "parentId": null, "kind": "task", "title": "Host title", "status": "pending" }
            ])),
        );
        let result = execute_todo_list(
            &callbacks,
            "main",
            &serde_json::json!({ "todos": [{ "title": "Submitted", "status": "pending" }] }),
        )
        .await;
        assert!(!result.is_error);
        assert!(result.content.contains("H1: Host title"));
        assert!(!result.content.contains("Submitted"));
    }

    #[tokio::test]
    async fn test_clear_mode() {
        let (callbacks, _, write_received) =
            scripted(read_ok(Value::Null), write_ok(serde_json::json!([])));
        let result =
            execute_todo_list(&callbacks, "main", &serde_json::json!({ "todos": [] })).await;
        assert!(!result.is_error);
        assert_eq!(result.content, "Todo list cleared.");
        let request = write_received.lock().unwrap().clone().unwrap();
        assert_eq!(request.value, serde_json::json!([]));
        assert!(request.undoable);
    }

    #[tokio::test]
    async fn test_write_echo_next_step_hint_variants() {
        let cases: [(Value, &str); 4] = [
            (
                serde_json::json!([
                    { "id": "T1", "kind": "task", "title": "only pending", "status": "pending" }
                ]),
                "No item is in_progress. Mark the one you are working on as in_progress.",
            ),
            (
                serde_json::json!([
                    { "id": "T1", "kind": "task", "title": "one", "status": "in_progress" },
                    { "id": "T2", "kind": "task", "title": "two", "status": "in_progress" }
                ]),
                "Several items are in_progress. Keep only the one you are working on now in_progress.",
            ),
            (
                serde_json::json!([
                    { "id": "T1", "kind": "task", "title": "current", "status": "in_progress" },
                    { "id": "T2", "kind": "task", "title": "next", "status": "pending" }
                ]),
                "When \"current\" is finished, mark it done and set the next item in_progress; that update can go in the same response as your next tool call.",
            ),
            (
                serde_json::json!([
                    { "id": "T1", "kind": "task", "title": "last", "status": "in_progress" },
                    { "id": "T2", "kind": "task", "title": "closed", "status": "done" }
                ]),
                "\"last\" is the last open item. Once it is finished, mark it done before you write your final message; if it stays blocked or unfinished, keep it in_progress and say what remains in your reply.",
            ),
        ];
        for (stored, hint) in cases {
            let (callbacks, _, _) = scripted(read_ok(Value::Null), write_ok(stored));
            let result = execute_todo_list(
                &callbacks,
                "main",
                &serde_json::json!({ "todos": [{ "title": "x", "status": "pending" }] }),
            )
            .await;
            assert!(!result.is_error);
            assert!(
                result.content.ends_with(hint),
                "expected the hint {hint:?} in: {}",
                result.content
            );
        }

        let (callbacks, _, _) = scripted(
            read_ok(Value::Null),
            write_ok(serde_json::json!([
                { "id": "T1", "kind": "task", "title": "closed", "status": "done" }
            ])),
        );
        let result = execute_todo_list(
            &callbacks,
            "main",
            &serde_json::json!({ "todos": [{ "title": "x", "status": "done" }] }),
        )
        .await;
        assert!(!result.is_error);
        assert_eq!(
            result.content,
            "Todo list updated.\nCurrent todo list: (overall 1/1 · 100%)\n  [done] T1: closed"
        );
    }

    #[tokio::test]
    async fn test_invalid_args_return_error_without_calling_host() {
        let (callbacks, read_received, write_received) =
            scripted(read_ok(Value::Null), write_ok(Value::Null));
        for bad in [
            serde_json::json!({ "todos": "nope" }),
            serde_json::json!({ "todos": [{}] }),
            serde_json::json!({ "todos": [{ "title": "", "status": "pending" }] }),
            serde_json::json!({ "todos": [{ "title": "x", "status": "weird" }] }),
            serde_json::json!({ "todos": [{ "title": "x" }] }),
            serde_json::json!({ "todos": [42] }),
        ] {
            let result = execute_todo_list(&callbacks, "main", &bad).await;
            assert!(result.is_error, "args: {bad}");
            assert!(result.content.contains("Invalid TodoList arguments"));
        }
        assert!(read_received.lock().unwrap().is_none());
        assert!(write_received.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn test_unsupported_host_returns_failure_message() {
        let (callbacks, _, _) = scripted(
            Err("host does not support state bridge".into()),
            write_ok(Value::Null),
        );
        let result = execute_todo_list(&callbacks, "main", &serde_json::json!({})).await;
        assert!(result.is_error);
        assert_eq!(result.content, STATE_BRIDGE_UNSUPPORTED_FAILURE_MESSAGE);
    }

    #[tokio::test]
    async fn test_unsupported_host_on_write_returns_failure_message() {
        let (callbacks, _, _) = scripted(
            read_ok(Value::Null),
            Err("State write error: [-32603] host does not support state bridge".into()),
        );
        let result = execute_todo_list(
            &callbacks,
            "main",
            &serde_json::json!({ "todos": [{ "title": "x", "status": "pending" }] }),
        )
        .await;
        assert!(result.is_error);
        assert_eq!(result.content, STATE_BRIDGE_UNSUPPORTED_FAILURE_MESSAGE);
    }

    #[tokio::test]
    async fn test_other_host_error_passes_through() {
        let (callbacks, _, _) = scripted(
            Err("State read error: [-32001] unknown domain: goal".into()),
            write_ok(Value::Null),
        );
        let result = execute_todo_list(&callbacks, "main", &serde_json::json!({})).await;
        assert!(result.is_error);
        assert!(result.content.contains("-32001"));
        assert!(result.content.contains("unknown domain"));
    }

    #[tokio::test]
    async fn test_write_error_passes_through() {
        let (callbacks, _, _) = scripted(
            read_ok(Value::Null),
            Err("State write error: [-32003] invalid value: title must be a string".into()),
        );
        let result = execute_todo_list(
            &callbacks,
            "main",
            &serde_json::json!({ "todos": [{ "title": "x", "status": "pending" }] }),
        )
        .await;
        assert!(result.is_error);
        assert!(result.content.contains("-32003"));
    }

    #[tokio::test]
    async fn test_turn_and_tool_call_ids_are_forwarded() {
        let (callbacks, read_received, _) =
            scripted(read_ok(serde_json::json!([])), write_ok(Value::Null));
        let result = execute_todo_list(
            &callbacks,
            "main",
            &serde_json::json!({ "turn_id": "turn-42", "tool_call_id": "call_abc" }),
        )
        .await;
        assert!(!result.is_error);
        let request = read_received.lock().unwrap().clone().unwrap();
        assert_eq!(request.turn_id, "turn-42");
        assert_eq!(request.tool_call_id, "call_abc");
    }

    #[test]
    fn test_tool_def_matches_v2_schema() {
        let def = todo_list_tool_def();
        assert_eq!(def.name, "TodoList");
        assert_eq!(def.input_schema["type"], "object");
        assert!(def.input_schema["properties"]["todos"].is_object());
        assert_eq!(
            def.input_schema["properties"]["todos"]["items"]["required"][0],
            "title"
        );
        assert_eq!(
            def.input_schema["properties"]["todos"]["items"]["properties"]["status"]["enum"][1],
            "in_progress"
        );
        let item = &def.input_schema["properties"]["todos"]["items"]["properties"];
        for field in ["id", "parentId", "kind", "progress", "description"] {
            assert!(item[field].is_object(), "todos item must declare {field}");
        }
        assert_eq!(item["kind"]["enum"][0], "milestone");
        assert_eq!(item["progress"]["minimum"], 0);
        assert_eq!(item["progress"]["maximum"], 100);
        assert!(def.description.contains("multi-step task"));
        assert!(def.description.contains("todos: []"));
    }
}
