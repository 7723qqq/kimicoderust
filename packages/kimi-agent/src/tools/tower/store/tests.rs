use super::*;
use crate::tools::tower::git;
use crate::tools::tower::types::{
    TowerAgentKind, TowerMergeStatus, TowerReanchorRelation, TowerRebaseMissionStatus,
};

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
async fn file_finding_omits_legacy_caller_tokens() {
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
        .file_finding("worker-a", finding.clone())
        .await
        .unwrap();
    let text = tokio::fs::read_to_string(store.abs(&rel)).await.unwrap();
    assert!(!text.contains("**Tokens**"), "{text}");

    // Without a known total the line is omitted rather than zero.
    let rel = store.file_finding("worker-a", finding).await.unwrap();
    let text = tokio::fs::read_to_string(store.abs(&rel)).await.unwrap();
    assert!(!text.contains("**Tokens**"), "{text}");
}

/// Upstream #3847: the sender's tokens ride the inbox frontmatter and
/// round-trip through `read_inbox`.
#[tokio::test]
async fn send_omits_legacy_tokens_from_the_inbox_record() {
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
        )
        .await
        .unwrap();

    let items = store.read_inbox(TOWER_NAME, 10).await.unwrap();
    assert_eq!(items.len(), 1);
    let raw = tokio::fs::read_to_string(store.abs(&items[0].file))
        .await
        .unwrap();
    assert!(!raw.contains("tokens:"), "{raw}");
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
async fn spawned_registration_persists_roster_and_mission_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_with_record_dirs(dir.path()).await;
    store
        .save(&state_with_missions(
            "main",
            vec![mission("M1", "feat/atomic", TowerMissionStatus::Active)],
        ))
        .await
        .unwrap();
    let mut spawned = entry("worker-a", "agent-a");
    spawned.mission_id = Some("M99".into());

    let error = store
        .register_spawned_agent(spawned.clone(), Some(("M99", "worker-a")))
        .await
        .unwrap_err();
    assert!(error.contains("unknown mission"), "{error}");
    assert!(store.load().await.unwrap().roster.agents.is_empty());

    spawned.mission_id = Some("M1".into());
    store
        .register_spawned_agent(spawned, Some(("M1", "worker-a")))
        .await
        .unwrap();
    let state = store.load().await.unwrap();
    assert_eq!(state.roster.agents.len(), 1);
    assert_eq!(state.missions[0].owner.as_deref(), Some("worker-a"));
    assert_eq!(state.missions[0].status, TowerMissionStatus::Active);
}

#[tokio::test]
async fn agent_death_records_cause_and_revive_clears_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_with_record_dirs(dir.path()).await;
    store
        .save(&state_with(vec![entry("worker-a", "agent-0")]))
        .await
        .unwrap();

    store
        .mark_agent_dead("agent-0", "failed", Some("boom"), Some("session-1"))
        .await
        .unwrap();
    let state = store.load().await.unwrap();
    let died = &state.roster.agents[0];
    assert_eq!(died.status.as_deref(), Some("dead"));
    assert_eq!(died.death_status.as_deref(), Some("failed"));
    assert_eq!(died.death_reason.as_deref(), Some("boom"));
    assert!(died.died_at.is_some());

    assert!(
        !store
            .clear_agent_died("agent-0", Some("another-session"))
            .await
            .unwrap()
    );
    assert!(
        store.load().await.unwrap().roster.agents[0]
            .died_at
            .is_some()
    );

    assert!(
        store
            .clear_agent_died("agent-0", Some("session-1"))
            .await
            .unwrap()
    );
    let revived = store.load().await.unwrap();
    let agent = &revived.roster.agents[0];
    assert_eq!(agent.status, None);
    assert_eq!(agent.died_at, None);
    assert_eq!(agent.death_status, None);
    assert_eq!(agent.death_reason, None);
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

async fn repo_with_branch_diff(root: &Path, branch: &str) -> String {
    let base = repo_with_commit(root).await;
    git::git(root, &["checkout", "-qb", branch]).await.unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/feature.txt"), branch).unwrap();
    git::git(root, &["add", "."]).await.unwrap();
    git::git(root, &["commit", "-qm", "mission work"])
        .await
        .unwrap();
    git::git(root, &["checkout", "-q", &base]).await.unwrap();
    let exclude = root.join(".git/info/exclude");
    let existing = std::fs::read_to_string(&exclude).unwrap_or_default();
    std::fs::write(&exclude, format!("{existing}\n.tower/\n")).unwrap();
    base
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
            .submit_review("reviewer-1", review())
            .await
            .unwrap_or_else(|error| panic!("round {expected} must be accepted: {error}"));
    }
    let err = store
        .submit_review("reviewer-1", review())
        .await
        .unwrap_err();
    assert!(err.contains("not converging"), "got: {err}");
    assert!(
        err.contains("reassign") && err.contains("split"),
        "got: {err}"
    );
    // A different reviewer on the same branch is still welcome.
    store.submit_review("reviewer-2", review()).await.unwrap();
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

#[tokio::test]
async fn completion_validates_git_before_acquiring_the_state_lock() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    repo_with_commit(root).await;
    let store = TowerStore::with_options(
        root,
        TowerStoreOptions {
            state_lock_timeout_ms: 100,
            state_lock_poll_ms: 1,
        },
    );
    tokio::fs::create_dir_all(store.abs(INBOX_DIR))
        .await
        .unwrap();
    tokio::fs::create_dir_all(store.abs(LOG_DIR)).await.unwrap();
    let mut worker = entry("worker-a", "agent-a");
    worker.mission_id = Some("M1".into());
    let mut state = state_with(vec![worker]);
    state
        .missions
        .push(mission("M1", "feat/missing", TowerMissionStatus::Active));
    store.save(&state).await.unwrap();
    let lock = store.abs(&format!("{STATE_FILE}.lock"));
    tokio::fs::write(
        &lock,
        format!(
            "pid={} since=2026-01-01T00:00:00Z token=live",
            std::process::id()
        ),
    )
    .await
    .unwrap();

    let error = store.complete("worker-a", "report").await.unwrap_err();

    assert!(error.contains("not available"), "{error}");
    assert!(!error.contains("timed out"), "{error}");
    tokio::fs::remove_file(lock).await.unwrap();
}

