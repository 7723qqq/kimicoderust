use super::*;
use crate::rpc::types::{AskQuestionItem, AskQuestionOption};
use std::sync::atomic::AtomicU32;

/// A stub that records emitted events so the counter can be asserted.
struct RecordingCallbacks {
    events: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
}

impl HostCallbacks for RecordingCallbacks {
    fn llm_chat(&self, _: LlmChatRequest) -> BoxFuture<'static, Result<LlmChatResponse, String>> {
        Box::pin(async { Err("not used".into()) })
    }

    fn execute_tool(
        &self,
        _: ToolExecuteRequest,
    ) -> BoxFuture<'static, Result<ToolExecuteResponse, String>> {
        Box::pin(async { Err("not used".into()) })
    }

    fn check_permission(
        &self,
        _: PermissionCheckRequest,
    ) -> BoxFuture<'static, Result<PermissionDecision, String>> {
        Box::pin(async {
            Ok(PermissionDecision {
                decision: "allow".into(),
                reason: None,
            })
        })
    }

    fn emit_event(&self, event: serde_json::Value) {
        self.events.lock().unwrap().push(event);
    }
}

#[test]
fn test_counting_callbacks_counts_events_and_forwards() {
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let counter = Arc::new(AtomicU32::new(0));
    let callbacks = CountingCallbacks::new(
        Arc::new(RecordingCallbacks {
            events: events.clone(),
        }),
        counter.clone(),
    );

    callbacks.emit_event(serde_json::json!({ "type": "a" }));
    callbacks.emit_event(serde_json::json!({ "type": "b" }));

    assert_eq!(counter.load(Ordering::Relaxed), 2);
    assert_eq!(
        events.lock().unwrap().len(),
        2,
        "events must still be forwarded"
    );
}

/// A turn's events name their agent, and the typed variants can actually
/// read them back.
///
/// Every event the turn loop produces stamps `agent_id` at construction
/// (v2 `loopService.ts:1426-1455`). Before that the events omitted the
/// field entirely, so `EngineEvent::from_json` fell through to its
/// `Custom` arm and every typed variant in this enum was dead code — a
/// subagent's tool call then reached the host with no owner at all, and
/// the host read it as `main`. Pin both halves: the stamp is present, and
/// the payload deserializes into the variant that declares the field.
#[test]
fn a_stamped_event_round_trips_into_its_typed_variant() {
    use crate::events::EngineEvent;

    let payload = serde_json::json!({
        "type": "tool.native",
        "agent_id": "subagent-42",
        "turn_id": "subturn-7",
        "tool_call_id": "call-1",
        "tool_name": "Read",
        "arguments": {},
        "content": "body",
        "is_error": false,
        "note": null,
    });

    let event = EngineEvent::from_json(payload);
    assert!(
        matches!(event, EngineEvent::ToolNative { .. }),
        "a stamped tool.native must deserialize into its typed variant, \
             not degrade to Custom — that degradation is what left a \
             subagent's tool call with no owner. Got: {event:?}"
    );
    assert_eq!(event.agent_id(), Some("subagent-42"));
}

/// `agent_id()` reads the `Custom` payload too, so an event that predates
/// the stamp is still attributable (and, like v2's
/// `matchesAgentFilter`, anything without one passes the filter through).
#[test]
fn a_custom_event_still_reports_an_owner_when_it_carries_one() {
    use crate::events::EngineEvent;

    let event = EngineEvent::from_json(serde_json::json!({
        "type": "custom.thing",
        "agent_id": "subagent-9",
    }));
    assert_eq!(event.agent_id(), Some("subagent-9"));

    let anonymous = EngineEvent::from_json(serde_json::json!({ "type": "custom.thing" }));
    assert_eq!(anonymous.agent_id(), None);
}

/// Base callbacks whose permission verdicts are scripted, recording any
/// tool executions that reach them.
struct ScriptedPermissionCallbacks {
    decision: PermissionDecision,
    permission_calls: Arc<AtomicU32>,
    executed: Arc<AtomicU32>,
}

impl HostCallbacks for ScriptedPermissionCallbacks {
    fn llm_chat(&self, _: LlmChatRequest) -> BoxFuture<'static, Result<LlmChatResponse, String>> {
        Box::pin(async { Err("not used".into()) })
    }

    fn execute_tool(
        &self,
        _: ToolExecuteRequest,
    ) -> BoxFuture<'static, Result<ToolExecuteResponse, String>> {
        self.executed.fetch_add(1, Ordering::Relaxed);
        Box::pin(async {
            Ok(ToolExecuteResponse {
                delivery: None,
                stop_turn: false,
                content: "host executed".into(),
                is_error: false,
                note: None,
            })
        })
    }

    fn check_permission(
        &self,
        _: PermissionCheckRequest,
    ) -> BoxFuture<'static, Result<PermissionDecision, String>> {
        self.permission_calls.fetch_add(1, Ordering::Relaxed);
        let decision = self.decision.clone();
        Box::pin(async move { Ok(decision) })
    }
}

fn gate_setup(
    decision: PermissionDecision,
) -> (
    tempfile::TempDir,
    NativeToolCallbacks,
    Arc<AtomicU32>,
    Arc<AtomicU32>,
    Arc<AtomicU32>,
) {
    let dir = tempfile::tempdir().unwrap();
    let toolset = Arc::new(NativeToolset::new(dir.path().to_str().unwrap(), None).unwrap());
    let permission_calls = Arc::new(AtomicU32::new(0));
    let executed = Arc::new(AtomicU32::new(0));
    let native_count = Arc::new(AtomicU32::new(0));
    let native = NativeToolCallbacks {
        inner: Arc::new(ScriptedPermissionCallbacks {
            decision,
            permission_calls: permission_calls.clone(),
            executed: executed.clone(),
        }),
        toolset,
        native_count: native_count.clone(),
        truncator: None,
        permission_engine: None,
        plan_guard: None,
        stale_guard: None,
        goal_guard: None,
        hook_guard: None,
        agent_tool_veto: None,
        tools_veto: None,
        todo_tool_veto: None,
        tower_worktree_root: None,
        tower_enabled: false,
        sandbox_policy: None,
    };
    (dir, native, permission_calls, executed, native_count)
}

use crate::tools::NativeToolset;

/// Inner callbacks for the P52 veto tests: counts host executions and
/// permission consults, and records every `emit_event` payload.
struct VetoProbeCallbacks {
    permission_calls: Arc<AtomicU32>,
    executed: Arc<AtomicU32>,
    events: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    checkpoints: Arc<std::sync::Mutex<Vec<CheckpointRequest>>>,
    telemetry_events: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
}

impl HostCallbacks for VetoProbeCallbacks {
    fn telemetry(&self, event: serde_json::Value) {
        self.telemetry_events.lock().unwrap().push(event);
    }
    fn checkpoint(&self, request: CheckpointRequest) -> BoxFuture<'static, Result<(), String>> {
        self.checkpoints.lock().unwrap().push(request);
        Box::pin(async { Ok(()) })
    }
    fn llm_chat(&self, _: LlmChatRequest) -> BoxFuture<'static, Result<LlmChatResponse, String>> {
        Box::pin(async { Err("not used".into()) })
    }
    fn execute_tool(
        &self,
        _: ToolExecuteRequest,
    ) -> BoxFuture<'static, Result<ToolExecuteResponse, String>> {
        self.executed.fetch_add(1, Ordering::Relaxed);
        Box::pin(async {
            Ok(ToolExecuteResponse {
                delivery: None,
                stop_turn: false,
                content: "host executed".into(),
                is_error: false,
                note: None,
            })
        })
    }
    fn check_permission(
        &self,
        _: PermissionCheckRequest,
    ) -> BoxFuture<'static, Result<PermissionDecision, String>> {
        self.permission_calls.fetch_add(1, Ordering::Relaxed);
        Box::pin(async { Ok(PermissionDecision::allow()) })
    }
    fn emit_event(&self, event: serde_json::Value) {
        self.events.lock().unwrap().push(event);
    }
}

type VetoProbe = (
    tempfile::TempDir,
    NativeToolCallbacks,
    Arc<AtomicU32>,
    Arc<AtomicU32>,
    Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
);

fn veto_setup(agent_tool_veto: Option<String>, tools_veto: Option<String>) -> VetoProbe {
    let dir = tempfile::tempdir().unwrap();
    let toolset = Arc::new(NativeToolset::new(dir.path().to_str().unwrap(), None).unwrap());
    let permission_calls = Arc::new(AtomicU32::new(0));
    let executed = Arc::new(AtomicU32::new(0));
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let native = NativeToolCallbacks {
        inner: Arc::new(VetoProbeCallbacks {
            permission_calls: permission_calls.clone(),
            executed: executed.clone(),
            events: events.clone(),
            checkpoints: Arc::new(std::sync::Mutex::new(Vec::new())),
            telemetry_events: Arc::new(std::sync::Mutex::new(Vec::new())),
        }),
        toolset,
        native_count: Arc::new(AtomicU32::new(0)),
        truncator: None,
        permission_engine: None,
        plan_guard: None,
        stale_guard: None,
        goal_guard: None,
        hook_guard: None,
        agent_tool_veto,
        tools_veto,
        todo_tool_veto: None,
        tower_worktree_root: None,
        tower_enabled: false,
        sandbox_policy: None,
    };
    (dir, native, permission_calls, executed, events)
}

fn veto_events(events: &std::sync::Mutex<Vec<serde_json::Value>>) -> Vec<(String, String)> {
    events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| {
            let kind = event.get("type")?.as_str()?.to_string();
            let content = event.get("content")?.as_str()?.to_string();
            Some((kind, content))
        })
        .collect()
}

#[tokio::test]
async fn test_tools_veto_denies_every_native_call() {
    let (_dir, native, permission_calls, executed, events) =
        veto_setup(None, Some("side chat: tools are off".into()));
    let response = native
        .execute_tool(ToolExecuteRequest {
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            turn_id: "t".into(),
            tool_call_id: "c1".into(),
            tool_name: "Read".into(),
            arguments: serde_json::json!({ "path": "a.txt" }),
        })
        .await
        .unwrap();
    assert!(response.is_error);
    assert_eq!(response.content, "side chat: tools are off");
    assert_eq!(
        permission_calls.load(Ordering::Relaxed),
        0,
        "veto outranks permission"
    );
    assert_eq!(executed.load(Ordering::Relaxed), 0, "no host fallback");
    assert_eq!(
        veto_events(&events),
        vec![(
            "tool.native".to_string(),
            "side chat: tools are off".to_string()
        )]
    );
}

