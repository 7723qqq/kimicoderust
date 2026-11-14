use serde::{Deserialize, Serialize};

pub const TOWER_TOOL_NAMES: &[&str] = &[
    "TowerPlan",
    "TowerSpawn",
    "TowerMerge",
    "TowerRebase",
    "TowerTeardown",
    "TowerSend",
    "TowerInbox",
    "TowerFinding",
    "TowerReview",
    "TowerMission",
    "TowerStatus",
];
pub const TOWER_WORKER_PROFILE: &str = "tower-worker";
pub const TOWER_FLAG_ID: &str = "tower";
pub const AGENT_TOWER_SERVICE_ID: &str = "agentTowerService";

pub fn has_pinned_permission_mode(profile_name: Option<&str>) -> bool {
    profile_name == Some(TOWER_WORKER_PROFILE)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TowerEnterFailure {
    NotMainAgent,
    ExperimentOff,
    FeatureNotAssembled,
    OwnedByLiveSession {
        owner: String,
        owner_title: Option<String>,
    },
}

impl TowerEnterFailure {
    pub fn reason(&self) -> &'static str {
        match self {
            Self::NotMainAgent => "not-main-agent",
            Self::ExperimentOff => "experiment-off",
            Self::FeatureNotAssembled => "feature-not-assembled",
            Self::OwnedByLiveSession { .. } => "owned-by-live-session",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TowerEnterResult {
    Entered,
    Failed(TowerEnterFailure),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TowerExitReason {
    User,
    Takeover,
    ForeignReconcile,
    ModeMutex,
}

impl TowerExitReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Takeover => "takeover",
            Self::ForeignReconcile => "foreign-reconcile",
            Self::ModeMutex => "mode-mutex",
        }
    }
}

impl Serialize for TowerEnterFailure {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("TowerEnterFailure", 4)?;
        state.serialize_field("entered", &false)?;
        state.serialize_field("reason", self.reason())?;
        if let Self::OwnedByLiveSession { owner, owner_title } = self {
            state.serialize_field("owner", owner)?;
            if let Some(owner_title) = owner_title {
                state.serialize_field("ownerTitle", owner_title)?;
            }
        }
        state.end()
    }
}

impl<'de> Deserialize<'de> for TowerEnterFailure {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        tower_enter_failure_from_value(&value).map_err(serde::de::Error::custom)
    }
}

impl Serialize for TowerEnterResult {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Entered => {
                use serde::ser::SerializeStruct;
                let mut state = serializer.serialize_struct("TowerEnterResult", 1)?;
                state.serialize_field("entered", &true)?;
                state.end()
            }
            Self::Failed(failure) => failure.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for TowerEnterResult {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        match value.get("entered").and_then(serde_json::Value::as_bool) {
            Some(true) => Ok(Self::Entered),
            Some(false) => tower_enter_failure_from_value(&value)
                .map(Self::Failed)
                .map_err(serde::de::Error::custom),
            _ => Err(serde::de::Error::custom("entered must be a boolean")),
        }
    }
}

fn tower_enter_failure_from_value(value: &serde_json::Value) -> Result<TowerEnterFailure, String> {
    if value.get("entered").and_then(serde_json::Value::as_bool) != Some(false) {
        return Err("entered must be false".to_string());
    }
    let reason = value
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "reason must be a string".to_string())?;
    match reason {
        "not-main-agent" => Ok(TowerEnterFailure::NotMainAgent),
        "experiment-off" => Ok(TowerEnterFailure::ExperimentOff),
        "feature-not-assembled" => Ok(TowerEnterFailure::FeatureNotAssembled),
        "owned-by-live-session" => {
            let owner = value
                .get("owner")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| "owner must be a string".to_string())?
                .to_string();
            let owner_title = value
                .get("ownerTitle")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            Ok(TowerEnterFailure::OwnedByLiveSession { owner, owner_title })
        }
        _ => Err("unknown tower enter failure reason".to_string()),
    }
}

pub fn tower_enter_failure_message(failure: &TowerEnterFailure) -> String {
    match failure {
        TowerEnterFailure::NotMainAgent => {
            "tower mode is only supported by the main agent".to_string()
        }
        TowerEnterFailure::ExperimentOff => "the tower experiment is disabled; enable it with KIMI_CODE_EXPERIMENTAL_TOWER=1 or `[experimental] tower = true` in config.toml".to_string(),
        TowerEnterFailure::FeatureNotAssembled => {
            "the tower feature is not assembled in this process; a restart is required".to_string()
        }
        TowerEnterFailure::OwnedByLiveSession { owner, owner_title } => {
            let owner = owner_title
                .as_ref()
                .map(|title| format!("{title} ({owner})"))
                .unwrap_or_else(|| owner.clone());
            format!("another live session owns the workspace tower (session {owner})")
        }
    }
}
