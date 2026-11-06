use super::super::*;
use std::collections::HashMap;

#[tokio::test]
async fn test_http_session_action_dual_syntax_and_controls() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let server = HttpServer::new(store.clone());

    // Create base session
    store
        .create_session("sess-dual", Some("Dual Syntax Test"))
        .unwrap();

    let msgs = [
        crate::turn_loop::types::LLMMessage::user("first question"),
        crate::turn_loop::types::LLMMessage::assistant("first reply"),
        crate::turn_loop::types::LLMMessage::user("second question"),
        crate::turn_loop::types::LLMMessage::assistant("second reply"),
    ];
    store
        .save_turn("sess-dual", "turn-1", 1, &msgs[..2], None, None)
        .unwrap();
    store
        .save_turn("sess-dual", "turn-2", 2, &msgs[2..], None, None)
        .unwrap();

    store
        .put_state(
            "goal",
            "sess-dual",
            &json!({ "objective": "verify dual syntax" }),
        )
        .unwrap();

    // 1. :status and /status
    let res_status_colon = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-dual:status".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_status_colon.status, 200);
    let status_val: Value = serde_json::from_slice(&res_status_colon.body).unwrap();
    // v2 `sessionStatusResponseSchema`: a bare status object keyed by field
    // name, with no `sessionId`.
    assert!(status_val["busy"].is_boolean());
    assert!(status_val["permission"].is_string());

    let res_status_slash = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-dual/status".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_status_slash.status, 200);

    // 2. :messages and /messages
    let res_msgs_colon = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-dual:messages".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_msgs_colon.status, 200);
    let msgs_val: Value = serde_json::from_slice(&res_msgs_colon.body).unwrap();
    assert_eq!(msgs_val["messages"].as_array().unwrap().len(), 4);

    let res_msgs_slash = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-dual/messages".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_msgs_slash.status, 200);

    // 3. :goal and /goal
    let res_goal_colon = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-dual:goal".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_goal_colon.status, 200);
    let goal_val: Value = serde_json::from_slice(&res_goal_colon.body).unwrap();
    // v2 answers the bare `goalSnapshotSchema`.
    assert_eq!(goal_val["objective"], "verify dual syntax");

    let res_goal_slash = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-dual/goal".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_goal_slash.status, 200);

    // 4. :abort and /abort
    let res_abort_colon = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/sessions/sess-dual:abort".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_abort_colon.status, 200);

    // 5. :export and /export
    let res_export_colon = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-dual:export".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_export_colon.status, 200);

    // 6. :fork
    let res_fork_colon = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/sessions/sess-dual:fork".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "title": "Colon Forked" })).unwrap(),
        })
        .await;
    assert_eq!(res_fork_colon.status, 200);
    let fork_val: Value = serde_json::from_slice(&res_fork_colon.body).unwrap();
    let new_sid = fork_val["id"].as_str().unwrap();
    let forked_history = store.load_session_history(new_sid).unwrap();
    assert_eq!(forked_history.len(), 4);

    // 7. :compact
    let res_compact_colon = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/sessions/sess-dual:compact".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    // 7. :compact — a summary is mandatory now, so a server with no
    // engine refuses before touching history. Both syntaxes must carry
    // the same refusal (the dedicated 503 contract is covered by
    // test_http_compact_without_summarizer_preserves_history).
    assert_eq!(res_compact_colon.status, 503);
    let compact_val: Value = serde_json::from_slice(&res_compact_colon.body).unwrap();
    assert_eq!(compact_val["error"], "no engine configured for this server");

    // 8. :undo
    let res_undo_colon = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/sessions/sess-dual:undo".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "count": 1 })).unwrap(),
        })
        .await;
    assert_eq!(res_undo_colon.status, 200);
    let undo_val: Value = serde_json::from_slice(&res_undo_colon.body).unwrap();
    assert_eq!(undo_val["sessionId"], "sess-dual");
}

