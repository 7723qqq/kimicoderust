use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use async_trait::async_trait;
use futures_util::{Stream, StreamExt, TryStreamExt};
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde_json::{Map, Value, json};

use crate::_base::di::lifecycle::{IDisposable, to_disposable};
use crate::_base::log::log::ILogService;
use crate::persistence::interface::append_log_store::{
    AppendLogCorruptedError, AppendLogOptions, AppendLogReadOptions, AppendLogRewriteOptions,
    AppendLogTruncation, AppendLogTruncationReason, AppendLogWrite, IAppendLogStore,
};
use crate::persistence::interface::storage::{
    IFileSystemStorageService, STORAGE_CORRUPTED, StorageAppendOptions, StorageError,
    StorageReadRange, is_storage_error,
};
use crate::wire::record::WireAsyncError;

const STUCK_FLUSH_CHECK_MS: u64 = 5_000;
const STUCK_FLUSH_WARN_MS: u64 = 30_000;
const STUCK_FLUSH_REPEAT_MS: u64 = 10 * 60_000;

static PENDING_RETIREMENTS: Lazy<Mutex<Vec<tokio::task::JoinHandle<()>>>> =
    Lazy::new(|| Mutex::new(Vec::new()));

pub async fn drain_append_log_retirements() {
    loop {
        let pending = std::mem::take(&mut *PENDING_RETIREMENTS.lock());
        if pending.is_empty() {
            return;
        }
        for task in pending {
            let _ = task.await;
        }
    }
}

#[derive(Debug)]
struct LatchedError(Arc<WireAsyncError>);

impl fmt::Display for LatchedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for LatchedError {}

struct AppendRecovery {
    batch: Vec<Value>,
    encoded: Vec<u8>,
    base_length: u64,
    externally_modified_at_start: bool,
}

struct Retirement {
    done: AtomicBool,
    notify: tokio::sync::Notify,
}

struct LogState {
    pending: Vec<Value>,
    flush_active: bool,
    flush_scheduled: bool,
    flush_generation: u64,
    flush_notify: Arc<tokio::sync::Notify>,
    storage_failure: Option<Arc<WireAsyncError>>,
    recovery: Option<AppendRecovery>,
    failure_warned: bool,
    cutover_epoch: u64,
    ref_count: usize,
    retired: bool,
    ready: Option<Arc<Retirement>>,
    retirement: Option<Arc<Retirement>>,
    known_length: Option<u64>,
    in_flight_bytes: u64,
    rewrite_in_flight: Option<u64>,
    guard_warned: bool,
    on_error: Option<Arc<dyn Fn(WireAsyncError) + Send + Sync>>,
}

impl LogState {
    fn new(ready: Option<Arc<Retirement>>) -> Self {
        Self {
            pending: Vec::new(),
            flush_active: false,
            flush_scheduled: false,
            flush_generation: 0,
            flush_notify: Arc::new(tokio::sync::Notify::new()),
            storage_failure: None,
            recovery: None,
            failure_warned: false,
            cutover_epoch: 0,
            ref_count: 0,
            retired: false,
            ready,
            retirement: None,
            known_length: None,
            in_flight_bytes: 0,
            rewrite_in_flight: None,
            guard_warned: false,
            on_error: None,
        }
    }
}

struct AppendLogInner {
    logs: Mutex<HashMap<String, Arc<Mutex<LogState>>>>,
    write_guards: Mutex<HashMap<String, Arc<dyn Fn() -> bool + Send + Sync>>>,
    write_listeners: Mutex<HashMap<u64, Arc<dyn Fn(AppendLogWrite) + Send + Sync>>>,
    next_listener_id: AtomicU64,
    storage: Arc<dyn IFileSystemStorageService>,
    logger: Arc<dyn ILogService>,
}

#[derive(Clone)]
pub struct AppendLogStore {
    inner: Arc<AppendLogInner>,
}

impl AppendLogStore {
    pub fn new(
        storage: Arc<dyn IFileSystemStorageService>,
        logger: Arc<dyn ILogService>,
    ) -> Self {
        Self {
            inner: Arc::new(AppendLogInner {
                logs: Mutex::new(HashMap::new()),
                write_guards: Mutex::new(HashMap::new()),
                write_listeners: Mutex::new(HashMap::new()),
                next_listener_id: AtomicU64::new(0),
                storage,
                logger,
            }),
        }
    }

