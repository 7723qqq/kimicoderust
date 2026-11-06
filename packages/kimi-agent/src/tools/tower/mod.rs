pub mod bash_guard;
pub mod concurrency;
pub mod frontmatter;
pub mod git;
pub mod mission_gate;
pub mod paths;
pub mod rate_limit;
pub mod status_reader;
pub mod store;
pub mod types;
pub mod wake;

use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::subagent::SubagentManager;
use crate::subagent::secondary::TowerModelBinding;
use crate::tools::tower::paths::{
    BROADCAST_NAME, MISSIONS_DIR, TOWER_NAME, WORKTREES_DIR, mission_file_name,
    resolve_tower_repo_root,
};
use crate::tools::tower::rate_limit::TowerRateLimit;
use crate::tools::tower::store::{TowerStore, TowerTeardownOptions};
use crate::tools::tower::types::{
    TowerFindingInput, TowerMissionPatch, TowerPlanInput, TowerReviewInput, TowerRosterEntry,
    TowerSendInput, TowerTaskDrop,
};
use crate::turn_loop::types::{ExecutableToolResult, ToolInfo};

pub const TOWER_MAIN_AGENT_ONLY: &str =
    "Tower orchestration tools are only supported by the main agent.";
pub const TOWER_USER_CANCELLATION_REASON: &str = "user cancelled";

fn err_result(msg: impl Into<String>) -> ExecutableToolResult {
    ExecutableToolResult {
        delivery: None,
        stop_turn: false,
        content: msg.into(),
        is_error: true,
        note: None,
        display: None,
    }
}

fn ok_result(msg: impl Into<String>) -> ExecutableToolResult {
    ExecutableToolResult {
        delivery: None,
        stop_turn: false,
        content: msg.into(),
        is_error: false,
        note: None,
        display: None,
    }
}

const TOWER_WORKER_PROFILE: &str = "tower-worker";

/// Resolve a resume id against the tower roster (v2 `towerService` Agent
/// veto's `store.resolveAgent`): `Ok(Some(entry))` when the id sits in the
/// roster, `Ok(None)` when the tower state is readable but the id is not a
/// member, `Err(())` when there is no tower to consult (missing cwd, missing
/// or unreadable state.json) — the caller silently lets the resume through
/// instead of failing a call the tower cannot vouch for. The last
/// registration wins (agent ids restart per session, so a stale entry can
/// share a live id).
pub async fn tower_resume_target(
    cwd: Option<std::path::PathBuf>,
    agent_id: &str,
) -> Result<Option<TowerRosterEntry>, String> {
    let cwd =
        cwd.ok_or_else(|| "no working directory to resolve the tower repo root".to_string())?;
    let repo_root = resolve_tower_repo_root(&cwd.to_string_lossy());
    let store = TowerStore::new(PathBuf::from(repo_root));
    let state = store
        .load()
        .await
        .map_err(|e| format!("failed to load the tower store: {e}"))?;
    let entry = state
        .roster
        .agents
        .iter()
        .rev()
        .find(|a| a.agent_id == agent_id)
        .cloned();
    if let Some(ref agent) = entry {
        if TowerStore::is_stopped_by_user(agent) {
            return Ok(None);
        }
        if agent.died_at.is_some() {
            store
                .clear_agent_died(agent_id, agent.session_id.as_deref())
                .await?;
        }
    }
    Ok(entry)
}

fn spawn_detached_run(
    manager: Arc<SubagentManager>,
    callbacks: Arc<dyn crate::callbacks::HostCallbacks>,
    store: Arc<TowerStore>,
    agent_id: &str,
    session_id: &str,
    prompt: &str,
    parent_cancel: Option<crate::subagent::types::ParentCancel>,
) {
    let agent = agent_id.to_string();
    let session = session_id.to_string();
    let prompt = prompt.to_string();
    tokio::spawn(async move {
        // Mirror the swarm launcher's terminal handling: pass the parent cancel
        // so a detached worker stops when the user cancels or the session ends
        // (run_foreground_turn aborts the turn and kills the instance), and emit
        // a terminal event for *every* outcome so the worker's card never sticks
        // in "running" — the previous `let _ = …map(…)` swallowed the error and
        // cancelled arms and only ever reported a clean completion.
        let outcome = manager
            .run_foreground_turn(&agent, &prompt, parent_cancel.as_ref())
            .await;
        let rate_limit = TowerRateLimit::global();
        rate_limit.release();
        match outcome {
            Ok(crate::subagent::manager::ForegroundTurnOutcome::Completed(turn)) => {
                rate_limit.report_success();
                if matches!(
                    turn.stop_reason,
                    crate::turn_loop::types::LoopTurnStopReason::Aborted
                ) {
                    crate::tools::agent_tool::emit_failed(
                        callbacks.as_ref(),
                        &agent,
                        TOWER_WORKER_PROFILE,
                        "The tower worker was stopped before it finished.",
                    );
                    // Record the death in the tower protocol so TowerStatus can
                    // flag the orphaned mission and suggest the resume hint.
                    if let Err(e) = store
                        .mark_agent_dead(
                            &agent,
                            "killed",
                            Some(TOWER_USER_CANCELLATION_REASON),
                            Some(&session),
                        )
                        .await
                    {
                        callbacks.emit_event(serde_json::json!({
                            "type": "subagent.failed",
                            "subagent_id": agent,
                            "error": format!("failed to record the dead roster entry: {e}"),
                        }));
                    }
                } else {
                    let summary = crate::subagent::manager::final_assistant_summary(&turn.messages);
                    crate::tools::agent_tool::emit_completed(
                        callbacks.as_ref(),
                        &agent,
                        TOWER_WORKER_PROFILE,
                        &summary,
                        &turn.usage,
                    );
                }
            }
            Ok(crate::subagent::manager::ForegroundTurnOutcome::ParentCancelled) => {
                crate::tools::agent_tool::emit_failed(
                    callbacks.as_ref(),
                    &agent,
                    TOWER_WORKER_PROFILE,
                    "The tower worker was stopped by the user before it finished.",
                );
                let _ = store
                    .mark_agent_dead(
                        &agent,
                        "killed",
                        Some(TOWER_USER_CANCELLATION_REASON),
                        Some(&session),
                    )
                    .await;
            }
            Err(err) => {
                if err.contains("rate limit") || err.contains("429") {
                    rate_limit.report_rate_limited();
                }
                crate::tools::agent_tool::emit_failed(
                    callbacks.as_ref(),
                    &agent,
                    TOWER_WORKER_PROFILE,
                    &err,
                );
                let _ = store
                    .mark_agent_dead(&agent, "failed", Some(&err), Some(&session))
                    .await;
            }
        }
    });
}

