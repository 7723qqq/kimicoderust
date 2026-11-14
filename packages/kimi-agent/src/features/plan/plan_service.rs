use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use kimi_core::primitives::errors::Error2;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::plan::{IAgentPlanService, PlanData, PlanFileOperation, PlanFileTarget};
use super::planOps::{
    PlanModeCancel, PlanModeEnter, PlanModeExit, PlanRevision, PlanState, plan_key,
};
use super::planToTodoConverter::{PlanToTodoOutcome, try_convert_plan_to_todos};
use crate::_base::di::lifecycle::DisposableRef;
use crate::_base::utils::hero_slug::generate_hero_slug;
use crate::agent::scope_context::scope_context::IAgentScopeContext;
use crate::agent::state::agent_state_v2::IAgentStateService;
use crate::app::telemetry::telemetry::ITelemetryService;
use crate::callbacks::MAIN_AGENT_ID;
use crate::environment::environment::EnvironmentPath;
use crate::features::todo::todo_service::AgentTodoService;
use crate::os::interface::host_environment::IHostEnvironment;
use crate::os::interface::host_file_system::IHostFileSystem;
use crate::persistence::interface::blob_store::IBlobStore;
use crate::primitives::read::ReadOutcome;
use crate::session::errors::SESSION_PLAN_MODE_INVALID;
use crate::state::event_dispatcher::IEventDispatcher;
use crate::storage::state_store::{StateStore, StateWriteOutcome};

#[derive(Clone)]
pub struct AgentPlanService {
    session_dir: PathBuf,
    agent_id: String,
    host_environment: Arc<dyn IHostEnvironment>,
    host_fs: Arc<dyn IHostFileSystem>,
    blobs: Arc<dyn IBlobStore>,
    dispatcher: Arc<dyn IEventDispatcher>,
    agent_ctx: Arc<dyn IAgentScopeContext>,
    telemetry: Arc<dyn ITelemetryService>,
    agent_state: Arc<dyn IAgentStateService>,
    todo: Arc<AgentTodoService>,
    _plan_state_registration: DisposableRef,
}

impl AgentPlanService {
    pub fn new(
        session_dir: impl Into<PathBuf>,
        agent_id: impl Into<String>,
        host_environment: Arc<dyn IHostEnvironment>,
        host_fs: Arc<dyn IHostFileSystem>,
        blobs: Arc<dyn IBlobStore>,
        dispatcher: Arc<dyn IEventDispatcher>,
        agent_ctx: Arc<dyn IAgentScopeContext>,
        telemetry: Arc<dyn ITelemetryService>,
        agent_state: Arc<dyn IAgentStateService>,
        todo: Arc<AgentTodoService>,
    ) -> Self {
        let plan_state_registration = agent_state
            .contribute_state(plan_key().state_key())
            .unwrap_or_else(|error| panic!("{error}"));
        Self {
            session_dir: session_dir.into(),
            agent_id: agent_id.into(),
            host_environment,
            host_fs,
            blobs,
            dispatcher,
            agent_ctx,
            telemetry,
            agent_state,
            todo,
            _plan_state_registration: plan_state_registration,
        }
    }

    pub fn plan_file_target(&self, id: &str) -> PlanFileTarget {
        let path_class = self.host_environment.path_class();
        PlanFileTarget {
            fs: self.host_fs.clone(),
            environment_path: EnvironmentPath::new(&path_class),
            path_class,
            path: normalize_path(
                &self
                    .session_dir
                    .join("agents")
                    .join(&self.agent_id)
                    .join("plans")
                    .join(format!("{id}.md")),
            ),
        }
    }