    fn state(&self, scope: &str, key: &str) -> Arc<Mutex<LogState>> {
        let id = log_id(scope, key);
        let mut logs = self.inner.logs.lock();
        if let Some(state) = logs.get(&id)
            && !state.lock().retired
        {
            return state.clone();
        }
        let ready = logs.get(&id).and_then(|state| state.lock().retirement.clone());
        let state = Arc::new(Mutex::new(LogState::new(ready)));
        logs.insert(id, state.clone());
        state
    }

    fn schedule_flush(&self, scope: &str, key: &str, state: Arc<Mutex<LogState>>) {
        {
            let mut state = state.lock();
            if state.flush_scheduled || state.flush_active {
                return;
            }
            state.flush_scheduled = true;
        }
        let this = self.clone();
        let scope = scope.to_string();
        let key = key.to_string();
        tokio::spawn(async move {
            state.lock().flush_scheduled = false;
            if let Err(error) = this.flush_state(&scope, &key, state.clone()).await {
                let callback = state.lock().on_error.clone();
                if let Some(callback) = callback {
                    callback(error);
                }
            }
        });
    }

    async fn flush_state(
        &self,
        scope: &str,
        key: &str,
        state: Arc<Mutex<LogState>>,
    ) -> Result<(), WireAsyncError> {
        loop {
            let generation = {
                let mut current = state.lock();
                if current.flush_active {
                    let generation = current.flush_generation;
                    drop(current);
                    wait_for_flush(&state, generation).await;
                    continue;
                }
                if let Some(error) = &current.storage_failure {
                    return Err(Box::new(LatchedError(error.clone())));
                }
                current.flush_active = true;
                current.flush_generation += 1;
                current.flush_generation
            };
            self.watch_stuck_flush(scope, key, state.clone(), generation);
            let wrote = Arc::new(AtomicBool::new(false));
            let result = self
                .drain(scope, key, state.clone(), Some(wrote.clone()))
                .await;
            let reschedule = self.finish_owned_flush(scope, key, state.clone());
            if wrote.load(Ordering::Relaxed) {
                self.fire_write(scope, key);
            }
            if let Err(error) = result {
                return Err(error);
            }
            if !reschedule {
                return Ok(());
            }
        }
    }

    fn finish_owned_flush(&self, scope: &str, key: &str, state: Arc<Mutex<LogState>>) -> bool {
        let reschedule = {
            let mut current = state.lock();
            current.flush_active = false;
            !current.pending.is_empty()
                && current.storage_failure.is_none()
                && self.write_allowed(scope, key, &mut current)
        };
        state_notify(&state);
        reschedule
    }

