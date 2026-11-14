use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::wire::record::WireAsyncError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CapabilityId {
    KimiCu,
    KimiWebbridge,
}

impl CapabilityId {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::KimiCu => "kimi-cu",
            Self::KimiWebbridge => "kimi-webbridge",
        }
    }
}

impl FromStr for CapabilityId {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "kimi-cu" => Ok(Self::KimiCu),
            "kimi-webbridge" => Ok(Self::KimiWebbridge),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityReadiness {
    NotInstalled,
    Partial,
    Ready,
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CapabilityStepState {
    Ok,
    Missing,
    Failed,
}

impl CapabilityStepState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Missing => "missing",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityStep {
    pub id: String,
    pub state: CapabilityStepState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub optional: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CapabilityConfirmationRequired {
    StopProcesses,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityInstallProgress {
    pub running: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percent: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmation_required: Option<CapabilityConfirmationRequired>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityInstallOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_running_processes: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityDetectResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub steps: Vec<CapabilityStep>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityStatus {
    pub id: CapabilityId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_id: Option<String>,
    pub display_name: String,
    pub description: String,
    pub supported: bool,
    pub state: CapabilityReadiness,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub steps: Vec<CapabilityStep>,
    pub install: CapabilityInstallProgress,
}

pub type CapabilityInstallReporter = Arc<dyn Fn(String, Option<f64>) + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityDescriptor {
    pub id: CapabilityId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_id: Option<String>,
    pub display_name: String,
    pub description: String,
    pub supported: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapabilityInstallChange {
    pub id: CapabilityId,
    pub install: CapabilityInstallProgress,
}

#[async_trait]
pub trait CapabilityEntry: Send + Sync {
    fn id(&self) -> CapabilityId;
    fn plugin_id(&self) -> Option<&str>;
    fn display_name(&self) -> &str;
    fn description(&self) -> &str;
    fn supported(&self) -> bool;
    async fn detect(&self) -> Result<CapabilityDetectResult, WireAsyncError>;
    async fn install(
        &self,
        report: CapabilityInstallReporter,
        options: Option<CapabilityInstallOptions>,
    ) -> Result<Option<String>, WireAsyncError>;
}
