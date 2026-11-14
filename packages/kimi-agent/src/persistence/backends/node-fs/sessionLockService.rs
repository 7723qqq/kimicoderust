use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{Value, json};

use crate::_base::di::lifecycle::{IDisposable};
use crate::_base::log::log::ILogService;
use crate::_base::utils::env::parse_boolean_env;
use crate::app::bootstrap::bootstrap::IBootstrapService;
use crate::persistence::interface::session_lock::{
    ISessionLockService, SessionLockHeldEvent, SessionLockHolder, SessionLockHolds,
    SessionLockHoldsBackend, SessionLockLease, session_occupied_error,
};
use crate::wire::record::WireAsyncError;

const LOCK_FILE_VERSION: u64 = 1;
pub const SESSION_LOCK_ENV: &str = "KIMI_CODE_SESSION_LOCK";

pub struct SessionLockService {
    holds: SessionLockHolds,
}

impl SessionLockService {
    pub fn new(bootstrap: Arc<dyn IBootstrapService>, log: Arc<dyn ILogService>) -> Self {
        let identity = bootstrap.client_identity();
        let client_identity = if identity.platform.is_empty() {
            identity.product_name.clone()
        } else {
            identity.platform.clone()
        };
        let client_display_name = if client_identity == "kimi_code_cli"
            && identity.user_agent_suffix.as_deref() == Some("web")
        {
            Some("Kimi Code Web".to_string())
        } else {
            match client_identity.as_str() {
                "kimi_code_cli" => Some("Kimi Code CLI".to_string()),
                "kimi_code_desktop" => Some("Kimi Code Desktop".to_string()),
                "kimi_code_vscode" => Some("Kimi Code for VS Code".to_string()),
                _ => None,
            }
        };
        let backend = Arc::new(NodeFsSessionLockBackend {
            lock_dir: Path::new(&bootstrap.home_dir()).join("locks").join("sessions"),
            hostname: hostname(),
            client_identity,
            client_display_name,
            disabled: parse_boolean_env(bootstrap.get_env(SESSION_LOCK_ENV).as_deref())
                == Some(false),
            lock_dir_ready: AtomicBool::new(false),
            log,
        });
        Self {
            holds: SessionLockHolds::new(backend),
        }
    }

    pub fn release_all(&self) {
        self.holds.release_all();
    }
}

impl ISessionLockService for SessionLockService {
    fn on_did_hold(
        &self,
        listener: Arc<dyn Fn(SessionLockHeldEvent) + Send + Sync>,
    ) -> Box<dyn IDisposable> {
        self.holds.on_did_hold(listener)
    }

    fn hold(&self, session_id: &str) -> Result<(), WireAsyncError> {
        self.holds.hold(session_id)
    }

    fn try_hold(&self, session_id: &str) -> Result<bool, WireAsyncError> {
        self.holds.try_hold(session_id)
    }

    fn release(&self, session_id: &str) {
        self.holds.release(session_id);
    }

    fn lease(&self, session_id: &str) -> Result<SessionLockLease, WireAsyncError> {
        self.holds.lease(session_id)
    }

    fn held(&self, session_id: &str) -> bool {
        self.holds.held(session_id)
    }

    fn takeover(&self, session_id: &str) -> Result<(), WireAsyncError> {
        self.holds.takeover(session_id)
    }
}

struct NodeFsSessionLockBackend {
    lock_dir: PathBuf,
    hostname: String,
    client_identity: String,
    client_display_name: Option<String>,
    disabled: bool,
    lock_dir_ready: AtomicBool,
    log: Arc<dyn ILogService>,
}

enum ClaimOutcome {
    Claimed,
    Occupied(Option<SessionLockHolder>),
}

impl NodeFsSessionLockBackend {
    fn claim(&self, lock_path: &Path, token: &str) -> Result<ClaimOutcome, WireAsyncError> {
        self.ensure_lock_dir()?;
        for _ in 0..2 {
            if self.try_publish(lock_path, &self.payload(token))? {
                return Ok(ClaimOutcome::Claimed);
            }
            let current = self.read_holder(lock_path)?;
            if current.as_ref().is_some_and(|current| self.alive(current)) {
                return Ok(ClaimOutcome::Occupied(current));
            }
            self.reclaim(lock_path, token)?;
        }
        Ok(ClaimOutcome::Occupied(self.read_holder(lock_path)?))
    }

    fn reclaim(&self, lock_path: &Path, token: &str) -> Result<(), WireAsyncError> {
        let reclaim_path = suffixed_path(lock_path, ".reclaim");
        if !self.try_publish(&reclaim_path, &self.payload(token))? {
            let current = self.read_holder(&reclaim_path)?;
            if current.as_ref().is_none_or(|current| !self.alive(current)) {
                unlink_silent(&reclaim_path);
            }
            return Ok(());
        }
        let result = (|| {
            let current = self.read_holder(lock_path)?;
            if current.as_ref().is_none_or(|current| !self.alive(current)) {
                unlink_silent(lock_path);
            }
            Ok(())
        })();
        unlink_silent(&reclaim_path);
        result
    }

