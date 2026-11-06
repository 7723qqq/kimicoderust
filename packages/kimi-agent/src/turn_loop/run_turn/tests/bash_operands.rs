use super::super::*;
use super::common::*;
use super::*;

// ── AGENTS.md bash operand dirs (`host/parse_bash`) ─────────────────────

// One scripted exchange: step 1 emits the configured Bash call, step 2
/// stops. Every request's messages are captured so the injection pass that
/// follows the Bash call is observable.
struct BashThenStopLlm {
    bash_args: serde_json::Value,
    requests: Arc<Mutex<Vec<Vec<LLMMessage>>>>,
}
impl LLM for BashThenStopLlm {
    fn system_prompt(&self) -> &str {
        "base"
    }
    fn model_name(&self) -> &str {
        "scripted"
    }
    fn is_retryable_error(&self, _: &str) -> bool {
        false
    }
    fn chat(
        &self,
        params: LLMChatParams,
    ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>> {
        let requests = self.requests.clone();
        let bash_args = self.bash_args.clone();
        Box::pin(async move {
            requests.lock().unwrap().push(params.messages.to_vec());
            let first = requests.lock().unwrap().len() == 1;
            let usage = TokenUsage {
                input_tokens: 1,
                output_tokens: 1,
                total_tokens: 2,
                ..Default::default()
            };
            if first {
                Ok(LLMChatResponse {
                    content: String::new(),
                    thinking: vec![],
                    tool_calls: vec![ToolCall {
                        id: "tc-bash".into(),
                        name: "Bash".into(),
                        arguments: bash_args,
                        extras: None,
                        args_parse_error: None,
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage,
                    timing: None,
                })
            } else {
                Ok(LLMChatResponse {
                    content: "done".into(),
                    thinking: vec![],
                    tool_calls: vec![],
                    finish_reason: Some("stop".into()),
                    usage,
                    timing: None,
                })
            }
        })
    }
}

/// A `HostCallbacks` wrapper whose `parse_bash` answer is scripted, with the
/// requests recorded for inspection.
struct ParseBashStub {
    inner: Arc<dyn HostCallbacks>,
    requests: Arc<Mutex<Vec<crate::rpc::types::ParseBashRequest>>>,
    answer: Result<Option<Vec<String>>, String>,
}
impl HostCallbacks for ParseBashStub {
    fn llm_chat(
        &self,
        request: crate::rpc::types::LlmChatRequest,
    ) -> crate::rpc::types::BoxFuture<'static, Result<crate::rpc::types::LlmChatResponse, String>>
    {
        self.inner.llm_chat(request)
    }
    fn execute_tool(
        &self,
        request: crate::rpc::types::ToolExecuteRequest,
    ) -> crate::rpc::types::BoxFuture<'static, Result<crate::rpc::types::ToolExecuteResponse, String>>
    {
        self.inner.execute_tool(request)
    }
    fn check_permission(
        &self,
        request: crate::rpc::types::PermissionCheckRequest,
    ) -> crate::rpc::types::BoxFuture<'static, Result<crate::rpc::types::PermissionDecision, String>>
    {
        self.inner.check_permission(request)
    }
    fn supports_parse_bash(&self) -> bool {
        true
    }

    fn parse_bash(
        &self,
        request: crate::rpc::types::ParseBashRequest,
    ) -> crate::rpc::types::BoxFuture<'static, Result<Option<Vec<String>>, String>> {
        self.requests.lock().unwrap().push(request);
        let answer = self.answer.clone();
        Box::pin(async move { answer })
    }
}

fn bash_turn_input<'a>(llm: &'a BashThenStopLlm) -> RunTurnInput<'a> {
    RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-bash-targets".into(),
        llm,
        messages: vec![LLMMessage {
            role: "user".into(),
            content: "hi".into(),
            ..Default::default()
        }],
        tools: &[],
        tool_defs: vec![],
        max_steps: 5,
        max_attempts: None,
        max_context_tokens: None,
        compaction_max_attempts: None,
        permission_mode: None,
        goal: None,
        cancellation: None,
        hook_guard: None,
        media: None,
        media_dropped: None,
        media_recovery: None,
        toolset: None,
    }
}

fn bash_turn_input_with_toolset<'a>(
    llm: &'a BashThenStopLlm,
    toolset: Arc<crate::tools::NativeToolset>,
) -> RunTurnInput<'a> {
    RunTurnInput {
        toolset: Some(toolset),
        ..bash_turn_input(llm)
    }
}

fn stub_tool_server() -> Arc<RpcServer> {
    let server = Arc::new(RpcServer::new());
    RpcServer::register_arc(&server, types::methods::HOST_EXECUTE_TOOL, |_params| {
        Box::pin(async move {
            let resp = ToolExecuteResponse {
                delivery: None,
                stop_turn: false,
                content: "stub".into(),
                is_error: false,
                note: None,
            };
            serde_json::to_value(&resp).map_err(|e| JsonRpcError::internal_error(e.to_string()))
        })
    });
    server
}