#[tokio::test]
async fn test_http_skills_endpoints() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let server = HttpServer::new(store.clone());

    let temp_dir = tempfile::tempdir().unwrap();
    let ws_root = temp_dir.path().to_string_lossy().to_string();

    let skill_dir = temp_dir
        .path()
        .join(".agents")
        .join("skills")
        .join("test-skill");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: test-skill\ndescription: Test workspace skill\n---\n",
    )
    .unwrap();

    let ws = store.create_workspace(&ws_root, Some("Skill WS")).unwrap();
    let ws_id = ws.id;

    // 1. GET /api/v1/workspaces/:id/skills
    let res_ws = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: format!("/api/v1/workspaces/{ws_id}/skills"),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_ws.status, 200);
    let val_ws: Value = serde_json::from_slice(&res_ws.body).unwrap();
    let skills_ws = val_ws["skills"].as_array().unwrap();
    assert!(skills_ws.iter().any(|s| s["name"] == "test-skill"));
    assert!(
        skills_ws
            .iter()
            .any(|s| s["name"] == "check-kimi-code-docs")
    );

    // 2. Non-existent workspace -> 404
    let res_ws_none = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/workspaces/ws-missing/skills".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_ws_none.status, 404);

    // 3. GET /api/v1/sessions/:id/skills (both :skills and /skills)
    store
        .create_session_with_workspace("sess-skill", Some("Skill Session"), Some(&ws_id))
        .unwrap();

    let res_sess_colon = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-skill:skills".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_sess_colon.status, 200);
    let val_sess: Value = serde_json::from_slice(&res_sess_colon.body).unwrap();
    assert_eq!(val_sess["sessionId"], "sess-skill");
    let skills_sess = val_sess["skills"].as_array().unwrap();
    assert!(skills_sess.iter().any(|s| s["name"] == "test-skill"));

    let res_sess_slash = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-skill/skills".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_sess_slash.status, 200);

    // 4. Non-existent session -> 404
    let res_sess_none = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-missing:skills".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_sess_none.status, 404);

    // 5. POST activate skill -> 200
    let res_act = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/sessions/sess-skill/skills/test-skill:activate".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "args": "foo" })).unwrap(),
        })
        .await;
    assert_eq!(res_act.status, 200);
    let val_act: Value = serde_json::from_slice(&res_act.body).unwrap();
    assert_eq!(val_act["activated"], true);
    assert_eq!(val_act["skill_name"], "test-skill");
}

#[tokio::test]
async fn test_http_interaction_questions_and_approvals() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let server = HttpServer::new(store.clone());

    store
        .create_session("sess-inter", Some("Interaction Test"))
        .unwrap();

    let inter_mgr = server.interaction_manager();

    // 1. Questions: Register and resolve
    let q_req = crate::rpc::types::AskQuestionRequest {
        question_id: "q_test_http".into(),
        turn_id: "turn-1".into(),
        tool_call_id: "call_q".into(),
        background: false,
        timeout_ms: None,
        questions: vec![crate::rpc::types::AskQuestionItem {
            question: "Select mode?".into(),
            header: Some("Mode".into()),
            options: vec![
                crate::rpc::types::AskQuestionOption {
                    label: "Fast".into(),
                    description: None,
                },
                crate::rpc::types::AskQuestionOption {
                    label: "Safe".into(),
                    description: None,
                },
            ],
            multi_select: false,
        }],
    };

    let rx_q = inter_mgr.register_question("sess-inter", q_req);

    // GET questions (both /questions and :questions)
    let res_q_list_slash = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-inter/questions".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_q_list_slash.status, 200);
    let val_q_list: Value = serde_json::from_slice(&res_q_list_slash.body).unwrap();
    assert_eq!(val_q_list["items"].as_array().unwrap().len(), 1);

    let res_q_list_colon = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-inter:questions".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_q_list_colon.status, 200);

    // POST resolve question
    let res_q_resolve = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/sessions/sess-inter/questions/q_test_http:resolve".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({
                "answers": {
                    "Select mode?": "Safe"
                },
                "method": "click"
            }))
            .unwrap(),
        })
        .await;
    assert_eq!(res_q_resolve.status, 200);
    let resp_q = rx_q.await.unwrap();
    assert_eq!(resp_q.answers.get("Select mode?").unwrap(), "Safe");

    // 2. Questions: Dismiss
    let q_dismiss_req = crate::rpc::types::AskQuestionRequest {
        question_id: "q_dismiss_http".into(),
        turn_id: "turn-2".into(),
        tool_call_id: "call_d".into(),
        background: false,
        timeout_ms: None,
        questions: vec![],
    };
    let rx_d = inter_mgr.register_question("sess-inter", q_dismiss_req);

    let res_q_dismiss = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/sessions/sess-inter/questions/q_dismiss_http:dismiss".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_q_dismiss.status, 200);
    let resp_d = rx_d.await.unwrap();
    assert!(resp_d.note.is_some());

    // 3. Approvals: Register and allow
    let appr_req = crate::rpc::types::PermissionCheckRequest {
        tool_name: "Bash".into(),
        session_approval_rule: None,
        tool_call_id: "call_bash_1".into(),
        turn_id: "turn-inter".into(),
        arguments: json!({ "command": "cargo build" }),
        reason: None,
    };
    let (aid, rx_a) = inter_mgr.register_approval("sess-inter", appr_req, "build project binary");

    let res_a_list = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-inter/approvals".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_a_list.status, 200);
    let val_a_list: Value = serde_json::from_slice(&res_a_list.body).unwrap();
    assert_eq!(val_a_list["items"].as_array().unwrap().len(), 1);

    let res_a_resolve = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/sess-inter/approvals/{aid}"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({
                "decision": "approved"
            }))
            .unwrap(),
        })
        .await;
    assert_eq!(res_a_resolve.status, 200);
    let decision_a = rx_a.await.unwrap();
    assert!(decision_a.is_allow());

    // 4. Approvals: Reject / Deny
    let appr_req_deny = crate::rpc::types::PermissionCheckRequest {
        tool_name: "Write".into(),
        session_approval_rule: None,
        tool_call_id: "call_write_1".into(),
        turn_id: "turn-inter".into(),
        arguments: json!({ "path": "/root/important.conf" }),
        reason: None,
    };
    let (aid_deny, rx_deny) =
        inter_mgr.register_approval("sess-inter", appr_req_deny, "overwrite system file");

    let res_a_deny = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/sess-inter/approvals/{aid_deny}"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({
                "decision": "rejected",
                "feedback": "Denied by user choice"
            }))
            .unwrap(),
        })
        .await;
    assert_eq!(res_a_deny.status, 200);
    let decision_deny = rx_deny.await.unwrap();
    assert!(!decision_deny.is_allow());
    assert_eq!(
        decision_deny.reason.as_deref(),
        Some("Denied by user choice")
    );
}

