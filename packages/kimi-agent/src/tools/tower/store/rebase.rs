use super::*;
use crate::tools::tower::git;
use crate::tools::tower::types::{
    TowerAgentKind, TowerReanchorDigest, TowerReanchorMissionDigest, TowerReanchorRelation,
    TowerRebaseMissionResult, TowerRebaseMissionStatus,
};

fn rebase_in_progress(git_dir: &Path) -> bool {
    git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists()
}

impl TowerStore {
    pub(super) async fn reanchor_locked(
        &self,
        state: &mut TowerState,
        base: &str,
    ) -> Result<TowerReanchorDigest, String> {
        if !branch_exists(&self.repo_root, base).await? {
            return Err(format!(
                "base branch \"{base}\" does not exist as a local branch — create a local branch first"
            ));
        }
        let base_tip = branch_tip(&self.repo_root, base).await?;
        let mut missions = Vec::new();
        for mission in state
            .missions
            .iter()
            .filter(|mission| mission.status.is_open())
        {
            if !branch_exists(&self.repo_root, &mission.branch).await? {
                missions.push(TowerReanchorMissionDigest {
                    id: mission.id.clone(),
                    branch: mission.branch.clone(),
                    relation: TowerReanchorRelation::NoBranch,
                    branch_tip: None,
                });
                continue;
            }
            let tip = branch_tip(&self.repo_root, &mission.branch).await?;
            let relation = if is_ancestor(&self.repo_root, &base_tip, &tip).await? {
                TowerReanchorRelation::UpToDate
            } else {
                TowerReanchorRelation::Stale
            };
            missions.push(TowerReanchorMissionDigest {
                id: mission.id.clone(),
                branch: mission.branch.clone(),
                relation,
                branch_tip: Some(tip),
            });
        }
        let digest = TowerReanchorDigest {
            from: state.base.clone(),
            to: base.to_string(),
            base_tip,
            missions,
        };
        state.base = base.to_string();
        self.save(state).await?;
        let relations = digest
            .missions
            .iter()
            .map(|mission| format!("{}:{}", mission.id, mission.relation.as_str()))
            .collect::<Vec<_>>()
            .join(",");
        self.append_log(
            TOWER_NAME,
            "rebase",
            &[
                ("from", digest.from.as_str()),
                ("to", digest.to.as_str()),
                ("relations", relations.as_str()),
            ],
            None,
        )
        .await?;
        Ok(digest)
    }

    pub async fn rebase_with_workers(
        &self,
        caller_name: &str,
        id: &str,
        active_agent_ids: Option<HashSet<String>>,
    ) -> Result<TowerRebaseMissionResult, String> {
        let caller_name = caller_name.to_string();
        let id = id.to_string();
        self.with_state_lock(move || async move {
            self.rebase_locked(&caller_name, &id, active_agent_ids.as_ref())
                .await
        })
        .await
    }

