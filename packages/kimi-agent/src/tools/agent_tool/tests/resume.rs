use super::super::*;
use super::common::*;

/// v2 `agentTool.ts` refuses to resume an agent the caller does not own.
/// Without it any agent could name any other agent's id in `resume` and
/// drive a turn inside its conversation.
#[tokio::test]
async fn a_resume_of_another_agents_subagent_is_refused() {
    let llm = Arc::new(RecordingPromptLlm::new(vec!["theirs".into()]));
    let manager = manager_with(llm).await;
    manager
        .register_discovered_definition(crate::subagent::types::SubagentDefinition {
            name: "research".into(),
            description: "d".into(),
            system_prompt: "You research.".into(),
            tools: vec![],
            disallowed_tools: vec![],
            when_to_use: None,
            prompt_prefix: None,
            summary_policy: None,
            model: None,
            subagents: None,
        })
        .await;
    // A subagent belonging to somebody else.
    let foreign = crate::tools::CALLER_AGENT_ID
        .scope(
            "other-agent".to_string(),
            manager.spawn("research", "Theirs"),
        )
        .await
        .unwrap();
    let _ = manager
        .run_foreground_turn(&foreign, "their work", None)
        .await;

    // This agent tries to resume it.
    let result = as_main_agent(execute_resume(
        &manager,
        None,
        &serde_json::json!({ "prompt": "continue" }),
        &foreign,
        None,
        None,
        None,
    ))
    .await
    .expect("the tool handled the call");

    assert!(result.is_error, "a cross-parent resume is refused");
    assert!(
        result
            .content
            .contains("does not belong to this parent agent"),
        "{}",
        result.content
    );
}

/// v2 `agentTool.ts`: an agent already running its own turn is not
/// resumable into a second concurrent turn.
#[tokio::test]
async fn a_resume_of_a_running_agent_is_refused() {
    let llm = Arc::new(RecordingPromptLlm::new(vec!["busy".into()]));
    let manager = manager_with(llm).await;
    manager
        .register_discovered_definition(crate::subagent::types::SubagentDefinition {
            name: "research".into(),
            description: "d".into(),
            system_prompt: "You research.".into(),
            tools: vec![],
            disallowed_tools: vec![],
            when_to_use: None,
            prompt_prefix: None,
            summary_policy: None,
            model: None,
            subagents: None,
        })
        .await;
    let id = as_main_agent(manager.spawn("research", "Busy"))
        .await
        .unwrap();
    manager
        .update_state(&id, crate::subagent::types::SubagentState::Running, None)
        .await;

    let result = as_main_agent(execute_resume(
        &manager,
        None,
        &serde_json::json!({ "prompt": "continue" }),
        &id,
        None,
        None,
        None,
    ))
    .await
    .expect("the tool handled the call");

    assert!(result.is_error);
    assert!(
        result
            .content
            .contains("already running and cannot run concurrently"),
        "{}",
        result.content
    );
}

#[tokio::test]
async fn resume_continues_a_completed_foreground_conversation() {
    let _owned = as_main_agent(async {
        let recorder = Arc::new(EventRecorder::new());
        let llm = Arc::new(RecordingPromptLlm::new(vec![
            "first pass findings".into(),
            "follow-up answer".into(),
        ]));
        let manager = manager_with_callbacks(llm.clone(), recorder.clone()).await;
        manager
            .register_discovered_definition(crate::subagent::types::SubagentDefinition {
                name: "research".into(),
                description: "d".into(),
                system_prompt: "You research.".into(),
                tools: vec![],
                disallowed_tools: vec![],
                when_to_use: None,
                prompt_prefix: None,
                summary_policy: None,
                model: None,
                subagents: None,
            })
            .await;
        let first = execute_agent(
            &manager,
            None,
            &serde_json::json!({ "subagent_type": "research", "prompt": "go" }),
            None,
            None,
            None,
            None,
        )
        .await
        .expect("first turn runs natively");
        assert!(!first.is_error);
        let agent_id = recorder.events.lock().unwrap()[0]["subagent_id"]
            .as_str()
            .unwrap()
            .to_string();

        let second = execute_agent(
            &manager,
            None,
            &serde_json::json!({ "resume": agent_id, "prompt": "continue" }),
            None,
            None,
            None,
            None,
        )
        .await
        .expect("native resume for a held conversation");
        assert!(!second.is_error);
        assert!(second.content.contains("follow-up answer"));
        assert_eq!(llm.call_count(), 2);
        // The resume turn saw the full prior conversation.
        let second_messages = llm.prompts.lock().unwrap().len();
        assert_eq!(second_messages, 2);
    })
    .await;
}

