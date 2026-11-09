use std::path::{Path, PathBuf};

use serde_json::Value;

use super::tower::paths::{TOWER_NAME, resolve_tower_repo_root};
use super::tower::store::TowerStore;
use super::tower::types::{TowerMission, TowerMissionPatch, TowerMissionStatus, TowerState};
use crate::callbacks::HostCallbacks;
use crate::rpc::types::{StateReadRequest, StateWriteRequest};

pub fn open_mission_ids(state: &TowerState) -> Vec<String> {
    state
        .missions
        .iter()
        .filter(|m| m.status.is_open())
        .map(|m| m.id.clone())
        .collect()
}

pub async fn pause_tower_for_mode_enter(workspace_root: &Path, reason: &str) -> Vec<String> {
    let repo_root = resolve_tower_repo_root(&workspace_root.to_string_lossy());
    let store = TowerStore::new(PathBuf::from(repo_root));
    if !store.is_initialized().await.unwrap_or(false) {
        return Vec::new();
    }

    let state_lock = store.state_lock();
    let _state_guard = state_lock.lock().await;
    let state = match store.load().await {
        Ok(state) => state,
        Err(_) => return Vec::new(),
    };
    let mut paused = Vec::new();
    for id in open_mission_ids(&state) {
        let patch = TowerMissionPatch {
            status: Some(TowerMissionStatus::Paused),
            note: Some(format!(
                "mode mutex: auto-paused ({reason}); resume with TowerMission status=active"
            )),
            ..Default::default()
        };

        if store.update_mission(TOWER_NAME, &id, patch).await.is_ok() {
            paused.push(id);
        }
    }
    paused
}

pub async fn exit_plan_for_tower_enter(callbacks: &dyn HostCallbacks) -> bool {
    let read = StateReadRequest {
        domain: "plan".into(),
        key: "plan".into(),
        turn_id: String::new(),
        tool_call_id: String::new(),
    };
    let plan = match callbacks.state_read(read).await {
        Ok(response) => response.value,
        Err(_) => return false,
    };
    if plan.get("active").and_then(Value::as_bool) != Some(true) {
        return false;
    }
    let write = StateWriteRequest {
        domain: "plan".into(),
        key: "plan".into(),
        value: serde_json::json!({ "active": false }),
        undoable: true,
        turn_id: String::new(),
        tool_call_id: String::new(),
    };
    callbacks.state_write(write).await.is_ok()
}

pub fn exit_swarm_for_tower_enter(callbacks: &dyn HostCallbacks, agent_id: &str) -> bool {
    crate::swarm::mode::set_swarm_mode(callbacks, agent_id, None)
}

/// v2 `AgentModeMutexService`'s `TowerModeEnter` handler: an active goal is
/// paused (`goal.pauseGoal({ reason: 'Paused after tower mode entered' },
/// 'system')`) so goal continuations cannot race the tower fleet. Returns
/// whether a pause was written; an unreadable or inactive goal is a no-op.
pub async fn pause_goal_for_tower_enter(callbacks: &dyn HostCallbacks) -> bool {
    let goal = match read_mode_state(callbacks, "goal").await {
        Ok(value) => value,
        Err(_) => return false,
    };
    if goal
        .get("goal")
        .and_then(|snapshot| snapshot.get("status"))
        .and_then(Value::as_str)
        != Some("active")
    {
        return false;
    }
    let write = StateWriteRequest {
        domain: "goal".into(),
        key: "goal".into(),
        value: serde_json::json!({
            "action": "lifecycle",
            "status": "paused",
            "reason": "Paused after tower mode entered",
        }),
        undoable: false,
        turn_id: String::new(),
        tool_call_id: String::new(),
    };
    callbacks.state_write(write).await.is_ok()
}

