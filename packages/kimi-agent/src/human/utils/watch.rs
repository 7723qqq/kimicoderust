use std::collections::HashMap;
use std::env;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Weak};
use std::thread;
use std::time::{Duration, SystemTime};

use parking_lot::{Condvar, Mutex};

use crate::_base::di::lifecycle::IDisposable;

pub const WATCH_ENV: &str = "KIMI_CODE_WATCH";
const NATIVE_RETRY_BASE_MS: u64 = 1000;
const NATIVE_RETRY_MAX_MS: u64 = 30000;
const LISTING_MISSING_RETRY_MS: u64 = 250;
const POLL_INTERVAL_MS: u64 = 200;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WatchChangeAction {
    Created,
    Modified,
    Deleted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WatchChangeKind {
    File,
    Directory,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WatchChange {
    pub path: String,
    pub action: WatchChangeAction,
    pub kind: WatchChangeKind,
}

pub type WatchIgnored = Arc<dyn Fn(&str) -> bool + Send + Sync>;

#[derive(Clone, Default)]
pub struct WatchOptions {
    pub recursive: Option<bool>,
    pub ignored: Option<WatchIgnored>,
    pub depth: Option<usize>,
    pub signal: Option<bool>,
    pub listing: Option<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WatchError {
    pub code: Option<String>,
    pub message: String,
}

impl WatchError {
    fn new(code: Option<&str>, message: impl Into<String>) -> Self {
        Self {
            code: code.map(str::to_string),
            message: message.into(),
        }
    }

    fn from_io(error: &io::Error) -> Self {
        let code = match error.kind() {
            io::ErrorKind::NotFound => Some("ENOENT"),
            io::ErrorKind::PermissionDenied => Some("EACCES"),
            _ => None,
        };
        Self::new(code, error.to_string())
    }
}

impl fmt::Display for WatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for WatchError {}

pub struct WatchSubscription {
    cleanup: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl WatchSubscription {
    fn new(cleanup: impl FnOnce() + Send + 'static) -> Self {
        Self {
            cleanup: Mutex::new(Some(Box::new(cleanup))),
        }
    }

    pub fn noop() -> Self {
        Self::new(|| {})
    }

    pub fn dispose(&self) {
        if let Some(cleanup) = self.cleanup.lock().take() {
            cleanup();
        }
    }
}

impl IDisposable for WatchSubscription {
    fn dispose(&self) {
        WatchSubscription::dispose(self);
    }
}

impl Drop for WatchSubscription {
    fn drop(&mut self) {
        self.dispose();
    }
}

pub trait NativeFsWatcher: Send + Sync {
    fn close(&self);
    fn on_error(&self, listener: Arc<dyn Fn(WatchError) + Send + Sync>);
}

pub trait WatchRuntime: Send + Sync {
    fn platform(&self) -> &str;

    fn resolve_path(&self, _path: &str) -> Option<String> {
        None
    }

    fn watch_native(
        &self,
        root: &str,
        listener: Arc<dyn Fn(String, Option<String>) + Send + Sync>,
        recursive: bool,
    ) -> Result<Arc<dyn NativeFsWatcher>, WatchError>;

    fn schedule_retry(&self, callback: Arc<dyn Fn() + Send + Sync>, delay_ms: u64)
    -> WatchSubscription;

    fn report_error(&self, error: &WatchError);
}

struct ReadyState {
    result: Mutex<Option<Result<(), WatchError>>>,
    ready: Condvar,
}

impl ReadyState {
    fn new() -> Self {
        Self {
            result: Mutex::new(None),
            ready: Condvar::new(),
        }
    }

    fn settle(&self, result: Result<(), WatchError>) {
        let mut current = self.result.lock();
        if current.is_some() {
            return;
        }
        *current = Some(result);
        self.ready.notify_all();
    }

    fn wait(&self) -> Result<(), WatchError> {
        let mut current = self.result.lock();
        while current.is_none() {
            self.ready.wait(&mut current);
        }
        current.clone().unwrap()
    }
}

struct HandleInner {
    listeners: Mutex<HashMap<u64, Arc<dyn Fn(WatchChange) + Send + Sync>>>,
    next_listener: AtomicU64,
    disposed: AtomicBool,
    ready: ReadyState,
    map_path: Arc<dyn Fn(&str) -> String + Send + Sync>,
}

impl HandleInner {
    fn new(map_path: Arc<dyn Fn(&str) -> String + Send + Sync>) -> Self {
        Self {
            listeners: Mutex::new(HashMap::new()),
            next_listener: AtomicU64::new(0),
            disposed: AtomicBool::new(false),
            ready: ReadyState::new(),
            map_path,
        }
    }

    fn emit(&self, change: WatchChange) {
        if self.disposed.load(Ordering::Acquire) {
            return;
        }
        let change = WatchChange {
            path: (self.map_path)(&change.path),
            ..change
        };
        let listeners = self
            .listeners
            .lock()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for listener in listeners {
            listener(change.clone());
        }
    }

    fn dispose(&self) {
        if self.disposed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.ready.settle(Ok(()));
        self.listeners.lock().clear();
    }
}

pub struct WatchHandle {
    inner: Arc<HandleInner>,
    cleanup: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl WatchHandle {
    fn new(
        inner: Arc<HandleInner>,
        cleanup: impl FnOnce() + Send + 'static,
    ) -> Self {
        Self {
            inner,
            cleanup: Mutex::new(Some(Box::new(cleanup))),
        }
    }

    pub async fn ready(&self) -> Result<(), WatchError> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || inner.ready.wait())
            .await
            .map_err(|error| WatchError::new(None, error.to_string()))?
    }

    pub fn on_did_change(
        &self,
        listener: impl Fn(WatchChange) + Send + Sync + 'static,
    ) -> Box<WatchSubscription> {
        if self.inner.disposed.load(Ordering::Acquire) {
            return Box::new(WatchSubscription::noop());
        }
        let id = self.inner.next_listener.fetch_add(1, Ordering::Relaxed);
        self.inner.listeners.lock().insert(id, Arc::new(listener));
        let weak = Arc::downgrade(&self.inner);
        Box::new(WatchSubscription::new(move || {
            if let Some(inner) = weak.upgrade() {
                inner.listeners.lock().remove(&id);
            }
        }))
    }

    pub fn dispose(&self) {
        if self.inner.disposed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.inner.ready.settle(Ok(()));
        if let Some(cleanup) = self.cleanup.lock().take() {
            cleanup();
        }
        self.inner.listeners.lock().clear();
    }
}

impl IDisposable for WatchHandle {
    fn dispose(&self) {
        WatchHandle::dispose(self);
    }
}

impl Drop for WatchHandle {
    fn drop(&mut self) {
        self.dispose();
    }
}

#[derive(Clone)]
struct EntryState {
    kind: WatchChangeKind,
    modified: Option<SystemTime>,
    len: u64,
}

fn default_ignored(path: &str) -> bool {
    path.replace('\\', "/")
        .split('/')
        .any(|segment| segment == ".git")
}

fn ignored(options: &WatchOptions, path: &str) -> bool {
    options
        .ignored
        .as_ref()
        .map_or_else(|| default_ignored(path), |callback| callback(path))
}

fn scan(
    root: &Path,
    recursive: bool,
    depth: Option<usize>,
    options: &WatchOptions,
) -> io::Result<HashMap<PathBuf, EntryState>> {
    let mut entries = HashMap::new();
    scan_entry(root, recursive, depth, options, &mut entries)?;
    Ok(entries)
}

fn scan_entry(
    path: &Path,
    recursive: bool,
    depth: Option<usize>,
    options: &WatchOptions,
    entries: &mut HashMap<PathBuf, EntryState>,
) -> io::Result<()> {
    let text = path.to_string_lossy().into_owned();
    if ignored(options, &text) {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(path)?;
    let kind = if metadata.is_dir() {
        WatchChangeKind::Directory
    } else {
        WatchChangeKind::File
    };
    entries.insert(
        path.to_path_buf(),
        EntryState {
            kind,
            modified: metadata.modified().ok(),
            len: metadata.len(),
        },
    );
    if kind == WatchChangeKind::Directory && recursive && depth != Some(0) {
        let next_depth = depth.map(|value| value.saturating_sub(1));
        for entry in fs::read_dir(path)? {
            scan_entry(&entry?.path(), recursive, next_depth, options, entries)?;
        }
    }
    Ok(())
}

fn diff(
    previous: &HashMap<PathBuf, EntryState>,
    current: &HashMap<PathBuf, EntryState>,
) -> Vec<WatchChange> {
    let mut changes = Vec::new();
    for (path, state) in current {
        match previous.get(path) {
            None => changes.push(WatchChange {
                path: path.to_string_lossy().into_owned(),
                action: WatchChangeAction::Created,
                kind: state.kind,
            }),
            Some(old)
                if state.kind == WatchChangeKind::File
                    && (old.modified != state.modified || old.len != state.len) =>
            {
                changes.push(WatchChange {
                    path: path.to_string_lossy().into_owned(),
                    action: WatchChangeAction::Modified,
                    kind: state.kind,
                });
            }
            _ => {}
        }
    }
    for (path, state) in previous {
        if !current.contains_key(path) {
            changes.push(WatchChange {
                path: path.to_string_lossy().into_owned(),
                action: WatchChangeAction::Deleted,
                kind: state.kind,
            });
        }
    }
    changes
}

struct PollWatcher {
    disposed: Arc<AtomicBool>,
    errors: Arc<Mutex<Option<Arc<dyn Fn(WatchError) + Send + Sync>>>>,
}

impl PollWatcher {
    fn start(
        root: &str,
        recursive: bool,
        depth: Option<usize>,
        options: WatchOptions,
        listener: Arc<dyn Fn(WatchChange) + Send + Sync>,
        fail_initial: bool,
    ) -> Result<Arc<Self>, WatchError> {
        let initial = scan(Path::new(root), recursive, depth, &options);
        if fail_initial {
            if let Err(error) = &initial {
                return Err(WatchError::from_io(error));
            }
        }
        let watcher = Arc::new(Self {
            disposed: Arc::new(AtomicBool::new(false)),
            errors: Arc::new(Mutex::new(None)),
        });
        let running = watcher.clone();
        let root = root.to_string();
        thread::spawn(move || {
            let mut previous = match initial {
                Ok(entries) => {
                    running.errors.lock().as_ref().inspect(|_| {});
                    entries
                }
                Err(error) => {
                    if let Some(listener) = running.errors.lock().clone() {
                        listener(WatchError::from_io(&error));
                    }
                    HashMap::new()
                }
            };
            let mut initialized = initial_is_ok(Path::new(&root), recursive, depth, &options);
            if initialized {
                running.errors.lock().as_ref().inspect(|_| {});
            }
            while !running.disposed.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));
                if running.disposed.load(Ordering::Acquire) {
                    break;
                }
                match scan(Path::new(&root), recursive, depth, &options) {
                    Ok(current) => {
                        for change in diff(&previous, &current) {
                            listener(change);
                        }
                        previous = current;
                        initialized = true;
                    }
                    Err(error) => {
                        if let Some(listener) = running.errors.lock().clone() {
                            listener(WatchError::from_io(&error));
                        }
                        if !initialized {
                            previous.clear();
                        }
                    }
                }
            }
        });
        Ok(watcher)
    }
}

fn initial_is_ok(
    root: &Path,
    recursive: bool,
    depth: Option<usize>,
    options: &WatchOptions,
) -> bool {
    scan(root, recursive, depth, options).is_ok()
}

impl NativeFsWatcher for PollWatcher {
    fn close(&self) {
        self.disposed.store(true, Ordering::Release);
    }

    fn on_error(&self, listener: Arc<dyn Fn(WatchError) + Send + Sync>) {
        *self.errors.lock() = Some(listener);
    }
}

struct NodeWatchRuntime;

impl NodeWatchRuntime {
    fn resolve_long_path(path: &str) -> String {
        let mut missing = Vec::new();
        let mut current = PathBuf::from(path);
        loop {
            match fs::canonicalize(&current) {
                Ok(base) => {
                    let mut resolved = base;
                    for part in missing.iter().rev() {
                        resolved.push(part);
                    }
                    return resolved.to_string_lossy().into_owned();
                }
                Err(_) => {
                    let Some(parent) = current.parent() else {
                        return path.to_string();
                    };
                    if parent == current {
                        return path.to_string();
                    }
                    let Some(name) = current.file_name() else {
                        return path.to_string();
                    };
                    missing.push(name.to_string_lossy().into_owned());
                    current = parent.to_path_buf();
                }
            }
        }
    }
}

impl WatchRuntime for NodeWatchRuntime {
    fn platform(&self) -> &str {
        if cfg!(windows) {
            "win32"
        } else if cfg!(target_os = "macos") {
            "darwin"
        } else {
            "linux"
        }
    }

    fn resolve_path(&self, path: &str) -> Option<String> {
        if cfg!(windows)
            && path
                .as_bytes()
                .windows(2)
                .any(|pair| pair[0] == b'~' && pair[1].is_ascii_digit())
        {
            Some(Self::resolve_long_path(path))
        } else {
            None
        }
    }

    fn watch_native(
        &self,
        root: &str,
        listener: Arc<dyn Fn(String, Option<String>) + Send + Sync>,
        recursive: bool,
    ) -> Result<Arc<dyn NativeFsWatcher>, WatchError> {
        let change_listener = Arc::new(move |change: WatchChange| {
            listener("change".to_string(), Some(change.path));
        });
        let watcher = PollWatcher::start(
            root,
            recursive,
            None,
            WatchOptions {
                recursive: Some(recursive),
                ..WatchOptions::default()
            },
            change_listener,
            true,
        )?;
        Ok(watcher)
    }

    fn schedule_retry(
        &self,
        callback: Arc<dyn Fn() + Send + Sync>,
        delay_ms: u64,
    ) -> WatchSubscription {
        let disposed = Arc::new(AtomicBool::new(false));
        let running = disposed.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(delay_ms));
            if !running.load(Ordering::Acquire) {
                callback();
            }
        });
        WatchSubscription::new(move || {
            disposed.store(true, Ordering::Release);
        })
    }

    fn report_error(&self, error: &WatchError) {
        eprintln!("{error}");
    }
}

