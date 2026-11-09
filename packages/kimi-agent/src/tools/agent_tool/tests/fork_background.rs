use super::super::*;
use super::common::*;

#[tokio::test]
async fn fork_inherits_conversation_history_and_runs_natively() {
    let recorder = Arc::new(EventRecorder::new());
    let llm = Arc::new(RecordingPromptLlm::new(vec![
        "forked agent response".into(),
    ]));
    let manager = manager_with_callbacks(llm.clone(), recorder.clone()).await;
    manager.register_builtin_profiles().await;

    let parent_history = Arc::new(std::sync::Mutex::new(vec![
        crate::turn_loop::types::LLMMessage::new("user", "parent prompt"),
        crate::turn_loop::types::LLMMessage {
            role: "assistant".into(),
            content: "I will call a tool".into(),
            blocks: Vec::new(),
            tool_calls: vec![crate::turn_loop::types::ToolCall {
                id: "call-1".into(),
                name: "Agent".into(),
                arguments: serde_json::json!({ "prompt": "child task", "fork": true }),
                extras: None,
                args_parse_error: None,
            }],
            tool_call_id: None,
            prompt_id: None,
            origin: None,
        },
    ]));

    let result = crate::tools::CURRENT_CONVERSATION_HISTORY
        .scope(parent_history, async {
            execute_agent(
                &manager,
                None,
                &serde_json::json!({
                    "prompt": "continue from fork",
                    "description": "forked child",
                    "fork": true,
                }),
                None,
                None,
                Some("call-1"),
                None,
            )
            .await
        })
        .await;

    assert!(
        result.is_some(),
        "fork must execute natively, not fall back to host"
    );
    let res = result.unwrap();
    assert!(
        !res.is_error,
        "fork execution must succeed: {}",
        res.content
    );
    assert!(res.content.contains("status: completed"));
    assert!(res.content.contains("forked agent response"));

    let messages_sent = llm.messages_sent.lock().unwrap();
    assert_eq!(messages_sent.len(), 1);
    let sent_messages = &messages_sent[0];

    let user_idx = sent_messages
        .iter()
        .position(|m| m.content == "parent prompt")
        .expect("parent prompt must be present");
    assert_eq!(sent_messages[user_idx].role, "user");
    assert_eq!(sent_messages[user_idx + 1].role, "assistant");
    assert_eq!(sent_messages[user_idx + 1].content, "I will call a tool");
    assert_eq!(sent_messages[user_idx + 2].role, "tool");
    assert_eq!(
        sent_messages[user_idx + 2].tool_call_id.as_deref(),
        Some("call-1")
    );
    assert_eq!(
        sent_messages[user_idx + 2].content,
        crate::subagent::INHERITED_IN_FLIGHT_TOOL_OUTPUT
    );

    assert_eq!(sent_messages[user_idx + 3].role, "user");
    assert!(crate::injection::is_system_reminder(
        &sent_messages[user_idx + 3].content
    ));
    assert!(
        sent_messages[user_idx + 3]
            .content
            .contains(crate::subagent::fork::FORK_CONTEXT_NOTICE.trim())
    );
    assert_eq!(sent_messages[user_idx + 4].role, "user");

    assert_eq!(sent_messages[user_idx + 4].content, "continue from fork");

    assert_eq!(sent_messages[0].role, "system");
    assert_eq!(sent_messages[0].content, "test");
}

#[tokio::test]
async fn a_spawn_sends_the_profile_system_prompt_and_the_bare_prompt() {
    let llm = Arc::new(RecordingPromptLlm::new(vec!["child answer".into()]));
    let manager = manager_with_callbacks(llm.clone(), Arc::new(EventRecorder::new())).await;
    manager.register_builtin_profiles().await;
    let coder_prompt = manager
        .get_definition("coder")
        .await
        .expect("the builtin coder profile is registered")
        .system_prompt;

    let result = execute_agent(
        &manager,
        None,
        &serde_json::json!({ "subagent_type": "coder", "prompt": "do the task" }),
        None,
        None,
        Some("call-spawn"),
        None,
    )
    .await
    .expect("a plain spawn runs natively");
    assert!(!result.is_error, "{}", result.content);

    let messages_sent = llm.messages_sent.lock().unwrap();
    let sent = &messages_sent[0];
    assert_eq!(sent[0].role, "system");
    assert_eq!(sent[0].content, coder_prompt);
    let user = sent
        .iter()
        .find(|message| message.role == "user")
        .expect("the spawn carries its prompt");
    assert_eq!(user.content, "do the task");
}

