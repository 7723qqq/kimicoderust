//! AGENTS.md presence, readability and change observation.
//!
//! The prompt loader ([`crate::prompt::agents_md`]) already reports an
//! unreadable file inside a free-form warning string, and nothing re-checks the
//! files after the system prompt was built. That leaves two gaps this module
//! closes without touching the prompt pipeline:
//!
//! - the three outcomes are distinguishable: **absent** (no candidate exists —
//!   normal), **empty** (exists but contributes nothing) and **unreadable**
//!   (exists but cannot be read — a configuration problem);
//! - a change to an instruction file after the prompt was built is detected by
//!   a SHA-256 of its contents (v2 `hashAgentsMdContents`) and reported, instead
//!   of being noticed only on the next process start.
//!
//! Candidate discovery reuses the prompt loader's own list
//! ([`crate::prompt::agents_md::instruction_dirs`] and
//! [`crate::prompt::agents_md::AGENTS_MD_PLAIN_NAMES`]), so a file the prompt
//! reads is a file this module watches — there is no second, drifting list.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::prompt::agents_md::{AGENTS_MD_PLAIN_NAMES, instruction_dirs};

/// One readable, non-empty instruction file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentsMdFile {
    pub path: PathBuf,
    pub bytes: usize,
    /// SHA-256 of the file contents, the fingerprint v2 compares
    /// (`hashAgentsMdContents`). A length+timestamp fingerprint misses a
    /// rewrite that keeps the size and lands within the filesystem's mtime
    /// granularity.
    pub digest: [u8; 32],
}

/// One candidate that exists but cannot be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentsMdUnreadable {
    pub path: PathBuf,
    pub cause: String,
}

/// A warning the caller should surface (log or user notification).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentsMdWarning {
    /// A candidate exists but is not a readable regular file.
    Unreadable { path: PathBuf, cause: String },
}

impl std::fmt::Display for AgentsMdWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreadable { path, cause } => {
                write!(
                    f,
                    "AGENTS.md at '{}' could not be read: {cause}",
                    path.display()
                )
            }
        }
    }
}

/// The current state of every AGENTS.md candidate for one work directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentsMdStatus {
    pub work_dir: PathBuf,
    /// Readable, non-empty files.
    pub files: Vec<AgentsMdFile>,
    /// Files that exist but are empty; they are not an error.
    pub empty: Vec<PathBuf>,
    /// Files that exist but could not be read.
    pub unreadable: Vec<AgentsMdUnreadable>,
    /// No candidate path exists at all. Absence is normal, not a warning, but
    /// it must be distinguishable from "tried and failed".
    pub absent: bool,
}

impl AgentsMdStatus {
    #[must_use]
    pub fn warnings(&self) -> Vec<AgentsMdWarning> {
        self.unreadable
            .iter()
            .map(|unreadable| AgentsMdWarning::Unreadable {
                path: unreadable.path.clone(),
                cause: unreadable.cause.clone(),
            })
            .collect()
    }
}

/// The observable change between two probes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentsMdChange {
    Added { path: PathBuf },
    Modified { path: PathBuf },
    Removed { path: PathBuf },
    BecameUnreadable { path: PathBuf, cause: String },
    BecameReadable { path: PathBuf },
}

impl std::fmt::Display for AgentsMdChange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Added { path } => write!(f, "AGENTS.md at '{}' was added", path.display()),
            Self::Modified { path } => write!(f, "AGENTS.md at '{}' changed", path.display()),
            Self::Removed { path } => write!(f, "AGENTS.md at '{}' was removed", path.display()),
            Self::BecameUnreadable { path, cause } => write!(
                f,
                "AGENTS.md at '{}' became unreadable: {cause}",
                path.display()
            ),
            Self::BecameReadable { path } => {
                write!(f, "AGENTS.md at '{}' is readable again", path.display())
            }
        }
    }
}

/// One candidate's fingerprint.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CandidateState {
    Empty { len: u64 },
    Readable { digest: [u8; 32] },
    Unreadable { cause: String },
}

/// A fingerprint of every candidate, taken by [`AgentsMdWatch::start`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AgentsMdProbe {
    entries: BTreeMap<String, CandidateState>,
}

