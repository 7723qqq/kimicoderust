use std::path::{Path, PathBuf};

use crate::callbacks::HostCallbacks;

use super::helloWorld::{HelloWorldModeTrigger, HelloWorldRunState, tool_signal};
use crate::features::helloWorld::helloWorldOps::{
    hello_world_mode_enter_event, hello_world_mode_exit_event, hello_world_mode_registry,
    hello_world_run_updated_event, hello_world_status_event,
};

pub struct HelloWorldService;

pub struct HelloWorldScope<'a> {
    pub agent_id: &'a str,
    pub session_id: &'a str,
    pub workspace_root: &'a Path,
}

fn emit_stage_change(
    callbacks: &dyn HostCallbacks,
    previous: Option<&str>,
    run: &HelloWorldRunState,
) {
    if let Some(previous) = previous
        && previous != run.stage
    {
        callbacks.telemetry(serde_json::json!({
            "event": "hello_world_stage_changed",
            "stage": run.stage,
            "previous": previous,
        }));
    }
}

impl HelloWorldService {
    pub fn enter_events(
        agent_id: &str,
        trigger: HelloWorldModeTrigger,
        task: Option<String>,
    ) -> Option<Vec<serde_json::Value>> {
        let run = hello_world_mode_registry().enter(agent_id, trigger, task.clone())?;
        Some(vec![
            hello_world_mode_enter_event(agent_id, trigger, task.as_deref()),
            hello_world_status_event(agent_id, true, Some(&run.stage)),
        ])
    }

    pub fn exit_events(agent_id: &str) -> Option<(HelloWorldModeTrigger, Vec<serde_json::Value>)> {
        let trigger = hello_world_mode_registry().exit(agent_id)?;
        Some((
            trigger,
            vec![
                hello_world_mode_exit_event(agent_id),
                hello_world_status_event(agent_id, false, None),
            ],
        ))
    }

    pub fn enter(
        callbacks: &dyn HostCallbacks,
        agent_id: &str,
        trigger: HelloWorldModeTrigger,
        task: Option<String>,
    ) -> bool {
        let Some(events) = Self::enter_events(agent_id, trigger, task) else {
            return false;
        };
        for event in events {
            callbacks.emit_event(event);
        }
        callbacks.telemetry(serde_json::json!({
            "event": "hello_world_mode_entered",
            "trigger": trigger.as_str(),
        }));
        true
    }

    pub async fn enter_scoped(
        callbacks: &dyn HostCallbacks,
        scope: HelloWorldScope<'_>,
        trigger: HelloWorldModeTrigger,
        task: Option<String>,
    ) -> bool {
        if !Self::enter(callbacks, scope.agent_id, trigger, task) {
            return false;
        }
        Self::evict_conflicts(callbacks, &scope).await;
        true
    }

    async fn evict_conflicts(callbacks: &dyn HostCallbacks, scope: &HelloWorldScope<'_>) {
        let _ = crate::tools::mode_mutex::exit_spec_for_mode_enter(callbacks, scope.agent_id).await;
        let _ = crate::swarm::mode::set_swarm_mode(callbacks, scope.agent_id, None);
        let repo_root = crate::tools::tower::paths::resolve_tower_repo_root(
            &scope.workspace_root.to_string_lossy(),
        );
        let store = crate::tools::tower::store::TowerStore::new(PathBuf::from(repo_root));
        if !store.is_initialized().await.unwrap_or(false) {
            return;
        }
        callbacks.emit_event(serde_json::json!({
            "type": "tower_mode.exit",
            "agentId": scope.agent_id,
        }));
        callbacks.emit_event(serde_json::json!({
            "type": "agent.status.updated",
            "agentId": scope.agent_id,
            "towerMode": false,
        }));
        callbacks.telemetry(serde_json::json!({
            "event": "tower_mode_exit",
            "reason": "mode-mutex",
        }));
        if let Err(error) = store.release(scope.session_id).await {
            tracing::warn!(%error, "failed to release tower workspace ownership");
        }
    }

