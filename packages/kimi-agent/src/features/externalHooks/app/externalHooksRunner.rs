use async_trait::async_trait;
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use super::super::internal::types::{HookBlockDecision, HookMatcherValue, HookResult};

pub const EXTERNAL_HOOKS_RUNNER_SERVICE_ID: &str = "externalHooksRunnerService";

#[derive(Debug, Clone, Default)]
pub struct ExternalHooksRunnerTriggerArgs {
    pub matcher_value: Option<HookMatcherValue>,
    pub input_data: Map<String, Value>,
    pub signal: Option<CancellationToken>,
    pub cwd: Option<String>,
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookExecutionError {
    pub event: String,
    pub session_id: Option<String>,
    pub command: Option<String>,
    pub message: String,
}

#[async_trait]
pub trait IExternalHooksRunnerService: Send + Sync {
    async fn trigger(
        &self,
        event: &str,
        args: ExternalHooksRunnerTriggerArgs,
    ) -> Vec<HookResult>;

    async fn trigger_block(
        &self,
        event: &str,
        args: ExternalHooksRunnerTriggerArgs,
    ) -> Option<HookBlockDecision>;

    async fn fire_and_forget_trigger(
        &self,
        event: &str,
        args: ExternalHooksRunnerTriggerArgs,
    ) -> Vec<HookResult>;

    fn has_hooks_for(&self, event: &str) -> bool;

    fn on_did_hook_error(&self) -> tokio::sync::broadcast::Receiver<HookExecutionError>;
}