struct NativeControl {
    root: String,
    options: WatchOptions,
    runtime: Arc<dyn WatchRuntime>,
    handle: Weak<HandleInner>,
    recursive: bool,
    recovering: AtomicBool,
    retry_attempts: AtomicU32,
    cleanup: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    retry: Mutex<Option<WatchSubscription>>,
}

fn invalidation(path: &str) -> WatchChange {
    WatchChange {
        path: path.to_string(),
        action: WatchChangeAction::Modified,
        kind: WatchChangeKind::Directory,
    }
}

fn start_native(control: Arc<NativeControl>) {
    let Some(handle) = control.handle.upgrade() else {
        return;
    };
    if handle.disposed.load(Ordering::Acquire) {
        return;
    }
    control.cleanup.lock().take();
    control.retry.lock().take();
    let event_control = control.clone();
    let listener = Arc::new(move |_event_type: String, filename: Option<String>| {
        event_control.retry_attempts.store(0, Ordering::Release);
        let Some(handle) = event_control.handle.upgrade() else {
            return;
        };
        let path = resolve_native_signal_path(&event_control.root, filename.as_deref());
        if path != event_control.root && ignored(&event_control.options, &path) {
            return;
        }
        handle.emit(invalidation(&event_control.root));
    });
    match control
        .runtime
        .watch_native(&control.root, listener, control.recursive)
    {
        Ok(watcher) => {
            let error_control = control.clone();
            watcher.on_error(Arc::new(move |error| {
                handle_native_error(error_control.clone(), error);
            }));
            *control.cleanup.lock() = Some(Box::new(move || watcher.close()));
            if control.recovering.swap(false, Ordering::AcqRel) {
                handle.emit(invalidation(&control.root));
            }
            handle.ready.settle(Ok(()));
        }
        Err(error) => handle_native_error(control, error),
    }
}

