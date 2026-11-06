use std::collections::HashSet;

use crate::tools::tower::types::{
    TowerMission, TowerMissionKind, TowerRebaseRecord, TowerReviewInfo, TowerState,
};

pub const REASON_NOT_COMPLETED: &str = "not-completed";
pub const REASON_NO_REVIEW: &str = "no-review";
pub const REASON_REVIEW_HOLD: &str = "review-hold";
pub const REASON_NOT_CLEAN: &str = "not-clean";
pub const REASON_REVIEW_NOT_APPROVED: &str = "review-not-approved";
pub const REASON_TIP_MOVED: &str = "tip-moved";
pub const REASON_REVIEW_MISSION_MISMATCH: &str = "review-mission-mismatch";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TowerGateBlock {
    pub reason: &'static str,
    pub message: String,
}

impl TowerGateBlock {
    pub fn new(reason: &'static str, message: impl Into<String>) -> Self {
        Self {
            reason,
            message: message.into(),
        }
    }
}

/// Select the latest review that is authoritative for this mission. Legacy
/// unbound reviews remain valid on an unshared branch; on a shared branch they
/// are selected only so the gate can report the binding mismatch explicitly.
pub fn select_mission_review(
    reviews: &[TowerReviewInfo],
    mission: &TowerMission,
    shared_branch: bool,
) -> Option<TowerReviewInfo> {
    let has_own = reviews
        .iter()
        .any(|review| review.mission.as_deref() == Some(mission.id.as_str()));
    reviews
        .iter()
        .rfind(|review| {
            if has_own {
                review.mission.as_deref() == Some(mission.id.as_str())
                    || (!shared_branch && review.mission.is_none())
            } else {
                review.mission.is_none()
            }
        })
        .cloned()
}