#[tokio::test]
async fn unknown_resume_ids_stay_host_owned() {
    let manager = manager_with(Arc::new(SummaryLlm)).await;
    let result = execute_agent(
        &manager,
        None,
        &serde_json::json!({ "resume": "agent-unknown", "prompt": "x" }),
        None,
        None,
        None,
        None,
    )
    .await;
    assert!(result.is_none(), "unknown resume ids fall back to the host");
}

/// A tower worker's foreground resume is refused with the background
/// hint (v2 `towerService` veto, #3648): the tower never needs a worker's
/// return value inline, and a blocking resume jams the whole fleet.
#[tokio::test]
async fn tower_roster_foreground_resume_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let repo_root = dir.path().to_path_buf();
    let store = crate::tools::tower::store::TowerStore::new(repo_root.clone());
    let state_dir = store
        .abs(crate::tools::tower::paths::STATE_FILE)
        .parent()
        .unwrap()
        .to_path_buf();
    tokio::fs::create_dir_all(state_dir).await.unwrap();
    let roster = crate::tools::tower::types::TowerRoster {
        agents: vec![crate::tools::tower::types::TowerRosterEntry {
            name: "worker-3648".into(),
            agent_id: "subagent-42".into(),
            session_id: Some("session-1".into()),
            kind: crate::tools::tower::types::TowerAgentKind::Worker,
            mission_id: Some("M1".into()),
            review_target: None,
            review_mission_id: None,
            worktree: Some("wt-1".into()),
            branch: Some("feat/x".into()),
            spawned_at: "2026-09-18T00:00:00Z".into(),
            died_at: None,
            death_status: None,
            death_reason: None,
            status: None,
        }],
    };
    let state = crate::tools::tower::types::TowerState {
        version: 1,
        base: "main".into(),
        mode: "branch".into(),
        created_at: "2026-09-18T00:00:00Z".into(),
        session_id: Some("session-1".into()),
        roster,
        missions: Vec::new(),
    };
    store.save(&state).await.unwrap();

    // The roster hit answers through the exposed resolver, and the gate
    // produces the run_in_background hint for a tower profile.
    let target = crate::tools::tower::tower_resume_target(Some(repo_root.clone()), "subagent-42")
        .await
        .expect("roster state is readable")
        .expect("roster member resolves");
    assert_eq!(target.name, "worker-3648");
    let denial = tower_resume_denial(
        Some(repo_root),
        "tower-worker",
        "subagent-42",
        Some(false),
        true,
    )
    .await
    .expect("foreground tower resume is denied");
    assert!(denial.contains("worker-3648"), "{denial}");
    assert!(denial.contains("run_in_background=true"), "{denial}");
    assert!(denial.contains("resume=\"subagent-42\""), "{denial}");
}

