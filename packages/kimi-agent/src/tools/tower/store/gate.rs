use super::*;
use crate::tools::tower::mission_gate::{
    TowerMissionBranchObservation, TowerMissionGateResult, TowerMissionGitObservations,
    TowerWorkerRuntime, TowerWorkerRuntimeStatus, evaluate_mission_gate,
};
use crate::tools::tower::types::{TowerAgentKind, TowerRebaseRecord};

impl TowerStore {
    pub async fn mission_gate(
        &self,
        state: &TowerState,
        mission: &TowerMission,
        active_agent_ids: Option<&HashSet<String>>,
    ) -> Result<TowerMissionGateResult, String> {
        let (reviews, observations) = tokio::join!(
            self.reviews_for(&mission.branch),
            self.observe_mission_git(state, mission),
        );
        let reviews = reviews?;
        let observations = observations?;
        let workers: Vec<TowerWorkerRuntime> = state
            .roster
            .agents
            .iter()
            .filter(|agent| {
                agent.kind == TowerAgentKind::Worker
                    && (agent.mission_id.as_deref() == Some(mission.id.as_str())
                        || agent.branch.as_deref() == Some(mission.branch.as_str())
                        || agent.worktree.as_deref() == Some(mission.worktree.as_str()))
            })
            .map(|agent| TowerWorkerRuntime {
                name: agent.name.clone(),
                status: match active_agent_ids {
                    None => TowerWorkerRuntimeStatus::Unknown,
                    Some(active) if active.contains(&agent.agent_id) => {
                        TowerWorkerRuntimeStatus::Busy
                    }
                    Some(_) => TowerWorkerRuntimeStatus::Idle,
                },
            })
            .collect();
        let raw = self.load_state_value().await?;
        let last_rebase = raw
            .get("missions")
            .and_then(serde_json::Value::as_array)
            .and_then(|missions| {
                missions.iter().find(|candidate| {
                    candidate.get("id").and_then(serde_json::Value::as_str)
                        == Some(mission.id.as_str())
                })
            })
            .and_then(|mission| mission.get("lastRebase"))
            .and_then(|rebase| {
                Some(TowerRebaseRecord {
                    from_commit: rebase.get("fromCommit")?.as_str()?.to_string(),
                    to_commit: rebase.get("toCommit")?.as_str()?.to_string(),
                })
            });
        Ok(evaluate_mission_gate(
            state,
            mission,
            &reviews,
            &workers,
            observations,
            last_rebase.as_ref(),
        ))
    }

    async fn observe_mission_git(
        &self,
        state: &TowerState,
        mission: &TowerMission,
    ) -> Result<TowerMissionGitObservations, String> {
        let (base_tip, head_tip, checked_out_branch, dirty_files, has_branch) = tokio::join!(
            branch_tip(&self.repo_root, &state.base),
            branch_tip(&self.repo_root, "HEAD"),
            async { current_branch(&self.repo_root).await.ok() },
            crate::tools::tower::git::dirty_files(&self.repo_root),
            branch_exists(&self.repo_root, &mission.branch),
        );
        let base_tip = base_tip?;
        let head_tip = head_tip?;
        let base_dirty_files = dirty_files?;
        let has_branch = has_branch?;
        let branch = if has_branch {
            let tip = branch_tip(&self.repo_root, &mission.branch).await?;
            let (base_is_ancestor, spawn_base_is_ancestor) =
                tokio::join!(is_ancestor(&self.repo_root, &base_tip, &tip), async {
                    match mission.spawn_base.as_deref() {
                        Some(spawn_base) => is_ancestor(&self.repo_root, spawn_base, &tip).await,
                        None => Ok(false),
                    }
                },);
            let base_is_ancestor = base_is_ancestor?;
            let diff_base = if spawn_base_is_ancestor? {
                mission
                    .spawn_base
                    .clone()
                    .unwrap_or_else(|| base_tip.clone())
            } else {
                base_tip.clone()
            };
            let (changed_files, merge_touched_files) = tokio::join!(
                diff_name_only(&self.repo_root, &diff_base, &tip),
                diff_name_only(&self.repo_root, &head_tip, &tip),
            );
            Some(TowerMissionBranchObservation {
                tip,
                base_is_ancestor,
                diff_base,
                changed_files: changed_files?,
                merge_touched_files: merge_touched_files?,
            })
        } else {
            None
        };
        Ok(TowerMissionGitObservations {
            base_tip,
            head_tip,
            checked_out_branch,
            base_dirty_files,
            branch,
        })
    }
}