/// v2 `AgentModeMutexService`'s `GoalUpdated` handler: a goal that (re)becomes
/// active evicts tower mode (`tower.exit('mode-mutex')`). The native engine
/// has no tower mode flag, so the equivalent is pausing open missions — worker
/// spawns and merges stay blocked until they are resumed. Returns the paused
/// mission ids; a goal that is not active leaves the tower alone.
pub async fn evict_tower_for_goal_activation(
    callbacks: &dyn HostCallbacks,
    workspace_root: &Path,
) -> Vec<String> {
    let goal = match read_mode_state(callbacks, "goal").await {
        Ok(value) => value,
        Err(_) => return Vec::new(),
    };
    if goal
        .get("goal")
        .and_then(|snapshot| snapshot.get("status"))
        .and_then(Value::as_str)
        != Some("active")
    {
        return Vec::new();
    }
    pause_tower_for_mode_enter(workspace_root, "goal mode").await
}

pub async fn refuse_swarm_with_active_tower(workspace_root: &Path) -> Option<String> {
    let repo_root = resolve_tower_repo_root(&workspace_root.to_string_lossy());
    let store = TowerStore::new(PathBuf::from(repo_root));
    if !store.is_initialized().await.unwrap_or(false) {
        return None;
    }
    let state_lock = store.state_lock();
    let _state_guard = state_lock.lock().await;
    let state = match store.load().await {
        Ok(state) => state,
        Err(error) => {
            return Some(format!(
                "AgentSwarm is not available: this workspace has a tower whose state could not be read ({error}), so the mode mutex cannot confirm that swarm and tower are exclusive. Repair or remove the tower state under .tower/, or run the swarm from a workspace without one."
            ));
        }
    };
    let open = open_mission_ids(&state);
    if open.is_empty() {
        return None;
    }

    Some(format!(
        "AgentSwarm is not available while tower mode is active — swarm and tower modes are mutually exclusive, and the tower fleet runs through TowerSpawn, one mission per worker in its own worktree. Open mission(s): {}. Finish them (merge or abandon with TowerMission) before dispatching a swarm.",
        open.join(", ")
    ))
}

pub fn refuse_paused_mission(mission: &TowerMission) -> Option<String> {
    if mission.status != TowerMissionStatus::Paused {
        return None;
    }
    Some(format!(
        "mission {} is paused by the mode mutex (plan mode or a swarm took over) — spawning workers and merging stay blocked so the modes cannot diverge. Resume it with TowerMission status=active when the current work no longer conflicts.",
        mission.id
    ))
}

pub fn tower_paused_note(ids: &[String]) -> String {
    format!(
        "\n\nMode mutex: paused {} open tower mission(s) ({}) so planning and worker writes cannot diverge. Worker spawns and merges on paused missions are refused until resume — resume any of them with TowerMission status=active when the current work no longer conflicts.",
        ids.len(),
        ids.join(", ")
    )
}

pub fn plan_exited_note() -> String {
    "Mode mutex: plan mode was active, so it was exited before tower init (plan file preserved)."
        .to_string()
}

pub fn swarm_exited_note() -> String {
    "Mode mutex: swarm mode was active, so it was exited before tower init.".to_string()
}

pub fn mode_switch_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
        std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));
    &LOCK
}

pub async fn read_mode_state(callbacks: &dyn HostCallbacks, domain: &str) -> Result<Value, String> {
    let read = StateReadRequest {
        domain: domain.into(),
        key: domain.into(),
        turn_id: String::new(),
        tool_call_id: String::new(),
    };
    callbacks
        .state_read(read)
        .await
        .map(|response| response.value)
}

pub async fn write_mode_state(
    callbacks: &dyn HostCallbacks,
    domain: &str,
    value: Value,
) -> Result<(), String> {
    let write = StateWriteRequest {
        domain: domain.into(),
        key: domain.into(),
        value,
        undoable: true,
        turn_id: String::new(),
        tool_call_id: String::new(),
    };
    callbacks.state_write(write).await.map(|_| ())
}

pub async fn exit_spec_for_mode_enter(
    callbacks: &dyn HostCallbacks,
    agent_id: &str,
) -> Result<bool, String> {
    let spec = read_mode_state(callbacks, "spec").await?;
    if spec.get("active").and_then(Value::as_bool) != Some(true) {
        return Ok(false);
    }
    write_mode_state(
        callbacks,
        "spec",
        serde_json::json!({ "active": false, "lastTransition": "exit" }),
    )
    .await?;
    emit_spec_mode_exit(callbacks, agent_id);
    Ok(true)
}

