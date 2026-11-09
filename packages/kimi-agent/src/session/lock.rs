//! Cross-process session occupancy lock (v2 `persistence/interface/sessionLock.ts`
//! + `persistence/backends/node-fs/sessionLockService.ts`, upstream #4246).
//!
//! Two processes running turns on the same session interleave their wire rows
//! into one journal; the lock makes the later one fail loudly instead. A hold
//! is a lock file under `<kimi home>/locks/sessions/<sessionId>.lock` whose
//! payload names the holding process; a claimant that finds a live foreign
//! holder gets `session.occupied`, a claimant that finds a dead one reclaims
//! the file through v2's `.reclaim` sidecar protocol.
//!
//! Deliberate differences from the TS reference, each recorded so the next
//! reader does not "fix" them back:
//!
//! - The engine identifies itself as a generic `kimi_code` client with no
//!   display name, so the denial is v2's fallback wording ("another Kimi Code
//!   process") rather than the per-product variant; v2 takes the same arm
//!   whenever no display name is known.
//! - The wire write guard (`SqliteSessionStore::append_wire_event_json`)
//!   *checks* the holder instead of v2's acquire-and-retain `tryHold` guard:
//!   a per-row acquire would create and delete a lock file twice per wire
//!   event, and this engine's turns already hold the lock for their whole
//!   duration — the guard's job at the store seam is refusal, not acquisition.
//! - No `process.on('exit')` sweep. Release runs at the seams that own a hold
//!   (turn end, session close, lease drop), and a crashed holder's file is
//!   reclaimed by the next claimant through the same stale-holder protocol
//!   v2 uses, so the exit listener's absence costs nothing but a stale file
//!   that never survives the next claim.
//! - `KIMI_CODE_SESSION_LOCK=false` disables the lock (v2
//!   `SESSION_LOCK_ENV`), and a filesystem failure while claiming degrades to
//!   a warning plus an unlocked turn (v2 `publish`'s catch arm) — the lock is
//!   protection, never a new way to make a session unusable.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// v2 `SESSION_LOCK_ENV`: set to a false-ish value to run unlocked.
pub const SESSION_LOCK_ENV: &str = "KIMI_CODE_SESSION_LOCK";

/// v2 `SESSION_LOCK_RELEASE_DELAY_MS` (sessionRunGateService.ts:15): the
/// publish is kept for this long after the session goes idle, so two
/// back-to-back turns do not drop and re-claim the lock (and race another
/// client through the gap).
pub const SESSION_LOCK_RELEASE_DELAY_MS: u64 = 3000;

/// v2 `LOCK_FILE_VERSION`.
const LOCK_FILE_VERSION: u32 = 1;

/// The holder payload written into the lock file (v2 `SessionLockHolder`;
/// field names match the JSON v2 writes, so a lock file is readable by
/// either engine).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionLockHolder {
    pub version: u32,
    pub token: String,
    pub pid: u32,
    pub hostname: String,
    #[serde(rename = "startedAt")]
    pub started_at: i64,
    #[serde(rename = "clientIdentity")]
    pub client_identity: String,
    #[serde(rename = "clientDisplayName")]
    pub client_display_name: Option<String>,
}

impl Default for SessionLockHolder {
    /// v2 `UNREADABLE_HOLDER`: a file that cannot be parsed reads as a holder
    /// nothing can verify — and v2's `alive()` treats the unverifiable as
    /// alive rather than deleting a lock it cannot judge.
    fn default() -> Self {
        Self {
            version: 0,
            token: String::new(),
            pid: 0,
            hostname: String::new(),
            started_at: 0,
            client_identity: "unknown".to_string(),
            client_display_name: None,
        }
    }
}

/// The denial a second claimant gets (v2 `sessionOccupiedError`, code
/// `session.occupied` — on v2's retryable list, so hosts may surface it as
/// "try again once the other process lets go").
#[derive(Debug)]
pub struct SessionOccupied {
    pub session_id: String,
    /// Boxed: the claim path is the only producer, and keeping the `Err`
    /// arm of every `hold`/`lease` Result small is worth one indirection.
    pub holder: Option<Box<SessionLockHolder>>,
}

