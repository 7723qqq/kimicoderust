pub mod concurrency;
pub mod frontmatter;
pub mod git;
pub mod mission_gate;
pub mod paths;
pub mod store;
pub mod types;

use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};

use crate::subagent::SubagentManager;
use crate::subagent::secondary::TowerModelBinding;
use crate::tools::tower::paths::{
    BROADCAST_NAME, MISSIONS_DIR, TOWER_NAME, WORKTREES_DIR, mission_file_name,
    resolve_tower_repo_root,
};
use crate::features::tower::towerRateLimitService::TowerRateLimitService as TowerRateLimit;
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
        spill: None,
        parts: Vec::new(),
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
        spill: None,
        parts: Vec::new(),
        delivery: None,
        stop_turn: false,
        content: msg.into(),
        is_error: false,
        note: None,
        display: None,
    }
}

const TOWER_WORKER_PROFILE: &str = "tower-worker";

static BRANCH_LEASES: OnceLock<StdMutex<HashSet<String>>> = OnceLock::new();

#[derive(Debug)]
struct BranchLease {
    key: String,
}

impl Drop for BranchLease {
    fn drop(&mut self) {
        let leases = BRANCH_LEASES.get_or_init(|| StdMutex::new(HashSet::new()));
        leases
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&self.key);
    }
}

