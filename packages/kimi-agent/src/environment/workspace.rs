//! Workspace roots and their change feed.
//!
//! A session is confined to a set of roots: one workdir plus optional
//! additional directories. Upstream keeps them in `workspaceInfo` and rebuilds
//! the workspace context when they change (`session/workspaceContext/
//! agentWorkspaceContextService.ts`). The fork only ever carried a single root
//! string, so a root change had no observable event and no revision to compare
//! against — a rebuild could not tell "unchanged" from "changed and lost".
//!
//! [`WorkspaceContext`] makes the change explicit: every accepted root change
//! bumps a revision and publishes it. Subscribers read the change or time out;
//! a slow subscriber is told how many changes it missed rather than silently
//! continuing with stale roots.

use std::sync::{Arc, RwLock};

use tokio::sync::broadcast;

use super::workdir;
use super::{EnvironmentError, codes};
use crate::primitives::WaitTimeout;

/// The directories a session may touch: one workdir and zero or more
/// additional roots, all realpaths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRoots {
    work_dir: std::path::PathBuf,
    additional_dirs: Vec<std::path::PathBuf>,
}

impl WorkspaceRoots {
    /// Resolve and validate the roots.
    ///
    /// Every root must exist and be a directory; a missing additional root is
    /// an error, not a silently dropped one, because a tool that later writes
    /// there would otherwise fail with a path error far from the cause.
    pub fn new(
        work_dir: impl Into<std::path::PathBuf>,
        additional_dirs: Vec<std::path::PathBuf>,
    ) -> Result<Self, EnvironmentError> {
        let work_dir = workdir::resolve_directory(&work_dir.into())?;
        let mut resolved_additional = Vec::with_capacity(additional_dirs.len());
        for dir in additional_dirs {
            let resolved = workdir::resolve_directory(&dir).map_err(|error| {
                // An additional root is validated like v2's `requireDirectory`
                // (`app/workspace/workspaceService.ts:227,237`): both "does
                // not exist" and "is not a directory" report
                // `fs.path_not_found`.
                EnvironmentError::new(
                    if error.code() == codes::WORKDIR_NOT_FOUND
                        || error.code() == codes::WORKDIR_NOT_DIRECTORY
                    {
                        codes::WORKSPACE_ROOT_NOT_FOUND
                    } else {
                        error.code()
                    },
                    error.detail().to_string(),
                )
            })?;
            if resolved == work_dir || resolved_additional.contains(&resolved) {
                continue;
            }
            resolved_additional.push(resolved);
        }
        Ok(Self {
            work_dir,
            additional_dirs: resolved_additional,
        })
    }

    #[must_use]
    pub fn work_dir(&self) -> &std::path::Path {
        &self.work_dir
    }

    #[must_use]
    pub fn additional_dirs(&self) -> &[std::path::PathBuf] {
        &self.additional_dirs
    }

    /// The workdir first, then every additional root.
    pub fn roots(&self) -> impl Iterator<Item = &std::path::Path> {
        std::iter::once(self.work_dir.as_path())
            .chain(self.additional_dirs.iter().map(std::path::PathBuf::as_path))
    }

    /// Is `candidate` inside any root (or equal to one)?
    #[must_use]
    pub fn contains(&self, candidate: &std::path::Path) -> bool {
        self.roots().any(|root| workdir::is_within(root, candidate))
    }
}

/// One accepted change to the roots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceChange {
    /// Monotonic; starts at 1 for the initial roots.
    pub revision: u64,
    pub roots: WorkspaceRoots,
}

struct WorkspaceState {
    revision: u64,
    roots: WorkspaceRoots,
}

struct WorkspaceInner {
    state: RwLock<WorkspaceState>,
    changes: broadcast::Sender<WorkspaceChange>,
}

/// Cheap-to-clone handle to the current roots and their change feed.
#[derive(Clone)]
pub struct WorkspaceContext {
    inner: Arc<WorkspaceInner>,
}

impl WorkspaceContext {
    /// How many changes a slow subscriber may fall behind before it is told it
    /// missed some. Small on purpose: a consumer that needs every revision
    /// should read [`WorkspaceContext::snapshot`] after a lag notice.
    const CHANGE_BUFFER: usize = 16;

    #[must_use]
    pub fn new(roots: WorkspaceRoots) -> Self {
        let (changes, _) = broadcast::channel(Self::CHANGE_BUFFER);
        Self {
            inner: Arc::new(WorkspaceInner {
                state: RwLock::new(WorkspaceState { revision: 1, roots }),
                changes,
            }),
        }
    }

    /// The current revision and roots, with no wait.
    #[must_use]
    pub fn snapshot(&self) -> WorkspaceChange {
        let state = self.inner.state.read().unwrap_or_else(|poison| {
            // A poisoned lock means a writer panicked mid-update. The roots
            // are a plain value; reading the last written value is safe and
            // strictly better than hiding the revision.
            poison.into_inner()
        });
        WorkspaceChange {
            revision: state.revision,
            roots: state.roots.clone(),
        }
    }

    #[must_use]
    pub fn revision(&self) -> u64 {
        self.snapshot().revision
    }