    pub async fn clear(&self, state: &PlanState) -> Result<(), String> {
        if !state.active {
            return Ok(());
        }
        let Some(id) = state.id.as_deref() else {
            return Ok(());
        };
        let target = self.plan_file_target(id);
        if let Some(parent) = target.path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| error.to_string())?;
        }
        tokio::fs::write(target.path, "")
            .await
            .map_err(|error| error.to_string())
    }

    pub async fn status(&self, state: &PlanState) -> Result<Option<PlanData>, String> {
        if !state.active {
            return Ok(None);
        }
        let Some(id) = state.id.as_deref() else {
            return Ok(None);
        };
        let target = self.plan_file_target(id);
        let content = match tokio::fs::read(&target.path).await {
            Ok(content) => String::from_utf8_lossy(&content).into_owned(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error.to_string()),
        };
        Ok(Some(PlanData {
            id: id.to_string(),
            content,
            path: Some(target.path.to_string_lossy().into_owned()),
        }))
    }

    pub async fn build_revision(&self, state: &PlanState) -> Result<Option<PlanRevision>, String> {
        if !state.active {
            return Ok(None);
        }
        let Some(id) = state.id.as_deref() else {
            return Ok(None);
        };
        let target = self.plan_file_target(id);
        let bytes = tokio::fs::read(&target.path)
            .await
            .map(|content| String::from_utf8_lossy(&content).into_owned().into_bytes())
            .map_err(|error| error.to_string())?;
        let version = state
            .revision_count
            .as_ref()
            .and_then(|counts| counts.get(id))
            .copied()
            .unwrap_or(0)
            + 1;
        Ok(Some(PlanRevision {
            agent_id: self.agent_id.clone(),
            id: id.to_string(),
            version,
            key: format!("plan/{id}/v{version}.md"),
            sha256: sha256(&bytes),
            bytes: bytes.len(),
        }))
    }

    pub fn resolve_file(
        &self,
        path: &str,
        operation: PlanFileOperation,
        state: &PlanState,
    ) -> Option<PlanFileTarget> {
        if path.contains('\0') {
            return None;
        }
        let path = Path::new(path);
        if state.active
            && let Some(id) = state.id.as_deref()
        {
            let target = self.plan_file_target(id);
            if path.is_absolute() && paths_equal(path, &target.path) {
                return Some(target);
            }
        }
        if operation != PlanFileOperation::Read || !path.is_absolute() {
            return None;
        }
        let plans_dir = self.plan_file_target("").path;
        let plans_dir = plans_dir.parent()?;
        let normalized = normalize_path(path);
        let name = normalized.strip_prefix(plans_dir).ok()?;
        let name = name.to_string_lossy();
        if !valid_plan_file_name(&name) {
            return None;
        }
        Some(self.plan_file_target(&name[..name.len() - 3]))
    }
}

impl AgentPlanService {
    fn plan_state(&self) -> Result<PlanState, String> {
        serde_json::from_value(self.agent_state.get(&plan_key().state_key()))
            .map_err(|error| error.to_string())
    }

    fn set_telemetry_mode(&self, mode: &str) {
        self.telemetry.set_context(serde_json::Map::from_iter([(
            "mode".to_string(),
            Value::String(mode.to_string()),
        )]));
    }

    fn create_plan_id() -> String {
        generate_hero_slug(&uuid::Uuid::new_v4().to_string(), &HashSet::new())
    }

    async fn ensure_plan_directory(&self, target: &PlanFileTarget) -> Result<(), String> {
        let path = target.environment_path.dirname(&target.path.to_string_lossy());
        target
            .fs
            .mkdir(&path, true)
            .await
            .map_err(|error| error.to_string())
    }

    async fn write_empty_plan_file(&self, target: &PlanFileTarget) -> Result<(), String> {
        self.ensure_plan_directory(target).await?;
        target
            .fs
            .write_text(&target.path.to_string_lossy(), "")
            .await
            .map_err(|error| error.to_string())
    }

    async fn record_revision_for_state(&self, state: &PlanState) -> Result<(), String> {
        if !state.active {
            return Ok(());
        }
        let Some(id) = state.id.as_deref() else {
            return Ok(());
        };
        let target = self.plan_file_target(id);
        let content = target
            .fs
            .read_text(&target.path.to_string_lossy(), None)
            .await
            .map_err(|error| error.to_string())?;
        let bytes = content.into_bytes();
        let version = state
            .revision_count
            .as_ref()
            .and_then(|counts| counts.get(id))
            .copied()
            .unwrap_or(0)
            + 1;
        let scope = self.agent_ctx.scope(None);
        let key = format!("plan/{id}/v{version}.md");
        let sha256 = sha256(&bytes);
        let byte_count = bytes.len();
        self.blobs
            .put(&scope, &key, bytes)
            .await
            .map_err(|error| error.to_string())?;
        self.dispatcher
            .dispatch(
                PlanRevision {
                    agent_id: self.agent_ctx.agent_id(),
                    id: id.to_string(),
                    version,
                    key,
                    sha256,
                    bytes: byte_count,
                }
                .to_event2(),
            )
            .await
            .map_err(|error| error.to_string())
    }

