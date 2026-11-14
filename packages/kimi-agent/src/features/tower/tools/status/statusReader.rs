use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::super::super::protocol::concurrency::{map_with_concurrency, TOWER_GIT_CONCURRENCY};
use super::super::super::protocol::missionGate::{TowerGateBlock, TowerReviewBinding};
use super::super::super::protocol::paths::resolve_tower_repo_root;
use super::super::super::protocol::store::{
    TowerActiveAgentIds, TowerMissionRuntimeOptions, TowerStore,
};
use super::super::super::protocol::types::{
    TowerMission, TowerReviewInfo, TowerRoster, TowerState,
};

const INBOX_COUNT_LIMIT: usize = 1000;
const RECENT_LOG_LINES: usize = 10;

#[derive(Debug, Clone, PartialEq)]
pub struct TowerStatusConcurrency {
    pub budget: f64,
    pub inflight: f64,
    pub blocked_until: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TowerStatusState {
    pub base: String,
    pub mode: String,
    pub recovered_at: Option<String>,
    pub missions: Vec<TowerMission>,
    pub roster: TowerRoster,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TowerStatusReview {
    pub reviewer: String,
    pub target: String,
    pub round: u32,
    pub status: String,
    pub merge: String,
    pub reviewed_commit: String,
    pub date: String,
    pub file: String,
    pub mission: Option<String>,
    pub seq: Option<u64>,
    pub mtime_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TowerStatusGate {
    pub mission_id: String,
    pub branch: String,
    pub status: String,
    pub ready: bool,
    pub noop: bool,
    pub reasons: Vec<String>,
    pub blocks: Vec<TowerGateBlock>,
    pub selected_review: Option<TowerStatusReview>,
    pub review_binding: Option<TowerReviewBinding>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TowerStatusInbox {
    pub count: usize,
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
                gate.review_binding
                    .map(|binding| binding.as_str().to_string())
            } else {
                None
            },
            status,
        }
    }
}

fn unavailable_status_gate(mission_id: &str, status: &str) -> TowerStatusGate {
    TowerStatusGate {
        mission_id: mission_id.to_string(),
        branch: String::new(),
        status: status.to_string(),
        ready: false,
        noop: false,
        reasons: Vec::new(),
        blocks: Vec::new(),
        selected_review: None,
        review_binding: None,
    }
}

fn status_review(review: TowerReviewInfo) -> TowerStatusReview {
    TowerStatusReview {
        reviewer: review.reviewer,
        target: review.target,
        round: review.round,
        status: review.status,
        merge: review.merge,
        reviewed_commit: review.reviewed_commit,
        date: review.date,
        file: review.file,
        mission: review.mission,
        seq: review.seq,
        mtime_ms: review.mtime_ms,
    }
}

async fn status_gate(
    store: &TowerStore,
    state: &TowerState,
    mission: &TowerMission,
    runtime: &TowerMissionRuntimeOptions,
) -> TowerStatusGate {
    let Ok(gate) = store.mission_gate(state, mission, runtime).await else {
        return unavailable_status_gate(&mission.id, "unavailable");
    };
    let reasons = gate
        .blocks
        .iter()
        .map(|block| block.reason.to_string())
        .collect();
    TowerStatusGate {
        mission_id: mission.id.clone(),
        branch: mission.branch.clone(),
        status: mission.status.as_str().to_string(),
        ready: gate.ready,
        noop: gate.noop,
        reasons,
        blocks: gate.blocks,
        selected_review: gate.selected_review.map(status_review),
        review_binding: gate.review_binding,
    }
}

pub async fn read_tower_wake_status_gates(
    store: &TowerStore,
    state: &TowerState,
    mission_ids: &[String],
    active_agent_ids: Option<&HashSet<String>>,
) -> Vec<TowerStatusGate> {
    let runtime = TowerMissionRuntimeOptions {
        active_agent_ids: active_agent_ids.map(|ids| TowerActiveAgentIds::Set(ids.clone())),
    };
    map_with_concurrency(
        mission_ids.to_vec(),
        TOWER_GIT_CONCURRENCY,
        |_, mission_id| {
            let runtime = &runtime;
            async move {
                let Some(mission) = state.missions.iter().find(|mission| mission.id == mission_id)
                else {
                    return unavailable_status_gate(&mission_id, "mission-missing");
                };
                if !mission.status.is_open() {
                    return TowerStatusGate {
                        branch: mission.branch.clone(),
                        ..unavailable_status_gate(&mission_id, mission.status.as_str())
                    };
                }
                status_gate(store, state, mission, runtime).await
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

#[derive(Debug, Clone, PartialEq)]
pub struct InitializedTowerStatus {
    pub active: bool,
    pub state: TowerStatusState,
    pub caller: String,
    pub gate: Vec<TowerStatusGate>,
    pub inbox: TowerStatusInbox,
    pub concurrency: TowerStatusConcurrency,
    pub recent_activity: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TowerStatus {
    NotInitialized { active: bool, state_lost: bool },
    Initialized(InitializedTowerStatus),
}

pub struct ReadTowerStatusInput<'a> {
    pub active: bool,
    pub cwd: &'a Path,
    pub agent_id: &'a str,
    pub running_agent_ids: &'a dyn Fn() -> HashSet<String>,
    pub concurrency: &'a dyn Fn() -> TowerStatusConcurrency,
}

pub async fn read_tower_status(
    input: &ReadTowerStatusInput<'_>,
) -> Result<TowerStatus, String> {
    let store = TowerStore::new(PathBuf::from(resolve_tower_repo_root(
        &input.cwd.to_string_lossy(),
    )));
    if !store.is_initialized().await? {
        return Ok(TowerStatus::NotInitialized {
            active: input.active,
            state_lost: store.is_state_lost().await?,
        });
    }
    let state = store.load().await?;
    let caller = store.resolve_caller_name(&state, input.agent_id)?;
    let pending: Vec<TowerMission> = state
        .missions
        .iter()
        .filter(|mission| mission.status.is_open())
        .cloned()
        .collect();
    let active_agent_ids = if !pending.is_empty() && input.agent_id == "main" {
        Some((input.running_agent_ids)())
    } else {
        None
    };
    let runtime = TowerMissionRuntimeOptions {
        active_agent_ids: active_agent_ids.map(TowerActiveAgentIds::Set),
    };
    let gates = map_with_concurrency(
        pending,
        TOWER_GIT_CONCURRENCY,
        |_, mission| {
            let state = &state;
            let runtime = &runtime;
            async move {
                let gate = match store.mission_gate(state, &mission, runtime).await {
                    Ok(gate) => gate,
                    Err(_) => {
                        return Ok::<_, String>(TowerStatusGate {
                            mission_id: mission.id.clone(),
                            branch: String::new(),
                            status: "unavailable".to_string(),
                            ready: false,
                            noop: false,
                            reasons: Vec::new(),
                            blocks: Vec::new(),
                            selected_review: None,
                            review_binding: None,
                        });
                    }
                };
                let reasons = gate
                    .blocks
                    .iter()
                    .map(|block| block.reason.to_string())
                    .collect();
                Ok::<_, String>(TowerStatusGate {
                    mission_id: mission.id.clone(),
                    branch: mission.branch.clone(),
                    status: mission.status.as_str().to_string(),
                    ready: gate.ready,
                    noop: gate.noop,
                    reasons,
                    blocks: gate.blocks,
                    selected_review: gate.selected_review.map(|review| TowerStatusReview {
                        reviewer: review.reviewer,
                        target: review.target,
                        round: review.round,
                        status: review.status,
                        merge: review.merge,
                        reviewed_commit: review.reviewed_commit,
                        date: review.date,
                        file: review.file,
                        mission: review.mission,
                        seq: review.seq,
                        mtime_ms: review.mtime_ms,
                    }),
                    review_binding: gate.review_binding,
                })
            }
        },
    )
    .await;
    let gate: Vec<TowerStatusGate> = gates.into_iter().collect::<Result<_, _>>()?;
    let inbox = store.read_inbox(&caller, INBOX_COUNT_LIMIT).await?;
    let concurrency = (input.concurrency)();
    let recent_activity = store.recent_log(RECENT_LOG_LINES).await;
    Ok(TowerStatus::Initialized(InitializedTowerStatus {
        active: input.active,
        state: TowerStatusState {
            base: state.base,
            mode: state.mode,
            recovered_at: state.recovered_at,
            missions: state.missions,
            roster: state.roster,
        },
        caller,
        gate,
        inbox: TowerStatusInbox {
            count: inbox.len(),
        },
        concurrency,
        recent_activity,
    }))
}

pub async fn read_tower_status_summary(
    input: &ReadTowerStatusInput<'_>,
) -> Result<String, String> {
    let status = read_tower_status(input).await?;
    match status {
        TowerStatus::NotInitialized { active, state_lost } => {
            if state_lost {
                return Ok(if active {
                    "Tower mode: ON\nTower state was lost: .tower/comms/state.json is missing but the .tower/ directory still exists — all recorded mission, roster, and review history is gone. Report this history loss to the user; TowerSend/TowerComplete recover a minimal state (stamped recoveredAt) automatically.".to_string()
                } else {
                    "Tower mode: OFF".to_string()
                });
            }
            Ok(if active {
                "Tower mode: ON\nTower is not initialized.".to_string()
            } else {
                "Tower mode: OFF".to_string()
            })
        }
        TowerStatus::Initialized(status) => {
            let mut lines = vec![
                if status.active {
                    "Tower status — ON".to_string()
                } else {
                    "Tower mode: OFF".to_string()
                },
                format!(
                    "Base: {} (mode: {}) · You are: {}",
                    status.state.base, status.state.mode, status.caller
                ),
            ];
            lines.extend(summary_recovered_at(&status.state));
            lines.push("Missions:".to_string());
            lines.extend(summary_missions(&status.state));
            lines.push("Roster:".to_string());
            lines.extend(summary_roster(&status.state));
            lines.push("Review gate:".to_string());
            lines.extend(summary_gate(&status.gate));
            lines.push(format!("Inbox: {} message(s)", status.inbox.count));
            lines.push(format!(
                "Concurrency: {}",
                summary_concurrency(&status.concurrency)
            ));
            lines.push("Recent activity:".to_string());
            if status.recent_activity.is_empty() {
                lines.push("  (empty)".to_string());
            } else {
                for line in &status.recent_activity {
                    lines.push(format!("  {line}"));
                }
            }
            Ok(lines.join("\n"))
        }
    }
}

fn summary_missions(state: &TowerStatusState) -> Vec<String> {
    if state.missions.is_empty() {
        return vec!["  (none)".to_string()];
    }
    state
        .missions
        .iter()
        .map(|mission| {
            format!(
                "  {} {} — {} · owner {} · {} · kind {}",
                mission.id,
                mission.title,
                mission.status.as_str(),
                mission.owner.as_deref().unwrap_or("—"),
                mission.branch,
                mission.kind.as_str()
            )
        })
        .collect()
}

fn summary_recovered_at(state: &TowerStatusState) -> Vec<String> {
    let Some(recovered_at) = state.recovered_at.as_deref() else {
        return Vec::new();
    };
    vec![format!(
        "⚠️ State recovered at {recovered_at} after a state loss — all mission, roster, and review history from before that point is gone; report this history loss to the user instead of continuing as if nothing happened."
    )]
}

fn summary_roster(state: &TowerStatusState) -> Vec<String> {
    if state.roster.agents.is_empty() {
        return vec!["  (none)".to_string()];
    }
    state
        .roster
        .agents
        .iter()
        .map(|agent| {
            let assignment = if agent.kind.as_str() == "worker" {
                format!("mission {}", agent.mission_id.as_deref().unwrap_or("?"))
            } else {
                format!("reviewing {}", agent.review_target.as_deref().unwrap_or("?"))
            };
            let death = match agent.died_at.as_deref() {
                None => String::new(),
                Some(_) => format!(
                    " · died {}",
                    agent.death_status.as_deref().unwrap_or("unknown")
                ),
            };
            format!(
                "  {} ({}) — {assignment} · {}{death}",
                agent.name,
                agent.kind.as_str(),
                agent.agent_id
            )
        })
        .collect()
}

fn summary_gate(gate: &[TowerStatusGate]) -> Vec<String> {
    if gate.is_empty() {
        return vec!["  (no open missions)".to_string()];
    }
    gate.iter()
        .map(|mission_gate| {
            let review_text = match mission_gate.selected_review.as_ref() {
                None => String::new(),
                Some(review) => format!(
                    " · review round {} by {} {}/{} · binding {}",
                    review.round,
                    review.reviewer,
                    review.status,
                    if review.merge.is_empty() {
                        "legacy"
                    } else {
                        review.merge.as_str()
                    },
                    mission_gate
                        .review_binding
                        .map(|binding| binding.as_str())
                        .unwrap_or("unavailable")
                ),
            };
            if mission_gate.ready {
                return format!(
                    "  {} {} — READY{}{review_text}",
                    mission_gate.mission_id,
                    mission_gate.branch,
                    if mission_gate.noop {
                        " (zero-diff no-op)"
                    } else {
                        ""
                    }
                );
            }
            let reasons = mission_gate
                .blocks
                .iter()
                .map(|block| block.reason)
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "  {} {} — BLOCKED: {reasons}{review_text}",
                mission_gate.mission_id, mission_gate.branch
            )
        })
        .collect()
}

fn summary_concurrency(snapshot: &TowerStatusConcurrency) -> String {
    let budget = if snapshot.budget.is_finite() {
        snapshot.budget.to_string()
    } else {
        "unlimited".to_string()
    };
    let base = format!(
        "budget: {budget} agent(s) · inflight: {}",
        snapshot.inflight
    );
    let Some(blocked_until) = snapshot.blocked_until else {
        return format!("{base} · spawns open");
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as f64)
        .unwrap_or(0.0);
    let remaining = blocked_until - now;
    if remaining > 0.0 {
        format!(
            "{base} · spawns paused for ~{}s",
            (remaining / 1000.0).ceil() as i64
        )
    } else {
        format!("{base} · spawn pause expired")
    }
}