    async fn rebase_locked(
        &self,
        caller_name: &str,
        id: &str,
        active_agent_ids: Option<&HashSet<String>>,
    ) -> Result<TowerRebaseMissionResult, String> {
        if caller_name != TOWER_NAME {
            return Err(format!(
                "agent \"{caller_name}\" cannot rebase a mission branch — only the tower runs TowerRebase"
            ));
        }
        let mut state = self.load().await?;
        let Some(mission_index) = state.missions.iter().position(|mission| mission.id == id) else {
            return Err(format!("unknown mission \"{id}\""));
        };
        let mission = state.missions[mission_index].clone();
        if !mission.status.is_open() {
            return Err(format!(
                "mission {id} is {} — only open missions rebase; closed missions are historical records",
                mission.status.as_str()
            ));
        }
        if mission.kind == TowerMissionKind::Survey {
            return Err(format!(
                "mission {id} is a read-only survey — it has no work branch to rebase"
            ));
        }
        let workers: Vec<&TowerRosterEntry> = state
            .roster
            .agents
            .iter()
            .filter(|agent| {
                agent.kind == TowerAgentKind::Worker
                    && (agent.mission_id.as_deref() == Some(mission.id.as_str())
                        || agent.branch.as_deref() == Some(mission.branch.as_str())
                        || agent.worktree.as_deref() == Some(mission.worktree.as_str()))
            })
            .collect();
        let busy: Vec<&str> = workers
            .iter()
            .filter(|worker| {
                active_agent_ids.is_some_and(|active| active.contains(&worker.agent_id))
            })
            .map(|worker| worker.name.as_str())
            .collect();
        if !busy.is_empty() {
            return Err(format!(
                "worker(s) {} are mid-turn — rebasing under a running worker could race its edits; retry once every worker is idle",
                busy.join(", ")
            ));
        }
        if active_agent_ids.is_none() && !workers.is_empty() {
            return Err(format!(
                "worker runtime for {} cannot be observed authoritatively — unknown is not idle; retry TowerRebase from the main tower agent",
                workers
                    .iter()
                    .map(|worker| worker.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !branch_exists(&self.repo_root, &mission.branch).await? {
            return Err(format!(
                "mission {id} has no branch \"{}\" yet — spawn a worker to create it before rebasing",
                mission.branch
            ));
        }
        let worktree = self.abs(&format!("{WORKTREES_DIR}/{}", mission.worktree));
        if !is_registered_worktree(&self.repo_root, &worktree).await? {
            return Err(format!(
                "mission {id} has no registered worktree at {WORKTREES_DIR}/{} — spawn a worker to create it before rebasing",
                mission.worktree
            ));
        }
        if is_worktree_dirty(&worktree).await? {
            return Err(format!(
                "worktree {} has uncommitted changes — commit or stash them before rebasing",
                mission.worktree
            ));
        }
        let git_dir_raw = git::git(&worktree, &["rev-parse", "--git-dir"]).await?;
        let git_dir_raw = PathBuf::from(git_dir_raw.trim());
        let git_dir = if git_dir_raw.is_absolute() {
            git_dir_raw
        } else {
            worktree.join(git_dir_raw)
        };
        if rebase_in_progress(&git_dir) {
            return Err(format!(
                "worktree {} already has a rebase in progress — finish or abort it before TowerRebase",
                mission.worktree
            ));
        }
        let base_commit = branch_tip(&self.repo_root, &state.base).await?;
        let from_commit = branch_tip(&self.repo_root, &mission.branch).await?;
        let checked_out = git::git(&worktree, &["rev-parse", "--abbrev-ref", "HEAD"]).await?;
        if checked_out != mission.branch {
            return Err(format!(
                "worktree {} is on \"{checked_out}\", not mission branch \"{}\" — restore the registered checkout before rebasing",
                mission.worktree, mission.branch
            ));
        }
        if branch_tip(&worktree, "HEAD").await? != from_commit {
            return Err(format!(
                "worktree {} HEAD does not match mission branch \"{}\" — repair the diverged checkout before rebasing",
                mission.worktree, mission.branch
            ));
        }
        if is_ancestor(&self.repo_root, &base_commit, &from_commit).await? {
            self.append_log(
                TOWER_NAME,
                "rebase.mission",
                &[("id", id), ("result", "up-to-date")],
                None,
            )
            .await?;
            return Ok(TowerRebaseMissionResult {
                status: TowerRebaseMissionStatus::UpToDate,
                from_commit: None,
                to_commit: None,
                files: Vec::new(),
            });
        }

        let rebase_error = git::git(&worktree, &["rebase", &base_commit]).await.err();
        if let Some(rebase_error) = rebase_error {
            let conflicted = git::git(&worktree, &["diff", "--name-only", "--diff-filter=U"])
                .await
                .map_err(|error| {
                    format!(
                        "recovery-required: rebase of mission {id} failed and its state could not be inspected: {error}"
                    )
                })?;
            let files: Vec<String> = conflicted
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect();
            let in_progress = rebase_in_progress(&git_dir);
            if in_progress {
                git::git(&worktree, &["rebase", "--abort"])
                    .await
                    .map_err(|error| {
                        format!(
                            "recovery-required: rebase of mission {id} failed and git rebase --abort also failed: {error}"
                        )
                    })?;
            }
            self.assert_rebase_recovered(&worktree, &git_dir, &mission, &from_commit)
                .await?;
            if in_progress && !files.is_empty() {
                let updated = &mut state.missions[mission_index];
                updated.status = TowerMissionStatus::Blocked;
                updated.blockers.push(format!(
                    "tower rebase onto {} conflicted in: {} — the rebase was aborted and the worktree was restored; resolve the conflicts manually, then clear this blocker and request a fresh review",
                    state.base,
                    files.join(", ")
                ));
                let updated = updated.clone();
                self.save(&state).await?;
                self.render_missions_index(&state).await?;
                self.render_mission_file(&updated).await?;
                self.append_log(
                    TOWER_NAME,
                    "rebase.mission",
                    &[
                        ("id", id),
                        ("result", "conflict"),
                        ("files", &files.join(",")),
                    ],
                    None,
                )
                .await?;
                return Ok(TowerRebaseMissionResult {
                    status: TowerRebaseMissionStatus::Conflict,
                    from_commit: Some(from_commit),
                    to_commit: None,
                    files,
                });
            }
            return Err(format!(
                "rebase of mission {id} failed: {rebase_error} — no unmerged-path conflict was observed, so the mission was not marked blocked and no review waiver was recorded"
            ));
        }

        let to_commit = branch_tip(&self.repo_root, &mission.branch).await?;
        let mut problems = Vec::new();
        if git::git(&worktree, &["rev-parse", "--abbrev-ref", "HEAD"]).await? != mission.branch {
            problems.push("the worktree no longer has the mission branch checked out");
        }
        if branch_tip(&worktree, "HEAD").await? != to_commit {
            problems.push("worktree HEAD and the mission branch tip differ");
        }
        if is_worktree_dirty(&worktree).await? {
            problems.push("the worktree is dirty");
        }
        if !is_ancestor(&self.repo_root, &base_commit, &to_commit).await? {
            problems.push("the rebased branch does not contain the evaluated base commit");
        }
        if !problems.is_empty() {
            return Err(format!(
                "recovery-required: rebase of mission {id} returned success but its postconditions failed ({}) — no review waiver was recorded",
                problems.join("; ")
            ));
        }
        self.save(&state).await?;
        let mut raw = self.load_state_value().await?;
        if let Some(raw_mission) = raw
            .get_mut("missions")
            .and_then(serde_json::Value::as_array_mut)
            .and_then(|missions| {
                missions.iter_mut().find(|candidate| {
                    candidate.get("id").and_then(serde_json::Value::as_str) == Some(id)
                })
            })
        {
            raw_mission["lastRebase"] = serde_json::json!({
                "fromCommit": &from_commit,
                "toCommit": &to_commit,
            });
        }
        self.save_state_value(&raw).await?;
        self.render_missions_index(&state).await?;
        self.render_mission_file(&mission).await?;
        self.append_log(
            TOWER_NAME,
            "rebase.mission",
            &[("id", id), ("result", "clean")],
            None,
        )
        .await?;
        Ok(TowerRebaseMissionResult {
            status: TowerRebaseMissionStatus::Rebased,
            from_commit: Some(from_commit),
            to_commit: Some(to_commit),
            files: Vec::new(),
        })
    }

    async fn assert_rebase_recovered(
        &self,
        worktree: &Path,
        git_dir: &Path,
        mission: &TowerMission,
        from_commit: &str,
    ) -> Result<(), String> {
        let mut problems = Vec::new();
        if rebase_in_progress(git_dir) {
            problems.push("a rebase is still in progress");
        }
        if git::git(worktree, &["rev-parse", "--abbrev-ref", "HEAD"]).await? != mission.branch {
            problems.push("the mission branch is not checked out");
        }
        if branch_tip(worktree, "HEAD").await? != from_commit {
            problems.push("HEAD was not restored to the pre-rebase commit");
        }
        if is_worktree_dirty(worktree).await? {
            problems.push("the worktree is dirty");
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "recovery-required: rebase abort did not restore the worktree ({})",
                problems.join("; ")
            ))
        }
    }
}