fn handle_native_error(control: Arc<NativeControl>, error: WatchError) {
    let Some(handle) = control.handle.upgrade() else {
        return;
    };
    control.cleanup.lock().take();
    if control.options.listing == Some(false) && error.code.as_deref() == Some("ENOENT") {
        control.recovering.store(true, Ordering::Release);
        handle.ready.settle(Ok(()));
        schedule_native_retry(control, LISTING_MISSING_RETRY_MS);
        return;
    }
    if error.code.as_deref() == Some("ERR_FEATURE_UNAVAILABLE_ON_PLATFORM") {
        control.recovering.store(false, Ordering::Release);
        handle.emit(invalidation(&control.root));
        start_polling(control, None);
        return;
    }
    control.recovering.store(true, Ordering::Release);
    let attempts = control.retry_attempts.fetch_add(1, Ordering::AcqRel);
    let delay = NATIVE_RETRY_BASE_MS
        .saturating_mul(1_u64.checked_shl(attempts).unwrap_or(u64::MAX))
        .min(NATIVE_RETRY_MAX_MS);
    control.runtime.report_error(&error);
    handle.emit(invalidation(&control.root));
    schedule_native_retry(control, delay);
}

fn schedule_native_retry(control: Arc<NativeControl>, delay_ms: u64) {
    let retry_control = control.clone();
    let callback = Arc::new(move || start_native(retry_control.clone()));
    *control.retry.lock() = Some(control.runtime.schedule_retry(callback, delay_ms));
}