fn reminder_in(request: &[LLMMessage]) -> Option<&LLMMessage> {
    request
        .iter()
        .find(|m| m.content.contains("AGENTS.md file(s) apply"))
}

/// The full chain: the seam asks the host parser, the returned operand dirs
/// feed the reminder, and the next request names the discovered files.
#[tokio::test]
async fn bash_operand_dirs_reach_the_agents_md_reminder_through_the_host_parser() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir(root.join("sub")).unwrap();
    std::fs::write(root.join("AGENTS.md"), "root rules").unwrap();
    std::fs::write(root.join("sub").join("AGENTS.md"), "sub rules").unwrap();
    let _cwd = crate::test_support::CwdScope::enter(root).await;

    let requests: Arc<Mutex<Vec<crate::rpc::types::ParseBashRequest>>> =
        Arc::new(Mutex::new(Vec::new()));
    let stub = ParseBashStub {
        inner: rpc_callbacks(stub_tool_server()),
        requests: requests.clone(),
        answer: Ok(Some(vec![root.join("sub").to_string_lossy().into_owned()])),
    };
    let callbacks: Arc<dyn HostCallbacks> = Arc::new(stub);
    let captured: Arc<Mutex<Vec<Vec<LLMMessage>>>> = Arc::new(Mutex::new(Vec::new()));
    let llm = BashThenStopLlm {
        bash_args: serde_json::json!({ "command": "ls sub" }),
        requests: captured.clone(),
    };

    let turn = run_turn(bash_turn_input(&llm), &callbacks).await.unwrap();
    assert!(matches!(turn.stop_reason, LoopTurnStopReason::EndTurn));

    // The seam asked exactly once, with the command and the call's
    // effective cwd (no declared `cwd` → the process one).
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].command, "ls sub");
    assert_eq!(
        std::path::PathBuf::from(&requests[0].cwd),
        std::env::current_dir().unwrap()
    );
    assert!(!requests[0].home.is_empty());

    // The returned operand dir fed the reminder: the chain lists the root
    // file and the one in `sub`.
    let captured = captured.lock().unwrap();
    let reminder = reminder_in(&captured[1]).expect("the operand dir must inject");
    assert!(reminder.content.contains("AGENTS.md"), "{reminder:?}");
    assert!(reminder.content.contains("sub"), "{reminder:?}");
}

/// The engine parses the command itself: a host that carries no parser still
/// gets operand directories for the reminder.
#[tokio::test]
async fn bash_operands_resolve_without_a_host_parser() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir(root.join("sub")).unwrap();
    // The only AGENTS.md sits where *only* an operand could reach it.
    std::fs::write(root.join("sub").join("AGENTS.md"), "sub rules").unwrap();
    let _cwd = crate::test_support::CwdScope::enter(root).await;

    let callbacks = rpc_callbacks(stub_tool_server());
    let captured: Arc<Mutex<Vec<Vec<LLMMessage>>>> = Arc::new(Mutex::new(Vec::new()));
    let llm = BashThenStopLlm {
        bash_args: serde_json::json!({ "command": "ls sub" }),
        requests: captured.clone(),
    };

    let turn = run_turn(bash_turn_input(&llm), &callbacks).await.unwrap();
    assert!(matches!(turn.stop_reason, LoopTurnStopReason::EndTurn));
    let captured = captured.lock().unwrap();
    let reminder =
        reminder_in(&captured[1]).expect("the engine's own parser must find the operand dir: {:?}");
    assert!(reminder.content.contains("sub"), "{reminder:?}");
}

/// The failure arm the reminder rides on: a command the parser rejects
/// contributes nothing beyond the declared cwd, and the turn still runs.
#[tokio::test]
async fn a_command_that_does_not_parse_keeps_the_cwd_only_reminder() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir(root.join("sub")).unwrap();
    std::fs::write(root.join("sub").join("AGENTS.md"), "sub rules").unwrap();
    let _cwd = crate::test_support::CwdScope::enter(root).await;

    let callbacks = rpc_callbacks(stub_tool_server());
    let captured: Arc<Mutex<Vec<Vec<LLMMessage>>>> = Arc::new(Mutex::new(Vec::new()));
    let llm = BashThenStopLlm {
        bash_args: serde_json::json!({ "command": "ls \"unterminated sub" }),
        requests: captured.clone(),
    };

    let turn = run_turn(bash_turn_input(&llm), &callbacks).await.unwrap();
    assert!(matches!(turn.stop_reason, LoopTurnStopReason::EndTurn));
    let captured = captured.lock().unwrap();
    assert!(
        reminder_in(&captured[1]).is_none(),
        "an unparseable command must not invent an operand dir: {:?}",
        captured[1]
    );
}