/// P2-2 regression: the roster lookup resolves the tower repo root from
/// the *passed* root, not the process cwd. With the tower state living
/// under `root` and the process cwd parked somewhere tower-less, the gate
/// must still deny (it used to consult the cwd's tower and let the resume
/// through), and the fallback without a root keeps reading the cwd.
#[tokio::test]
async fn tower_resume_denial_uses_passed_root_not_process_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let repo_root = dir.path().to_path_buf();
    let store = crate::tools::tower::store::TowerStore::new(repo_root.clone());
    let state_dir = store
        .abs(crate::tools::tower::paths::STATE_FILE)
        .parent()
        .unwrap()
        .to_path_buf();
    tokio::fs::create_dir_all(state_dir).await.unwrap();
    let state = crate::tools::tower::types::TowerState {
        version: 1,
        base: "main".into(),
        mode: "branch".into(),
        created_at: "2026-09-18T00:00:00Z".into(),
        session_id: Some("session-1".into()),
        roster: crate::tools::tower::types::TowerRoster {
            agents: vec![crate::tools::tower::types::TowerRosterEntry {
                name: "worker-p22".into(),
                agent_id: "subagent-p22".into(),
                session_id: Some("session-1".into()),
                kind: crate::tools::tower::types::TowerAgentKind::Worker,
                mission_id: Some("M1".into()),
                review_target: None,
                review_mission_id: None,
                worktree: None,
                branch: None,
                spawned_at: "2026-09-18T00:00:00Z".into(),
                died_at: None,
                death_status: None,
                death_reason: None,
                status: None,
            }],
        },
        missions: Vec::new(),
    };
    store.save(&state).await.unwrap();

    // The process cwd sits in a second, tower-less temp dir. `CwdScope` holds
    // the suite-wide cwd lock, so no other test spawns a child inside this
    // window, and the directory is restored even if an assertion below panics.
    let towerless = tempfile::tempdir().unwrap();
    let _cwd = crate::test_support::CwdScope::enter(towerless.path()).await;

    // With the canonical root passed, the gate denies even though the cwd
    // has no tower state at all — the roster is found via `root`.
    let denial = tower_resume_denial(
        Some(repo_root.clone()),
        "research",
        "subagent-p22",
        Some(false),
        true,
    )
    .await
    .expect("roster resolves through the passed root, not the cwd");
    assert!(denial.contains("worker-p22"), "{denial}");

    // Without a root (no-toolset call site), the fallback reads the
    // process cwd — tower-less here, so nothing is gated.
    assert!(
        tower_resume_denial(None, "research", "subagent-p22", Some(false), true)
            .await
            .is_none(),
        "the cwd fallback keeps the pre-P2-2 behavior"
    );
}

/// A non-roster resume path is untouched: the gate stays silent for a
/// plain profile, a non-member id, and an explicit background call.
#[tokio::test]
async fn non_tower_foreground_resume_is_unchanged() {
    // No roster file anywhere: the gate passes everything through.
    let cwd = tempfile::tempdir().unwrap().path().to_path_buf();
    assert!(
        tower_resume_denial(
            Some(cwd.clone()),
            "tower-worker",
            "subagent-42",
            Some(false),
            true
        )
        .await
        .is_none(),
        "no tower state.json = no gate"
    );
    // A roster member with run_in_background=true goes through.
    let dir = tempfile::tempdir().unwrap();
    let store = crate::tools::tower::store::TowerStore::new(dir.path());
    let state_dir = store
        .abs(crate::tools::tower::paths::STATE_FILE)
        .parent()
        .unwrap()
        .to_path_buf();
    tokio::fs::create_dir_all(state_dir).await.unwrap();
    let mut state = crate::tools::tower::types::TowerState {
        version: 1,
        base: "main".into(),
        mode: "branch".into(),
        created_at: "2026-09-18T00:00:00Z".into(),
        session_id: Some("session-1".into()),
        roster: crate::tools::tower::types::TowerRoster::default(),
        missions: Vec::new(),
    };
    state
        .roster
        .agents
        .push(crate::tools::tower::types::TowerRosterEntry {
            name: "worker-3648".into(),
            agent_id: "subagent-42".into(),
            session_id: Some("session-1".into()),
            kind: crate::tools::tower::types::TowerAgentKind::Worker,
            mission_id: None,
            review_target: None,
            review_mission_id: None,
            worktree: None,
            branch: None,
            spawned_at: "2026-09-18T00:00:00Z".into(),
            died_at: None,
            death_status: None,
            death_reason: None,
            status: None,
        });
    store.save(&state).await.unwrap();
    assert!(
        tower_resume_denial(
            Some(dir.path().to_path_buf()),
            "tower-worker",
            "subagent-42",
            Some(true),
            true
        )
        .await
        .is_none(),
        "background resume is allowed"
    );
    // A non-tower profile is only the tower's business when its id sits
    // in the roster: "research" + roster member subagent-42 is denied
    // through the roster arm, "research" + unknown id is not gated.
    let denial = tower_resume_denial(
        Some(dir.path().to_path_buf()),
        "research",
        "subagent-42",
        Some(false),
        true,
    )
    .await
    .expect("a roster member is denied whatever its profile");
    assert!(denial.contains("worker-3648"), "{denial}");
    assert!(
        tower_resume_denial(
            Some(dir.path().to_path_buf()),
            "research",
            "subagent-999",
            Some(false),
            true
        )
        .await
        .is_none(),
        "a plain profile outside the roster is not gated"
    );
    // No background availability (no TaskList/TaskOutput/TaskStop on the
    // host table): v2 lets the foreground resume through rather than
    // leaving the agent unrecovered.
    assert!(
        tower_resume_denial(
            Some(dir.path().to_path_buf()),
            "tower-worker",
            "subagent-42",
            Some(false),
            false
        )
        .await
        .is_none(),
        "without a task runner the foreground resume is the only form"
    );
    // An unknown id is not a roster member, so a plain profile stays
    // allowed...
    assert!(
        crate::tools::tower::tower_resume_target(Some(dir.path().to_path_buf()), "subagent-999")
            .await
            .expect("roster state is readable")
            .is_none()
    );
    assert!(
        tower_resume_denial(
            Some(dir.path().to_path_buf()),
            "research",
            "subagent-999",
            Some(false),
            true
        )
        .await
        .is_none(),
        "a plain profile outside the roster is not denied"
    );
    // ...but the tower-worker profile alone denies: the profile hit is
    // enough (mission spec: profile hit OR roster membership).
    let denial = tower_resume_denial(
        Some(dir.path().to_path_buf()),
        "tower-worker",
        "subagent-999",
        Some(false),
        true,
    )
    .await
    .expect("tower profile foreground resume is denied");
    assert!(denial.contains("run_in_background=true"), "{denial}");
}

