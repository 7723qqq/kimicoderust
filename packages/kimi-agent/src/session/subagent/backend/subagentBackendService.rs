use std::sync::Arc;

use async_trait::async_trait;
use indexmap::IndexMap;

use crate::_base::di::scope::{ScopeActivation, register_scoped_service};
use crate::app::config::config::IConfigService;
use crate::app::scopes::LifecycleScope;
use crate::i18n::{LocalizedText, i18n_params};
use crate::session::process::process_runner::ISessionProcessRunner;
use crate::wire::record::WireAsyncError;
use crate::workspace::sandbox::sandbox::{SandboxMode, resolve_sandbox_policy};

use super::acp_backend::AcpBackend;
use super::claude_code_backend::ClaudeCodeBackend;
use super::codex_backend::CodexBackend;
use super::subagent_backend::{
    ISubagentBackend, ISubagentBackendService, SUBAGENT_BACKEND_NAMES, SubagentBackendName,
    SubagentBackendRun, SubagentBackendStartRequest,
};

pub struct SubagentBackendService {
    config_service: Arc<dyn IConfigService>,
    backends: IndexMap<SubagentBackendName, Arc<dyn ISubagentBackend>>,
}

impl SubagentBackendService {
    pub fn new(
        process_runner: Arc<dyn ISessionProcessRunner>,
        config_service: Arc<dyn IConfigService>,
    ) -> Self {
        let mut this = Self {
            config_service: config_service.clone(),
            backends: IndexMap::new(),
        };
        this.register(Arc::new(ClaudeCodeBackend::new(config_service.clone())));
        this.register(Arc::new(CodexBackend::new(
            process_runner.clone(),
            config_service.clone(),
        )));
        this.register(Arc::new(AcpBackend::new(process_runner, config_service)));
        this
    }

    pub fn get_backend(&self, name: SubagentBackendName) -> Option<Arc<dyn ISubagentBackend>> {
        self.backends.get(&name).cloned()
    }

    pub fn list(&self) -> Vec<Arc<dyn ISubagentBackend>> {
        self.backends.values().cloned().collect()
    }

    pub async fn start(
        &self,
        name: SubagentBackendName,
        request: SubagentBackendStartRequest,
    ) -> Result<SubagentBackendRun, WireAsyncError> {
        let Some(backend) = self.backends.get(&name) else {
            return Err(format!(
                "Unknown subagent backend \"{}\". Available backends: {}.",
                name.as_str(),
                SUBAGENT_BACKEND_NAMES.join(", ")
            )
            .into());
        };
        let policy = resolve_sandbox_policy(self.config_service.as_ref(), &request.cwd);
        if policy.mode != SandboxMode::Off {
            return Err(LocalizedText::with_params(
                "toolsV2.sandbox.externalSubagentBackendBlocked",
                i18n_params!["mode" => policy.mode.as_str(), "backend" => name.as_str()],
            )
            .render()
            .into());
        }
        backend.start(request).await
    }

    fn register(&mut self, backend: Arc<dyn ISubagentBackend>) {
        self.backends.insert(backend.name(), backend);
    }
}

#[async_trait]
impl ISubagentBackendService for SubagentBackendService {
    fn get_backend(&self, name: SubagentBackendName) -> Option<Arc<dyn ISubagentBackend>> {
        SubagentBackendService::get_backend(self, name)
    }

    fn list(&self) -> Vec<Arc<dyn ISubagentBackend>> {
        SubagentBackendService::list(self)
    }

    async fn start(
        &self,
        name: SubagentBackendName,
        request: SubagentBackendStartRequest,
    ) -> Result<SubagentBackendRun, WireAsyncError> {
        SubagentBackendService::start(self, name, request).await
    }
}

pub fn register_subagent_backend_service() {
    register_scoped_service(
        LifecycleScope::Session,
        ScopeActivation::OnScopeCreated,
        "subagent",
    );
}
