//! Process-wide fixtures the test suite has to share.
//!
//! The crate's tests run in one process, so whatever they change globally is
//! shared whether they mean it or not. The process cwd is the dangerous one:
//! while one test holds the cwd inside its temp workspace, every other test
//! that spawns a child inherits that directory, and a restore that lands on a
//! directory another test already deleted makes `CreateProcess` fail with
//! "access denied" for the rest of the run — dozens of tests away from the
//! culprit.
//!
//! Environment variables have the same shape (a test that points `HOME` at a
//! temp directory while another resolves the home directory), and the same
//! failure mode when a panicking assertion skips the restore. [`CwdScope`] and
//! [`EnvScope`] are the two guards; [`crate::env::ENV_TEST_LOCK`] is the lock
//! the suite already shares for the environment, so `EnvScope` holds that one
//! instead of inventing a fourth.
//!
//! Two locks rather than one is deliberate: no test needs both, and a test that
//! ever does must take the cwd first, then the environment — the order both
//! guards document.

use std::path::{Path, PathBuf};
use tokio::sync::{Mutex, MutexGuard};

/// Serializes every test that moves the process cwd.
static CWD_LOCK: Mutex<()> = Mutex::const_new(());

/// The process cwd, held inside `path` for as long as the guard lives.
///
/// Taking the scope is the only supported way to move the process cwd in a
/// test: it acquires the shared lock, records the previous directory, and
/// restores it on drop (including on panic), so no window is left open behind
/// a failing assertion.
pub(crate) struct CwdScope {
    /// Declared before `previous` so it drops last: the directory is put back
    /// before the next test is allowed to move it.
    _lock: MutexGuard<'static, ()>,
    previous: PathBuf,
}

impl CwdScope {
    /// Block until no other test holds the cwd, then move it into `path`.
    pub(crate) async fn enter(path: &Path) -> Self {
        let lock = CWD_LOCK.lock().await;
        let previous = std::env::current_dir()
            .expect("the process cwd must be readable before a test moves it");
        std::env::set_current_dir(path).expect("the test workspace must be enterable");
        Self {
            _lock: lock,
            previous,
        }
    }
}

impl Drop for CwdScope {
    fn drop(&mut self) {
        // Restoring is reported rather than panicked on: this runs during
        // unwinding too, and a panic in a destructor aborts the whole run,
        // which would hide the assertion that actually failed.
        if let Err(error) = std::env::set_current_dir(&self.previous) {
            eprintln!(
                "test_support: could not restore the process cwd to {}: {error}",
                self.previous.display()
            );
        }
    }
}

/// Environment variables, set for the guard's lifetime and put back on drop.
///
/// Takes the suite's existing environment lock, so a holder cannot be observed
/// by another test that also takes it (a reader and a writer both have to hold
/// it for the window to close — `HOME`/`USERPROFILE` and the provider base URLs
/// are the ones that matter).
///
/// The restore lives in `Drop` because a failed assertion between "point HOME
/// at a temp dir" and "put it back" would otherwise leave the whole run with a
/// redirected home — the same class of damage as an unrestored cwd, and the
/// reason a manual restore is not enough.
pub(crate) struct EnvScope {
    /// Declared first so it drops last: the variables are restored before the
    /// next test is allowed in.
    _lock: tokio::sync::MutexGuard<'static, ()>,
    previous: Vec<(&'static str, Option<String>)>,
}

impl EnvScope {
    /// Set (or with `None`, unset) each variable until the guard drops.
    pub(crate) fn set(vars: Vec<(&'static str, Option<String>)>) -> Self {
        // `blocking_lock` because every current user is a synchronous `#[test]`;
        // an async test would need `lock().await` and a different guard.
        let lock = crate::env::ENV_TEST_LOCK.blocking_lock();
        let previous = vars
            .iter()
            .map(|(key, _)| (*key, std::env::var(*key).ok()))
            .collect();
        for (key, value) in &vars {
            match value {
                Some(value) => unsafe { std::env::set_var(key, value) },
                None => unsafe { std::env::remove_var(key) },
            }
        }
        Self {
            _lock: lock,
            previous,
        }
    }

    /// Unset each variable until the guard drops.
    ///
    /// The shape most config tests need: a value inherited from the developer's
    /// shell would otherwise decide what the test proves.
    pub(crate) fn unset(keys: &[&'static str]) -> Self {
        Self::set(keys.iter().map(|key| (*key, None)).collect())
    }
}

impl Drop for EnvScope {
    fn drop(&mut self) {
        for (key, value) in &self.previous {
            match value {
                Some(value) => unsafe { std::env::set_var(key, value) },
                None => unsafe { std::env::remove_var(key) },
            }
        }
    }
}