fn start_polling(control: Arc<NativeControl>, depth: Option<usize>) {
    let Some(handle) = control.handle.upgrade() else {
        return;
    };
    let change_handle = handle.clone();
    let listener = Arc::new(move |change: WatchChange| {
        change_handle.emit(change);
    });
    match PollWatcher::start(
        &control.root,
        control.options.recursive != Some(false),
        depth,
        control.options.clone(),
        listener,
        false,
    ) {
        Ok(watcher) => {
            let error_handle = handle.clone();
            let error_runtime = control.runtime.clone();
            watcher.on_error(Arc::new(move |error| {
                error_runtime.report_error(&error);
                if error_handle.ready.result.lock().is_none() {
                    error_handle.ready.settle(Err(error));
                }
            }));
            handle.ready.settle(Ok(()));
            *control.cleanup.lock() = Some(Box::new(move || watcher.close()));
        }
        Err(error) => {
            control.runtime.report_error(&error);
            handle.ready.settle(Err(error));
        }
    }
}

#[derive(Clone)]
pub struct WatchService {
    runtime: Arc<dyn WatchRuntime>,
}

impl WatchService {
    pub fn watch(&self, path: &str) -> Box<WatchHandle> {
        self.watch_with_options(path, WatchOptions::default())
    }

    pub fn watch_with_options(&self, path: &str, mut options: WatchOptions) -> Box<WatchHandle> {
        if !is_watch_enabled() {
            let inner = Arc::new(HandleInner::new(Arc::new(|path| path.to_string())));
            inner.ready.settle(Ok(()));
            inner.disposed.store(true, Ordering::Release);
            return Box::new(WatchHandle::new(inner, || {}));
        }
        let watched = self.runtime.resolve_path(path).unwrap_or_else(|| path.to_string());
        let watched_for_map = watched.clone();
        let requested = path.to_string();
        let map_path = Arc::new(move |changed: &str| {
            requested_path(&watched_for_map, &requested, changed)
        });
        if let Some(original) = options.ignored.take() {
            let watched = watched.clone();
            let requested = path.to_string();
            options.ignored = Some(Arc::new(move |changed| {
                original(&requested_path(&watched, &requested, changed))
            }));
        }
        let inner = Arc::new(HandleInner::new(map_path));
        let native = options.listing == Some(false)
            || (options.signal == Some(true)
                && options.recursive != Some(false)
                && matches!(self.runtime.platform(), "darwin" | "win32"));
        let control = Arc::new(NativeControl {
            root: watched,
            options: options.clone(),
            runtime: self.runtime.clone(),
            handle: Arc::downgrade(&inner),
            recursive: options.listing != Some(false),
            recovering: AtomicBool::new(false),
            retry_attempts: AtomicU32::new(0),
            cleanup: Mutex::new(None),
            retry: Mutex::new(None),
        });
        if native {
            start_native(control.clone());
        } else {
            let depth = options.depth.or(if options.recursive == Some(false) {
                Some(0)
            } else {
                None
            });
            start_polling(control.clone(), depth);
        }
        Box::new(WatchHandle::new(inner, move || {
            control.cleanup.lock().take();
            control.retry.lock().take();
        }))
    }
}

