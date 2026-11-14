use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use futures_util::future::BoxFuture;
use parking_lot::Mutex;
use serde_json::Value;

use crate::_base::event::{Event, Listener, Subscription};
use super::tool_executor::ExecutableToolResult;
use crate::wire::record::WireAsyncError;

use super::tool_hooks::{BeforeExecuteDecision, ResolvedToolExecutionHookContext};

pub type PendingVetoFactory = Arc<
    dyn Fn() -> BoxFuture<'static, Result<Option<BeforeExecuteDecision>, WireAsyncError>>
        + Send
        + Sync,
>;

pub fn deny_tool_execution(reason: &str) -> ExecutableToolResult {
    ExecutableToolResult {
        output: reason.to_string().into(),
        is_error: Some(true),
        ..Default::default()
    }
}

#[derive(Default)]
struct BeforeExecuteState {
    veto_result: Option<ExecutableToolResult>,
    final_allowed: bool,
    pass_metadata: Option<Value>,
    pending_vetos: Vec<PendingVetoFactory>,
    open: bool,
}

pub struct BeforeToolExecuteEventImpl {
    pub context: ResolvedToolExecutionHookContext,
    state: Mutex<BeforeExecuteState>,
}

impl BeforeToolExecuteEventImpl {
    pub fn new(context: ResolvedToolExecutionHookContext) -> Self {
        Self {
            context,
            state: Mutex::new(BeforeExecuteState {
                open: true,
                ..Default::default()
            }),
        }
    }

    pub fn veto(&self, result: ExecutableToolResult) {
        let mut state = self.state.lock();
        assert_open(state.open, "veto");
        if state.veto_result.is_none() {
            state.veto_result = Some(result);
        }
    }

    pub fn allow(&self) {
        let state = self.state.lock();
        assert_open(state.open, "allow");
        drop(state);
        self.state.lock().final_allowed = true;
    }

    pub fn pass(&self, metadata: Option<Value>) {
        let mut state = self.state.lock();
        assert_open(state.open, "pass");
        if matches!(state.pass_metadata.as_ref(), None | Some(Value::Null)) {
            state.pass_metadata = metadata;
        }
    }

    pub fn wait_until(&self, factory: PendingVetoFactory) {
        let mut state = self.state.lock();
        assert_open(state.open, "waitUntil");
        state.pending_vetos.push(factory);
    }

    pub fn veto_result(&self) -> Option<ExecutableToolResult> {
        self.state.lock().veto_result.clone()
    }

    pub fn final_allowed(&self) -> bool {
        self.state.lock().final_allowed
    }

    pub fn pass_metadata(&self) -> Option<Value> {
        self.state.lock().pass_metadata.clone()
    }

    pub fn pending_vetos(&self) -> Vec<PendingVetoFactory> {
        self.state.lock().pending_vetos.clone()
    }

    pub fn close_registration(&self) {
        self.state.lock().open = false;
    }
}

fn assert_open(open: bool, statement: &str) {
    assert!(open, "{statement} can NOT be called asynchronously");
}

struct BeforeToolExecuteEmitterInner {
    listeners: Mutex<BTreeMap<u64, Listener<Arc<BeforeToolExecuteEventImpl>>>>,
    next_listener: AtomicU64,
    disposed: AtomicBool,
}

#[derive(Clone)]
pub struct BeforeToolExecuteEmitter {
    inner: Arc<BeforeToolExecuteEmitterInner>,
}

impl BeforeToolExecuteEmitter {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(BeforeToolExecuteEmitterInner {
                listeners: Mutex::new(BTreeMap::new()),
                next_listener: AtomicU64::new(1),
                disposed: AtomicBool::new(false),
            }),
        }
    }

    pub fn event(&self) -> Event<Arc<BeforeToolExecuteEventImpl>> {
        let inner = self.inner.clone();
        Event::new(move |listener| {
            if inner.disposed.load(Ordering::SeqCst) {
                return Subscription::none();
            }
            let id = inner.next_listener.fetch_add(1, Ordering::Relaxed);
            inner.listeners.lock().insert(id, listener);
            let inner = inner.clone();
            Subscription::new(None, move || {
                if !inner.disposed.load(Ordering::SeqCst) {
                    inner.listeners.lock().remove(&id);
                }
            })
        })
    }

    pub fn listener_count(&self) -> usize {
        self.inner.listeners.lock().len()
    }

    pub fn dispose(&self) {
        if self.inner.disposed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.inner.listeners.lock().clear();
    }

    pub fn is_disposed(&self) -> bool {
        self.inner.disposed.load(Ordering::SeqCst)
    }

    pub fn fire_before_execute(
        &self,
        context: ResolvedToolExecutionHookContext,
    ) -> BoxFuture<'static, Result<Option<BeforeExecuteDecision>, WireAsyncError>> {
        if self.is_disposed() || self.listener_count() == 0 {
            return Box::pin(async { Ok(None) });
        }
        let listeners = self
            .inner
            .listeners
            .lock()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        Box::pin(async move {
            let event = Arc::new(BeforeToolExecuteEventImpl::new(context));
            for listener in listeners {
                listener(event.clone());
                if event.final_allowed() {
                    return Ok(None);
                }
                if let Some(veto) = event.veto_result() {
                    return Ok(Some(BeforeExecuteDecision {
                        veto: Some(veto),
                        execution_metadata: None,
                    }));
                }
            }
            event.close_registration();
            let mut pass_metadata = event.pass_metadata();
            for factory in event.pending_vetos() {
                let decision = factory().await?;
                let execution_metadata = match decision {
                    Some(decision) => {
                        if decision.veto.is_some() {
                            return Ok(Some(BeforeExecuteDecision {
                                veto: decision.veto,
                                execution_metadata: None,
                            }));
                        }
                        decision.execution_metadata
                    }
                    None => None,
                };
                if matches!(pass_metadata.as_ref(), None | Some(Value::Null)) {
                    pass_metadata = execution_metadata;
                }
            }
            Ok(pass_metadata.map(|metadata| BeforeExecuteDecision {
                veto: None,
                execution_metadata: Some(metadata),
            }))
        })
    }
}

impl Default for BeforeToolExecuteEmitter {
    fn default() -> Self {
        Self::new()
    }
}