/// An approval resolution must be explicit. A missing or unparsable body
/// must never resolve to "approved" — that would let any client able to
/// reach the route grant a dangerous tool call with an empty POST.
#[tokio::test]
async fn test_approval_resolve_is_fail_closed() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let server = HttpServer::new(store.clone());
    store
        .create_session("sess-fc", Some("Fail Closed"))
        .unwrap();

    let inter_mgr = server.interaction_manager();
    let appr_req = crate::rpc::types::PermissionCheckRequest {
        tool_name: "Bash".into(),
        session_approval_rule: None,
        tool_call_id: "call_fc".into(),
        turn_id: "turn-fc".into(),
        arguments: json!({ "command": "rm -rf /" }),
        reason: None,
    };

    // Empty body must not approve.
    let (aid, mut rx) = inter_mgr.register_approval("sess-fc", appr_req.clone(), "dangerous");
    let res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/sess-fc/approvals/{aid}"),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res.status, 400, "empty body must be rejected");
    assert!(
        rx.try_recv().is_err(),
        "no decision must reach the waiter on a rejected body"
    );

    // Malformed JSON must not approve.
    let res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/sess-fc/approvals/{aid}"),
            query: None,
            headers: HashMap::new(),
            body: b"{not json".to_vec(),
        })
        .await;
    assert_eq!(res.status, 400, "malformed body must be rejected");

    // A well-formed body without `decision` must not approve either.
    let res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/sess-fc/approvals/{aid}"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "feedback": "looks fine" })).unwrap(),
        })
        .await;
    assert_eq!(res.status, 400, "missing decision must be rejected");
    assert!(
        rx.try_recv().is_err(),
        "missing decision must not resolve the approval"
    );

    // An explicit decision still works.
    let res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/sess-fc/approvals/{aid}"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "decision": "approved" })).unwrap(),
        })
        .await;
    assert_eq!(res.status, 200);
    assert!(rx.try_recv().unwrap().is_allow());

    // Unparsable question resolution is rejected the same way.
    let q_req = crate::rpc::types::AskQuestionRequest {
        question_id: "q_fc".into(),
        turn_id: "turn-1".into(),
        tool_call_id: "call_q_fc".into(),
        background: false,
        timeout_ms: None,
        questions: vec![crate::rpc::types::AskQuestionItem {
            question: "Proceed?".into(),
            header: None,
            options: vec![crate::rpc::types::AskQuestionOption {
                label: "Yes".into(),
                description: None,
            }],
            multi_select: false,
        }],
    };
    let _rx_q = inter_mgr.register_question("sess-fc", q_req);
    let res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/sessions/sess-fc/questions/q_fc".into(),
            query: None,
            headers: HashMap::new(),
            body: b"{not json".to_vec(),
        })
        .await;
    assert_eq!(res.status, 400, "malformed question body must be rejected");
}
