use std::sync::LazyLock;
use std::sync::Arc;
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
use serde::{Deserialize, Serialize};

use crate::persistence::JsonAtomicDocumentStore;
use crate::prompt::builder::NOTIFY_USER_GUIDANCE;

pub const SESSION_NOTIFY_SERVICE_ID: &str = "sessionNotify";
pub const SESSION_NOTIFY_SCOPE: &str = "session";
pub const SESSION_NOTIFY_ACTIVATION: &str = "onDemand";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub struct SessionNotifyState {
    pub enabled: bool,
}

pub struct SessionNotify {
    store: JsonAtomicDocumentStore,
    scope: String,
    enabled: bool,
}

impl SessionNotify {
    pub fn new(store: JsonAtomicDocumentStore, scope: impl Into<String>) -> Self {
        Self {
            store,
            scope: scope.into(),
            enabled: false,
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn load(
        &mut self,
        initial: bool,
        latest_profile_system_prompt: Option<&str>,
    ) -> Result<bool, String> {
        let scope = self.scope.clone();
        let stored = self
            .store
            .get::<serde_json::Value>(&scope, "state.json")
            .map_err(|error| error.to_string())?;
        if stored.is_some_and(|state| {
            state.get("enabled").and_then(serde_json::Value::as_bool) == Some(true)
        }) {
            self.enabled = true;
            return Ok(true);
        }
        if initial {
            self.enabled = true;
        } else {
            self.enabled = latest_profile_system_prompt.is_some_and(|prompt| {
                prompt.contains(NOTIFY_USER_GUIDANCE)
                    || prompt.contains("The `NotifyUser` tool is your channel to the user while you work")
            });
        }
        self.store
            .set(
                &scope,
                "state.json",
                &SessionNotifyState {
                    enabled: self.enabled,
                },
            )
            .map_err(|error| error.to_string())?;
        Ok(self.enabled)
    }
}

pub trait ISessionNotify: Send + Sync {
    fn ready(
        &self,
    ) -> futures_util::future::BoxFuture<'static, Result<(), crate::wire::record::WireAsyncError>>;
    fn enabled(&self) -> bool;
}

impl ISessionNotify for SessionNotify {
    fn ready(
        &self,
    ) -> futures_util::future::BoxFuture<'static, Result<(), crate::wire::record::WireAsyncError>> {
        Box::pin(async { Ok(()) })
    }

    fn enabled(&self) -> bool {
        SessionNotify::enabled(self)
    }
}


pub static I_SESSION_NOTIFY: LazyLock<ServiceIdentifier<Arc<dyn ISessionNotify>>> =
    LazyLock::new(|| create_decorator("sessionNotify"));