pub fn create_watch_service(runtime: Arc<dyn WatchRuntime>) -> WatchService {
    WatchService { runtime }
}

static DEFAULT_SERVICE: LazyLock<WatchService> =
    LazyLock::new(|| create_watch_service(Arc::new(NodeWatchRuntime)));
static WATCH_ENABLED_FROM_CONFIG: AtomicBool = AtomicBool::new(true);

pub fn set_watch_enabled(enabled: bool) {
    WATCH_ENABLED_FROM_CONFIG.store(enabled, Ordering::Release);
}

fn js_trim(value: &str) -> &str {
    value.trim_matches(|character| {
        matches!(
            character,
            '\u{0009}'..='\u{000d}'
                | ' '
                | '\u{00a0}'
                | '\u{1680}'
                | '\u{2000}'..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
        )
    })
}

pub fn is_watch_enabled() -> bool {
    if let Ok(raw) = env::var(WATCH_ENV) {
        match js_trim(&raw).to_ascii_lowercase().as_str() {
            "0" | "false" | "no" | "off" => return false,
            "1" | "true" | "yes" | "on" => return true,
            _ => {}
        }
    }
    WATCH_ENABLED_FROM_CONFIG.load(Ordering::Acquire)
}

pub fn watch(path: &str) -> Box<WatchHandle> {
    DEFAULT_SERVICE.watch(path)
}

