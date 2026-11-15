use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use kimi_core::primitives::errors::{Error2, Error2Options};
use parking_lot::Mutex as StdMutex;
use serde_json::Value;
use tokio::sync::{Mutex, OnceCell};

use crate::app::config::config::{ConfigSchema, JsonConfigSchema};
use crate::app::config::errors::CONFIG_INVALID;
use crate::app::project_local_config::project_local_config::IProjectLocalConfigService;
use crate::environment::environment::LOCAL_ENVIRONMENT_ID;
use crate::persistence::interface::storage::StorageError;
use crate::wire::record::WireAsyncError;
use crate::workspace::workspace_trust::workspace_trust::IWorkspaceTrust;

use super::config_section::PERSISTED_PERMISSION_RULE_SCHEMA;
use super::matches_rule::parse_permission_pattern;
use super::permission_rules::PermissionRule;
use super::project_permission_rules::IProjectPermissionRulesService;

#[derive(Debug)]
pub struct ProjectPermissionRulesError(String);

impl fmt::Display for ProjectPermissionRulesError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for ProjectPermissionRulesError {}

#[derive(Debug, Clone)]
struct ReadyError {
    code: Option<String>,
    message: String,
}

impl ReadyError {
    fn from_error(error: WireAsyncError) -> Self {
        let code = error
            .downcast_ref::<Error2>()
            .map(|error| error.code().as_str().to_string())
            .or_else(|| {
                error
                    .downcast_ref::<StorageError>()
                    .map(|error| error.code().as_str().to_string())
            });
        Self {
            code,
            message: error.to_string(),
        }
    }

    fn into_error(self) -> WireAsyncError {
        match self.code {
            Some(code) => Box::new(Error2::new(code, self.message)),
            None => Box::new(ProjectPermissionRulesError(self.message)),
        }
    }
}

pub struct ProjectPermissionRulesService {
    cwd: String,
    local: bool,
    current_rules: StdMutex<Vec<PermissionRule>>,
    trusted: AtomicBool,
    ready_state: OnceCell<Result<(), ReadyError>>,
    mutation_tail: Mutex<()>,
    local_config: Arc<dyn IProjectLocalConfigService>,
    trust: Arc<dyn IWorkspaceTrust>,
}

impl ProjectPermissionRulesService {
    pub fn new(
        session_cwd: &str,
        workdir_cwd: Option<&str>,
        environment_id: &str,
        local_config: Arc<dyn IProjectLocalConfigService>,
        trust: Arc<dyn IWorkspaceTrust>,
    ) -> Self {
        Self {
            cwd: workdir_cwd.unwrap_or(session_cwd).to_string(),
            local: environment_id == LOCAL_ENVIRONMENT_ID,
            current_rules: StdMutex::new(Vec::new()),
            trusted: AtomicBool::new(false),
            ready_state: OnceCell::new(),
            mutation_tail: Mutex::new(()),
            local_config,
            trust,
        }
    }

    async fn load_from_disk(&self) -> Result<(), WireAsyncError> {
        let _guard = self.mutation_tail.lock().await;
        let trusted = self.trust.get().await?;
        self.trusted.store(trusted, Ordering::Release);
        if !trusted {
            return Ok(());
        }
        let loaded = self.local_config.read_permission_rules(&self.cwd).await?;
        let parsed = parse_project_permission_rules(&loaded.rules)?;
        *self.current_rules.lock() = parsed;
        Ok(())
    }
}

#[async_trait]
impl IProjectPermissionRulesService for ProjectPermissionRulesService {
    async fn ready(&self) -> Result<(), WireAsyncError> {
        if !self.local {
            return Ok(());
        }
        let result = self
            .ready_state
            .get_or_init(|| async { self.load_from_disk().await.map_err(ReadyError::from_error) })
            .await;
        match result {
            Ok(()) => Ok(()),
            Err(error) => Err(error.clone().into_error()),
        }
    }

    fn rules(&self) -> Vec<PermissionRule> {
        self.current_rules.lock().clone()
    }

    async fn append_rules(&self, rules: &[PermissionRule]) -> Result<(), WireAsyncError> {
        if !self.local {
            return Err(Box::new(ProjectPermissionRulesError(
                "Project permission rules are not supported in remote environments".to_string(),
            )));
        }
        let values = permission_rules_to_values(rules)?;
        let _guard = self.mutation_tail.lock().await;
        parse_project_permission_rules(&values)?;
        let appended = self
            .local_config
            .append_permission_rules(&self.cwd, &values)
            .await?;
        if self.trusted.load(Ordering::Acquire) {
            *self.current_rules.lock() = parse_project_permission_rules(&appended.rules)?;
        }
        Ok(())
    }
}

fn permission_rules_to_values(rules: &[PermissionRule]) -> Result<Vec<Value>, WireAsyncError> {
    let mut values = Vec::with_capacity(rules.len());
    for rule in rules {
        values.push(serde_json::to_value(rule).map_err(|error| Box::new(error) as WireAsyncError)?);
    }
    Ok(values)
}

fn parse_project_permission_rules(
    raw: &[Value],
) -> Result<Vec<PermissionRule>, WireAsyncError> {
    let mut rules = Vec::with_capacity(raw.len());
    for entry in raw {
        let value = match entry {
            Value::Object(map) => {
                let mut map = map.clone();
                map.entry("scope".to_string())
                    .or_insert_with(|| Value::String("project".to_string()));
                Value::Object(map)
            }
            entry => entry.clone(),
        };
        let parsed = JsonConfigSchema::new(PERSISTED_PERMISSION_RULE_SCHEMA.clone())
            .parse(value)
            .map_err(|error| invalid_project_rule(Some(Box::new(error))))?;
        let pattern = parsed
            .get("pattern")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if parse_permission_pattern(pattern).is_none() {
            return Err(invalid_project_rule(None));
        }
        let rule = serde_json::from_value::<PermissionRule>(parsed)
            .map_err(|error| invalid_project_rule(Some(Box::new(error))))?;
        rules.push(rule);
    }
    Ok(rules)
}

fn invalid_project_rule(cause: Option<Box<dyn Error + Send + Sync>>) -> WireAsyncError {
    match cause {
        Some(cause) => Box::new(Error2::with_options(
            CONFIG_INVALID,
            "Invalid project permission rule",
            Error2Options {
                cause: Some(cause),
                ..Default::default()
            },
        )),
        None => Box::new(Error2::new(
            CONFIG_INVALID,
            "Invalid project permission rule",
        )),
    }
}