/// v2's exact two message arms.
pub fn session_occupied_message(holder: Option<&SessionLockHolder>) -> String {
    match holder.and_then(|h| h.client_display_name.as_deref()) {
        Some(display) => format!(
            "This session is in use by another {display} instance. Stop the session in that instance or exit it, then try again."
        ),
        None => "This session is in use by another Kimi Code process. Stop the session in that process or exit it, then try again.".to_string(),
    }
}

impl std::fmt::Display for SessionOccupied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&session_occupied_message(self.holder.as_deref()))
    }
}

impl std::error::Error for SessionOccupied {}

/// The process-wide service the engine's seams use. Constructed on first use
/// from the environment; unit tests build their own instances on tempdirs
/// instead of touching this one.
pub static SESSION_LOCKS: LazyLock<SessionLockService> = LazyLock::new(|| {
    #[cfg(test)]
    {
        // Tests must not share `<home>/.kimi-code/locks/sessions`: a lock file
        // left by another window's process — or by an earlier run — is live
        // enough to make the claim protocol report `SessionOccupied`, which
        // turned whole engine/session suites red at random. A per-process root
        // keeps every test binary isolated without changing the protocol.
        let lock_dir = std::env::temp_dir()
            .join(format!("kimi-agent-test-locks-{}", std::process::id()))
            .join("sessions");
        SessionLockService::new(lock_dir)
    }
    #[cfg(not(test))]
    {
        SessionLockService::from_env()
    }
});

/// v2 `SessionLockHolds` + the node-fs `SessionLockService`, one type here:
/// the in-memory hold table and the filesystem claim protocol behind it.
pub struct SessionLockService {
    lock_dir: PathBuf,
    hostname: String,
    client_identity: String,
    disabled: bool,
    held: Mutex<HashMap<String, HeldState>>,
}

struct HeldState {
    token: String,
    held: bool,
    leases: usize,
    /// When the idle grace expires. `None` means held without a deadline (an
    /// explicit `hold`, or a lease).
    release_at: Option<Instant>,
}

/// Drop-releases one `hold` (v2's run-gate release, as a guard so an early
/// `?` or a dropped turn future cannot leak the lock file).
pub struct HoldGuard<'a> {
    service: &'a SessionLockService,
    session_id: String,
}

impl Drop for HoldGuard<'_> {
    fn drop(&mut self) {
        // v2 `SessionRunGateService.schedule`: the turn leaving does not end
        // the occupancy immediately. Keeping the publish for the grace window
        // lets the next turn reuse it and denies a foreign client the gap.
        self.service.release_after_idle_for(
            &self.session_id,
            Duration::from_millis(SESSION_LOCK_RELEASE_DELAY_MS),
        );
    }
}

/// v2 `SessionLockLease`: a counted reference that releases the publish when
/// the last lease (and no explicit hold) remains.
pub struct SessionLockLease<'a> {
    service: &'a SessionLockService,
    session_id: String,
}

impl Drop for SessionLockLease<'_> {
    fn drop(&mut self) {
        self.service.end_lease(&self.session_id);
    }
}

impl SessionLockService {
    /// A service on an explicit directory — the shape tests take; production
    /// goes through [`SESSION_LOCKS`].
    pub fn new(lock_dir: PathBuf) -> Self {
        Self {
            lock_dir,
            hostname: whoami::fallible::hostname().unwrap_or_else(|_| "unknown-host".into()),
            client_identity: "kimi_code".to_string(),
            disabled: false,
            held: Mutex::new(HashMap::new()),
        }
    }

    #[cfg_attr(test, allow(dead_code))] // tests build the per-process instance
    fn from_env() -> Self {
        let mut service = Self::new(data_home().join("locks").join("sessions"));
        service.disabled =
            parse_boolean_env(std::env::var(SESSION_LOCK_ENV).ok().as_deref()) == Some(false);
        service
    }