    async fn maybe_seed_todo_from_plan(&self) -> Result<(), String> {
        let state = self.plan_state()?;
        let plan_data = self.status(&state).await?;
        if plan_data.is_none() {
            return Ok(());
        }
        let current_todos = self.todo.get();
        let conversion = try_convert_plan_to_todos(plan_data.as_ref(), &current_todos, true);
        let PlanToTodoOutcome::Converted { count } = conversion.outcome else {
            return Ok(());
        };
        let Some(items) = conversion.items else {
            return Ok(());
        };
        self.todo.replace(items).await?;
        self.telemetry.track2(
            "plan_to_todo_converted",
            Some(serde_json::Map::from_iter([(
                "item_count".to_string(),
                Value::from(count),
            )])),
        );
        Ok(())
    }
}

#[async_trait]
impl IAgentPlanService for AgentPlanService {
    async fn enter(&self, id: Option<String>, create_file: bool) -> Result<(), String> {
        if self.agent_ctx.agent_id() != MAIN_AGENT_ID {
            return Err(Error2::new(
                SESSION_PLAN_MODE_INVALID,
                "Plan mode is only available to the main agent",
            )
            .to_string());
        }
        if self.plan_state()?.active {
            return Err(Error2::new(SESSION_PLAN_MODE_INVALID, "Already in plan mode").to_string());
        }
        let id = id.unwrap_or_else(Self::create_plan_id);
        let target = self.plan_file_target(&id);
        let mut enter_recorded = false;
        let result = async {
            self.ensure_plan_directory(&target).await?;
            self.dispatcher
                .dispatch(
                    PlanModeEnter {
                        agent_id: self.agent_ctx.agent_id(),
                        id: id.clone(),
                    }
                    .to_event2(),
                )
                .await
                .map_err(|error| error.to_string())?;
            self.set_telemetry_mode("plan");
            enter_recorded = true;
            if create_file {
                self.write_empty_plan_file(&target).await?;
            }
            Ok(())
        }
        .await;
        if result.is_err() && enter_recorded {
            self.cancel(Some(id));
        }
        result
    }

    fn cancel(&self, id: Option<String>) {
        let dispatcher = self.dispatcher.clone();
        let event = PlanModeCancel {
            agent_id: self.agent_ctx.agent_id(),
            id,
        }
        .to_event2();
        tokio::spawn(async move {
            let _ = dispatcher.dispatch(event).await;
        });
        self.set_telemetry_mode("agent");
    }

    async fn clear(&self) -> Result<(), String> {
        let state = self.plan_state()?;
        AgentPlanService::clear(self, &state).await
    }

    fn exit(&self, id: Option<String>) {
        let service = self.clone();
        let dispatcher = self.dispatcher.clone();
        let event = PlanModeExit {
            agent_id: self.agent_ctx.agent_id(),
            id,
        }
        .to_event2();
        tokio::spawn(async move {
            let _ = futures_util::future::join(
                service.maybe_seed_todo_from_plan(),
                dispatcher.dispatch(event),
            )
            .await;
        });
        self.set_telemetry_mode("agent");
    }

    async fn record_revision(&self) -> Result<(), String> {
        let state = self.plan_state()?;
        self.record_revision_for_state(&state).await
    }

    async fn resolve_file(
        &self,
        path: &str,
        operation: Option<PlanFileOperation>,
    ) -> Option<PlanFileTarget> {
        let state = self.plan_state().ok()?;
        AgentPlanService::resolve_file(
            self,
            path,
            operation.unwrap_or(PlanFileOperation::Write),
            &state,
        )
    }