fn acquire_branch_lease(repo_root: &Path, branch: &str) -> Result<BranchLease, String> {
    let key = format!("{}\u{0}{branch}", repo_root.display());
    let leases = BRANCH_LEASES.get_or_init(|| StdMutex::new(HashSet::new()));
    let mut held = leases.lock().unwrap_or_else(|error| error.into_inner());
    if !held.insert(key.clone()) {
        return Err(format!(
            "branch \"{branch}\" is leased by another tower merge/rebase/spawn — not waiting; retry after that operation finishes"
        ));
    }
    Ok(BranchLease { key })
}

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
    callbacks: Option<&dyn crate::callbacks::HostCallbacks>,
) -> ExecutableToolResult {
    if caller_agent_id != "main" {
        return err_result(TOWER_MAIN_AGENT_ONLY);
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct InitArgs {
        base: Option<String>,
    }
    let args: InitArgs = match serde_json::from_str(raw_args) {
        Ok(a) => a,
        Err(e) => return err_result(format!("failed to parse TowerInit input: {e}")),
    };

    let repo_root = resolve_tower_repo_root(&cwd.to_string_lossy());
    let store = TowerStore::new(PathBuf::from(repo_root));

    let _state_guard = match store.lock_state_process("tool call").await {
        Ok(guard) => guard,
        Err(error) => return err_result(error),
    };

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

            if let Some(callbacks) = callbacks {
                callbacks.emit_event(serde_json::json!({
                    "type": "tower_mode.enter",
                    "agentId": caller_agent_id,
                    "sessionId": session_id,
                }));
                callbacks.telemetry(serde_json::json!({
                    "event": "tower_mode_enter",
                    "outcome": "entered",
                }));
                crate::features::helloWorld::agent::helloWorldService::HelloWorldService::exit(
                    callbacks,
                    caller_agent_id,
                );
            }
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
    #[serde(deny_unknown_fields)]
    struct PlanArgs {
        missions: Vec<TowerPlanInput>,
    }
    let args: PlanArgs = match serde_json::from_str(raw_args) {
        Ok(a) => a,
        Err(e) => return err_result(format!("failed to parse TowerPlan input: {e}")),
    };

    let repo_root = resolve_tower_repo_root(&cwd.to_string_lossy());
    let store = TowerStore::new(PathBuf::from(repo_root));

    let _state_guard = match store.lock_state_process("tool call").await {
        Ok(guard) => guard,
        Err(error) => return err_result(error),
    };

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
    #[serde(deny_unknown_fields)]
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

    if args.name.trim().is_empty() || args.name.trim() != args.name {
        return Some(err_result(format!(
            "tower agent name \"{}\" must not be blank or carry surrounding whitespace",
            args.name
        )));
    }
    if args.name == TOWER_NAME || args.name == BROADCAST_NAME {
        return Some(err_result(format!(
            "tower agent name \"{}\" is reserved by the tower protocol — pick a different name",
            args.name
        )));
    }

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
            let Some(mut mission) = state.missions.iter().find(|m| &m.id == mid).cloned() else {
                let known = state
                    .missions
                    .iter()
                    .map(|m| m.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Some(err_result(format!(
                    "unknown mission \"{mid}\" — known missions: {}",
                    if known.is_empty() {
                        "(none planned yet)"
                    } else {
                        &known
                    }
                )));
            };
            let _branch_lease = match acquire_branch_lease(&store.repo_root, &mission.branch) {
                Ok(lease) => lease,
                Err(e) => return Some(err_result(e)),
            };
            if !mission.status.is_open() {
                return Some(err_result(format!(
                    "mission {mid} is {} — terminal missions cannot spawn workers; no worktree or agent was created",
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

            let added = match store
                .add_worktree(&mission.worktree, &mission.branch, &state.base)
                .await
            {
                Ok(added) => added,
                Err(e) => return Some(err_result(format!("failed to create worktree: {e}"))),
            };
            if let Some(ref spawn_base) = added.spawn_base {
                if let Err(e) = store
                    .set_mission_spawn_base(&mission.worktree, &mission.branch, spawn_base)
                    .await
                {
                    return Some(err_result(format!(
                        "failed to record the base snapshot on mission {mid}: {e}"
                    )));
                }
                mission.spawn_base = Some(spawn_base.clone());
            }

            let worktree_abs = store.abs(&format!("{WORKTREES_DIR}/{}", mission.worktree));
            let prompt = build_worker_prompt(
                &args.name,
                &mission,
                &worktree_abs,
                &state.base,
                args.instructions.as_deref(),
            );
            let agent_id = match manager
                .spawn_in_environment(
                    TOWER_WORKER_PROFILE,
                    &args.name,
                    crate::environment::AgentEnvironmentRequest {
                        cwd: Some(worktree_abs.clone()),
                        env: std::collections::BTreeMap::new(),
                    },
                )
                .await
            {
                Ok(id) => id,
                Err(e) => return Some(err_result(format!("failed to spawn worker: {e}"))),
            };
            manager.set_instance_llm(&agent_id, model.llm.clone()).await;

            manager.bind_permission_mode(&agent_id, crate::permission::PermissionMode::Auto);

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
            ]);
            if let Some(ref spawn_base) = mission.spawn_base {
                let short = &spawn_base[..spawn_base.len().min(7)];
                lines.push(format!(
                    "base snapshot: {short} — the base checkout had uncommitted changes; they are committed as the branch's first commit (the checkout itself was left untouched)"
                ));
            }
            lines.extend([
                String::new(),
                "The worker runs detached in the background; its completion arrives as a notification. Track progress with TowerStatus / TowerInbox. If it dies, diagnose the death status/reason and task state first. Resume or reassign only for transient causes (lost contact, timeout, OOM); fix or escalate systematic defects first. A user-stopped worker is never resumed, reassigned, or replaced.".to_string(),
            ]);

            Some(ok_result(lines.join("\n")))
        }
        "reviewer" => {
            let Some(ref target) = args.review_target else {
                return Some(err_result("reviewer spawns require review_target"));
            };
            match git::branch_exists(&store.repo_root, target).await {
                Ok(true) => {}
                Ok(false) => {
                    return Some(err_result(format!("branch \"{target}\" does not exist")));
                }
                Err(error) => {
                    return Some(err_result(format!(
                        "cannot determine whether branch \"{target}\" exists: {error}"
                    )));
                }
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
                    "mission {} is {} — terminal missions cannot spawn reviewers; no worktree or agent was created",
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
            let artifact = state
                .missions
                .iter()
                .find(|m| &m.branch == target)
                .is_some_and(|m| m.kind == crate::tools::tower::types::TowerMissionKind::Artifact);
            let prompt = build_reviewer_prompt(
                &args.name,
                target,
                &state.base,
                &store.repo_root,
                author,
                artifact,
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

            manager.bind_permission_mode(&agent_id, crate::permission::PermissionMode::Auto);

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
        "# Your workplace\n- Your private git worktree: {}\n- Your branch: {} (base: {})\n{}- Address the worktree explicitly for code operations.\n- Scope: {}\n\n",
        worktree.to_string_lossy(),
        mission.branch,
        base,
        mission
            .spawn_base
            .as_ref()
            .map(|spawn_base| {
                let short = &spawn_base[..spawn_base.len().min(7)];
                format!("- Your branch starts from snapshot commit {short}: the base checkout's uncommitted changes (WIP), captured at spawn so you can build on them. That commit is your foundation — never revert, amend, or claim it as your own work; your own commits go on top of it.\n")
            })
            .unwrap_or_default(),
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
        let artifact = mission.kind == crate::tools::tower::types::TowerMissionKind::Artifact;
        let deliver_wording = if artifact {
            "what to deliver"
        } else {
            "what to build"
        };
        let step1 = if artifact {
            "git add + git commit every artifact needed for acceptance in the worktree, inside your scope. Uncommitted files are excluded from acceptance and merge."
        } else {
            "Commit your changes in the worktree."
        };
        let step3 = if artifact {
            "TowerComplete(report=what you delivered and why, reconciled against every mission task, key_artifacts=[the committed paths of the key deliverables the human should inspect]). The tower presents those key artifacts and explains that approval authorizes the entire tip; no independent reviewer is required. If you commit anything after TowerComplete, run it again with the updated report and key_artifacts — acceptance is bound to the last completion tip."
        } else {
            "TowerComplete(report=what changed and why, reconciled against every mission task)"
        };
        let step4 = if artifact {
            "key artifact paths, key decisions, open follow-ups."
        } else {
            "files changed, key decisions, open follow-ups."
        };
        format!(
            "You are \"{name}\", a tower worker agent in a multi-agent workspace.\n\n\
            {workplace}\
            # Your mission\nMission {}: {}\n\n\
            # Communication protocol\n\
            - Coordinate via TowerSend / TowerInbox / TowerFinding / TowerMission / TowerStatus; never create or edit protocol files under .tower/comms by hand.\n\
            - Keep tasks up to date with TowerMission; file out-of-scope discoveries with TowerFinding.\n\
            - Escalate substantive ambiguity to the tower with TowerSend before acting; never ask the user directly.\n\n\
            # When done\n\
            1. {step1}\n\
            2. Call TowerInbox and incorporate anything new; completion is refused while unread messages wait. Re-read TowerMission for mid-flight changes.\n\
            3. {step3}\n\
            4. Finish with a structured summary: {step4}\n\
            If the mission and its Context leave substantive doubt about {deliver_wording}, ask before acting. Split long blocking work into chunks and check TowerInbox between chunks.{extra}",
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
    artifact: bool,
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

    let verdict_scope = if artifact {
        " This independent review is advisory only — it cannot replace explicit human acceptance recorded by reviewer=tower on the exact branch tip."
    } else {
        ""
    };

    format!(
        "You are \"{name}\", a tower reviewer agent.\n\n\
        # Your assignment\n\
        Review branch \"{target}\" against base \"{base}\" in {}.\n\
        - Work read-only via git diff / git log.\n\n\
        # When done\n\
        1. TowerReview(target=\"{target}\", status=\"clean\", merge=\"merge\", ...).{verdict_scope}\n\
        2. {notify}{extra}",
        repo_root.to_string_lossy()
    )
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
    #[serde(deny_unknown_fields)]
    struct RebaseArgs {
        mission: String,
    }
    let args: RebaseArgs = match serde_json::from_str(raw_args) {
        Ok(args) => args,
        Err(error) => return err_result(format!("failed to parse TowerRebase input: {error}")),
    };
    let repo_root = resolve_tower_repo_root(&cwd.to_string_lossy());
    let store = TowerStore::new(PathBuf::from(repo_root));

    let lease_branch = store.load().await.ok().and_then(|state| {
        state
            .missions
            .iter()
            .find(|mission| mission.id == args.mission)
            .map(|mission| mission.branch.clone())
    });
    let _branch_lease = match lease_branch
        .as_deref()
        .map(|branch| acquire_branch_lease(&store.repo_root, branch))
    {
        Some(Ok(lease)) => Some(lease),
        Some(Err(error)) => return err_result(error),
        None => None,
    };

    let _state_guard = match store.lock_state_process("tool call").await {
        Ok(guard) => guard,
        Err(error) => return err_result(error),
    };
    match store
        .rebase_with_workers(
            TOWER_NAME,
            &args.mission,
            crate::features::tower::towerService::active_tower_agent_ids(task_runner),
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
    #[serde(deny_unknown_fields)]
    struct MergeArgs {
        branch: String,
    }
    let args: MergeArgs = match serde_json::from_str(raw_args) {
        Ok(a) => a,
        Err(e) => return err_result(format!("failed to parse TowerMerge input: {e}")),
    };

    let repo_root = resolve_tower_repo_root(&cwd.to_string_lossy());
    let store = TowerStore::new(PathBuf::from(repo_root));

    let _branch_lease = match acquire_branch_lease(&store.repo_root, &args.branch) {
        Ok(lease) => lease,
        Err(error) => return err_result(error),
    };

    let _state_guard = match store.lock_state_process("tool call").await {
        Ok(guard) => guard,
        Err(error) => return err_result(error),
    };

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

    let active_agent_ids = crate::features::tower::towerService::active_tower_agent_ids(task_runner);
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
    wake_runtime: Option<&Arc<crate::features::tower::wake::TowerWakeRuntime>>,
    callbacks: Option<&dyn crate::callbacks::HostCallbacks>,
) -> ExecutableToolResult {
    if caller_agent_id != "main" {
        return err_result(TOWER_MAIN_AGENT_ONLY);
    }

    #[derive(Deserialize, Default)]
    #[serde(deny_unknown_fields)]
    struct TeardownArgs {
        #[serde(default)]
        force: Option<bool>,
        #[serde(default)]
        exclude: Option<Vec<String>>,
        #[serde(default)]
        dry_run: Option<bool>,
    }
    let args: TeardownArgs = match serde_json::from_str(raw_args) {
        Ok(a) => a,
        Err(e) => return err_result(format!("failed to parse TowerTeardown input: {e}")),
    };

    let repo_root = resolve_tower_repo_root(&cwd.to_string_lossy());
    let store = TowerStore::new(PathBuf::from(repo_root));

    let _state_guard = match store.lock_state_process("tool call").await {
        Ok(guard) => guard,
        Err(error) => return err_result(error),
    };

    let state = match store.load().await {
        Ok(state) => state,
        Err(error) => return err_result(error),
    };
    if let Some(previous) = state
        .session_id
        .as_deref()
        .filter(|owner| *owner != session_id)
    {
        if crate::session::lock::SESSION_LOCKS.held(previous)
            || crate::session::lock::SESSION_LOCKS
                .occupied_by_other(previous)
                .is_some()
        {
            return err_result(format!(
                "tower workspace is owned by a live session ({previous}) — tearing it down would dismantle that session's fleet. Use TowerTeardown from that session, or close it first."
            ));
        }
    }

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
            if !dry_run {
                if let Some(runtime) = wake_runtime {
                    let _ = runtime
                        .clear(session_id, crate::features::tower::wake::TOWER_INBOX_WAKE_VARIANT)
                        .await;
                }

                let _ = store.release(session_id).await;
            }
            let mut lines = vec![
                if dry_run {
                    "tower teardown (dry run — nothing was changed):"
                } else {
                    "tower teardown:"
                }
                .to_string(),
            ];
            for item in report {
                lines.push(format!("- {item}"));
            }
            lines.push(String::new());
            lines.push("Tower mode stays active — the next objective starts with TowerInit, and the human can turn the mode off with /tower off. .tower/comms/ (state, inbox, findings, reviews, activity log) is kept as the audit trail — remove it by hand only if you are sure.".to_string());
            let content = lines.join("\n");

            if !dry_run && let Some(callbacks) = callbacks {
                callbacks.emit_event(serde_json::json!({
                    "type": "tower_mode.exit",
                    "agentId": caller_agent_id,
                }));
                callbacks.telemetry(serde_json::json!({
                    "event": "tower_mode_exit",
                    "reason": "user",
                }));
            }
            ok_result(content)
        }
        Err(e) => err_result(e),
    }
}

async fn enqueue_tower_inbox_wake(
    wake_runtime: Option<&Arc<crate::features::tower::wake::TowerWakeRuntime>>,
    task_runner: Option<&Arc<crate::storage::TaskRunner>>,
    session_id: Option<&str>,
    repo_root: &Path,
    delivery: &crate::tools::tower::types::TowerInboxDelivery,
) {
    let (Some(runtime), Some(session_id)) = (wake_runtime, session_id) else {
        return;
    };
    let delivery = crate::tools::tower::types::TowerInboxDelivery {
        from: delivery.from.clone(),
        to: delivery.to.clone(),
        subject: delivery.subject.clone(),
        message_id: delivery.message_id.clone(),
        file: delivery.file.clone(),
        sent_at: delivery.sent_at.clone(),
        mission_id: delivery.mission_id.clone(),
    };
    runtime
        .enqueue(repo_root, task_runner, session_id, &delivery)
        .await;
}

pub async fn execute_tower_send(
    cwd: &Path,
    caller_agent_id: &str,
    raw_args: &str,
    task_runner: Option<&Arc<crate::storage::TaskRunner>>,
    wake_runtime: Option<&Arc<crate::features::tower::wake::TowerWakeRuntime>>,
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

            let delivery = if outcome.caller == TOWER_NAME
                && let Some(agent_id) = outcome.target_agent_id.as_deref()
                && task_runner.is_some_and(|runner| {
                    !runner.list().into_iter().any(|task| {
                        matches!(
                            task.get("kind").and_then(serde_json::Value::as_str),
                            Some("subagent" | "agent")
                        ) && task.get("status").and_then(serde_json::Value::as_str)
                            == Some("running")
                            && task.get("agentId").and_then(serde_json::Value::as_str)
                                == Some(agent_id)
                    })
                }) {
                format!(
                    "\nnote: {to} has no running task in this session — the message sits in its inbox until you deliver it with Agent(resume=\"{agent_id}\", run_in_background=true, prompt=\"...\")"
                )
            } else {
                String::new()
            };
            let mut lines = vec![format!(
                "message sent to {to}\nfile: {}{delivery}",
                outcome.message.item.file
            )];
            if outcome.placeholder {
                lines.push(format!(
                    "note: the tower state was recovered after a loss, so your roster entry was reconstructed as placeholder \"{}\" — your original name and mission assignment are gone; the recovered state's recoveredAt marker reminds the tower to report the history loss to the user",
                    outcome.caller
                ));
            }
            let activity_log_error = outcome.message.activity_log_error.as_deref();
            if let Some(error) = activity_log_error {
                lines.push(format!("activity log error: {error}"));
            }
            let content = lines.join("\n");

            if activity_log_error.is_some() {
                err_result(content)
            } else {
                ok_result(content)
            }
        }
        Err(e) => err_result(e),
    }
}

pub async fn execute_tower_complete(
    cwd: &Path,
    caller_agent_id: &str,
    raw_args: &str,
    task_runner: Option<&Arc<crate::storage::TaskRunner>>,
    wake_runtime: Option<&Arc<crate::features::tower::wake::TowerWakeRuntime>>,
) -> ExecutableToolResult {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct CompleteArgs {
        report: String,
        #[serde(default)]
        key_artifacts: Option<Vec<String>>,
    }
    let args: CompleteArgs = match serde_json::from_str(raw_args) {
        Ok(args) => args,
        Err(error) => return err_result(format!("failed to parse TowerComplete input: {error}")),
    };
    let repo_root = resolve_tower_repo_root(&cwd.to_string_lossy());
    let store = TowerStore::new(PathBuf::from(&repo_root));

    match store
        .complete_agent(
            caller_agent_id,
            &args.report,
            args.key_artifacts.as_deref().unwrap_or_default(),
        )
        .await
    {
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
            let has_error =
                result.notification_error.is_some() || result.activity_log_error.is_some();
            if let Some(error) = result.notification_error {
                lines.push(format!("notification error: {error}"));
            }
            if let Some(error) = result.activity_log_error {
                lines.push(format!("activity log error: {error}"));
            }
            let content = lines.join("\n");

            if has_error {
                err_result(content)
            } else {
                ok_result(content)
            }
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

    let _state_guard = match store.lock_state_process("tool call").await {
        Ok(guard) => guard,
        Err(error) => return err_result(error),
    };

    let state = match store.load().await {
        Ok(s) => s,
        Err(e) => return err_result(e),
    };

    let caller = match store.resolve_caller_name(&state, caller_agent_id) {
        Ok(c) => c,
        Err(e) => return err_result(e),
    };

    #[derive(Deserialize, Default)]
    #[serde(deny_unknown_fields)]
    struct InboxArgs {
        limit: Option<usize>,
        include_read: Option<bool>,
    }
    let args: InboxArgs = match serde_json::from_str(raw_args) {
        Ok(a) => a,
        Err(e) => return err_result(format!("failed to parse TowerInbox input: {e}")),
    };

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

    let _state_guard = match store.lock_state_process("tool call").await {
        Ok(guard) => guard,
        Err(error) => return err_result(error),
    };

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
    wake_runtime: Option<&Arc<crate::features::tower::wake::TowerWakeRuntime>>,
    non_interactive: bool,
    permission_mode: Option<crate::permission::PermissionMode>,
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

    let artifact_mission = state
        .missions
        .iter()
        .find(|mission| {
            mission.branch == input.target
                && mission.kind == crate::tools::tower::types::TowerMissionKind::Artifact
                && mission.status != crate::tools::tower::types::TowerMissionStatus::Merged
                && mission.status != crate::tools::tower::types::TowerMissionStatus::Abandoned
        })
        .cloned();
    if caller == TOWER_NAME
        && input.status == "clean"
        && input.merge == "merge"
        && (non_interactive || permission_mode == Some(crate::permission::PermissionMode::Auto))
        && let Some(mission) = artifact_mission.as_ref()
    {
        return err_result(format!(
            "artifact acceptance for {} cannot be recorded in an automatic or noninteractive run — leave mission {} completed and waiting for explicit human approval in an interactive session",
            input.target, mission.id
        ));
    }

    let target = input.target.clone();
    let status = input.status.clone();
    let merge = input.merge.clone();
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
            let notified = if result.notifications.is_empty() {
                "(none)".to_string()
            } else {
                result
                    .notifications
                    .iter()
                    .map(|notification| notification.item.to.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let mut lines = vec![
                format!("review submitted: {}", result.file),
                format!("notified: {notified}"),
            ];
            if !result.notification_errors.is_empty() {
                lines.push(format!(
                    "notification error: {}",
                    result.notification_errors.join(" | ")
                ));
            }
            if !result.activity_log_errors.is_empty() {
                lines.push(format!(
                    "activity log error: {}",
                    result.activity_log_errors.join(" | ")
                ));
            }
            let next = match artifact_mission.as_ref() {
                Some(mission) => {
                    if status == "clean" && merge == "merge" {
                        if caller == TOWER_NAME
                            && result.review.mission.as_deref() == Some(mission.id.as_str())
                        {
                            format!(
                                "explicit human acceptance for {target} is recorded on the exact tip — the tower can TowerMerge it in Dependency Flow order."
                            )
                        } else {
                            format!(
                                "{target} is still waiting for mission-matching explicit human acceptance recorded by reviewer=tower; this review alone does not make the artifact merge-ready."
                            )
                        }
                    } else if status == "clean" {
                        format!(
                            "{target} is not merge-approved — artifact acceptance requires status \"clean\" with merge verdict \"merge\", recorded by reviewer=tower after explicit user approval."
                        )
                    } else {
                        format!(
                            "{target} is not merge-ready — the author must rework the artifacts, then the tower must obtain fresh explicit human acceptance on the exact tip."
                        )
                    }
                }
                None => {
                    if status == "clean" && merge == "merge" {
                        format!(
                            "{target} is merge-ready — the tower can TowerMerge it in Dependency Flow order."
                        )
                    } else if status == "clean" {
                        if merge == "hold" {
                            format!(
                                "{target} is blocked by the latest review's \"hold\" verdict — the merge gate rejects it; do not TowerMerge until a later clean review with merge verdict \"merge\" supersedes it."
                            )
                        } else {
                            format!(
                                "{target} is not merge-approved — \"fix-then-merge\" is not approval; the merge gate requires a clean review with merge verdict \"merge\" before TowerMerge."
                            )
                        }
                    } else {
                        format!(
                            "{target} is not merge-ready — the gate requires a clean review with merge verdict \"merge\" on the exact tip; the author must fix and re-review."
                        )
                    }
                }
            };
            lines.push(format!("next: {next}"));
            let has_error =
                !result.notification_errors.is_empty() || !result.activity_log_errors.is_empty();
            let output = lines.join("\n");
            if has_error {
                err_result(output)
            } else {
                ok_result(output)
            }
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

    let _state_guard = match store.lock_state_process("tool call").await {
        Ok(guard) => guard,
        Err(error) => return err_result(error),
    };

    let state = match store.load().await {
        Ok(s) => s,
        Err(e) => return err_result(e),
    };

    let caller = match store.resolve_caller_name(&state, caller_agent_id) {
        Ok(c) => c,
        Err(e) => return err_result(e),
    };

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
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
    raw_args: &str,
    task_runner: Option<&crate::storage::TaskRunner>,
    max_running_tasks: Option<u32>,
) -> ExecutableToolResult {
    #[derive(Deserialize, Default)]
    #[serde(deny_unknown_fields)]
    struct StatusArgs {
        limit: Option<usize>,
    }
    let args: StatusArgs = match serde_json::from_str(raw_args) {
        Ok(args) => args,
        Err(error) => return err_result(format!("failed to parse TowerStatus input: {error}")),
    };
    let limit = args.limit.unwrap_or(20).max(1);
    let repo_root = resolve_tower_repo_root(&cwd.to_string_lossy());
    let store = TowerStore::new(PathBuf::from(repo_root));

    let _state_guard = match store.lock_state_process("tool call").await {
        Ok(guard) => guard,
        Err(error) => return err_result(error),
    };
    let active_agent_ids = (caller_agent_id == "main")
        .then(|| crate::features::tower::towerService::active_tower_agent_ids(task_runner))
        .flatten();
    let running_agent_ids = move || active_agent_ids.clone().unwrap_or_default();
    let limiter = TowerRateLimit::global().snapshot(max_running_tasks);
    let concurrency = move || {
        crate::features::tower::tools::status::statusReader::TowerStatusConcurrency {
            budget: limiter.budget as f64,
            inflight: limiter.inflight as f64,
            blocked_until: limiter.blocked_until.map(|until| until as f64),
        }
    };
    let input = crate::features::tower::tools::status::statusReader::ReadTowerStatusInput {
        active: true,
        cwd,
        agent_id: caller_agent_id,
        running_agent_ids: &running_agent_ids,
        concurrency: &concurrency,
    };
    match crate::features::tower::tools::status::statusReader::read_tower_status(&input).await {
        Ok(status) => {
            let (text, is_error) =
                crate::features::tower::tools::status::statusTool::render_status_text(&status, limit);
            if is_error {
                err_result(text)
            } else {
                ok_result(text)
            }
        }
        Err(error) => err_result(error),
    }
}

const TOWER_INIT_DESCRIPTION: &str = include_str!("descriptions/init.md");
const TOWER_PLAN_DESCRIPTION: &str = include_str!("descriptions/plan.md");
const TOWER_SPAWN_DESCRIPTION: &str = include_str!("descriptions/spawn.md");
const TOWER_REBASE_DESCRIPTION: &str = include_str!("descriptions/rebase.md");
const TOWER_MERGE_DESCRIPTION: &str = include_str!("descriptions/merge.md");
const TOWER_TEARDOWN_DESCRIPTION: &str = include_str!("descriptions/teardown.md");
const TOWER_SEND_DESCRIPTION: &str = include_str!("descriptions/send.md");
const TOWER_COMPLETE_DESCRIPTION: &str = include_str!("descriptions/complete.md");
const TOWER_INBOX_DESCRIPTION: &str = include_str!("descriptions/inbox.md");
const TOWER_FINDING_DESCRIPTION: &str = include_str!("descriptions/finding.md");
const TOWER_REVIEW_DESCRIPTION: &str = include_str!("descriptions/review.md");
const TOWER_MISSION_DESCRIPTION: &str = include_str!("descriptions/mission.md");
const TOWER_STATUS_DESCRIPTION: &str = include_str!("descriptions/status.md");

pub fn tower_tool_defs() -> Vec<ToolInfo> {
    tower_tool_defs_with_models(None)
}

pub fn tower_tool_defs_with_models(
    secondary_model: Option<&crate::subagent::secondary::SecondaryModelRuntime>,
) -> Vec<ToolInfo> {
    let mut spawn = ToolInfo {
        name: "TowerSpawn".into(),
        description: TOWER_SPAWN_DESCRIPTION.into(),
        input_schema: serde_json::json!({
            "type": "object",
            "required": ["name", "kind"],
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Unique tower name for the agent (e.g. \"agent-build\", \"reviewer-a\"). Used for inbox addressing and mission ownership."
                },
                "kind": {
                    "type": "string",
                    "enum": ["worker", "reviewer"],
                    "description": "workers execute a mission in their worktree; reviewers review one branch"
                },
                "mission_id": {
                    "type": "string",
                    "description": "Required for workers: the mission id (e.g. \"M1\") from TowerPlan"
                },
                "review_target": {
                    "type": "string",
                    "description": "Required for reviewers: the branch to review (e.g. \"feat/vulkan-build\")"
                },
                "instructions": {
                    "type": "string",
                    "description": "Extra tower instructions appended to the generated briefing"
                }
            },
            "additionalProperties": false
        }),
    };
    if let Some(pool) = secondary_model {
        spawn
            .description
            .push_str(&format!("\n\n{}", pool.tower_description()));
        spawn.input_schema["properties"]["model"] = serde_json::json!({
            "type": "string",
            "description": "Model for this new spawn: one of the aliases listed under \"Available models\" in this tool description, or \"primary\" for your current model and thinking level. Pass this only when the user explicitly requests a model; when omitted, Tower keeps its worker/reviewer defaults."
        });
    }
    vec![
        ToolInfo {
            name: "TowerInit".into(),
            description: TOWER_INIT_DESCRIPTION.into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "base": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Local branch that missions fork from and merge back into (e.g. \"develop\"). Defaults to the branch currently checked out in the main worktree. On an existing workspace an explicit base re-anchors it — allowed anytime, even with open missions, whose branches then rebase onto the new base before merging. Remote-tracking refs (e.g. \"origin/main\") and tags are not accepted — create a local branch first."
                    }
                },
                "additionalProperties": false
            }),
        },
        ToolInfo {
            name: "TowerPlan".into(),
            description: TOWER_PLAN_DESCRIPTION.into(),
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["missions"],
                "properties": {
                    "missions": {
                        "type": "array",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "required": ["title", "scope"],
                            "properties": {
                                "title": {
                                    "type": "string",
                                    "description": "Short mission title; becomes the branch/worktree slug. Printable ASCII English only — any non-ASCII character is rejected. Include a unique identifier word (a business code, a module name, a ticket id)."
                                },
                                "scope": {
                                    "type": "array",
                                    "minItems": 1,
                                    "items": { "type": "string" },
                                    "description": "Files/globs this mission may touch (e.g. \"src/build/**\"). Scopes of different build and artifact missions should be disjoint; when they overlap, the store auto-adds a dependency that serializes their merges."
                                },
                                "tasks": {
                                    "type": "array",
                                    "items": { "type": "string" },
                                    "description": "Checklist the worker will tick off via TowerMission task_done — write each task as a verifiable item a reviewer can map to the diff"
                                },
                                "context": {
                                    "type": "string",
                                    "description": "The user's own key sentences about this mission, copied verbatim — the tower's paraphrase supplements them, never replaces them. Fill this whenever the requirement could be misread; never paste the full conversation history."
                                },
                                "deps": {
                                    "type": "array",
                                    "items": { "type": "string" },
                                    "description": "Mission ids (e.g. \"M1\") that must merge before this one can merge"
                                },
                                "kind": {
                                    "type": "string",
                                    "enum": ["build", "survey", "artifact"],
                                    "description": "\"survey\" = read-only, zero-diff investigation with no reserved scope or review. \"artifact\" = committed deliverables with reserved scope and explicit human acceptance instead of independent review. Default \"build\"."
                                }
                            },
                            "additionalProperties": false
                        }
                    }
                },
                "additionalProperties": false
            }),
        },
        spawn,
        ToolInfo {
            name: "TowerRebase".into(),
            description: TOWER_REBASE_DESCRIPTION.into(),
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["mission"],
                "properties": {
                    "mission": {
                        "type": "string",
                        "description": "Mission id (e.g. \"M3\") whose branch should be rebased onto the current base"
                    }
                },
                "additionalProperties": false
            }),
        },
        ToolInfo {
            name: "TowerMerge".into(),
            description: TOWER_MERGE_DESCRIPTION.into(),
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["branch"],
                "properties": {
                    "branch": {
                        "type": "string",
                        "description": "The mission branch to merge into the base branch (e.g. \"feat/vulkan-build\")"
                    }
                },
                "additionalProperties": false
            }),
        },
        ToolInfo {
            name: "TowerTeardown".into(),
            description: TOWER_TEARDOWN_DESCRIPTION.into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "force": {
                        "type": "boolean",
                        "description": "Remove worktrees even when they contain uncommitted changes — a worktree whose roster agent is still running is kept regardless"
                    },
                    "exclude": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Worktree names to keep — they are skipped and reported, whatever their state"
                    },
                    "dry_run": {
                        "type": "boolean",
                        "description": "Print the would-remove / would-keep list with reasons without changing anything on disk or in state"
                    }
                },
                "additionalProperties": false
            }),
        },
        ToolInfo {
            name: "TowerSend".into(),
            description: TOWER_SEND_DESCRIPTION.into(),
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["to", "subject", "body"],
                "properties": {
                    "to": {
                        "type": "string",
                        "description": "Recipient: a roster agent name, \"tower\", or \"all\" (broadcast)"
                    },
                    "subject": {
                        "type": "string",
                        "description": "One-line subject; keep it greppable"
                    },
                    "body": {
                        "type": "string",
                        "description": "Full message body (markdown)"
                    },
                    "scope": {
                        "type": "string",
                        "description": "Optional scope tag (e.g. the mission id)"
                    },
                    "action": {
                        "type": "string",
                        "description": "Optional action tag for machine routing"
                    },
                    "consent_ref": {
                        "type": "string",
                        "description": "Optional reference to a consent/approval record this message relies on"
                    }
                },
                "additionalProperties": false
            }),
        },
        ToolInfo {
            name: "TowerComplete".into(),
            description: TOWER_COMPLETE_DESCRIPTION.into(),
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["report"],
                "properties": {
                    "report": {
                        "type": "string",
                        "description": "Full completion report for the tower and the next review round"
                    },
                    "key_artifacts": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Required for artifact missions: committed paths of the key deliverables for human acceptance. Approval covers the entire completion tip, not only these paths"
                    }
                },
                "additionalProperties": false
            }),
        },
        ToolInfo {
            name: "TowerInbox".into(),
            description: TOWER_INBOX_DESCRIPTION.into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "limit": {
                        "type": "integer",
                        "exclusiveMinimum": 0,
                        "description": "Max messages to return (default 20), newest first"
                    },
                    "include_read": {
                        "type": "boolean",
                        "description": "Include previously acknowledged messages for history lookup; only returned messages are acknowledged"
                    }
                },
                "additionalProperties": false
            }),
        },
        ToolInfo {
            name: "TowerFinding".into(),
            description: TOWER_FINDING_DESCRIPTION.into(),
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["type", "title", "summary", "details", "suggested_fix"],
                "properties": {
                    "type": {
                        "type": "string",
                        "enum": ["bug", "improve", "vuln", "idea"],
                        "description": "Finding category"
                    },
                    "title": {
                        "type": "string",
                        "description": "Short finding title"
                    },
                    "severity": {
                        "type": "string",
                        "enum": ["low", "medium", "high", "critical"]
                    },
                    "summary": {
                        "type": "string",
                        "description": "What was found, in a sentence or two"
                    },
                    "location": {
                        "type": "string",
                        "description": "File/symbol the finding concerns"
                    },
                    "details": {
                        "type": "string",
                        "description": "Full details: evidence, reproduction, impact"
                    },
                    "suggested_fix": {
                        "type": "string",
                        "description": "What you would do about it"
                    }
                },
                "additionalProperties": false
            }),
        },
        ToolInfo {
            name: "TowerReview".into(),
            description: TOWER_REVIEW_DESCRIPTION.into(),
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["target", "status", "merge", "findings", "decision"],
                "properties": {
                    "target": {
                        "type": "string",
                        "description": "The branch you were assigned to review"
                    },
                    "status": {
                        "type": "string",
                        "pattern": "^(clean|p[12]-\\d+items)$",
                        "description": "Verdict: \"clean\", or \"p1-Nitems\" / \"p2-Nitems\" with the number of findings at that priority"
                    },
                    "merge": {
                        "type": "string",
                        "enum": ["merge", "fix-then-merge", "hold"],
                        "description": "Merge recommendation for the tower"
                    },
                    "findings": {
                        "type": "string",
                        "description": "Full findings text (markdown); write \"none\" when clean"
                    },
                    "checks": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Checklist items you verified (e.g. \"tests pass\", \"no secrets\")"
                    },
                    "decision": {
                        "type": "string",
                        "description": "The reasoning behind your verdict"
                    },
                    "acceptance_tip": {
                        "type": "string",
                        "description": "Artifact missions only: the completion tip from the TowerComplete acceptance-request the human approved. Required for a clean merge verdict on an artifact mission."
                    }
                },
                "additionalProperties": false
            }),
        },
        ToolInfo {
            name: "TowerMission".into(),
            description: TOWER_MISSION_DESCRIPTION.into(),
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["id"],
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "Mission id (e.g. \"M1\")"
                    },
                    "status": {
                        "type": "string",
                        "enum": ["planned", "active", "completed", "blocked", "paused", "merged", "abandoned"],
                        "description": "New lifecycle status. Workers may update only their own mission. \"abandoned\" is tower-only and gives the mission up without merging. \"merged\" cannot be set here; only TowerMerge records it after the merge gate succeeds. Merged and abandoned are terminal and cannot be reopened by a generic patch."
                    },
                    "note": {
                        "type": "string",
                        "description": "Append a decision-log note"
                    },
                    "blocker": {
                        "type": "string",
                        "description": "Report a blocker (also sets status to blocked)"
                    },
                    "clear_blockers": {
                        "type": "boolean",
                        "description": "Clear all recorded blockers"
                    },
                    "task_done": {
                        "type": "string",
                        "description": "Mark the first open task containing this text as done"
                    },
                    "task_drop": {
                        "type": "string",
                        "description": "Mark the first open task containing this text as dropped — the escape hatch for legitimately descoped tasks; requires task_drop_reason and is recorded in the mission notes and the activity log"
                    },
                    "task_drop_reason": {
                        "type": "string",
                        "description": "Mandatory with task_drop: why the task is legitimately descoped"
                    },
                    "scope": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Tower only: replace the mission scope globs (picomatch — `**` crosses directories). Logged; widens what the merge gate accepts."
                    }
                },
                "additionalProperties": false
            }),
        },
        ToolInfo {
            name: "TowerStatus".into(),
            description: TOWER_STATUS_DESCRIPTION.into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "limit": {
                        "type": "integer",
                        "exclusiveMinimum": 0,
                        "description": "Maximum entries to return in each detail list (default 20), newest first"
                    }
                },
                "additionalProperties": false
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

    #[derive(Default)]
    struct EventCapture {
        events: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
        telemetry: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    }

    impl crate::callbacks::HostCallbacks for EventCapture {
        fn llm_chat(
            &self,
            _: crate::rpc::types::LlmChatRequest,
        ) -> crate::rpc::types::BoxFuture<'static, Result<crate::rpc::types::LlmChatResponse, String>>
        {
            Box::pin(async { Err("not used".into()) })
        }

        fn execute_tool(
            &self,
            _: crate::rpc::types::ToolExecuteRequest,
        ) -> crate::rpc::types::BoxFuture<
            'static,
            Result<crate::rpc::types::ToolExecuteResponse, String>,
        > {
            Box::pin(async { Err("not used".into()) })
        }

        fn check_permission(
            &self,
            _: crate::rpc::types::PermissionCheckRequest,
        ) -> crate::rpc::types::BoxFuture<
            'static,
            Result<crate::rpc::types::PermissionDecision, String>,
        > {
            Box::pin(async { Ok(crate::rpc::types::PermissionDecision::allow()) })
        }

        fn state_read(
            &self,
            _: crate::rpc::types::StateReadRequest,
        ) -> crate::rpc::types::BoxFuture<
            'static,
            Result<crate::rpc::types::StateReadResponse, String>,
        > {
            Box::pin(async { Err("not used".into()) })
        }

        fn state_write(
            &self,
            _: crate::rpc::types::StateWriteRequest,
        ) -> crate::rpc::types::BoxFuture<
            'static,
            Result<crate::rpc::types::StateWriteResponse, String>,
        > {
            Box::pin(async { Err("not used".into()) })
        }

        fn emit_event(&self, event: serde_json::Value) {
            self.events.lock().unwrap().push(event);
        }

        fn telemetry(&self, event: serde_json::Value) {
            self.telemetry.lock().unwrap().push(event);
        }
    }

    #[tokio::test]
    async fn tower_init_and_teardown_emit_mode_events_and_telemetry() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let git = crate::tools::tower::git::git;
        git(root, &["init", "-q"]).await.unwrap();
        let _ = git(root, &["config", "user.email", "t@e.test"]).await;
        let _ = git(root, &["config", "user.name", "t"]).await;
        std::fs::write(root.join("f.txt"), "base").unwrap();
        git(root, &["add", "."]).await.unwrap();
        git(root, &["commit", "-qm", "init"]).await.unwrap();

        let capture = EventCapture::default();
        let result = execute_tower_init(root, "main", "session-1", r#"{}"#, Some(&capture)).await;
        assert!(!result.is_error, "{}", result.content);
        let events = capture.events.lock().unwrap().clone();
        assert!(
            events
                .iter()
                .any(|event| event["type"] == "tower_mode.enter"
                    && event["agentId"] == "main"
                    && event["sessionId"] == "session-1"),
            "{events:?}"
        );
        let telemetry = capture.telemetry.lock().unwrap().clone();
        assert!(
            telemetry
                .iter()
                .any(|event| event["event"] == "tower_mode_enter" && event["outcome"] == "entered"),
            "{telemetry:?}"
        );

        let result = execute_tower_teardown(
            root,
            "main",
            "session-1",
            r#"{}"#,
            None,
            None,
            Some(&capture),
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        let events = capture.events.lock().unwrap().clone();
        assert!(
            events
                .iter()
                .any(|event| event["type"] == "tower_mode.exit" && event["agentId"] == "main"),
            "{events:?}"
        );
        let telemetry = capture.telemetry.lock().unwrap().clone();
        assert!(
            telemetry
                .iter()
                .any(|event| event["event"] == "tower_mode_exit" && event["reason"] == "user"),
            "{telemetry:?}"
        );
    }

    #[test]
    fn branch_lease_fails_fast_for_the_same_branch_only() {
        let root = Path::new("lease-test-repo");
        let first = acquire_branch_lease(root, "feat/lease").unwrap();
        let error = acquire_branch_lease(root, "feat/lease").unwrap_err();
        assert!(error.contains("leased by another"), "{error}");

        let other_branch = acquire_branch_lease(root, "feat/other").unwrap();
        let other_repo =
            acquire_branch_lease(Path::new("lease-test-repo-2"), "feat/lease").unwrap();
        drop(first);
        assert!(acquire_branch_lease(root, "feat/lease").is_ok());
        drop(other_branch);
        drop(other_repo);
    }

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
            completion_tip: None,
            key_artifacts: None,
        }
    }

    async fn review_repo_with_branch(root: &Path, branch: &str) -> String {
        let git = crate::tools::tower::git::git;
        git(root, &["init", "-q"]).await.unwrap();
        let _ = git(root, &["config", "user.email", "t@e.test"]).await;
        let _ = git(root, &["config", "user.name", "t"]).await;
        std::fs::write(root.join("f.txt"), "base").unwrap();
        git(root, &["add", "."]).await.unwrap();
        git(root, &["commit", "-qm", "init"]).await.unwrap();
        let base = git(root, &["rev-parse", "--abbrev-ref", "HEAD"])
            .await
            .unwrap();
        git(root, &["checkout", "-qb", branch]).await.unwrap();
        std::fs::write(root.join("f.txt"), "changed").unwrap();
        git(root, &["add", "."]).await.unwrap();
        git(root, &["commit", "-qm", "work"]).await.unwrap();
        git(root, &["checkout", "-q", &base]).await.unwrap();
        let exclude = root.join(".git/info/exclude");
        let existing = std::fs::read_to_string(&exclude).unwrap_or_default();
        std::fs::write(&exclude, format!("{existing}\n.tower/\n")).unwrap();
        base
    }

    async fn save_review_state(
        root: &Path,
        base: &str,
        mission: TowerMission,
        agents: Vec<TowerRosterEntry>,
    ) {
        let store = TowerStore::new(root);
        for dir in [
            crate::tools::tower::paths::MISSIONS_DIR,
            crate::tools::tower::paths::FINDINGS_DIR,
            crate::tools::tower::paths::INBOX_DIR,
            crate::tools::tower::paths::REVIEWS_DIR,
        ] {
            tokio::fs::create_dir_all(store.abs(dir)).await.unwrap();
        }
        tokio::fs::create_dir_all(
            store
                .abs(crate::tools::tower::paths::ACTIVITY_LOG)
                .parent()
                .unwrap(),
        )
        .await
        .unwrap();
        store
            .save(&TowerState {
                version: 1,
                base: base.into(),
                mode: "branch".into(),
                created_at: "2026-01-01T00:00:00Z".into(),
                session_id: Some("session-1".into()),
                roster: TowerRoster { agents },
                missions: vec![mission],
            })
            .await
            .unwrap();
    }

    fn reviewer_entry(name: &str, agent_id: &str, target: &str) -> TowerRosterEntry {
        TowerRosterEntry {
            name: name.into(),
            agent_id: agent_id.into(),
            session_id: Some("session-1".into()),
            kind: TowerAgentKind::Reviewer,
            mission_id: None,
            review_target: Some(target.into()),
            review_mission_id: Some("M1".into()),
            worktree: None,
            branch: None,
            spawned_at: "2026-01-01T00:00:00Z".into(),
            died_at: None,
            death_status: None,
            death_reason: None,
            status: None,
        }
    }

    #[tokio::test]
    async fn review_output_lists_recipients_and_next_guidance() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let base = review_repo_with_branch(root, "feat/review").await;
        let mut mission = spawn_test_mission(TowerMissionStatus::Completed);
        mission.branch = "feat/review".into();
        mission.scope = vec!["**".into()];
        save_review_state(
            root,
            &base,
            mission,
            vec![reviewer_entry(
                "reviewer-1",
                "agent-reviewer",
                "feat/review",
            )],
        )
        .await;

        let result = execute_tower_review(
            root,
            "agent-reviewer",
            r#"{"target":"feat/review","status":"clean","merge":"merge","findings":"none","decision":"ok"}"#,
            None,
            None,
            false,
            None,
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        let lines: Vec<&str> = result.content.lines().collect();
        assert!(
            lines[0].starts_with("review submitted: "),
            "{}",
            result.content
        );
        assert_eq!(lines[1], "notified: tower");
        assert!(
            lines[2].contains("next: feat/review is merge-ready"),
            "{}",
            result.content
        );

        let hold = execute_tower_review(
            root,
            "agent-reviewer",
            r#"{"target":"feat/review","status":"clean","merge":"hold","findings":"none","decision":"ok"}"#,
            None,
            None,
            false,
            None,
        )
        .await;
        assert!(
            hold.content
                .contains("next: feat/review is blocked by the latest review's \"hold\" verdict"),
            "{}",
            hold.content
        );

        let fix = execute_tower_review(
            root,
            "agent-reviewer",
            r#"{"target":"feat/review","status":"clean","merge":"fix-then-merge","findings":"none","decision":"ok"}"#,
            None,
            None,
            false,
            None,
        )
        .await;
        assert!(
            fix.content
                .contains("next: feat/review is not merge-approved"),
            "{}",
            fix.content
        );

        let rework = execute_tower_review(
            root,
            "agent-reviewer",
            r#"{"target":"feat/review","status":"p1-2items","merge":"hold","findings":"f","decision":"ok"}"#,
            None,
            None,
            false,
            None,
        )
        .await;
        assert!(
            rework
                .content
                .contains("next: feat/review is not merge-ready"),
            "{}",
            rework.content
        );
    }

    #[tokio::test]
    async fn artifact_acceptance_is_refused_in_an_automatic_run() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let base = review_repo_with_branch(root, "feat/artifact").await;
        let mut mission = spawn_test_mission(TowerMissionStatus::Completed);
        mission.kind = TowerMissionKind::Artifact;
        mission.branch = "feat/artifact".into();
        mission.scope = vec!["**".into()];
        mission.completion_tip = Some(
            crate::tools::tower::git::branch_tip(root, "feat/artifact")
                .await
                .unwrap(),
        );
        save_review_state(root, &base, mission, Vec::new()).await;

        let args = r#"{"target":"feat/artifact","status":"clean","merge":"merge","findings":"none","decision":"ok","acceptance_tip":"abc"}"#;
        let result = execute_tower_review(root, "main", args, None, None, true, None).await;
        assert!(result.is_error);
        assert!(
            result
                .content
                .contains("cannot be recorded in an automatic or noninteractive run"),
            "{}",
            result.content
        );

        let result = execute_tower_review(
            root,
            "main",
            args,
            None,
            None,
            false,
            Some(crate::permission::PermissionMode::Auto),
        )
        .await;
        assert!(result.is_error);
        assert!(
            result
                .content
                .contains("cannot be recorded in an automatic or noninteractive run"),
            "{}",
            result.content
        );
    }

    #[tokio::test]
    async fn artifact_review_next_distinguishes_the_tower_from_a_reviewer() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let base = review_repo_with_branch(root, "feat/artifact").await;
        let tip = crate::tools::tower::git::branch_tip(root, "feat/artifact")
            .await
            .unwrap();
        let mut mission = spawn_test_mission(TowerMissionStatus::Completed);
        mission.kind = TowerMissionKind::Artifact;
        mission.branch = "feat/artifact".into();
        mission.scope = vec!["**".into()];
        mission.completion_tip = Some(tip.clone());
        save_review_state(
            root,
            &base,
            mission,
            vec![reviewer_entry(
                "reviewer-1",
                "agent-reviewer",
                "feat/artifact",
            )],
        )
        .await;

        let reviewer_args = r#"{"target":"feat/artifact","status":"clean","merge":"merge","findings":"none","decision":"ok"}"#;
        let result = execute_tower_review(
            root,
            "agent-reviewer",
            reviewer_args,
            None,
            None,
            false,
            None,
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(result.content.lines().nth(1), Some("notified: tower"));
        assert!(
            result
                .content
                .contains("next: feat/artifact is still waiting for mission-matching explicit human acceptance recorded by reviewer=tower"),
            "{}",
            result.content
        );

        let tower_args = format!(
            r#"{{"target":"feat/artifact","status":"clean","merge":"merge","findings":"none","decision":"ok","acceptance_tip":"{tip}"}}"#
        );
        let result = execute_tower_review(root, "main", &tower_args, None, None, false, None).await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(result.content.lines().nth(1), Some("notified: (none)"));
        assert!(
            result.content.contains(
                "next: explicit human acceptance for feat/artifact is recorded on the exact tip"
            ),
            "{}",
            result.content
        );
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
        assert!(
            result
                .content
                .contains("terminal missions cannot spawn workers"),
            "{}",
            result.content
        );
    }

    #[tokio::test]
    async fn spawn_refuses_blank_reserved_and_unknown_mission_names() {
        let dir = tempfile::tempdir().unwrap();
        let mut mission = spawn_test_mission(TowerMissionStatus::Active);
        mission.id = "M7".into();
        save_spawn_state(dir.path(), mission, Vec::new()).await;

        for (name, expected) in [
            ("   ", "must not be blank or carry surrounding whitespace"),
            (
                " padded ",
                "must not be blank or carry surrounding whitespace",
            ),
            ("tower", "is reserved by the tower protocol"),
            ("all", "is reserved by the tower protocol"),
        ] {
            let args = serde_json::json!({ "name": name, "kind": "worker", "mission_id": "M7" });
            let result = execute_tower_spawn(
                dir.path(),
                "main",
                "session-1",
                None,
                None,
                None,
                &args.to_string(),
            )
            .await
            .expect("name refusals happen before the manager is needed");
            assert!(result.is_error, "{name:?}");
            assert!(
                result.content.contains(expected),
                "{name:?}: {}",
                result.content
            );
        }

        let args = serde_json::json!({ "name": "worker-1", "kind": "worker", "mission_id": "M9" });
        let result = execute_tower_spawn(
            dir.path(),
            "main",
            "session-1",
            None,
            None,
            None,
            &args.to_string(),
        )
        .await
        .expect("unknown mission is refused before the manager is needed");
        assert!(result.is_error);
        assert!(
            result
                .content
                .contains("unknown mission \"M9\" — known missions: M7"),
            "{}",
            result.content
        );
    }

    #[tokio::test]
    async fn teardown_refuses_a_workspace_owned_by_a_live_session() {
        let dir = tempfile::tempdir().unwrap();
        let mission = spawn_test_mission(TowerMissionStatus::Merged);
        let store = TowerStore::new(dir.path());
        tokio::fs::create_dir_all(store.abs(STATE_FILE).parent().unwrap())
            .await
            .unwrap();
        store
            .save(&TowerState {
                version: 1,
                base: "main".into(),
                mode: "branch".into(),
                created_at: "2026-01-01T00:00:00Z".into(),
                session_id: Some("session-other".into()),
                roster: TowerRoster { agents: Vec::new() },
                missions: vec![mission],
            })
            .await
            .unwrap();

        let _guard = crate::session::lock::SESSION_LOCKS
            .hold_guard("session-other")
            .expect("the test owns this session lock");
        let result =
            execute_tower_teardown(dir.path(), "main", "session-1", r#"{}"#, None, None, None)
                .await;
        assert!(result.is_error);
        assert!(
            result
                .content
                .contains("tearing it down would dismantle that session's fleet"),
            "{}",
            result.content
        );
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
            plan.description.contains("auto-adds merge dependencies"),
            "{}",
            plan.description
        );
        assert!(
            plan.description.contains("add explicit deps"),
            "{}",
            plan.description
        );
    }

    #[tokio::test]
    async fn tower_init_reports_malformed_arguments_instead_of_running_with_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let result =
            execute_tower_init(dir.path(), "main", "session-1", r#"{"base":42}"#, None).await;
        assert!(result.is_error);
        assert!(
            result.content.contains("failed to parse TowerInit input"),
            "{}",
            result.content
        );
    }

    #[tokio::test]
    async fn tower_teardown_reports_malformed_arguments_instead_of_running_with_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let result = execute_tower_teardown(
            dir.path(),
            "main",
            "session-1",
            r#"{"force":"yes"}"#,
            None,
            None,
            None,
        )
        .await;
        assert!(result.is_error);
        assert!(
            result
                .content
                .contains("failed to parse TowerTeardown input"),
            "{}",
            result.content
        );
    }

    #[tokio::test]
    async fn tower_inbox_reports_malformed_arguments_instead_of_running_with_defaults() {
        let dir = tempfile::tempdir().unwrap();
        save_spawn_state(
            dir.path(),
            spawn_test_mission(TowerMissionStatus::Active),
            Vec::new(),
        )
        .await;
        let result = execute_tower_inbox(dir.path(), "main", r#"{"limit":"ten"}"#).await;
        assert!(result.is_error);
        assert!(
            result.content.contains("failed to parse TowerInbox input"),
            "{}",
            result.content
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
