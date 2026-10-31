use super::*;
use crate::tools::tower::git;
use crate::tools::tower::types::TowerAgentKind;

fn entry(name: &str, agent_id: &str) -> TowerRosterEntry {
    TowerRosterEntry {
        name: name.into(),
        agent_id: agent_id.into(),
        session_id: Some("session-1".into()),
        kind: TowerAgentKind::Worker,
        mission_id: None,
        review_target: None,
        review_mission_id: None,
        worktree: None,
        branch: None,
        spawned_at: "2026-09-15T00:00:00Z".into(),
        died_at: None,
        death_status: None,
        death_reason: None,
        status: None,
    }
}

fn state_with(agents: Vec<TowerRosterEntry>) -> TowerState {
    TowerState {
        version: 1,
        base: "main".into(),
        mode: "branch".into(),
        created_at: "2026-09-15T00:00:00Z".into(),
        session_id: Some("session-1".into()),
        roster: TowerRoster { agents },
        missions: Vec::new(),
    }
}

/// A store rooted at a fresh temp dir, with the state directory in place
/// so `save` can write.
async fn store_in(dir: &Path) -> TowerStore {
    let store = TowerStore::new(dir);
    let state_dir = store.abs(STATE_FILE).parent().unwrap().to_path_buf();
    tokio::fs::create_dir_all(state_dir).await.unwrap();
    store
}

/// Like [`store_in`], with the record directories the writers target in
/// place (production gets them from `TowerInit`).
async fn store_with_record_dirs(dir: &Path) -> TowerStore {
    let store = store_in(dir).await;
    tokio::fs::create_dir_all(store.abs(MISSIONS_DIR))
        .await
        .unwrap();
    tokio::fs::create_dir_all(store.abs(FINDINGS_DIR))
        .await
        .unwrap();
    tokio::fs::create_dir_all(store.abs(INBOX_DIR))
        .await
        .unwrap();
    tokio::fs::create_dir_all(store.abs(REVIEWS_DIR))
        .await
        .unwrap();
    tokio::fs::create_dir_all(store.abs(ACTIVITY_LOG).parent().unwrap())
        .await
        .unwrap();
    store
}

/// Agent ids restart per session, so a freshly spawned worker can share an
/// id with a dead one still sitting in the roster. Resolving to the first
/// match handed the new worker the old agent's name — wrong inbox, wrong
/// sender, denied writes.
/// Upstream #3847: the filer's cumulative token total rides the finding
/// record (v2 `callerTokens`); an unknown total omits the line.
#[tokio::test]
async fn file_finding_records_the_callers_tokens() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_with_record_dirs(dir.path()).await;
    store
        .save(&state_with(vec![entry("worker-a", "agent-0")]))
        .await
        .unwrap();

    let finding = TowerFindingInput {
        r#type: crate::tools::tower::types::TowerFindingType::Bug,
        title: "Leaky cache".into(),
        severity: None,
        summary: "s".into(),
        location: None,
        details: "d".into(),
        suggested_fix: "f".into(),
    };
    let rel = store
        .file_finding("worker-a", finding.clone(), Some(4242))
        .await
        .unwrap();
    let text = tokio::fs::read_to_string(store.abs(&rel)).await.unwrap();
    assert!(text.contains("**Tokens**: 4242"), "{text}");

    // Without a known total the line is omitted rather than zero.
    let rel = store.file_finding("worker-a", finding, None).await.unwrap();
    let text = tokio::fs::read_to_string(store.abs(&rel)).await.unwrap();
    assert!(!text.contains("**Tokens**"), "{text}");
}

/// Upstream #3847: the sender's tokens ride the inbox frontmatter and
/// round-trip through `read_inbox`.
#[tokio::test]
async fn send_records_tokens_and_read_inbox_round_trips_them() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_with_record_dirs(dir.path()).await;
    store
        .save(&state_with(vec![entry("worker-a", "agent-0")]))
        .await
        .unwrap();

    store
        .send(
            "worker-a",
            TowerSendInput {
                to: TOWER_NAME.into(),
                subject: "heads up".into(),
                body: "b".into(),
                scope: None,
                action: None,
                consent_ref: None,
            },
            Some(777),
        )
        .await
        .unwrap();

    let items = store.read_inbox(TOWER_NAME, 10).await.unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].tokens, Some(777));
}