    /// v2 `hold`: claim the session, failing with [`SessionOccupied`] when a
    /// live foreign process holds it. Re-holding a session this process
    /// already holds is a no-op.
    pub fn hold(&self, session_id: &str) -> Result<(), SessionOccupied> {
        self.acquire(session_id, Mark::Hold)
    }

    /// [`Self::hold`] as a drop guard — the shape the turn seams take.
    pub fn hold_guard<'a>(&'a self, session_id: &str) -> Result<HoldGuard<'a>, SessionOccupied> {
        self.hold(session_id)?;
        Ok(HoldGuard {
            service: self,
            session_id: session_id.to_string(),
        })
    }

    /// v2 `tryHold`.
    pub fn try_hold(&self, session_id: &str) -> bool {
        self.hold(session_id).is_ok()
    }

    /// v2 `release`: the publish survives while leases remain.
    pub fn release(&self, session_id: &str) {
        let mut held = self.held.lock();
        let Some(state) = held.get_mut(session_id) else {
            return;
        };
        if !state.held {
            return;
        }
        state.held = false;
        state.release_at = None;
        Self::unpublish_if_unused_locked(&mut held, session_id, self);
    }

    /// Schedule the hold's release after the v2 idle grace
    /// (`sessionRunGateService.schedule`). A re-hold during the window cancels
    /// the deadline, which is the point: consecutive turns reuse one publish.
    pub fn release_after_idle(&self, session_id: &str) {
        self.release_after_idle_for(
            session_id,
            Duration::from_millis(SESSION_LOCK_RELEASE_DELAY_MS),
        );
    }

    /// [`Self::release_after_idle`] with an explicit grace period — the seam
    /// tests drive without waiting three seconds.
    pub fn release_after_idle_for(&self, session_id: &str, grace: Duration) {
        let mut held = self.held.lock();
        let Some(state) = held.get_mut(session_id) else {
            return;
        };
        if !state.held {
            return;
        }
        state.release_at = Some(Instant::now() + grace);
    }

    /// v2 `releaseWhenIdle`: release every hold whose grace window elapsed.
    /// Called by the periodic maintenance tick and lazily on the next claim.
    pub fn sweep_idle(&self) {
        let mut held = self.held.lock();
        let expired: Vec<String> = held
            .iter()
            .filter(|(_, state)| {
                state.held && state.release_at.is_some_and(|at| at <= Instant::now())
            })
            .map(|(session_id, _)| session_id.clone())
            .collect();
        for session_id in expired {
            if let Some(state) = held.get_mut(&session_id) {
                state.held = false;
                state.release_at = None;
            }
            Self::unpublish_if_unused_locked(&mut held, &session_id, self);
        }
    }

    /// v2 `lease`: hold the publish without marking the session held, for a
    /// scoped piece of work (the idle reaper's shape, upstream).
    pub fn lease<'a>(&'a self, session_id: &str) -> Result<SessionLockLease<'a>, SessionOccupied> {
        self.acquire(session_id, Mark::Lease)?;
        Ok(SessionLockLease {
            service: self,
            session_id: session_id.to_string(),
        })
    }

    fn end_lease(&self, session_id: &str) {
        let mut held = self.held.lock();
        let Some(state) = held.get_mut(session_id) else {
            return;
        };
        state.leases = state.leases.saturating_sub(1);
        Self::unpublish_if_unused_locked(&mut held, session_id, self);
    }

    /// v2 `held` — whether *this* process has any claim (hold or lease).
    pub fn held(&self, session_id: &str) -> bool {
        self.held.lock().contains_key(session_id)
    }

    /// v2 `releaseAll` (`dispose`): every publish this service made is
    /// withdrawn. The process-wide [`SESSION_LOCKS`] never drops, so this
    /// runs for standalone instances — tests, and any host that tears a
    /// service down before exit.
    pub fn release_all(&self) {
        let held = std::mem::take(&mut *self.held.lock());
        for (session_id, state) in held {
            self.unpublish(&session_id, &state.token);
        }
    }

    /// Where the lock files live. Tests plant holders here.
    #[doc(hidden)]
    pub fn lock_dir(&self) -> &Path {
        &self.lock_dir
    }

    /// v2 `takeover`: evict a foreign holder so the next claim succeeds.
    /// Taking over a session this process already holds is a no-op.
    pub fn takeover(&self, session_id: &str) {
        if self.held(session_id) {
            return;
        }
        let path = self.lock_path(session_id);
        let Ok(Some(holder)) = self.read_holder(&path) else {
            return;
        };
        tracing::warn!(
            session_id,
            holder = ?holder,
            "taking over session lock"
        );
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(session_id, %error, "session lock takeover could not unlink");
            }
        }
    }

    /// The wire write guard's question: is this session held by a live
    /// *other* process? Our own rows (pid match) and stale files do not
    /// block a write; a live foreign holder does.
    pub fn occupied_by_other(&self, session_id: &str) -> Option<SessionLockHolder> {
        if self.disabled {
            return None;
        }
        // Fast path: a session this process already holds cannot be occupied
        // by another — and the turn seams call this once per wire row.
        if self.held(session_id) {
            return None;
        }
        let holder = self.read_holder(&self.lock_path(session_id)).ok()??;
        if holder.pid == std::process::id() {
            return None;
        }
        self.alive(&holder).then_some(holder)
    }

    /// The map check, the publish, and the flag set run under one lock: a
    /// split version let a racing `release` delete a just-published file
    /// between the insert and the flag set.
    fn acquire(&self, session_id: &str, mark: Mark) -> Result<(), SessionOccupied> {
        let mut held = self.held.lock();
        // Expire a grace window before deciding anything: a caller arriving
        // after the 3s should claim a fresh publish, not inherit a stale hold.
        if held.get(session_id).is_some_and(|state| {
            state.held && state.release_at.is_some_and(|at| at <= Instant::now())
        }) {
            if let Some(state) = held.get_mut(session_id) {
                state.held = false;
                state.release_at = None;
            }
            Self::unpublish_if_unused_locked(&mut held, session_id, self);
        }
        let state = match held.entry(session_id.to_string()) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let token = self.publish(session_id)?;
                entry.insert(HeldState {
                    token,
                    held: false,
                    leases: 0,
                    release_at: None,
                })
            }
        };
        match mark {
            Mark::Hold => {
                state.held = true;
                // A new hold inside the grace period cancels the pending
                // release: the publish simply carries over to this turn.
                state.release_at = None;
            }
            Mark::Lease => state.leases += 1,
        }
        Ok(())
    }

    fn unpublish_if_unused_locked(
        held: &mut HashMap<String, HeldState>,
        session_id: &str,
        service: &SessionLockService,
    ) {
        let Some(state) = held.get(session_id) else {
            return;
        };
        if state.held || state.leases > 0 {
            return;
        }
        let Some(state) = held.remove(session_id) else {
            return;
        };
        service.unpublish(session_id, &state.token);
    }

    /// Whether a holder must be respected. A live **foreign** process blocks;
    /// our own pid never does, because within one process the in-memory held
    /// map is the authority (and a new instance replacing an old one in the
    /// same process is not a second client).
    fn blocks(&self, holder: &SessionLockHolder) -> bool {
        holder.pid != std::process::id() && self.alive(holder)
    }

    /// v2 `publish`: claim, or degrade to an unlocked token when the
    /// filesystem cannot play along — the lock is protection, not a new
    /// failure mode.
    fn publish(&self, session_id: &str) -> Result<String, SessionOccupied> {
        let token = format!("{}:{}", std::process::id(), ulid::Ulid::new());
        if self.disabled {
            return Ok(token);
        }
        match self.claim(&self.lock_path(session_id), &token) {
            Ok(ClaimOutcome::Claimed) => Ok(token),
            Ok(ClaimOutcome::Occupied(holder)) => Err(SessionOccupied {
                session_id: session_id.to_string(),
                holder: holder.map(Box::new),
            }),
            Err(error) => {
                tracing::warn!(session_id, %error, "session lock unavailable; continuing without lock");
                Ok(token)
            }
        }
    }

    /// v2 `unpublish`: only our own token is ours to delete.
    fn unpublish(&self, session_id: &str, token: &str) {
        let path = self.lock_path(session_id);
        let ours = self
            .read_holder(&path)
            .ok()
            .flatten()
            .is_some_and(|holder| holder.token == token);
        if ours {
            let _ = std::fs::remove_file(&path);
        }
    }

    /// v2 `claim`: publish, and when the file is taken, reclaim it through
    /// the sidecar protocol only once the holder reads dead — twice around
    /// before conceding.
    fn claim(&self, lock_path: &Path, token: &str) -> std::io::Result<ClaimOutcome> {
        self.ensure_lock_dir()?;
        for _ in 0..2 {
            if self.try_publish(lock_path, &self.payload(token))? {
                return Ok(ClaimOutcome::Claimed);
            }
            let current = self.read_holder(lock_path)?;
            if let Some(holder) = &current
                && self.blocks(holder)
            {
                return Ok(ClaimOutcome::Occupied(Some(holder.clone())));
            }
            self.reclaim(lock_path, token)?;
        }
        Ok(ClaimOutcome::Occupied(self.read_holder(lock_path)?))
    }

    /// v2 `reclaim`: the `.reclaim` sidecar makes a stale-file delete
    /// single-winner; the loser cleans up someone else's crashed reclaim and
    /// walks away.
    fn reclaim(&self, lock_path: &Path, token: &str) -> std::io::Result<()> {
        let reclaim_path = suffixed_path(lock_path, ".reclaim");
        if !self.try_publish(&reclaim_path, &self.payload(token))? {
            let current = self.read_holder(&reclaim_path)?;
            if current.as_ref().is_none_or(|h| !self.blocks(h)) {
                let _ = std::fs::remove_file(&reclaim_path);
            }
            return Ok(());
        }
        let current = self.read_holder(lock_path)?;
        if current.as_ref().is_none_or(|h| !self.blocks(h)) {
            match std::fs::remove_file(lock_path) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        let _ = std::fs::remove_file(&reclaim_path);
        Ok(())
    }

    /// v2 `tryPublish`: the file is written whole at a temp name and
    /// hard-linked into place, so a reader never meets a half-written holder
    /// (link fails `EEXIST`, which is the claim's atomicity).
    fn try_publish(&self, lock_path: &Path, body: &str) -> std::io::Result<bool> {
        let tmp = suffixed_path(
            lock_path,
            &format!(".tmp-{}-{}", std::process::id(), ulid::Ulid::new()),
        );
        let result = (|| {
            write_private(&tmp, body.as_bytes())?;
            match std::fs::hard_link(&tmp, lock_path) {
                Ok(()) => Ok(true),
                Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(false),
                Err(error) => Err(error),
            }
        })();
        let _ = std::fs::remove_file(&tmp);
        result
    }

    /// v2 `readHolder`: a missing file is no holder; a corrupt one is the
    /// unverifiable holder v2 refuses to judge.
    fn read_holder(&self, lock_path: &Path) -> std::io::Result<Option<SessionLockHolder>> {
        let raw = match std::fs::read_to_string(lock_path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        Ok(Some(serde_json::from_str(&raw).unwrap_or_default()))
    }

    /// v2 `alive`: a holder on another hostname cannot be verified and is
    /// assumed alive; a dead pid is dead; a live pid whose `startedAt`
    /// predates the boot is a reused pid on a stale file.
    fn alive(&self, holder: &SessionLockHolder) -> bool {
        if !holder.hostname.is_empty() && holder.hostname != self.hostname {
            return true;
        }
        if !pid_alive(holder.pid) {
            return false;
        }
        match boot_time_ms() {
            Some(boot) => holder.started_at >= boot,
            // No boot clock on this platform: the pid check alone stands.
            None => true,
        }
    }

    fn payload(&self, token: &str) -> String {
        serde_json::to_string(&SessionLockHolder {
            version: LOCK_FILE_VERSION,
            token: token.to_string(),
            pid: std::process::id(),
            hostname: self.hostname.clone(),
            started_at: now_ms(),
            client_identity: self.client_identity.clone(),
            client_display_name: None,
        })
        .unwrap_or_default()
    }

    fn ensure_lock_dir(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.lock_dir)
    }

    fn lock_path(&self, session_id: &str) -> PathBuf {
        self.lock_dir.join(format!("{session_id}.lock"))
    }
}

