
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub type TeardownError = Box<dyn Error + Send + Sync>;

pub type TeardownFuture = Pin<Box<dyn Future<Output = Result<(), TeardownError>> + Send>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TeardownReason {
    #[default]
    ScopeClose,
    Cascade,
    Unload,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerState {
    Active,
    Disposing,
    Disposed,
}

impl LedgerState {
    fn as_str(self) -> &'static str {
        match self {
            LedgerState::Active => "active",
            LedgerState::Disposing => "disposing",
            LedgerState::Disposed => "disposed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerEntryKind {
    Disposer,
    Effect,
    Ledger,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerEntryInfo {
    pub label: String,
    pub kind: LedgerEntryKind,
    pub stack: Option<String>,
    pub children: Vec<LedgerEntryInfo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LedgerEntryId {
    generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerDisposedError {
    pub ledger_label: String,
    pub operation: String,
    pub ledger_state: LedgerState,
}

impl fmt::Display for LedgerDisposedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.ledger_state.as_str();
        write!(
            f,
            "Ledger '{}' is {state}: cannot {} on a {state} ledger",
            self.ledger_label, self.operation
        )
    }
}

impl Error for LedgerDisposedError {}

pub struct Disposer {
    execute: Box<dyn FnOnce(TeardownReason) -> TeardownFuture + Send>,
}

impl Disposer {
    pub fn new<F>(execute: F) -> Self
    where
        F: FnOnce(TeardownReason) -> TeardownFuture + Send + 'static,
    {
        Self {
            execute: Box::new(execute),
        }
    }

    pub fn sync<F>(cleanup: F) -> Self
    where
        F: FnOnce(TeardownReason) + Send + 'static,
    {
        Self::new(move |reason| {
            Box::pin(async move {
                cleanup(reason);
                Ok(())
            })
        })
    }

    pub fn fallible<F>(cleanup: F) -> Self
    where
        F: FnOnce(TeardownReason) -> Result<(), TeardownError> + Send + 'static,
    {
        Self::new(move |reason| Box::pin(async move { cleanup(reason) }))
    }
}

enum EntryRun {
    Disposer(Disposer),
    Child(Box<Ledger>),
}

impl EntryRun {
    fn empty() -> Self {
        EntryRun::Disposer(Disposer::sync(|_| {}))
    }
}

struct EntryRecord {
    generation: u64,
    label: String,
    kind: LedgerEntryKind,
    active: Arc<AtomicBool>,
    run: EntryRun,
    stack: Option<String>,
}

pub struct Ledger {
    label: String,
    state: LedgerState,
    records: Vec<EntryRecord>,
    next_generation: u64,
    parent_entry_active: Option<Arc<AtomicBool>>,
}

impl Default for Ledger {
    fn default() -> Self {
        Self::new("ledger")
    }
}

impl Ledger {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            state: LedgerState::Active,
            records: Vec::new(),
            next_generation: 0,
            parent_entry_active: None,
        }
    }

    fn with_parent(label: impl Into<String>, parent_entry_active: Arc<AtomicBool>) -> Self {
        Self {
            parent_entry_active: Some(parent_entry_active),
            ..Self::new(label)
        }
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn state(&self) -> LedgerState {
        self.state
    }

    pub fn is_active(&self) -> bool {
        self.state == LedgerState::Active
    }

    pub fn is_disposed(&self) -> bool {
        self.state == LedgerState::Disposed
    }

    pub fn size(&self) -> usize {
        self.records
            .iter()
            .filter(|record| record.active.load(Ordering::Relaxed))
            .count()
    }

    pub fn register(&mut self, disposer: Disposer) -> Result<LedgerEntryId, LedgerDisposedError> {
        self.register_labeled("disposer", disposer)
    }

    pub fn register_labeled(
        &mut self,
        label: impl Into<String>,
        disposer: Disposer,
    ) -> Result<LedgerEntryId, LedgerDisposedError> {
        self.assert_active("register")?;
        Ok(self.push(
            label.into(),
            LedgerEntryKind::Disposer,
            EntryRun::Disposer(disposer),
            false,
        ))
    }

    pub fn register_finalizer(
        &mut self,
        disposer: Disposer,
    ) -> Result<LedgerEntryId, LedgerDisposedError> {
        self.register_finalizer_labeled("finalizer", disposer)
    }

    pub fn register_finalizer_labeled(
        &mut self,
        label: impl Into<String>,
        disposer: Disposer,
    ) -> Result<LedgerEntryId, LedgerDisposedError> {
        self.assert_active("registerFinalizer")?;
        Ok(self.push(
            label.into(),
            LedgerEntryKind::Disposer,
            EntryRun::Disposer(disposer),
            true,
        ))
    }

    pub fn effect<F>(
        &mut self,
        label: impl Into<String>,
        body: F,
    ) -> Result<LedgerEntryId, LedgerDisposedError>
    where
        F: FnOnce() -> Disposer,
    {
        self.effect_many(label, move || vec![body()])
    }

    pub fn effect_many<F>(
        &mut self,
        label: impl Into<String>,
        body: F,
    ) -> Result<LedgerEntryId, LedgerDisposedError>
    where
        F: FnOnce() -> Vec<Disposer>,
    {
        self.assert_active("effect")?;
        let disposers = body();
        let disposer = Disposer::new(move |reason| {
            Box::pin(async move {
                for disposer in disposers.into_iter().rev() {
                    run_guarded("effect", EntryRun::Disposer(disposer), reason).await;
                }
                Ok(())
            })
        });
        Ok(self.push(
            label.into(),
            LedgerEntryKind::Effect,
            EntryRun::Disposer(disposer),
            false,
        ))
    }

    pub fn create_child(
        &mut self,
        label: impl Into<String>,
    ) -> Result<LedgerEntryId, LedgerDisposedError> {
        self.assert_active("createChild")?;
        let label = label.into();
        let parent_entry_active = Arc::new(AtomicBool::new(true));
        let child = Ledger::with_parent(label.clone(), Arc::clone(&parent_entry_active));
        Ok(self.push_record(
            label,
            LedgerEntryKind::Ledger,
            EntryRun::Child(Box::new(child)),
            false,
            parent_entry_active,
        ))
    }

    pub fn child_mut(&mut self, id: LedgerEntryId) -> Option<&mut Ledger> {
        let index = self.index_of(id)?;
        match &mut self.records[index].run {
            EntryRun::Child(child) => Some(child),
            EntryRun::Disposer(_) => None,
        }
    }

    pub async fn teardown(&mut self, reason: TeardownReason) {
        if self.state != LedgerState::Active {
            return;
        }
        self.state = LedgerState::Disposing;
        // A ledger torn down on its own must stop being an active record of
        // its parent (`ledger.ts:128`, `ledger.ts:194-197`). The parent's
        // later drain then skips it instead of running it a second time.
        if let Some(parent_entry_active) = self.parent_entry_active.take() {
            parent_entry_active.store(false, Ordering::Relaxed);
        }
        self.drain(reason).await;
        self.state = LedgerState::Disposed;
    }

    pub async fn clear(&mut self, reason: TeardownReason) -> Result<(), LedgerDisposedError> {
        self.assert_active("clear")?;
        self.drain(reason).await;
        Ok(())
    }

    pub fn entries(&self) -> Vec<LedgerEntryInfo> {
        self.records
            .iter()
            .filter(|record| record.active.load(Ordering::Relaxed))
            .map(|record| LedgerEntryInfo {
                label: record.label.clone(),
                kind: record.kind,
                stack: record.stack.clone(),
                children: match &record.run {
                    EntryRun::Child(child) => child.entries(),
                    EntryRun::Disposer(_) => Vec::new(),
                },
            })
            .collect()
    }

    pub async fn dispose_entry(&mut self, id: LedgerEntryId, reason: TeardownReason) {
        let Some(index) = self.index_of(id) else {
            return;
        };
        if !self.records[index].active.load(Ordering::Relaxed) {
            return;
        }
        self.records[index].active.store(false, Ordering::Relaxed);
        let label = self.records[index].label.clone();
        let run = std::mem::replace(&mut self.records[index].run, EntryRun::empty());
        run_guarded(&label, run, reason).await;
    }

    pub fn release_entry(&mut self, id: LedgerEntryId) {
        if let Some(index) = self.index_of(id) {
            self.records[index].active.store(false, Ordering::Relaxed);
        }
    }

    fn assert_active(&self, operation: &str) -> Result<(), LedgerDisposedError> {
        if self.state == LedgerState::Active {
            return Ok(());
        }
        Err(LedgerDisposedError {
            ledger_label: self.label.clone(),
            operation: operation.to_string(),
            ledger_state: self.state,
        })
    }

    fn push(
        &mut self,
        label: String,
        kind: LedgerEntryKind,
        run: EntryRun,
        front: bool,
    ) -> LedgerEntryId {
        self.push_record(label, kind, run, front, Arc::new(AtomicBool::new(true)))
    }

    fn push_record(
        &mut self,
        label: String,
        kind: LedgerEntryKind,
        run: EntryRun,
        front: bool,
        active: Arc<AtomicBool>,
    ) -> LedgerEntryId {
        let generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1);
        let record = EntryRecord {
            generation,
            label,
            kind,
            active,
            run,
            stack: None,
        };
        if front {
            self.records.insert(0, record);
        } else {
            self.records.push(record);
        }
        LedgerEntryId { generation }
    }

    fn index_of(&self, id: LedgerEntryId) -> Option<usize> {
        self.records
            .iter()
            .position(|record| record.generation == id.generation)
    }

    async fn drain(&mut self, reason: TeardownReason) {
        let mut index = self.records.len();
        while index > 0 {
            index -= 1;
            if !self.records[index].active.load(Ordering::Relaxed) {
                continue;
            }
            self.records[index].active.store(false, Ordering::Relaxed);
            let label = self.records[index].label.clone();
            let run = std::mem::replace(&mut self.records[index].run, EntryRun::empty());
            run_guarded(&label, run, reason).await;
        }
    }
}

async fn run_guarded(label: &str, run: EntryRun, reason: TeardownReason) {
    let result = match run {
        EntryRun::Disposer(disposer) => (disposer.execute)(reason).await,
        EntryRun::Child(mut child) => {
            // Box the recursive future: a ledger tears down its children.
            Box::pin(child.teardown(reason)).await;
            Ok(())
        }
    };
    if let Err(error) = result {
        tracing::error!("[ledger:{label}] {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn recorder(order: &Arc<Mutex<Vec<&'static str>>>, label: &'static str) -> Disposer {
        let order = Arc::clone(order);
        Disposer::sync(move |_| order.lock().unwrap().push(label))
    }

    #[tokio::test]
    async fn disposers_run_in_reverse_registration_order() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut ledger = Ledger::new("root");
        ledger.register(recorder(&order, "first")).unwrap();
        ledger.register(recorder(&order, "second")).unwrap();
        ledger.register(recorder(&order, "third")).unwrap();
        ledger.teardown(TeardownReason::ScopeClose).await;
        assert_eq!(*order.lock().unwrap(), vec!["third", "second", "first"]);
        assert_eq!(ledger.state(), LedgerState::Disposed);
        assert_eq!(ledger.size(), 0);
    }

    #[tokio::test]
    async fn finalizer_runs_last_despite_being_registered_first() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut ledger = Ledger::new("root");
        ledger
            .register_finalizer(recorder(&order, "finalizer"))
            .unwrap();
        ledger.register(recorder(&order, "work")).unwrap();
        ledger.teardown(TeardownReason::Unload).await;
        assert_eq!(*order.lock().unwrap(), vec!["work", "finalizer"]);
    }

    #[tokio::test]
    async fn teardown_is_idempotent() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut ledger = Ledger::new("root");
        ledger.register(recorder(&order, "once")).unwrap();
        ledger.teardown(TeardownReason::ScopeClose).await;
        ledger.teardown(TeardownReason::ScopeClose).await;
        assert_eq!(*order.lock().unwrap(), vec!["once"]);
    }

    #[tokio::test]
    async fn a_failing_step_does_not_stop_the_rest() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut ledger = Ledger::new("root");
        ledger.register(recorder(&order, "first")).unwrap();
        ledger
            .register(Disposer::fallible(|_| {
                Err(Box::new(std::io::Error::other("boom")) as TeardownError)
            }))
            .unwrap();
        ledger.register(recorder(&order, "third")).unwrap();
        ledger.teardown(TeardownReason::ScopeClose).await;
        assert_eq!(*order.lock().unwrap(), vec!["third", "first"]);
    }

    #[tokio::test]
    async fn registering_after_teardown_is_rejected() {
        let mut ledger = Ledger::new("root");
        ledger.teardown(TeardownReason::ScopeClose).await;
        let error = ledger
            .register(Disposer::sync(|_| {}))
            .expect_err("disposed ledger must reject registration");
        assert_eq!(error.ledger_state, LedgerState::Disposed);
        assert_eq!(
            error.to_string(),
            "Ledger 'root' is disposed: cannot register on a disposed ledger"
        );
    }

    #[tokio::test]
    async fn clear_drains_without_changing_state() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut ledger = Ledger::new("root");
        ledger.register(recorder(&order, "cleared")).unwrap();
        ledger.clear(TeardownReason::Cascade).await.unwrap();
        assert_eq!(*order.lock().unwrap(), vec!["cleared"]);
        assert_eq!(ledger.state(), LedgerState::Active);
        assert_eq!(ledger.size(), 0);
    }

    #[tokio::test]
    async fn child_ledgers_tear_down_with_the_parent() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut root = Ledger::new("root");
        let child_id = root.create_child("child").unwrap();
        root.child_mut(child_id)
            .unwrap()
            .register(recorder(&order, "child-work"))
            .unwrap();
        root.register(recorder(&order, "root-work")).unwrap();
        root.teardown(TeardownReason::ScopeClose).await;
        assert_eq!(*order.lock().unwrap(), vec!["root-work", "child-work"]);
    }

    #[tokio::test]
    async fn entries_report_active_records_with_children() {
        let mut root = Ledger::new("root");
        root.register(Disposer::sync(|_| {})).unwrap();
        let child_id = root.create_child("nested").unwrap();
        root.child_mut(child_id)
            .unwrap()
            .register(Disposer::sync(|_| {}))
            .unwrap();
        let entries = root.entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].kind, LedgerEntryKind::Disposer);
        assert_eq!(entries[1].kind, LedgerEntryKind::Ledger);
        assert_eq!(entries[1].children.len(), 1);
    }

    #[tokio::test]
    async fn dispose_entry_runs_once_and_release_skips() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut ledger = Ledger::new("root");
        let early = ledger.register(recorder(&order, "early")).unwrap();
        let dropped = ledger.register(recorder(&order, "dropped")).unwrap();
        ledger.dispose_entry(early, TeardownReason::Cascade).await;
        ledger.dispose_entry(early, TeardownReason::Cascade).await;
        ledger.release_entry(dropped);
        ledger.teardown(TeardownReason::ScopeClose).await;
        assert_eq!(*order.lock().unwrap(), vec!["early"]);
    }

    #[tokio::test]
    async fn independently_torn_down_child_leaves_the_parent() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut root = Ledger::new("root");
        let child_id = root.create_child("child").unwrap();
        root.child_mut(child_id)
            .unwrap()
            .register(recorder(&order, "child-work"))
            .unwrap();
        root.child_mut(child_id)
            .unwrap()
            .teardown(TeardownReason::Cascade)
            .await;
        assert_eq!(*order.lock().unwrap(), vec!["child-work"]);
        assert_eq!(
            root.size(),
            0,
            "the detached child must not stay an active parent record"
        );
        assert!(root.entries().is_empty());

        root.teardown(TeardownReason::ScopeClose).await;
        assert_eq!(
            *order.lock().unwrap(),
            vec!["child-work"],
            "parent teardown must not run the detached child a second time"
        );
    }

    #[tokio::test]
    async fn effect_many_produces_zero_to_many_disposers_in_reverse() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut ledger = Ledger::new("root");
        ledger.effect_many("empty", || Vec::new()).unwrap();
        let many = Arc::clone(&order);
        ledger
            .effect_many("many", move || {
                vec![
                    recorder(&many, "a"),
                    recorder(&many, "b"),
                    recorder(&many, "c"),
                ]
            })
            .unwrap();
        assert_eq!(
            ledger.size(),
            2,
            "one effect is one record even when it produces no disposer"
        );
        ledger.teardown(TeardownReason::ScopeClose).await;
        assert_eq!(*order.lock().unwrap(), vec!["c", "b", "a"]);
    }

    #[tokio::test]
    async fn effect_many_guards_each_disposer() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut ledger = Ledger::new("root");
        let mixed = Arc::clone(&order);
        ledger
            .effect_many("mixed", move || {
                vec![
                    recorder(&mixed, "first"),
                    Disposer::fallible(|_| {
                        Err(Box::new(std::io::Error::other("boom")) as TeardownError)
                    }),
                    recorder(&mixed, "third"),
                ]
            })
            .unwrap();
        ledger.teardown(TeardownReason::ScopeClose).await;
        assert_eq!(*order.lock().unwrap(), vec!["third", "first"]);
    }
}