#[tokio::test]
async fn a_backend_argument_is_refused_before_spawn() {
    let recorder = Arc::new(EventRecorder::new());
    let manager = manager_with_callbacks(Arc::new(SummaryLlm), recorder.clone()).await;
    manager.register_builtin_profiles().await;

    let result = execute_agent(
        &manager,
        None,
        &serde_json::json!({
            "prompt": "must not launch",
            "subagent_type": "coder",
            "backend": "claude-code",
        }),
        None,
        None,
        Some("call-backend-unavailable"),
        None,
    )
    .await
    .expect("the native gate answers instead of falling back");

    assert!(result.is_error, "{}", result.content);
    assert_eq!(
        result.content,
        "backend is disabled: the subagent_backends experimental flag is off."
    );
    assert!(
        manager.list().await.is_empty(),
        "the refusal happens before any subagent is spawned"
    );

    let launched = execute_agent(
        &manager,
        None,
        &serde_json::json!({
            "prompt": "run in process",
            "subagent_type": "coder",
            "backend": "  ",
        }),
        None,
        None,
        Some("call-backend-blank"),
        None,
    )
    .await
    .expect("a blank backend does not fall back to the host");
    assert!(!launched.is_error, "{}", launched.content);
}

#[tokio::test]
async fn run_in_background_is_refused_before_spawn_without_task_output() {
    let manager =
        manager_with_callbacks(Arc::new(SummaryLlm), Arc::new(MissingTaskOutputCallbacks)).await;
    manager.register_builtin_profiles().await;
    let result = execute_agent(
        &manager,
        None,
        &serde_json::json!({
            "run_in_background": true,
            "prompt": "must not launch",
            "subagent_type": "coder",
        }),
        None,
        None,
        Some("call-bg-unavailable"),
        None,
    )
    .await
    .expect("the native gate answers instead of falling back");

    assert!(result.is_error, "{}", result.content);
    assert_eq!(
        result.content,
        "Background agent execution is not available for this agent because TaskList, TaskOutput, and TaskStop are not enabled."
    );
}

#[tokio::test]
async fn run_in_background_is_refused_when_caller_profile_disables_task_stop() {
    let recorder = Arc::new(EventRecorder::new());
    let manager = manager_with_callbacks(Arc::new(SummaryLlm), recorder).await;
    manager
        .register_discovered_definition(crate::subagent::types::SubagentDefinition {
            name: "foreground-only".into(),
            description: "foreground only".into(),
            system_prompt: "foreground only".into(),
            tools: vec!["Agent".into(), "TaskList".into(), "TaskOutput".into()],
            disallowed_tools: vec![],
            when_to_use: None,
            prompt_prefix: None,
            summary_policy: None,
            model: None,
            subagents: Some(vec!["coder".into()]),
        })
        .await;
    manager.set_main_profile_name(Some("foreground-only")).await;

    let result = execute_agent(
        &manager,
        None,
        &serde_json::json!({
            "run_in_background": true,
            "prompt": "must not launch",
            "subagent_type": "coder",
        }),
        None,
        None,
        None,
        None,
    )
    .await
    .expect("the native profile gate answers");
    assert!(result.is_error, "{}", result.content);
    assert_eq!(
        result.content,
        "Background agent execution is not available for this agent because TaskList, TaskOutput, and TaskStop are not enabled."
    );
}