    fn watch_stuck_flush(
        &self,
        scope: &str,
        key: &str,
        state: Arc<Mutex<LogState>>,
        generation: u64,
    ) {
        let logger = self.inner.logger.clone();
        let scope = scope.to_string();
        let key = key.to_string();
        tokio::spawn(async move {
            let started = std::time::Instant::now();
            let mut last_warn = None;
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(STUCK_FLUSH_CHECK_MS)).await;
                let (active, pending) = {
                    let state = state.lock();
                    (
                        state.flush_active && state.flush_generation == generation,
                        state.pending.len(),
                    )
                };
                if !active {
                    return;
                }
                let stuck_ms = started.elapsed().as_millis() as u64;
                if stuck_ms < STUCK_FLUSH_WARN_MS {
                    continue;
                }
                if last_warn.is_some_and(|last: std::time::Instant| {
                    (last.elapsed().as_millis() as u64) < STUCK_FLUSH_REPEAT_MS
                }) {
                    continue;
                }
                last_warn = Some(std::time::Instant::now());
                logger.warn(
                    "append log flush is stuck; records are accumulating in memory",
                    Some(json!({ "scope": scope, "key": key, "pending": pending, "stuckMs": stuck_ms })),
                );
            }
        });
    }

    async fn drain(
        &self,
        scope: &str,
        key: &str,
        state: Arc<Mutex<LogState>>,
        wrote_box: Option<Arc<AtomicBool>>,
    ) -> Result<bool, WireAsyncError> {
        let (cutover_epoch, ready) = {
            let state = state.lock();
            (state.cutover_epoch, state.ready.clone())
        };
        if let Some(ready) = ready {
            wait_for_retirement(&ready).await;
        }
        if state.lock().cutover_epoch != cutover_epoch {
            return Ok(false);
        }
        if !state.lock().pending.is_empty()
            && !self.write_allowed(scope, key, &mut state.lock())
        {
            return Ok(false);
        }
        let mut wrote = false;
        loop {
            let (batch, encoded, base_length, recovery) = {
                let current = state.lock();
                if current.pending.is_empty() {
                    break;
                }
                let recovery = current.recovery.as_ref();
                let batch = recovery
                    .map(|recovery| recovery.batch.clone())
                    .unwrap_or_else(|| current.pending.clone());
                let encoded = recovery
                    .map(|recovery| recovery.encoded.clone())
                    .unwrap_or(encode_batch(&batch));
                let base_length = recovery.map(|recovery| recovery.base_length);
                (
                    batch,
                    encoded,
                    base_length,
                    current.recovery.as_ref().map(|recovery| {
                        (
                            recovery.encoded.clone(),
                            recovery.base_length,
                            recovery.externally_modified_at_start,
                        )
                    }),
                )
            };
            let base_length = base_length.or_else(|| self.disk_length(scope, key));
            let mut offset = 0usize;
            if recovery.is_some() {
                let recovery_data = AppendRecovery {
                    batch: batch.clone(),
                    encoded: encoded.clone(),
                    base_length: base_length.unwrap_or(0),
                    externally_modified_at_start: recovery.as_ref().unwrap().2,
                };
                match self.recovery_offset(scope, key, &recovery_data).await {
                    Ok(value) => offset = value,
                    Err(error) if is_storage_error(&error, STORAGE_CORRUPTED) => {
                        let mut current = state.lock();
                        if current.storage_failure.is_none() {
                            current.storage_failure = Some(Arc::new(error));
                            current.failure_warned = false;
                        }
                        return Err(Box::new(LatchedError(
                            current.storage_failure.as_ref().unwrap().clone(),
                        )));
                    }
                    Err(error) => return Err(error),
                }
                if state.lock().cutover_epoch != cutover_epoch {
                    return Ok(wrote);
                }
            }
            let externally_modified_at_start = recovery
                .as_ref()
                .map(|recovery| recovery.2)
                .unwrap_or_else(|| self.externally_modified(scope, key));
            state.lock().in_flight_bytes = (encoded.len() - offset) as u64;
            let append_result = self
                .inner
                .storage
                .append(
                    scope,
                    key,
                    encoded[offset..].to_vec(),
                    Some(StorageAppendOptions { durable: Some(true) }),
                )
                .await;
            match append_result {
                Ok(()) => {
                    let mut current = state.lock();
                    current.in_flight_bytes = 0;
                    current.recovery = None;
                    wrote = true;
                    if let Some(known) = current.known_length.as_mut() {
                        *known += encoded.len() as u64;
                    }
                    if let Some(wrote_box) = &wrote_box {
                        wrote_box.store(true, Ordering::Relaxed);
                    }
                }
                Err(error) => {
                    let mut current = state.lock();
                    current.in_flight_bytes = 0;
                    let Some(base_length) = base_length else {
                        if current.storage_failure.is_none() {
                            current.storage_failure = Some(Arc::new(error));
                            current.failure_warned = false;
                        }
                        return Err(Box::new(LatchedError(
                            current.storage_failure.as_ref().unwrap().clone(),
                        )));
                    };
                    current.recovery = Some(AppendRecovery {
                        batch,
                        encoded,
                        base_length,
                        externally_modified_at_start,
                    });
                    return Err(error);
                }
            }
            if state.lock().cutover_epoch != cutover_epoch {
                return Ok(wrote);
            }
            let mut current = state.lock();
            current.pending.drain(..batch.len().min(current.pending.len()));
            if current.pending.is_empty() {
                break;
            }
        }
        Ok(wrote)
    }

    async fn recovery_offset(
        &self,
        scope: &str,
        key: &str,
        recovery: &AppendRecovery,
    ) -> Result<usize, WireAsyncError> {
        let disk_length = self.inner.storage.size(scope, key).await?.unwrap_or(0);
        let suffix_length = disk_length as i128 - recovery.base_length as i128;
        if suffix_length < 0 || suffix_length > recovery.encoded.len() as i128 {
            return Err(Box::new(recovery_conflict(scope, key, recovery)));
        }
        if suffix_length == 0 {
            return Ok(0);
        }
        let mut offset = 0usize;
        let mut stream = self.inner.storage.read_stream(
            scope,
            key,
            Some(StorageReadRange {
                start: recovery.base_length,
                end: disk_length - 1,
            }),
        );
        while let Some(chunk) = stream.next().await {
            for byte in chunk? {
                if offset >= suffix_length as usize || byte != recovery.encoded[offset] {
                    return Err(Box::new(recovery_conflict(scope, key, recovery)));
                }
                offset += 1;
            }
        }
        if offset != suffix_length as usize {
            return Err(Box::new(recovery_conflict(scope, key, recovery)));
        }
        Ok(offset)
    }

    fn disk_length(&self, scope: &str, key: &str) -> Option<u64> {
        Some(self.inner.storage.size_sync(scope, key).unwrap_or(0))
    }

    fn write_allowed(&self, scope: &str, key: &str, state: &mut LogState) -> bool {
        let guard = self.inner.write_guards.lock().get(&log_id(scope, key)).cloned();
        if guard.as_ref().is_none_or(|guard| guard()) {
            state.guard_warned = false;
            return true;
        }
        if !state.guard_warned {
            state.guard_warned = true;
            tracing::warn!("[append-log] {scope}/{key}: write guard declined; keeping writes in memory");
        }
        false
    }

    fn fire_write(&self, scope: &str, key: &str) {
        let event = AppendLogWrite {
            scope: scope.to_string(),
            key: key.to_string(),
        };
        for listener in self.inner.write_listeners.lock().values() {
            listener(event.clone());
        }
    }

    async fn settle_retired_state(
        &self,
        scope: &str,
        key: &str,
        state: Arc<Mutex<LogState>>,
    ) {
        if self.flush_state(scope, key, state.clone()).await.is_ok() {
            let pending = state.lock().pending.len();
            if pending > 0 {
                tracing::warn!("[append-log] {scope}/{key}: dropping {pending} unflushed record(s) on close; write guard declined");
            }
        }
        let id = log_id(scope, key);
        let mut logs = self.inner.logs.lock();
        if logs.get(&id).is_some_and(|current| Arc::ptr_eq(current, &state)) {
            logs.remove(&id);
        }
    }
}