pub async fn execute_tower_init(
    cwd: &Path,
    caller_agent_id: &str,
    session_id: &str,
    raw_args: &str,
) -> ExecutableToolResult {
    if caller_agent_id != "main" {
        return err_result(TOWER_MAIN_AGENT_ONLY);
    }

    #[derive(Deserialize, Default)]
    struct InitArgs {
        base: Option<String>,
    }
    let args: InitArgs = serde_json::from_str(raw_args).unwrap_or_default();

    let repo_root = resolve_tower_repo_root(&cwd.to_string_lossy());
    let store = TowerStore::new(PathBuf::from(repo_root));
    // Serialize this repo's tower state against concurrent workers: hold the
    // per-repo lock across the whole operation so its load→mutate→save cannot
    // interleave with another agent's and lose an update.
    let state_lock = store.state_lock();
    let _state_guard = state_lock.lock().await;

    match store.init(Some(session_id.to_string()), args.base).await {
        Ok(res) => {
            let mut lines = vec![
                if res.created {
                    "tower workspace initialized".to_string()
                } else {
                    "tower workspace already initialized — existing state preserved".to_string()
                },
                format!("base branch: {}", res.base),
            ];
            if let Some(digest) = res.rebase_digest.as_ref() {
                lines.push(format!(
                    "base re-anchored from \"{}\" to \"{}\" — open missions now evaluate against \"{}\"",
                    digest.from, digest.to, digest.to
                ));
                for mission in &digest.missions {
                    lines.push(format!(
                        "mission {} ({}): {}{}",
                        mission.id,
                        mission.branch,
                        mission.relation.as_str(),
                        match mission.relation {
                            crate::tools::tower::types::TowerReanchorRelation::UpToDate =>
                                " — already contains the new base tip",
                            crate::tools::tower::types::TowerReanchorRelation::Stale =>
                                " — run TowerRebase before merging",
                            crate::tools::tower::types::TowerReanchorRelation::NoBranch =>
                                " — spawn first, then evaluate its relation",
                        }
                    ));
                }
            }
            if let Some(ignored) = res.ignored_base {
                lines.push(format!(
                    "requested base \"{ignored}\" ignored — the existing workspace already records base \"{}\"; tear it down first to rebase the tower",
                    res.base
                ));
            }
            if res.checkout != res.base {
                if res.checkout == "HEAD" {
                    lines.push(format!(
                        "note: the main checkout is in a detached HEAD state — merges stay blocked until the base is checked out (git checkout {})",
                        res.base
                    ));
                } else {
                    lines.push(format!(
                        "note: the main checkout is on \"{}\", not base \"{}\" — merges stay blocked until it is switched over (git checkout {})",
                        res.checkout, res.base, res.base
                    ));
                }
            }
            lines.push(
                "workspace: .tower/ (comms under .tower/comms/, worktrees under .tower/worktrees/)"
                    .to_string(),
            );
            if !res.open_missions.is_empty() {
                lines.push(format!(
                    "carried-over open missions: {} — their scopes are still reserved. Continue them (TowerSpawn fresh workers), or — when they belong to an unrelated earlier task — abandon them first (TowerMission status=abandoned) so a new plan can use those files.",
                    res.open_missions.join(", ")
                ));
            }
            if !res.retired_agents.is_empty() {
                lines.push(format!(
                    "adopted from a previous session — retired its stale roster entries: {}. Their agents belong to the dead session and cannot be resumed; missions and worktrees are preserved — TowerSpawn fresh workers to continue them.",
                    res.retired_agents.join(", ")
                ));
            }
            lines.push(String::new());
            lines.push("Tower mode is active and the tower tool set is enabled.".to_string());
            lines.push("Next: split the work with TowerPlan (one mission per disjoint file scope), then TowerSpawn a worker per mission. Assign reviewers for their branches, and merge with TowerMerge only after a clean review.".to_string());

            ok_result(lines.join("\n"))
        }
        Err(e) => err_result(e),
    }
}

pub async fn execute_tower_plan(
    cwd: &Path,
    caller_agent_id: &str,
    raw_args: &str,
) -> ExecutableToolResult {
    if caller_agent_id != "main" {
        return err_result(TOWER_MAIN_AGENT_ONLY);
    }

    #[derive(Deserialize)]
    struct PlanArgs {
        missions: Vec<TowerPlanInput>,
    }
    let args: PlanArgs = match serde_json::from_str(raw_args) {
        Ok(a) => a,
        Err(e) => return err_result(format!("failed to parse TowerPlan input: {e}")),
    };

    let repo_root = resolve_tower_repo_root(&cwd.to_string_lossy());
    let store = TowerStore::new(PathBuf::from(repo_root));
    // Serialize this repo's tower state against concurrent workers: hold the
    // per-repo lock across the whole operation so its load→mutate→save cannot
    // interleave with another agent's and lose an update.
    let state_lock = store.state_lock();
    let _state_guard = state_lock.lock().await;

    match store.plan(&args.missions).await {
        Ok(missions) => {
            let mut lines = vec![
                format!("planned {} mission(s):", missions.len()),
                String::new(),
                "| ID | Mission | Kind | Branch | Worktree | Scope |".to_string(),
                "| -- | ------- | ---- | ------ | -------- | ----- |".to_string(),
            ];
            for m in &missions {
                lines.push(format!(
                    "| {} | {} | {} | {} | {} | {} |",
                    m.id,
                    m.title,
                    m.kind.as_str(),
                    m.branch,
                    m.worktree,
                    m.scope.join(", ")
                ));
            }
            lines.push(String::new());
            lines.push("Next: TowerSpawn one worker per mission (workers get their worktree path and mission briefing automatically), plus reviewers for the branches. Survey missions need no reviewer — they close with a zero-diff TowerMerge.".to_string());
            ok_result(lines.join("\n"))
        }
        Err(e) => err_result(e),
    }
}

fn resolve_tower_spawn_model(
    pool: Option<&crate::subagent::secondary::SecondaryModelRuntime>,
    primary: Arc<dyn crate::turn_loop::types::LLM>,
    reviewer: bool,
    requested: Option<&str>,
) -> Result<TowerModelBinding, String> {
    match pool {
        Some(pool) => pool.resolve_tower(primary, reviewer, requested),
        None => crate::subagent::secondary::SecondaryModelRuntime::resolve_tower_without_pool(
            primary, reviewer, requested,
        ),
    }
}

fn spawn_result_metadata(
    name: &str,
    kind_line: String,
    agent_id: &str,
    model: &str,
) -> [String; 6] {
    [
        format!("name: {name}"),
        kind_line,
        format!("agent_id: {agent_id}"),
        format!("task_id: {agent_id}"),
        "status: running".to_string(),
        format!("model: {model}"),
    ]
}

async fn record_spawn_log(
    store: &TowerStore,
    kind: &str,
    name: &str,
    agent_id: &str,
    model: &str,
    mission: Option<&crate::tools::tower::types::TowerMission>,
    review_target: Option<&str>,
) -> Result<(), String> {
    let mission_id = mission.map_or("", |mission| mission.id.as_str());
    let details = [
        ("name", name),
        ("kind", kind),
        ("agent", agent_id),
        ("mission", mission_id),
        ("target", review_target.unwrap_or("")),
        ("model", model),
    ];
    let ref_path = mission.map(|mission| {
        format!(
            "{MISSIONS_DIR}/{}",
            mission_file_name(&mission.id, &mission.slug)
        )
    });
    store
        .append_log(TOWER_NAME, "spawn", &details, ref_path.as_deref())
        .await
}