pub fn watch_with_options(path: &str, options: WatchOptions) -> Box<WatchHandle> {
    DEFAULT_SERVICE.watch_with_options(path, options)
}

struct CandidatePlan {
    paths: Vec<String>,
    notify: Vec<String>,
    pending: Vec<String>,
}

struct CandidateControl {
    root: String,
    candidates: Vec<String>,
    options: WatchOptions,
    handle: Weak<HandleInner>,
    plan_key: Mutex<String>,
    children: Mutex<Vec<(Box<WatchHandle>, Box<WatchSubscription>)>>,
    rebuild_lock: Mutex<()>,
}

impl CandidateControl {
    fn rebuild(self: &Arc<Self>, initial: bool) {
        let Some(handle) = self.handle.upgrade() else {
            return;
        };
        if handle.disposed.load(Ordering::Acquire) {
            return;
        }
        let _rebuilding = self.rebuild_lock.lock();
        let plan = plan_candidate_watches(&self.root, &self.candidates);
        let key = format!(
            "{}\0\0{}\0\0{}",
            plan.notify.join("\0"),
            plan.paths.join("\0"),
            plan.pending.join("\0")
        );
        if !initial && *self.plan_key.lock() == key {
            return;
        }
        *self.plan_key.lock() = key;
        let old = std::mem::take(&mut *self.children.lock());
        drop(old);
        let mut children = Vec::new();
        for path in plan.paths {
            self.push_child(&mut children, watch_with_options(&path, self.options.clone()));
        }
        for dir in plan.notify {
            if !watch_path_exists(&dir) {
                continue;
            }
            let candidates = self.candidates.clone();
            let candidate_dir = dir.clone();
            let original = self.options.ignored.clone();
            let mut options = self.options.clone();
            options.listing = Some(false);
            options.recursive = Some(false);
            options.ignored = Some(Arc::new(move |path| {
                !is_candidate_related(&candidate_dir, &candidates, path)
                    || original.as_ref().is_some_and(|ignored| ignored(path))
            }));
            self.push_child(&mut children, watch_with_options(&dir, options));
        }
        for path in plan.pending {
            let mut options = self.options.clone();
            options.listing = Some(false);
            self.push_child(&mut children, watch_with_options(&path, options));
        }
        let readiness = children
            .iter()
            .map(|(child, _)| child.inner.clone())
            .collect::<Vec<_>>();
        *self.children.lock() = children;
        if initial {
            let ready_handle = handle.clone();
            thread::spawn(move || {
                for child in readiness {
                    if let Err(error) = child.ready.wait() {
                        ready_handle.ready.settle(Err(error));
                        return;
                    }
                }
                ready_handle.ready.settle(Ok(()));
            });
        }
    }