#[tokio::test]
async fn run_in_background_registers_in_task_runner() {
    let recorder = Arc::new(EventRecorder::new());
    let llm = Arc::new(SummaryLlm);
    let manager = manager_with_callbacks(llm, recorder.clone()).await;
    manager.register_builtin_profiles().await;
    let runner = Arc::new(crate::storage::TaskRunner::new(None));
    manager.set_task_runner(runner.clone()).await;

    let result = execute_agent(
        &manager,
        None,
        &serde_json::json!({
            "run_in_background": true,
            "prompt": "background work",
            "description": "Scout background",
            "subagent_type": "coder",
        }),
        None,
        None,
        Some("call-bg-1"),
        None,
    )
    .await
    .expect("must execute natively");

    assert!(!result.is_error);
    assert!(result.content.contains("status: running"));
    assert!(
        result.content.contains("task_id: agent-"),
        "v2 mints the background task id from the agent prefix:\n{}",
        result.content
    );

    let field = |name: &str| {
        result
            .content
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name}: ")))
            .unwrap_or_else(|| panic!("the result carries {name}:\n{}", result.content))
            .to_string()
    };
    let task_id = field("task_id");
    let agent_id = field("agent_id");
    assert_ne!(
        task_id, agent_id,
        "the task id is its own identifier, not the agent id"
    );
    assert!(
        result
            .content
            .contains(&format!("NOT task_id (\"{task_id}\")")),
        "the resume hint names the task id explicitly:\n{}",
        result.content
    );

    let wait_res = runner.wait(&task_id, 3000).await;
    assert!(matches!(
        wait_res,
        crate::storage::TaskWaitResult::Completed(_)
    ));

    let output = runner.get_output(&task_id);
    assert_eq!(
        output.as_deref(),
        Some("findings: the loop is in run_turn.rs")
    );

    let entry = runner.entry(&task_id).expect("entry must exist");
    assert_eq!(entry["status"], "completed");
    assert_eq!(entry["description"], "Scout background");
    assert_eq!(
        entry["agentId"], agent_id,
        "the task keeps the agent id for resume"
    );
}

#[tokio::test]
async fn run_in_background_cooperative_stop_via_task_runner() {
    let recorder = Arc::new(EventRecorder::new());
    let llm = Arc::new(ToolForeverLlm);
    let manager = manager_with_callbacks(llm, recorder.clone()).await;
    register_looper(&manager).await;
    let runner = Arc::new(crate::storage::TaskRunner::new(None));
    manager.set_task_runner(runner.clone()).await;

    let result = execute_agent(
        &manager,
        None,
        &serde_json::json!({
            "run_in_background": true,
            "prompt": "loop in background",
            "description": "Background looper",
            "subagent_type": "looper",
        }),
        None,
        None,
        Some("call-bg-2"),
        None,
    )
    .await
    .expect("must execute natively");

    let task_id = result
        .content
        .lines()
        .find(|l| l.starts_with("task_id: "))
        .unwrap()
        .strip_prefix("task_id: ")
        .unwrap();
    assert!(
        task_id.starts_with("agent-"),
        "a background subagent task id carries the agent prefix, got {task_id}"
    );

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let stop_res = runner
        .stop(task_id, None)
        .await
        .expect("stop should succeed")
        .wire;
    assert_eq!(stop_res["status"], "killed");
    assert_eq!(stop_res["stopReason"], "Stopped by TaskStop");
}

#[tokio::test]
async fn a_fork_may_name_the_callers_own_model_alias() {
    let recorder = Arc::new(EventRecorder::new());
    let llm =
        Arc::new(RecordingPromptLlm::new(vec!["child result".into()]).with_alias("mock-model"));
    let manager = manager_with_callbacks(llm.clone(), recorder.clone()).await;
    manager.register_builtin_profiles().await;

    let refused = execute_agent(
        &manager,
        None,
        &serde_json::json!({
            "prompt": "Continue the analysis",
            "description": "Fork context",
            "fork": true,
            "model": "provider-other",
        }),
        None,
        None,
        None,
        None,
    )
    .await
    .expect("the tool handles the call");
    assert!(refused.is_error, "content: {}", refused.content);
    assert!(
        refused.content.contains("Cannot override the model"),
        "content: {}",
        refused.content
    );

    let accepted = execute_agent(
        &manager,
        None,
        &serde_json::json!({
            "prompt": "Continue the analysis",
            "description": "Fork context",
            "fork": true,
            "model": "mock-model",
        }),
        None,
        None,
        None,
        None,
    )
    .await
    .expect("the tool handles the call");
    assert!(
        !accepted.is_error,
        "the caller's own alias is an inherit, not an override: {}",
        accepted.content
    );
    assert!(accepted.content.contains("child result"));
}