pub async fn execute_tower_spawn(
    cwd: &Path,
    caller_agent_id: &str,
    session_id: &str,
    spawn_models: Option<(
        &Arc<SubagentManager>,
        Option<&crate::subagent::secondary::SecondaryModelRuntime>,
        Option<u32>,
    )>,
    tool_call_id: Option<&str>,
    parent_cancel: Option<&crate::subagent::types::ParentCancel>,
    raw_args: &str,
) -> Option<ExecutableToolResult> {
    if caller_agent_id != "main" {
        return Some(err_result(TOWER_MAIN_AGENT_ONLY));
    }
    let (subagent_manager, secondary_model, max_running_tasks) = match spawn_models {
        Some((manager, pool, cap)) => (Some(manager), pool, cap),
        None => (None, None, None),
    };

    #[derive(Deserialize)]
    struct SpawnArgs {
        name: String,
        kind: String,
        mission_id: Option<String>,
        review_target: Option<String>,
        instructions: Option<String>,
        model: Option<String>,
    }
    let args: SpawnArgs = match serde_json::from_str(raw_args) {
        Ok(a) => a,
        Err(e) => return Some(err_result(format!("failed to parse TowerSpawn input: {e}"))),
    };

    let repo_root = resolve_tower_repo_root(&cwd.to_string_lossy());
    let store = TowerStore::new(PathBuf::from(&repo_root));

    let state = match store.load().await {
        Ok(s) => s,
        Err(e) => return Some(err_result(e)),
    };

    if let Some(existing) = store.find_agent(&state, &args.name) {
        if TowerStore::is_stopped_by_user(&existing) {
            return Some(err_result(format!(
                "tower agent \"{}\" was stopped by the user — never resume, reassign, or replace it unless the human explicitly asks",
                args.name
            )));
        }
        if existing.kind == crate::tools::tower::types::TowerAgentKind::Reviewer {
            return Some(err_result(format!(
                "tower reviewer \"{}\" is one-shot — diagnose its existing result or death, then spawn a fresh reviewer with a new name instead of resuming it",
                args.name
            )));
        }
        return Some(err_result(format!(
            "tower agent \"{}\" already exists — resume it with Agent(resume=\"{}\", prompt=\"...\") instead",
            args.name, existing.agent_id
        )));
    }

    struct TowerSlotGuard {
        held: bool,
    }
    impl TowerSlotGuard {
        fn acquire(max_running_tasks: Option<u32>) -> Result<Self, String> {
            TowerRateLimit::global()
                .acquire(max_running_tasks)
                .map(|()| Self { held: true })
        }
        fn disarm(mut self) {
            self.held = false;
        }
    }
    impl Drop for TowerSlotGuard {
        fn drop(&mut self) {
            if self.held {
                TowerRateLimit::global().release();
            }
        }
    }

    let slot = match TowerSlotGuard::acquire(max_running_tasks) {
        Ok(s) => s,
        Err(reason) => return Some(err_result(reason)),
    };

    let spawned_at = chrono::Utc::now().to_rfc3339();

    match args.kind.as_str() {
        "worker" => {
            let Some(ref mid) = args.mission_id else {
                return Some(err_result("worker spawns require mission_id"));
            };
            let Some(mission) = state.missions.iter().find(|m| &m.id == mid).cloned() else {
                return Some(err_result(format!("unknown mission \"{mid}\"")));
            };
            if !mission.status.is_open() {
                return Some(err_result(format!(
                    "mission {mid} is terminal ({}) — spawn a fresh mission before creating a worker",
                    mission.status.as_str()
                )));
            }
            if mission
                .owner
                .as_deref()
                .and_then(|owner| store.find_agent(&state, owner))
                .as_ref()
                .is_some_and(TowerStore::is_stopped_by_user)
            {
                return Some(err_result(format!(
                    "mission {mid} owner was stopped by the user — never resume, reassign, or replace it unless the human explicitly asks"
                )));
            }
            // Mode mutex: a mission paused by plan/swarm entry refuses new
            // workers until resumed (v2 gated the tower toolset once exited).
            if let Some(refusal) = crate::tools::mode_mutex::refuse_paused_mission(&mission) {
                return Some(err_result(refusal));
            }

            let manager = subagent_manager?;
            let runtime = manager.runtime().await?;
            manager.get_definition(TOWER_WORKER_PROFILE).await?;
            let model = match resolve_tower_spawn_model(
                secondary_model,
                runtime.llm.clone(),
                false,
                args.model.as_deref(),
            ) {
                Ok(model) => model,
                Err(error) => return Some(err_result(error)),
            };

            if let Err(e) = store
                .add_worktree(&mission.worktree, &mission.branch, &state.base)
                .await
            {
                return Some(err_result(format!("failed to create worktree: {e}")));
            }

            let worktree_abs = store.abs(&format!("{WORKTREES_DIR}/{}", mission.worktree));
            let prompt = build_worker_prompt(
                &args.name,
                &mission,
                &worktree_abs,
                &state.base,
                args.instructions.as_deref(),
            );
            let agent_id = match manager.spawn(TOWER_WORKER_PROFILE, &args.name).await {
                Ok(id) => id,
                Err(e) => return Some(err_result(format!("failed to spawn worker: {e}"))),
            };
            manager.set_instance_llm(&agent_id, model.llm.clone()).await;

            let entry = TowerRosterEntry {
                name: args.name.clone(),
                agent_id: agent_id.clone(),
                session_id: Some(session_id.to_string()),
                kind: crate::tools::tower::types::TowerAgentKind::Worker,
                mission_id: Some(mission.id.clone()),
                review_target: None,
                review_mission_id: None,
                worktree: Some(mission.worktree.clone()),
                branch: Some(mission.branch.clone()),
                spawned_at,
                died_at: None,
                death_status: None,
                death_reason: None,
                status: None,
            };
            if let Err(e) = store
                .register_spawned_agent(entry, Some((&mission.id, &args.name)))
                .await
            {
                let _ = manager.kill(&agent_id).await;
                return Some(err_result(e));
            }
            if let Err(error) = record_spawn_log(
                &store,
                "worker",
                &args.name,
                &agent_id,
                &model.model,
                Some(&mission),
                None,
            )
            .await
            {
                let _ = manager.kill(&agent_id).await;
                return Some(err_result(error));
            }

            crate::tools::agent_tool::emit_spawned_started(
                runtime.callbacks.as_ref(),
                &agent_id,
                TOWER_WORKER_PROFILE,
                tool_call_id,
                Some(&args.name),
                true,
                None,
                &prompt,
            );
            slot.disarm();
            spawn_detached_run(
                manager.clone(),
                runtime.callbacks.clone(),
                Arc::new(store.clone()),
                &agent_id,
                session_id,
                &prompt,
                parent_cancel.cloned(),
            );

            let mut lines = spawn_result_metadata(
                &args.name,
                "kind: worker".to_string(),
                &agent_id,
                &model.model,
            )
            .into_iter()
            .collect::<Vec<_>>();
            lines.extend([
                format!("mission: {} — {}", mission.id, mission.title),
                format!("branch: {}", mission.branch),
                format!("worktree: {}", worktree_abs.to_string_lossy()),
                String::new(),
                "The worker runs detached in the background; its completion arrives as a notification. Track progress with TowerStatus / TowerInbox. If it dies, diagnose the death status/reason and task state first. Resume or reassign only for transient causes (lost contact, timeout, OOM); fix or escalate systematic defects first. A user-stopped worker is never resumed, reassigned, or replaced.".to_string(),
            ]);

            Some(ok_result(lines.join("\n")))
        }
        "reviewer" => {
            let Some(ref target) = args.review_target else {
                return Some(err_result("reviewer spawns require review_target"));
            };
            if !git::branch_exists(&store.repo_root, target).await {
                return Some(err_result(format!("branch \"{target}\" does not exist")));
            }
            let review_mission = state
                .missions
                .iter()
                .rev()
                .find(|mission| mission.branch == *target);
            if let Some(mission) = review_mission
                && !mission.status.is_open()
            {
                return Some(err_result(format!(
                    "mission {} is terminal ({}) — spawn a fresh mission before creating a reviewer",
                    mission.id,
                    mission.status.as_str()
                )));
            }

            let manager = subagent_manager?;
            let runtime = manager.runtime().await?;
            manager.get_definition(TOWER_WORKER_PROFILE).await?;

            let author = state
                .missions
                .iter()
                .find(|m| &m.branch == target)
                .and_then(|m| m.owner.as_deref());
            let prompt = build_reviewer_prompt(
                &args.name,
                target,
                &state.base,
                &store.repo_root,
                author,
                args.instructions.as_deref(),
            );
            let model = match resolve_tower_spawn_model(
                secondary_model,
                runtime.llm.clone(),
                true,
                args.model.as_deref(),
            ) {
                Ok(model) => model,
                Err(error) => return Some(err_result(error)),
            };
            let agent_id = match manager.spawn(TOWER_WORKER_PROFILE, &args.name).await {
                Ok(id) => id,
                Err(e) => return Some(err_result(format!("failed to spawn reviewer: {e}"))),
            };
            manager.set_instance_llm(&agent_id, model.llm.clone()).await;

            let entry = TowerRosterEntry {
                name: args.name.clone(),
                agent_id: agent_id.clone(),
                session_id: Some(session_id.to_string()),
                kind: crate::tools::tower::types::TowerAgentKind::Reviewer,
                mission_id: None,
                review_target: Some(target.clone()),
                review_mission_id: review_mission.map(|mission| mission.id.clone()),
                worktree: None,
                branch: None,
                spawned_at,
                died_at: None,
                death_status: None,
                death_reason: None,
                status: None,
            };
            if let Err(e) = store.register_spawned_agent(entry, None).await {
                let _ = manager.kill(&agent_id).await;
                return Some(err_result(e));
            }
            if let Err(error) = record_spawn_log(
                &store,
                "reviewer",
                &args.name,
                &agent_id,
                &model.model,
                None,
                Some(target),
            )
            .await
            {
                let _ = manager.kill(&agent_id).await;
                return Some(err_result(error));
            }

            crate::tools::agent_tool::emit_spawned_started(
                runtime.callbacks.as_ref(),
                &agent_id,
                TOWER_WORKER_PROFILE,
                tool_call_id,
                Some(&args.name),
                true,
                None,
                &prompt,
            );
            slot.disarm();
            spawn_detached_run(
                manager.clone(),
                runtime.callbacks.clone(),
                Arc::new(store.clone()),
                &agent_id,
                session_id,
                &prompt,
                parent_cancel.cloned(),
            );

            let mut lines = spawn_result_metadata(
                &args.name,
                "kind: reviewer".to_string(),
                &agent_id,
                &model.model,
            )
            .into_iter()
            .collect::<Vec<_>>();
            lines.extend([
                format!("review_target: {target}"),
                String::new(),
                "The reviewer runs detached in the background; its completion arrives as a notification. Track progress with TowerStatus / TowerInbox. If it dies, diagnose the death status/reason and task state first. A user-stopped reviewer is never resumed or replaced.".to_string(),
            ]);

            Some(ok_result(lines.join("\n")))
        }
        other => Some(err_result(format!(
            "unknown kind \"{other}\" (expected worker | reviewer)"
        ))),
    }
}