/// The mandated recovery form actually runs: `Agent(resume=…,
/// run_in_background=true)` returns immediately with the running shape
/// (no duplicate spawned/started pair) and the resumed turn completes
/// detached, reporting one `subagent.completed`.
#[tokio::test]
async fn background_resume_runs_detached_and_reports_once() {
    let _owned = as_main_agent(async {
        let recorder = Arc::new(EventRecorder::new());
        let llm = Arc::new(RecordingPromptLlm::new(vec![
            "first pass findings".into(),
            "follow-up answer".into(),
        ]));
        let manager = manager_with_callbacks(llm.clone(), recorder.clone()).await;
        manager
            .register_discovered_definition(crate::subagent::types::SubagentDefinition {
                name: "reviewer".into(),
                description: "d".into(),
                system_prompt: "You are a tower worker.".into(),
                tools: vec![],
                disallowed_tools: vec![],
                when_to_use: None,
                prompt_prefix: None,
                summary_policy: None,
                model: None,
                subagents: None,
            })
            .await;
        let first = execute_agent(
            &manager,
            None,
            &serde_json::json!({ "subagent_type": "reviewer", "prompt": "go" }),
            None,
            None,
            None,
            None,
        )
        .await
        .expect("first turn runs natively");
        assert!(!first.is_error);
        let agent_id = recorder.events.lock().unwrap()[0]["subagent_id"]
            .as_str()
            .unwrap()
            .to_string();
        recorder.events.lock().unwrap().clear();

        let second = execute_agent(
        &manager,
        None,
        &serde_json::json!({ "resume": agent_id, "prompt": "continue", "run_in_background": true }),
        None,
        None,
        None,
        None,
    )
    .await
    .expect("native resume for a held conversation");
        assert!(!second.is_error, "{}", second.content);
        assert!(
            second.content.contains("status: running"),
            "{}",
            second.content
        );
        assert!(
            second.content.contains("automatic_notification: true"),
            "{}",
            second.content
        );
        // Without a task runner there is no task entry, so the running shape
        // carries only the stable agent id.
        assert!(
            !second.content.contains("task_id:"),
            "no runner, no task id: {}",
            second.content
        );
        // The call returns without awaiting the resumed turn.
        assert_eq!(llm.call_count(), 1, "the resumed turn has not run yet");

        // The detached turn completes and reports exactly one terminal event.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        for _ in 0..50 {
            if llm.call_count() == 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert_eq!(llm.call_count(), 2, "the resumed turn ran");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let events = recorder.events.lock().unwrap();
        let spawned: Vec<&serde_json::Value> = events
            .iter()
            .filter(|e| e["type"] == "subagent.spawned")
            .collect();
        assert_eq!(spawned.len(), 1, "exactly one spawned event: {events:?}");
        assert_eq!(spawned[0]["run_in_background"], true);
        let completed: Vec<&serde_json::Value> = events
            .iter()
            .filter(|e| e["type"] == "subagent.completed")
            .collect();
        assert_eq!(
            completed.len(),
            1,
            "exactly one completed event: {events:?}"
        );
        assert_eq!(completed[0]["result_summary"], "follow-up answer");
    })
    .await;
}