pub fn emit_spec_mode_exit(callbacks: &dyn HostCallbacks, agent_id: &str) {
    callbacks.emit_event(serde_json::json!({
        "type": "spec_mode.exit",
        "agentId": agent_id,
    }));
    callbacks.emit_event(serde_json::json!({
        "type": "agent.status.updated",
        "agentId": agent_id,
        "specMode": false,
    }));
}

pub fn spec_exited_note() -> String {
    "Mode mutex: spec mode was active, so it was exited before plan mode entered (spec files preserved)."
        .to_string()
}

pub fn spec_exited_for_tower_note() -> String {
    "Mode mutex: spec mode was active, so it was exited before tower mode entered (spec files preserved)."
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::callbacks::HostCallbacks;
    use crate::rpc::types::{
        BoxFuture, LlmChatRequest, LlmChatResponse, PermissionCheckRequest, PermissionDecision,
        StateReadResponse, StateWriteResponse, ToolExecuteRequest, ToolExecuteResponse,
    };
    use crate::tools::tower::types::{TowerMission, TowerMissionKind};
    use std::sync::{Arc, Mutex as StdMutex};

    fn mission(id: &str, status: TowerMissionStatus) -> TowerMission {
        TowerMission {
            id: id.into(),
            title: format!("{id} title"),
            slug: id.into(),
            kind: TowerMissionKind::default(),
            scope: vec!["src/".into()],
            branch: format!("tower/{id}"),
            worktree: format!("wt-{id}"),
            spawn_base: None,
            deps: Vec::new(),
            status,
            owner: None,
            context: None,
            tasks: Vec::new(),
            notes: Vec::new(),
            blockers: Vec::new(),
        }
    }

    fn state_with(missions: Vec<TowerMission>) -> TowerState {
        TowerState {
            version: 1,
            base: "main".into(),
            mode: "tower".into(),
            created_at: "2026-09-14T00:00:00Z".into(),
            session_id: None,
            roster: Default::default(),
            missions,
        }
    }

    #[test]
    fn open_mission_ids_lists_only_open() {
        let state = state_with(vec![
            mission("m1", TowerMissionStatus::Active),
            mission("m2", TowerMissionStatus::Merged),
            mission("m3", TowerMissionStatus::Abandoned),
            mission("m4", TowerMissionStatus::Paused),
        ]);
        assert_eq!(
            open_mission_ids(&state),
            vec!["m1".to_string(), "m4".to_string()]
        );
    }

    #[test]
    fn refuse_paused_mission_gates_only_paused() {
        let refused = refuse_paused_mission(&mission("m1", TowerMissionStatus::Paused)).unwrap();
        assert!(refused.contains("m1"), "refusal must name the mission");
        assert!(
            refused.contains("status=active"),
            "refusal must tell how to resume"
        );
        assert!(refuse_paused_mission(&mission("m2", TowerMissionStatus::Active)).is_none());
        assert!(refuse_paused_mission(&mission("m3", TowerMissionStatus::Planned)).is_none());
    }

    #[tokio::test]
    async fn pause_skips_uninitialized_repo() {
        let dir = tempfile::tempdir().unwrap();
        let paused = pause_tower_for_mode_enter(dir.path(), "test").await;
        assert!(paused.is_empty());
    }

    #[tokio::test]
    async fn pause_marks_open_paused_and_keeps_closed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".tower/comms/missions")).unwrap();
        std::fs::create_dir_all(root.join(".tower/comms/log")).unwrap();

        let store = TowerStore::new(root.to_path_buf());
        store
            .save(&state_with(vec![
                mission("m1", TowerMissionStatus::Active),
                mission("m2", TowerMissionStatus::Merged),
                mission("m3", TowerMissionStatus::Planned),
            ]))
            .await
            .unwrap();

        let paused = pause_tower_for_mode_enter(root, "test").await;
        assert_eq!(paused, vec!["m1".to_string(), "m3".to_string()]);

        let reloaded = store.load().await.unwrap();
        let status_of = |id: &str| {
            reloaded
                .missions
                .iter()
                .find(|m| m.id == id)
                .map(|m| m.status)
        };
        assert_eq!(status_of("m1"), Some(TowerMissionStatus::Paused));
        assert_eq!(status_of("m3"), Some(TowerMissionStatus::Paused));
        assert_eq!(status_of("m2"), Some(TowerMissionStatus::Merged));
    }

    #[tokio::test]
    async fn refuse_swarm_with_active_tower_denies_when_missions_open() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".tower/comms/missions")).unwrap();
        std::fs::create_dir_all(root.join(".tower/comms/log")).unwrap();
        let store = TowerStore::new(root.to_path_buf());
        store.save(&state_with(Vec::new())).await.unwrap();
        assert!(
            refuse_swarm_with_active_tower(root).await.is_none(),
            "an initialized tower with no open mission still allows a swarm"
        );

        store
            .save(&state_with(vec![mission("m1", TowerMissionStatus::Active)]))
            .await
            .unwrap();
        let denial = refuse_swarm_with_active_tower(root)
            .await
            .expect("open mission denies the swarm");
        assert!(denial.contains("not available while tower mode is active"));
        assert!(denial.contains("mutually exclusive"));

        assert!(denial.contains("m1"), "{denial}");
        assert!(denial.contains("merge or abandon"), "{denial}");
    }

    #[tokio::test]
    async fn refuse_swarm_with_active_tower_denies_when_the_state_is_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".tower/comms")).unwrap();
        std::fs::write(root.join(".tower/comms/state.json"), "{ not json").unwrap();

        let denial = refuse_swarm_with_active_tower(root)
            .await
            .expect("an unreadable tower state denies the swarm");
        assert!(denial.contains("could not be read"), "{denial}");
    }

    #[tokio::test]
    async fn refuse_swarm_with_active_tower_skips_uninitialized_repo() {
        let dir = tempfile::tempdir().unwrap();
        assert!(refuse_swarm_with_active_tower(dir.path()).await.is_none());
    }

    struct MutexProbeCallbacks {
        plan_active: bool,
        goal_status: Option<&'static str>,
        writes: Arc<StdMutex<Vec<Value>>>,
        events: Arc<StdMutex<Vec<Value>>>,
    }

    impl HostCallbacks for MutexProbeCallbacks {
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

        fn emit_event(&self, event: Value) {
            self.events.lock().unwrap().push(event);
        }

        fn state_read(
            &self,
            request: StateReadRequest,
        ) -> BoxFuture<'static, Result<StateReadResponse, String>> {
            let value = if request.domain == "goal" {
                serde_json::json!({
                    "goal": self.goal_status.map(|status| serde_json::json!({ "status": status })),
                })
            } else {
                serde_json::json!({ "active": self.plan_active })
            };
            Box::pin(async move { Ok(StateReadResponse { value }) })
        }

        fn state_write(
            &self,
            request: StateWriteRequest,
        ) -> BoxFuture<'static, Result<StateWriteResponse, String>> {
            self.writes.lock().unwrap().push(request.value.clone());
            let value = request.value;
            Box::pin(async move { Ok(StateWriteResponse { ok: true, value }) })
        }
    }

    fn probe(active: bool) -> (MutexProbeCallbacks, Arc<StdMutex<Vec<Value>>>) {
        probe_with_goal(active, None)
    }

    fn probe_with_goal(
        active: bool,
        goal_status: Option<&'static str>,
    ) -> (MutexProbeCallbacks, Arc<StdMutex<Vec<Value>>>) {
        let writes = Arc::new(StdMutex::new(Vec::new()));
        (
            MutexProbeCallbacks {
                plan_active: active,
                goal_status,
                writes: writes.clone(),
                events: Arc::new(StdMutex::new(Vec::new())),
            },
            writes,
        )
    }

    #[tokio::test]
    async fn exit_plan_deactivates_active_plan() {
        let (callbacks, writes) = probe(true);
        assert!(exit_plan_for_tower_enter(&callbacks).await);
        let writes = writes.lock().unwrap();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0], serde_json::json!({ "active": false }));
    }

    #[tokio::test]
    async fn exit_plan_noop_when_inactive() {
        let (callbacks, writes) = probe(false);
        assert!(!exit_plan_for_tower_enter(&callbacks).await);
        assert!(writes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn tower_enter_pauses_an_active_goal() {
        let (callbacks, writes) = probe_with_goal(false, Some("active"));
        assert!(pause_goal_for_tower_enter(&callbacks).await);
        let writes = writes.lock().unwrap();
        assert_eq!(writes.len(), 1, "one lifecycle write: {writes:?}");
        assert_eq!(
            writes[0],
            serde_json::json!({
                "action": "lifecycle",
                "status": "paused",
                "reason": "Paused after tower mode entered",
            })
        );
    }

    #[tokio::test]
    async fn tower_enter_leaves_an_inactive_goal_alone() {
        for goal in [None, Some("paused"), Some("complete"), Some("blocked")] {
            let (callbacks, writes) = probe_with_goal(false, goal);
            assert!(
                !pause_goal_for_tower_enter(&callbacks).await,
                "goal: {goal:?}"
            );
            assert!(writes.lock().unwrap().is_empty(), "goal: {goal:?}");
        }
    }

    #[tokio::test]
    async fn goal_activation_pauses_open_tower_missions() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".tower/comms/missions")).unwrap();
        std::fs::create_dir_all(root.join(".tower/comms/log")).unwrap();
        let store = TowerStore::new(root.to_path_buf());
        store
            .save(&state_with(vec![
                mission("m1", TowerMissionStatus::Active),
                mission("m2", TowerMissionStatus::Merged),
            ]))
            .await
            .unwrap();

        let (callbacks, _writes) = probe_with_goal(false, Some("active"));
        assert_eq!(
            evict_tower_for_goal_activation(&callbacks, root).await,
            vec!["m1".to_string()]
        );
        let reloaded = store.load().await.unwrap();
        assert_eq!(reloaded.missions[0].status, TowerMissionStatus::Paused);

        store
            .save(&state_with(vec![mission("m3", TowerMissionStatus::Active)]))
            .await
            .unwrap();
        let (callbacks, _writes) = probe_with_goal(false, Some("paused"));
        assert!(
            evict_tower_for_goal_activation(&callbacks, root)
                .await
                .is_empty(),
            "a paused goal must not evict the tower"
        );
        let reloaded = store.load().await.unwrap();
        assert_eq!(reloaded.missions[0].status, TowerMissionStatus::Active);
    }

    #[test]
    fn entering_a_tower_closes_swarm_mode() {
        let agent = "mode-mutex-tower-agent";
        crate::swarm::mode::swarm_mode_registry().exit(agent);
        let (callbacks, _writes) = probe(false);
        assert!(crate::swarm::mode::set_swarm_mode(
            &callbacks,
            agent,
            Some(crate::swarm::mode::SwarmModeTrigger::Manual)
        ));
        callbacks.events.lock().unwrap().clear();

        assert!(
            exit_swarm_for_tower_enter(&callbacks, agent),
            "a tower entered over an open swarm closes it"
        );
        assert!(
            !crate::swarm::mode::swarm_mode_registry().is_active(agent),
            "the mode is off afterwards: a swarm left open with no exit would keep \
             announcing a workflow the tower fleet has replaced"
        );

        let events = callbacks.events.lock().unwrap().clone();
        assert_eq!(
            events,
            vec![
                serde_json::json!({ "type": "swarm_mode.exit", "agentId": agent }),
                serde_json::json!({
                    "type": "agent.status.updated",
                    "agentId": agent,
                    "swarmMode": false,
                }),
            ],
            "got: {events:?}"
        );
    }

    #[test]
    fn entering_a_tower_without_swarm_is_silent() {
        let agent = "mode-mutex-tower-idle-agent";
        crate::swarm::mode::swarm_mode_registry().exit(agent);
        let (callbacks, _writes) = probe(false);
        assert!(!exit_swarm_for_tower_enter(&callbacks, agent));
        assert!(callbacks.events.lock().unwrap().is_empty());
    }

    #[test]
    fn the_swarm_exit_note_says_why() {
        let note = swarm_exited_note();
        assert!(note.contains("swarm mode"), "{note}");
        assert!(note.contains("tower init"), "{note}");
    }
}
