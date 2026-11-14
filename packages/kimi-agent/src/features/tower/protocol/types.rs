use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TowerAgentKind {
    Worker,
    Reviewer,
}

impl TowerAgentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Worker => "worker",
            Self::Reviewer => "reviewer",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TowerRosterEntry {
    pub name: String,
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub kind: TowerAgentKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mission_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_mission_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub spawned_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_inbox_read_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inbox_ack_ids: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub died_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub death_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub death_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TowerRoster {
    #[serde(default)]
    pub agents: Vec<TowerRosterEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TowerMissionStatus {
    Planned,
    Active,
    Completed,
    Blocked,
    Paused,
    Merged,
    Abandoned,
}

impl TowerMissionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Active => "active",
            Self::Completed => "completed",
            Self::Blocked => "blocked",
            Self::Paused => "paused",
            Self::Merged => "merged",
            Self::Abandoned => "abandoned",
        }
    }

    pub fn emoji(self) -> &'static str {
        match self {
            Self::Planned => "🟡",
            Self::Active => "🔵",
            Self::Completed => "🟢",
            Self::Blocked => "🔴",
            Self::Paused => "⏸️",
            Self::Merged => "✅",
            Self::Abandoned => "🚫",
        }
    }

    pub fn is_open(self) -> bool {
        !matches!(self, Self::Merged | Self::Abandoned)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TowerMissionKind {
    #[default]
    Build,
    Survey,
    Artifact,
}

impl TowerMissionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::Survey => "survey",
            Self::Artifact => "artifact",
        }
    }
}

