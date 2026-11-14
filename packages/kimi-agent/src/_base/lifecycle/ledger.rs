use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use futures_util::future::{BoxFuture, FutureExt, Shared};
use futures_util::StreamExt;
use parking_lot::Mutex;

use super::disposer::{
    Disposer, DisposerOutput, EffectBody, EffectResult, IntoDisposerOutput, TeardownReason,
};
use super::errors::LedgerDisposedError;
use crate::_base::errors::unexpected_error::on_unexpected_error;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LedgerState {
    Active,
    Disposing,
    Disposed,
}

impl LedgerState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Disposing => "disposing",
            Self::Disposed => "disposed",
        }
    }
}

#[derive(Clone)]
pub struct LedgerEntryInfo {
    pub label: String,
    pub kind: &'static str,
    pub stack: Option<String>,
    pub children: Option<Vec<LedgerEntryInfo>>,
}

pub static CAPTURE_STACKS: AtomicBool = AtomicBool::new(false);
static NEXT_RECORD_ID: AtomicU64 = AtomicU64::new(1);

struct EntryRecord {
    id: u64,
    label: String,
    kind: &'static str,
    stack: Option<String>,
    active: AtomicBool,
    run: Disposer,
    ledger: Option<Ledger>,
}

struct LedgerInner {
    label: String,
    state: Mutex<LedgerState>,
    records: Mutex<Vec<Arc<EntryRecord>>>,
    teardown_promise: Mutex<Option<Shared<BoxFuture<'static, ()>>>>,
    parent_entry: Mutex<Option<LedgerEntry>>,
}

#[derive(Clone)]
pub struct Ledger {
    inner: Arc<LedgerInner>,
}

#[derive(Clone)]
pub struct LedgerEntry {
    label: String,
    record: Weak<EntryRecord>,
    owner: Weak<LedgerInner>,
}

impl LedgerEntry {
    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn disposed(&self) -> bool {
        self.record
            .upgrade()
            .is_none_or(|record| !record.active.load(Ordering::SeqCst))
    }

    pub fn dispose(&self, reason: TeardownReason<'_>) -> Option<BoxFuture<'static, ()>> {
        let record = self.record.upgrade()?;
        if !record.active.swap(false, Ordering::SeqCst) {
            return None;
        }
        if let Some(owner) = self.owner.upgrade() {
            remove_record(&owner, record.id);
        }
        run_guarded(record, reason)
    }

    pub fn release(&self) {
        let Some(record) = self.record.upgrade() else {
            return;
        };
        if !record.active.swap(false, Ordering::SeqCst) {
            return;
        }
        if let Some(owner) = self.owner.upgrade() {
            remove_record(&owner, record.id);
        }
    }
}

impl Ledger {
    pub fn new(label: &str) -> Self {
        Self {
            inner: Arc::new(LedgerInner {
                label: label.to_string(),
                state: Mutex::new(LedgerState::Active),
                records: Mutex::new(Vec::new()),
                teardown_promise: Mutex::new(None),
                parent_entry: Mutex::new(None),
            }),
        }
    }

    pub fn label(&self) -> &str {
        &self.inner.label
    }

    pub fn state(&self) -> LedgerState {
        *self.inner.state.lock()
    }

    pub fn is_active(&self) -> bool {
        self.state() == LedgerState::Active
    }

    pub fn is_disposed(&self) -> bool {
        self.state() == LedgerState::Disposed
    }

    pub fn size(&self) -> usize {
        self.inner
            .records
            .lock()
            .iter()
            .filter(|record| record.active.load(Ordering::SeqCst))
            .count()
    }

    pub fn register<F, R>(&self, disposer: F, label: &str) -> LedgerEntry
    where
        F: for<'a> Fn(TeardownReason<'a>) -> R + Send + Sync + 'static,
        R: IntoDisposerOutput,
    {
        self.assert_active("register")
            .unwrap_or_else(|error| panic!("{error}"));
        let disposer = Arc::new(move |reason| disposer(reason).into_disposer_output()) as Disposer;
        self.push("disposer", label, disposer, false, None)
    }