#[tokio::test]
async fn review_prepares_before_acquiring_the_state_lock() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    repo_with_commit(root).await;
    let store = TowerStore::with_options(
        root,
        TowerStoreOptions {
            state_lock_timeout_ms: 100,
            state_lock_poll_ms: 1,
        },
    );
    tokio::fs::create_dir_all(store.abs(STATE_FILE).parent().unwrap())
        .await
        .unwrap();
    store
        .save(&state_with(vec![review_entry(
            "reviewer-1",
            "feat/missing",
        )]))
        .await
        .unwrap();
    let lock = store.abs(&format!("{STATE_FILE}.lock"));
    tokio::fs::write(
        &lock,
        format!(
            "pid={} since=2026-01-01T00:00:00Z token=live",
            std::process::id()
        ),
    )
    .await
    .unwrap();

    let error = store
        .submit_review(
            "reviewer-1",
            TowerReviewInput {
                target: "feat/missing".into(),
                status: "clean".into(),
                merge: "merge".into(),
                findings: "f".into(),
                checks: None,
                decision: "d".into(),
            },
        )
        .await
        .unwrap_err();

    assert!(error.contains("git rev-parse"), "{error}");
    assert!(!error.contains("timed out"), "{error}");
    tokio::fs::remove_file(lock).await.unwrap();
}

#[tokio::test]
async fn prepared_commits_keep_retrying_beyond_eight_snapshot_conflicts() {
    let dir = tempfile::tempdir().unwrap();
    let store = TowerStore::with_options(
        dir.path(),
        TowerStoreOptions {
            state_lock_timeout_ms: 100,
            state_lock_poll_ms: 1,
        },
    );
    let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let prepared = attempts.clone();
    let result = store
        .run_prepared(
            move || {
                let attempts = prepared.clone();
                async move {
                    Ok::<_, String>(attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst))
                }
            },
            |value| async move {
                if value < 10 {
                    Err(PREPARATION_CONFLICT.to_string())
                } else {
                    Ok(value)
                }
            },
        )
        .await
        .unwrap();

    assert_eq!(result, 10);
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 11);
    assert_eq!(PREPARED_OPERATION_TIMEOUT_MS, 60_000);
    assert_eq!(PREPARED_OPERATION_MAX_BACKOFF_MS, 100);
}

#[tokio::test]
async fn preparation_snapshot_rejects_a_changed_state_file() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path()).await;
    store.save(&state_with(Vec::new())).await.unwrap();
    let snapshot = store.load_snapshot().await.unwrap();
    store
        .save(&state_with(vec![entry("worker-a", "agent-a")]))
        .await
        .unwrap();

    let error = store
        .assert_preparation_current(&snapshot, None, None)
        .await
        .unwrap_err();

    assert_eq!(error, PREPARATION_CONFLICT);
}

#[tokio::test]
async fn state_lock_reclaims_a_dead_holder() {
    let dir = tempfile::tempdir().unwrap();
    repo_with_commit(dir.path()).await;
    let store = TowerStore::with_options(
        dir.path(),
        TowerStoreOptions {
            state_lock_timeout_ms: 100,
            state_lock_poll_ms: 1,
        },
    );
    tokio::fs::create_dir_all(store.abs(STATE_FILE).parent().unwrap())
        .await
        .unwrap();
    store.save(&state_with(Vec::new())).await.unwrap();
    let lock = store.abs(&format!("{STATE_FILE}.lock"));
    tokio::fs::write(&lock, "pid=999999 since=2026-01-01T00:00:00Z token=stale")
        .await
        .unwrap();

    store
        .register_agent(entry("worker-a", "agent-0"))
        .await
        .unwrap();

    assert!(!lock.exists());
    assert_eq!(store.load().await.unwrap().roster.agents.len(), 1);
}

#[tokio::test]
async fn inbox_send_does_not_wait_for_the_state_lock() {
    let dir = tempfile::tempdir().unwrap();
    let store = TowerStore::with_options(
        dir.path(),
        TowerStoreOptions {
            state_lock_timeout_ms: 100,
            state_lock_poll_ms: 1,
        },
    );
    tokio::fs::create_dir_all(store.abs(INBOX_DIR))
        .await
        .unwrap();
    tokio::fs::create_dir_all(store.abs(LOG_DIR)).await.unwrap();
    store
        .save(&state_with(vec![entry("worker-a", "agent-a")]))
        .await
        .unwrap();
    let lock = store.abs(&format!("{STATE_FILE}.lock"));
    tokio::fs::write(
        &lock,
        format!(
            "pid={} since=2026-01-01T00:00:00Z token=live",
            std::process::id()
        ),
    )
    .await
    .unwrap();

    let sent = store
        .send_agent(
            "main",
            TowerSendInput {
                to: "worker-a".into(),
                subject: "lock free".into(),
                body: "x".into(),
                scope: None,
                action: None,
                consent_ref: None,
            },
        )
        .await
        .unwrap();

    assert_eq!(sent.message.item.subject, "lock free");
    assert!(lock.exists());
    tokio::fs::remove_file(lock).await.unwrap();
}