    fn try_publish(&self, lock_path: &Path, body: &str) -> Result<bool, WireAsyncError> {
        let temporary = suffixed_path(
            lock_path,
            &format!(".tmp-{}-{}", std::process::id(), uuid::Uuid::new_v4()),
        );
        let result = (|| {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temporary)?;
            std::io::Write::write_all(&mut file, body.as_bytes())?;
            drop(file);
            match std::fs::hard_link(&temporary, lock_path) {
                Ok(()) => Ok(true),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
                Err(error) => Err(Box::new(error) as WireAsyncError),
            }
        })();
        unlink_silent(&temporary);
        result
    }

    fn read_holder(&self, lock_path: &Path) -> Result<Option<SessionLockHolder>, WireAsyncError> {
        let raw = match std::fs::read(lock_path) {
            Ok(raw) => String::from_utf8_lossy(&raw).into_owned(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(Box::new(error)),
        };
        let parsed = match serde_json::from_str::<Value>(&raw) {
            Ok(parsed) => parsed,
            Err(_) => return Ok(Some(unreadable_holder())),
        };
        Ok(Some(SessionLockHolder {
            version: parsed
                .get("version")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            token: parsed
                .get("token")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            pid: parsed.get("pid").and_then(Value::as_u64).unwrap_or(0) as u32,
            hostname: parsed
                .get("hostname")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            started_at: parsed
                .get("startedAt")
                .and_then(Value::as_i64)
                .unwrap_or(0),
            client_identity: parsed
                .get("clientIdentity")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string(),
            client_display_name: parsed
                .get("clientDisplayName")
                .and_then(Value::as_str)
                .map(str::to_string),
        }))
    }

    fn alive(&self, holder: &SessionLockHolder) -> bool {
        if !holder.hostname.is_empty() && holder.hostname != self.hostname {
            return true;
        }
        if !pid_alive(holder.pid) {
            return false;
        }
        boot_time_ms().is_none_or(|boot_time| holder.started_at >= boot_time)
    }

    fn payload(&self, token: &str) -> String {
        serde_json::to_string(&SessionLockHolder {
            version: LOCK_FILE_VERSION,
            token: token.to_string(),
            pid: std::process::id(),
            hostname: self.hostname.clone(),
            started_at: now_ms(),
            client_identity: self.client_identity.clone(),
            client_display_name: self.client_display_name.clone(),
        })
        .unwrap()
    }

    fn ensure_lock_dir(&self) -> Result<(), WireAsyncError> {
        if self.lock_dir_ready.load(Ordering::Relaxed) {
            return Ok(());
        }
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&self.lock_dir)?;
        self.lock_dir_ready.store(true, Ordering::Relaxed);
        Ok(())
    }

    fn lock_path(&self, session_id: &str) -> PathBuf {
        self.lock_dir.join(format!("{session_id}.lock"))
    }
}

impl SessionLockHoldsBackend for NodeFsSessionLockBackend {
    fn publish(&self, session_id: &str) -> Result<String, WireAsyncError> {
        let token = format!("{}:{}", std::process::id(), uuid::Uuid::new_v4());
        if self.disabled {
            return Ok(token);
        }
        match self.claim(&self.lock_path(session_id), &token) {
            Ok(ClaimOutcome::Claimed) => Ok(token),
            Ok(ClaimOutcome::Occupied(holder)) => {
                Err(Box::new(session_occupied_error(session_id, holder.as_ref())))
            }
            Err(error) => {
                self.log.warn(
                    "session lock unavailable; continuing without lock",
                    Some(json!({ "sessionId": session_id, "error": error.to_string() })),
                );
                Ok(token)
            }
        }
    }

    fn unpublish(&self, session_id: &str, token: &str) {
        let lock_path = self.lock_path(session_id);
        if self
            .read_holder(&lock_path)
            .ok()
            .flatten()
            .is_some_and(|holder| holder.token == token)
        {
            unlink_silent(&lock_path);
        }
    }

    fn evict(&self, session_id: &str) -> Result<(), WireAsyncError> {
        let lock_path = self.lock_path(session_id);
        let Some(holder) = self.read_holder(&lock_path)? else {
            return Ok(());
        };
        self.log.warn(
            "taking over session lock",
            Some(json!({ "sessionId": session_id, "holder": holder })),
        );
        match std::fs::remove_file(lock_path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(Box::new(error)),
        }
    }
}

fn unreadable_holder() -> SessionLockHolder {
    SessionLockHolder {
        version: 0,
        token: String::new(),
        pid: 0,
        hostname: String::new(),
        started_at: 0,
        client_identity: "unknown".to_string(),
        client_display_name: None,
    }
}

fn suffixed_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn unlink_silent(path: &Path) {
    let _ = std::fs::remove_file(path);
}

fn hostname() -> String {
    whoami::fallible::hostname().unwrap_or_default()
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    let alive = unsafe { libc::kill(pid as i32, 0) } == 0
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    if !alive {
        return false;
    }
    match std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "stat="])
        .output()
    {
        Ok(output) => {
            !output.status.success()
                || !String::from_utf8_lossy(&output.stdout)
                    .trim_start()
                    .starts_with('Z')
        }
        Err(_) => true,
    }
}

#[cfg(windows)]
fn pid_alive(pid: u32) -> bool {
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const ERROR_ACCESS_DENIED: u32 = 5;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
        fn CloseHandle(handle: isize) -> i32;
        fn GetLastError() -> u32;
    }
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle != 0 {
            CloseHandle(handle);
            true
        } else {
            GetLastError() == ERROR_ACCESS_DENIED
        }
    }
}

#[cfg(windows)]
fn boot_time_ms() -> Option<i64> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetTickCount64() -> u64;
    }
    Some(now_ms() - unsafe { GetTickCount64() } as i64)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn boot_time_ms() -> Option<i64> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) } != 0 {
        return None;
    }
    Some(now_ms() - (ts.tv_sec * 1000 + ts.tv_nsec / 1_000_000))
}

#[cfg(target_os = "macos")]
fn boot_time_ms() -> Option<i64> {
    let mut mib = [libc::CTL_KERN, libc::KERN_BOOTTIME];
    let mut tv = libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };
    let mut size = std::mem::size_of_val(&tv);
    let result = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            2,
            &mut tv as *mut _ as *mut _,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 {
        return None;
    }
    Some(now_ms() - (tv.tv_sec * 1000 + tv.tv_usec / 1000))
}
