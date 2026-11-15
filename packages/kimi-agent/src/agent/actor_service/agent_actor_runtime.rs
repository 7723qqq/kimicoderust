use std::sync::Arc;

use parking_lot::Mutex;

use crate::agent::state::errors::StateError;
use crate::agent::state::listeners::Listeners;
use crate::rpc::types::BoxFuture;

pub trait DurableAgentRuntimeParticipant: Send + Sync {
    fn id(&self) -> &str;

    fn events(&self) -> Vec<String>;

    fn undoable(&self) -> bool;
}

#[derive(Default, Clone)]
pub struct AgentActorRestore {
    pending: Arc<Mutex<Vec<BoxFuture<'static, ()>>>>,
}

impl AgentActorRestore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn wait_until(&self, work: BoxFuture<'static, ()>) {
        self.pending.lock().push(work);
    }

    pub async fn drain(&self) {
        loop {
            let batch = {
                let mut pending = self.pending.lock();
                if pending.is_empty() {
                    return;
                }
                std::mem::take(&mut *pending)
            };
            for work in batch {
                work.await;
            }
        }
    }
}

pub type ActorDispose = Arc<dyn Fn() + Send + Sync>;

pub trait AgentActorHost: Send + Sync {
    fn restore_phase_ready(&self) -> bool;

    fn attach(&self, participant: Arc<dyn DurableAgentRuntimeParticipant>) -> ActorDispose;

    fn attach_late(
        &self,
        participant: Arc<dyn DurableAgentRuntimeParticipant>,
    ) -> BoxFuture<'static, Result<ActorDispose, String>>;

    fn on_did_restore(
        &self,
        id: &str,
        hook: Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>,
    ) -> ActorDispose;
}

pub type ActorRestoreHook = Arc<dyn Fn(AgentActorRestore) -> BoxFuture<'static, ()> + Send + Sync>;

pub struct ActorAttachment {
    disposed: Arc<Mutex<bool>>,
    attachment: Arc<Mutex<Option<ActorDispose>>>,
    restore_hook: Arc<Mutex<Option<ActorDispose>>>,
}

struct AttachmentHandle {
    disposed: Arc<Mutex<bool>>,
    attachment: Arc<Mutex<Option<ActorDispose>>>,
    restore_hook: Arc<Mutex<Option<ActorDispose>>>,
}

impl AttachmentHandle {
    fn is_disposed(&self) -> bool {
        *self.disposed.lock()
    }

    fn set_attachment(&self, dispose: ActorDispose) {
        *self.attachment.lock() = Some(dispose);
    }

    fn dispose(&self) {
        let mut disposed = self.disposed.lock();
        if *disposed {
            return;
        }
        *disposed = true;
        drop(disposed);
        if let Some(dispose) = self.attachment.lock().take() {
            dispose();
        }
        if let Some(dispose) = self.restore_hook.lock().take() {
            dispose();
        }
    }
}

impl ActorAttachment {
    pub fn attach(
        id: &str,
        durable: Option<Arc<dyn DurableAgentRuntimeParticipant>>,
        host: Arc<dyn AgentActorHost>,
        restore: ActorRestoreHook,
        on_error: Arc<dyn Fn(String) + Send + Sync>,
    ) -> Self {
        let handle = Arc::new(AttachmentHandle {
            disposed: Arc::new(Mutex::new(false)),
            attachment: Arc::new(Mutex::new(None)),
            restore_hook: Arc::new(Mutex::new(None)),
        });

        if host.restore_phase_ready() {
            let task_handle = Arc::clone(&handle);
            let task_restore = Arc::clone(&restore);
            tokio::spawn(async move {
                tokio::task::yield_now().await;
                let outcome =
                    attach_late_and_restore(&task_handle, durable, Arc::clone(&host), task_restore)
                        .await;
                if let Err(error) = outcome {
                    on_error(error);
                }
            });
        } else {
            if let Some(participant) = durable {
                handle.set_attachment(host.attach(participant));
            }
            let hook_handle = Arc::clone(&handle);
            let hook_restore = Arc::clone(&restore);
            let hook = Arc::new(move || {
                let restore = Arc::clone(&hook_restore);
                let handle = Arc::clone(&hook_handle);
                Box::pin(async move {
                    if handle.is_disposed() {
                        return;
                    }
                    run_restore(restore).await;
                }) as BoxFuture<'static, ()>
            });
            *handle.restore_hook.lock() = Some(host.on_did_restore(id, hook));
        }

        Self {
            disposed: Arc::clone(&handle.disposed),
            attachment: Arc::clone(&handle.attachment),
            restore_hook: Arc::clone(&handle.restore_hook),
        }
    }

