use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::broadcast;

use super::super::internal::matchHooks::{
    HookRunCallbacks, block_decision, index_hooks, run_matched_hooks,
};
use super::super::internal::types::{HookBlockDecision, HookDef, HookResult};
use super::externalHooksRunner::{
    ExternalHooksRunnerTriggerArgs, HookExecutionError, IExternalHooksRunnerService,
};

pub struct ExternalHooksRunnerService {
    by_event: RwLock<BTreeMap<String, Vec<HookDef>>>,
    cwd: Option<String>,
    client_type: Option<String>,
    callbacks: HookRunCallbacks,
    on_did_reload: broadcast::Sender<()>,
    on_did_hook_error: broadcast::Sender<HookExecutionError>,
}

impl ExternalHooksRunnerService {
    pub fn new(hooks: &[HookDef]) -> Self {
        let (on_did_reload, _) = broadcast::channel(16);
        let (on_did_hook_error, _) = broadcast::channel(64);
        Self {
            by_event: RwLock::new(index_hooks(hooks)),
            cwd: None,
            client_type: None,
            callbacks: HookRunCallbacks::default(),
            on_did_reload,
            on_did_hook_error,
        }
    }

    pub fn with_bootstrap(
        mut self,
        cwd: Option<String>,
        client_type: Option<String>,
    ) -> Self {
        self.cwd = cwd;
        self.client_type = client_type;
        self
    }

    pub fn with_callbacks(mut self, callbacks: HookRunCallbacks) -> Self {
        self.callbacks = callbacks;
        self
    }

    pub fn reload(&self, hooks: &[HookDef]) {
        *self.by_event.write().unwrap() = index_hooks(hooks);
        let _ = self.on_did_reload.send(());
    }

    pub fn subscribe_reload(&self) -> broadcast::Receiver<()> {
        self.on_did_reload.subscribe()
    }

    pub fn subscribe_hook_error(&self) -> broadcast::Receiver<HookExecutionError> {
        self.on_did_hook_error.subscribe()
    }

    pub fn summary(&self) -> BTreeMap<String, usize> {
        self.by_event
            .read()
            .unwrap()
            .iter()
            .map(|(event, hooks)| (event.clone(), hooks.len()))
            .collect()
    }

    fn has_hooks_for(&self, event: &str) -> bool {
        self.by_event
            .read()
            .unwrap()
            .get(event)
            .is_some_and(|hooks| !hooks.is_empty())
    }

    async fn trigger_inner(
        &self,
        event: &str,
        mut args: ExternalHooksRunnerTriggerArgs,
        fail_closed: bool,
    ) -> Vec<HookResult> {
        if args.cwd.is_none() {
            args.cwd = self.cwd.clone();
        }
        if let Some(client_type) = &self.client_type {
            args.input_data
                .entry("clientType".to_string())
                .or_insert_with(|| Value::String(client_type.clone()));
        }
        let session_id = args.session_id.clone();
        let base_callbacks = self.callbacks.clone();
        let errors = self.on_did_hook_error.clone();
        let callbacks = HookRunCallbacks {
            on_triggered: base_callbacks.on_triggered,
            on_resolved: base_callbacks.on_resolved,
            on_error: Some(Arc::new(move |failed_event, hook, message| {
                if let Some(callback) = &base_callbacks.on_error {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        callback(failed_event, hook, message);
                    }));
                }
                tracing::warn!(
                    event = failed_event,
                    session_id = session_id.as_deref(),
                    command = hook.command.as_str(),
                    "hook execution failed for event {failed_event}: {message}"
                );
                let _ = errors.send(HookExecutionError {
                    event: failed_event.to_string(),
                    session_id: session_id.clone(),
                    command: Some(hook.command.clone()),
                    message: message.to_string(),
                });
            })), 
        };
        let by_event = self.by_event.read().unwrap().clone();
        run_matched_hooks(&by_event, event, args, callbacks, fail_closed).await
    }
}

#[async_trait]
impl IExternalHooksRunnerService for ExternalHooksRunnerService {
    async fn trigger(
        &self,
        event: &str,
        args: ExternalHooksRunnerTriggerArgs,
    ) -> Vec<HookResult> {
        self.trigger_inner(event, args, false).await
    }

    async fn trigger_block(
        &self,
        event: &str,
        args: ExternalHooksRunnerTriggerArgs,
    ) -> Option<HookBlockDecision> {
        block_decision(event, &self.trigger_inner(event, args, true).await)
    }

    async fn fire_and_forget_trigger(
        &self,
        event: &str,
        args: ExternalHooksRunnerTriggerArgs,
    ) -> Vec<HookResult> {
        self.trigger(event, args).await
    }

    fn has_hooks_for(&self, event: &str) -> bool {
        Self::has_hooks_for(self, event)
    }

    fn on_did_hook_error(&self) -> broadcast::Receiver<HookExecutionError> {
        self.subscribe_hook_error()
    }
}