    fn push_child(
        self: &Arc<Self>,
        children: &mut Vec<(Box<WatchHandle>, Box<WatchSubscription>)>,
        child: Box<WatchHandle>,
    ) {
        let weak = Arc::downgrade(self);
        let subscription = child.on_did_change(move |change| {
            let Some(control) = weak.upgrade() else {
                return;
            };
            control.rebuild(false);
            if let Some(handle) = control.handle.upgrade() {
                handle.emit(change);
            }
        });
        children.push((child, subscription));
    }
}

pub fn watch_candidates(
    root: &str,
    candidates: &[String],
    ignored: Option<WatchIgnored>,
) -> Box<WatchHandle> {
    watch_candidates_with_options(
        root,
        candidates,
        WatchOptions {
            ignored,
            ..WatchOptions::default()
        },
    )
}

pub fn watch_candidates_with_options(
    root: &str,
    candidates: &[String],
    options: WatchOptions,
) -> Box<WatchHandle> {
    let inner = Arc::new(HandleInner::new(Arc::new(|path| path.to_string())));
    if !is_watch_enabled() {
        inner.ready.settle(Ok(()));
        inner.disposed.store(true, Ordering::Release);
        return Box::new(WatchHandle::new(inner, || {}));
    }
    let control = Arc::new(CandidateControl {
        root: root.to_string(),
        candidates: candidates.to_vec(),
        options,
        handle: Arc::downgrade(&inner),
        plan_key: Mutex::new(String::new()),
        children: Mutex::new(Vec::new()),
        rebuild_lock: Mutex::new(()),
    });
    control.rebuild(true);
    Box::new(WatchHandle::new(inner, move || {
        std::mem::take(&mut *control.children.lock());
    }))
}

fn plan_candidate_watches(root: &str, candidates: &[String]) -> CandidatePlan {
    let mut paths = Vec::new();
    let mut notify = Vec::new();
    let mut pending = Vec::new();
    for candidate in candidates {
        if watch_path_exists(candidate) {
            if !paths.contains(candidate) {
                paths.push(candidate.clone());
            }
            continue;
        }
        let mut current = PathBuf::from(candidate)
            .parent()
            .unwrap_or(Path::new(root))
            .to_path_buf();
        while !watch_path_exists(&current.to_string_lossy())
            && !same_watch_path(&current.to_string_lossy(), root)
        {
            let Some(parent) = current.parent() else {
                break;
            };
            if parent == current {
                break;
            }
            current = parent.to_path_buf();
        }
        let current_text = current.to_string_lossy().into_owned();
        if watch_path_exists(&current_text) && !is_kimi_code_dir(&current_text) {
            if !notify.contains(&current_text) {
                notify.push(current_text);
            }
        } else if !pending.contains(candidate) {
            pending.push(candidate.clone());
        }
    }
    paths.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
    notify.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
    pending.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
    CandidatePlan {
        paths,
        notify,
        pending,
    }
}

