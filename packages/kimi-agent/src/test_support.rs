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
//! [`CwdScope`] is the single lock. Two locks (one per test file) serialize
//! each file against itself and leave the cross-file window exactly as wide as
//! it was, which is how the split test files reintroduced this race once.

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