#[tokio::test]
async fn test_agent_tool_veto_denies_only_agent() {
    let (_dir, native, permission_calls, executed, events) =
        veto_setup(Some("swarm mode denies Agent".into()), None);
    let denied = native
        .execute_tool(ToolExecuteRequest {
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            turn_id: "t".into(),
            tool_call_id: "c1".into(),
            tool_name: "Agent".into(),
            arguments: serde_json::json!({ "prompt": "x" }),
        })
        .await
        .unwrap();
    assert!(denied.is_error);
    assert_eq!(denied.content, "swarm mode denies Agent");
    // Any other tool passes the veto gate and reaches permission.
    let allowed = native
        .execute_tool(ToolExecuteRequest {
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            turn_id: "t".into(),
            tool_call_id: "c2".into(),
            tool_name: "Glob".into(),
            arguments: serde_json::json!({ "pattern": "*.rs" }),
        })
        .await
        .unwrap();
    assert!(!allowed.is_error, "non-Agent tools are not vetoed");
    assert_eq!(permission_calls.load(Ordering::Relaxed), 1);
    let events = veto_events(&events);
    assert_eq!(
        events[0],
        (
            "tool.native".to_string(),
            "swarm mode denies Agent".to_string()
        )
    );
    let _ = executed;
}

#[tokio::test]
async fn test_todo_tool_veto_denies_todolist() {
    let mut callbacks_tuple = veto_setup(None, None);
    callbacks_tuple.1.todo_tool_veto = Some("TodoList is disabled in tower mode".into());
    let native = callbacks_tuple.1;
    let events = callbacks_tuple.4;

    let denied = native
        .execute_tool(ToolExecuteRequest {
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            turn_id: "t1".into(),
            tool_call_id: "c_todo".into(),
            tool_name: "TodoList".into(),
            arguments: serde_json::json!({ "todos": [] }),
        })
        .await
        .unwrap();

    assert!(denied.is_error);
    assert_eq!(denied.content, "TodoList is disabled in tower mode");

    let recorded_events = veto_events(&events);
    assert_eq!(
        recorded_events[0],
        (
            "tool.native".to_string(),
            "TodoList is disabled in tower mode".to_string()
        )
    );
}

#[tokio::test]
async fn test_tower_worktree_root_restricts_worker_writes() {
    let mut callbacks_tuple = veto_setup(None, None);
    callbacks_tuple.1.tower_worktree_root = Some("G:/repo/.tower/worktrees/worker-1".into());
    let native = callbacks_tuple.1;

    // 1. 写工作区外部路径 -> 拦截拒绝
    let escape_res = native
        .execute_tool(ToolExecuteRequest {
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            turn_id: "t2".into(),
            tool_call_id: "c_esc".into(),
            tool_name: "Write".into(),
            arguments: serde_json::json!({
                "path": "G:/repo/src/root.rs",
                "content": "fn bad() {}"
            }),
        })
        .await
        .unwrap();

    assert!(escape_res.is_error);
    assert!(
        escape_res
            .content
            .contains("tower workers may only write inside their own worktree")
    );

    // 2. 写工作区合法内部路径 -> 放行至权限/执行层
    let in_worktree_res = native
        .execute_tool(ToolExecuteRequest {
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            turn_id: "t2".into(),
            tool_call_id: "c_ok".into(),
            tool_name: "Write".into(),
            arguments: serde_json::json!({
                "path": "G:/repo/.tower/worktrees/worker-1/src/lib.rs",
                "content": "fn ok() {}"
            }),
        })
        .await
        .unwrap();

    // 不应触发 tower worktree 逃逸拦截
    assert!(!escape_res.content.contains("fn ok"));
    assert!(
        !in_worktree_res
            .content
            .contains("tower workers may only write inside their own worktree")
    );
}

/// The advertised Tower* table must follow the experiment flag, not the
/// worker write guard. The gate used to be `tower_worktree_root.is_some()`
/// — a per-worker scope that no production caller sets, which left the
/// whole tower toolset unreachable on the product path.
#[tokio::test]
async fn test_tower_tool_table_follows_the_experiment_flag() {
    let disabled = veto_setup(None, None).1;
    let table = disabled.list_tools().await.unwrap();
    assert!(
        !table.tools.iter().any(|t| t.name == "TowerInit"),
        "tower tools must not be advertised while the experiment is off"
    );

    let mut enabled = veto_setup(None, None).1;
    enabled.tower_enabled = true;
    let table = enabled.list_tools().await.unwrap();
    assert!(
        table.tools.iter().any(|t| t.name == "TowerInit"),
        "tower tools must be advertised once the experiment is on"
    );

    // Setting only the worker guard must NOT re-enable the table.
    let mut guard_only = veto_setup(None, None).1;
    guard_only.tower_worktree_root = Some("G:/repo/.tower/worktrees/worker-1".into());
    let table = guard_only.list_tools().await.unwrap();
    assert!(
        !table.tools.iter().any(|t| t.name == "TowerInit"),
        "the worker write guard is not the feature switch"
    );
}

#[tokio::test]
async fn test_sandbox_policy_enforcement() {
    use crate::tools::sandbox::SandboxExecutionPolicy;

    // 1. ReadOnly 模式阻断 Write 与 Edit
    let mut callbacks_tuple = veto_setup(None, None);
    callbacks_tuple.1.sandbox_policy = Some(SandboxExecutionPolicy::read_only("/workspace/root"));
    let native = callbacks_tuple.1;

    let ro_res = native
        .execute_tool(ToolExecuteRequest {
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            turn_id: "t_ro".into(),
            tool_call_id: "c_ro".into(),
            tool_name: "Write".into(),
            arguments: serde_json::json!({
                "path": "/workspace/root/test.txt",
                "content": "hello"
            }),
        })
        .await
        .unwrap();

    assert!(ro_res.is_error);
    assert!(ro_res.content.contains("blocks writes to the filesystem"));

    // 2. WorkspaceWrite 模式拦截工作区外部写
    let mut callbacks_tuple = veto_setup(None, None);
    callbacks_tuple.1.sandbox_policy =
        Some(SandboxExecutionPolicy::workspace_write("/workspace/root"));
    let native_ww = callbacks_tuple.1;

    let out_res = native_ww
        .execute_tool(ToolExecuteRequest {
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            turn_id: "t_out".into(),
            tool_call_id: "c_out".into(),
            tool_name: "Edit".into(),
            arguments: serde_json::json!({
                "path": "/etc/shadow",
                "old_string": "a",
                "new_string": "b"
            }),
        })
        .await
        .unwrap();

    assert!(out_res.is_error);
    assert!(
        out_res
            .content
            .contains("blocks writes outside the workspace root")
    );

    // 3. WorkspaceWrite 模式放行工作区内部写（进入权限层，不会被沙箱拒否）
    let in_res = native_ww
        .execute_tool(ToolExecuteRequest {
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            turn_id: "t_in".into(),
            tool_call_id: "c_in".into(),
            tool_name: "Write".into(),
            arguments: serde_json::json!({
                "path": "/workspace/root/sub/file.txt",
                "content": "ok"
            }),
        })
        .await
        .unwrap();

    assert!(
        !in_res
            .content
            .contains("blocks writes outside the workspace root")
    );

    // 4. 只读工具（如 Glob）在只读沙箱模式下不受写沙箱影响
    let glob_res = native
        .execute_tool(ToolExecuteRequest {
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            turn_id: "t_glob".into(),
            tool_call_id: "c_glob".into(),
            tool_name: "Glob".into(),
            arguments: serde_json::json!({ "pattern": "*.txt" }),
        })
        .await
        .unwrap();

    assert!(!glob_res.is_error);

    // 5. ReadOnly 模式必须拦截 Bash：shell 是代码执行入口，不能绕过写沙箱。
    //    拒绝必须发生在真实 shell 派生之前（否则只读沙箱形同虚设）。
    let bash_ro_res = native
        .execute_tool(ToolExecuteRequest {
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            turn_id: "t_bash_ro".into(),
            tool_call_id: "c_bash_ro".into(),
            tool_name: "Bash".into(),
            arguments: serde_json::json!({ "command": "echo sandbox-escape" }),
        })
        .await
        .unwrap();

    assert!(bash_ro_res.is_error);
    assert!(bash_ro_res.content.contains("blocks code execution"));
    assert!(!bash_ro_res.content.contains("sandbox-escape"));

    // 6. WorkspaceWrite 模式放行命令执行（纯策略断言，不派生真 shell）。
    let ww_policy = SandboxExecutionPolicy::workspace_write("/workspace/root");
    assert!(ww_policy.sandbox_code_execution_guard().is_none());
    // Off 模式同样放行。
    let off_policy = SandboxExecutionPolicy::off("/workspace/root");
    assert!(off_policy.sandbox_code_execution_guard().is_none());
}

