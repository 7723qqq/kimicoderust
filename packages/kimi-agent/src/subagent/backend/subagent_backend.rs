//! External subagent backend contract, ported from v2
//! `session/subagent/backend/subagentBackend.ts` and `subagentBackendService.ts`.
//!
//! A backend runs a fresh session in another CLI (`claude`, `codex`, an ACP
//! agent). v2 looks the backends up in a session-scoped DI service; the engine
//! has no scope, so the registry is an owned value the caller constructs and the
//! sandbox gate reads the engine's own `[sandbox]` config.

use std::collections::HashMap;
use std::sync::Arc;

use crate::rpc::types::BoxFuture;
use crate::tools::sandbox::{SandboxExecutionPolicy, SandboxMode};
use tokio_util::sync::CancellationToken;

use super::process::BackendProcess;

pub const SUBAGENT_BACKEND_NAMES: [&str; 3] = ["claude-code", "codex", "acp"];

/// v2 `SUBAGENT_BACKENDS_FLAG_ID` / `SUBAGENT_BACKENDS_FLAG_ENV`.
pub const SUBAGENT_BACKENDS_FLAG_ID: &str = "subagent_backends";
pub const SUBAGENT_BACKENDS_FLAG_ENV: &str = "KIMI_CODE_EXPERIMENTAL_SUBAGENT_BACKENDS";

/// v2 `BACKENDS_EXPERIMENTAL_UNAVAILABLE`, the refusal the Agent tool shows while
/// the flag is off.
pub const BACKENDS_EXPERIMENTAL_UNAVAILABLE: &str =
    "backend is disabled: the subagent_backends experimental flag is off.";

/// v2 `buildSubagentBackendDescriptions`.
pub fn build_subagent_backend_descriptions(backends: &[Arc<dyn SubagentBackend>]) -> String {
    let names = backends
        .iter()
        .map(|backend| backend.name())
        .collect::<Vec<_>>()
        .join(", ");
    format!("Available backends (pass via backend): {names}")
}

/// v2 `stripSubagentBackendParameter`: drop the `backend` property (and its
/// `required` entry) from a tool schema when the backends are unavailable.
pub fn strip_subagent_backend_parameter(
    parameters: &mut serde_json::Map<String, serde_json::Value>,
) {
    let Some(serde_json::Value::Object(properties)) = parameters.get_mut("properties") else {
        return;
    };
    if !properties.contains_key("backend") {
        return;
    }
    properties.remove("backend");
    if let Some(serde_json::Value::Array(required)) = parameters.get_mut("required") {
        required.retain(|entry| entry.as_str() != Some("backend"));
    }
}

/// v2 `SubagentBackendStopReason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentBackendStopReason {
    Completed,
    Aborted,
    Error,
    MaxTokens,
    Refusal,
}

impl SubagentBackendStopReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Aborted => "aborted",
            Self::Error => "error",
            Self::MaxTokens => "max-tokens",
            Self::Refusal => "refusal",
        }
    }
}

/// v2 `SubagentBackendStartRequest`. `cancel` replaces v2's `AbortSignal`.
pub struct SubagentBackendStartRequest {
    pub prompt: String,
    pub cwd: String,
    pub cancel: CancellationToken,
}

/// v2 `SubagentBackendResult`.
pub struct SubagentBackendResult {
    pub output: String,
    pub stop_reason: SubagentBackendStopReason,
}

/// v2 `SubagentBackendRun`: an id, the pending result, and a dispose that stops
/// the child.
pub struct SubagentBackendRun {
    pub id: String,
    pub result: tokio::sync::oneshot::Receiver<Result<SubagentBackendResult, String>>,
    pub cancel: CancellationToken,
    pub process: Arc<std::sync::Mutex<Option<BackendProcess>>>,
    pub wire: Option<Arc<super::codex_wire::JsonRpcWire>>,
}

impl SubagentBackendRun {
    /// v2 `dispose()`: abort the run, stop the transport and terminate the child.
    pub async fn dispose(&self) {
        self.cancel.cancel();
        if let Some(wire) = &self.wire {
            wire.dispose();
        }
        let process = self
            .process
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(mut process) = process {
            process.dispose().await;
        }
    }
}

pub trait SubagentBackend: Send + Sync {
    fn name(&self) -> &'static str;
    fn start<'a>(
        &'a self,
        request: SubagentBackendStartRequest,
    ) -> BoxFuture<'a, Result<SubagentBackendRun, String>>;
}

/// v2 `resolveSandboxPolicy`: the `[sandbox]` mode for the cwd, defaulting off.
pub fn resolve_sandbox_policy(
    config: &crate::config::KimiConfig,
    cwd: &str,
) -> SandboxExecutionPolicy {
    SandboxExecutionPolicy::new(config.resolve_sandbox_mode().unwrap_or_default(), cwd)
}