fn is_kimi_code_dir(path: &str) -> bool {
    let name = Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    if cfg!(windows) {
        name.to_lowercase() == ".kimi-code"
    } else {
        name == ".kimi-code"
    }
}

fn is_candidate_related(root: &str, candidates: &[String], path: &str) -> bool {
    for candidate in candidates {
        if same_watch_path(path, candidate)
            || is_path_inside(path, candidate)
            || is_path_inside(candidate, path)
        {
            return true;
        }
        if let Some(segment) = first_path_segment(root, candidate) {
            let basename = Path::new(path)
                .file_name()
                .map(|name| name.to_string_lossy())
                .unwrap_or_default();
            if basename == segment
                || path.ends_with(&format!("{}{}", std::path::MAIN_SEPARATOR, segment))
            {
                return true;
            }
        }
    }
    false
}

fn first_path_segment(root: &str, candidate: &str) -> Option<String> {
    let relative = pathdiff(Path::new(root), Path::new(candidate))?;
    if relative.as_os_str().is_empty() || is_outside(&relative) {
        return None;
    }
    relative
        .components()
        .next()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
}

fn is_path_inside(path: &str, parent: &str) -> bool {
    pathdiff(Path::new(parent), Path::new(path))
        .is_some_and(|relative| !relative.as_os_str().is_empty() && !is_outside(&relative))
}

fn same_watch_path(left: &str, right: &str) -> bool {
    let normalize = |path: &str| path.replace('\\', "/").trim_end_matches('/').to_string();
    let left = normalize(left);
    let right = normalize(right);
    if cfg!(windows) {
        left.to_lowercase() == right.to_lowercase()
    } else {
        left == right
    }
}

fn watch_path_exists(path: &str) -> bool {
    fs::metadata(path).is_ok()
}

fn requested_path(watched: &str, requested: &str, changed: &str) -> String {
    if watched == requested {
        return changed.to_string();
    }
    let Some(relative) = pathdiff(Path::new(watched), Path::new(changed)) else {
        return changed.to_string();
    };
    if relative.as_os_str().is_empty() {
        return requested.to_string();
    }
    if is_outside(&relative) {
        return changed.to_string();
    }
    Path::new(requested)
        .join(relative)
        .to_string_lossy()
        .into_owned()
}

fn resolve_native_signal_path(root: &str, filename: Option<&str>) -> String {
    let root_name = Path::new(root).file_name().map(|name| name.to_string_lossy());
    if filename.is_none_or(|name| name.is_empty() || Some(name.into()) == root_name) {
        return root.to_string();
    }
    let filename = filename.unwrap();
    let path = PathBuf::from(filename);
    let absolute = if path.is_absolute() {
        path
    } else {
        Path::new(root).join(path)
    };
    let Some(relative) = pathdiff(Path::new(root), &absolute) else {
        return root.to_string();
    };
    if is_outside(&relative) {
        root.to_string()
    } else {
        absolute.to_string_lossy().into_owned()
    }
}

fn is_outside(path: &Path) -> bool {
    path.is_absolute()
        || path
            .components()
            .next()
            .is_some_and(|component| component.as_os_str() == "..")
}

fn pathdiff(base: &Path, path: &Path) -> Option<PathBuf> {
    let base = base.components().collect::<Vec<_>>();
    let path = path.components().collect::<Vec<_>>();
    let common = base
        .iter()
        .zip(path.iter())
        .take_while(|(left, right)| left == right)
        .count();
    if common == 0 && !base.is_empty() && !path.is_empty() {
        return None;
    }
    let mut result = PathBuf::new();
    for _ in common..base.len() {
        result.push("..");
    }
    for component in &path[common..] {
        result.push(component.as_os_str());
    }
    Some(result)
}