#[async_trait]
impl IAppendLogStore for AppendLogStore {
    fn on_did_write(
        &self,
        listener: Arc<dyn Fn(AppendLogWrite) + Send + Sync>,
    ) -> Box<dyn IDisposable> {
        let id = self.inner.next_listener_id.fetch_add(1, Ordering::Relaxed);
        self.inner.write_listeners.lock().insert(id, listener);
        let inner = Arc::downgrade(&self.inner);
        to_disposable(move || {
            if let Some(inner) = inner.upgrade() {
                inner.write_listeners.lock().remove(&id);
            }
        })
    }

    fn append(
        &self,
        scope: &str,
        key: &str,
        record: Value,
        options: Option<AppendLogOptions>,
    ) -> Result<(), WireAsyncError> {
        let state = self.state(scope, key);
        {
            let mut current = state.lock();
            current.pending.push(record);
            if current.storage_failure.is_some() && !current.failure_warned {
                current.failure_warned = true;
                self.inner.logger.warn(
                    "append accepted while a storage failure is latched; the record may never reach disk",
                    Some(json!({ "scope": scope, "key": key, "pending": current.pending.len() })),
                );
            }
            if current.on_error.is_none() {
                current.on_error = options.and_then(|options| options.on_error);
            }
        }
        self.schedule_flush(scope, key, state);
        Ok(())
    }