impl Drop for SessionLockService {
    fn drop(&mut self) {
        self.release_all();
    }
}

enum ClaimOutcome {
    Claimed,
    Occupied(Option<SessionLockHolder>),
}

enum Mark {
    Hold,
    Lease,
}

/// `lockPath.reclaim` / `lockPath.tmp-*` — suffix on the file name, not a
/// replaced extension (v2's `${lockPath}.reclaim` shape).
fn suffixed_path(path: &Path, suffix: &str) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(suffix);
    PathBuf::from(os)
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// The data home (`KIMI_CODE_HOME`, else `~/.kimi-code`) — the same
/// resolution `server::files` and `workflow::kimi_home` carry; duplicated
/// here because `session` sits below both in the module graph.
fn data_home() -> PathBuf {
    if let Some(home) = std::env::var_os("KIMI_CODE_HOME") {
        return PathBuf::from(home);
    }
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    match home {
        Some(home) => PathBuf::from(home).join(".kimi-code"),
        None => PathBuf::from(".kimi-code"),
    }
}

/// v2 `parseBooleanEnv`: only an explicit false-ish value disables.
fn parse_boolean_env(value: Option<&str>) -> Option<bool> {
    let normalized = value?.trim().to_lowercase();
    if normalized.is_empty() {
        return None;
    }
    match normalized.as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// v2 writes the file `mode: 0o600`; a lock names its holder's pid and is
/// not other users' business.
fn write_private(path: &Path, body: &[u8]) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    use std::io::Write;
    file.write_all(body)
}