fn build_worker_prompt(
    name: &str,
    mission: &crate::tools::tower::types::TowerMission,
    worktree: &Path,
    base: &str,
    instructions: Option<&str>,
) -> String {
    let extra = instructions
        .filter(|i| !i.trim().is_empty())
        .map(|i| format!("\n\n# Additional instructions from the tower\n{}", i.trim()))
        .unwrap_or_default();

    let workplace = format!(
        "# Your workplace\n- Your private git worktree: {}\n- Your branch: {} (base: {})\n- Address the worktree explicitly for code operations.\n- Scope: {}\n\n",
        worktree.to_string_lossy(),
        mission.branch,
        base,
        mission.scope.join(", ")
    );

    if mission.kind == crate::tools::tower::types::TowerMissionKind::Survey {
        format!(
            "You are \"{name}\", a tower worker agent assigned a READ-ONLY survey mission.\n\n\
            {workplace}\
            # Your mission\nMission {}: {}\n\n\
            # Read-only discipline\n\
            - Do NOT modify any code in the repo.\n\
            - Record findings as TowerMission notes and send summaries with TowerSend.\n\n\
            # Communication protocol\n\
            - Coordinate through tower tools ONLY; never create or edit protocol files under .tower/comms by hand.\n\
            - Escalate substantive ambiguity to the tower with TowerSend before acting; never ask the user directly.\n\n\
            # When done\n\
            1. Call TowerInbox and fold anything new into your summary — completion is refused while unread messages wait.\n\
            2. TowerComplete(report=the full survey result)\n\
            3. Finish with a structured summary: coverage, key facts with file:line references, open questions.{extra}",
            mission.id, mission.title
        )
    } else {
        format!(
            "You are \"{name}\", a tower worker agent in a multi-agent workspace.\n\n\
            {workplace}\
            # Your mission\nMission {}: {}\n\n\
            # Communication protocol\n\
            - Coordinate via TowerSend / TowerInbox / TowerFinding / TowerMission / TowerStatus; never create or edit protocol files under .tower/comms by hand.\n\
            - Keep tasks up to date with TowerMission; file out-of-scope discoveries with TowerFinding.\n\
            - Escalate substantive ambiguity to the tower with TowerSend before acting; never ask the user directly.\n\n\
            # When done\n\
            1. Commit your changes in the worktree.\n\
            2. Call TowerInbox and incorporate anything new; completion is refused while unread messages wait. Re-read TowerMission for mid-flight changes.\n\
            3. TowerComplete(report=what changed and why, reconciled against every mission task)\n\
            4. Finish with a structured summary: files changed, key decisions, open follow-ups.\n\
            Split long blocking work into chunks and check TowerInbox between chunks.{extra}",
            mission.id, mission.title
        )
    }
}

fn build_reviewer_prompt(
    name: &str,
    target: &str,
    base: &str,
    repo_root: &Path,
    author: Option<&str>,
    instructions: Option<&str>,
) -> String {
    let extra = instructions
        .filter(|i| !i.trim().is_empty())
        .map(|i| format!("\n\n# Additional instructions from the tower\n{}", i.trim()))
        .unwrap_or_default();

    let notify = match author {
        Some(a) => {
            format!("Notify the author: TowerSend(to=\"{a}\", subject=\"review-result\", ...)")
        }
        None => {
            "Notify the tower: TowerSend(to=\"tower\", subject=\"review-result\", ...)".to_string()
        }
    };

    format!(
        "You are \"{name}\", a tower reviewer agent.\n\n\
        # Your assignment\n\
        Review branch \"{target}\" against base \"{base}\" in {}.\n\
        - Work read-only via git diff / git log.\n\n\
        # When done\n\
        1. TowerReview(target=\"{target}\", status=\"clean\", merge=\"merge\", ...)\n\
        2. {notify}{extra}",
        repo_root.to_string_lossy()
    )
}