    #[must_use]
    pub fn roots(&self) -> WorkspaceRoots {
        self.snapshot().roots
    }

    #[must_use]
    pub fn starts_within(&self, candidate: &std::path::Path) -> bool {
        self.roots().contains(candidate)
    }

    /// Replace the roots and publish the change.
    ///
    /// The caller has already resolved the new roots; this never rewrites a
    /// path. Publishing with no subscribers is expected and not an error.
    pub fn set_roots(&self, roots: WorkspaceRoots) -> WorkspaceChange {
        let change = {
            let mut state = self
                .inner
                .state
                .write()
                .unwrap_or_else(|poison| poison.into_inner());
            state.revision = state.revision.saturating_add(1);
            state.roots = roots;
            WorkspaceChange {
                revision: state.revision,
                roots: state.roots.clone(),
            }
        };
        if let Err(error) = self.inner.changes.send(change.clone()) {
            tracing::debug!(
                revision = change.revision,
                reason = %error,
                "workspace change had no subscribers"
            );
        }
        change
    }

    #[must_use]
    pub fn subscribe(&self) -> WorkspaceSubscription {
        WorkspaceSubscription {
            receiver: self.inner.changes.subscribe(),
        }
    }
}

/// Why a change wait ended without a change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceWatchError {
    /// The bounded wait expired.
    TimedOut(WaitTimeout),
    /// The subscriber fell behind and missed `missed` revisions; read the
    /// snapshot before continuing.
    Lagged { missed: u64 },
    /// Every sender is gone: the workspace context was dropped.
    Closed,
}

impl std::fmt::Display for WorkspaceWatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TimedOut(timeout) => write!(f, "{timeout}"),
            Self::Lagged { missed } => {
                write!(f, "workspace change subscriber missed {missed} revisions")
            }
            Self::Closed => write!(f, "workspace change feed is closed"),
        }
    }
}

impl std::error::Error for WorkspaceWatchError {}

/// A bounded receiver of root changes.
pub struct WorkspaceSubscription {
    receiver: broadcast::Receiver<WorkspaceChange>,
}

impl WorkspaceSubscription {
    /// Wait for the next change, bounded by `deadline`.
    ///
    /// A lag is reported, not swallowed: the caller re-reads
    /// [`WorkspaceContext::snapshot`] to recover the newest roots.
    pub async fn changed(
        &mut self,
        deadline: &crate::primitives::Deadline,
    ) -> Result<WorkspaceChange, WorkspaceWatchError> {
        match deadline
            .wait("waiting for a workspace change", self.receiver.recv())
            .await
        {
            Ok(Ok(change)) => Ok(change),
            Ok(Err(broadcast::error::RecvError::Lagged(missed))) => {
                Err(WorkspaceWatchError::Lagged { missed })
            }
            Ok(Err(broadcast::error::RecvError::Closed)) => Err(WorkspaceWatchError::Closed),
            Err(timeout) => Err(WorkspaceWatchError::TimedOut(timeout)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::primitives::Deadline;
    use std::fs;

    fn tempdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kimi-environment-workspace-{}-{}",
            std::process::id(),
            fastrand::u64(..)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn roots_resolve_and_deduplicate() {
        let work = tempdir();
        let extra = tempdir();
        let roots = WorkspaceRoots::new(&work, vec![extra.clone(), extra.clone()]).unwrap();
        assert_eq!(roots.work_dir(), fs::canonicalize(&work).unwrap());
        assert_eq!(roots.additional_dirs().len(), 1);
        assert!(roots.contains(&fs::canonicalize(&extra).unwrap()));
        fs::remove_dir_all(&work).ok();
        fs::remove_dir_all(&extra).ok();
    }

    #[test]
    fn a_missing_additional_root_is_an_error() {
        let work = tempdir();
        let missing = work.join("missing");
        let error = WorkspaceRoots::new(&work, vec![missing]).unwrap_err();
        assert_eq!(error.code(), codes::WORKSPACE_ROOT_NOT_FOUND);
        fs::remove_dir_all(&work).ok();
    }

    #[tokio::test]
    async fn a_root_change_bumps_the_revision_and_wakes_subscribers() {
        let work = tempdir();
        let context = WorkspaceContext::new(WorkspaceRoots::new(&work, Vec::new()).unwrap());
        assert_eq!(context.snapshot().revision, 1);

        let mut subscription = context.subscribe();
        let next = tempdir();
        let change = context.set_roots(WorkspaceRoots::new(&next, Vec::new()).unwrap());
        assert_eq!(change.revision, 2);

        // Test observation window only; the feed itself has no bound.
        let deadline = Deadline::new("workspace change test", std::time::Duration::from_secs(5));
        let received = subscription.changed(&deadline).await.unwrap();
        assert_eq!(received.revision, 2);
        assert_eq!(received.roots.work_dir(), fs::canonicalize(&next).unwrap());

        // A second change is also observed: the feed is not a one-shot.
        context.set_roots(WorkspaceRoots::new(&work, Vec::new()).unwrap());
        let received = subscription.changed(&deadline).await.unwrap();
        assert_eq!(received.revision, 3);

        fs::remove_dir_all(&work).ok();
        fs::remove_dir_all(&next).ok();
    }
}