#[tokio::test]
async fn stale_lock_reaper_releases_a_crashed_git_ref_guard() {
    let dir = tempfile::tempdir().unwrap();
    repo_with_commit(dir.path()).await;
    let store = store_in(dir.path()).await;
    store.save(&state_with(Vec::new())).await.unwrap();
    let lock = store.abs(&format!("{STATE_FILE}.lock"));
    let reaper_ref = store.state_lock_reaper_ref(&lock).await.unwrap();
    let guard_token = git::git_with_input(
        dir.path(),
        &["hash-object", "-w", "--stdin"],
        "pid=999999 since=2026-01-01T00:00:00Z token=crashed-reaper",
    )
    .await
    .unwrap();
    git::git(dir.path(), &["update-ref", &reaper_ref, &guard_token, ""])
        .await
        .unwrap();
    tokio::fs::write(&lock, "pid=999999 since=2026-01-01T00:00:00Z token=stale")
        .await
        .unwrap();

    assert!(store.reap_stale_state_lock(&lock).await.unwrap());
    assert_eq!(
        git::try_git(
            dir.path(),
            &["rev-parse", "--verify", "--quiet", &reaper_ref],
        )
        .await,
        None
    );
}

#[tokio::test]
async fn state_lock_preserves_both_concurrent_registrations() {
    let dir = tempfile::tempdir().unwrap();
    repo_with_commit(dir.path()).await;
    let first = store_in(dir.path()).await;
    first.save(&state_with(Vec::new())).await.unwrap();
    let second = TowerStore::new(dir.path());

    let (a, b) = tokio::join!(
        first.register_agent(entry("worker-a", "agent-a")),
        second.register_agent(entry("worker-b", "agent-b")),
    );
    a.unwrap();
    b.unwrap();

    let state = first.load().await.unwrap();
    assert_eq!(state.roster.agents.len(), 2);
}

#[tokio::test]
async fn completion_is_refused_until_inbox_is_marked_read() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_with_record_dirs(dir.path()).await;
    let mut worker = entry("worker-a", "agent-0");
    worker.mission_id = Some("M1".into());
    let mut state = state_with(vec![worker]);
    let mut survey = mission("M1", "feat/survey", TowerMissionStatus::Active);
    survey.kind = TowerMissionKind::Survey;
    survey.owner = Some("worker-a".into());
    state.missions.push(survey);
    store.save(&state).await.unwrap();
    store
        .send(
            TOWER_NAME,
            TowerSendInput {
                to: "worker-a".into(),
                subject: "read me".into(),
                body: "new instruction".into(),
                scope: None,
                action: None,
                consent_ref: None,
            },
        )
        .await
        .unwrap();

    let patch = TowerMissionPatch {
        status: Some(TowerMissionStatus::Completed),
        ..Default::default()
    };
    let error = store
        .update_mission("worker-a", "M1", patch.clone())
        .await
        .unwrap_err();
    assert!(error.contains("unread inbox"), "{error}");

    let items = store.read_inbox("worker-a", 20).await.unwrap();
    store
        .mark_inbox_read("worker-a", items[0].sent_at.clone())
        .await
        .unwrap();
    store.update_mission("worker-a", "M1", patch).await.unwrap();
    let raw: serde_json::Value = serde_json::from_str(
        &tokio::fs::read_to_string(store.abs(STATE_FILE))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        raw["roster"]["agents"][0]["lastInboxReadAt"].as_str(),
        Some(items[0].sent_at.as_str())
    );
}