#[tokio::test]
async fn test_native_write_checkpoints_prepare_and_record() {
    let (_dir, _native, _permission_calls, _executed, _events) = veto_setup(None, None);
    // Drill into the inner probe: the setup closure hides it, so drive
    // the public seam twice and assert on the checkpoint calls via a
    // fresh probe we control directly.
    let dir = tempfile::tempdir().unwrap();
    let toolset = Arc::new(NativeToolset::new(dir.path().to_str().unwrap(), None).unwrap());
    let checkpoints = Arc::new(std::sync::Mutex::new(Vec::new()));
    let telemetry_events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let native = NativeToolCallbacks {
        inner: Arc::new(VetoProbeCallbacks {
            permission_calls: Arc::new(AtomicU32::new(0)),
            executed: Arc::new(AtomicU32::new(0)),
            events: Arc::new(std::sync::Mutex::new(Vec::new())),
            checkpoints: checkpoints.clone(),
            telemetry_events: telemetry_events.clone(),
        }),
        toolset,
        native_count: Arc::new(AtomicU32::new(0)),
        truncator: None,
        permission_engine: None,
        plan_guard: None,
        stale_guard: None,
        goal_guard: None,
        hook_guard: None,
        agent_tool_veto: None,
        tools_veto: None,
        todo_tool_veto: None,
        tower_worktree_root: None,
        tower_enabled: false,
        sandbox_policy: None,
    };
    let response = native
        .execute_tool(ToolExecuteRequest {
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            turn_id: "turn-7".into(),
            tool_call_id: "c9".into(),
            tool_name: "Write".into(),
            arguments: serde_json::json!({ "path": "cp.txt", "content": "x" }),
        })
        .await
        .unwrap();
    assert!(!response.is_error);
    let recorded = checkpoints.lock().unwrap().clone();
    let phases: Vec<&str> = recorded.iter().map(|r| r.phase.as_str()).collect();
    assert_eq!(phases, vec!["prepare", "record"]);
    assert_eq!(recorded[0].turn_id, "turn-7");
    assert_eq!(recorded[0].paths, vec!["cp.txt".to_string()]);
    assert!(!recorded[0].executed);
    assert!(recorded[1].executed);
    // P54: one tool_call telemetry event, success outcome, numeric turn
    // id (the wire turn id is not numeric here, so it degrades to 0).
    let telemetry = telemetry_events.lock().unwrap();
    assert_eq!(telemetry.len(), 1);
    let event = &telemetry[0];
    assert_eq!(event["event"], "tool_call");
    assert_eq!(event["tool_call_id"], "c9");
    assert_eq!(event["tool_name"], "Write");
    assert_eq!(event["outcome"], "success");
    assert_eq!(event["dup_type"], "normal");
    assert_eq!(event["turn_id"], 0);
    assert!(event["duration_ms"].is_u64());
}

#[tokio::test]
async fn test_native_write_requires_permission_and_runs_on_allow() {
    let (dir, native, permission_calls, executed, native_count) = gate_setup(PermissionDecision {
        decision: "allow".into(),
        reason: None,
    });
    let response = native
        .execute_tool(ToolExecuteRequest {
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            turn_id: "t".into(),
            tool_call_id: "c1".into(),
            tool_name: "Write".into(),
            arguments: serde_json::json!({ "path": "made.txt", "content": "x" }),
        })
        .await
        .unwrap();
    assert_eq!(
        permission_calls.load(Ordering::Relaxed),
        1,
        "permission must be consulted"
    );
    assert_eq!(
        executed.load(Ordering::Relaxed),
        0,
        "allowed calls never reach the host executor"
    );
    assert_eq!(
        native_count.load(Ordering::Relaxed),
        1,
        "an in-process execution is reported"
    );
    assert!(!response.is_error);
    assert!(dir.path().join("made.txt").exists());
}

#[tokio::test]
async fn test_native_write_denial_never_executes() {
    let (_dir, native, permission_calls, executed, native_count) = gate_setup(PermissionDecision {
        decision: "deny".into(),
        reason: Some("user declined".into()),
    });
    let response = native
        .execute_tool(ToolExecuteRequest {
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            turn_id: "t".into(),
            tool_call_id: "c2".into(),
            tool_name: "Write".into(),
            arguments: serde_json::json!({ "path": "made.txt", "content": "x" }),
        })
        .await
        .unwrap();
    assert_eq!(permission_calls.load(Ordering::Relaxed), 1);
    assert_eq!(
        executed.load(Ordering::Relaxed),
        0,
        "denied calls must not fall back to the host"
    );
    assert_eq!(
        native_count.load(Ordering::Relaxed),
        0,
        "a denied call executed nowhere, so it is not a native execution"
    );
    assert!(response.is_error);
    assert!(response.content.contains("user declined"));
    assert!(!_dir.path().join("made.txt").exists());
}

#[tokio::test]
async fn test_read_only_tools_are_gated_too() {
    // The host policy chain can require interactive approval even for
    // reads (sensitive-file access) — so read-only native executions are
    // gated exactly like mutating ones.
    let (_dir, native, permission_calls, executed, native_count) = gate_setup(PermissionDecision {
        decision: "deny".into(),
        reason: Some("sensitive file".into()),
    });
    std::fs::write(
        _dir.path().join("a.txt"),
        "alpha
",
    )
    .unwrap();
    let response = native
        .execute_tool(ToolExecuteRequest {
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            turn_id: "t".into(),
            tool_call_id: "c3".into(),
            tool_name: "Read".into(),
            arguments: serde_json::json!({ "path": "a.txt" }),
        })
        .await
        .unwrap();
    assert_eq!(permission_calls.load(Ordering::Relaxed), 1);
    assert_eq!(executed.load(Ordering::Relaxed), 0);
    assert_eq!(native_count.load(Ordering::Relaxed), 0);
    assert!(response.is_error);
    assert!(response.content.contains("sensitive file"));
}

#[tokio::test]
async fn test_sandbox_escape_is_not_reported_as_native_execution() {
    // A call the engine does not own runs on the host instead, so it must
    // not inflate native_tool_calls — otherwise the turn result would
    // claim a path that never served the call.
    let (_dir, native, _permission_calls, executed, native_count) =
        gate_setup(PermissionDecision {
            decision: "allow".into(),
            reason: None,
        });
    let response = native
        .execute_tool(ToolExecuteRequest {
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            turn_id: "t".into(),
            tool_call_id: "c4".into(),
            tool_name: "SomeHostTool".into(),
            arguments: serde_json::json!({}),
        })
        .await
        .unwrap();
    assert_eq!(
        executed.load(Ordering::Relaxed),
        1,
        "the host picks up what the engine does not own"
    );
    assert_eq!(native_count.load(Ordering::Relaxed), 0);
    assert!(!response.is_error);
}

/// Base callbacks for stale-gate tests: scripted permission verdict,
/// recorded events, counted host executions, and a scripted plan domain
/// for the state bridge.
struct StaleGateHostCallbacks {
    decision: PermissionDecision,
    permission_calls: Arc<AtomicU32>,
    executed: Arc<AtomicU32>,
    events: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    plan: std::sync::Mutex<Option<serde_json::Value>>,
    state_reads: Arc<AtomicU32>,
}

impl HostCallbacks for StaleGateHostCallbacks {
    fn llm_chat(&self, _: LlmChatRequest) -> BoxFuture<'static, Result<LlmChatResponse, String>> {
        Box::pin(async { Err("not used".into()) })
    }

    fn execute_tool(
        &self,
        _: ToolExecuteRequest,
    ) -> BoxFuture<'static, Result<ToolExecuteResponse, String>> {
        self.executed.fetch_add(1, Ordering::Relaxed);
        Box::pin(async {
            Ok(ToolExecuteResponse {
                delivery: None,
                stop_turn: false,
                content: "host executed".into(),
                is_error: false,
                note: None,
            })
        })
    }

    fn check_permission(
        &self,
        _: PermissionCheckRequest,
    ) -> BoxFuture<'static, Result<PermissionDecision, String>> {
        self.permission_calls.fetch_add(1, Ordering::Relaxed);
        let decision = self.decision.clone();
        Box::pin(async move { Ok(decision) })
    }

    fn emit_event(&self, event: serde_json::Value) {
        self.events.lock().unwrap().push(event);
    }

    fn state_read(
        &self,
        _: StateReadRequest,
    ) -> BoxFuture<'static, Result<StateReadResponse, String>> {
        self.state_reads.fetch_add(1, Ordering::Relaxed);
        let value = self.plan.lock().unwrap().clone();
        Box::pin(async move {
            Ok(StateReadResponse {
                value: value.ok_or("no plan state")?,
            })
        })
    }
}

#[allow(clippy::type_complexity)]
fn stale_gate_setup(
    decision: PermissionDecision,
    plan: Option<serde_json::Value>,
) -> (
    tempfile::TempDir,
    NativeToolCallbacks,
    Arc<AtomicU32>,
    Arc<AtomicU32>,
    Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    Arc<AtomicU32>,
) {
    let dir = tempfile::tempdir().unwrap();
    let toolset = Arc::new(NativeToolset::new(dir.path().to_str().unwrap(), None).unwrap());
    let shell_bridge = toolset.shell_bridge();
    let permission_calls = Arc::new(AtomicU32::new(0));
    let executed = Arc::new(AtomicU32::new(0));
    let native_count = Arc::new(AtomicU32::new(0));
    let state_reads = Arc::new(AtomicU32::new(0));
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let native = NativeToolCallbacks {
        inner: Arc::new(StaleGateHostCallbacks {
            decision,
            permission_calls: permission_calls.clone(),
            executed: executed.clone(),
            events: events.clone(),
            plan: std::sync::Mutex::new(plan),
            state_reads: state_reads.clone(),
        }),
        toolset,
        native_count: native_count.clone(),
        truncator: None,
        permission_engine: None,
        plan_guard: None,
        stale_guard: Some(Arc::new(crate::tools::stale_guard::StaleGate::new(
            Some(dir.path().to_path_buf()),
            shell_bridge,
        ))),
        goal_guard: None,
        hook_guard: None,
        agent_tool_veto: None,
        tools_veto: None,
        todo_tool_veto: None,
        tower_worktree_root: None,
        tower_enabled: false,
        sandbox_policy: None,
    };
    (dir, native, executed, native_count, events, state_reads)
}

#[tokio::test]
async fn test_stale_guard_denies_unread_native_write() {
    let (dir, native, executed, native_count, events, _state_reads) = stale_gate_setup(
        PermissionDecision {
            decision: "allow".into(),
            reason: None,
        },
        Some(serde_json::json!({ "active": false })),
    );
    std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
    let response = native
        .execute_tool(ToolExecuteRequest {
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            turn_id: "t".into(),
            tool_call_id: "c1".into(),
            tool_name: "Write".into(),
            arguments: serde_json::json!({ "path": "a.txt", "content": "x" }),
        })
        .await
        .unwrap();
    assert!(response.is_error);
    assert!(response.content.contains("has not been read by this agent"));
    assert_eq!(
        executed.load(Ordering::Relaxed),
        0,
        "a stale denial must not fall back to the host"
    );
    assert_eq!(
        native_count.load(Ordering::Relaxed),
        0,
        "a stale denial executed nowhere"
    );
    let events = events.lock().unwrap();
    let native_event = events
        .iter()
        .find(|e| e["type"] == "tool.native")
        .expect("the refusal must be reported as tool.native");
    assert_eq!(native_event["is_error"], true);
    assert!(
        native_event["content"]
            .as_str()
            .unwrap()
            .contains("has not been read by this agent")
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "hello",
        "the denied write must not touch the file"
    );
}