pub(crate) fn active_tower_agent_ids(
    task_runner: Option<&crate::storage::TaskRunner>,
) -> Option<std::collections::HashSet<String>> {
    task_runner.map(|runner| {
        runner
            .list()
            .into_iter()
            .filter(|task| {
                task.get("kind").and_then(serde_json::Value::as_str) == Some("agent")
                    && task.get("status").and_then(serde_json::Value::as_str) == Some("running")
            })
            .filter_map(|task| {
                task.get("agentId")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
            .collect()
    })
}

pub async fn execute_tower_rebase(
    cwd: &Path,
    caller_agent_id: &str,
    raw_args: &str,
    task_runner: Option<&crate::storage::TaskRunner>,
) -> ExecutableToolResult {
    if caller_agent_id != "main" {
        return err_result(TOWER_MAIN_AGENT_ONLY);
    }
    #[derive(Deserialize)]
    struct RebaseArgs {
        mission: String,
    }
    let args: RebaseArgs = match serde_json::from_str(raw_args) {
        Ok(args) => args,
        Err(error) => return err_result(format!("failed to parse TowerRebase input: {error}")),
    };
    let repo_root = resolve_tower_repo_root(&cwd.to_string_lossy());
    let store = TowerStore::new(PathBuf::from(repo_root));
    let state_lock = store.state_lock();
    let _state_guard = state_lock.lock().await;
    match store
        .rebase_with_workers(
            TOWER_NAME,
            &args.mission,
            active_tower_agent_ids(task_runner),
        )
        .await
    {
        Ok(result) => {
            let status = match result.status {
                crate::tools::tower::types::TowerRebaseMissionStatus::UpToDate => {
                    return ok_result(format!("mission {} is already up to date", args.mission));
                }
                crate::tools::tower::types::TowerRebaseMissionStatus::Rebased => "rebased",
                crate::tools::tower::types::TowerRebaseMissionStatus::Conflict => {
                    return err_result(format!(
                        "mission {} rebase conflicted in: {} — the rebase was aborted, the worktree was restored, and the mission is blocked",
                        args.mission,
                        result.files.join(", ")
                    ));
                }
            };
            ok_result(format!(
                "mission {} {status}: {} → {}. The exact clean-rebase review waiver was recorded.",
                args.mission,
                result.from_commit.unwrap_or_default(),
                result.to_commit.unwrap_or_default()
            ))
        }
        Err(error) => err_result(error),
    }
}

pub async fn execute_tower_merge(
    cwd: &Path,
    caller_agent_id: &str,
    raw_args: &str,
    task_runner: Option<&crate::storage::TaskRunner>,
) -> ExecutableToolResult {
    if caller_agent_id != "main" {
        return err_result(TOWER_MAIN_AGENT_ONLY);
    }

    #[derive(Deserialize)]
    struct MergeArgs {
        branch: String,
    }
    let args: MergeArgs = match serde_json::from_str(raw_args) {
        Ok(a) => a,
        Err(e) => return err_result(format!("failed to parse TowerMerge input: {e}")),
    };

    let repo_root = resolve_tower_repo_root(&cwd.to_string_lossy());
    let store = TowerStore::new(PathBuf::from(repo_root));
    // Serialize this repo's tower state against concurrent workers: hold the
    // per-repo lock across the whole operation so its load→mutate→save cannot
    // interleave with another agent's and lose an update.
    let state_lock = store.state_lock();
    let _state_guard = state_lock.lock().await;

    // Mode mutex: merging a paused mission's branch would apply worker output
    // while another mode owns the tree — refuse until resumed.
    match store.load().await {
        Ok(state) => {
            if let Some(mission) = state.missions.iter().find(|m| m.branch == args.branch)
                && let Some(refusal) = crate::tools::mode_mutex::refuse_paused_mission(mission)
            {
                return err_result(refusal);
            }
        }
        Err(e) => return err_result(e),
    }

    let active_agent_ids = active_tower_agent_ids(task_runner);
    match store
        .merge_with_workers(&args.branch, active_agent_ids)
        .await
    {
        Ok(result) => {
            if result.status == crate::tools::tower::types::TowerMergeStatus::Advanced {
                let message = if result.noop {
                    format!(
                        "SURVEY NOT CLOSED: {} advanced from {} to {} while merging was being evaluated; no git merge ran and the mission was not marked merged. Re-evaluate and retry.",
                        args.branch, result.evaluated_branch_tip, result.current_branch_tip
                    )
                } else {
                    format!(
                        "PARTIAL MERGE: evaluated tip {} was merged as {}, but {} advanced to {}. The later commits were not merged and the mission was not marked merged; review the new tip and retry.",
                        result.evaluated_branch_tip,
                        result.merge_commit,
                        args.branch,
                        result.current_branch_tip
                    )
                };
                return err_result(message);
            }
            if result.noop {
                return ok_result(format!(
                    "{} is a read-only survey with a zero-diff branch — mission marked merged, no git merge needed.\nContinue with the remaining missions in Dependency Flow order.",
                    args.branch
                ));
            }
            let commit = result.merge_commit;
            let short_commit = if commit.len() >= 7 {
                &commit[..7]
            } else {
                &commit
            };
            let mut lines = vec![
                format!("merged {} (merge commit {short_commit})", args.branch),
                format!("full commit: {commit}"),
            ];
            if !result.conflicts_with.is_empty() {
                lines.push(String::new());
                lines.push("These unmerged branches changed the same files and now likely conflict with the base:".to_string());
                for (b, files) in result.conflicts_with {
                    lines.push(format!("- {b}: {}", files.join(", ")));
                }
                lines.push("Tell each affected worker (Agent resume) to rebase onto the updated base, resolve, push, and request a re-review.".to_string());
            } else {
                lines.push("The mission is now marked merged. Continue with the remaining missions in Dependency Flow order.".to_string());
            }
            ok_result(lines.join("\n"))
        }
        Err(e) => err_result(e),
    }
}

pub async fn execute_tower_teardown(
    cwd: &Path,
    caller_agent_id: &str,
    session_id: &str,
    raw_args: &str,
    task_runner: Option<&crate::storage::TaskRunner>,
    wake_runtime: Option<&Arc<wake::TowerWakeRuntime>>,
) -> ExecutableToolResult {
    if caller_agent_id != "main" {
        return err_result(TOWER_MAIN_AGENT_ONLY);
    }

    #[derive(Deserialize, Default)]
    struct TeardownArgs {
        #[serde(default)]
        force: Option<bool>,
        #[serde(default)]
        exclude: Option<Vec<String>>,
        #[serde(default)]
        dry_run: Option<bool>,
    }
    let args: TeardownArgs = serde_json::from_str(raw_args).unwrap_or_default();

    let repo_root = resolve_tower_repo_root(&cwd.to_string_lossy());
    let store = TowerStore::new(PathBuf::from(repo_root));
    // Serialize this repo's tower state against concurrent workers: hold the
    // per-repo lock across the whole operation so its load→mutate→save cannot
    // interleave with another agent's and lose an update.
    let state_lock = store.state_lock();
    let _state_guard = state_lock.lock().await;

    let live_agent_ids = task_runner
        .map(|runner| {
            runner
                .list()
                .into_iter()
                .filter(|task| {
                    task.get("kind").and_then(serde_json::Value::as_str) == Some("agent")
                        && task.get("status").and_then(serde_json::Value::as_str) == Some("running")
                })
                .filter_map(|task| {
                    task.get("agentId")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default();
    let dry_run = args.dry_run.unwrap_or(false);
    let options = TowerTeardownOptions {
        force: args.force.unwrap_or(false),
        exclude: args.exclude.unwrap_or_default(),
        dry_run,
        live_agent_ids,
    };
    match store.teardown(options).await {
        Ok(report) => {
            if !dry_run && let Some(runtime) = wake_runtime {
                let _ = runtime
                    .clear(session_id, wake::TOWER_INBOX_WAKE_VARIANT)
                    .await;
            }
            let mut lines = vec!["tower teardown:".to_string()];
            for item in report {
                lines.push(format!("- {item}"));
            }
            lines.push(String::new());
            lines.push("Tower mode remains active. Branches and .tower/comms/ (state, inbox, findings, reviews, activity log) are kept as the audit trail.".to_string());
            ok_result(lines.join("\n"))
        }
        Err(e) => err_result(e),
    }
}

/// Legacy task-notification id still recognized by the session's persisted
/// notification drain. New tower wakes use `wake::TOWER_INBOX_WAKE_VARIANT`.
pub const TOWER_WAKE_TASK_ID: &str = "tower-inbox-wake";

async fn enqueue_tower_inbox_wake(
    wake_runtime: Option<&Arc<wake::TowerWakeRuntime>>,
    task_runner: Option<&Arc<crate::storage::TaskRunner>>,
    session_id: Option<&str>,
    repo_root: &Path,
    delivery: &crate::tools::tower::types::TowerInboxDelivery,
) {
    let (Some(runtime), Some(session_id)) = (wake_runtime, session_id) else {
        return;
    };
    runtime
        .enqueue(repo_root, task_runner, session_id, delivery)
        .await;
}

pub async fn execute_tower_send(
    cwd: &Path,
    caller_agent_id: &str,
    raw_args: &str,
    task_runner: Option<&Arc<crate::storage::TaskRunner>>,
    wake_runtime: Option<&Arc<wake::TowerWakeRuntime>>,
) -> ExecutableToolResult {
    let repo_root = resolve_tower_repo_root(&cwd.to_string_lossy());
    let store = TowerStore::new(PathBuf::from(&repo_root));

    let input: TowerSendInput = match serde_json::from_str(raw_args) {
        Ok(i) => i,
        Err(e) => return err_result(format!("failed to parse TowerSend input: {e}")),
    };

    let to = input.to.clone();
    match store.send_agent(caller_agent_id, input).await {
        Ok(outcome) => {
            // A worker's message only writes an inbox file; the rich wake
            // carries its references and fresh gate digests to the session
            // that ran TowerInit. Main's own sends never wake it.
            if outcome.caller != TOWER_NAME && (to == TOWER_NAME || to == BROADCAST_NAME) {
                enqueue_tower_inbox_wake(
                    wake_runtime,
                    task_runner,
                    outcome.session_id.as_deref(),
                    Path::new(&repo_root),
                    &outcome.message.delivery,
                )
                .await;
            }
            ok_result(format!(
                "message sent to {to}\nfile: {}",
                outcome.message.item.file
            ))
        }
        Err(e) => err_result(e),
    }
}

pub async fn execute_tower_complete(
    cwd: &Path,
    caller_agent_id: &str,
    raw_args: &str,
    task_runner: Option<&Arc<crate::storage::TaskRunner>>,
    wake_runtime: Option<&Arc<wake::TowerWakeRuntime>>,
) -> ExecutableToolResult {
    #[derive(Deserialize)]
    struct CompleteArgs {
        report: String,
    }
    let args: CompleteArgs = match serde_json::from_str(raw_args) {
        Ok(args) => args,
        Err(error) => return err_result(format!("failed to parse TowerComplete input: {error}")),
    };
    let repo_root = resolve_tower_repo_root(&cwd.to_string_lossy());
    let store = TowerStore::new(PathBuf::from(&repo_root));

    match store.complete_agent(caller_agent_id, &args.report).await {
        Ok(result) => {
            if let Some(message) = result.message.as_ref()
                && caller_agent_id != "main"
                && (message.item.to == TOWER_NAME || message.item.to == BROADCAST_NAME)
            {
                let session_id = store.load().await.ok().and_then(|state| state.session_id);
                enqueue_tower_inbox_wake(
                    wake_runtime,
                    task_runner,
                    session_id.as_deref(),
                    Path::new(&repo_root),
                    &message.delivery,
                )
                .await;
            }
            let mut lines = Vec::new();
            if let Some(mission) = result.mission.as_ref() {
                lines.push(format!(
                    "mission {} marked completed ({})",
                    mission.id, mission.title
                ));
            } else {
                lines.push("no mission was marked completed".to_string());
            }
            if let Some(recovered_at) = result.recovered_at.as_deref() {
                lines.push(format!("state recovered at: {recovered_at}"));
            }
            if result.notification_created {
                lines.push("notification: stored once".to_string());
            } else if result.message.is_some() {
                lines.push(
                    "notification: reused the stored message; no duplicate was written".to_string(),
                );
            }
            if let Some(error) = result.notification_error {
                lines.push(format!("notification error: {error}"));
            }
            if let Some(error) = result.activity_log_error {
                lines.push(format!("activity log error: {error}"));
            }
            ok_result(lines.join("\n"))
        }
        Err(error) => err_result(error),
    }
}

pub async fn execute_tower_inbox(
    cwd: &Path,
    caller_agent_id: &str,
    raw_args: &str,
) -> ExecutableToolResult {
    let repo_root = resolve_tower_repo_root(&cwd.to_string_lossy());
    let store = TowerStore::new(PathBuf::from(repo_root));
    // Serialize this repo's tower state against concurrent workers: hold the
    // per-repo lock across the whole operation so its load→mutate→save cannot
    // interleave with another agent's and lose an update.
    let state_lock = store.state_lock();
    let _state_guard = state_lock.lock().await;

    let state = match store.load().await {
        Ok(s) => s,
        Err(e) => return err_result(e),
    };

    let caller = match store.resolve_caller_name(&state, caller_agent_id) {
        Ok(c) => c,
        Err(e) => return err_result(e),
    };

    #[derive(Deserialize, Default)]
    struct InboxArgs {
        limit: Option<usize>,
        include_read: Option<bool>,
    }
    let args: InboxArgs = serde_json::from_str(raw_args).unwrap_or_default();

    match store
        .read_inbox_page(
            &caller,
            args.limit.unwrap_or(20),
            args.include_read.unwrap_or(false),
        )
        .await
    {
        Ok(page) => {
            let items = page.items;
            if items.is_empty() {
                return ok_result(format!("inbox empty for {caller}"));
            }
            let mut sections = Vec::new();
            for item in &items {
                let mut header = vec![
                    format!("file: {}", item.file),
                    format!("from: {}", item.from),
                    format!("to: {}", item.to),
                    format!("subject: {}", item.subject),
                    format!("sent_at: {}", item.sent_at),
                ];
                if let Some(ref s) = item.scope {
                    header.push(format!("scope: {s}"));
                }
                if let Some(ref a) = item.action {
                    header.push(format!("action: {a}"));
                }
                header.push(String::new());
                header.push(item.body.clone());
                sections.push(header.join("\n"));
            }
            let output = format!(
                "{} message(s) for {caller} (newest first), {} unread remaining:\n\n{}",
                items.len(),
                page.unread,
                sections.join("\n\n---\n\n")
            );
            ok_result(output)
        }
        Err(e) => err_result(e),
    }
}

pub async fn execute_tower_finding(
    cwd: &Path,
    caller_agent_id: &str,
    raw_args: &str,
) -> ExecutableToolResult {
    let repo_root = resolve_tower_repo_root(&cwd.to_string_lossy());
    let store = TowerStore::new(PathBuf::from(repo_root));
    // Serialize this repo's tower state against concurrent workers: hold the
    // per-repo lock across the whole operation so its load→mutate→save cannot
    // interleave with another agent's and lose an update.
    let state_lock = store.state_lock();
    let _state_guard = state_lock.lock().await;

    let state = match store.load().await {
        Ok(s) => s,
        Err(e) => return err_result(e),
    };

    let caller = match store.resolve_caller_name(&state, caller_agent_id) {
        Ok(c) => c,
        Err(e) => return err_result(e),
    };

    let input: TowerFindingInput = match serde_json::from_str(raw_args) {
        Ok(i) => i,
        Err(e) => return err_result(format!("failed to parse TowerFinding input: {e}")),
    };

    match store.file_finding(&caller, input).await {
        Ok(rel) => ok_result(format!(
            "finding filed: {rel}\nThe tower will route it — do not fix out-of-scope issues yourself."
        )),
        Err(e) => err_result(e),
    }
}

pub async fn execute_tower_review(
    cwd: &Path,
    caller_agent_id: &str,
    raw_args: &str,
    task_runner: Option<&Arc<crate::storage::TaskRunner>>,
    wake_runtime: Option<&Arc<wake::TowerWakeRuntime>>,
) -> ExecutableToolResult {
    let repo_root = resolve_tower_repo_root(&cwd.to_string_lossy());
    let store = TowerStore::new(PathBuf::from(&repo_root));

    let state = match store.load().await {
        Ok(s) => s,
        Err(e) => return err_result(e),
    };

    let caller = match store.resolve_caller_name(&state, caller_agent_id) {
        Ok(c) => c,
        Err(e) => return err_result(e),
    };

    let input: TowerReviewInput = match serde_json::from_str(raw_args) {
        Ok(i) => i,
        Err(e) => return err_result(format!("failed to parse TowerReview input: {e}")),
    };

    match store.submit_review_detailed(&caller, input).await {
        Ok(result) => {
            for notification in &result.notifications {
                if notification.item.to == TOWER_NAME || notification.item.to == BROADCAST_NAME {
                    enqueue_tower_inbox_wake(
                        wake_runtime,
                        task_runner,
                        state.session_id.as_deref(),
                        Path::new(&repo_root),
                        &notification.delivery,
                    )
                    .await;
                }
            }
            let mut output = format!(
                "review submitted: {}\nstandard review-result notifications: {}",
                result.file,
                result.notifications.len()
            );
            for error in result.notification_errors {
                output.push_str(&format!("\nnotification error: {error}"));
            }
            ok_result(output)
        }
        Err(e) => err_result(e),
    }
}

pub async fn execute_tower_mission(
    cwd: &Path,
    caller_agent_id: &str,
    raw_args: &str,
) -> ExecutableToolResult {
    let repo_root = resolve_tower_repo_root(&cwd.to_string_lossy());
    let store = TowerStore::new(PathBuf::from(&repo_root));
    // Serialize this repo's tower state against concurrent workers (see above).
    let state_lock = store.state_lock();
    let _state_guard = state_lock.lock().await;

    let state = match store.load().await {
        Ok(s) => s,
        Err(e) => return err_result(e),
    };

    let caller = match store.resolve_caller_name(&state, caller_agent_id) {
        Ok(c) => c,
        Err(e) => return err_result(e),
    };

    #[derive(Deserialize)]
    struct MissionArgs {
        id: String,
        status: Option<crate::tools::tower::types::TowerMissionStatus>,
        note: Option<String>,
        blocker: Option<String>,
        clear_blockers: Option<bool>,
        task_done: Option<String>,
        task_drop: Option<String>,
        task_drop_reason: Option<String>,
        scope: Option<Vec<String>>,
    }
    let args: MissionArgs = match serde_json::from_str(raw_args) {
        Ok(a) => a,
        Err(e) => return err_result(format!("failed to parse TowerMission input: {e}")),
    };

    let has_patch = args.status.is_some()
        || args.note.is_some()
        || args.blocker.is_some()
        || args.clear_blockers.is_some()
        || args.task_done.is_some()
        || args.task_drop.is_some()
        || args.scope.is_some();

    if !has_patch {
        let Some(mission) = state.missions.iter().find(|m| m.id == args.id) else {
            let known = state
                .missions
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return err_result(format!(
                "unknown mission \"{}\" — known missions: {}",
                args.id,
                if known.is_empty() {
                    "(none planned yet)"
                } else {
                    &known
                }
            ));
        };
        let file_path = store.abs(&format!(
            "{MISSIONS_DIR}/{}",
            mission_file_name(&mission.id, &mission.slug)
        ));
        let content = tokio::fs::read_to_string(&file_path)
            .await
            .unwrap_or_default();
        return ok_result(content);
    }

    let patch = TowerMissionPatch {
        status: args.status,
        note: args.note,
        blocker: args.blocker,
        clear_blockers: args.clear_blockers,
        task_done: args.task_done,
        task_drop: args.task_drop.map(|text| TowerTaskDrop {
            text,
            reason: args.task_drop_reason,
        }),
        owner: None,
        scope: args.scope,
    };

    match store.update_mission(&caller, &args.id, patch).await {
        Ok(mission) => {
            let open_tasks = mission
                .tasks
                .iter()
                .filter(|t| !t.done && !t.dropped)
                .count();
            let file_path = store.abs(&format!(
                "{MISSIONS_DIR}/{}",
                mission_file_name(&mission.id, &mission.slug)
            ));
            let content = tokio::fs::read_to_string(&file_path)
                .await
                .unwrap_or_default();
            let msg = format!(
                "mission {} updated — status: {}, open tasks: {open_tasks}, blockers: {}\n\n{content}",
                mission.id,
                mission.status.as_str(),
                mission.blockers.len()
            );
            ok_result(msg)
        }
        Err(e) => err_result(e),
    }
}

pub async fn execute_tower_status(
    cwd: &Path,
    caller_agent_id: &str,
    task_runner: Option<&crate::storage::TaskRunner>,
    max_running_tasks: Option<u32>,
) -> ExecutableToolResult {
    let repo_root = resolve_tower_repo_root(&cwd.to_string_lossy());
    let store = TowerStore::new(PathBuf::from(repo_root));
    let state_lock = store.state_lock();
    let _state_guard = state_lock.lock().await;
    let active_agent_ids = (caller_agent_id == "main")
        .then(|| active_tower_agent_ids(task_runner))
        .flatten();
    match status_reader::read_tower_status(
        &store,
        caller_agent_id,
        active_agent_ids.as_ref(),
        max_running_tasks,
    )
    .await
    {
        Ok(snapshot) if !snapshot.initialized && !snapshot.state_lost => {
            err_result(status_reader::render_tower_status_summary(&snapshot))
        }
        Ok(snapshot) => ok_result(status_reader::render_tower_status_summary(&snapshot)),
        Err(error) => err_result(error),
    }
}

pub fn tower_tool_defs() -> Vec<ToolInfo> {
    tower_tool_defs_with_models(None)
}

pub fn tower_tool_defs_with_models(
    secondary_model: Option<&crate::subagent::secondary::SecondaryModelRuntime>,
) -> Vec<ToolInfo> {
    let mut spawn = ToolInfo {
        name: "TowerSpawn".into(),
        description: "Spawn a worker or reviewer tower subagent".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "required": ["name", "kind"],
            "properties": {
                "name": { "type": "string" },
                "kind": { "type": "string", "enum": ["worker", "reviewer"] },
                "mission_id": { "type": "string" },
                "review_target": { "type": "string" },
                "instructions": { "type": "string" }
            }
        }),
    };
    if let Some(pool) = secondary_model {
        spawn.description.push_str(&format!(
            "\n\n{}\nThe [default] marker is the worker pool default, not the reviewer default.",
            pool.tower_description()
        ));
        spawn.input_schema["properties"]["model"] = serde_json::json!({
            "type": "string",
            "description": "Worker: pool alias (default = pool default) or primary. Reviewer: primary by default, or an explicit pool alias."
        });
    }
    vec![
        ToolInfo {
            name: "TowerInit".into(),
            description:
                "Initialize tower multi-agent orchestration workspace in the current repository"
                    .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "base": { "type": "string", "description": "Base branch for the tower (defaults to current branch)" }
                }
            }),
        },
        ToolInfo {
            name: "TowerPlan".into(),
            description: "Plan tower missions with disjoint file scopes. Auto-wired dependencies order merges only, not spawning or editing; add explicit deps when one mission must merge after another.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["missions"],
                "properties": {
                    "missions": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "required": ["title", "scope"],
                            "properties": {
                                "title": { "type": "string" },
                                "scope": { "type": "array", "items": { "type": "string" } },
                                "tasks": { "type": "array", "items": { "type": "string" } },
                                "deps": { "type": "array", "items": { "type": "string" } },
                                "kind": { "type": "string", "enum": ["build", "survey"] }
                            }
                        }
                    }
                }
            }),
        },
        spawn,
        ToolInfo {
            name: "TowerRebase".into(),
            description: "Rebase an open build mission onto the current tower base in its registered worktree".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["mission"],
                "properties": {
                    "mission": { "type": "string" }
                }
            }),
        },
        ToolInfo {
            name: "TowerMerge".into(),
            description: "Merge a completed tower mission branch back into base".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["branch"],
                "properties": {
                    "branch": { "type": "string" }
                }
            }),
        },
        ToolInfo {
            name: "TowerTeardown".into(),
            description: "Tear down eligible tower worktrees while tower mode stays active".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "force": { "type": "boolean" },
                    "exclude": { "type": "array", "items": { "type": "string" } },
                    "dry_run": { "type": "boolean" }
                }
            }),
        },
        ToolInfo {
            name: "TowerSend".into(),
            description: "Send an inbox message to another tower agent or broadcast".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["to", "subject", "body"],
                "properties": {
                    "to": { "type": "string" },
                    "subject": { "type": "string" },
                    "body": { "type": "string" },
                    "scope": { "type": "string" },
                    "action": { "type": "string" },
                    "consent_ref": { "type": "string" }
                }
            }),
        },
        ToolInfo {
            name: "TowerComplete".into(),
            description: "Mark your mission completed and send its standard completion report atomically".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["report"],
                "properties": {
                    "report": { "type": "string", "description": "Full delivery report; build missions request review and surveys return their summary" }
                }
            }),
        },
        ToolInfo {
            name: "TowerInbox".into(),
            description: "Read messages from your tower inbox".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "limit": { "type": "integer" },
                    "include_read": { "type": "boolean" }
                }
            }),
        },
        ToolInfo {
            name: "TowerFinding".into(),
            description: "Record an out-of-scope finding for the tower to route".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["type", "title", "summary", "details", "suggested_fix"],
                "properties": {
                    "type": { "type": "string", "enum": ["bug", "improve", "vuln", "idea"] },
                    "title": { "type": "string" },
                    "severity": { "type": "string", "enum": ["low", "medium", "high", "critical"] },
                    "summary": { "type": "string" },
                    "location": { "type": "string" },
                    "details": { "type": "string" },
                    "suggested_fix": { "type": "string" }
                }
            }),
        },
        ToolInfo {
            name: "TowerReview".into(),
            description: "Submit a code review for a mission branch".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["target", "status", "merge", "findings", "decision"],
                "properties": {
                    "target": { "type": "string" },
                    "status": { "type": "string" },
                    "merge": { "type": "string", "enum": ["merge", "fix-then-merge", "hold"] },
                    "findings": { "type": "string" },
                    "checks": { "type": "array", "items": { "type": "string" } },
                    "decision": { "type": "string" }
                }
            }),
        },
        ToolInfo {
            name: "TowerMission".into(),
            description: "Inspect or update a tower mission's status, notes, or tasks".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["id"],
                "properties": {
                    "id": { "type": "string" },
                    "status": { "type": "string" },
                    "note": { "type": "string" },
                    "blocker": { "type": "string" },
                    "clear_blockers": { "type": "boolean" },
                    "task_done": { "type": "string" },
                    "task_drop": {
                        "type": "string",
                        "description": "Mark the first open task containing this text as dropped — the escape hatch for legitimately descoped tasks; requires task_drop_reason and is recorded in the mission notes and the activity log"
                    },
                    "task_drop_reason": {
                        "type": "string",
                        "description": "Mandatory with task_drop: why the task is legitimately descoped"
                    },
                    "scope": { "type": "array", "items": { "type": "string" } }
                }
            }),
        },
        ToolInfo {
            name: "TowerStatus".into(),
            description: "Check the overall status of tower missions, roster, reviews, and inbox"
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::tower::paths::STATE_FILE;
    use crate::tools::tower::types::{
        TowerAgentKind, TowerMission, TowerMissionKind, TowerMissionStatus, TowerRoster,
        TowerRosterEntry, TowerState,
    };

    fn spawn_test_mission(status: TowerMissionStatus) -> TowerMission {
        TowerMission {
            id: "M1".into(),
            title: "Mission".into(),
            slug: "mission".into(),
            kind: TowerMissionKind::Build,
            scope: vec!["src/**".into()],
            branch: "feat/mission".into(),
            worktree: "wt-1".into(),
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

    async fn save_spawn_state(root: &Path, mission: TowerMission, agents: Vec<TowerRosterEntry>) {
        let store = TowerStore::new(root);
        tokio::fs::create_dir_all(store.abs(STATE_FILE).parent().unwrap())
            .await
            .unwrap();
        store
            .save(&TowerState {
                version: 1,
                base: "main".into(),
                mode: "branch".into(),
                created_at: "2026-01-01T00:00:00Z".into(),
                session_id: Some("session-1".into()),
                roster: TowerRoster { agents },
                missions: vec![mission],
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn spawn_refuses_a_terminal_mission_before_requesting_runtime() {
        let dir = tempfile::tempdir().unwrap();
        save_spawn_state(
            dir.path(),
            spawn_test_mission(TowerMissionStatus::Merged),
            Vec::new(),
        )
        .await;
        let result = execute_tower_spawn(
            dir.path(),
            "main",
            "session-1",
            None,
            None,
            None,
            r#"{"name":"worker-1","kind":"worker","mission_id":"M1"}"#,
        )
        .await
        .expect("terminal mission must be refused before the manager is needed");
        assert!(result.is_error);
        assert!(result.content.contains("terminal"), "{}", result.content);
    }

    #[tokio::test]
    async fn dead_reviewer_requires_a_fresh_one_shot_name() {
        let dir = tempfile::tempdir().unwrap();
        let reviewer = TowerRosterEntry {
            name: "reviewer-1".into(),
            agent_id: "agent-1".into(),
            session_id: Some("session-1".into()),
            kind: TowerAgentKind::Reviewer,
            mission_id: None,
            review_target: Some("feat/mission".into()),
            review_mission_id: Some("M1".into()),
            worktree: None,
            branch: None,
            spawned_at: "2026-01-01T00:00:00Z".into(),
            died_at: Some("2026-01-01T01:00:00Z".into()),
            death_status: Some("failed".into()),
            death_reason: Some("boom".into()),
            status: Some("dead".into()),
        };
        save_spawn_state(
            dir.path(),
            spawn_test_mission(TowerMissionStatus::Completed),
            vec![reviewer],
        )
        .await;
        let result = execute_tower_spawn(
            dir.path(),
            "main",
            "session-1",
            None,
            None,
            None,
            r#"{"name":"reviewer-1","kind":"reviewer","review_target":"feat/mission"}"#,
        )
        .await
        .expect("one-shot reviewer duplicate must be refused before runtime");
        assert!(result.is_error);
        assert!(result.content.contains("fresh"), "{}", result.content);
        assert!(
            !result.content.contains("Agent(resume"),
            "{}",
            result.content
        );
    }

    #[test]
    fn spawn_result_metadata_reports_the_actual_model() {
        let lines = spawn_result_metadata(
            "worker-1",
            "kind: worker".to_string(),
            "agent-7",
            "smart/slow",
        );
        assert_eq!(lines[1], "kind: worker");
        assert_eq!(lines[5], "model: smart/slow");
    }

    #[tokio::test]
    async fn spawn_activity_log_reports_the_actual_model() {
        let dir = tempfile::tempdir().unwrap();
        let mission = spawn_test_mission(TowerMissionStatus::Active);
        save_spawn_state(dir.path(), mission.clone(), Vec::new()).await;
        let store = TowerStore::new(dir.path());
        tokio::fs::create_dir_all(
            store
                .abs(crate::tools::tower::paths::ACTIVITY_LOG)
                .parent()
                .unwrap(),
        )
        .await
        .unwrap();

        record_spawn_log(
            &store,
            "worker",
            "worker-1",
            "agent-7",
            "smart/slow",
            Some(&mission),
            None,
        )
        .await
        .unwrap();

        let log = tokio::fs::read_to_string(store.abs(crate::tools::tower::paths::ACTIVITY_LOG))
            .await
            .unwrap();
        assert!(log.contains("spawn"), "{log}");
        assert!(log.contains("model=smart/slow"), "{log}");
    }

    #[test]
    fn plan_description_scopes_auto_dependencies_to_merge_order() {
        let defs = super::tower_tool_defs();
        let plan = defs
            .iter()
            .find(|definition| definition.name == "TowerPlan")
            .unwrap();
        assert!(
            plan.description
                .contains("order merges only, not spawning or editing"),
            "{}",
            plan.description
        );
        assert!(
            plan.description.contains("explicit deps"),
            "{}",
            plan.description
        );
    }

    #[test]
    fn complete_tool_is_advertised_and_inbox_supports_history() {
        let defs = super::tower_tool_defs();
        let complete = defs
            .iter()
            .find(|definition| definition.name == "TowerComplete")
            .expect("TowerComplete must be advertised");
        assert!(complete.input_schema["properties"]["report"].is_object());
        let inbox = defs
            .iter()
            .find(|definition| definition.name == "TowerInbox")
            .unwrap();
        assert_eq!(
            inbox.input_schema["properties"]["include_read"]["type"],
            "boolean"
        );
        assert!(
            defs.iter()
                .any(|definition| definition.name == "TowerRebase")
        );
    }
}