    pub fn register_finalizer<F, R>(&self, disposer: F, label: &str) -> LedgerEntry
    where
        F: for<'a> Fn(TeardownReason<'a>) -> R + Send + Sync + 'static,
        R: IntoDisposerOutput,
    {
        self.assert_active("registerFinalizer")
            .unwrap_or_else(|error| panic!("{error}"));
        let disposer = Arc::new(move |reason| disposer(reason).into_disposer_output()) as Disposer;
        self.push("disposer", label, disposer, true, None)
    }

    pub fn effect(&self, body: EffectBody, label: &str) -> LedgerEntry {
        self.assert_active("effect")
            .unwrap_or_else(|error| panic!("{error}"));
        match body() {
            EffectResult::None => self.push(
                "effect",
                label,
                Arc::new(|_| None),
                false,
                None,
            ),
            EffectResult::Disposer(disposer) => {
                self.push("effect", label, disposer, false, None)
            }
            EffectResult::Future(future) => {
                let future = future.shared();
                self.push(
                    "effect",
                    label,
                    Arc::new(move |reason| {
                        let reason = reason.to_string();
                        let future = future.clone();
                        Some(Box::pin(async move {
                            if let Some(disposer) = future.await
                                && let Some(future) = disposer(&reason)
                            {
                                future.await?;
                            }
                            Ok(())
                        }) as futures_util::future::BoxFuture<'static, Result<(), String>>)
                    }),
                    false,
                    None,
                )
            }
            EffectResult::Sync(disposers) => self.push(
                "effect",
                label,
                Arc::new(move |reason| run_disposers_reverse(disposers.clone(), reason)),
                false,
                None,
            ),
            EffectResult::Async(stream) => {
                let future = async move {
                    let disposers = stream.collect::<Vec<_>>().await;
                    Arc::new(move |reason| run_disposers_reverse(disposers.clone(), reason)) as Disposer
                }
                .boxed()
                .shared();
                self.push(
                    "effect",
                    label,
                    Arc::new(move |reason| {
                        let reason = reason.to_string();
                        let future = future.clone();
                        Some(Box::pin(async move {
                            let disposer = future.await;
                            if let Some(future) = disposer(&reason) {
                                future.await?;
                            }
                            Ok(())
                        }) as futures_util::future::BoxFuture<'static, Result<(), String>>)
                    }),
                    false,
                    None,
                )
            }
        }
    }

    pub fn create_child(&self, label: &str) -> Ledger {
        self.assert_active("createChild")
            .unwrap_or_else(|error| panic!("{error}"));
        let child = Ledger::new(label);
        let teardown_child = child.clone();
        let entry = self.push(
            "ledger",
            label,
            Arc::new(move |reason| teardown_child.teardown(reason)),
            false,
            Some(child.clone()),
        );
        *child.inner.parent_entry.lock() = Some(entry);
        child
    }

    pub fn teardown(&self, reason: TeardownReason<'_>) -> Option<BoxFuture<'static, ()>> {
        if self.state() != LedgerState::Active {
            return self
                .inner
                .teardown_promise
                .lock()
                .as_ref()
                .map(|promise| Box::pin(promise.clone()) as BoxFuture<'static, ()>);
        }
        *self.inner.state.lock() = LedgerState::Disposing;
        self.detach_from_parent();
        let records = self.inner.records.lock().clone();
        if let Some(drain) = drain_records(records, reason) {
            let inner = Arc::downgrade(&self.inner);
            let promise = async move {
                drain.await;
                if let Some(inner) = inner.upgrade() {
                    inner.records.lock().clear();
                    *inner.state.lock() = LedgerState::Disposed;
                }
            }
            .boxed()
            .shared();
            *self.inner.teardown_promise.lock() = Some(promise.clone());
            Some(Box::pin(promise))
        } else {
            self.inner.records.lock().clear();
            *self.inner.state.lock() = LedgerState::Disposed;
            None
        }
    }

    pub fn clear(&self, reason: TeardownReason<'_>) -> Option<BoxFuture<'static, ()>> {
        self.assert_active("clear")
            .unwrap_or_else(|error| panic!("{error}"));
        let records = self.inner.records.lock().clone();
        if let Some(future) = drain_records(records, reason) {
            let inner = Arc::downgrade(&self.inner);
            Some(async move {
                future.await;
                if let Some(inner) = inner.upgrade() {
                    inner.records.lock().clear();
                }
            }
            .boxed())
        } else {
            self.inner.records.lock().clear();
            None
        }
    }

    pub fn entries(&self) -> Vec<LedgerEntryInfo> {
        self.inner
            .records
            .lock()
            .iter()
            .filter(|record| record.active.load(Ordering::SeqCst))
            .map(|record| LedgerEntryInfo {
                label: record.label.clone(),
                kind: record.kind,
                stack: record.stack.clone(),
                children: record.ledger.as_ref().map(|ledger| ledger.entries()),
            })
            .collect()
    }

    fn push(
        &self,
        kind: &'static str,
        label: &str,
        run: Disposer,
        front: bool,
        ledger: Option<Ledger>,
    ) -> LedgerEntry {
        let record = Arc::new(EntryRecord {
            id: NEXT_RECORD_ID.fetch_add(1, Ordering::Relaxed),
            label: label.to_string(),
            kind,
            stack: CAPTURE_STACKS
                .load(Ordering::SeqCst)
                .then(|| std::backtrace::Backtrace::capture().to_string()),
            active: AtomicBool::new(true),
            run,
            ledger,
        });
        if front {
            self.inner.records.lock().insert(0, record.clone());
        } else {
            self.inner.records.lock().push(record.clone());
        }
        LedgerEntry {
            label: label.to_string(),
            record: Arc::downgrade(&record),
            owner: Arc::downgrade(&self.inner),
        }
    }

    fn detach_from_parent(&self) {
        if let Some(entry) = self.inner.parent_entry.lock().take() {
            entry.release();
        }
    }

    fn assert_active(&self, operation: &str) -> Result<(), LedgerDisposedError> {
        let state = self.state();
        if state == LedgerState::Active {
            Ok(())
        } else {
            Err(LedgerDisposedError::new(
                &self.inner.label,
                operation,
                state.as_str(),
            ))
        }
    }
}

fn remove_record(inner: &LedgerInner, id: u64) {
    inner.records.lock().retain(|record| record.id != id);
}

fn tagged(error: impl ToString, label: &str) -> String {
    format!("[ledger:{label}] {}", error.to_string())
}

fn run_guarded(record: Arc<EntryRecord>, reason: TeardownReason<'_>) -> Option<BoxFuture<'static, ()>> {
    let output = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        (record.run)(reason)
    }));
    match output {
        Ok(output) => output.map(|future| {
            let label = record.label.clone();
            async move {
                if let Err(error) = future.await {
                    on_unexpected_error(tagged(error, &label));
                }
            }
            .boxed()
        }),
        Err(error) => {
            let error = error
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| error.downcast_ref::<&str>().map(|value| (*value).to_string()))
                .unwrap_or_else(|| "unknown disposer error".to_string());
            on_unexpected_error(tagged(error, &record.label));
            None
        }
    }
}

fn drain_records(
    records: Vec<Arc<EntryRecord>>,
    reason: TeardownReason<'_>,
) -> Option<BoxFuture<'static, ()>> {
    let reason = reason.to_string();
    drain_records_at(records, records.len(), reason)
}