#[tokio::test]
async fn test_stale_guard_allows_read_then_native_write() {
    let (dir, native, _executed, native_count, _events, _state_reads) = stale_gate_setup(
        PermissionDecision {
            decision: "allow".into(),
            reason: None,
        },
        Some(serde_json::json!({ "active": false })),
    );
    std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
    let read = native
        .execute_tool(ToolExecuteRequest {
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            turn_id: "t".into(),
            tool_call_id: "c1".into(),
            tool_name: "Read".into(),
            arguments: serde_json::json!({ "path": "a.txt" }),
        })
        .await
        .unwrap();
    assert!(!read.is_error);
    let write = native
        .execute_tool(ToolExecuteRequest {
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            turn_id: "t".into(),
            tool_call_id: "c2".into(),
            tool_name: "Write".into(),
            arguments: serde_json::json!({ "path": "a.txt", "content": "x" }),
        })
        .await
        .unwrap();
    assert!(!write.is_error, "read-then-write must pass: {write:?}");
    assert_eq!(native_count.load(Ordering::Relaxed), 2);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "x"
    );
}

#[tokio::test]
async fn test_stale_guard_ignores_a_read_that_failed() {
    // A read the engine could not serve is a tool error now, not a host
    // fallback — and a failed read must not count as having read the file,
    // so the guard still blocks the write that follows.
    let (dir, native, executed, _native_count, _events, _state_reads) = stale_gate_setup(
        PermissionDecision {
            decision: "allow".into(),
            reason: None,
        },
        Some(serde_json::json!({ "active": false })),
    );
    std::fs::write(dir.path().join("blob.bin"), b"plain prefix\x00\x01").unwrap();
    let read = native
        .execute_tool(ToolExecuteRequest {
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            turn_id: "t".into(),
            tool_call_id: "c1".into(),
            tool_name: "Read".into(),
            arguments: serde_json::json!({ "path": "blob.bin" }),
        })
        .await
        .unwrap();
    assert!(
        read.is_error,
        "a binary read is a tool error, not a host fallback: {read:?}"
    );
    let write = native
        .execute_tool(ToolExecuteRequest {
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            turn_id: "t".into(),
            tool_call_id: "c2".into(),
            tool_name: "Write".into(),
            arguments: serde_json::json!({ "path": "blob.bin", "content": "x" }),
        })
        .await
        .unwrap();
    assert!(
        write.is_error && write.content.contains("has not been read"),
        "a failed read must not clear the guard: {write:?}"
    );
    assert_eq!(executed.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn test_permission_deny_beats_stale_guard() {
    // v2 chain order: permission → plan → staleGuard. A permission deny
    // short-circuits before the stale gate is consulted at all.
    let (dir, native, _executed, _native_count, _events, state_reads) = stale_gate_setup(
        PermissionDecision {
            decision: "deny".into(),
            reason: Some("user declined".into()),
        },
        Some(serde_json::json!({ "active": false })),
    );
    std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
    let response = native
        .execute_tool(ToolExecuteRequest {
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            turn_id: "t".into(),
            tool_call_id: "c1".into(),
            tool_name: "Write".into(),
            arguments: serde_json::json!({ "path": "a.txt", "content": "x" }),
        })
        .await
        .unwrap();
    assert!(response.is_error);
    assert!(response.content.contains("user declined"));
    assert!(!response.content.contains("has not been read"));
    assert_eq!(
        state_reads.load(Ordering::Relaxed),
        0,
        "the stale gate must not be consulted after a permission deny"
    );
}

/// Base callbacks for goal-guard tests: scripted permission verdict, a
/// scripted current goal, counted host executions, recorded events.
struct GoalGateHostCallbacks {
    decision: PermissionDecision,
    permission_calls: Arc<AtomicU32>,
    executed: Arc<AtomicU32>,
    goal_reads: Arc<AtomicU32>,
    events: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    goal: std::sync::Mutex<Option<crate::turn_loop::types::GoalContext>>,
}

impl HostCallbacks for GoalGateHostCallbacks {
    fn llm_chat(&self, _: LlmChatRequest) -> BoxFuture<'static, Result<LlmChatResponse, String>> {
        Box::pin(async { Err("not used".into()) })
    }

    fn execute_tool(
        &self,
        _: ToolExecuteRequest,
    ) -> BoxFuture<'static, Result<ToolExecuteResponse, String>> {
        self.executed.fetch_add(1, Ordering::Relaxed);
        Box::pin(async {
            Ok(ToolExecuteResponse {
                delivery: None,
                stop_turn: false,
                content: "host executed".into(),
                is_error: false,
                note: None,
            })
        })
    }

    fn check_permission(
        &self,
        _: PermissionCheckRequest,
    ) -> BoxFuture<'static, Result<PermissionDecision, String>> {
        self.permission_calls.fetch_add(1, Ordering::Relaxed);
        let decision = self.decision.clone();
        Box::pin(async move { Ok(decision) })
    }

    fn emit_event(&self, event: serde_json::Value) {
        self.events.lock().unwrap().push(event);
    }

    fn goal(&self) -> BoxFuture<'static, Result<Option<GoalContext>, String>> {
        self.goal_reads.fetch_add(1, Ordering::Relaxed);
        let goal = self.goal.lock().unwrap().clone();
        Box::pin(async move { Ok(goal) })
    }
}

#[allow(clippy::type_complexity)]
fn goal_gate_setup(
    mode: crate::permission::PermissionMode,
    route_to_host: bool,
    current_goal: Option<crate::turn_loop::types::GoalContext>,
) -> (
    tempfile::TempDir,
    NativeToolCallbacks,
    Arc<AtomicU32>,
    Arc<AtomicU32>,
    Arc<AtomicU32>,
    Arc<AtomicU32>,
    Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    Arc<crate::tools::goal_guard::GoalGuard>,
) {
    let dir = tempfile::tempdir().unwrap();
    let permission_calls = Arc::new(AtomicU32::new(0));
    let executed = Arc::new(AtomicU32::new(0));
    let native_count = Arc::new(AtomicU32::new(0));
    let goal_reads = Arc::new(AtomicU32::new(0));
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let guard = Arc::new(crate::tools::goal_guard::GoalGuard::new(
        Some(mode),
        route_to_host,
    ));
    let fake: Arc<dyn HostCallbacks> = Arc::new(GoalGateHostCallbacks {
        decision: PermissionDecision {
            decision: "allow".into(),
            reason: None,
        },
        permission_calls: permission_calls.clone(),
        executed: executed.clone(),
        goal_reads: goal_reads.clone(),
        events: events.clone(),
        goal: std::sync::Mutex::new(current_goal),
    });
    let toolset = Arc::new(
        NativeToolset::new(dir.path().to_str().unwrap(), None)
            .unwrap()
            .with_callbacks(fake.clone()),
    );
    let native = NativeToolCallbacks {
        inner: fake,
        toolset,
        native_count: native_count.clone(),
        truncator: None,
        permission_engine: None,
        plan_guard: None,
        stale_guard: None,
        goal_guard: Some(guard.clone()),
        hook_guard: None,
        agent_tool_veto: None,
        tools_veto: None,
        todo_tool_veto: None,
        tower_worktree_root: None,
        tower_enabled: false,
        sandbox_policy: None,
    };
    (
        dir,
        native,
        executed,
        native_count,
        permission_calls,
        goal_reads,
        events,
        guard,
    )
}

fn gate_goal(id: &str) -> crate::turn_loop::types::GoalContext {
    crate::turn_loop::types::GoalContext {
        goal_id: id.into(),
        objective: String::new(),
        status: crate::turn_loop::types::GoalStatus::Active,
        token_budget: None,
        turn_budget: None,
        wall_clock_budget_ms: None,
        tokens_used: 0,
        turns_used: 0,
        wall_clock_ms: 0,
    }
}

#[tokio::test]
async fn test_goal_guard_routes_create_goal_to_host_outside_auto_mode() {
    let (_dir, native, executed, native_count, permission_calls, _goal_reads, _events, _guard) =
        goal_gate_setup(
            crate::permission::PermissionMode::Manual,
            true,
            Some(gate_goal("g1")),
        );
    let response = native
        .execute_tool(ToolExecuteRequest {
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            turn_id: "t".into(),
            tool_call_id: "c1".into(),
            tool_name: "CreateGoal".into(),
            arguments: serde_json::json!({ "objective": "do it" }),
        })
        .await
        .unwrap();
    assert!(
        !response.is_error,
        "the host runs the reviewed call: {response:?}"
    );
    assert_eq!(
        executed.load(Ordering::Relaxed),
        1,
        "non-auto CreateGoal must run on the host"
    );
    assert_eq!(
        native_count.load(Ordering::Relaxed),
        0,
        "routed calls are not native executions"
    );
    assert_eq!(
        permission_calls.load(Ordering::Relaxed),
        0,
        "routing happens before the engine's permission step"
    );
}

#[tokio::test]
async fn test_goal_guard_keeps_create_goal_native_in_auto_mode() {
    let (_dir, native, executed, native_count, permission_calls, _goal_reads, _events, _guard) =
        goal_gate_setup(crate::permission::PermissionMode::Auto, true, None);
    let _response = native
        .execute_tool(ToolExecuteRequest {
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            turn_id: "t".into(),
            tool_call_id: "c1".into(),
            tool_name: "CreateGoal".into(),
            arguments: serde_json::json!({ "objective": "do it" }),
        })
        .await
        .unwrap();
    // The native CreateGoal runs against the test fake whose state
    // bridge is unwired — the result is a bridge error, but the point
    // is that it executed natively, not on the host.
    assert_eq!(native_count.load(Ordering::Relaxed), 1);
    assert_eq!(executed.load(Ordering::Relaxed), 0);
    assert_eq!(
        permission_calls.load(Ordering::Relaxed),
        1,
        "auto mode goes through the native permission gate"
    );
}