impl AgentsMdProbe {
    fn capture(status: &AgentsMdStatus) -> Self {
        let mut entries = BTreeMap::new();
        for file in &status.files {
            entries.insert(candidate_key(&file.path), readable_state(file.digest));
        }
        for path in &status.empty {
            entries.insert(
                candidate_key(path),
                CandidateState::Empty {
                    len: std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0),
                },
            );
        }
        for unreadable in &status.unreadable {
            entries.insert(
                candidate_key(&unreadable.path),
                CandidateState::Unreadable {
                    cause: unreadable.cause.clone(),
                },
            );
        }
        Self { entries }
    }

    /// Compare against a fresh status.
    ///
    /// Only paths that appeared, disappeared or changed are returned; a file
    /// that stayed absent is not a change.
    #[must_use]
    pub fn diff(&self, next: &AgentsMdStatus) -> Vec<AgentsMdChange> {
        let next_probe = Self::capture(next);
        let mut changes = Vec::new();
        for (key, state) in &next_probe.entries {
            let previous = self.entries.get(key);
            if previous == Some(state) {
                continue;
            }
            let path = next
                .files
                .iter()
                .map(|file| &file.path)
                .chain(next.empty.iter())
                .chain(next.unreadable.iter().map(|item| &item.path))
                .find(|path| candidate_key(path) == *key)
                .cloned()
                .unwrap_or_else(|| PathBuf::from(key));
            match (previous, state) {
                (None, _) => {
                    changes.push(classification_for(path, state));
                }
                (Some(_), CandidateState::Unreadable { cause }) => {
                    changes.push(AgentsMdChange::BecameUnreadable {
                        path,
                        cause: cause.clone(),
                    });
                }
                (Some(CandidateState::Unreadable { .. }), _) => {
                    changes.push(AgentsMdChange::BecameReadable { path });
                }
                (Some(_), _) => changes.push(AgentsMdChange::Modified { path }),
            }
        }
        for key in self.entries.keys() {
            if next_probe.entries.contains_key(key) {
                continue;
            }
            changes.push(AgentsMdChange::Removed {
                path: PathBuf::from(key),
            });
        }
        changes.sort_by_key(|change| change_sort_key(change));
        changes
    }
}

fn classification_for(path: PathBuf, state: &CandidateState) -> AgentsMdChange {
    match state {
        CandidateState::Unreadable { cause } => AgentsMdChange::BecameUnreadable {
            path,
            cause: cause.clone(),
        },
        _ => AgentsMdChange::Added { path },
    }
}

fn change_sort_key(change: &AgentsMdChange) -> String {
    match change {
        AgentsMdChange::Added { path }
        | AgentsMdChange::Modified { path }
        | AgentsMdChange::Removed { path }
        | AgentsMdChange::BecameReadable { path } => format!("{path:?}"),
        AgentsMdChange::BecameUnreadable { path, .. } => format!("{path:?}"),
    }
}

fn readable_state(digest: [u8; 32]) -> CandidateState {
    CandidateState::Readable { digest }
}

fn candidate_key(path: &Path) -> String {
    let display = path.display().to_string();
    if cfg!(windows) {
        display.to_lowercase()
    } else {
        display
    }
}

/// Probe every candidate for `work_dir`.
///
/// `brand_home` maps to `<brandHome>/AGENTS.md`; when absent, `home` maps to
/// `~/.kimi-code/AGENTS.md`. Both then fall back to `~/.agents/AGENTS.md`, and
/// the workspace chain is walked root-first — exactly the loader's order.
#[must_use]
pub fn probe(work_dir: &Path, brand_home: Option<&Path>, home: Option<&Path>) -> AgentsMdStatus {
    let mut files = Vec::new();
    let mut empty = Vec::new();
    let mut unreadable = Vec::new();
    let mut seen = std::collections::HashSet::new();

    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(brand_home) = brand_home {
        candidates.push(brand_home.join("AGENTS.md"));
    } else if let Some(home) = home {
        candidates.push(home.join(".kimi-code").join("AGENTS.md"));
    }
    if let Some(home) = home {
        candidates.extend(
            AGENTS_MD_PLAIN_NAMES
                .iter()
                .map(|name| home.join(".agents").join(name)),
        );
    }
    for dir in instruction_dirs(work_dir) {
        candidates.push(dir.join(".kimi-code").join("AGENTS.md"));
        candidates.extend(AGENTS_MD_PLAIN_NAMES.iter().map(|name| dir.join(name)));
    }

    for path in candidates {
        let key = candidate_key(&path);
        if !seen.insert(key) {
            continue;
        }
        match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                unreadable.push(AgentsMdUnreadable {
                    path,
                    cause: format!("cannot inspect the path: {error}"),
                });
            }
            Ok(metadata) if !metadata.is_file() => {
                unreadable.push(AgentsMdUnreadable {
                    path,
                    cause: "exists but is not a regular file".to_string(),
                });
            }
            Ok(_) => match std::fs::read_to_string(&path) {
                Ok(raw) if raw.trim().is_empty() => empty.push(path),
                Ok(raw) => files.push(AgentsMdFile {
                    bytes: raw.len(),
                    digest: Sha256::digest(raw.as_bytes()).into(),
                    path,
                }),
                Err(error) => unreadable.push(AgentsMdUnreadable {
                    path,
                    cause: error.to_string(),
                }),
            },
        }
    }

    let absent = files.is_empty() && empty.is_empty() && unreadable.is_empty();
    AgentsMdStatus {
        work_dir: work_dir.to_path_buf(),
        files,
        empty,
        unreadable,
        absent,
    }
}

