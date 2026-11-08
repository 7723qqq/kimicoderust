use kimi_tui_app::UiState;

#[tokio::test]
async fn approval_callback_blocks_until_ui_answers() {
    use kimi_agent::callbacks::HostCallbacks;
    use kimi_agent::rpc::types::PermissionCheckRequest;
    use kimi_tui_app::ApprovalResponse;
    use std::sync::Arc;
    use std::time::Duration;

    let dir = std::env::temp_dir().join(format!("kimi-tui-host-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Arc::new(kimi_agent::storage::StateStore::for_dir(dir.clone()).unwrap());

    let (transport, _commands) = kimi_tui_host::transport::EngineTransport::channel();
    let ui = UiState::new(transport);
    let host = kimi_tui_host::TuiHostCallbacks::new(store, Arc::clone(&ui));

    let req = PermissionCheckRequest {
        tool_name: "Bash".into(),
        session_approval_rule: None,
        tool_call_id: "call_1".into(),
        turn_id: "turn_1".into(),
        arguments: serde_json::json!({"command":"ls"}),
        reason: Some("run command".into()),
    };

    let ui_handle = Arc::clone(&ui);
    let engine_handle = tokio::spawn(async move { host.check_permission(req).await.unwrap() });

    // wait for pending approval to appear
    for _ in 0..40 {
        if ui_handle.pending_approval().is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        ui_handle.pending_approval().is_some(),
        "approval request must reach UI"
    );

    let approval = ui_handle.pending_approval().unwrap();
    approval.answer(ApprovalResponse {
        allowed: true,
        reason: None,
        remember: false,
    });

    let decision = engine_handle.await.unwrap();
    assert!(decision.is_allow());
    assert!(ui_handle.pending_approval().is_none());
}

#[tokio::test]
async fn question_callback_blocks_until_ui_answers() {
    use kimi_agent::callbacks::HostCallbacks;
    use kimi_agent::rpc::types::{AskQuestionItem, AskQuestionOption, AskQuestionRequest};
    use std::sync::Arc;
    use std::time::Duration;

    let dir = std::env::temp_dir().join(format!("kimi-tui-host-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Arc::new(kimi_agent::storage::StateStore::for_dir(dir.clone()).unwrap());

    let (transport, _commands) = kimi_tui_host::transport::EngineTransport::channel();
    let ui = UiState::new(transport);
    let host = kimi_tui_host::TuiHostCallbacks::new(store, Arc::clone(&ui));

    let req = AskQuestionRequest {
        question_id: "q_1".into(),
        turn_id: "turn_1".into(),
        tool_call_id: "call_q".into(),
        background: false,
        timeout_ms: None,
        questions: vec![AskQuestionItem {
            question: "Choose".into(),
            header: None,
            options: vec![AskQuestionOption {
                label: "Yes".into(),
                description: None,
            }],
            multi_select: false,
        }],
    };

    let ui_handle = Arc::clone(&ui);
    let engine_handle = tokio::spawn(async move { host.ask_question(req).await.unwrap() });

    for _ in 0..40 {
        if ui_handle.pending_question().is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(ui_handle.pending_question().is_some());

    let question = ui_handle.pending_question().unwrap();
    question.select(0, 0);
    question.answer();
    let response = engine_handle.await.unwrap();
    assert_eq!(
        response.answers.get("Choose").cloned(),
        Some("Yes".to_string())
    );
    assert!(ui_handle.pending_question().is_none());
}
