use std::collections::HashSet;

use crate::tools::tower::concurrency::{TOWER_GIT_CONCURRENCY, map_with_concurrency};
use crate::tools::tower::mission_gate::TowerGateBlock;
use crate::tools::tower::rate_limit::{TowerRateLimit, TowerRateLimitSnapshot};
use crate::tools::tower::store::TowerStore;
use crate::tools::tower::types::{TowerReviewInfo, TowerState};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TowerStatusGate {
    pub mission_id: String,
    pub branch: String,
    pub status: String,
    pub ready: bool,
    pub noop: bool,
    pub reasons: Vec<String>,
    pub blocks: Vec<TowerGateBlock>,
    pub selected_review: Option<TowerReviewInfo>,
    pub binding: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TowerWakeGateDigest {
    pub mission_id: String,
    pub status: String,
    pub reasons: Vec<String>,
    pub binding: Option<String>,
}

impl From<TowerStatusGate> for TowerWakeGateDigest {
    fn from(gate: TowerStatusGate) -> Self {
        let status = match gate.status.as_str() {
            "mission-missing" | "unavailable" | "merged" | "abandoned" => gate.status,
            _ if gate.ready => "READY".to_string(),
            _ => "BLOCKED".to_string(),
        };
        Self {
            mission_id: gate.mission_id,
            reasons: if status == "READY" {
                Vec::new()
            } else {
                gate.reasons
            },
            binding: if status == "READY" {
                gate.binding
            } else {
                None
            },
            status,
        }
    }
}

pub async fn read_tower_wake_status_gates(
    store: &TowerStore,
    state: &TowerState,
    mission_ids: &[String],
    active_agent_ids: Option<&HashSet<String>>,
) -> Vec<TowerStatusGate> {
    map_with_concurrency(
        mission_ids.to_vec(),
        TOWER_GIT_CONCURRENCY,
        |_, mission_id| async move {
            let unavailable = |status: &str| TowerStatusGate {
                mission_id: mission_id.clone(),
                branch: String::new(),
                status: status.to_string(),
                ready: false,
                noop: false,
                reasons: Vec::new(),
                blocks: Vec::new(),
                selected_review: None,
                binding: None,
            };
            let Some(mission) = state
                .missions
                .iter()
                .find(|mission| mission.id == mission_id)
            else {
                return unavailable("mission-missing");
            };
            if !mission.status.is_open() {
                return TowerStatusGate {
                    branch: mission.branch.clone(),
                    status: mission.status.as_str().to_string(),
                    ..unavailable(mission.status.as_str())
                };
            }
            let Ok(gate) = store.mission_gate(state, mission, active_agent_ids).await else {
                return unavailable("unavailable");
            };
            TowerStatusGate {
                mission_id,
                branch: mission.branch.clone(),
                status: mission.status.as_str().to_string(),
                ready: gate.ready,
                noop: gate.noop,
                reasons: gate
                    .blocks
                    .iter()
                    .map(|block| block.reason.to_string())
                    .collect(),
                blocks: gate.blocks.clone(),
                selected_review: gate.selected_review,
                binding: gate
                    .review_binding
                    .map(|binding| binding.as_str().to_string()),
            }
        },
    )
    .await
}

pub async fn read_tower_wake_gate_digests(
    store: &TowerStore,
    state: &TowerState,
    mission_ids: &[String],
    active_agent_ids: Option<&HashSet<String>>,
) -> Vec<TowerWakeGateDigest> {
    read_tower_wake_status_gates(store, state, mission_ids, active_agent_ids)
        .await
        .into_iter()
        .map(TowerWakeGateDigest::from)
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TowerStatusSnapshot {
    pub active: bool,
    pub initialized: bool,
    pub state_lost: bool,
    pub state: Option<TowerState>,
    pub recovered_at: Option<String>,
    pub caller: String,
    pub gates: Vec<TowerStatusGate>,
    pub inbox_count: usize,
    pub concurrency: TowerRateLimitSnapshot,
    pub recent_activity: Vec<String>,
}

pub async fn read_tower_status(
    store: &TowerStore,
    caller_agent_id: &str,
    active_agent_ids: Option<&HashSet<String>>,
    max_running_tasks: Option<u32>,
) -> Result<TowerStatusSnapshot, String> {
    let concurrency = TowerRateLimit::global().snapshot(max_running_tasks);
    let recent_activity = store.recent_log(10).await;
    let state = match store.load().await {
        Ok(state) => state,
        Err(_) if store.state_was_lost().await => {
            return Ok(TowerStatusSnapshot {
                active: false,
                initialized: false,
                state_lost: true,
                state: None,
                recovered_at: None,
                caller: caller_agent_id.to_string(),
                gates: Vec::new(),
                inbox_count: 0,
                concurrency,
                recent_activity,
            });
        }
        Err(_) if !store.is_initialized().await? => {
            return Ok(TowerStatusSnapshot {
                active: false,
                initialized: false,
                state_lost: false,
                state: None,
                recovered_at: None,
                caller: caller_agent_id.to_string(),
                gates: Vec::new(),
                inbox_count: 0,
                concurrency,
                recent_activity,
            });
        }
        Err(error) => return Err(error),
    };
    let caller = store.resolve_caller_name(&state, caller_agent_id)?;
    let pending: Vec<_> = state
        .missions
        .iter()
        .filter(|mission| mission.status.is_open())
        .collect();
    let state_ref = &state;
    let gates = map_with_concurrency(pending, TOWER_GIT_CONCURRENCY, |_, mission| async move {
        let gate = store
            .mission_gate(state_ref, mission, active_agent_ids)
            .await?;
        Ok::<_, String>(TowerStatusGate {
            mission_id: mission.id.clone(),
            branch: mission.branch.clone(),
            status: mission.status.as_str().to_string(),
            ready: gate.ready,
            noop: gate.noop,
            reasons: gate
                .blocks
                .iter()
                .map(|block| block.reason.to_string())
                .collect(),
            blocks: gate.blocks.clone(),
            selected_review: gate.selected_review,
            binding: gate
                .review_binding
                .map(|binding| binding.as_str().to_string()),
        })
    })
    .await
    .into_iter()
    .collect::<Result<Vec<_>, _>>()?;
    let recovered_at = store.recovered_at().await?;
    let inbox_count = store.unread_inbox_count_locked(&caller).await?;
    Ok(TowerStatusSnapshot {
        active: true,
        initialized: true,
        state_lost: false,
        recovered_at,
        state: Some(state),
        caller,
        gates,
        inbox_count,
        concurrency,
        recent_activity,
    })
}

fn render_state_lost_report() -> String {
    [
        "# Tower status — STATE LOST",
        "",
        ".tower/comms/state.json is missing but the .tower/ directory still exists — this tower was initialized here before, and its comms state was deleted (e.g. by git clean). All recorded mission, roster, and review history is lost.",
        "",
        "Report this history loss to the user now — continuing silently would pretend the loss never happened.",
        "",
        "What still works:",
        "- TowerSend and TowerComplete recover a minimal state automatically (stamped recoveredAt, base taken from the current checkout branch) and keep delivering messages — worker reports are not lost.",
        "- TowerMerge and TowerRebase refuse with an explicit state-lost error instead of silently running against an empty history.",
        "- Mission branches and worktrees under .tower/worktrees survive on disk; re-plan from that evidence only after the user decides how to proceed.",
    ]
    .join("\n")
}

fn render_missions(state: &TowerState) -> Vec<String> {
    if state.missions.is_empty() {
        return vec!["(no missions planned — use TowerPlan)".into()];
    }
    let mut lines = vec![
        "| ID | Mission | Branch | Worktree | Status | Owner |".into(),
        "| -- | ------- | ------ | -------- | ------ | ----- |".into(),
    ];
    lines.extend(state.missions.iter().map(|mission| {
        format!(
            "| {} | {}{} | {} | {} | {} {} | {} |",
            mission.id,
            mission.title,
            if mission.kind == crate::tools::tower::types::TowerMissionKind::Survey {
                " 🔍"
            } else {
                ""
            },
            mission.branch,
            mission.worktree,
            mission.status.emoji(),
            mission.status.as_str(),
            mission.owner.as_deref().unwrap_or("—")
        )
    }));
    lines
}

fn render_unspawned_missions(state: &TowerState) -> Vec<String> {
    let pending = state
        .missions
        .iter()
        .filter(|mission| {
            mission.status == crate::tools::tower::types::TowerMissionStatus::Planned
                && mission.owner.is_none()
        })
        .collect::<Vec<_>>();
    if pending.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![String::new(), "## Awaiting spawn".into(), String::new()];
    lines.extend(pending.into_iter().map(|mission| {
        format!(
            "- {} ({}) — planned but no worker spawned yet: launch one with TowerSpawn(kind=\"worker\", mission_id=\"{}\", name=\"...\")",
            mission.id, mission.branch, mission.id
        )
    }));
    lines
}

fn render_death_warnings(state: &TowerState) -> Vec<String> {
    let dead_by_name = state
        .roster
        .agents
        .iter()
        .filter(|agent| agent.died_at.is_some())
        .map(|agent| (agent.name.as_str(), agent))
        .collect::<std::collections::HashMap<_, _>>();
    let mut lines = Vec::new();
    for mission in &state.missions {
        let Some(owner) = mission.owner.as_deref() else {
            continue;
        };
        if !mission.status.is_open() {
            continue;
        }
        let Some(entry) = dead_by_name.get(owner).copied() else {
            continue;
        };
        let death_status = entry.death_status.as_deref().unwrap_or("unknown");
        if TowerStore::is_stopped_by_user(entry) {
            lines.push(format!(
                "- 🛑 {} owner {} was stopped by the user ({}) — dead by intent: never resume it and do not reassign the mission unless the human asks",
                mission.id, entry.name, death_status
            ));
        } else if entry.kind == crate::tools::tower::types::TowerAgentKind::Reviewer {
            lines.push(format!(
                "- ⚠️ {} reviewer {} died ({}) — diagnose first: check the death status/reason and task state. A reviewer is one-shot: do not resume it or another roster agent; after diagnosis, spawn a fresh reviewer only if the mission still needs one",
                mission.id, entry.name, death_status
            ));
        } else {
            lines.push(format!(
                "- ⚠️ {} owner {} died ({}) — diagnose first: check why it died (the died entry's status/reason, its task state) before reviving anything. Resume with Agent(resume=\"{}\", run_in_background=true, prompt=\"...\") (foreground only when background task tools are unavailable) or reassign the mission only when the cause is transient (lost contact, timeout, OOM); a systematic cause (code or environment defect) is fixed or escalated to the human before any revive",
                mission.id, entry.name, death_status, entry.agent_id
            ));
        }
    }
    if lines.is_empty() {
        return lines;
    }
    let mut section = vec![String::new(), "## Dead workers".into(), String::new()];
    section.extend(lines);
    section
}

fn render_roster(state: &TowerState) -> Vec<String> {
    if state.roster.agents.is_empty() {
        return vec!["(no agents registered — spawn workers/reviewers with TowerSpawn)".into()];
    }
    state
        .roster
        .agents
        .iter()
        .map(|agent| {
            let assignment = match agent.kind {
                crate::tools::tower::types::TowerAgentKind::Worker => format!(
                    "mission {} (branch {}, worktree {})",
                    agent.mission_id.as_deref().unwrap_or("?"),
                    agent.branch.as_deref().unwrap_or("?"),
                    agent.worktree.as_deref().unwrap_or("?")
                ),
                crate::tools::tower::types::TowerAgentKind::Reviewer => format!(
                    "reviewing {}",
                    agent.review_target.as_deref().unwrap_or("?")
                ),
            };
            let death = agent
                .died_at
                .as_ref()
                .map(|_| format!(" — 💀 {}", agent.death_status.as_deref().unwrap_or("died")));
            format!(
                "- {} ({}) — agent {}, {}{}",
                agent.name,
                agent.kind.as_str(),
                agent.agent_id,
                assignment,
                death.unwrap_or_default()
            )
        })
        .collect()
}

fn render_review_gate(state: &TowerState, gates: &[TowerStatusGate]) -> Vec<String> {
    if !state
        .missions
        .iter()
        .any(|mission| mission.status.is_open())
    {
        return vec!["(no open missions — or none planned yet)".into()];
    }
    gates
        .iter()
        .map(|gate| {
            let review_text = gate
                .selected_review
                .as_ref()
                .map_or_else(String::new, |review| {
                    format!(
                        "; review=round {} by {} {}/{} binding={}",
                        review.round,
                        review.reviewer,
                        review.status,
                        if review.merge.is_empty() {
                            "legacy"
                        } else {
                            &review.merge
                        },
                        gate.binding.as_deref().unwrap_or("unavailable")
                    )
                });
            if gate.ready {
                format!(
                    "- {} ({}): READY reasons=none{}{}",
                    gate.branch,
                    gate.mission_id,
                    if gate.noop {
                        "; zero-diff survey no-op, review skipped"
                    } else {
                        ""
                    },
                    review_text
                )
            } else {
                let details = gate
                    .blocks
                    .iter()
                    .map(|block| {
                        block
                            .message
                            .strip_prefix("merge blocked: ")
                            .unwrap_or(&block.message)
                    })
                    .collect::<Vec<_>>()
                    .join(" | ");
                format!(
                    "- {} ({}): BLOCKED reasons={}{}{}",
                    gate.branch,
                    gate.mission_id,
                    gate.reasons.join(","),
                    if details.is_empty() {
                        String::new()
                    } else {
                        format!("; {details}")
                    },
                    review_text
                )
            }
        })
        .collect()
}

pub fn render_tower_status_summary(snapshot: &TowerStatusSnapshot) -> String {
    if snapshot.state_lost {
        return render_state_lost_report();
    }
    let Some(state) = snapshot.state.as_ref() else {
        return "tower is not initialized in this repository — run TowerInit first".into();
    };
    if !snapshot.active {
        return "# Tower status — OFF".into();
    }
    let mut lines = vec![
        format!(
            "# Tower status — base: {} (mode: {}), you are: {}",
            state.base, state.mode, snapshot.caller
        ),
        String::new(),
    ];
    if let Some(recovered_at) = snapshot.recovered_at.as_deref() {
        lines.extend([
            "## State recovered after a loss".into(),
            String::new(),
            format!(
                "This tower's state was reconstructed at {recovered_at} after .tower/comms/state.json was lost — every mission, roster entry, and review from before that point is gone, so the lists below are the recovered minimum, not reality. Report the history loss to the user before continuing routine orchestration."
            ),
            String::new(),
        ]);
    }
    lines.push("## Missions".into());
    lines.push(String::new());
    lines.extend(render_missions(state));
    lines.extend(render_unspawned_missions(state));
    lines.extend(render_death_warnings(state));
    lines.push(String::new());
    lines.push("## Roster".into());
    lines.push(String::new());
    lines.extend(render_roster(state));
    lines.push(String::new());
    lines.push("## Review gate (unmerged branches)".into());
    lines.push(String::new());
    lines.extend(render_review_gate(state, &snapshot.gates));
    if !state.missions.is_empty()
        && state
            .missions
            .iter()
            .all(|mission| !mission.status.is_open())
    {
        lines.extend([
            String::new(),
            "## Done".into(),
            String::new(),
            "All missions are merged or abandoned. Free the worktree checkouts now: run TowerTeardown (branches and .tower/comms/ are kept; dirty worktrees are protected).".into(),
        ]);
    }
    lines.extend([
        String::new(),
        "## Inbox".into(),
        String::new(),
        format!(
            "{} message(s) visible to you — read with TowerInbox.",
            snapshot.inbox_count
        ),
        String::new(),
        "## Concurrency (adaptive)".into(),
        String::new(),
        TowerRateLimit::render_snapshot(&snapshot.concurrency),
        String::new(),
        "## Recent activity".into(),
        String::new(),
    ]);
    if snapshot.recent_activity.is_empty() {
        lines.push("(activity log is empty)".into());
    } else {
        lines.extend(snapshot.recent_activity.iter().cloned());
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::tower::mission_gate::TowerGateBlock;
    use crate::tools::tower::types::{
        TowerAgentKind, TowerMission, TowerMissionStatus, TowerRoster, TowerRosterEntry,
    };

    fn mission(id: &str, status: TowerMissionStatus, owner: Option<&str>) -> TowerMission {
        TowerMission {
            id: id.into(),
            title: format!("Mission {id}"),
            slug: id.to_ascii_lowercase(),
            kind: Default::default(),
            scope: vec![format!("src/{id}/**")],
            branch: format!("feat/{id}"),
            worktree: format!("wt-{id}"),
            spawn_base: None,
            deps: Vec::new(),
            status,
            owner: owner.map(str::to_string),
            context: None,
            tasks: Vec::new(),
            notes: Vec::new(),
            blockers: Vec::new(),
        }
    }

    fn agent(
        name: &str,
        kind: TowerAgentKind,
        mission_id: Option<&str>,
        death_reason: Option<&str>,
    ) -> TowerRosterEntry {
        TowerRosterEntry {
            name: name.into(),
            agent_id: format!("agent-{name}"),
            session_id: Some("session-main".into()),
            kind,
            mission_id: mission_id.map(str::to_string),
            review_target: (kind == TowerAgentKind::Reviewer).then(|| "feat/M1".into()),
            review_mission_id: (kind == TowerAgentKind::Reviewer).then(|| "M1".into()),
            worktree: Some(format!("wt-{name}")),
            branch: Some(format!("feat/{name}")),
            spawned_at: "2026-11-04T00:00:00Z".into(),
            died_at: death_reason.map(|_| "2026-11-04T01:00:00Z".into()),
            death_status: death_reason.map(|_| {
                if kind == TowerAgentKind::Reviewer {
                    "failed".to_string()
                } else {
                    "killed".to_string()
                }
            }),
            death_reason: death_reason.map(str::to_string),
            status: death_reason.map(|_| "dead".into()),
        }
    }

    fn state(missions: Vec<TowerMission>, agents: Vec<TowerRosterEntry>) -> TowerState {
        TowerState {
            version: 1,
            base: "main".into(),
            mode: "branch".into(),
            created_at: "2026-11-04T00:00:00Z".into(),
            session_id: Some("session-main".into()),
            roster: TowerRoster { agents },
            missions,
        }
    }

    fn snapshot(state: TowerState, gates: Vec<TowerStatusGate>) -> TowerStatusSnapshot {
        TowerStatusSnapshot {
            active: true,
            initialized: true,
            state_lost: false,
            state: Some(state),
            recovered_at: None,
            caller: "tower".into(),
            gates,
            inbox_count: 0,
            concurrency: TowerRateLimitSnapshot {
                budget: 2,
                inflight: 0,
                blocked_until: None,
            },
            recent_activity: Vec::new(),
        }
    }

    fn review() -> TowerReviewInfo {
        TowerReviewInfo {
            reviewer: "r1".into(),
            target: "feat/M2".into(),
            round: 2,
            status: "p1-2items".into(),
            merge: "hold".into(),
            reviewed_commit: "abc123".into(),
            date: "2026-11-04T00:00:00Z".into(),
            file: "reviews/2.md".into(),
            mission: Some("M2".into()),
            seq: Some(2),
            modified_at_ms: 1,
        }
    }

    #[test]
    fn death_warnings_split_user_stop_worker_death_and_reviewer_one_shot() {
        let owner = mission("M1", TowerMissionStatus::Active, Some("w1"));
        let user_stopped = render_tower_status_summary(&snapshot(
            state(
                vec![owner.clone()],
                vec![agent(
                    "w1",
                    TowerAgentKind::Worker,
                    Some("M1"),
                    Some(super::super::TOWER_USER_CANCELLATION_REASON),
                )],
            ),
            Vec::new(),
        ));
        assert!(user_stopped.contains("## Dead workers"), "{user_stopped}");
        assert!(
            user_stopped.contains("M1 owner w1 was stopped by the user"),
            "{user_stopped}"
        );
        assert!(user_stopped.contains("dead by intent"), "{user_stopped}");
        assert!(!user_stopped.contains("diagnose first"), "{user_stopped}");
        assert!(!user_stopped.contains("Agent(resume="), "{user_stopped}");

        let worker_died = render_tower_status_summary(&snapshot(
            state(
                vec![owner.clone()],
                vec![agent(
                    "w1",
                    TowerAgentKind::Worker,
                    Some("M1"),
                    Some("provider blew up"),
                )],
            ),
            Vec::new(),
        ));
        assert!(worker_died.contains("M1 owner w1 died"), "{worker_died}");
        assert!(worker_died.contains("diagnose first"), "{worker_died}");
        assert!(
            worker_died.contains("Agent(resume=\"agent-w1\""),
            "{worker_died}"
        );

        let reviewer_died = render_tower_status_summary(&snapshot(
            state(
                vec![owner],
                vec![agent(
                    "w1",
                    TowerAgentKind::Reviewer,
                    Some("M1"),
                    Some("provider blew up"),
                )],
            ),
            Vec::new(),
        ));
        assert!(
            reviewer_died.contains("M1 reviewer w1 died"),
            "{reviewer_died}"
        );
        assert!(reviewer_died.contains("one-shot"), "{reviewer_died}");
        assert!(
            reviewer_died.contains("spawn a fresh reviewer"),
            "{reviewer_died}"
        );
        assert!(!reviewer_died.contains("Agent(resume="), "{reviewer_died}");
    }

    #[test]
    fn upstream_cancellation_reason_is_a_user_stop_and_fork_reason_still_is() {
        for reason in [
            "Aborted by the user",
            "  aborted by the user  ",
            "ABORTED BY THE USER",
        ] {
            let rendered = render_tower_status_summary(&snapshot(
                state(
                    vec![mission("M1", TowerMissionStatus::Active, Some("w1"))],
                    vec![agent(
                        "w1",
                        TowerAgentKind::Worker,
                        Some("M1"),
                        Some(reason),
                    )],
                ),
                Vec::new(),
            ));
            assert!(
                rendered.contains("M1 owner w1 was stopped by the user"),
                "{reason}: {rendered}"
            );
            assert!(rendered.contains("dead by intent"), "{reason}: {rendered}");
            assert!(!rendered.contains("diagnose first"), "{reason}: {rendered}");
        }

        for reason in [
            super::super::TOWER_USER_CANCELLATION_REASON,
            "  USER CANCELLED ",
        ] {
            let rendered = render_tower_status_summary(&snapshot(
                state(
                    vec![mission("M1", TowerMissionStatus::Active, Some("w1"))],
                    vec![agent(
                        "w1",
                        TowerAgentKind::Worker,
                        Some("M1"),
                        Some(reason),
                    )],
                ),
                Vec::new(),
            ));
            assert!(
                rendered.contains("M1 owner w1 was stopped by the user"),
                "{reason}: {rendered}"
            );
            assert!(rendered.contains("dead by intent"), "{reason}: {rendered}");
        }
    }

    #[test]
    fn non_cancellation_death_reasons_stay_ordinary_deaths() {
        for reason in ["provider blew up", "user canceled", "aborted"] {
            let rendered = render_tower_status_summary(&snapshot(
                state(
                    vec![mission("M1", TowerMissionStatus::Active, Some("w1"))],
                    vec![agent(
                        "w1",
                        TowerAgentKind::Worker,
                        Some("M1"),
                        Some(reason),
                    )],
                ),
                Vec::new(),
            ));
            assert!(
                rendered.contains("M1 owner w1 died"),
                "{reason}: {rendered}"
            );
            assert!(rendered.contains("diagnose first"), "{reason}: {rendered}");
            assert!(!rendered.contains("dead by intent"), "{reason}: {rendered}");
        }
    }

    #[test]
    fn upstream_user_stop_outranks_the_reviewer_one_shot_branch() {
        let rendered = render_tower_status_summary(&snapshot(
            state(
                vec![mission("M1", TowerMissionStatus::Active, Some("r1"))],
                vec![agent(
                    "r1",
                    TowerAgentKind::Reviewer,
                    Some("M1"),
                    Some("Aborted by the user"),
                )],
            ),
            Vec::new(),
        ));
        assert!(
            rendered.contains("M1 owner r1 was stopped by the user"),
            "{rendered}"
        );
        assert!(rendered.contains("dead by intent"), "{rendered}");
        assert!(!rendered.contains("one-shot"), "{rendered}");
        assert!(!rendered.contains("spawn a fresh reviewer"), "{rendered}");
    }

    #[test]
    fn death_warnings_user_stop_outranks_the_reviewer_one_shot_branch() {
        let rendered = render_tower_status_summary(&snapshot(
            state(
                vec![mission("M1", TowerMissionStatus::Active, Some("r1"))],
                vec![agent(
                    "r1",
                    TowerAgentKind::Reviewer,
                    Some("M1"),
                    Some(super::super::TOWER_USER_CANCELLATION_REASON),
                )],
            ),
            Vec::new(),
        ));
        assert!(
            rendered.contains("M1 owner r1 was stopped by the user"),
            "{rendered}"
        );
        assert!(rendered.contains("dead by intent"), "{rendered}");
        assert!(!rendered.contains("one-shot"), "{rendered}");
        assert!(!rendered.contains("spawn a fresh reviewer"), "{rendered}");
    }

    #[test]
    fn dashboard_renders_awaiting_spawn_roster_and_review_gate() {
        let missions = vec![
            mission("M1", TowerMissionStatus::Planned, None),
            mission("M2", TowerMissionStatus::Active, Some("w2")),
        ];
        let agents = vec![
            agent("w2", TowerAgentKind::Worker, Some("M2"), None),
            agent("r2", TowerAgentKind::Reviewer, Some("M2"), None),
        ];
        let gate = TowerStatusGate {
            mission_id: "M2".into(),
            branch: "feat/M2".into(),
            status: "active".into(),
            ready: false,
            noop: false,
            reasons: vec!["not-clean".into()],
            blocks: vec![TowerGateBlock::new(
                "not-clean",
                "merge blocked: findings need fixes",
            )],
            selected_review: Some(review()),
            binding: Some("exact-tip".into()),
        };
        let rendered = render_tower_status_summary(&snapshot(state(missions, agents), vec![gate]));

        assert!(rendered.contains("## Missions"), "{rendered}");
        assert!(
            rendered.contains("| ID | Mission | Branch | Worktree | Status | Owner |"),
            "{rendered}"
        );
        assert!(rendered.contains("## Awaiting spawn"), "{rendered}");
        assert!(
            rendered.contains("M1 (feat/M1) — planned but no worker spawned yet"),
            "{rendered}"
        );
        assert!(
            !rendered.contains("M2 (feat/M2) — planned but no worker spawned yet"),
            "{rendered}"
        );
        assert!(
            rendered.contains("w2 (worker) — agent agent-w2, mission M2"),
            "{rendered}"
        );
        assert!(
            rendered.contains("r2 (reviewer) — agent agent-r2, reviewing feat/M1"),
            "{rendered}"
        );
        assert!(
            rendered.contains("## Review gate (unmerged branches)"),
            "{rendered}"
        );
        assert!(
            rendered.contains("feat/M2 (M2): BLOCKED reasons=not-clean; findings need fixes"),
            "{rendered}"
        );
        assert!(
            rendered.contains("review=round 2 by r1 p1-2items/hold binding=exact-tip"),
            "{rendered}"
        );
        assert!(
            rendered.contains("## Inbox\n\n0 message(s) visible to you"),
            "{rendered}"
        );
    }

    #[test]
    fn dashboard_done_requires_all_missions_terminal() {
        let terminal = state(
            vec![
                mission("M1", TowerMissionStatus::Merged, Some("w1")),
                mission("M2", TowerMissionStatus::Abandoned, None),
            ],
            Vec::new(),
        );
        let rendered = render_tower_status_summary(&snapshot(terminal, Vec::new()));
        assert!(rendered.contains("## Done"), "{rendered}");
        assert!(
            rendered.contains("All missions are merged or abandoned"),
            "{rendered}"
        );
        assert!(
            rendered.contains("(no open missions — or none planned yet)"),
            "{rendered}"
        );
        assert!(!rendered.contains("## Awaiting spawn"), "{rendered}");

        let still_open = state(
            vec![mission("M1", TowerMissionStatus::Completed, None)],
            Vec::new(),
        );
        let rendered = render_tower_status_summary(&snapshot(still_open, Vec::new()));
        assert!(!rendered.contains("## Done"), "{rendered}");
    }

    #[test]
    fn dashboard_empty_and_recovered_states_keep_explicit_sections() {
        let empty =
            render_tower_status_summary(&snapshot(state(Vec::new(), Vec::new()), Vec::new()));
        assert!(
            empty.contains("(no missions planned — use TowerPlan)"),
            "{empty}"
        );
        assert!(
            empty.contains("(no agents registered — spawn workers/reviewers with TowerSpawn)"),
            "{empty}"
        );
        assert!(!empty.contains("## Done"), "{empty}");
        assert!(empty.contains("(activity log is empty)"), "{empty}");

        let mut recovered = snapshot(state(Vec::new(), Vec::new()), Vec::new());
        recovered.recovered_at = Some("2026-11-04T02:00:00Z".into());
        let recovered = render_tower_status_summary(&recovered);
        assert!(
            recovered.contains("## State recovered after a loss"),
            "{recovered}"
        );
        assert!(recovered.contains("2026-11-04T02:00:00Z"), "{recovered}");
        assert!(
            recovered.contains("Report the history loss to the user"),
            "{recovered}"
        );
    }
}