    pub fn exit(callbacks: &dyn HostCallbacks, agent_id: &str) -> bool {
        let Some((trigger, events)) = Self::exit_events(agent_id) else {
            return false;
        };
        for event in events {
            callbacks.emit_event(event);
        }
        callbacks.telemetry(serde_json::json!({
            "event": "hello_world_mode_exited",
            "trigger": trigger.as_str(),
        }));
        let _ = crate::tools::CURRENT_CONVERSATION_HISTORY.try_with(|slot| {
            let mut history = slot.lock().unwrap_or_else(|error| error.into_inner());
            let remove = history.last().and_then(|message| message.origin.as_ref()).is_some_and(
                |origin| {
                    origin.get("kind").and_then(serde_json::Value::as_str) == Some("injection")
                        && origin.get("variant").and_then(serde_json::Value::as_str)
                            == Some(crate::injection::hello_world_mode::HELLO_WORLD_MODE_INJECTION_VARIANT)
                },
            );
            if remove {
                history.pop();
            }
        });
        true
    }

    pub fn advance(callbacks: &dyn HostCallbacks, agent_id: &str, stage: Option<&str>) -> bool {
        let previous = hello_world_mode_registry()
            .run(agent_id)
            .map(|run| run.stage);
        let Some(run) = hello_world_mode_registry().advance(agent_id, stage) else {
            return false;
        };
        callbacks.emit_event(hello_world_run_updated_event(agent_id, &run));
        callbacks.emit_event(hello_world_status_event(agent_id, true, Some(&run.stage)));
        emit_stage_change(callbacks, previous.as_deref(), &run);
        true
    }

    pub fn waive(callbacks: &dyn HostCallbacks, agent_id: &str, item: &str) -> bool {
        let Some(run) = hello_world_mode_registry().waive(agent_id, item) else {
            return false;
        };
        callbacks.emit_event(hello_world_run_updated_event(agent_id, &run));
        callbacks.emit_event(hello_world_status_event(agent_id, true, Some(&run.stage)));
        true
    }