    async fn status(&self) -> Result<Option<PlanData>, String> {
        let state = self.plan_state()?;
        AgentPlanService::status(self, &state).await
    }
}

fn paths_equal(left: &Path, right: &Path) -> bool {
    let normalize = |path: &Path| {
        let value = normalize_path(path).to_string_lossy().replace('\\', "/");
        if cfg!(windows) {
            value.to_lowercase()
        } else {
            value
        }
    };
    if normalize(left) == normalize(right) {
        return true;
    }
    normalize(&canonicalize_existing_prefix(left)) == normalize(&canonicalize_existing_prefix(right))
}

fn normalize_path(path: &Path) -> PathBuf {
    let absolute = path.is_absolute();
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if output
                    .components()
                    .next_back()
                    .is_some_and(|component| matches!(component, std::path::Component::Normal(_)))
                {
                    output.pop();
                } else if !absolute {
                    output.push("..");
                }
            }
            other => output.push(other.as_os_str()),
        }
    }
    if output.as_os_str().is_empty() && !absolute {
        output.push(".");
    }
    output
}

fn canonicalize_existing_prefix(path: &Path) -> PathBuf {
    let mut current = path;
    let mut suffix = Vec::new();
    loop {
        match fs::canonicalize(current) {
            Ok(real) => {
                let mut output = real;
                for component in suffix.iter().rev() {
                    output.push(component);
                }
                return output;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return path.to_path_buf(),
        }
        let Some(name) = current.file_name() else {
            return path.to_path_buf();
        };
        suffix.push(name.to_os_string());
        let Some(parent) = current.parent() else {
            return path.to_path_buf();
        };
        current = parent;
    }
}

fn valid_plan_file_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.len() < 3 || !bytes[bytes.len() - 3..].eq_ignore_ascii_case(b".md") {
        return false;
    }
    let stem = &name[..name.len() - 3];
    !stem.is_empty()
        && stem
            .split('-')
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_alphanumeric()))
}

fn sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn plan_mode_write_denied_message(plan_file_path: Option<&str>) -> String {
    let target = plan_file_path
        .filter(|path| !path.is_empty())
        .unwrap_or("(no plan file selected yet)");
    format!(
        "Plan mode is active. You may only write to the current plan file: {target}. Call ExitPlanMode to exit plan mode before editing other files."
    )
}

impl StateStore {
    pub(crate) fn apply_plan_write(&self, value: &Value) -> Result<StateWriteOutcome, String> {
        let Some(active) = value.get("active").and_then(|v| v.as_bool()) else {
            return Err(
                "State write error: [-32003] invalid plan state value: expected { active: boolean }"
                    .into(),
            );
        };
        if !active {
            let stored = json!({ "active": false });
            return Ok(StateWriteOutcome {
                stored: stored.clone(),
                response: stored,
            });
        }
        let already_active = match self.read_domain_state("plan") {
            ReadOutcome::Found(plan) => plan.get("active").and_then(|v| v.as_bool()) == Some(true),
            ReadOutcome::Absent => false,
            ReadOutcome::Unreadable(cause) => {
                return Err(format!(
                    "State write error: plan state is unreadable ({cause}); \
                     entering plan mode would overwrite it"
                ));
            }
        };
        if already_active {
            return Err("State write error: [-32004] plan mode is already active".into());
        }
        let id = format!("plan-{:016x}", fastrand::u64(..));
        let plans_dir = self
            .state_dir
            .parent()
            .unwrap_or(&self.state_dir)
            .join("plans");
        fs::create_dir_all(&plans_dir)
            .map_err(|e| format!("create plan directory {}: {e}", plans_dir.display()))?;
        let path = plans_dir.join(format!("{id}.md"));
        fs::write(&path, "").map_err(|e| format!("create plan file {}: {e}", path.display()))?;
        let stored = json!({
            "active": true,
            "id": id,
            "path": path.to_string_lossy(),
        });
        Ok(StateWriteOutcome {
            stored: stored.clone(),
            response: stored,
        })
    }
}
