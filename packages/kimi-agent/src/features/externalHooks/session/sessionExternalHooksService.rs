use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde_json::{Map, Value};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::super::app::externalHooksRunner::{
    ExternalHooksRunnerTriggerArgs, IExternalHooksRunnerService,
};
use super::super::app::externalHooksRunnerService::ExternalHooksRunnerService;
use super::super::internal::types::{HookDef, HookMatcherValue};
use super::sessionExternalHooks::ISessionExternalHooksService;

pub const HEARTBEAT_INTERVAL_MS: u64 = 60_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionHeartbeat {
    pub uptime_ms: u64,
}

impl SessionHeartbeat {
    pub fn new(created_at_ms: u64, now_ms: u64) -> Self {
        Self {
            uptime_ms: now_ms.saturating_sub(created_at_ms),
        }
    }
}

pub struct SessionExternalHooksService {
    runner: Arc<ExternalHooksRunnerService>,
    session_id: String,
    session_title: Mutex<Option<String>>,
    created_at_ms: u64,
    heartbeat: Mutex<Option<JoinHandle<()>>>,
}

impl SessionExternalHooksService {
    pub fn new(hooks: Vec<HookDef>, session_id: impl Into<String>) -> Arc<Self> {
        Self::with_bootstrap(hooks, session_id, None, None)
    }

    pub fn with_bootstrap(
        hooks: Vec<HookDef>,
        session_id: impl Into<String>,
        cwd: Option<String>,
        client_type: Option<String>,
    ) -> Arc<Self> {
        let service = Arc::new(Self {
            runner: Arc::new(ExternalHooksRunnerService::new(&hooks).with_bootstrap(cwd, client_type)),
            session_id: session_id.into(),
            session_title: Mutex::new(None),
            created_at_ms: now_ms(),
            heartbeat: Mutex::new(None),
        });
        if tokio::runtime::Handle::try_current().is_ok() {
            service.sync_heartbeat();
        }
        service
    }

    pub fn runner(&self) -> Arc<ExternalHooksRunnerService> {
        self.runner.clone()
    }

    pub fn set_session_title(&self, title: Option<String>) {
        *self.session_title.lock() = title;
    }

    pub fn reload(&self, hooks: &[HookDef]) {
        self.runner.reload(hooks);
        self.sync_heartbeat();
    }

    pub async fn ready(&self) {
        self.sync_heartbeat();
    }

    pub async fn trigger_session_start(
        &self,
        source: &str,
        cwd: String,
        model: Option<String>,
        profile: Option<String>,
    ) {
        self.runner
            .trigger(
                "SessionStart",
                self.args(
                    Some(source),
                    Some(cwd),
                    None,
                    input_data([
                        ("source", Value::String(source.to_string())),
                        ("sessionTitle", self.title()),
                        ("model", model.map(Value::String).unwrap_or(Value::Null)),
                        ("profile", profile.map(Value::String).unwrap_or(Value::Null)),
                    ]),
                ),
            )
            .await;
    }

    pub async fn trigger_session_end(&self, reason: &str, cwd: String) {
        self.runner
            .trigger(
                "SessionEnd",
                self.args(
                    Some(reason),
                    Some(cwd),
                    None,
                    input_data([
                        ("reason", Value::String(reason.to_string())),
                        ("sessionTitle", self.title()),
                    ]),
                ),
            )
            .await;
    }

    pub async fn run_subagent_start(
        &self,
        agent_name: &str,
        prompt: &str,
        cwd: String,
        signal: Option<CancellationToken>,
    ) {
        self.runner
            .trigger(
                "SubagentStart",
                self.args(
                    Some(agent_name),
                    Some(cwd),
                    signal,
                    input_data([
                        ("agentName", Value::String(agent_name.to_string())),
                        ("prompt", Value::String(prompt.to_string())),
                        ("sessionTitle", self.title()),
                    ]),
                ),
            )
            .await;
    }

    pub async fn notify_subagent_stop(&self, agent_name: &str, response: &str, cwd: String) {
        let runner = self.runner.clone();
        let args = self.args(
            Some(agent_name),
            Some(cwd),
            None,
            input_data([
                ("agentName", Value::String(agent_name.to_string())),
                ("response", Value::String(response.to_string())),
                ("sessionTitle", self.title()),
            ]),
        );
        tokio::spawn(async move {
            runner.fire_and_forget_trigger("SubagentStop", args).await;
        });
    }

    pub fn sync_heartbeat(self: &Arc<Self>) {
        self.disarm_heartbeat();
        if !self.runner.has_hooks_for("SessionHeartbeat")
            || tokio::runtime::Handle::try_current().is_err()
        {
            return;
        }
        let weak = Arc::downgrade(self);
        *self.heartbeat.lock() = Some(tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(HEARTBEAT_INTERVAL_MS)).await;
                let Some(this) = weak.upgrade() else { return };
                if !this.runner.has_hooks_for("SessionHeartbeat") {
                    return;
                }
                let heartbeat = SessionHeartbeat::new(this.created_at_ms, now_ms());
                let runner = this.runner.clone();
                let args = this.args(
                    None,
                    None,
                    None,
                    input_data([
                        ("sessionTitle", this.title()),
                        ("uptimeMs", Value::from(heartbeat.uptime_ms)),
                    ]),
                );
                tokio::spawn(async move {
                    runner.fire_and_forget_trigger("SessionHeartbeat", args).await;
                });
            }
        }));
    }

    pub fn dispose(&self) {
        self.disarm_heartbeat();
    }

    fn disarm_heartbeat(&self) {
        if let Some(heartbeat) = self.heartbeat.lock().take() {
            heartbeat.abort();
        }
    }

    fn title(&self) -> Value {
        self.session_title
            .lock()
            .clone()
            .map(Value::String)
            .unwrap_or(Value::Null)
    }

    fn args(
        &self,
        matcher: Option<&str>,
        cwd: Option<String>,
        signal: Option<CancellationToken>,
        input_data: Map<String, Value>,
    ) -> ExternalHooksRunnerTriggerArgs {
        ExternalHooksRunnerTriggerArgs {
            matcher_value: matcher.map(|value| HookMatcherValue::Text(value.to_string())),
            input_data,
            signal,
            cwd,
            session_id: Some(self.session_id.clone()),
        }
    }
}

impl ISessionExternalHooksService for SessionExternalHooksService {}

impl Drop for SessionExternalHooksService {
    fn drop(&mut self) {
        self.dispose();
    }
}

fn input_data<const N: usize>(entries: [(&str, Value); N]) -> Map<String, Value> {
    entries
        .into_iter()
        .filter(|(_, value)| !value.is_null())
        .map(|(key, value)| (key.to_string(), value))
        .collect()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