#[tokio::test]
async fn test_goal_guard_vetoes_stale_goal_mutation() {
    let (_dir, native, executed, _native_count, _permission_calls, goal_reads, events, guard) =
        goal_gate_setup(
            crate::permission::PermissionMode::Auto,
            true,
            Some(gate_goal("g2")),
        );
    guard.bind_turn("t", Some("g1"));

    let response = native
        .execute_tool(ToolExecuteRequest {
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            turn_id: "t".into(),
            tool_call_id: "c1".into(),
            tool_name: "UpdateGoal".into(),
            arguments: serde_json::json!({ "status": "complete" }),
        })
        .await
        .unwrap();
    assert!(response.is_error);
    assert_eq!(
        response.content,
        "Goal changed since this turn started; ignored stale goal tool call."
    );
    assert_eq!(executed.load(Ordering::Relaxed), 0);
    assert_eq!(goal_reads.load(Ordering::Relaxed), 1);
    let native_event = events
        .lock()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "tool.native")
        .expect("the refusal is reported as tool.native")
        .clone();
    assert_eq!(native_event["is_error"], true);
}

#[tokio::test]
async fn test_goal_guard_exempts_read_only_goal_tools() {
    let (_dir, native, _executed, _native_count, _permission_calls, goal_reads, _events, guard) =
        goal_gate_setup(
            crate::permission::PermissionMode::Auto,
            true,
            Some(gate_goal("g2")),
        );
    guard.bind_turn("t", Some("g1"));

    // GetGoal is read-only and never stale-checked (v2 `isGoalMutationTool`
    // excludes it): it runs (natively or via the fallback) without a veto.
    let _response = native
        .execute_tool(ToolExecuteRequest {
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            turn_id: "t".into(),
            tool_call_id: "c1".into(),
            tool_name: "GetGoal".into(),
            arguments: serde_json::json!({}),
        })
        .await
        .unwrap();
    assert_eq!(
        goal_reads.load(Ordering::Relaxed),
        0,
        "the stale check must not fire for GetGoal"
    );
}

/// PreToolUse hook gate tests: a scripted hook list runs before native
/// execution through the same fake host as the goal-gate setup.
#[allow(clippy::type_complexity)]
fn hook_gate_setup(
    hooks: Vec<crate::permission::HookDef>,
) -> (
    tempfile::TempDir,
    NativeToolCallbacks,
    Arc<AtomicU32>,
    Arc<AtomicU32>,
    Arc<AtomicU32>,
    Arc<AtomicU32>,
    Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
) {
    let dir = tempfile::tempdir().unwrap();
    let permission_calls = Arc::new(AtomicU32::new(0));
    let executed = Arc::new(AtomicU32::new(0));
    let native_count = Arc::new(AtomicU32::new(0));
    let goal_reads = Arc::new(AtomicU32::new(0));
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let fake: Arc<dyn HostCallbacks> = Arc::new(GoalGateHostCallbacks {
        decision: PermissionDecision {
            decision: "allow".into(),
            reason: None,
        },
        permission_calls: permission_calls.clone(),
        executed: executed.clone(),
        goal_reads: goal_reads.clone(),
        events: events.clone(),
        goal: std::sync::Mutex::new(None),
    });
    let toolset = Arc::new(
        NativeToolset::new(dir.path().to_str().unwrap(), None)
            .unwrap()
            .with_callbacks(fake.clone()),
    );
    let native = NativeToolCallbacks {
        inner: fake,
        toolset,
        native_count: native_count.clone(),
        truncator: None,
        permission_engine: None,
        plan_guard: None,
        stale_guard: None,
        goal_guard: None,
        hook_guard: Some(Arc::new(crate::tools::external_hooks::HookGuard::new(
            hooks,
        ))),
        agent_tool_veto: None,
        tools_veto: None,
        todo_tool_veto: None,
        tower_worktree_root: None,
        tower_enabled: false,
        sandbox_policy: None,
    };
    (
        dir,
        native,
        executed,
        native_count,
        permission_calls,
        goal_reads,
        events,
    )
}

fn hook_exit_two_with_stderr() -> crate::permission::HookDef {
    crate::permission::HookDef {
        event: "PreToolUse".into(),
        matcher: String::new(),
        command: if cfg!(windows) {
            "echo denied by gate hook 1>&2 & exit /b 2".into()
        } else {
            "echo denied by gate hook >&2; exit 2".into()
        },
        timeout: None,
        cwd: None,
        env: None,
    }
}

#[tokio::test]
async fn test_hook_guard_denies_native_call() {
    let (_dir, native, executed, _native_count, _permission_calls, _goal_reads, events) =
        hook_gate_setup(vec![hook_exit_two_with_stderr()]);
    let response = native
        .execute_tool(ToolExecuteRequest {
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            turn_id: "t".into(),
            tool_call_id: "c1".into(),
            tool_name: "Write".into(),
            arguments: serde_json::json!({ "path": "a.txt", "content": "x" }),
        })
        .await
        .unwrap();
    assert!(response.is_error);
    assert_eq!(response.content, "denied by gate hook");
    assert_eq!(
        executed.load(Ordering::Relaxed),
        0,
        "a hook block must not fall back to the host"
    );
    let native_event = events
        .lock()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "tool.native")
        .expect("the refusal is reported as tool.native")
        .clone();
    assert_eq!(native_event["is_error"], true);
    assert_eq!(native_event["content"], "denied by gate hook");
}

#[tokio::test]
async fn test_hook_guard_allows_when_hook_passes() {
    let (_dir, native, executed, native_count, _permission_calls, _goal_reads, _events) =
        hook_gate_setup(vec![crate::permission::HookDef {
            event: "PreToolUse".into(),
            matcher: String::new(),
            command: if cfg!(windows) {
                "exit /b 0".into()
            } else {
                "exit 0".into()
            },
            timeout: None,
            cwd: None,
            env: None,
        }]);
    let response = native
        .execute_tool(ToolExecuteRequest {
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            turn_id: "t".into(),
            tool_call_id: "c1".into(),
            tool_name: "Write".into(),
            arguments: serde_json::json!({ "path": "made.txt", "content": "x" }),
        })
        .await
        .unwrap();
    assert!(
        !response.is_error,
        "an allowing hook must not block: {response:?}"
    );
    assert_eq!(native_count.load(Ordering::Relaxed), 1);
    assert_eq!(executed.load(Ordering::Relaxed), 0);
}

/// The permission hook wiring on the host-owned edge (no local engine):
/// `PermissionRequest` fires before `check_permission` is asked, and
/// `PermissionResult` carries the resolved verdict — remove either dispatch
/// site and its capture file stays empty.
#[tokio::test]
async fn permission_hooks_fire_on_the_host_owned_edge() {
    use crate::tools::external_hooks::capture;
    let Some(dir) = capture::dir() else {
        return;
    };
    let request = dir.path().join("permission-request.json");
    let result = dir.path().join("permission-result.json");
    let (_tool_dir, native, _executed, _native_count, permission_calls, _goal_reads, _events) =
        hook_gate_setup(vec![
            capture::hook("PermissionRequest", "Write", &request),
            capture::hook("PermissionResult", "Write", &result),
        ]);
    let response = native
        .execute_tool(ToolExecuteRequest {
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            turn_id: "t".into(),
            tool_call_id: "c1".into(),
            tool_name: "Write".into(),
            arguments: serde_json::json!({ "path": "a.txt", "content": "x" }),
        })
        .await
        .unwrap();
    assert!(!response.is_error);
    assert_eq!(permission_calls.load(Ordering::Relaxed), 1);

    let request_payload = capture::wait(&request).await;
    assert_eq!(request_payload["hook_event_name"], "PermissionRequest");
    assert_eq!(request_payload["tool_name"], "Write");
    assert_eq!(request_payload["tool_call_id"], "c1");
    assert_eq!(request_payload["turn_id"], "t");
    // The host-owned edge carries no engine-side explanation.
    assert!(request_payload.get("reason").is_none());

    let result_payload = capture::wait(&result).await;
    assert_eq!(result_payload["hook_event_name"], "PermissionResult");
    assert_eq!(result_payload["tool_call_id"], "c1");
    assert_eq!(result_payload["decision"], "approved");
}
/// A stub that answers questions, recording the request it received.
struct AskQuestionCallbacks {
    received: Arc<std::sync::Mutex<Option<AskQuestionRequest>>>,
}

impl HostCallbacks for AskQuestionCallbacks {
    fn llm_chat(&self, _: LlmChatRequest) -> BoxFuture<'static, Result<LlmChatResponse, String>> {
        Box::pin(async { Err("not used".into()) })
    }

    fn execute_tool(
        &self,
        _: ToolExecuteRequest,
    ) -> BoxFuture<'static, Result<ToolExecuteResponse, String>> {
        Box::pin(async { Err("not used".into()) })
    }

    fn check_permission(
        &self,
        _: PermissionCheckRequest,
    ) -> BoxFuture<'static, Result<PermissionDecision, String>> {
        Box::pin(async { Ok(PermissionDecision::allow()) })
    }

    fn ask_question(
        &self,
        request: AskQuestionRequest,
    ) -> BoxFuture<'static, Result<AskQuestionResponse, String>> {
        *self.received.lock().unwrap() = Some(request);
        Box::pin(async {
            Ok(AskQuestionResponse {
                answers: std::collections::HashMap::new(),
                method: Some("enter".into()),
                note: None,
                cancelled: None,
                reason: None,
            })
        })
    }
}

fn sample_ask_question_request() -> AskQuestionRequest {
    AskQuestionRequest {
        question_id: "question_1".into(),
        turn_id: "turn-1".into(),
        tool_call_id: "call_1".into(),
        background: false,
        timeout_ms: None,
        questions: vec![AskQuestionItem {
            question: "Pick one".into(),
            header: None,
            options: vec![AskQuestionOption {
                label: "Option A".into(),
                description: None,
            }],
            multi_select: false,
        }],
    }
}

/// A host that never wired the interactive-question seam must get the
/// trait default: an explicit "not supported" error, so the tool result
/// tells the model not to call the tool again.
#[tokio::test]
async fn test_ask_question_default_impl_errors() {
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let callbacks = RecordingCallbacks { events };
    let err = callbacks
        .ask_question(sample_ask_question_request())
        .await
        .unwrap_err();
    assert!(err.contains("does not support interactive questions"));
}