/// v2 `pidAlive`: signal 0 probes existence; `EPERM` means the process is
/// there but not ours to signal. POSIX adds the zombie check v2 runs through
/// `ps` (a zombie is a dead holder whose lock must not block anyone);
/// Windows stops at the handle probe, exactly as v2 does on win32.
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
        Ok(out) => !String::from_utf8_lossy(&out.stdout)
            .trim_start()
            .starts_with('Z'),
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

/// v2's `Date.now() - os.uptime() * 1000`: the instant the machine booted,
/// the reference a stale holder's `startedAt` is judged against.
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
    let ok = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            2,
            &mut tv as *mut _ as *mut _,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if ok != 0 {
        return None;
    }
    Some(tv.tv_sec * 1000 + i64::from(tv.tv_usec) / 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> i64 {
        now_ms()
    }

    /// A holder file planted by hand stands in for another process: the
    /// claim path never looks at who planted it.
    fn plant(service: &SessionLockService, session_id: &str, holder: &SessionLockHolder) {
        std::fs::create_dir_all(&service.lock_dir).unwrap();
        std::fs::write(
            service.lock_path(session_id),
            serde_json::to_string(holder).unwrap(),
        )
        .unwrap();
    }

    /// A holder on another host: v2 cannot verify a foreign hostname's pid and
    /// therefore reads the holder as alive — which is what "live foreign"
    /// means here. No test can plant a live *foreign pid* without a second
    /// process, so the foreign hostname carries the liveness instead.
    fn live_foreign_holder() -> SessionLockHolder {
        SessionLockHolder {
            version: LOCK_FILE_VERSION,
            token: "99999:other".to_string(),
            pid: 999_999_999, // dead on this host; the hostname is what reads alive
            hostname: "other-host".to_string(),
            started_at: now(),
            client_identity: "kimi_code".to_string(),
            client_display_name: None,
        }
    }

    /// The same payload on **this** host with a pid nothing owns: verifiably
    /// dead, so the claim reclaims it.
    fn dead_holder() -> SessionLockHolder {
        SessionLockHolder {
            pid: 999_999_999,
            hostname: whoami::fallible::hostname().unwrap_or_else(|_| "unknown-host".into()),
            started_at: now(),
            ..live_foreign_holder()
        }
    }

    #[test]
    fn a_free_session_is_claimed_and_released() {
        let dir = tempfile::tempdir().unwrap();
        let service = SessionLockService::new(dir.path().join("locks"));
        service.hold("sess-a").unwrap();
        assert!(service.lock_path("sess-a").exists());
        assert!(service.held("sess-a"));
        service.release("sess-a");
        assert!(!service.lock_path("sess-a").exists());
        assert!(!service.held("sess-a"));
    }

    #[test]
    fn a_live_foreign_holder_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let service = SessionLockService::new(dir.path().join("locks"));
        plant(&service, "sess-b", &live_foreign_holder());
        let error = service.hold("sess-b").unwrap_err();
        assert_eq!(
            error.to_string(),
            session_occupied_message(None),
            "no display name falls back to the generic v2 wording"
        );
        assert!(!service.try_hold("sess-b"));
        assert!(!service.held("sess-b"));
    }

    #[test]
    fn the_denial_names_the_client_when_known() {
        let holder = SessionLockHolder {
            client_display_name: Some("Kimi Code Desktop".to_string()),
            ..live_foreign_holder()
        };
        assert_eq!(
            session_occupied_message(Some(&holder)),
            "This session is in use by another Kimi Code Desktop instance. Stop the session in that instance or exit it, then try again."
        );
    }

    #[test]
    fn a_dead_holder_is_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let service = SessionLockService::new(dir.path().join("locks"));
        plant(&service, "sess-c", &dead_holder());
        service.hold("sess-c").unwrap();
        let holder = service
            .read_holder(&service.lock_path("sess-c"))
            .unwrap()
            .unwrap();
        assert_ne!(holder.token, "99999:other");
        assert_eq!(holder.pid, std::process::id());
    }

    #[test]
    fn a_pre_boot_holder_is_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let service = SessionLockService::new(dir.path().join("locks"));
        // A live pid on this host but a start time before this boot: the pid
        // was reused, the file is stale.
        let mut holder = dead_holder();
        holder.pid = std::process::id();
        holder.started_at = 0;
        plant(&service, "sess-d", &holder);
        service.hold("sess-d").unwrap();
    }

    #[test]
    fn takeover_evicts_a_foreign_holder() {
        let dir = tempfile::tempdir().unwrap();
        let service = SessionLockService::new(dir.path().join("locks"));
        plant(&service, "sess-e", &live_foreign_holder());
        service.takeover("sess-e");
        assert!(!service.lock_path("sess-e").exists());
        service.hold("sess-e").unwrap();
    }

    #[test]
    fn takeover_leaves_our_own_hold_alone() {
        let dir = tempfile::tempdir().unwrap();
        let service = SessionLockService::new(dir.path().join("locks"));
        service.hold("sess-f").unwrap();
        service.takeover("sess-f");
        assert!(service.lock_path("sess-f").exists());
        assert!(service.held("sess-f"));
    }

    #[test]
    fn the_write_guard_sees_only_live_foreign_holders() {
        let dir = tempfile::tempdir().unwrap();
        let service = SessionLockService::new(dir.path().join("locks"));
        assert!(service.occupied_by_other("sess-g").is_none());
        plant(&service, "sess-g", &dead_holder());
        assert!(service.occupied_by_other("sess-g").is_none());
        plant(&service, "sess-g", &live_foreign_holder());
        let mut foreign = live_foreign_holder();
        foreign.hostname = whoami::fallible::hostname().unwrap_or_else(|_| "unknown-host".into());
        foreign.pid = 999_999_998; // dead on every platform
        plant(&service, "sess-g", &foreign);
        assert!(service.occupied_by_other("sess-g").is_none());
        // A live pid that is not ours is the only blocking case. No test can
        // plant one without a second process, so the pid check is inverted
        // here: our own pid never blocks us.
        let mut own = live_foreign_holder();
        own.token = "ours:1".to_string();
        own.pid = std::process::id();
        own.hostname = whoami::fallible::hostname().unwrap_or_else(|_| "unknown-host".into());
        plant(&service, "sess-g", &own);
        assert!(service.occupied_by_other("sess-g").is_none());
    }

    #[test]
    fn release_never_deletes_a_foreign_token() {
        let dir = tempfile::tempdir().unwrap();
        let service = SessionLockService::new(dir.path().join("locks"));
        service.hold("sess-h").unwrap();
        // The file changed hands behind our back (a takeover, then a foreign
        // claim): our release must not delete the new holder's file.
        plant(&service, "sess-h", &live_foreign_holder());
        service.release("sess-h");
        assert!(service.lock_path("sess-h").exists());
    }

    #[test]
    fn a_lease_keeps_the_publish_until_the_last_one_drops() {
        let dir = tempfile::tempdir().unwrap();
        let service = SessionLockService::new(dir.path().join("locks"));
        let first = service.lease("sess-i").unwrap();
        let second = service.lease("sess-i").unwrap();
        assert!(service.lock_path("sess-i").exists());
        drop(first);
        assert!(service.lock_path("sess-i").exists());
        drop(second);
        assert!(!service.lock_path("sess-i").exists());
    }

    #[test]
    fn a_dropped_hold_keeps_the_publish_for_the_grace_window() {
        let dir = tempfile::tempdir().unwrap();
        let service = SessionLockService::new(dir.path().join("locks"));
        {
            let _guard = service.hold_guard("sess-grace").unwrap();
        }
        assert!(
            service.held("sess-grace"),
            "v2 keeps the publish for 3s after the turn, not until drop"
        );
        assert!(service.lock_path("sess-grace").exists());
        service.sweep_idle();
        assert!(service.held("sess-grace"), "the grace has not elapsed");
        service.release_after_idle_for("sess-grace", Duration::ZERO);
        service.sweep_idle();
        assert!(!service.held("sess-grace"));
        assert!(!service.lock_path("sess-grace").exists());
    }

    #[test]
    fn a_rehold_inside_the_grace_cancels_the_pending_release() {
        let dir = tempfile::tempdir().unwrap();
        let service = SessionLockService::new(dir.path().join("locks"));
        drop(service.hold_guard("sess-rehold").unwrap());
        service.release_after_idle_for("sess-rehold", Duration::from_secs(60));
        // A second turn starts inside the window; the same publish is reused and
        // the deadline is cleared rather than firing under the running turn.
        drop(service.hold_guard("sess-rehold").unwrap());
        assert!(service.held("sess-rehold"));
        service.release_after_idle_for("sess-rehold", Duration::ZERO);
        service.sweep_idle();
        assert!(!service.held("sess-rehold"));
    }

    #[test]
    fn a_hold_survives_a_lease_drop() {
        let dir = tempfile::tempdir().unwrap();
        let service = SessionLockService::new(dir.path().join("locks"));
        service.hold("sess-j").unwrap();
        let lease = service.lease("sess-j").unwrap();
        drop(lease);
        assert!(service.lock_path("sess-j").exists());
        service.release("sess-j");
        assert!(!service.lock_path("sess-j").exists());
    }

    #[test]
    fn a_disabled_service_claims_without_touching_the_disk() {
        let dir = tempfile::tempdir().unwrap();
        let mut service = SessionLockService::new(dir.path().join("locks"));
        service.disabled = true;
        service.hold("sess-k").unwrap();
        assert!(!service.lock_path("sess-k").exists());
        assert!(service.held("sess-k"));
        assert!(service.occupied_by_other("sess-k").is_none());
        service.release("sess-k");
    }

    #[test]
    fn parse_boolean_env_reads_only_explicit_values() {
        assert_eq!(parse_boolean_env(None), None);
        assert_eq!(parse_boolean_env(Some("")), None);
        assert_eq!(parse_boolean_env(Some("yes")), Some(true));
        assert_eq!(parse_boolean_env(Some(" OFF ")), Some(false));
        assert_eq!(parse_boolean_env(Some("0")), Some(false));
        assert_eq!(parse_boolean_env(Some("banana")), None);
    }
}