#[tokio::test]
async fn user_stopped_worker_cannot_be_reassigned() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_with_record_dirs(dir.path()).await;
    let mut worker = entry("worker-a", "agent-0");
    worker.mission_id = Some("M1".into());
    worker.died_at = Some("2026-09-16T00:00:00Z".into());
    worker.death_status = Some("killed".into());
    worker.death_reason = Some("user cancelled".into());
    worker.status = Some("dead".into());
    let mut state = state_with(vec![worker]);
    let mut owned = mission("M1", "feat/owned", TowerMissionStatus::Active);
    owned.owner = Some("worker-a".into());
    state.missions.push(owned);
    store.save(&state).await.unwrap();

    let error = store
        .update_mission(
            TOWER_NAME,
            "M1",
            TowerMissionPatch {
                owner: Some("worker-b".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(error.contains("stopped by the user"), "{error}");
    assert_eq!(
        store.load().await.unwrap().missions[0].owner.as_deref(),
        Some("worker-a")
    );
}

#[tokio::test]
async fn teardown_supports_dry_run_live_agent_and_exclude() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let base = repo_with_commit(root).await;
    let store = store_with_record_dirs(root).await;
    let mut worker = entry("worker-a", "agent-0");
    worker.worktree = Some("wt-M1".into());
    worker.mission_id = Some("M1".into());
    let mut state = state_with_missions(
        &base,
        vec![mission("M1", "feat/teardown", TowerMissionStatus::Merged)],
    );
    state.roster.agents.push(worker);
    store.save(&state).await.unwrap();
    store
        .add_worktree("wt-M1", "feat/teardown", &base)
        .await
        .unwrap();
    let worktree = store.abs(&format!("{WORKTREES_DIR}/wt-M1"));
    assert!(worktree.exists());

    let report = store
        .teardown(TowerTeardownOptions {
            dry_run: true,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        report.iter().any(|line| line.contains("would remove")),
        "{report:?}"
    );
    assert!(worktree.exists());

    let report = store
        .teardown(TowerTeardownOptions {
            force: true,
            live_agent_ids: std::collections::HashSet::from(["agent-0".to_string()]),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        report.iter().any(|line| line.contains("live agent")),
        "{report:?}"
    );
    assert!(worktree.exists());

    let report = store
        .teardown(TowerTeardownOptions {
            exclude: vec!["wt-M1".into()],
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        report.iter().any(|line| line.contains("excluded")),
        "{report:?}"
    );
    assert!(worktree.exists());

    store
        .teardown(TowerTeardownOptions::default())
        .await
        .unwrap();
    assert!(!worktree.exists());
}

#[tokio::test]
async fn overlapping_build_scopes_gain_an_automatic_dependency() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_with_record_dirs(dir.path()).await;
    store.save(&state_with(Vec::new())).await.unwrap();
    let input = |title: &str, scope: &str| TowerPlanInput {
        title: title.into(),
        scope: vec![scope.into()],
        tasks: None,
        deps: None,
        kind: None,
    };

    let planned = store
        .plan(&[input("base", "src/**"), input("overlapping", "src/app/**")])
        .await
        .unwrap();

    assert_eq!(planned[1].deps, vec!["M1"]);
    assert!(
        planned[1]
            .notes
            .iter()
            .any(|note| note.contains("auto dependency") && note.contains("M1")),
        "{:?}",
        planned[1].notes
    );
}

#[tokio::test]
async fn widening_into_an_existing_dependents_scope_does_not_create_a_cycle() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_with_record_dirs(dir.path()).await;
    store.save(&state_with(Vec::new())).await.unwrap();
    let planned = store
        .plan(&[
            TowerPlanInput {
                title: "first".into(),
                scope: vec!["src/a/**".into()],
                tasks: None,
                deps: None,
                kind: None,
            },
            TowerPlanInput {
                title: "second".into(),
                scope: vec!["src/b/**".into()],
                tasks: None,
                deps: Some(vec!["M1".into()]),
                kind: None,
            },
        ])
        .await
        .unwrap();
    assert_eq!(planned[1].deps, vec!["M1"]);

    let widened = store
        .update_mission(
            TOWER_NAME,
            "M1",
            TowerMissionPatch {
                scope: Some(vec!["src/b/**".into()]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(widened.deps.is_empty(), "must not add M1 -> M2");
    assert_eq!(
        store.load().await.unwrap().missions[1].deps,
        vec!["M1"],
        "the existing M2 -> M1 edge already serializes the overlap"
    );
}

#[tokio::test]
async fn generic_mission_patch_cannot_mark_a_mission_merged() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_with_record_dirs(dir.path()).await;
    store
        .save(&state_with_missions(
            "main",
            vec![mission("M1", "feat/x", TowerMissionStatus::Completed)],
        ))
        .await
        .unwrap();

    let error = store
        .update_mission(
            TOWER_NAME,
            "M1",
            TowerMissionPatch {
                status: Some(TowerMissionStatus::Merged),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(error.contains("TowerMerge"), "{error}");
    assert_eq!(
        store.load().await.unwrap().missions[0].status,
        TowerMissionStatus::Completed
    );
}

#[tokio::test]
async fn terminal_missions_reject_status_changes_and_new_blockers() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_with_record_dirs(dir.path()).await;
    store
        .save(&state_with_missions(
            "main",
            vec![mission("M1", "feat/x", TowerMissionStatus::Merged)],
        ))
        .await
        .unwrap();

    for patch in [
        TowerMissionPatch {
            status: Some(TowerMissionStatus::Active),
            ..Default::default()
        },
        TowerMissionPatch {
            blocker: Some("late blocker".into()),
            ..Default::default()
        },
    ] {
        let error = store
            .update_mission(TOWER_NAME, "M1", patch)
            .await
            .unwrap_err();
        assert!(error.contains("terminal"), "{error}");
    }
    assert_eq!(
        store.load().await.unwrap().missions[0].status,
        TowerMissionStatus::Merged
    );
}

#[tokio::test]
async fn completion_refuses_a_mission_with_blockers() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_with_record_dirs(dir.path()).await;
    let mut blocked = mission("M1", "feat/survey", TowerMissionStatus::Blocked);
    blocked.kind = TowerMissionKind::Survey;
    blocked.blockers.push("waiting for an answer".into());
    store
        .save(&state_with_missions("main", vec![blocked]))
        .await
        .unwrap();

    let error = store
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
    assert!(error.contains("blocker"), "{error}");
    assert_eq!(
        store.load().await.unwrap().missions[0].status,
        TowerMissionStatus::Blocked
    );
}

#[tokio::test]
async fn survey_completion_refuses_a_nonzero_diff() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let base = repo_with_commit(root).await;
    git::git(root, &["checkout", "-qb", "feat/survey"])
        .await
        .unwrap();
    std::fs::write(root.join("survey.txt"), "not read only").unwrap();
    git::git(root, &["add", "."]).await.unwrap();
    git::git(root, &["commit", "-qm", "survey diff"])
        .await
        .unwrap();
    git::git(root, &["checkout", "-q", &base]).await.unwrap();

    let store = store_with_record_dirs(root).await;
    let mut survey = mission("M1", "feat/survey", TowerMissionStatus::Active);
    survey.kind = TowerMissionKind::Survey;
    store
        .save(&state_with_missions(&base, vec![survey]))
        .await
        .unwrap();
    let error = store
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
    assert!(
        error.contains("read-only") || error.contains("zero-diff"),
        "{error}"
    );
    assert_eq!(
        store.load().await.unwrap().missions[0].status,
        TowerMissionStatus::Active
    );
}

#[tokio::test]
async fn shared_branch_merge_requires_a_review_for_the_open_mission() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let base = repo_with_branch_diff(root, "feat/shared").await;
    let store = store_with_record_dirs(root).await;
    let mut reviewer = review_entry("reviewer-1", "feat/shared");
    reviewer.review_mission_id = Some("M1".into());
    let mut state = state_with_missions(
        &base,
        vec![
            mission("M1", "feat/shared", TowerMissionStatus::Abandoned),
            mission("M2", "feat/shared", TowerMissionStatus::Completed),
        ],
    );
    state.roster.agents.push(reviewer);
    store.save(&state).await.unwrap();
    store
        .submit_review(
            "reviewer-1",
            TowerReviewInput {
                target: "feat/shared".into(),
                status: "clean".into(),
                merge: "merge".into(),
                findings: "f".into(),
                checks: None,
                decision: "d".into(),
            },
        )
        .await
        .unwrap();

    let error = store.merge("feat/shared").await.unwrap_err();
    assert!(error.contains("no review"), "{error}");
    let state = store.load().await.unwrap();
    assert_eq!(state.missions[0].status, TowerMissionStatus::Abandoned);
    assert_eq!(state.missions[1].status, TowerMissionStatus::Completed);
}

#[tokio::test]
async fn shared_branch_merge_rejects_an_unbound_legacy_review() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let base = repo_with_branch_diff(root, "feat/shared").await;
    let store = store_with_record_dirs(root).await;
    let mut state = state_with_missions(
        &base,
        vec![
            mission("M1", "feat/shared", TowerMissionStatus::Abandoned),
            mission("M2", "feat/shared", TowerMissionStatus::Completed),
        ],
    );
    state
        .roster
        .agents
        .push(review_entry("reviewer-1", "feat/shared"));
    store.save(&state).await.unwrap();
    store
        .submit_review(
            "reviewer-1",
            TowerReviewInput {
                target: "feat/shared".into(),
                status: "clean".into(),
                merge: "merge".into(),
                findings: "f".into(),
                checks: None,
                decision: "d".into(),
            },
        )
        .await
        .unwrap();

    let error = store.merge("feat/shared").await.unwrap_err();
    assert!(
        error.contains("mission") && error.contains("review"),
        "{error}"
    );
}

#[tokio::test]
async fn clean_review_must_explicitly_approve_merge() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let base = repo_with_branch_diff(root, "feat/hold").await;
    let store = store_with_record_dirs(root).await;
    let mut reviewer = review_entry("reviewer-1", "feat/hold");
    reviewer.review_mission_id = Some("M1".into());
    let mut state = state_with_missions(
        &base,
        vec![mission("M1", "feat/hold", TowerMissionStatus::Completed)],
    );
    state.roster.agents.push(reviewer);
    store.save(&state).await.unwrap();
    store
        .submit_review(
            "reviewer-1",
            TowerReviewInput {
                target: "feat/hold".into(),
                status: "clean".into(),
                merge: "hold".into(),
                findings: "f".into(),
                checks: None,
                decision: "d".into(),
            },
        )
        .await
        .unwrap();

    let error = store.merge("feat/hold").await.unwrap_err();
    assert!(error.contains("hold"), "{error}");
    let log = tokio::fs::read_to_string(store.abs(ACTIVITY_LOG))
        .await
        .unwrap();
    assert!(log.contains("reason=review-hold"), "{log}");
}