/// v2 `SubagentBackendService`.
pub struct SubagentBackendService {
    backends: HashMap<&'static str, Arc<dyn SubagentBackend>>,
    config: Arc<crate::config::KimiConfig>,
}

impl SubagentBackendService {
    pub fn new(config: Arc<crate::config::KimiConfig>) -> Self {
        Self {
            backends: HashMap::new(),
            config,
        }
    }

    /// v2 `register(backend)`.
    pub fn register(&mut self, backend: Arc<dyn SubagentBackend>) {
        self.backends.insert(backend.name(), backend);
    }

    /// v2 `getBackend(name)`.
    pub fn get_backend(&self, name: &str) -> Option<Arc<dyn SubagentBackend>> {
        self.backends.get(name).cloned()
    }

    /// v2 `list()`.
    pub fn list(&self) -> Vec<Arc<dyn SubagentBackend>> {
        self.backends.values().cloned().collect()
    }

    /// v2 `start(name, request)`: an unknown name is refused, and so is any
    /// non-off sandbox policy.
    pub async fn start(
        &self,
        name: &str,
        request: SubagentBackendStartRequest,
    ) -> Result<SubagentBackendRun, String> {        let backend = self.backends.get(name).ok_or_else(|| {
            format!(
                "Unknown subagent backend \"{name}\". Available backends: {}.",
                SUBAGENT_BACKEND_NAMES.join(", ")
            )
        })?;
        let policy = resolve_sandbox_policy(&self.config, &request.cwd);
        if policy.mode != SandboxMode::Off {
            return Err(format!(
                "the {} subagent backend is blocked while the sandbox mode is {}",
                name,
                policy.mode.as_str()
            ));
        }
        backend.start(request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DummyBackend;

    impl SubagentBackend for DummyBackend {
        fn name(&self) -> &'static str {
            "codex"
        }

        fn start<'a>(
            &'a self,
            _request: SubagentBackendStartRequest,
        ) -> BoxFuture<'a, Result<SubagentBackendRun, String>> {
            Box::pin(async { Err("not reached".to_string()) })
        }
    }

    fn request() -> SubagentBackendStartRequest {
        SubagentBackendStartRequest {
            prompt: "hi".to_string(),
            cwd: std::env::temp_dir().to_string_lossy().to_string(),
            cancel: CancellationToken::new(),
        }
    }

    fn service(mode: Option<&str>) -> SubagentBackendService {
        let mut config = crate::config::KimiConfig::default();
        config.sandbox.mode = mode.map(str::to_string);
        let mut service = SubagentBackendService::new(Arc::new(config));
        service.register(Arc::new(DummyBackend));
        service
    }

    #[tokio::test]
    async fn an_unknown_backend_lists_the_available_names() {
        let error = match service(None).start("nope", request()).await {
            Ok(_) => panic!("an unknown backend must be refused"),
            Err(error) => error,
        };
        assert!(error.contains("\"nope\""), "{error}");
        assert!(
            error.contains(&SUBAGENT_BACKEND_NAMES.join(", ")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_non_off_sandbox_blocks_the_backend() {
        let error = match service(Some("read-only")).start("codex", request()).await {
            Ok(_) => panic!("a sandboxed backend must be refused"),
            Err(error) => error,
        };
        assert!(error.contains("read-only"), "{error}");
        assert!(error.contains("codex"), "{error}");
    }

    #[tokio::test]
    async fn an_off_sandbox_reaches_the_backend() {
        assert!(service(None).get_backend("codex").is_some());
        assert_eq!(service(None).list().len(), 1);
        assert!(service(None).get_backend("acp").is_none());
    }

    #[test]
    fn the_backend_parameter_is_stripped_only_when_present() {
        let mut schema = serde_json::Map::new();
        schema.insert(
            "properties".to_string(),
            serde_json::json!({ "backend": { "type": "string" }, "prompt": { "type": "string" } }),
        );
        schema.insert("required".to_string(), serde_json::json!(["backend", "prompt"]));
        strip_subagent_backend_parameter(&mut schema);
        assert!(schema["properties"].get("backend").is_none());
        assert_eq!(schema["required"], serde_json::json!(["prompt"]));

        let mut bare = serde_json::Map::new();
        bare.insert(
            "properties".to_string(),
            serde_json::json!({ "prompt": { "type": "string" } }),
        );
        strip_subagent_backend_parameter(&mut bare);
        assert!(bare["properties"].get("backend").is_none());
    }

    #[test]
    fn the_descriptions_list_every_registered_backend() {
        let backends: Vec<Arc<dyn SubagentBackend>> = vec![Arc::new(DummyBackend)];
        assert_eq!(
            build_subagent_backend_descriptions(&backends),
            "Available backends (pass via backend): codex"
        );
    }
}