    fn read(
        &self,
        scope: &str,
        key: &str,
        options: Option<AppendLogReadOptions>,
    ) -> crate::persistence::interface::append_log_store::AppendLogReadStream {
        let context = ReadContext {
            store: self.clone(),
            scope: scope.to_string(),
            key: key.to_string(),
            options,
            state: None,
            stream: None,
            pending: Vec::new(),
            line_number: 0,
            length_at_start: None,
            finished: false,
        };
        Box::pin(futures_util::stream::try_unfold(context, |context| async move {
            next_record(context).await
        }))
    }

    async fn rewrite(
        &self,
        scope: &str,
        key: &str,
        records: Vec<Value>,
        options: Option<AppendLogRewriteOptions>,
    ) -> Result<(), WireAsyncError> {
        let encoded = encode_batch(&records);
        let state = self.state(scope, key);
        state.lock().cutover_epoch += 1;
        loop {
            let (prior_generation, ready) = {
                let mut current = state.lock();
                if current.flush_active {
                    (Some(current.flush_generation), None)
                } else if current.ready.as_ref().is_some_and(|ready| !ready.done.load(Ordering::Acquire)) {
                    (None, current.ready.clone())
                } else {
                    current.flush_active = true;
                    current.flush_generation += 1;
                    current.rewrite_in_flight = Some(encoded.len() as u64);
                    break;
                }
            };
            if let Some(generation) = prior_generation {
                wait_for_flush(&state, generation).await;
            } else if let Some(ready) = ready {
                wait_for_retirement(&ready).await;
            }
        }
        let result = self
            .inner
            .storage
            .write(
                scope,
                key,
                encoded.clone(),
                Some(crate::persistence::interface::storage::StorageWriteOptions {
                    atomic: Some(true),
                    signal: None,
                }),
            )
            .await;
        let wrote = match result {
            Ok(()) => {
                let mut current = state.lock();
                current.rewrite_in_flight = None;
                current.storage_failure = None;
                current.recovery = None;
                current.known_length = Some(encoded.len() as u64);
                true
            }
            Err(error) => {
                state.lock().rewrite_in_flight = None;
                if let Some(on_error) = options.and_then(|options| options.on_error) {
                    on_error(error);
                    false
                } else {
                    let mut current = state.lock();
                    current.storage_failure = Some(Arc::new(error));
                    current.flush_active = false;
                    drop(current);
                    state_notify(&state);
                    return Err(Box::new(LatchedError(
                        state.lock().storage_failure.as_ref().unwrap().clone(),
                    )));
                }
            }
        };
        let wrote_flag = Arc::new(AtomicBool::new(wrote));
        let result = self
            .drain(scope, key, state.clone(), Some(wrote_flag.clone()))
            .await;
        let reschedule = self.finish_owned_flush(scope, key, state.clone());
        if wrote_flag.load(Ordering::Relaxed) {
            self.fire_write(scope, key);
        }
        result?;
        if reschedule {
            self.flush_state(scope, key, state).await?;
        }
        Ok(())
    }

    async fn flush(&self) -> Result<(), WireAsyncError> {
        let logs = self
            .inner
            .logs
            .lock()
            .iter()
            .map(|(id, state)| (id.clone(), state.clone()))
            .collect::<Vec<_>>();
        let mut pending = Vec::new();
        for (id, state) in logs {
            let (scope, key) = from_log_id(&id);
            let this = self.clone();
            pending.push(async move { this.flush_state(&scope, &key, state).await });
        }
        for result in futures_util::future::join_all(pending).await {
            result?;
        }
        Ok(())
    }

    async fn flush_log(&self, scope: &str, key: &str) -> Result<(), WireAsyncError> {
        let state = self.state(scope, key);
        self.flush_state(scope, key, state).await
    }

    async fn close(&self) -> Result<(), WireAsyncError> {
        self.flush().await
    }