fn drain_records_at(
    records: Vec<Arc<EntryRecord>>,
    mut index: usize,
    reason: String,
) -> Option<BoxFuture<'static, ()>> {
    while index > 0 {
        index -= 1;
        let record = records[index].clone();
        if !record.active.swap(false, Ordering::SeqCst) {
            continue;
        }
        if let Some(future) = run_guarded(record, &reason) {
            return Some(async move {
                future.await;
                if let Some(future) = drain_records_at(records, index, reason) {
                    future.await;
                }
            }
            .boxed());
        }
    }
    None
}

fn run_disposers_reverse(
    disposers: Vec<Option<Disposer>>,
    reason: TeardownReason<'_>,
) -> DisposerOutput {
    let reason = reason.to_string();
    run_disposers_at(disposers, disposers.len(), reason)
}

fn run_disposers_at(
    disposers: Vec<Option<Disposer>>,
    mut index: usize,
    reason: String,
) -> DisposerOutput {
    while index > 0 {
        index -= 1;
        let Some(disposer) = disposers[index].clone() else {
            continue;
        };
        let output = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| disposer(&reason)));
        match output {
            Ok(Some(future)) => {
                return Some(async move {
                    if let Err(error) = future.await {
                        on_unexpected_error(tagged(error, "effect"));
                    }
                    if let Some(future) = run_disposers_at(disposers, index, reason) {
                        let _ = future.await;
                    }
                    Ok(())
                }
                .boxed());
            }
            Ok(None) => {}
            Err(error) => {
                let error = error
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| error.downcast_ref::<&str>().map(|value| (*value).to_string()))
                    .unwrap_or_else(|| "unknown effect error".to_string());
                on_unexpected_error(tagged(error, "effect"));
            }
        }
    }
    None
}