fn deserialize_mission_kind<'de, D>(deserializer: D) -> Result<TowerMissionKind, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<TowerMissionKind>::deserialize(deserializer)?;
    Ok(value.unwrap_or_default())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TowerMissionTask {
    pub text: String,
    pub done: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dropped: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TowerRebaseRecord {
    pub from_commit: String,
    pub to_commit: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TowerMission {
    pub id: String,
    pub title: String,
    pub slug: String,
    #[serde(default, deserialize_with = "deserialize_mission_kind")]
    pub kind: TowerMissionKind,
    #[serde(default)]
    pub scope: Vec<String>,
    pub branch: String,
    pub worktree: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn_base: Option<String>,
    #[serde(default)]
    pub deps: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_rebase: Option<TowerRebaseRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tip: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_artifacts: Option<Vec<String>>,
    pub status: TowerMissionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    #[serde(default)]
    pub tasks: Vec<TowerMissionTask>,
    #[serde(default)]
    pub notes: Vec<String>,
    #[serde(default)]
    pub blockers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TowerState {
    pub version: u32,
    pub base: String,
    pub mode: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovered_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inbox_ack_ids: Option<Vec<String>>,
    #[serde(default)]
    pub roster: TowerRoster,
    #[serde(default)]
    pub missions: Vec<TowerMission>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TowerFindingType {
    Bug,
    Improve,
    Vuln,
    Idea,
}

impl TowerFindingType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bug => "bug",
            Self::Improve => "improve",
            Self::Vuln => "vuln",
            Self::Idea => "idea",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TowerFindingSeverity {
    Low,
    #[default]
    Medium,
    High,
    Critical,
}

impl TowerFindingSeverity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TowerReviewInfo {
    pub reviewer: String,
    pub target: String,
    pub round: u32,
    pub status: String,
    pub merge: String,
    pub reviewed_commit: String,
    pub date: String,
    pub file: String,
    pub mtime_ms: u64,
    pub seq: Option<u64>,
    pub mission: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TowerInboxItem {
    pub message_id: String,
    pub file: String,
    pub from: String,
    pub to: String,
    pub subject: String,
    pub sent_at: String,
    pub scope: Option<String>,
    pub action: Option<String>,
    pub consent_ref: Option<String>,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TowerInboxPage {
    pub items: Vec<TowerInboxItem>,
    pub unread: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TowerInboxDelivery {
    pub from: String,
    pub to: String,
    pub subject: String,
    pub message_id: String,
    pub file: String,
    pub sent_at: String,
    pub mission_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TowerInboxSent {
    pub item: TowerInboxItem,
    pub delivery: TowerInboxDelivery,
    pub mission_id: Option<String>,
    pub created: bool,
    pub activity_log_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TowerMergeStatus {
    Merged,
    Advanced,
    Noop,
}

impl TowerMergeStatus {
    pub fn after(evaluated_tip: &str, current_tip: &str, noop: bool) -> Self {
        if evaluated_tip != current_tip {
            Self::Advanced
        } else if noop {
            Self::Noop
        } else {
            Self::Merged
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Merged => "merged",
            Self::Advanced => "advanced",
            Self::Noop => "noop",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TowerBranchConflict {
    pub branch: String,
    pub files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TowerMergeResult {
    pub status: TowerMergeStatus,
    pub merge_commit: String,
    pub evaluated_branch_tip: String,
    pub current_branch_tip: String,
    pub conflicts_with: Vec<TowerBranchConflict>,
    pub noop: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TowerRebaseMissionStatus {
    UpToDate,
    Rebased,
    Conflict,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TowerRebaseMissionResult {
    pub status: TowerRebaseMissionStatus,
    pub from_commit: Option<String>,
    pub to_commit: Option<String>,
    pub files: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TowerReanchorRelation {
    NoBranch,
    UpToDate,
    Stale,
}

impl TowerReanchorRelation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoBranch => "no-branch",
            Self::UpToDate => "up-to-date",
            Self::Stale => "stale",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TowerReanchorMissionDigest {
    pub id: String,
    pub branch: String,
    pub relation: TowerReanchorRelation,
    pub branch_tip: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TowerReanchorDigest {
    pub from: String,
    pub to: String,
    pub base_tip: String,
    pub missions: Vec<TowerReanchorMissionDigest>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TowerInitResult {
    pub base: String,
    pub created: bool,
    pub retired_agents: Vec<String>,
    pub checkout: String,
    pub rebased_from: Option<String>,
    pub rebase_digest: Option<TowerReanchorDigest>,
    pub open_missions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TowerPlanInput {
    pub title: String,
    pub scope: Vec<String>,
    pub tasks: Option<Vec<String>>,
    pub context: Option<String>,
    pub deps: Option<Vec<String>>,
    pub kind: Option<TowerMissionKind>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TowerSendInput {
    pub to: String,
    pub subject: String,
    pub body: String,
    pub scope: Option<String>,
    pub action: Option<String>,
    pub consent_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TowerSendResult {
    pub item: TowerInboxItem,
    pub mission_id: Option<String>,
    pub activity_log_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TowerSendOutcome {
    pub caller: String,
    pub session_id: Option<String>,
    pub recovered_at: Option<String>,
    pub target_agent_id: Option<String>,
    pub placeholder: bool,
    pub message: TowerInboxSent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TowerCompleteResult {
    pub mission: Option<TowerMission>,
    pub recovered_at: Option<String>,
    pub message: Option<TowerSendResult>,
    pub notification_created: bool,
    pub notification_error: Option<String>,
    pub activity_log_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TowerFindingInput {
    pub finding_type: TowerFindingType,
    pub title: String,
    pub severity: Option<TowerFindingSeverity>,
    pub summary: String,
    pub location: Option<String>,
    pub details: String,
    pub suggested_fix: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TowerReviewInput {
    pub target: String,
    pub status: String,
    pub merge: String,
    pub findings: String,
    pub checks: Option<Vec<String>>,
    pub decision: String,
    pub acceptance_tip: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TowerReviewResult {
    pub review: TowerReviewInfo,
    pub stored_messages: Vec<TowerSendResult>,
    pub notification_error: Option<String>,
    pub activity_log_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TowerTaskDrop {
    pub text: String,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TowerMissionPatch {
    pub status: Option<TowerMissionStatus>,
    pub note: Option<String>,
    pub blocker: Option<String>,
    pub clear_blockers: Option<bool>,
    pub task_done: Option<String>,
    pub task_drop: Option<TowerTaskDrop>,
    pub owner: Option<String>,
    pub scope: Option<Vec<String>>,
    pub spawn_base: Option<String>,
}