#[tokio::test]
async fn test_counting_callbacks_forwards_ask_question() {
    let received = Arc::new(std::sync::Mutex::new(None));
    let counting = CountingCallbacks::new(
        Arc::new(AskQuestionCallbacks {
            received: received.clone(),
        }),
        Arc::new(AtomicU32::new(0)),
    );
    let response = counting
        .ask_question(sample_ask_question_request())
        .await
        .unwrap();
    assert_eq!(response.method.as_deref(), Some("enter"));
    assert!(response.answers.is_empty());
    let received = received.lock().unwrap();
    assert_eq!(received.as_ref().unwrap().question_id, "question_1");
    assert_eq!(received.as_ref().unwrap().questions.len(), 1);
}

#[tokio::test]
async fn test_native_tool_callbacks_forwards_ask_question() {
    let dir = tempfile::tempdir().unwrap();
    let toolset = Arc::new(NativeToolset::new(dir.path().to_str().unwrap(), None).unwrap());
    let received = Arc::new(std::sync::Mutex::new(None));
    let native = NativeToolCallbacks {
        inner: Arc::new(AskQuestionCallbacks {
            received: received.clone(),
        }),
        toolset,
        native_count: Arc::new(AtomicU32::new(0)),
        truncator: None,
        permission_engine: None,
        plan_guard: None,
        stale_guard: None,
        goal_guard: None,
        hook_guard: None,
        agent_tool_veto: None,
        tools_veto: None,
        todo_tool_veto: None,
        tower_worktree_root: None,
        tower_enabled: false,
        sandbox_policy: None,
    };
    let mut request = sample_ask_question_request();
    request.question_id = "question_2".into();
    request.background = true;
    request.timeout_ms = Some(30_000);
    let response = native.ask_question(request).await.unwrap();
    assert_eq!(response.method.as_deref(), Some("enter"));
    let received = received.lock().unwrap();
    let req = received.as_ref().unwrap();
    assert_eq!(req.question_id, "question_2");
    assert!(req.background);
    assert_eq!(req.timeout_ms, Some(30_000));
}

/// A stub that answers state bridge calls, recording the requests it
/// received.
struct StateBridgeCallbacks {
    read_received: Arc<std::sync::Mutex<Option<StateReadRequest>>>,
    write_received: Arc<std::sync::Mutex<Option<StateWriteRequest>>>,
}

impl HostCallbacks for StateBridgeCallbacks {
    fn llm_chat(&self, _: LlmChatRequest) -> BoxFuture<'static, Result<LlmChatResponse, String>> {
        Box::pin(async { Err("not used".into()) })
    }

    fn execute_tool(
        &self,
        _: ToolExecuteRequest,
    ) -> BoxFuture<'static, Result<ToolExecuteResponse, String>> {
        Box::pin(async { Err("not used".into()) })
    }

    fn check_permission(
        &self,
        _: PermissionCheckRequest,
    ) -> BoxFuture<'static, Result<PermissionDecision, String>> {
        Box::pin(async { Ok(PermissionDecision::allow()) })
    }

    fn state_read(
        &self,
        request: StateReadRequest,
    ) -> BoxFuture<'static, Result<StateReadResponse, String>> {
        *self.read_received.lock().unwrap() = Some(request);
        Box::pin(async {
            Ok(StateReadResponse {
                value: serde_json::json!([
                    {"id": "T1", "title": "Read session-control.ts", "status": "in_progress"}
                ]),
            })
        })
    }

    fn state_write(
        &self,
        request: StateWriteRequest,
    ) -> BoxFuture<'static, Result<StateWriteResponse, String>> {
        *self.write_received.lock().unwrap() = Some(request);
        Box::pin(async {
            Ok(StateWriteResponse {
                ok: true,
                value: serde_json::json!({"active": true, "id": "plan-7f3a"}),
            })
        })
    }
}

fn sample_state_read_request() -> StateReadRequest {
    StateReadRequest {
        domain: "todo".into(),
        key: "todo".into(),
        turn_id: "turn-1".into(),
        tool_call_id: "call_1".into(),
    }
}

fn sample_state_write_request() -> StateWriteRequest {
    StateWriteRequest {
        domain: "plan".into(),
        key: "plan".into(),
        value: serde_json::json!({"active": true}),
        undoable: true,
        turn_id: "turn-1".into(),
        tool_call_id: "call_1".into(),
    }
}

/// A host that never wired the state bridge seam must get the trait
/// default: an explicit "not supported" error, so the tool result tells
/// the model not to call the tool again.
#[tokio::test]
async fn test_state_read_default_impl_errors() {
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let callbacks = RecordingCallbacks { events };
    let err = callbacks
        .state_read(sample_state_read_request())
        .await
        .unwrap_err();
    assert!(err.contains("does not support state bridge"));
}

#[tokio::test]
async fn test_state_write_default_impl_errors() {
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let callbacks = RecordingCallbacks { events };
    let err = callbacks
        .state_write(sample_state_write_request())
        .await
        .unwrap_err();
    assert!(err.contains("does not support state bridge"));
}

/// A state write that cannot be persisted must fail the call. It used to
/// report `ok: true` after dropping the persist error, so the host believed
/// the state was durable when it was not.
#[tokio::test]
async fn test_state_write_reports_a_failed_persist() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(crate::storage::StateStore::for_workspace(dir.path()).unwrap());
    // A directory where the domain file belongs makes the atomic rename
    // fail, which is the persist step `apply_write` does not cover.
    let domain_path = crate::storage::engine_state_dir(dir.path())
        .unwrap()
        .join("state")
        .join("todo.json");
    std::fs::create_dir_all(&domain_path).unwrap();

    let callbacks = StateStoreCallbacks {
        inner: Arc::new(RecordingCallbacks {
            events: Arc::new(std::sync::Mutex::new(Vec::new())),
        }),
        store,
    };
    let request = StateWriteRequest {
        domain: "todo".into(),
        key: "todo".into(),
        value: serde_json::json!([]),
        undoable: true,
        turn_id: "turn-1".into(),
        tool_call_id: "call_1".into(),
    };
    let err = callbacks.state_write(request).await.unwrap_err();
    assert!(
        err.contains("rename"),
        "the persist failure must surface: {err}"
    );
}

#[tokio::test]
async fn test_counting_callbacks_forwards_state_bridge() {
    let read_received = Arc::new(std::sync::Mutex::new(None));
    let write_received = Arc::new(std::sync::Mutex::new(None));
    let counting = CountingCallbacks::new(
        Arc::new(StateBridgeCallbacks {
            read_received: read_received.clone(),
            write_received: write_received.clone(),
        }),
        Arc::new(AtomicU32::new(0)),
    );
    let read = counting
        .state_read(sample_state_read_request())
        .await
        .unwrap();
    assert_eq!(read.value[0]["id"], "T1");
    let write = counting
        .state_write(sample_state_write_request())
        .await
        .unwrap();
    assert!(write.ok);
    assert_eq!(write.value["id"], "plan-7f3a");
    let read_req = read_received.lock().unwrap();
    assert_eq!(read_req.as_ref().unwrap().domain, "todo");
    assert_eq!(read_req.as_ref().unwrap().turn_id, "turn-1");
    let write_req = write_received.lock().unwrap();
    assert_eq!(write_req.as_ref().unwrap().domain, "plan");
    assert!(write_req.as_ref().unwrap().undoable);
}

#[tokio::test]
async fn test_native_tool_callbacks_forwards_state_bridge() {
    let dir = tempfile::tempdir().unwrap();
    let toolset = Arc::new(NativeToolset::new(dir.path().to_str().unwrap(), None).unwrap());
    let read_received = Arc::new(std::sync::Mutex::new(None));
    let write_received = Arc::new(std::sync::Mutex::new(None));
    let native = NativeToolCallbacks {
        inner: Arc::new(StateBridgeCallbacks {
            read_received: read_received.clone(),
            write_received: write_received.clone(),
        }),
        toolset,
        native_count: Arc::new(AtomicU32::new(0)),
        truncator: None,
        permission_engine: None,
        plan_guard: None,
        stale_guard: None,
        goal_guard: None,
        hook_guard: None,
        agent_tool_veto: None,
        tools_veto: None,
        todo_tool_veto: None,
        tower_worktree_root: None,
        tower_enabled: false,
        sandbox_policy: None,
    };
    let mut read_request = sample_state_read_request();
    read_request.turn_id = "turn-2".into();
    let read = native.state_read(read_request).await.unwrap();
    assert_eq!(read.value[0]["status"], "in_progress");
    let mut write_request = sample_state_write_request();
    write_request.undoable = false;
    let write = native.state_write(write_request).await.unwrap();
    assert!(write.ok);
    let read_req = read_received.lock().unwrap();
    assert_eq!(read_req.as_ref().unwrap().turn_id, "turn-2");
    let write_req = write_received.lock().unwrap();
    assert!(!write_req.as_ref().unwrap().undoable);
}

#[tokio::test]
async fn test_state_store_callbacks_local_bridge() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(crate::storage::StateStore::for_dir(tmp.path().join("state")).unwrap());
    let dummy = Arc::new(RecordingCallbacks {
        events: Arc::new(std::sync::Mutex::new(Vec::new())),
    });
    let adapter = StateStoreCallbacks {
        inner: dummy.clone(),
        store: store.clone(),
    };

    // 1. Initial read of todo domain returns default empty array
    let read_req = StateReadRequest {
        domain: "todo".into(),
        key: "todo".into(),
        turn_id: "turn-1".into(),
        tool_call_id: "call-1".into(),
    };
    let read_res = adapter.state_read(read_req).await.unwrap();
    assert_eq!(read_res.value, serde_json::json!([]));

    // 2. Write todo item via StateStoreCallbacks
    let write_req = StateWriteRequest {
        domain: "todo".into(),
        key: "todo".into(),
        value: serde_json::json!([
            { "id": "1", "title": "Refactor host bridge", "status": "completed" }
        ]),
        undoable: true,
        turn_id: "turn-1".into(),
        tool_call_id: "call-1".into(),
    };
    let write_res = adapter.state_write(write_req).await.unwrap();
    assert!(write_res.ok);
    assert_eq!(write_res.value[0]["title"], "Refactor host bridge");

    // 3. Read back verified from store
    let read_req2 = StateReadRequest {
        domain: "todo".into(),
        key: "todo".into(),
        turn_id: "turn-2".into(),
        tool_call_id: "call-2".into(),
    };
    let read_res2 = adapter.state_read(read_req2).await.unwrap();
    assert_eq!(read_res2.value[0]["title"], "Refactor host bridge");

    // 4. Checkpoint works locally
    adapter
        .checkpoint(CheckpointRequest {
            turn_id: "turn-2".into(),
            tool_call_id: "call-2".into(),
            phase: "prepare".into(),
            paths: vec!["test.txt".into()],
            executed: false,
        })
        .await
        .unwrap();
    assert_eq!(store.checkpoint_depth(), 1);
}