/// The same availability gate guards the resume arm: it must reject before
/// the detached turn starts, not merely change the Agent description.
#[tokio::test]
async fn background_resume_is_refused_without_task_output() {
    as_main_agent(async {
        let llm = Arc::new(RecordingPromptLlm::new(vec![
            "first pass findings".into(),
            "must not run".into(),
        ]));
        let manager = manager_with_callbacks(
            llm.clone(),
            Arc::new(MissingTaskOutputCallbacks),
        )
        .await;
        manager.register_builtin_profiles().await;
        let first = execute_agent(
            &manager,
            None,
            &serde_json::json!({ "subagent_type": "coder", "prompt": "go" }),
            None,
            None,
            None,
            None,
        )
        .await
        .expect("the foreground turn runs natively");
        assert!(!first.is_error, "{}", first.content);
        let agent_id = first
            .content
            .lines()
            .find_map(|line| line.strip_prefix("agent_id: "))
            .expect("the foreground result carries the agent id")
            .to_string();

        let resumed = execute_agent(
            &manager,
            None,
            &serde_json::json!({
                "resume": agent_id,
                "prompt": "continue",
                "run_in_background": true,
            }),
            None,
            None,
            None,
            None,
        )
        .await
        .expect("the native resume gate answers");
        assert!(resumed.is_error, "{}", resumed.content);
        assert_eq!(
            resumed.content,
            "Background agent execution is not available for this agent because TaskList, TaskOutput, and TaskStop are not enabled."
        );
        assert_eq!(llm.call_count(), 1, "the resumed turn never launched");
    })
    .await;
}