/// A per-session observer: start once, then [`Self::check`] each turn.
#[derive(Debug)]
pub struct AgentsMdWatch {
    probe: AgentsMdProbe,
    status: AgentsMdStatus,
}

impl AgentsMdWatch {
    #[must_use]
    pub fn start(work_dir: &Path, brand_home: Option<&Path>, home: Option<&Path>) -> Self {
        let status = probe(work_dir, brand_home, home);
        Self {
            probe: AgentsMdProbe::capture(&status),
            status,
        }
    }

    /// Re-probe and return what changed since the last check.
    pub fn check(
        &mut self,
        work_dir: &Path,
        brand_home: Option<&Path>,
        home: Option<&Path>,
    ) -> Vec<AgentsMdChange> {
        let status = probe(work_dir, brand_home, home);
        let changes = self.probe.diff(&status);
        self.probe = AgentsMdProbe::capture(&status);
        self.status = status;
        changes
    }

    #[must_use]
    pub fn status(&self) -> &AgentsMdStatus {
        &self.status
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kimi-environment-agents-md-{}-{}",
            std::process::id(),
            fastrand::u64(..)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_directory_without_instruction_files_is_absent_not_unreadable() {
        let work = tempdir();
        let status = probe(&work, None, None);
        assert!(status.absent);
        assert!(status.files.is_empty());
        assert!(status.unreadable.is_empty());
        assert!(status.warnings().is_empty());
        fs::remove_dir_all(&work).ok();
    }

    #[test]
    fn an_empty_file_is_reported_as_empty() {
        let work = tempdir();
        fs::write(work.join("AGENTS.md"), "  \n\t").unwrap();
        let status = probe(&work, None, None);
        assert!(!status.absent);
        assert_eq!(status.empty.len(), 1);
        assert!(status.files.is_empty());
        fs::remove_dir_all(&work).ok();
    }

    #[test]
    fn a_same_length_rewrite_is_detected_as_modified() {
        // A length+timestamp fingerprint misses a rewrite that keeps the size
        // and lands within the filesystem's mtime granularity; v2 compares a
        // content hash, so the change is visible.
        let work = tempdir();
        let file = work.join("AGENTS.md");
        fs::write(&file, "first").unwrap();
        let mut watch = AgentsMdWatch::start(&work, None, None);
        assert_eq!(watch.status().files[0].bytes, 5);

        fs::write(&file, "FIRST").unwrap();
        let changes = watch.check(&work, None, None);
        assert_eq!(
            changes,
            vec![AgentsMdChange::Modified { path: file.clone() }]
        );
        fs::remove_dir_all(&work).ok();
    }

    #[test]
    fn an_unreadable_candidate_is_a_warning_and_not_absence() {
        let work = tempdir();
        // A directory where the file should be: exists, but is not a regular
        // file, which is the cross-platform stand-in for "cannot read".
        fs::create_dir_all(work.join("AGENTS.md")).unwrap();
        let status = probe(&work, None, None);
        assert!(!status.absent);
        assert_eq!(status.unreadable.len(), 1);
        let warnings = status.warnings();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].to_string().contains("AGENTS.md"));
        fs::remove_dir_all(&work).ok();
    }

    #[test]
    fn a_rewritten_file_is_detected_as_modified() {
        let work = tempdir();
        let file = work.join("AGENTS.md");
        fs::write(&file, "first").unwrap();
        let mut watch = AgentsMdWatch::start(&work, None, None);
        assert_eq!(watch.status().files.len(), 1);

        fs::write(&file, "second, longer").unwrap();
        let changes = watch.check(&work, None, None);
        assert_eq!(
            changes,
            vec![AgentsMdChange::Modified { path: file.clone() }]
        );

        let changes = watch.check(&work, None, None);
        assert!(changes.is_empty(), "no change means no event");
        fs::remove_dir_all(&work).ok();
    }

    #[test]
    fn going_from_readable_to_unreadable_is_reported_separately() {
        let work = tempdir();
        let file = work.join("AGENTS.md");
        fs::write(&file, "content").unwrap();
        let mut watch = AgentsMdWatch::start(&work, None, None);

        fs::remove_file(&file).unwrap();
        fs::create_dir_all(&file).unwrap();
        let changes = watch.check(&work, None, None);
        assert_eq!(changes.len(), 1);
        assert!(matches!(
            changes[0],
            AgentsMdChange::BecameUnreadable { .. }
        ));
        fs::remove_dir_all(&work).ok();
    }
}