pub const REASON_OPEN_TASKS: &str = "open-tasks";
pub const REASON_BLOCKERS: &str = "blockers";
pub const REASON_WORKER_BUSY: &str = "worker-busy";
pub const REASON_WORKER_UNKNOWN: &str = "worker-unknown";
pub const REASON_BRANCH_MISSING: &str = "branch-missing";
pub const REASON_DEPS_UNMERGED: &str = "deps-unmerged";
pub const REASON_READ_ONLY_SURVEY: &str = "read-only-survey";
pub const REASON_STALE_BASE: &str = "stale-base";
pub const REASON_OUT_OF_SCOPE: &str = "out-of-scope";
pub const REASON_BASE_MISMATCH: &str = "base-mismatch";
pub const REASON_BASE_DIRTY: &str = "base-dirty";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TowerWorkerRuntimeStatus {
    Busy,
    Idle,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TowerWorkerRuntime {
    pub name: String,
    pub status: TowerWorkerRuntimeStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TowerMissionBranchObservation {
    pub tip: String,
    pub base_is_ancestor: bool,
    pub diff_base: String,
    pub changed_files: Vec<String>,
    pub merge_touched_files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TowerMissionGitObservations {
    pub base_tip: String,
    pub head_tip: String,
    pub checked_out_branch: Option<String>,
    pub base_dirty_files: Vec<String>,
    pub branch: Option<TowerMissionBranchObservation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TowerReviewBinding {
    ExactTip,
    CleanRebaseWaived,
    StaleTip,
}

impl TowerReviewBinding {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExactTip => "exact-tip",
            Self::CleanRebaseWaived => "clean-rebase-waived",
            Self::StaleTip => "stale-tip",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TowerMissionGateResult {
    pub ready: bool,
    pub noop: bool,
    pub blocks: Vec<TowerGateBlock>,
    pub selected_review: Option<TowerReviewInfo>,
    pub review_binding: Option<TowerReviewBinding>,
    pub observations: TowerMissionGitObservations,
}

fn gate_result(
    mission: &TowerMission,
    blocks: Vec<TowerGateBlock>,
    selected_review: Option<TowerReviewInfo>,
    review_binding: Option<TowerReviewBinding>,
    observations: TowerMissionGitObservations,
) -> TowerMissionGateResult {
    let ready = blocks.is_empty();
    TowerMissionGateResult {
        ready,
        noop: ready && mission.kind == TowerMissionKind::Survey,
        blocks,
        selected_review,
        review_binding,
        observations,
    }
}

pub fn evaluate_mission_gate(
    state: &TowerState,
    mission: &TowerMission,
    reviews: &[TowerReviewInfo],
    workers: &[TowerWorkerRuntime],
    observations: TowerMissionGitObservations,
    last_rebase: Option<&TowerRebaseRecord>,
) -> TowerMissionGateResult {
    let shared_branch = state
        .missions
        .iter()
        .filter(|candidate| candidate.branch == mission.branch)
        .count()
        > 1;
    let selected_review = select_mission_review(reviews, mission, shared_branch);
    let review_binding = selected_review.as_ref().map(|review| {
        let tip = observations
            .branch
            .as_ref()
            .map(|branch| branch.tip.as_str());
        if tip == Some(review.reviewed_commit.as_str()) {
            TowerReviewBinding::ExactTip
        } else if tip.is_some_and(|tip| {
            last_rebase.is_some_and(|rebase| {
                rebase.from_commit == review.reviewed_commit && rebase.to_commit == tip
            })
        }) {
            TowerReviewBinding::CleanRebaseWaived
        } else {
            TowerReviewBinding::StaleTip
        }
    });

    let mut common = Vec::new();
    if mission.status != crate::tools::tower::types::TowerMissionStatus::Completed {
        common.push(TowerGateBlock::new(
            REASON_NOT_COMPLETED,
            format!(
                "merge blocked: mission {} is {} — completed status is required",
                mission.id,
                mission.status.as_str()
            ),
        ));
    }
    let open_tasks: Vec<&str> = mission
        .tasks
        .iter()
        .filter(|task| !task.done && !task.dropped)
        .map(|task| task.text.as_str())
        .collect();
    if !open_tasks.is_empty() {
        common.push(TowerGateBlock::new(
            REASON_OPEN_TASKS,
            format!(
                "merge blocked: mission {} has open task(s): {}",
                mission.id,
                open_tasks.join(" | ")
            ),
        ));
    }
    if !mission.blockers.is_empty() {
        common.push(TowerGateBlock::new(
            REASON_BLOCKERS,
            format!(
                "merge blocked: mission {} has {} blocker(s): {}",
                mission.id,
                mission.blockers.len(),
                mission.blockers.join(" | ")
            ),
        ));
    }
    let busy: Vec<&str> = workers
        .iter()
        .filter(|worker| worker.status == TowerWorkerRuntimeStatus::Busy)
        .map(|worker| worker.name.as_str())
        .collect();
    if !busy.is_empty() {
        common.push(TowerGateBlock::new(
            REASON_WORKER_BUSY,
            format!(
                "merge blocked: worker(s) {} are mid-turn — wait for every mission worker to become idle before merging",
                busy.join(", ")
            ),
        ));
    }
    let unknown: Vec<&str> = workers
        .iter()
        .filter(|worker| worker.status == TowerWorkerRuntimeStatus::Unknown)
        .map(|worker| worker.name.as_str())
        .collect();
    if !unknown.is_empty() {
        common.push(TowerGateBlock::new(
            REASON_WORKER_UNKNOWN,
            format!(
                "merge blocked: worker runtime for {} cannot be observed authoritatively — unknown is not idle; retry from the main tower agent",
                unknown.join(", ")
            ),
        ));
    }
    if !common.is_empty() {
        return gate_result(
            mission,
            common,
            selected_review,
            review_binding,
            observations,
        );
    }

    let Some(branch) = observations.branch.as_ref() else {
        return gate_result(
            mission,
            vec![TowerGateBlock::new(
                REASON_BRANCH_MISSING,
                format!(
                    "merge blocked: mission {} branch \"{}\" does not exist — spawn a worker and complete the mission before merging",
                    mission.id, mission.branch
                ),
            )],
            selected_review,
            review_binding,
            observations,
        );
    };
    let unmerged: Vec<String> = mission
        .deps
        .iter()
        .filter(|dependency| {
            state
                .missions
                .iter()
                .any(|candidate| candidate.id == dependency.as_str() && candidate.status.is_open())
        })
        .cloned()
        .collect();
    if !unmerged.is_empty() {
        return gate_result(
            mission,
            vec![TowerGateBlock::new(
                REASON_DEPS_UNMERGED,
                format!(
                    "merge blocked: dependencies not merged yet ({}) — merge in Dependency Flow order",
                    unmerged.join(", ")
                ),
            )],
            selected_review,
            review_binding,
            observations,
        );
    }
    if mission.kind == TowerMissionKind::Survey {
        let blocks = if branch.changed_files.is_empty() {
            Vec::new()
        } else {
            vec![TowerGateBlock::new(
                REASON_READ_ONLY_SURVEY,
                format!(
                    "merge blocked: survey mission {} is read-only but {} has {} changed file(s): {} — investigate the worker; move keepable changes to a build mission",
                    mission.id,
                    mission.branch,
                    branch.changed_files.len(),
                    branch
                        .changed_files
                        .iter()
                        .take(5)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            )]
        };
        return gate_result(
            mission,
            blocks,
            selected_review,
            review_binding,
            observations,
        );
    }
    if !branch.base_is_ancestor {
        return gate_result(
            mission,
            vec![TowerGateBlock::new(
                REASON_STALE_BASE,
                format!(
                    "merge blocked: {} is behind base \"{}\" — run TowerRebase(mission=\"{}\") first, then retry the merge",
                    mission.branch, state.base, mission.id
                ),
            )],
            selected_review,
            review_binding,
            observations,
        );
    }
    let review_block = match selected_review.as_ref() {
        None => Some(TowerGateBlock::new(
            REASON_NO_REVIEW,
            format!(
                "merge blocked: {} has no review for mission {} — assign a reviewer first",
                mission.branch, mission.id
            ),
        )),
        Some(review) if review.merge == "hold" => Some(TowerGateBlock::new(
            REASON_REVIEW_HOLD,
            format!(
                "merge blocked: latest review (round {} by {}) is on hold",
                review.round, review.reviewer
            ),
        )),
        Some(review) if review.status != "clean" => Some(TowerGateBlock::new(
            REASON_NOT_CLEAN,
            format!(
                "merge blocked: latest review (round {} by {}) is \"{}\" — a clean round is required",
                review.round, review.reviewer, review.status
            ),
        )),
        Some(review) if review.merge != "merge" && !review.merge.is_empty() => {
            Some(TowerGateBlock::new(
                REASON_REVIEW_NOT_APPROVED,
                format!(
                    "merge blocked: latest review recommends \"{}\" rather than merge",
                    review.merge
                ),
            ))
        }
        Some(_) if review_binding == Some(TowerReviewBinding::StaleTip) => {
            Some(TowerGateBlock::new(
                REASON_TIP_MOVED,
                format!(
                    "merge blocked: {} moved since the clean review — re-review required",
                    mission.branch
                ),
            ))
        }
        Some(review) if shared_branch && review.mission.is_none() => Some(TowerGateBlock::new(
            REASON_REVIEW_MISSION_MISMATCH,
            format!(
                "merge blocked: the latest review for {} is legacy and not bound to mission {}; a mission-stamped review is required on a shared branch",
                mission.branch, mission.id
            ),
        )),
        _ => None,
    };
    if let Some(block) = review_block {
        return gate_result(
            mission,
            vec![block],
            selected_review,
            review_binding,
            observations,
        );
    }
    let out_of_scope: Vec<String> = branch
        .changed_files
        .iter()
        .filter(|file| {
            !mission.scope.iter().any(|pattern| {
                globset::Glob::new(pattern)
                    .map(|glob| glob.compile_matcher().is_match(file))
                    .unwrap_or_else(|_| file.starts_with(pattern.trim_end_matches('*')))
            })
        })
        .cloned()
        .collect();
    if !out_of_scope.is_empty() {
        return gate_result(
            mission,
            vec![TowerGateBlock::new(
                REASON_OUT_OF_SCOPE,
                format!(
                    "merge blocked: {} changed files outside mission {} scope ({}): {} — widen the scope or revert those changes",
                    mission.branch,
                    mission.id,
                    mission.scope.join(", "),
                    out_of_scope.join(", ")
                ),
            )],
            selected_review,
            review_binding,
            observations,
        );
    }
    if observations.checked_out_branch.as_deref() != Some(state.base.as_str()) {
        return gate_result(
            mission,
            vec![TowerGateBlock::new(
                REASON_BASE_MISMATCH,
                format!(
                    "merge blocked: the main checkout is not on the recorded base \"{}\" — switch to it before merging; nothing was merged",
                    state.base
                ),
            )],
            selected_review,
            review_binding,
            observations,
        );
    }
    let dirty: HashSet<&str> = observations
        .base_dirty_files
        .iter()
        .map(String::as_str)
        .collect();
    let blocked_dirty: Vec<String> = branch
        .merge_touched_files
        .iter()
        .filter(|file| dirty.contains(file.as_str()))
        .cloned()
        .collect();
    if !blocked_dirty.is_empty() {
        return gate_result(
            mission,
            vec![TowerGateBlock::new(
                REASON_BASE_DIRTY,
                format!(
                    "merge blocked: the main checkout has uncommitted changes in file(s) this merge would overwrite: {} — commit or stash them first; nothing was merged",
                    blocked_dirty
                        .iter()
                        .take(5)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            )],
            selected_review,
            review_binding,
            observations,
        );
    }
    gate_result(
        mission,
        Vec::new(),
        selected_review,
        review_binding,
        observations,
    )
}

pub fn scope_stem(raw: &str) -> String {
    raw.trim_end_matches("/**")
        .trim_end_matches("/*")
        .trim_end_matches('/')
        .trim_end_matches('*')
        .trim_end_matches('/')
        .to_string()
}

pub fn scopes_overlap(a: &str, b: &str) -> bool {
    let a = scope_stem(a);
    let b = scope_stem(b);
    !a.is_empty()
        && !b.is_empty()
        && (a == b || a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/")))
}

/// Whether one mission already reaches another through the open dependency
/// graph. Checking both directions before adding an edge preserves the
/// existing serialization order and prevents automatic cycles.
pub fn dep_path_exists(missions: &[TowerMission], from: &str, to: &str) -> bool {
    let Some(origin) = missions.iter().find(|mission| mission.id == from) else {
        return false;
    };
    let mut pending = origin.deps.clone();
    let mut seen = HashSet::new();
    while let Some(id) = pending.pop() {
        if id == to {
            return true;
        }
        if !seen.insert(id.clone()) {
            continue;
        }
        if let Some(mission) = missions
            .iter()
            .find(|mission| mission.id == id && mission.status.is_open())
        {
            pending.extend(mission.deps.iter().cloned());
        }
    }
    false
}

/// Serialize a mission behind every older open build mission whose scope it
/// overlaps. Existing paths win, so transitive and reverse edges are not
/// duplicated and an update cannot introduce a dependency cycle.
pub fn add_auto_dependencies(missions: &mut [TowerMission], target_index: usize) {
    let Some(target) = missions.get(target_index) else {
        return;
    };
    if !target.status.is_open() || target.kind == TowerMissionKind::Survey {
        return;
    }
    let target_id = target.id.clone();
    let target_scope = target.scope.clone();

    for candidate_index in 0..target_index {
        let candidate = &missions[candidate_index];
        if !candidate.status.is_open() || candidate.kind == TowerMissionKind::Survey {
            continue;
        }
        if !target_scope.iter().any(|left| {
            candidate
                .scope
                .iter()
                .any(|right| scopes_overlap(left, right))
        }) {
            continue;
        }
        let candidate_id = candidate.id.clone();
        if dep_path_exists(missions, &target_id, &candidate_id)
            || dep_path_exists(missions, &candidate_id, &target_id)
        {
            continue;
        }
        let target = &mut missions[target_index];
        target.deps.push(candidate_id.clone());
        target.notes.push(format!(
            "auto dependency on {candidate_id} because the mission scopes overlap"
        ));
    }
}

pub fn assert_scopes_disjoint(missions: &[TowerMission]) -> Result<(), String> {
    for mission in missions
        .iter()
        .filter(|mission| mission.status.is_open() && mission.kind == TowerMissionKind::Build)
    {
        for raw in &mission.scope {
            if scope_stem(raw).is_empty() {
                return Err(format!(
                    "mission {} scope \"{raw}\" covers the whole repo — narrow it down",
                    mission.id
                ));
            }
        }
    }

    let builds: Vec<&TowerMission> = missions
        .iter()
        .filter(|mission| mission.status.is_open() && mission.kind == TowerMissionKind::Build)
        .collect();
    for (index, left) in builds.iter().enumerate() {
        for right in &builds[index + 1..] {
            let overlap = left.scope.iter().find_map(|left_raw| {
                right.scope.iter().find_map(|right_raw| {
                    scopes_overlap(left_raw, right_raw).then_some((left_raw, right_raw))
                })
            });
            let Some((left_raw, right_raw)) = overlap else {
                continue;
            };
            if dep_path_exists(missions, &left.id, &right.id)
                || dep_path_exists(missions, &right.id, &left.id)
            {
                continue;
            }
            return Err(format!(
                "mission scopes overlap: {} (\"{left_raw}\") vs {} (\"{right_raw}\") without a dependency between them — add an explicit dependency or narrow the scopes",
                left.id, right.id
            ));
        }
    }
    Ok(())
}