#[tokio::test]
async fn merge_requires_the_mission_to_be_completed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let base = repo_with_branch_diff(root, "feat/active").await;
    let store = store_with_record_dirs(root).await;
    let mut reviewer = review_entry("reviewer-1", "feat/active");
    reviewer.review_mission_id = Some("M1".into());
    let mut state = state_with_missions(
        &base,
        vec![mission("M1", "feat/active", TowerMissionStatus::Active)],
    );
    state.roster.agents.push(reviewer);
    store.save(&state).await.unwrap();
    store
        .submit_review(
            "reviewer-1",
            TowerReviewInput {
                target: "feat/active".into(),
                status: "clean".into(),
                merge: "merge".into(),
                findings: "f".into(),
                checks: None,
                decision: "d".into(),
            },
        )
        .await
        .unwrap();

    let error = store.merge("feat/active").await.unwrap_err();
    assert!(error.contains("completed"), "{error}");
    let log = tokio::fs::read_to_string(store.abs(ACTIVITY_LOG))
        .await
        .unwrap();
    assert!(log.contains("reason=not-completed"), "{log}");
}

#[tokio::test]
async fn latest_review_follows_sequence_instead_of_round_or_mtime() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_with_record_dirs(dir.path()).await;
    for (reviewer, round, seq) in [("old-high-round", 9, 1), ("new-low-round", 1, 2)] {
        let frontmatter = render_frontmatter(&[
            ("date", "2026-01-01"),
            ("reviewer", reviewer),
            ("target", "feat/x"),
            ("round", &round.to_string()),
            ("status", "clean"),
            ("merge", "merge"),
            ("reviewed_commit", "abc"),
            ("seq", &seq.to_string()),
        ])
        .unwrap();
        tokio::fs::write(
            store.abs(&format!(
                "{REVIEWS_DIR}/{}",
                review_file_name("feat/x", reviewer, round)
            )),
            format!("{frontmatter}\n\nbody\n"),
        )
        .await
        .unwrap();
    }

    let latest = store.latest_review("feat/x").await.unwrap().unwrap();
    assert_eq!(latest.reviewer, "new-low-round");
}

#[tokio::test]
async fn complete_persists_mission_and_deduplicates_its_standard_notification() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_with_record_dirs(dir.path()).await;
    let mut worker = entry("worker-a", "agent-0");
    worker.mission_id = Some("M1".into());
    let mut survey = mission("M1", "feat/survey", TowerMissionStatus::Active);
    survey.kind = TowerMissionKind::Survey;
    survey.owner = Some("worker-a".into());
    let mut state = state_with_missions("main", vec![survey]);
    state.roster.agents.push(worker);
    store.save(&state).await.unwrap();

    let first = store
        .complete("worker-a", "  final report  ")
        .await
        .unwrap();
    assert_eq!(first.mission.unwrap().status, TowerMissionStatus::Completed);
    assert!(first.notification_created);
    let message = first.message.unwrap();
    assert_eq!(message.item.subject, "survey-summary");
    assert_eq!(message.item.scope.as_deref(), Some("M1"));
    assert_eq!(message.item.action.as_deref(), Some("complete"));
    assert_eq!(message.item.body, "final report");
    assert!(!message.delivery.message_id.is_empty());
    assert_eq!(message.delivery.mission_id.as_deref(), Some("M1"));

    let retry = store.complete("worker-a", "final report").await.unwrap();
    assert!(!retry.notification_created);
    let mut entries = tokio::fs::read_dir(store.abs(INBOX_DIR)).await.unwrap();
    let mut count = 0;
    while entries.next_entry().await.unwrap().is_some() {
        count += 1;
    }
    assert_eq!(count, 1, "the retry must reuse the stored notification");

    let changed = store.complete("worker-a", "updated report").await.unwrap();
    assert!(changed.notification_created);
}