    pub fn dispose(&self) {
        let handle = AttachmentHandle {
            disposed: Arc::clone(&self.disposed),
            attachment: Arc::clone(&self.attachment),
            restore_hook: Arc::clone(&self.restore_hook),
        };
        handle.dispose();
    }

    pub fn is_disposed(&self) -> bool {
        *self.disposed.lock()
    }
}

impl Drop for ActorAttachment {
    fn drop(&mut self) {
        self.dispose();
    }
}

async fn attach_late_and_restore(
    handle: &AttachmentHandle,
    durable: Option<Arc<dyn DurableAgentRuntimeParticipant>>,
    host: Arc<dyn AgentActorHost>,
    restore: ActorRestoreHook,
) -> Result<(), String> {
    if let Some(participant) = durable {
        let late = host.attach_late(participant).await?;
        if handle.is_disposed() {
            late();
            return Ok(());
        }
        handle.set_attachment(late);
    }
    if handle.is_disposed() {
        return Ok(());
    }
    run_restore(restore).await;
    Ok(())
}

async fn run_restore(restore: ActorRestoreHook) {
    let event = AgentActorRestore::new();
    restore(event.clone());
    event.drain().await;
}

pub struct ActorStateEmitter<State: PartialEq + Clone + Send + Sync + 'static> {
    previous: Mutex<Option<State>>,
    listeners: Arc<Listeners<State>>,
}

impl<State: PartialEq + Clone + Send + Sync + 'static> Default for ActorStateEmitter<State> {
    fn default() -> Self {
        Self {
            previous: Mutex::new(None),
            listeners: Arc::new(Listeners::default()),
        }
    }
}

