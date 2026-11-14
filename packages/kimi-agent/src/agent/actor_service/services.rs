use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::oneshot;

use crate::agent::agent_context::AgentContext;
use crate::rpc::types::BoxFuture;

pub type Dispose = Box<dyn FnOnce() + Send>;
pub type EventHandler = Arc<dyn Fn(Value) + Send + Sync>;
pub type BeforeToolExecuteEvent = Arc<Mutex<Value>>;
pub type BeforeToolExecuteHandler =
    Arc<dyn Fn(BeforeToolExecuteEvent) -> BoxFuture<'static, Result<(), String>> + Send + Sync>;
pub type NextHook = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;

#[derive(Clone, Default)]
pub struct HookContext {
    pub value: Value,
    pub signal: Option<tokio_util::sync::CancellationToken>,
}

pub type AsyncHook = Arc<dyn Fn(HookContext, NextHook) -> BoxFuture<'static, ()> + Send + Sync>;

pub trait HookRegistry: Send + Sync {
    fn register(&self, id: &str, hook: AsyncHook) -> Dispose;
}

#[derive(Clone)]
pub struct LoopHooks {
    pub on_will_begin_step: Arc<dyn HookRegistry>,
    pub on_did_finish_step: Arc<dyn HookRegistry>,
}

#[derive(Clone)]
pub struct ToolExecutorHooks {
    pub on_did_execute_tool: Arc<dyn HookRegistry>,
}

pub struct PromptHandle {
    pub launched: oneshot::Receiver<Value>,
    pub completion: oneshot::Receiver<Value>,
}

pub trait AgentLoopService: Send + Sync {
    fn submit(&self, request: Value, options: Option<Value>) -> Result<String, String>;

    fn prompt_handle(&self, id: &str) -> Option<PromptHandle>;

    fn cancel(&self, selector: Value, cancellation: Value);

    fn notify(&self);

    fn snapshot(&self) -> Value;

    fn hooks(&self) -> LoopHooks;

    fn try_acquire_quiescence(&self) -> Option<Dispose>;
}

pub trait TelemetryService: Send + Sync {
    fn track2(&self, event: &str, properties: Value);
}

pub trait ConfigService: Send + Sync {
    fn ready(&self) -> BoxFuture<'static, Result<(), String>>;

    fn get(&self, domain: &str) -> Option<Value>;
}

pub trait FlagService: Send + Sync {
    fn enabled(&self, id: &str) -> bool;
}

pub trait LogService: Send + Sync {
    fn error(&self, message: &str, payload: Value);
}

pub trait EventBusService: Send + Sync {
    fn subscribe(&self, event_type: &str, handler: EventHandler) -> Dispose;
}

pub trait AgentLifecycleService: Send + Sync {
    fn on_will_close(&self, handler: Arc<dyn Fn(AgentContext) + Send + Sync>) -> Dispose;
}

pub trait SessionUsageService: Send + Sync {
    fn on_did_record(&self, handler: EventHandler) -> Dispose;
}

pub trait ToolExecutorService: Send + Sync {
    fn on_before_execute_tool(&self, handler: BeforeToolExecuteHandler) -> Dispose;

    fn hooks(&self) -> ToolExecutorHooks;
}

pub type ResolveApproval = Arc<dyn Fn(Value) -> BoxFuture<'static, Option<Value>> + Send + Sync>;

pub struct ToolApprovalRequest {
    pub result: Value,
    pub resolve_approval: Option<ResolveApproval>,
}

pub trait ToolApprovalService: Send + Sync {
    fn request_tool_approval(
        &self,
        event: BeforeToolExecuteEvent,
        request: ToolApprovalRequest,
        origin: &str,
    ) -> BoxFuture<'static, Result<Option<Value>, String>>;
}

pub trait BootstrapService: Send + Sync {
    fn get_env(&self, name: &str) -> Option<String>;

    fn ui_capabilities(&self) -> Vec<String>;
}

pub trait SessionContextService: Send + Sync {
    fn cwd(&self) -> String;
}

pub trait EnvironmentBindingService: Send + Sync {
    fn current_cwd(&self) -> Option<String>;
}