#[tokio::test]
async fn complete_rejects_an_empty_report() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_with_record_dirs(dir.path()).await;
    let error = store.complete("worker-a", " \n ").await.unwrap_err();
    assert_eq!(error, "TowerComplete report must not be empty");
}

#[tokio::test]
async fn inbox_pages_ack_only_the_returned_unread_messages() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_with_record_dirs(dir.path()).await;
    store
        .save(&state_with(vec![entry("worker-a", "agent-0")]))
        .await
        .unwrap();
    for subject in ["one", "two", "three"] {
        store
            .send(
                TOWER_NAME,
                TowerSendInput {
                    to: "worker-a".into(),
                    subject: subject.into(),
                    body: subject.into(),
                    scope: None,
                    action: None,
                    consent_ref: None,
                },
            )
            .await
            .unwrap();
    }

    let first = store.read_inbox_page("worker-a", 2, false).await.unwrap();
    assert_eq!(first.items.len(), 2);
    assert_eq!(first.unread, 1);
    assert!(first.items.iter().all(|item| !item.message_id.is_empty()));
    let second = store.read_inbox_page("worker-a", 2, false).await.unwrap();
    assert_eq!(second.items.len(), 1);
    assert_eq!(second.unread, 0);
    assert!(
        store
            .read_inbox_page("worker-a", 20, false)
            .await
            .unwrap()
            .items
            .is_empty()
    );
}

#[tokio::test]
async fn include_read_returns_history_without_acking_outside_the_page() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_with_record_dirs(dir.path()).await;
    store
        .save(&state_with(vec![entry("worker-a", "agent-0")]))
        .await
        .unwrap();
    for subject in ["old", "new"] {
        store
            .send(
                TOWER_NAME,
                TowerSendInput {
                    to: "worker-a".into(),
                    subject: subject.into(),
                    body: subject.into(),
                    scope: None,
                    action: None,
                    consent_ref: None,
                },
            )
            .await
            .unwrap();
    }

    let history = store.read_inbox_page("worker-a", 1, true).await.unwrap();
    assert_eq!(history.items.len(), 1);
    assert_eq!(history.items[0].subject, "new");
    assert_eq!(history.unread, 1);
    let unread = store.read_inbox_page("worker-a", 20, false).await.unwrap();
    assert_eq!(unread.items.len(), 1);
    assert_eq!(unread.items[0].subject, "old");
}

#[tokio::test]
async fn complete_recovers_lost_state_and_degrades_to_a_report() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let base = repo_with_commit(root).await;
    let store = store_with_record_dirs(root).await;

    let recovered = store
        .complete_agent("agent-0", "recovered report")
        .await
        .unwrap();
    assert!(recovered.recovered_at.is_some());
    assert!(recovered.mission.is_none());
    let message = recovered.message.unwrap();
    assert_eq!(message.item.subject, "completion-report-state-lost");
    assert_eq!(message.item.scope, None);
    assert_eq!(message.item.body, "recovered report");

    let raw: serde_json::Value = serde_json::from_str(
        &tokio::fs::read_to_string(store.abs(STATE_FILE))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(raw["base"].as_str(), Some(base.as_str()));
    assert_eq!(
        raw["recoveredAt"].as_str(),
        recovered.recovered_at.as_deref()
    );
    assert_eq!(
        raw["roster"]["agents"][0]["agentId"].as_str(),
        Some("agent-0")
    );

    let retry = store
        .complete_agent("agent-0", "recovered report")
        .await
        .unwrap();
    assert!(!retry.notification_created);
}

#[tokio::test]
async fn non_clean_review_notifies_the_owner_and_tower_with_full_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let base = repo_with_branch_diff(root, "feat/review").await;
    let store = store_with_record_dirs(root).await;
    let mut worker = entry("worker-a", "agent-1");
    worker.mission_id = Some("M1".into());
    let mut reviewer = review_entry("reviewer-1", "feat/review");
    reviewer.agent_id = "agent-2".into();
    reviewer.review_mission_id = Some("M1".into());
    let mut owned = mission("M1", "feat/review", TowerMissionStatus::Completed);
    owned.owner = Some("worker-a".into());
    let mut state = state_with_missions(&base, vec![owned]);
    state.roster.agents.extend([worker, reviewer]);
    store.save(&state).await.unwrap();

    store
        .submit_review(
            "reviewer-1",
            TowerReviewInput {
                target: "feat/review".into(),
                status: "p1-1items".into(),
                merge: "fix-then-merge".into(),
                findings: "f".into(),
                checks: None,
                decision: "d".into(),
            },
        )
        .await
        .unwrap();

    let messages: Vec<TowerInboxItem> = store
        .read_inbox(TOWER_NAME, usize::MAX)
        .await
        .unwrap()
        .into_iter()
        .filter(|item| item.subject == "review-result")
        .collect();
    assert_eq!(messages.len(), 2, "{messages:?}");
    assert!(messages.iter().any(|item| item.to == "worker-a"));
    assert!(messages.iter().any(|item| item.to == TOWER_NAME));
    for message in messages {
        assert_eq!(message.scope.as_deref(), Some("M1"));
        assert_eq!(message.action.as_deref(), Some("review-result"));
        assert!(
            message.body.contains("target: feat/review"),
            "{}",
            message.body
        );
        assert!(message.body.contains("mission: M1"), "{}", message.body);
        assert!(message.body.contains("round: 1"), "{}", message.body);
        assert!(
            message.body.contains("verdict: p1-1items"),
            "{}",
            message.body
        );
        assert!(
            message
                .body
                .contains("merge recommendation: fix-then-merge"),
            "{}",
            message.body
        );
        assert!(message.body.contains("owner: worker-a"), "{}", message.body);
    }
}