/// The regression for the round-1 review P1-1: background resume tasks
/// are registered under a *fresh* task id, because TaskRunner keeps
/// settled entries and rejects duplicate ids — under the stable agent id
/// the second background resume of the same tower agent used to be a
/// silently swallowed no-op. Both resumes here must actually run.
#[tokio::test]
async fn second_background_resume_of_the_same_agent_still_runs() {
    let _owned = as_main_agent(async {
    let recorder = Arc::new(EventRecorder::new());
    let llm = Arc::new(RecordingPromptLlm::new(vec![
        "first pass findings".into(),
        "second pass answer".into(),
        "third pass answer".into(),
    ]));
    let manager = manager_with_callbacks(llm.clone(), recorder.clone()).await;
    let runner = Arc::new(crate::storage::TaskRunner::new(None));
    manager.set_task_runner(runner.clone()).await;
    manager
        .register_discovered_definition(crate::subagent::types::SubagentDefinition {
            name: "reviewer".into(),
            description: "d".into(),
            system_prompt: "You are a tower worker.".into(),
            tools: vec![],
            disallowed_tools: vec![],
            when_to_use: None,
            prompt_prefix: None,
            summary_policy: None,
            model: None,
            subagents: None,
        })
        .await;
    let first = execute_agent(
        &manager,
        None,
        &serde_json::json!({ "subagent_type": "reviewer", "prompt": "go" }),
        None,
        None,
        None,
        None,
    )
    .await
    .expect("first turn runs natively");
    assert!(!first.is_error);
    let agent_id = recorder.events.lock().unwrap()[0]["subagent_id"]
        .as_str()
        .unwrap()
        .to_string();
    recorder.events.lock().unwrap().clear();

    let resume_args = |prompt: &str| serde_json::json!({ "resume": agent_id, "prompt": prompt, "run_in_background": true });

    // First background resume: registered, and its fresh task id is the
    // one the running shape advertises (TaskList/TaskStop key off it).
    let second = execute_agent(
        &manager,
        None,
        &resume_args("continue"),
        None,
        None,
        None,
        None,
    )
    .await
    .expect("native resume for a held conversation");
    assert!(!second.is_error, "{}", second.content);
    let first_task_id = second
        .content
        .lines()
        .find(|l| l.starts_with("task_id: "))
        .map(|l| l["task_id: ".len()..].to_string())
        .expect("runner arm advertises a fresh task id");
    assert_ne!(
        first_task_id, agent_id,
        "the task id must not reuse the stable agent id"
    );
    assert!(
        second.content.contains(&format!("agent_id: {agent_id}")),
        "the agent id stays in the shape: {}",
        second.content
    );
    // The registered task exists and eventually settles as completed.
    for _ in 0..50 {
        if llm.call_count() == 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(matches!(
        runner.wait(&first_task_id, 5000).await,
        crate::storage::TaskWaitResult::Completed(_)
    ));

    // Second background resume of the SAME agent: must register and run,
    // not silently no-op behind a duplicate-id rejection.
    let third = execute_agent(
        &manager,
        None,
        &resume_args("once more"),
        None,
        None,
        None,
        None,
    )
    .await
    .expect("second background resume is a native resume");
    assert!(
        !third.is_error,
        "duplicate task registration must not fail the call: {}",
        third.content
    );
    let second_task_id = third
        .content
        .lines()
        .find(|l| l.starts_with("task_id: "))
        .map(|l| l["task_id: ".len()..].to_string())
        .expect("the second resume also advertises a task id");
    assert_ne!(
        second_task_id, first_task_id,
        "every registration mints its own task id"
    );
    for _ in 0..50 {
        if llm.call_count() == 3 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(llm.call_count(), 3, "the second resumed turn actually ran");
    assert!(matches!(
        runner.wait(&second_task_id, 5000).await,
        crate::storage::TaskWaitResult::Completed(_)
    ));
    // Both turns reported their own completion to the host.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let events = recorder.events.lock().unwrap();
    let completed: Vec<&serde_json::Value> = events
        .iter()
        .filter(|e| e["type"] == "subagent.completed")
        .collect();
    assert_eq!(
        completed.len(),
        2,
        "one completion per background resume: {events:?}"
    );
    assert_eq!(completed[0]["result_summary"], "second pass answer");
    assert_eq!(completed[1]["result_summary"], "third pass answer");
    }).await;
}

/// v2 `agentTool.ts` refuses a resume that also names a `subagent_type` before
/// it touches the lifecycle: the resumed agent keeps its own profile. The fork
/// accepted both and silently ignored the type, and the schema description
/// already claimed the combination was rejected.
#[tokio::test]
async fn resume_with_a_subagent_type_is_refused() {
    let _owned = as_main_agent(async {
        let recorder = Arc::new(EventRecorder::new());
        let llm = Arc::new(RecordingPromptLlm::new(vec![]));
        let manager = manager_with_callbacks(llm.clone(), recorder.clone()).await;

        let result = execute_agent(
            &manager,
            None,
            &serde_json::json!({
                "resume": "agent-existing",
                "prompt": "continue",
                "subagent_type": "explore",
            }),
            None,
            None,
            None,
            None,
        )
        .await
        .expect("the tool handles the call");

        assert!(result.is_error, "content: {}", result.content);
        assert_eq!(
            result.content,
            "Cannot set subagent_type when resuming an existing agent. Resume by agent id only."
        );
        assert_eq!(llm.call_count(), 0, "no turn may run");
    })
    .await;
}