#[test]
fn resolve_caller_name_prefers_the_latest_registration() {
    let store = TowerStore::new(std::env::temp_dir());
    let state = state_with(vec![
        entry("worker-a", "agent-0"),
        entry("worker-b", "agent-0"),
    ]);
    assert_eq!(
        store.resolve_caller_name(&state, "agent-0").unwrap(),
        "worker-b"
    );
    assert_eq!(
        store.resolve_caller_name(&state, "main").unwrap(),
        TOWER_NAME
    );
    assert!(store.resolve_caller_name(&state, "agent-9").is_err());
}

#[tokio::test]
async fn register_agent_retires_entries_with_the_same_agent_id() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path()).await;
    store
        .save(&state_with(vec![entry("worker-a", "agent-0")]))
        .await
        .unwrap();

    store
        .register_agent(entry("worker-b", "agent-0"))
        .await
        .unwrap();

    let state = store.load().await.unwrap();
    assert_eq!(state.roster.agents.len(), 1);
    assert_eq!(state.roster.agents[0].name, "worker-b");
    assert_eq!(
        store.resolve_caller_name(&state, "agent-0").unwrap(),
        "worker-b"
    );
}

#[tokio::test]
async fn register_agent_still_rejects_a_duplicate_name() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path()).await;
    store
        .save(&state_with(vec![entry("worker-a", "agent-0")]))
        .await
        .unwrap();

    let error = store
        .register_agent(entry("worker-a", "agent-1"))
        .await
        .unwrap_err();
    assert!(error.contains("already registered"), "got: {error}");
    // The rejected registration must not have retired the live entry.
    let state = store.load().await.unwrap();
    assert_eq!(state.roster.agents.len(), 1);
    assert_eq!(state.roster.agents[0].agent_id, "agent-0");
}

#[tokio::test]
async fn mark_agent_dead_flags_the_entry_and_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path()).await;
    store
        .save(&state_with(vec![entry("worker-a", "agent-0")]))
        .await
        .unwrap();

    store.mark_agent_dead("agent-0").await.unwrap();
    let state = store.load().await.unwrap();
    assert_eq!(state.roster.agents[0].status.as_deref(), Some("dead"));

    // Marking again is a no-op, not an error or a duplicate flag.
    store.mark_agent_dead("agent-0").await.unwrap();
    let state = store.load().await.unwrap();
    assert_eq!(state.roster.agents[0].status.as_deref(), Some("dead"));

    // An unknown or already-retired agent id is silently ignored.
    store.mark_agent_dead("agent-missing").await.unwrap();
}

/// [`state_with`] takes a roster; this variant takes missions (the tower
/// invariants under test are all mission-level). `base` is explicit
/// because `git init` names the first branch `master` or `main`
/// depending on the host's config, and the diff check resolves it.
fn state_with_missions(base: &str, missions: Vec<TowerMission>) -> TowerState {
    TowerState {
        base: base.into(),
        missions,
        ..state_with(Vec::new())
    }
}

/// A reviewer roster entry for the given branch (the review cap counts
/// rounds per assigned reviewer).
fn review_entry(name: &str, target: &str) -> TowerRosterEntry {
    TowerRosterEntry {
        kind: crate::tools::tower::types::TowerAgentKind::Reviewer,
        review_target: Some(target.into()),
        review_mission_id: None,
        ..entry(name, "agent-0")
    }
}