async fn store_ready_to_merge(root: &Path, branch: &str) -> (TowerStore, String) {
    let base = repo_with_branch_diff(root, branch).await;
    let store = store_with_record_dirs(root).await;
    let mut reviewer = review_entry("reviewer-1", branch);
    reviewer.review_mission_id = Some("M1".into());
    let mut state = state_with_missions(
        &base,
        vec![mission("M1", branch, TowerMissionStatus::Completed)],
    );
    state.roster.agents.push(reviewer);
    store.save(&state).await.unwrap();
    store
        .submit_review(
            "reviewer-1",
            TowerReviewInput {
                target: branch.into(),
                status: "clean".into(),
                merge: "merge".into(),
                findings: "f".into(),
                checks: None,
                decision: "d".into(),
            },
        )
        .await
        .unwrap();
    (store, base)
}

#[tokio::test]
async fn merge_fails_closed_for_unknown_and_busy_workers() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let (store, _) = store_ready_to_merge(root, "feat/workers").await;
    let mut worker = entry("worker-a", "agent-1");
    worker.mission_id = Some("M1".into());
    worker.branch = Some("feat/workers".into());
    worker.worktree = Some("wt-M1".into());
    let mut state = store.load().await.unwrap();
    state.missions[0].owner = Some("worker-a".into());
    state.roster.agents.push(worker);
    store.save(&state).await.unwrap();

    let error = store.merge("feat/workers").await.unwrap_err();
    assert!(error.contains("unknown is not idle"), "{error}");
    let active = HashSet::from(["agent-1".to_string()]);
    let error = store
        .merge_with_workers("feat/workers", Some(active))
        .await
        .unwrap_err();
    assert!(error.contains("mid-turn"), "{error}");
}

#[tokio::test]
async fn merge_refuses_a_branch_that_does_not_contain_the_current_base() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let (store, _) = store_ready_to_merge(root, "feat/stale").await;
    std::fs::write(root.join("base-moved.txt"), "new base").unwrap();
    git::git(root, &["add", "."]).await.unwrap();
    git::git(root, &["commit", "-qm", "advance base"])
        .await
        .unwrap();

    let error = store.merge("feat/stale").await.unwrap_err();
    assert!(error.contains("TowerRebase"), "{error}");
    let log = tokio::fs::read_to_string(store.abs(ACTIVITY_LOG))
        .await
        .unwrap();
    assert!(log.contains("reason=stale-base"), "{log}");
}

#[tokio::test]
async fn merge_allows_unrelated_checkout_dirt() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let (store, _) = store_ready_to_merge(root, "feat/dirty-ok").await;
    std::fs::write(root.join("unrelated-notes.txt"), "do not merge me").unwrap();

    let result = store.merge("feat/dirty-ok").await.unwrap();
    assert_eq!(result.status, TowerMergeStatus::Merged);
    assert_eq!(
        std::fs::read_to_string(root.join("unrelated-notes.txt")).unwrap(),
        "do not merge me"
    );
}

#[tokio::test]
async fn merge_refuses_checkout_dirt_that_intersects_the_merge() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let (store, _) = store_ready_to_merge(root, "feat/dirty-blocked").await;
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/feature.txt"), "local overwrite").unwrap();

    let error = store.merge("feat/dirty-blocked").await.unwrap_err();
    assert!(error.contains("src/feature.txt"), "{error}");
    let log = tokio::fs::read_to_string(store.abs(ACTIVITY_LOG))
        .await
        .unwrap();
    assert!(log.contains("reason=base-dirty"), "{log}");
}

#[test]
fn merge_status_distinguishes_advanced_merged_and_noop() {
    assert_eq!(
        TowerMergeStatus::after("a", "b", false),
        TowerMergeStatus::Advanced
    );
    assert_eq!(
        TowerMergeStatus::after("a", "a", false),
        TowerMergeStatus::Merged
    );
    assert_eq!(
        TowerMergeStatus::after("a", "a", true),
        TowerMergeStatus::Noop
    );
}

#[tokio::test]
async fn clean_tower_rebase_records_an_exact_review_waiver() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let (store, base) = store_ready_to_merge(root, "feat/rebase").await;
    store
        .add_worktree("wt-M1", "feat/rebase", &base)
        .await
        .unwrap();
    let from_commit = git::branch_tip(root, "feat/rebase").await.unwrap();
    std::fs::write(root.join("base-moved.txt"), "new base").unwrap();
    git::git(root, &["add", "."]).await.unwrap();
    git::git(root, &["commit", "-qm", "advance base"])
        .await
        .unwrap();

    let rebased = store
        .rebase_with_workers(TOWER_NAME, "M1", Some(HashSet::new()))
        .await
        .unwrap();
    assert_eq!(rebased.status, TowerRebaseMissionStatus::Rebased);
    assert_eq!(rebased.from_commit.as_deref(), Some(from_commit.as_str()));
    let to_commit = rebased.to_commit.unwrap();
    assert_ne!(to_commit, from_commit);
    let raw: serde_json::Value = serde_json::from_str(
        &tokio::fs::read_to_string(store.abs(STATE_FILE))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        raw["missions"][0]["lastRebase"]["fromCommit"].as_str(),
        Some(from_commit.as_str())
    );
    assert_eq!(
        raw["missions"][0]["lastRebase"]["toCommit"].as_str(),
        Some(to_commit.as_str())
    );

    let merged = store.merge("feat/rebase").await.unwrap();
    assert_eq!(merged.status, TowerMergeStatus::Merged);
}