impl<State: PartialEq + Clone + Send + Sync + 'static> ActorStateEmitter<State> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn observe(&self, state: State) -> Result<(), StateError> {
        let previous = self.previous.lock().clone();
        let Some(previous) = previous else {
            *self.previous.lock() = Some(state);
            return Ok(());
        };
        if previous == state {
            return Ok(());
        }
        *self.previous.lock() = Some(state.clone());
        self.listeners.fire(&state)
    }

    pub fn on_did_change(&self) -> Arc<Listeners<State>> {
        Arc::clone(&self.listeners)
    }

    pub fn current(&self) -> Option<State> {
        self.previous.lock().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct Participant {
        id: String,
    }

    impl DurableAgentRuntimeParticipant for Participant {
        fn id(&self) -> &str {
            &self.id
        }

        fn events(&self) -> Vec<String> {
            vec!["todo.updated".to_string()]
        }

        fn undoable(&self) -> bool {
            true
        }
    }

    #[derive(Clone, Default)]
    struct HostProbe {
        ready: Arc<AtomicBool>,
        late_error: Arc<AtomicBool>,
        calls: Arc<StdMutex<Vec<String>>>,
        attach_disposals: Arc<AtomicUsize>,
        late_disposals: Arc<AtomicUsize>,
        hook_disposals: Arc<AtomicUsize>,
    }

    impl HostProbe {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn counter(&self, increment: Arc<AtomicUsize>) -> ActorDispose {
            Arc::new(move || {
                increment.fetch_add(1, Ordering::SeqCst);
            })
        }
    }

    impl AgentActorHost for HostProbe {
        fn restore_phase_ready(&self) -> bool {
            self.ready.load(Ordering::SeqCst)
        }

        fn attach(&self, participant: Arc<dyn DurableAgentRuntimeParticipant>) -> ActorDispose {
            self.calls
                .lock()
                .unwrap()
                .push(format!("attach:{}", participant.id()));
            self.counter(Arc::clone(&self.attach_disposals))
        }

        fn attach_late(
            &self,
            participant: Arc<dyn DurableAgentRuntimeParticipant>,
        ) -> BoxFuture<'static, Result<ActorDispose, String>> {
            let id = participant.id().to_string();
            self.calls.lock().unwrap().push(format!("attach-late:{id}"));
            if self.late_error.load(Ordering::SeqCst) {
                return Box::pin(async { Err("late attach failed".to_string()) });
            }
            let dispose = self.counter(Arc::clone(&self.late_disposals));
            Box::pin(async move { Ok(dispose) })
        }

        fn on_did_restore(
            &self,
            id: &str,
            _hook: Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>,
        ) -> ActorDispose {
            self.calls.lock().unwrap().push(format!("hook:{id}"));
            self.counter(Arc::clone(&self.hook_disposals))
        }
    }

    fn participant() -> Arc<dyn DurableAgentRuntimeParticipant> {
        Arc::new(Participant {
            id: "todo".to_string(),
        })
    }

    fn restore_hook(calls: Arc<StdMutex<Vec<String>>>) -> ActorRestoreHook {
        Arc::new(move |event: AgentActorRestore| {
            calls.lock().unwrap().push("restore".to_string());
            Box::pin(async move {
                event.drain().await;
            }) as BoxFuture<'static, ()>
        })
    }

    #[tokio::test]
    async fn non_ready_hosts_attach_immediately_and_register_the_restore_hook() {
        let host = HostProbe::default();
        let restores = Arc::new(StdMutex::new(Vec::new()));
        let attachment = ActorAttachment::attach(
            "todo",
            Some(participant()),
            Arc::new(host.clone()),
            restore_hook(Arc::clone(&restores)),
            Arc::new(|_| {}),
        );
        assert_eq!(host.calls(), vec!["attach:todo", "hook:todo"]);
        assert!(!attachment.is_disposed());
        assert!(restores.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn dispose_is_idempotent_and_releases_the_attachment_and_hook() {
        let host = HostProbe::default();
        let attachment = ActorAttachment::attach(
            "todo",
            Some(participant()),
            Arc::new(host.clone()),
            restore_hook(Arc::new(StdMutex::new(Vec::new()))),
            Arc::new(|_| {}),
        );
        attachment.dispose();
        attachment.dispose();
        assert!(attachment.is_disposed());
        assert_eq!(host.attach_disposals.load(Ordering::SeqCst), 1);
        assert_eq!(host.hook_disposals.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn ready_hosts_attach_late_after_yielding_and_then_restore() {
        let host = HostProbe::default();
        host.ready.store(true, Ordering::SeqCst);
        let restores = Arc::new(StdMutex::new(Vec::new()));
        let attachment = ActorAttachment::attach(
            "todo",
            Some(participant()),
            Arc::new(host.clone()),
            restore_hook(Arc::clone(&restores)),
            Arc::new(|_| {}),
        );
        assert!(host.calls().is_empty());
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if restores.lock().unwrap().len() == 1 {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("late attach then restore must run");
        assert_eq!(host.calls(), vec!["attach-late:todo"]);
        assert!(!attachment.is_disposed());
    }

    #[tokio::test]
    async fn disposing_before_late_attach_finishes_releases_the_late_attachment() {
        let host = HostProbe::default();
        host.ready.store(true, Ordering::SeqCst);
        let restores = Arc::new(StdMutex::new(Vec::new()));
        let attachment = ActorAttachment::attach(
            "todo",
            Some(participant()),
            Arc::new(host.clone()),
            restore_hook(Arc::clone(&restores)),
            Arc::new(|_| {}),
        );
        attachment.dispose();
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(host.late_disposals.load(Ordering::SeqCst), 1);
        assert!(restores.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn late_attach_errors_are_reported_without_restoring() {
        let host = HostProbe::default();
        host.ready.store(true, Ordering::SeqCst);
        host.late_error.store(true, Ordering::SeqCst);
        let restores = Arc::new(StdMutex::new(Vec::new()));
        let error = Arc::new(StdMutex::new(None));
        let _attachment = ActorAttachment::attach(
            "todo",
            Some(participant()),
            Arc::new(host.clone()),
            restore_hook(Arc::clone(&restores)),
            {
                let error = Arc::clone(&error);
                Arc::new(move |message| *error.lock().unwrap() = Some(message))
            },
        );
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if error.lock().unwrap().is_some() {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the failure must be reported");
        assert_eq!(error.lock().unwrap().as_deref(), Some("late attach failed"));
        assert!(restores.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn restore_waits_for_every_registered_readiness_future() {
        let restore = AgentActorRestore::new();
        let done = Arc::new(AtomicBool::new(false));
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        {
            let done = Arc::clone(&done);
            restore.wait_until(Box::pin(async move {
                let _ = rx.await;
                done.store(true, Ordering::SeqCst);
            }));
        }
        let waiter = {
            let restore = restore.clone();
            tokio::spawn(async move { restore.drain().await })
        };
        tokio::task::yield_now().await;
        assert!(!done.load(Ordering::SeqCst));
        let _ = tx.send(());
        waiter.await.unwrap();
        assert!(done.load(Ordering::SeqCst));
    }

    #[test]
    fn state_emitter_skips_the_first_snapshot_and_equal_values() {
        let emitter = ActorStateEmitter::new();
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let _registration = emitter.on_did_change().add({
            let seen = Arc::clone(&seen);
            move |state| {
                seen.lock().unwrap().push(*state);
                Ok(())
            }
        });
        emitter.observe(1).unwrap();
        assert!(seen.lock().unwrap().is_empty());
        emitter.observe(1).unwrap();
        assert!(seen.lock().unwrap().is_empty());
        emitter.observe(2).unwrap();
        assert_eq!(*seen.lock().unwrap(), vec![2]);
        assert_eq!(emitter.current(), Some(2));
    }
}
