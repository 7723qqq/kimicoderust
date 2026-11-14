use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures_util::future::BoxFuture;
use parking_lot::Mutex;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecycleTransitionErrorReason {
    InvalidState,
    TransitionConflict,
    MissingCommitState,
    MissingRollbackState,
    AlreadyCommitted,
    AlreadyRolledBack,
}

#[derive(Clone)]
pub struct LifecycleTransitionError<TState>
where
    TState: Clone + Eq + fmt::Display,
{
    pub reason: LifecycleTransitionErrorReason,
    pub operation: String,
    pub state: TState,
    pub expected: Option<Vec<TState>>,
    pub active_operation: Option<String>,
}

impl<TState> LifecycleTransitionError<TState>
where
    TState: Clone + Eq + fmt::Display,
{
    fn message(&self) -> String {
        match self.reason {
            LifecycleTransitionErrorReason::InvalidState => format!(
                "Lifecycle operation \"{}\" is not allowed from state \"{}\"",
                self.operation, self.state
            ),
            LifecycleTransitionErrorReason::TransitionConflict => format!(
                "Lifecycle operation \"{}\" conflicts with active operation \"{}\"",
                self.operation,
                self.active_operation.clone().unwrap_or_default()
            ),
            LifecycleTransitionErrorReason::MissingCommitState => format!(
                "Lifecycle operation \"{}\" did not select a commit state",
                self.operation
            ),
            LifecycleTransitionErrorReason::MissingRollbackState => format!(
                "Lifecycle operation \"{}\" did not select a rollback state",
                self.operation
            ),
            LifecycleTransitionErrorReason::AlreadyCommitted => format!(
                "Lifecycle operation \"{}\" already selected a commit state",
                self.operation
            ),
            LifecycleTransitionErrorReason::AlreadyRolledBack => format!(
                "Lifecycle operation \"{}\" already selected a rollback state",
                self.operation
            ),
        }
    }
}

impl<TState> fmt::Display for LifecycleTransitionError<TState>
where
    TState: Clone + Eq + fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message())
    }
}

impl<TState> fmt::Debug for LifecycleTransitionError<TState>
where
    TState: Clone + Eq + fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message())
    }
}

impl<TState> std::error::Error for LifecycleTransitionError<TState> where
    TState: Clone + Eq + fmt::Display + fmt::Debug + Send + Sync + 'static
{}

#[derive(Clone)]
pub struct LifecycleSnapshot<TState> {
    pub state: TState,
    pub transitioning: bool,
    pub operation: Option<String>,
}

#[derive(Clone)]
pub enum ExpectedStates<TState> {
    One(TState),
    Many(Vec<TState>),
}

impl<TState> From<TState> for ExpectedStates<TState> {
    fn from(value: TState) -> Self {
        Self::One(value)
    }
}

impl<TState> From<Vec<TState>> for ExpectedStates<TState> {
    fn from(value: Vec<TState>) -> Self {
        Self::Many(value)
    }
}

impl<TState> ExpectedStates<TState> {
    fn states(&self) -> Vec<TState>
    where
        TState: Clone,
    {
        match self {
            Self::One(state) => vec![state.clone()],
            Self::Many(states) => states.clone(),
        }
    }
}

#[derive(Clone)]
pub struct LifecycleSwitchOptions<TState> {
    pub operation: String,
    pub from: ExpectedStates<TState>,
    pub to: TState,
}

#[derive(Clone)]
pub struct LifecycleTransactionOptions<TState> {
    pub operation: String,
    pub from: ExpectedStates<TState>,
    pub enter: TState,
    pub commit: Option<TState>,
    pub rollback: Option<TState>,
}

pub type LifecycleAction =
    Arc<dyn Fn() -> BoxFuture<'static, Result<(), String>> + Send + Sync>;

struct MachineStatus<TState> {
    state: TState,
    active_operation: Option<String>,
}

struct MachineInner<TState> {
    status: Mutex<MachineStatus<TState>>,
}

pub struct LifecycleTransaction<TState>
where
    TState: Clone + Eq + fmt::Display + Send + Sync + 'static,
{
    inner: Arc<MachineInner<TState>>,
    operation: String,
    deferred: Mutex<Vec<LifecycleAction>>,
    rollbacks: Mutex<Vec<LifecycleAction>>,
    after_commit: Mutex<Vec<LifecycleAction>>,
    commit_state: Mutex<Option<TState>>,
    rollback_state: Mutex<Option<TState>>,
    commit_selected: AtomicBool,
    rollback_selected: AtomicBool,
}