/// A domain the local store does not own (v2's host-owned `skill`) must
/// surface the *host's* error, not the store's `-32001 unknown state
/// domain`: the latter is a fact about this store, not about the request,
/// and it defeats the Skill tool's own error mapping (which matches on the
/// host's "does not support state bridge" phrase to tell the model to stop
/// calling the tool). An engine-owned domain keeps the store's verdict.
#[tokio::test]
async fn test_unowned_domain_surfaces_the_host_error() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(crate::storage::StateStore::for_dir(tmp.path().join("state")).unwrap());
    // RecordingCallbacks does not override `state_read`, so it answers with
    // the trait default: "host does not support state bridge".
    let adapter = StateStoreCallbacks {
        inner: Arc::new(RecordingCallbacks {
            events: Arc::new(std::sync::Mutex::new(Vec::new())),
        }),
        store: store.clone(),
    };

    let err = adapter
        .state_read(StateReadRequest {
            domain: "skill".into(),
            key: "commit".into(),
            turn_id: "turn-1".into(),
            tool_call_id: "call-1".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(err, "host does not support state bridge");

    // Engine-owned domain: the store stays the authority, so its own
    // verdict survives (task output lookup misses are `-32002`).
    let err = adapter
        .state_read(StateReadRequest {
            domain: "task".into(),
            key: "no-such-task".into(),
            turn_id: "turn-1".into(),
            tool_call_id: "call-1".into(),
        })
        .await
        .unwrap_err();
    assert!(err.contains("-32002"), "{err}");
}

/// A host whose checkpoint reports a real failure (not the unsupported
/// sentinel): the StateStore bridge must surface it, because the host's
/// undo anchor is then incomplete.
struct CheckpointFailingCallbacks;

impl HostCallbacks for CheckpointFailingCallbacks {
    fn llm_chat(&self, _: LlmChatRequest) -> BoxFuture<'static, Result<LlmChatResponse, String>> {
        Box::pin(async { Err("not used".into()) })
    }
    fn execute_tool(
        &self,
        _: ToolExecuteRequest,
    ) -> BoxFuture<'static, Result<ToolExecuteResponse, String>> {
        Box::pin(async { Err("not used".into()) })
    }
    fn check_permission(
        &self,
        _: PermissionCheckRequest,
    ) -> BoxFuture<'static, Result<PermissionDecision, String>> {
        Box::pin(async {
            Ok(PermissionDecision {
                decision: "allow".into(),
                reason: None,
            })
        })
    }
    fn checkpoint(&self, _: CheckpointRequest) -> BoxFuture<'static, Result<(), String>> {
        Box::pin(async { Err("pre-image capture failed: disk full".into()) })
    }
}

#[tokio::test]
async fn test_state_store_checkpoint_surfaces_real_host_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(crate::storage::StateStore::for_dir(tmp.path().join("state")).unwrap());
    let adapter = StateStoreCallbacks {
        inner: Arc::new(CheckpointFailingCallbacks),
        store: store.clone(),
    };
    let err = adapter
        .checkpoint(CheckpointRequest {
            turn_id: "turn-1".into(),
            tool_call_id: "call-1".into(),
            phase: "prepare".into(),
            paths: vec!["test.txt".into()],
            executed: false,
        })
        .await
        .expect_err("a real host checkpoint failure must propagate");
    assert!(
        err.contains("pre-image capture failed"),
        "unexpected error: {err}"
    );
}

/// A host that answers plan reads: the bridge must prefer it over a stale
/// local `plan.json` so the guard sees the live Plan-mode toggle.
struct PlanHostCallbacks {
    plan: serde_json::Value,
}

impl HostCallbacks for PlanHostCallbacks {
    fn llm_chat(&self, _: LlmChatRequest) -> BoxFuture<'static, Result<LlmChatResponse, String>> {
        Box::pin(async { Err("not used".into()) })
    }
    fn execute_tool(
        &self,
        _: ToolExecuteRequest,
    ) -> BoxFuture<'static, Result<ToolExecuteResponse, String>> {
        Box::pin(async { Err("not used".into()) })
    }
    fn check_permission(
        &self,
        _: PermissionCheckRequest,
    ) -> BoxFuture<'static, Result<PermissionDecision, String>> {
        Box::pin(async {
            Ok(PermissionDecision {
                decision: "allow".into(),
                reason: None,
            })
        })
    }
    fn state_read(
        &self,
        request: StateReadRequest,
    ) -> BoxFuture<'static, Result<StateReadResponse, String>> {
        let plan = self.plan.clone();
        Box::pin(async move {
            if request.domain == "plan" {
                Ok(StateReadResponse { value: plan })
            } else {
                Err("host does not support state bridge".into())
            }
        })
    }
}

#[tokio::test]
async fn test_state_read_plan_prefers_host_over_stale_local() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(crate::storage::StateStore::for_dir(tmp.path().join("state")).unwrap());
    // A stale local plan.json says Plan mode is OFF.
    store
        .write_domain("plan", &serde_json::json!({ "active": false }))
        .unwrap();
    let adapter = StateStoreCallbacks {
        inner: Arc::new(PlanHostCallbacks {
            plan: serde_json::json!({ "active": true }),
        }),
        store: store.clone(),
    };
    let resp = adapter
        .state_read(StateReadRequest {
            domain: "plan".into(),
            key: "plan".into(),
            turn_id: "t".into(),
            tool_call_id: "c".into(),
        })
        .await
        .unwrap();
    assert_eq!(
        resp.value["active"],
        serde_json::json!(true),
        "the host's live Plan mode must win over the stale local file"
    );
}

#[tokio::test]
async fn test_state_read_plan_falls_back_to_local_when_host_silent() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(crate::storage::StateStore::for_dir(tmp.path().join("state")).unwrap());
    store
        .write_domain("plan", &serde_json::json!({ "active": true }))
        .unwrap();
    // RecordingCallbacks uses the trait-default state_read (an error), so
    // the bridge must fall back to the local plan.json.
    let adapter = StateStoreCallbacks {
        inner: Arc::new(RecordingCallbacks {
            events: Arc::new(std::sync::Mutex::new(Vec::new())),
        }),
        store: store.clone(),
    };
    let resp = adapter
        .state_read(StateReadRequest {
            domain: "plan".into(),
            key: "plan".into(),
            turn_id: "t".into(),
            tool_call_id: "c".into(),
        })
        .await
        .unwrap();
    assert_eq!(
        resp.value["active"],
        serde_json::json!(true),
        "with no host plan state, the local store is the fallback"
    );
}

struct HostWithToolsCallbacks {
    tools: Vec<crate::turn_loop::types::ToolInfo>,
}

/// A host with no tool table at all: `list_tools` falls back to the
/// trait's error, like the napi harness whose callbacks do not answer
/// `host/list_tools`.
struct HostWithoutToolTable;

impl HostCallbacks for HostWithoutToolTable {
    fn llm_chat(&self, _: LlmChatRequest) -> BoxFuture<'static, Result<LlmChatResponse, String>> {
        Box::pin(async { Err("not used".into()) })
    }

    fn execute_tool(
        &self,
        _: ToolExecuteRequest,
    ) -> BoxFuture<'static, Result<ToolExecuteResponse, String>> {
        Box::pin(async { Err("not used".into()) })
    }

    fn check_permission(
        &self,
        _: PermissionCheckRequest,
    ) -> BoxFuture<'static, Result<PermissionDecision, String>> {
        Box::pin(async { Ok(PermissionDecision::allow()) })
    }
}

impl HostCallbacks for HostWithToolsCallbacks {
    fn llm_chat(&self, _: LlmChatRequest) -> BoxFuture<'static, Result<LlmChatResponse, String>> {
        Box::pin(async { Err("not used".into()) })
    }

    fn execute_tool(
        &self,
        _: ToolExecuteRequest,
    ) -> BoxFuture<'static, Result<ToolExecuteResponse, String>> {
        Box::pin(async { Err("not used".into()) })
    }

    fn check_permission(
        &self,
        _: PermissionCheckRequest,
    ) -> BoxFuture<'static, Result<PermissionDecision, String>> {
        Box::pin(async { Ok(PermissionDecision::allow()) })
    }

    fn list_tools(&self) -> BoxFuture<'static, Result<ListToolsResponse, String>> {
        let tools = self.tools.clone();
        Box::pin(async move { Ok(ListToolsResponse { tools }) })
    }
}

#[tokio::test]
async fn test_native_tool_callbacks_list_tools_merges_mcp_tools() {
    let tmp = tempfile::tempdir().unwrap();
    let mcp_mgr = Arc::new(crate::mcp::McpManager::new());
    let mock_client = crate::mcp::McpClient::mock("test_server");
    mcp_mgr.add_client(mock_client).await;

    let toolset = Arc::new(
        NativeToolset::new(tmp.path().to_str().unwrap(), None)
            .unwrap()
            .with_mcp(mcp_mgr),
    );

    let inner = Arc::new(HostWithToolsCallbacks {
        tools: vec![crate::turn_loop::types::ToolInfo {
            name: "host_custom_tool".into(),
            description: "host tool description".into(),
            input_schema: serde_json::json!({}),
        }],
    });

    let callbacks = NativeToolCallbacks {
        inner,
        toolset,
        native_count: Arc::new(AtomicU32::new(0)),
        truncator: None,
        permission_engine: None,
        plan_guard: None,
        stale_guard: None,
        goal_guard: None,
        hook_guard: None,
        agent_tool_veto: None,
        tools_veto: None,
        todo_tool_veto: None,
        tower_worktree_root: None,
        tower_enabled: false,
        sandbox_policy: None,
    };

    let res = callbacks.list_tools().await.unwrap();
    assert!(res.tools.iter().any(|t| t.name == "host_custom_tool"));
    assert!(
        res.tools
            .iter()
            .any(|t| t.name.starts_with("mcp__test_server__"))
    );
}