/// A git repo with one commit, plus its actual first-branch name (the
/// completion diff check resolves `state.base` against the repo).
async fn repo_with_commit(root: &Path) -> String {
    git::git(root, &["init", "-q"]).await.unwrap();
    let _ = git::git(root, &["config", "user.email", "t@e.test"]).await;
    let _ = git::git(root, &["config", "user.name", "t"]).await;
    std::fs::write(root.join("f.txt"), "base").unwrap();
    git::git(root, &["add", "."]).await.unwrap();
    git::git(root, &["commit", "-qm", "init"]).await.unwrap();
    git::git(root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .await
        .unwrap()
}

fn mission(id: &str, branch: &str, status: TowerMissionStatus) -> TowerMission {
    TowerMission {
        id: id.into(),
        title: format!("Mission {id}"),
        slug: format!("mission-{id}"),
        kind: crate::tools::tower::types::TowerMissionKind::Build,
        scope: vec!["src/**".into()],
        branch: branch.into(),
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

/// Merging a branch whose only mission record is closed (merged or
/// abandoned) is refused — a merge never flips a historical mission's
/// state (v2 #3648).
#[tokio::test]
async fn merge_refuses_a_branch_whose_only_mission_record_is_closed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git::git(root, &["init", "-q"]).await.unwrap();
    let _ = git::git(root, &["config", "user.email", "t@e.test"]).await;
    let _ = git::git(root, &["config", "user.name", "t"]).await;
    std::fs::write(root.join("f.txt"), "base").unwrap();
    git::git(root, &["add", "."]).await.unwrap();
    git::git(root, &["commit", "-qm", "init"]).await.unwrap();
    let store = store_in(root).await;
    // `init` normally creates the comms tree; the test saves state
    // directly, so the log directory must exist for merge's blocking
    // append_log calls.
    tokio::fs::create_dir_all(store.abs(crate::tools::tower::paths::LOG_DIR))
        .await
        .unwrap();
    let mut state = state_with(Vec::new());
    state
        .missions
        .push(mission("M1", "feat/done", TowerMissionStatus::Merged));
    state
        .missions
        .push(mission("M2", "feat/gone", TowerMissionStatus::Abandoned));
    store.save(&state).await.unwrap();

    let err = store.merge("feat/done").await.unwrap_err();
    assert!(err.contains("closed"), "got: {err}");
    assert!(err.contains("fresh mission"), "got: {err}");

    let err = store.merge("feat/gone").await.unwrap_err();
    assert!(err.contains("closed"), "got: {err}");

    // No mission for the branch at all still errors distinctly.
    let err = store.merge("feat/unknown").await.unwrap_err();
    assert!(err.contains("no tower mission owns branch"), "got: {err}");
}

/// v2 #3976: the completion invariants. A real git repo with a build
/// mission whose branch has a commit passes; the refusals name the open
/// tasks, the missing branch, and the zero-diff case.
#[tokio::test]
async fn completion_refuses_open_tasks_missing_branch_and_zero_diff() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let base_branch = repo_with_commit(root).await;
    let head = git::git(root, &["rev-parse", "HEAD"]).await.unwrap();
    let store = store_with_record_dirs(root).await;

    // (a) open task blocks completion even on a branch that exists.
    let mut with_task = mission("M1", "feat/open", TowerMissionStatus::Active);
    with_task.tasks = vec![TowerMissionTask {
        text: "write the thing".into(),
        done: false,
        dropped: false,
    }];
    // The branch is created so the refusal is about the task, not the branch.
    git::git(root, &["branch", "feat/open"]).await.unwrap();
    store
        .save(&state_with_missions(&base_branch, vec![with_task.clone()]))
        .await
        .unwrap();
    let err = store
        .update_mission(
            TOWER_NAME,
            "M1",
            TowerMissionPatch {
                status: Some(TowerMissionStatus::Completed),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(err.contains("open task"), "got: {err}");
    assert!(
        err.contains("task_done") && err.contains("task_drop"),
        "got: {err}"
    );
    // The refused transition left the state untouched.
    assert_eq!(
        store.load().await.unwrap().missions[0].status,
        TowerMissionStatus::Active
    );

    // (b) a missing branch is refused once the tasks are clear.
    let mut no_branch = with_task.clone();
    no_branch.tasks = Vec::new();
    no_branch.status = TowerMissionStatus::Active;
    no_branch.branch = "feat/never-created".into();
    store
        .save(&state_with_missions(&base_branch, vec![no_branch.clone()]))
        .await
        .unwrap();
    let err = store
        .update_mission(
            TOWER_NAME,
            "M1",
            TowerMissionPatch {
                status: Some(TowerMissionStatus::Completed),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(err.contains("is not available"), "got: {err}");
    assert!(err.contains("abandoned"), "got: {err}");

    // (c) a branch that exists but has no diff against the base is refused.
    let mut zero_diff = no_branch.clone();
    zero_diff.branch = "feat/no-diff".into();
    git::git(root, &["branch", "feat/no-diff"]).await.unwrap();
    store
        .save(&state_with_missions(&base_branch, vec![zero_diff]))
        .await
        .unwrap();
    let err = store
        .update_mission(
            TOWER_NAME,
            "M1",
            TowerMissionPatch {
                status: Some(TowerMissionStatus::Completed),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(err.contains("no changes"), "got: {err}");

    // (d) a branch with a real diff completes, and a survey mission with
    // no diff is exempt.
    let mut landed = no_branch.clone();
    landed.branch = "feat/landed".into();
    git::git(root, &["branch", "feat/landed"]).await.unwrap();
    git::git(root, &["checkout", "-q", "feat/landed"])
        .await
        .unwrap();
    std::fs::write(root.join("f.txt"), "landed").unwrap();
    git::git(root, &["add", "."]).await.unwrap();
    git::git(root, &["commit", "-qm", "work"]).await.unwrap();
    git::git(root, &["checkout", "-q", &head]).await.unwrap();
    store
        .save(&state_with_missions(&base_branch, vec![landed]))
        .await
        .unwrap();
    let completed = store
        .update_mission(
            TOWER_NAME,
            "M1",
            TowerMissionPatch {
                status: Some(TowerMissionStatus::Completed),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(completed.status, TowerMissionStatus::Completed);

    let mut survey = no_branch.clone();
    survey.kind = crate::tools::tower::types::TowerMissionKind::Survey;
    survey.status = TowerMissionStatus::Active;
    store
        .save(&state_with_missions(&base_branch, vec![survey]))
        .await
        .unwrap();
    let completed = store
        .update_mission(
            TOWER_NAME,
            "M1",
            TowerMissionPatch {
                status: Some(TowerMissionStatus::Completed),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(completed.status, TowerMissionStatus::Completed);
}

/// v2 #3976: `task_drop` requires a reason, records the drop in the
/// mission notes and the activity log, and unblocks completion.
#[tokio::test]
async fn task_drop_requires_a_reason_and_records_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let base_branch = repo_with_commit(root).await;
    let store = store_with_record_dirs(root).await;

    let mut m = mission("M1", "feat/x", TowerMissionStatus::Active);
    m.tasks = vec![
        TowerMissionTask {
            text: "keep this".into(),
            done: false,
            dropped: false,
        },
        TowerMissionTask {
            text: "drop that".into(),
            done: false,
            dropped: false,
        },
    ];
    store
        .save(&state_with_missions(&base_branch, vec![m]))
        .await
        .unwrap();

    // A reasonless drop is refused and changes nothing.
    let err = store
        .update_mission(
            TOWER_NAME,
            "M1",
            TowerMissionPatch {
                task_drop: Some(crate::tools::tower::types::TowerTaskDrop {
                    text: "drop that".into(),
                    reason: None,
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(err.contains("requires a reason"), "got: {err}");
    let reloaded = store.load().await.unwrap();
    assert!(!reloaded.missions[0].tasks[1].dropped);
    assert!(reloaded.missions[0].notes.is_empty());

    // With a reason the task is marked and audited in notes + activity log.
    let updated = store
        .update_mission(
            TOWER_NAME,
            "M1",
            TowerMissionPatch {
                task_drop: Some(crate::tools::tower::types::TowerTaskDrop {
                    text: "drop that".into(),
                    reason: Some("turned out unnecessary".into()),
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(updated.tasks[1].dropped);
    assert!(!updated.tasks[0].dropped);
    assert!(
        updated
            .notes
            .iter()
            .any(|n| n.contains("dropped task \"drop that\"")
                && n.contains("turned out unnecessary")),
        "{:?}",
        updated.notes
    );
    let log = tokio::fs::read_to_string(store.abs(ACTIVITY_LOG))
        .await
        .unwrap();
    assert!(log.contains("task_drop"), "{log}");

    // A dropped task no longer blocks completion (`keep this` still does).
    let err = store
        .update_mission(
            TOWER_NAME,
            "M1",
            TowerMissionPatch {
                status: Some(TowerMissionStatus::Completed),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(err.contains("keep this"), "got: {err}");
    assert!(!err.contains("drop that"), "got: {err}");
}

/// v2 #3976: after `MAX_REVIEW_ROUNDS` rounds the same reviewer is
/// refused, with the redirect guidance in the error.
///
/// The loop below reads the constant, so a test that only counted rounds
/// would still pass if the value changed. The literal pins the contract
/// v2 fixes: five rounds per reviewer, then a redirect.
#[test]
fn the_review_round_cap_is_five() {
    assert_eq!(MAX_REVIEW_ROUNDS, 5);
}

/// The review file name is a lossy key — `feat/x` and `feat-x` slug to the
/// same prefix — so a review of one branch must never be read as a review of
/// the other. The merge gate reads `latest_review`, which is where a
/// collision is dangerous: it could wave an unreviewed branch through.
#[tokio::test]
async fn a_colliding_branch_slug_does_not_stand_in_for_another_branch() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    repo_with_commit(root).await;
    git::git(root, &["branch", "feat/x"]).await.unwrap();
    git::git(root, &["branch", "feat-x"]).await.unwrap();
    let store = store_with_record_dirs(root).await;
    store
        .save(&state_with(vec![review_entry("reviewer-1", "feat/x")]))
        .await
        .unwrap();
    store
        .submit_review(
            "reviewer-1",
            TowerReviewInput {
                target: "feat/x".into(),
                status: "clean".into(),
                merge: "merge".into(),
                findings: "f".into(),
                checks: None,
                decision: "d".into(),
            },
            None,
        )
        .await
        .unwrap();

    assert_eq!(
        store.reviews_for("feat/x").await.unwrap().len(),
        1,
        "the branch's own review is found"
    );
    assert!(
        store.reviews_for("feat-x").await.unwrap().is_empty(),
        "a colliding slug must not inherit the other branch's review"
    );
    assert!(
        store.latest_review("feat-x").await.unwrap().is_none(),
        "the merge gate must see no review for the unreviewed branch"
    );
}

#[tokio::test]
async fn review_rounds_cap_after_the_max() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    repo_with_commit(root).await;
    git::git(root, &["branch", "feat/r"]).await.unwrap();
    let store = store_with_record_dirs(root).await;
    // Two assigned reviewers: the cap counts rounds per reviewer, so the
    // second one starts fresh on the same branch.
    store
        .save(&state_with(vec![
            review_entry("reviewer-1", "feat/r"),
            review_entry("reviewer-2", "feat/r"),
        ]))
        .await
        .unwrap();

    let review = || TowerReviewInput {
        target: "feat/r".into(),
        status: "clean".into(),
        merge: "merge".into(),
        findings: "f".into(),
        checks: None,
        decision: "d".into(),
    };

    for expected in 1..=MAX_REVIEW_ROUNDS {
        store
            .submit_review("reviewer-1", review(), None)
            .await
            .unwrap_or_else(|error| panic!("round {expected} must be accepted: {error}"));
    }
    let err = store
        .submit_review("reviewer-1", review(), None)
        .await
        .unwrap_err();
    assert!(err.contains("not converging"), "got: {err}");
    assert!(
        err.contains("reassign") && err.contains("split"),
        "got: {err}"
    );
    // A different reviewer on the same branch is still welcome.
    store
        .submit_review("reviewer-2", review(), None)
        .await
        .unwrap();
}

/// v2 #3976: a non-clean review flips a completed mission back to active
/// so the rework loop can resume, and the flip is logged.
#[tokio::test]
async fn a_non_clean_review_reopens_a_completed_mission() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    repo_with_commit(root).await;
    git::git(root, &["branch", "feat/rework"]).await.unwrap();
    let store = store_with_record_dirs(root).await;
    // A reviewer assigned to the mission's branch: the non-clean verdict
    // resolves the mission by the caller's recorded target.
    let mut state = state_with(vec![review_entry("reviewer-1", "feat/rework")]);
    state
        .missions
        .push(mission("M1", "feat/rework", TowerMissionStatus::Completed));
    store.save(&state).await.unwrap();

    store
        .submit_review(
            "reviewer-1",
            TowerReviewInput {
                target: "feat/rework".into(),
                status: "p1-2items".into(),
                merge: "fix-then-merge".into(),
                findings: "f".into(),
                checks: None,
                decision: "d".into(),
            },
            None,
        )
        .await
        .unwrap();
    let reloaded = store.load().await.unwrap();
    assert_eq!(
        reloaded.missions[0].status,
        TowerMissionStatus::Active,
        "a non-clean verdict reopens the completed mission"
    );
    let log = tokio::fs::read_to_string(store.abs(ACTIVITY_LOG))
        .await
        .unwrap();
    assert!(log.contains("mission.rework"), "{log}");
}

/// Only a missing state file means "uninitialized". An existing but
/// unstattable one used to read as a fresh workspace, and `init` then
/// overwrote the foreign roster with a new one.
#[tokio::test]
async fn is_initialized_only_treats_a_missing_state_file_as_uninitialized() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path()).await;
    assert!(!store.is_initialized().await.unwrap());

    store.save(&state_with(Vec::new())).await.unwrap();
    assert!(store.is_initialized().await.unwrap());

    // A NUL byte makes the path unstattable for a reason other than
    // "missing" on every platform.
    let unstattable = TowerStore::new(dir.path().join("bad\0root"));
    assert!(unstattable.is_initialized().await.is_err());
}