impl<TState> LifecycleTransaction<TState>
where
    TState: Clone + Eq + fmt::Display + Send + Sync + 'static,
{
    pub fn defer(&self, callback: LifecycleAction) {
        self.deferred.lock().push(callback);
    }

    pub fn rollback(&self, callback: LifecycleAction) {
        self.rollbacks.lock().push(callback);
    }

    pub fn after_commit(&self, callback: LifecycleAction) {
        self.after_commit.lock().push(callback);
    }

    pub fn commit(&self, state: TState) -> Result<(), LifecycleTransitionError<TState>> {
        if self.commit_selected.swap(true, Ordering::SeqCst) {
            return Err(create_error(
                &self.inner,
                &self.operation,
                LifecycleTransitionErrorReason::AlreadyCommitted,
                None,
            ));
        }
        *self.commit_state.lock() = Some(state);
        Ok(())
    }

    pub fn rollback_to(&self, state: TState) -> Result<(), LifecycleTransitionError<TState>> {
        if self.rollback_selected.swap(true, Ordering::SeqCst) {
            return Err(create_error(
                &self.inner,
                &self.operation,
                LifecycleTransitionErrorReason::AlreadyRolledBack,
                None,
            ));
        }
        *self.rollback_state.lock() = Some(state);
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct LifecycleError {
    pub message: String,
    pub errors: Vec<String>,
}

impl fmt::Display for LifecycleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for LifecycleError {}

pub struct LifecycleMachine<TState>
where
    TState: Clone + Eq + fmt::Display + Send + Sync + 'static,
{
    inner: Arc<MachineInner<TState>>,
}

impl<TState> LifecycleMachine<TState>
where
    TState: Clone + Eq + fmt::Display + Send + Sync + 'static,
{
    pub fn new(initial: TState) -> Self {
        Self {
            inner: Arc::new(MachineInner {
                status: Mutex::new(MachineStatus {
                    state: initial,
                    active_operation: None,
                }),
            }),
        }
    }

    pub fn state(&self) -> TState {
        self.inner.status.lock().state.clone()
    }

    pub fn snapshot(&self) -> LifecycleSnapshot<TState> {
        let status = self.inner.status.lock();
        LifecycleSnapshot {
            state: status.state.clone(),
            transitioning: status.active_operation.is_some(),
            operation: status.active_operation.clone(),
        }
    }

    pub fn is(&self, states: &[TState]) -> bool {
        states.contains(&self.state())
    }

    pub fn switch(
        &self,
        options: LifecycleSwitchOptions<TState>,
    ) -> Result<(), LifecycleTransitionError<TState>> {
        let mut status = self.inner.status.lock();
        assert_idle(&self.inner, &options.operation, &mut status)?;
        assert_state(&self.inner, &options.operation, &options.from, &mut status)?;
        status.state = options.to;
        Ok(())
    }

    pub async fn transaction<TResult, F, Fut>(
        &self,
        options: LifecycleTransactionOptions<TState>,
        callback: F,
    ) -> Result<TResult, LifecycleError>
    where
        F: FnOnce(&LifecycleTransaction<TState>) -> Fut,
        Fut: Future<Output = Result<TResult, String>>,
    {
        {
            let mut status = self.inner.status.lock();
            assert_idle(&self.inner, &options.operation, &mut status).map_err(transition_failure)?;
            assert_state(&self.inner, &options.operation, &options.from, &mut status)
                .map_err(transition_failure)?;
            status.active_operation = Some(options.operation.clone());
            status.state = options.enter.clone();
        }
        let guard = ActiveGuard {
            inner: self.inner.clone(),
        };
        let transaction = LifecycleTransaction {
            inner: self.inner.clone(),
            operation: options.operation.clone(),
            deferred: Mutex::new(Vec::new()),
            rollbacks: Mutex::new(Vec::new()),
            after_commit: Mutex::new(Vec::new()),
            commit_state: Mutex::new(options.commit.clone()),
            rollback_state: Mutex::new(options.rollback.clone()),
            commit_selected: AtomicBool::new(false),
            rollback_selected: AtomicBool::new(false),
        };
        let result = match callback(&transaction).await {
            Ok(result) => result,
            Err(error) => {
                let mut errors = vec![error];
                errors.extend(run_actions(transaction.rollbacks.lock().clone()).await);
                errors.extend(run_actions(transaction.deferred.lock().clone()).await);
                let rollback_state = transaction.rollback_state.lock().clone();
                if let Some(rollback_state) = rollback_state {
                    self.inner.status.lock().state = rollback_state;
                } else {
                    errors.push(
                        create_error(
                            &self.inner,
                            &options.operation,
                            LifecycleTransitionErrorReason::MissingRollbackState,
                            None,
                        )
                        .to_string(),
                    );
                }
                return Err(aggregate_errors(
                    errors,
                    &format!("Lifecycle transaction \"{}\" failed", options.operation),
                ));
            }
        };
        let Some(commit_state) = transaction.commit_state.lock().clone() else {
            return Err(transition_failure(create_error(
                &self.inner,
                &options.operation,
                LifecycleTransitionErrorReason::MissingCommitState,
                None,
            )));
        };
        let cleanup_errors = run_actions(transaction.deferred.lock().clone()).await;
        self.inner.status.lock().state = commit_state;
        let after_commit_errors = run_actions(transaction.after_commit.lock().clone()).await;
        let mut errors = cleanup_errors;
        errors.extend(after_commit_errors);
        if !errors.is_empty() {
            return Err(aggregate_errors(
                errors,
                &format!(
                    "Lifecycle transaction \"{}\" committed with action failures",
                    options.operation
                ),
            ));
        }
        drop(guard);
        Ok(result)
    }
}

struct ActiveGuard<TState> {
    inner: Arc<MachineInner<TState>>,
}

impl<TState> Drop for ActiveGuard<TState> {
    fn drop(&mut self) {
        self.inner.status.lock().active_operation = None;
    }
}

fn assert_idle<TState>(
    inner: &Arc<MachineInner<TState>>,
    operation: &str,
    status: &mut MachineStatus<TState>,
) -> Result<(), LifecycleTransitionError<TState>>
where
    TState: Clone + Eq + fmt::Display,
{
    if status.active_operation.is_none() {
        return Ok(());
    }
    Err(LifecycleTransitionError {
        reason: LifecycleTransitionErrorReason::TransitionConflict,
        operation: operation.to_string(),
        state: status.state.clone(),
        expected: None,
        active_operation: status.active_operation.clone(),
    })
}

fn assert_state<TState>(
    _inner: &Arc<MachineInner<TState>>,
    operation: &str,
    expected: &ExpectedStates<TState>,
    status: &mut MachineStatus<TState>,
) -> Result<(), LifecycleTransitionError<TState>>
where
    TState: Clone + Eq + fmt::Display,
{
    let states = expected.states();
    if states.contains(&status.state) {
        return Ok(());
    }
    Err(LifecycleTransitionError {
        reason: LifecycleTransitionErrorReason::InvalidState,
        operation: operation.to_string(),
        state: status.state.clone(),
        expected: Some(states),
        active_operation: None,
    })
}

fn create_error<TState>(
    inner: &Arc<MachineInner<TState>>,
    operation: &str,
    reason: LifecycleTransitionErrorReason,
    expected: Option<Vec<TState>>,
) -> LifecycleTransitionError<TState>
where
    TState: Clone + Eq + fmt::Display,
{
    let status = inner.status.lock();
    LifecycleTransitionError {
        reason,
        operation: operation.to_string(),
        state: status.state.clone(),
        expected,
        active_operation: status.active_operation.clone(),
    }
}

async fn run_actions(actions: Vec<LifecycleAction>) -> Vec<String> {
    let mut errors = Vec::new();
    for action in actions.into_iter().rev() {
        if let Err(error) = action().await {
            errors.push(error);
        }
    }
    errors
}

fn aggregate_errors(errors: Vec<String>, message: &str) -> LifecycleError {
    if errors.len() == 1 {
        return LifecycleError {
            message: errors[0].clone(),
            errors,
        };
    }
    LifecycleError {
        message: message.to_string(),
        errors,
    }
}

fn transition_failure<TState>(error: LifecycleTransitionError<TState>) -> LifecycleError
where
    TState: Clone + Eq + fmt::Display,
{
    LifecycleError {
        message: error.to_string(),
        errors: vec![error.to_string()],
    }
}