    fn acquire(&self, scope: &str, key: &str) -> Box<dyn IDisposable> {
        let state = self.state(scope, key);
        state.lock().ref_count += 1;
        let this = self.clone();
        let scope = scope.to_string();
        let key = key.to_string();
        let mut released = false;
        to_disposable(move || {
            if released {
                return;
            }
            released = true;
            let retirement = {
                let mut current = state.lock();
                current.ref_count = current.ref_count.saturating_sub(1);
                if current.ref_count > 0 {
                    return;
                }
                current.retired = true;
                let retirement = Arc::new(Retirement {
                    done: AtomicBool::new(false),
                    notify: tokio::sync::Notify::new(),
                });
                current.retirement = Some(retirement.clone());
                retirement
            };
            let task = {
                let this = this.clone();
                let scope = scope.clone();
                let key = key.clone();
                let state = state.clone();
                let retirement = retirement.clone();
                tokio::spawn(async move {
                    this.settle_retired_state(&scope, &key, state).await;
                    retirement.done.store(true, Ordering::Release);
                    retirement.notify.notify_waiters();
                })
            };
            PENDING_RETIREMENTS.lock().push(task);
        })
    }

    async fn drain_retirements(&self) -> Result<(), WireAsyncError> {
        drain_append_log_retirements().await;
        Ok(())
    }

    fn externally_modified(&self, scope: &str, key: &str) -> bool {
        let Some(state) = self.inner.logs.lock().get(&log_id(scope, key)).cloned() else {
            return false;
        };
        let current = state.lock();
        if current
            .recovery
            .as_ref()
            .is_some_and(|recovery| recovery.externally_modified_at_start)
        {
            return true;
        }
        let Some(known) = current.known_length else {
            return false;
        };
        let Some(disk) = self.disk_length(scope, key) else {
            return false;
        };
        if current.rewrite_in_flight == Some(disk) {
            return false;
        }
        let in_flight = current
            .recovery
            .as_ref()
            .map(|recovery| recovery.encoded.len() as u64)
            .unwrap_or(current.in_flight_bytes);
        disk < known || disk > known + in_flight
    }