/// Degradation arm two: without a parser the Bash call's own declared `cwd`
/// still probes, exactly as before this seam existed.
#[tokio::test]
async fn the_declared_cwd_still_probes_when_the_host_has_no_parser() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir(root.join("sub")).unwrap();
    std::fs::write(root.join("sub").join("AGENTS.md"), "sub rules").unwrap();
    let _cwd = crate::test_support::CwdScope::enter(root).await;

    let callbacks = rpc_callbacks(stub_tool_server());
    let captured: Arc<Mutex<Vec<Vec<LLMMessage>>>> = Arc::new(Mutex::new(Vec::new()));
    let llm = BashThenStopLlm {
        bash_args: serde_json::json!({ "command": "ls", "cwd": "sub" }),
        requests: captured.clone(),
    };

    let turn = run_turn(bash_turn_input(&llm), &callbacks).await.unwrap();
    assert!(matches!(turn.stop_reason, LoopTurnStopReason::EndTurn));
    let captured = captured.lock().unwrap();
    let reminder = reminder_in(&captured[1]).expect("the declared cwd must still inject");
    assert!(reminder.content.contains("sub"), "{reminder:?}");
}

/// Degradation arm three: a wired host whose parse fails (`Ok(None)`, v2's
/// `!parsed.ok` arm) or whose channel errors keeps the cwd-only shape —
/// the declared cwd probes, the operands do not.
#[tokio::test]
async fn a_failed_parse_keeps_the_cwd_only_shape() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir(root.join("sub")).unwrap();
    std::fs::create_dir(root.join("other")).unwrap();
    std::fs::write(root.join("sub").join("AGENTS.md"), "sub rules").unwrap();
    std::fs::write(root.join("other").join("AGENTS.md"), "other rules").unwrap();
    let _cwd = crate::test_support::CwdScope::enter(root).await;

    for answer in [Ok(None), Err("channel dead".to_string())] {
        let requests: Arc<Mutex<Vec<crate::rpc::types::ParseBashRequest>>> =
            Arc::new(Mutex::new(Vec::new()));
        let stub = ParseBashStub {
            inner: rpc_callbacks(stub_tool_server()),
            requests: requests.clone(),
            answer,
        };
        let callbacks: Arc<dyn HostCallbacks> = Arc::new(stub);
        let captured: Arc<Mutex<Vec<Vec<LLMMessage>>>> = Arc::new(Mutex::new(Vec::new()));
        let llm = BashThenStopLlm {
            bash_args: serde_json::json!({ "command": "ls other", "cwd": "sub" }),
            requests: captured.clone(),
        };

        let turn = run_turn(bash_turn_input(&llm), &callbacks).await.unwrap();
        assert!(matches!(turn.stop_reason, LoopTurnStopReason::EndTurn));
        let captured = captured.lock().unwrap();
        let reminder = reminder_in(&captured[1]).expect("the declared cwd must still inject");
        assert!(reminder.content.contains("sub"), "{reminder:?}");
        assert!(
            !reminder.content.contains("other"),
            "a failed parse must not leak operand dirs: {reminder:?}"
        );
    }
}

#[tokio::test]
async fn owned_tower_bash_deletion_returns_a_guard_veto_before_execution() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let comms = root.join(".tower/comms");
    std::fs::create_dir_all(&comms).unwrap();
    std::fs::write(
        comms.join("state.json"),
        r#"{"sessionId":"session-1","missions":[],"roster":{"agents":[]}}"#,
    )
    .unwrap();
    let _cwd = crate::test_support::CwdScope::enter(root).await;
    let toolset = Arc::new(crate::tools::NativeToolset::new(root.to_str().unwrap(), None).unwrap());
    let state = std::fs::read_to_string(
        toolset
            .tower_root()
            .join(crate::tools::tower::paths::STATE_FILE),
    )
    .unwrap();
    assert!(crate::tools::tower::bash_guard::tower_workspace_owned_json(
        &state
    ));
    assert!(
        crate::tools::tower::bash_guard::analyze_tower_bash_command(
            &crate::tools::tower::bash_guard::TowerBashGuardTarget {
                command: "rm -rf .tower/comms".into(),
                cwd: root.to_path_buf(),
                main_checkout: root.to_path_buf(),
            },
            crate::tools::tower::bash_guard::parse_shell_script,
        )
        .is_some()
    );
    let callbacks = rpc_callbacks(stub_tool_server());
    let captured: Arc<Mutex<Vec<Vec<LLMMessage>>>> = Arc::new(Mutex::new(Vec::new()));
    let llm = BashThenStopLlm {
        bash_args: serde_json::json!({ "command": "rm -rf .tower/comms" }),
        requests: captured.clone(),
    };

    let turn = run_turn(bash_turn_input_with_toolset(&llm, toolset), &callbacks)
        .await
        .unwrap();
    assert!(matches!(turn.stop_reason, LoopTurnStopReason::EndTurn));
    let captured = captured.lock().unwrap();
    let follow_up = captured[1]
        .iter()
        .find(|message| {
            message.role == "tool" && message.tool_call_id.as_deref() == Some("tc-bash")
        })
        .expect("the guard must return a tool result");
    assert!(
        follow_up.content.contains("not allowed"),
        "{}",
        follow_up.content
    );
    assert!(!follow_up.content.contains("stub"), "{}", follow_up.content);
}
