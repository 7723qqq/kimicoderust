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
    // sent_messages has: system message (from run_turn), then the 5 forked
    // messages (inherited history + the fork-context system reminder)
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
    // The fork-context notice rides as a <system-reminder> injection
    // between the inherited history and the task prompt (v2 #3412).
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
    assert!(
        sent_messages[user_idx + 4]
            .content
            .contains("continue from fork")
    );
}

/// v2 refuses a background launch before `spawn` when the live table lacks
/// any of TaskList / TaskOutput / TaskStop.
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

/// Availability is caller-relative: a complete live table does not override
/// the current profile's own tool policy.
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
    assert!(result.content.contains("task_id: subagent-"));

    let agent_id = result
        .content
        .lines()
        .find(|l| l.starts_with("task_id: "))
        .unwrap()
        .strip_prefix("task_id: ")
        .unwrap();

    let wait_res = runner.wait(agent_id, 3000).await;
    assert!(matches!(
        wait_res,
        crate::storage::TaskWaitResult::Completed(_)
    ));

    let output = runner.get_output(agent_id);
    assert_eq!(
        output.as_deref(),
        Some("findings: the loop is in run_turn.rs")
    );

    let entry = runner.entry(agent_id).expect("entry must exist");
    assert_eq!(entry["status"], "completed");
    assert_eq!(entry["description"], "Scout background");
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

    let agent_id = result
        .content
        .lines()
        .find(|l| l.starts_with("task_id: "))
        .unwrap()
        .strip_prefix("task_id: ")
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let stop_res = runner
        .stop(agent_id, None)
        .await
        .expect("stop should succeed");
    assert_eq!(stop_res["status"], "killed");
    assert_eq!(stop_res["stopReason"], "Stopped by TaskStop");
}