#[tokio::test]
async fn conflicting_tower_rebase_aborts_and_blocks_without_a_waiver() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let (store, base) = store_ready_to_merge(root, "feat/rebase-conflict").await;
    store
        .add_worktree("wt-M1", "feat/rebase-conflict", &base)
        .await
        .unwrap();
    let from_commit = git::branch_tip(root, "feat/rebase-conflict").await.unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/feature.txt"), "conflicting base").unwrap();
    git::git(root, &["add", "."]).await.unwrap();
    git::git(root, &["commit", "-qm", "conflicting base"])
        .await
        .unwrap();

    let rebased = store
        .rebase_with_workers(TOWER_NAME, "M1", Some(HashSet::new()))
        .await
        .unwrap();
    assert_eq!(rebased.status, TowerRebaseMissionStatus::Conflict);
    assert!(rebased.files.contains(&"src/feature.txt".to_string()));
    let worktree = store.abs(&format!("{WORKTREES_DIR}/wt-M1"));
    assert_eq!(
        git::branch_tip(&worktree, "HEAD").await.unwrap(),
        from_commit
    );
    let state = store.load().await.unwrap();
    assert_eq!(state.missions[0].status, TowerMissionStatus::Blocked);
    assert!(!state.missions[0].blockers.is_empty());
    let raw: serde_json::Value = serde_json::from_str(
        &tokio::fs::read_to_string(store.abs(STATE_FILE))
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(raw["missions"][0].get("lastRebase").is_none());
}

#[tokio::test]
async fn init_reanchors_base_and_reports_stale_open_missions() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let (store, base) = store_ready_to_merge(root, "feat/reanchor").await;
    let branch_tip_before = git::branch_tip(root, "feat/reanchor").await.unwrap();
    git::git(root, &["checkout", "-qb", "new-base"])
        .await
        .unwrap();
    std::fs::write(root.join("new-base.txt"), "new base").unwrap();
    git::git(root, &["add", "."]).await.unwrap();
    git::git(root, &["commit", "-qm", "new base tip"])
        .await
        .unwrap();

    let initialized = store.init(None, Some("new-base".into())).await.unwrap();
    assert!(initialized.ignored_base.is_none());
    let digest = initialized.rebase_digest.unwrap();
    assert_eq!(digest.from, base);
    assert_eq!(digest.to, "new-base");
    assert_eq!(digest.missions[0].relation, TowerReanchorRelation::Stale);
    assert_eq!(store.load().await.unwrap().base, "new-base");
    assert_eq!(
        git::branch_tip(root, "feat/reanchor").await.unwrap(),
        branch_tip_before
    );
}

#[tokio::test]
async fn status_reader_exposes_gate_reasons_binding_and_unread_count() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let (store, _) = store_ready_to_merge(root, "feat/status").await;
    let mut worker = entry("worker-a", "agent-1");
    worker.mission_id = Some("M1".into());
    let mut state = store.load().await.unwrap();
    state.roster.agents.push(worker);
    store.save(&state).await.unwrap();

    let status = crate::tools::tower::status_reader::read_tower_status(&store, "main", None, None)
        .await
        .unwrap();
    assert!(status.initialized);
    assert_eq!(status.caller, TOWER_NAME);
    assert_eq!(status.gates.len(), 1);
    assert!(
        status.gates[0]
            .reasons
            .contains(&"worker-unknown".to_string())
    );
    assert_eq!(status.gates[0].binding.as_deref(), Some("exact-tip"));
    assert!(status.inbox_count >= 1);
}

#[tokio::test]
async fn wake_digest_preparation_preserves_mission_order_without_calling_a_wake_api() {
    let dir = tempfile::tempdir().unwrap();
    let (store, _) = store_ready_to_merge(dir.path(), "feat/wake-digest").await;
    let state = store.load().await.unwrap();
    let mission_ids = vec!["M404".to_string(), "M1".to_string()];

    let digests = crate::tools::tower::status_reader::read_tower_wake_gate_digests(
        &store,
        &state,
        &mission_ids,
        None,
    )
    .await;

    assert_eq!(digests.len(), 2);
    assert_eq!(digests[0].mission_id, "M404");
    assert_eq!(digests[0].status, "mission-missing");
    assert_eq!(digests[1].mission_id, "M1");
    assert_eq!(digests[1].status, "READY");
    assert!(digests[1].reasons.is_empty());
    assert_eq!(digests[1].binding.as_deref(), Some("exact-tip"));
}

#[tokio::test]
async fn status_reader_reports_state_loss_without_recovering() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    repo_with_commit(root).await;
    let store = store_with_record_dirs(root).await;

    let status = crate::tools::tower::status_reader::read_tower_status(&store, "main", None, None)
        .await
        .unwrap();
    assert!(!status.initialized);
    assert!(status.state_lost);
    assert!(!store.is_initialized().await.unwrap());
}

#[tokio::test]
async fn write_unique_publishes_hard_links_without_temporary_leftovers() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_with_record_dirs(dir.path()).await;
    let rel = format!("{INBOX_DIR}/unique.md");

    let first = store
        .write_unique_with(&rel, "first", |from, to| async move {
            fs::hard_link(from, to).await
        })
        .await
        .unwrap();
    let second = store
        .write_unique_with(&rel, "second", |from, to| async move {
            fs::hard_link(from, to).await
        })
        .await
        .unwrap();

    assert_eq!(first, rel);
    assert_eq!(second, format!("{INBOX_DIR}/unique-2.md"));
    assert_eq!(
        fs::read_to_string(store.abs(&first)).await.unwrap(),
        "first"
    );
    assert_eq!(
        fs::read_to_string(store.abs(&second)).await.unwrap(),
        "second"
    );
    for entry in std::fs::read_dir(store.abs(INBOX_DIR)).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().to_string();
        assert!(!name.contains(".tmp-"), "leftover temporary file: {name}");
    }
}

#[tokio::test]
async fn write_unique_falls_back_to_exclusive_creation_when_links_are_unsupported() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_with_record_dirs(dir.path()).await;
    let rel = format!("{INBOX_DIR}/fallback.md");

    let written = store
        .write_unique_with(&rel, "fallback", |_, _| async {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "hard links disabled",
            ))
        })
        .await
        .unwrap();

    assert_eq!(written, rel);
    assert_eq!(
        fs::read_to_string(store.abs(&written)).await.unwrap(),
        "fallback"
    );
}