/// The napi harness has no host tool table (its `host/list_tools` errors):
/// the engine-owned definitions must stand alone instead of leaving the
/// model with nothing to call.
#[tokio::test]
async fn test_native_tool_callbacks_list_tools_survives_a_missing_host_table() {
    let tmp = tempfile::tempdir().unwrap();
    let toolset = Arc::new(NativeToolset::new(tmp.path().to_str().unwrap(), None).unwrap());
    let callbacks = NativeToolCallbacks {
        inner: Arc::new(HostWithoutToolTable),
        toolset,
        native_count: Arc::new(AtomicU32::new(0)),
        truncator: None,
        permission_engine: None,
        plan_guard: None,
        stale_guard: None,
        goal_guard: None,
        hook_guard: None,
        agent_tool_veto: None,
        tools_veto: None,
        todo_tool_veto: None,
        tower_worktree_root: None,
        tower_enabled: false,
        sandbox_policy: None,
    };

    let res = callbacks.list_tools().await.unwrap();
    assert!(res.tools.iter().any(|t| t.name == "Read"));
    assert!(res.tools.iter().any(|t| t.name == "Agent"));
    assert!(res.tools.iter().any(|t| t.name == "AskUserQuestion"));
    assert!(
        !res.tools.iter().any(|t| t.name == "TowerInit"),
        "tower tools stay gated"
    );
}

/// The global `[tools]` switch filters the merged table (host and MCP
/// definitions included), not just the engine's own definitions.
#[tokio::test]
async fn test_native_tool_callbacks_list_tools_applies_the_tools_filter() {
    let tmp = tempfile::tempdir().unwrap();
    let mcp_mgr = Arc::new(crate::mcp::McpManager::new());
    mcp_mgr
        .add_client(crate::mcp::McpClient::mock("test_server"))
        .await;
    let toolset = Arc::new(
        NativeToolset::new(tmp.path().to_str().unwrap(), None)
            .unwrap()
            .with_mcp(mcp_mgr)
            .with_tools_filter(Some(crate::tools::tool_policy::ToolsFilter {
                enabled: vec!["Read".into(), "host_custom_tool".into()],
                disabled: vec!["mcp__test_server__*".into()],
            })),
    );

    let callbacks = NativeToolCallbacks {
        inner: Arc::new(HostWithToolsCallbacks {
            tools: vec![
                crate::turn_loop::types::ToolInfo {
                    name: "host_custom_tool".into(),
                    description: "host tool".into(),
                    input_schema: serde_json::json!({}),
                },
                crate::turn_loop::types::ToolInfo {
                    name: "host_other_tool".into(),
                    description: "host tool".into(),
                    input_schema: serde_json::json!({}),
                },
            ],
        }),
        toolset,
        native_count: Arc::new(AtomicU32::new(0)),
        truncator: None,
        permission_engine: None,
        plan_guard: None,
        stale_guard: None,
        goal_guard: None,
        hook_guard: None,
        agent_tool_veto: None,
        tools_veto: None,
        todo_tool_veto: None,
        tower_worktree_root: None,
        tower_enabled: false,
        sandbox_policy: None,
    };

    let res = callbacks.list_tools().await.unwrap();
    let names: Vec<&str> = res.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, vec!["Read", "host_custom_tool"]);
}

// ── Local permission reasons reaching the host ───────────────────────
//
// The local permission engine renders *why* it is asking, and that
// explanation has to survive the hop to the host — otherwise the approval
// prompt can only name the tool. These cover the hand-off, which no other
// test exercises (the existing `ScriptedPermissionCallbacks` discards the
// request).

/// Records every `PermissionCheckRequest` the wrapper forwards.
struct CapturingPermissionCallbacks {
    seen: Arc<std::sync::Mutex<Vec<PermissionCheckRequest>>>,
}

impl HostCallbacks for CapturingPermissionCallbacks {
    fn llm_chat(&self, _: LlmChatRequest) -> BoxFuture<'static, Result<LlmChatResponse, String>> {
        Box::pin(async { Err("not used".into()) })
    }

    fn execute_tool(
        &self,
        _: ToolExecuteRequest,
    ) -> BoxFuture<'static, Result<ToolExecuteResponse, String>> {
        Box::pin(async {
            Ok(ToolExecuteResponse {
                delivery: None,
                stop_turn: false,
                content: "host executed".into(),
                is_error: false,
                note: None,
            })
        })
    }

    fn check_permission(
        &self,
        request: PermissionCheckRequest,
    ) -> BoxFuture<'static, Result<PermissionDecision, String>> {
        self.seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(request);
        Box::pin(async { Ok(PermissionDecision::allow()) })
    }
}

fn capturing_setup() -> (
    tempfile::TempDir,
    NativeToolCallbacks,
    Arc<std::sync::Mutex<Vec<PermissionCheckRequest>>>,
) {
    let dir = tempfile::tempdir().unwrap();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    // A default (Manual) snapshot asks for a sensitive path, so `evaluate`
    // returns an Ask verdict whose reason must reach the host.
    let engine = Arc::new(crate::permission::PermissionEngine::new(
        crate::permission::PolicySnapshot::default(),
    ));
    let native = NativeToolCallbacks {
        inner: Arc::new(CapturingPermissionCallbacks { seen: seen.clone() }),
        toolset: Arc::new(NativeToolset::new(dir.path().to_str().unwrap(), None).unwrap()),
        native_count: Arc::new(AtomicU32::new(0)),
        truncator: None,
        permission_engine: Some(engine),
        plan_guard: None,
        stale_guard: None,
        goal_guard: None,
        hook_guard: None,
        agent_tool_veto: None,
        tools_veto: None,
        todo_tool_veto: None,
        tower_worktree_root: None,
        tower_enabled: false,
        sandbox_policy: None,
    };
    (dir, native, seen)
}

/// The permission evaluation happens inside `execute_tool`, not
/// `check_permission` (which is a bare pass-through on this wrapper), so the
/// hand-off is driven from there. `Read` is native, so the toolset handles it
/// and the local engine gets to decide.
fn exec_request(tool_name: &str, arguments: serde_json::Value) -> ToolExecuteRequest {
    ToolExecuteRequest {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        turn_id: "turn-1".into(),
        tool_call_id: "call_1".into(),
        tool_name: tool_name.into(),
        arguments,
    }
}

/// Serializes the tests below.
///
/// One of them installs a process-wide locale, and the others assert the
/// English catalog entry for a key that locale *does* translate — without
/// this, a parallel run would hand one test the other's language.
///
/// A tokio `Mutex` rather than a `std` one: the guard is held across the
/// `execute_tool` await, which `clippy::await_holding_lock` rejects on the
/// blocking kind.
static REASON_LOCK: once_cell::sync::Lazy<tokio::sync::Mutex<()>> =
    once_cell::sync::Lazy::new(|| tokio::sync::Mutex::new(()));

async fn reason_lock() -> tokio::sync::MutexGuard<'static, ()> {
    REASON_LOCK.lock().await
}

#[tokio::test]
async fn an_ask_verdict_forwards_its_reason_to_the_host() {
    let _lock = reason_lock().await;
    let (_dir, callbacks, seen) = capturing_setup();

    let _ = callbacks
        .execute_tool(exec_request("Read", serde_json::json!({ "path": ".env" })))
        .await
        .expect("the host answers the ask");

    let requests = seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(requests.len(), 1, "exactly one permission consult");
    assert_eq!(
        requests[0].reason.as_deref(),
        Some("Access to sensitive file requires approval: .env"),
        "the policy's explanation must reach the host, not just the tool name"
    );
}

#[tokio::test]
async fn the_forwarded_reason_follows_the_host_locale() {
    let _lock = reason_lock().await;
    // Same hand-off, with a locale installed: the host receives the
    // localized sentence, so it needs no translation table of its own.
    crate::i18n::set_locale(crate::i18n::Locale::Zh);

    let (_dir, callbacks, seen) = capturing_setup();
    let _ = callbacks
        .execute_tool(exec_request("Read", serde_json::json!({ "path": ".env" })))
        .await
        .expect("the host answers the ask");

    crate::i18n::set_locale(crate::i18n::Locale::En);

    let requests = seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(
        requests[0].reason.as_deref(),
        Some("访问敏感文件需要审批：.env")
    );
}

#[tokio::test]
async fn a_deny_verdict_keeps_its_reason_on_the_returned_decision() {
    let _lock = reason_lock().await;
    // A deny never reaches the host: the engine answers it itself, and the
    // reason becomes the tool result the model sees.
    let dir = tempfile::tempdir().unwrap();
    let snapshot = crate::permission::PolicySnapshot {
        deny_rules: vec!["Read".to_string()],
        ..Default::default()
    };
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let callbacks = NativeToolCallbacks {
        inner: Arc::new(CapturingPermissionCallbacks { seen: seen.clone() }),
        toolset: Arc::new(NativeToolset::new(dir.path().to_str().unwrap(), None).unwrap()),
        native_count: Arc::new(AtomicU32::new(0)),
        truncator: None,
        permission_engine: Some(Arc::new(crate::permission::PermissionEngine::new(snapshot))),
        plan_guard: None,
        stale_guard: None,
        goal_guard: None,
        hook_guard: None,
        agent_tool_veto: None,
        tools_veto: None,
        todo_tool_veto: None,
        tower_worktree_root: None,
        tower_enabled: false,
        sandbox_policy: None,
    };

    let response = callbacks
        .execute_tool(exec_request("Read", serde_json::json!({ "path": "a.txt" })))
        .await
        .expect("the engine answers a deny itself");

    assert!(response.is_error);
    assert_eq!(response.content, "Denied by user rule: Read");
    assert!(
        seen.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty(),
        "a deny must not consult the host"
    );
}