    fn bind_write_guard(
        &self,
        scope: &str,
        key: &str,
        guard: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Box<dyn IDisposable> {
        let id = log_id(scope, key);
        self.inner.write_guards.lock().insert(id.clone(), guard.clone());
        let inner = Arc::downgrade(&self.inner);
        to_disposable(move || {
            if let Some(inner) = inner.upgrade() {
                let mut guards = inner.write_guards.lock();
                if guards.get(&id).is_some_and(|current| Arc::ptr_eq(current, &guard)) {
                    guards.remove(&id);
                }
            }
        })
    }
}

struct ReadContext {
    store: AppendLogStore,
    scope: String,
    key: String,
    options: Option<AppendLogReadOptions>,
    state: Option<Arc<Mutex<LogState>>>,
    stream: Option<crate::persistence::interface::storage::StorageByteStream>,
    pending: Vec<u8>,
    line_number: usize,
    length_at_start: Option<u64>,
    finished: bool,
}

impl Drop for ReadContext {
    fn drop(&mut self) {
        let Some(state) = &self.state else {
            return;
        };
        let Some(disk) = self.store.disk_length(&self.scope, &self.key) else {
            return;
        };
        state.lock().known_length = Some(if self.length_at_start == Some(disk) {
            disk
        } else {
            self.length_at_start.unwrap_or(0)
        });
    }
}

async fn next_record(
    mut context: ReadContext,
) -> Result<Option<(Value, ReadContext)>, WireAsyncError> {
    if context.finished {
        return Ok(None);
    }
    if context.stream.is_none() {
        context.store.flush_log(&context.scope, &context.key).await?;
        let state = context.store.state(&context.scope, &context.key);
        context.length_at_start = context.store.disk_length(&context.scope, &context.key);
        context.stream = Some(context.store.inner.storage.read_stream(&context.scope, &context.key, None));
        context.state = Some(state);
    }
    loop {
        if context.line_number == 0 && context.pending.starts_with(&[0xef, 0xbb, 0xbf]) {
            context.pending.drain(..3);
        }
        if let Some(newline) = context.pending.iter().position(|byte| *byte == b'\n') {
            let raw = context.pending.drain(..=newline).collect::<Vec<_>>();
            context.line_number += 1;
            let raw = String::from_utf8_lossy(&raw[..raw.len() - 1]).into_owned();
            match parse_line(&context.scope, &context.key, context.line_number, &raw, false) {
                Ok(Some(record)) => return Ok(Some((record, context))),
                Ok(None) => continue,
                Err(error) => {
                    if let Some(on_truncate) = context.options.as_ref().and_then(|options| options.on_truncate.clone()) {
                        on_truncate(AppendLogTruncation {
                            line_number: context.line_number,
                            reason: AppendLogTruncationReason::Corrupted,
                            cause: Some(error),
                        });
                        context.finished = true;
                        return Ok(None);
                    }
                    return Err(error);
                }
            }
        }
        let next = context.stream.as_mut().unwrap().next().await;
        match next {
            Some(chunk) => context.pending.extend(chunk?),
            None => {
                context.finished = true;
                if context.pending.is_empty() {
                    return Ok(None);
                }
                context.line_number += 1;
                let raw = String::from_utf8_lossy(&context.pending).into_owned();
                context.pending.clear();
                match parse_line(&context.scope, &context.key, context.line_number, &raw, true) {
                    Ok(Some(record)) => return Ok(Some((record, context))),
                    Ok(None) => {
                        if let Some(on_truncate) = context.options.as_ref().and_then(|options| options.on_truncate.clone()) {
                            let line = raw.strip_suffix('\r').unwrap_or(&raw);
                            if !line.is_empty() {
                                on_truncate(AppendLogTruncation {
                                    line_number: context.line_number,
                                    reason: AppendLogTruncationReason::Truncated,
                                    cause: None,
                                });
                            }
                        }
                        return Ok(None);
                    }
                    Err(error) => return Err(error),
                }
            }
        }
    }
}

fn parse_line(
    scope: &str,
    key: &str,
    line_number: usize,
    raw: &str,
    allow_truncated: bool,
) -> Result<Option<Value>, WireAsyncError> {
    let line = raw.strip_suffix('\r').unwrap_or(raw);
    if line.is_empty() {
        return Ok(None);
    }
    match serde_json::from_str(line) {
        Ok(record) => Ok(Some(record)),
        Err(error) if allow_truncated => Ok(None),
        Err(error) => Err(Box::new(AppendLogCorruptedError::new(
            scope,
            key,
            line_number,
            Some(Box::new(error)),
        ))),
    }
}

fn recovery_conflict(scope: &str, key: &str, recovery: &AppendRecovery) -> StorageError {
    StorageError::with_options(
        STORAGE_CORRUPTED,
        format!("append-log {scope}/{key}: unexpected content during append recovery"),
        kimi_core::primitives::errors::Error2Options {
            details: Some(Map::from_iter([
                ("scope".to_string(), Value::String(scope.to_string())),
                ("key".to_string(), Value::String(key.to_string())),
                ("baseLength".to_string(), Value::from(recovery.base_length)),
            ])),
            ..Default::default()
        },
    )
}

fn log_id(scope: &str, key: &str) -> String {
    format!("{scope}\n{key}")
}

fn from_log_id(id: &str) -> (String, String) {
    let index = id.find('\n').unwrap();
    (id[..index].to_string(), id[index + 1..].to_string())
}

fn encode_batch(records: &[Value]) -> Vec<u8> {
    let mut encoded = Vec::new();
    for record in records {
        serde_json::to_writer(&mut encoded, record).unwrap();
        encoded.push(b'\n');
    }
    encoded
}

async fn wait_for_retirement(retirement: &Retirement) {
    loop {
        let notified = retirement.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if retirement.done.load(Ordering::Acquire) {
            return;
        }
        notified.await;
    }
}

async fn wait_for_flush(state: &Arc<Mutex<LogState>>, generation: u64) {
    loop {
        let notify = {
            let state = state.lock();
            if !state.flush_active || state.flush_generation != generation {
                return;
            }
            state.flush_notify.clone()
        };
        let notified = notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        {
            let state = state.lock();
            if !state.flush_active || state.flush_generation != generation {
                return;
            }
        }
        notified.await;
    }
}

fn state_notify(state: &Arc<Mutex<LogState>>) {
    state.lock().flush_notify.notify_waiters();
}