    pub fn observe_tool_round(
        callbacks: &dyn HostCallbacks,
        agent_id: &str,
        tool_name: &str,
        args: Option<&serde_json::Value>,
        executed: bool,
        is_error: bool,
    ) -> bool {
        let Some(signal) = tool_signal(tool_name, args, executed, is_error) else {
            return false;
        };
        let previous = hello_world_mode_registry()
            .run(agent_id)
            .map(|run| run.stage);
        let Some(run) = hello_world_mode_registry().observe(agent_id, &signal) else {
            return false;
        };
        callbacks.emit_event(hello_world_run_updated_event(agent_id, &run));
        callbacks.emit_event(hello_world_status_event(agent_id, true, Some(&run.stage)));
        emit_stage_change(callbacks, previous.as_deref(), &run);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::types::{
        BoxFuture, LlmChatRequest, LlmChatResponse, PermissionCheckRequest, PermissionDecision,
        StateReadRequest, StateReadResponse, StateWriteRequest, StateWriteResponse,
        ToolExecuteRequest, ToolExecuteResponse,
    };
    use std::sync::Arc;

    struct RecordingCallbacks {
        events: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
        telemetry: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
        states: Arc<std::sync::Mutex<std::collections::HashMap<String, serde_json::Value>>>,
    }

    impl HostCallbacks for RecordingCallbacks {
        fn llm_chat(
            &self,
            _: LlmChatRequest,
        ) -> BoxFuture<'static, Result<LlmChatResponse, String>> {
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

        fn emit_event(&self, event: serde_json::Value) {
            self.events.lock().unwrap().push(event);
        }

        fn telemetry(&self, event: serde_json::Value) {
            self.telemetry.lock().unwrap().push(event);
        }

        fn state_read(
            &self,
            request: StateReadRequest,
        ) -> BoxFuture<'static, Result<StateReadResponse, String>> {
            let value = self
                .states
                .lock()
                .unwrap()
                .get(&request.domain)
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            Box::pin(async move { Ok(StateReadResponse { value }) })
        }

        fn state_write(
            &self,
            request: StateWriteRequest,
        ) -> BoxFuture<'static, Result<StateWriteResponse, String>> {
            let value = request.value;
            self.states
                .lock()
                .unwrap()
                .insert(request.domain, value.clone());
            Box::pin(async move { Ok(StateWriteResponse { ok: true, value }) })
        }
    }

    fn agent_id(label: &str) -> String {
        format!("{label}-{}", ulid::Ulid::new())
    }

    fn recording() -> RecordingCallbacks {
        RecordingCallbacks {
            events: Arc::new(std::sync::Mutex::new(Vec::new())),
            telemetry: Arc::new(std::sync::Mutex::new(Vec::new())),
            states: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        }
    }

    fn recorded(callbacks: &RecordingCallbacks) -> Vec<serde_json::Value> {
        callbacks.events.lock().unwrap().clone()
    }

    fn recorded_telemetry(callbacks: &RecordingCallbacks) -> Vec<serde_json::Value> {
        callbacks.telemetry.lock().unwrap().clone()
    }

    #[test]
    fn enter_emits_the_record_and_the_status_once() {
        let callbacks = recording();
        let agent = agent_id("enter");
        assert!(HelloWorldService::enter(
            &callbacks,
            &agent,
            HelloWorldModeTrigger::Manual,
            Some("ship it".to_string())
        ));
        assert!(!HelloWorldService::enter(
            &callbacks,
            &agent,
            HelloWorldModeTrigger::Task,
            None
        ));
        let events = recorded(&callbacks);
        assert_eq!(events.len(), 2, "{events:?}");
        assert_eq!(events[0]["type"], "hello_world_mode.enter");
        assert_eq!(events[0]["agentId"], agent);
        assert_eq!(events[0]["trigger"], "manual");
        assert_eq!(events[0]["task"], "ship it");
        assert_eq!(events[1]["type"], "agent.status.updated");
        assert_eq!(events[1]["helloWorldMode"], true);
        assert_eq!(events[1]["helloWorldStage"], "requirements");
    }

    #[test]
    fn exit_emits_the_record_and_clears_the_status() {
        let callbacks = recording();
        let agent = agent_id("exit");
        assert!(!HelloWorldService::exit(&callbacks, &agent));
        HelloWorldService::enter(&callbacks, &agent, HelloWorldModeTrigger::Task, None);
        assert!(HelloWorldService::exit(&callbacks, &agent));
        let events = recorded(&callbacks);
        assert_eq!(events.len(), 4, "{events:?}");
        assert_eq!(events[2]["type"], "hello_world_mode.exit");
        assert_eq!(events[2]["agentId"], agent);
        assert!(events[2].get("trigger").is_none());
        assert_eq!(events[3]["type"], "agent.status.updated");
        assert_eq!(events[3]["helloWorldMode"], false);
        assert!(events[3].get("helloWorldStage").is_none());
    }

    #[test]
    fn advance_and_waive_emit_the_run_record() {
        let callbacks = recording();
        let agent = agent_id("advance");
        HelloWorldService::enter(&callbacks, &agent, HelloWorldModeTrigger::Manual, None);
        assert!(HelloWorldService::advance(&callbacks, &agent, None));
        let events = recorded(&callbacks);
        assert_eq!(events[2]["type"], "hello_world_run.updated");
        assert_eq!(events[2]["stage"], "design");
        assert_eq!(events[3]["helloWorldStage"], "design");

        assert!(!HelloWorldService::advance(
            &callbacks,
            &agent,
            Some("requirements")
        ));
        assert!(HelloWorldService::waive(
            &callbacks,
            &agent,
            "design-written"
        ));
        let events = recorded(&callbacks);
        assert_eq!(events[4]["type"], "hello_world_run.updated");
        let items = events[4]["items"].as_array().unwrap();
        assert_eq!(items[1]["id"], "design-written");
        assert_eq!(items[1]["done"], true);
        assert_eq!(items[1]["evidence"], "waived");
        assert_eq!(events[4]["waivers"][0], "design-written");
    }

    #[test]
    fn a_matching_tool_round_advances_the_stage_and_emits() {
        let callbacks = recording();
        let agent = agent_id("observe");
        HelloWorldService::enter(&callbacks, &agent, HelloWorldModeTrigger::Manual, None);
        let args = serde_json::json!({ "path": "specs/x/requirements.md" });
        assert!(HelloWorldService::observe_tool_round(
            &callbacks,
            &agent,
            "Write",
            Some(&args),
            true,
            false
        ));
        let events = recorded(&callbacks);
        assert_eq!(events[2]["type"], "hello_world_run.updated");
        assert_eq!(events[2]["stage"], "design");
        assert_eq!(events[3]["helloWorldStage"], "design");

        assert!(!HelloWorldService::observe_tool_round(
            &callbacks, &agent, "Read", None, true, false
        ));
        assert!(!HelloWorldService::observe_tool_round(
            &callbacks,
            &agent,
            "Write",
            Some(&args),
            true,
            true
        ));
        assert_eq!(recorded(&callbacks).len(), 4);
    }

    #[test]
    fn mode_transitions_emit_the_v2_telemetry() {
        let callbacks = recording();
        let agent = agent_id("telemetry");
        HelloWorldService::enter(&callbacks, &agent, HelloWorldModeTrigger::Manual, None);
        assert_eq!(
            recorded_telemetry(&callbacks),
            vec![serde_json::json!({
                "event": "hello_world_mode_entered",
                "trigger": "manual",
            })]
        );

        HelloWorldService::advance(&callbacks, &agent, None);
        assert_eq!(
            recorded_telemetry(&callbacks),
            vec![
                serde_json::json!({
                    "event": "hello_world_mode_entered",
                    "trigger": "manual",
                }),
                serde_json::json!({
                    "event": "hello_world_stage_changed",
                    "stage": "design",
                    "previous": "requirements",
                }),
            ]
        );

        HelloWorldService::exit(&callbacks, &agent);
        let telemetry = recorded_telemetry(&callbacks);
        assert_eq!(telemetry.len(), 3, "{telemetry:?}");
        assert_eq!(telemetry[2]["event"], "hello_world_mode_exited");
        assert_eq!(telemetry[2]["trigger"], "manual");
    }

    #[test]
    fn a_stage_unchanged_round_emits_no_stage_telemetry() {
        let callbacks = recording();
        let agent = agent_id("no-stage-change");
        HelloWorldService::enter(&callbacks, &agent, HelloWorldModeTrigger::Task, None);
        HelloWorldService::waive(&callbacks, &agent, "requirements-approved");
        let telemetry = recorded_telemetry(&callbacks);
        assert_eq!(telemetry.len(), 1, "{telemetry:?}");
        assert_eq!(telemetry[0]["event"], "hello_world_mode_entered");
        assert_eq!(telemetry[0]["trigger"], "task");
    }

    #[tokio::test]
    async fn scoped_enter_evicts_spec_swarm_and_tower_but_not_plan() {
        let callbacks = recording();
        let agent = agent_id("conflicts");
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        callbacks
            .states
            .lock()
            .unwrap()
            .insert("spec".to_string(), serde_json::json!({ "active": true }));
        callbacks
            .states
            .lock()
            .unwrap()
            .insert("plan".to_string(), serde_json::json!({ "active": true }));
        crate::swarm::mode::set_swarm_mode(
            &callbacks,
            &agent,
            Some(crate::swarm::mode::SwarmModeTrigger::Manual),
        );
        let state_path = root.join(".tower/comms/state.json");
        std::fs::create_dir_all(state_path.parent().unwrap()).unwrap();
        std::fs::write(
            &state_path,
            serde_json::to_string(&serde_json::json!({
                "version": 1,
                "base": "main",
                "mode": "tower",
                "createdAt": "2026-01-01T00:00:00.000Z",
                "sessionId": "session-1",
            }))
            .unwrap(),
        )
        .unwrap();
        callbacks.events.lock().unwrap().clear();
        callbacks.telemetry.lock().unwrap().clear();

        assert!(
            HelloWorldService::enter_scoped(
                &callbacks,
                HelloWorldScope {
                    agent_id: &agent,
                    session_id: "session-1",
                    workspace_root: root,
                },
                HelloWorldModeTrigger::Manual,
                None,
            )
            .await
        );
        assert!(hello_world_mode_registry().is_active(&agent));
        assert_eq!(callbacks.states.lock().unwrap()["spec"]["active"], false);
        assert_eq!(callbacks.states.lock().unwrap()["plan"]["active"], true);
        assert!(!crate::swarm::mode::swarm_mode_registry().is_active(&agent));
        let store = crate::tools::tower::store::TowerStore::new(root.to_path_buf());
        assert_eq!(store.load().await.unwrap().session_id, None);
        let event_types: Vec<String> = recorded(&callbacks)
            .iter()
            .map(|event| event["type"].as_str().unwrap().to_string())
            .collect();
        assert!(event_types.contains(&"hello_world_mode.enter".to_string()));
        assert!(event_types.contains(&"swarm_mode.exit".to_string()));
        assert!(event_types.contains(&"tower_mode.exit".to_string()));
        let telemetry = recorded_telemetry(&callbacks);
        assert!(
            telemetry
                .iter()
                .any(|event| event["event"] == "tower_mode_exit" && event["reason"] == "mode-mutex")
        );
        HelloWorldService::exit(&callbacks, &agent);
    }

    #[tokio::test]
    async fn swarm_spec_and_tower_entries_evict_hello_world() {
        let callbacks = recording();
        let agent = "main";
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .output()
                .unwrap();
            assert!(output.status.success(), "git {args:?} failed");
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@e.test"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);
        hello_world_mode_registry().exit(agent);

        HelloWorldService::enter(&callbacks, agent, HelloWorldModeTrigger::Manual, None);
        crate::swarm::mode::set_swarm_mode(
            &callbacks,
            agent,
            Some(crate::swarm::mode::SwarmModeTrigger::Manual),
        );
        assert!(!hello_world_mode_registry().is_active(agent));

        HelloWorldService::enter(&callbacks, agent, HelloWorldModeTrigger::Manual, None);
        crate::tools::spec_mode::emit_spec_mode_enter(&callbacks, agent, "spec-demo");
        assert!(!hello_world_mode_registry().is_active(agent));

        HelloWorldService::enter(&callbacks, agent, HelloWorldModeTrigger::Manual, None);
        let result = crate::tools::tower::execute_tower_init(
            root,
            agent,
            "session-tower",
            "{}",
            Some(&callbacks),
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(!hello_world_mode_registry().is_active(agent));
    }

    #[tokio::test]
    async fn exit_pops_a_trailing_hello_world_reminder_from_live_history() {
        let callbacks = recording();
        let agent = agent_id("pop-history");
        let history = std::sync::Arc::new(std::sync::Mutex::new(vec![
            crate::turn_loop::types::LLMMessage::user("keep"),
            crate::turn_loop::types::LLMMessage {
                origin: Some(serde_json::json!({
                    "kind": "injection",
                    "variant": "hello_world_mode",
                })),
                ..crate::turn_loop::types::LLMMessage::user("reminder")
            },
        ]));
        crate::tools::CURRENT_CONVERSATION_HISTORY
            .scope(history.clone(), async move {
                HelloWorldService::enter(&callbacks, &agent, HelloWorldModeTrigger::Manual, None);
                HelloWorldService::exit(&callbacks, &agent);
            })
            .await;
        let history = history.lock().unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].content, "keep");
    }
}
