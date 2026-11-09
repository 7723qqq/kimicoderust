//! Workdir resolution: the one place a directory becomes a trusted path.
//!
//! Upstream v2 has three separate resolvers — `session/workdir/workdir.ts`
//! (`resolveSessionWorkdir`), `session/workspaceContext/workspacePaths.ts`
//! (`resolveWorkspacePath`) and `workspace/workspaceFs/fsService.ts:1116`
//! (`realpathExistingPrefix`). Each answers a different question and each
//! reports a different bare error, so a caller that only needs "is this
//! directory inside the workspace" has to re-implement the containment rule.
//!
//! Here the questions are separated by function but the failure vocabulary is
//! shared. A path that does not exist, a path that exists but is a file, and a
//! path that exists but cannot be resolved are three distinct outcomes; none
//! of them is a silent fallback to the process cwd.

use std::path::{Component, Path, PathBuf};

use super::{EnvironmentError, codes};

/// How far the realpath walk climbs before giving up. The same bound v2 uses:
/// a path deeper than this is not trusted on faith.
const MAX_RESOLUTION_DEPTH: usize = 256;

/// Resolve one working directory.
///
/// The path must be absolute, must exist, must be a directory, and must
/// realpath. Anything else is refused with a code that names which of those
/// four failed. The process cwd is never substituted.
pub fn resolve_directory(raw: &Path) -> Result<PathBuf, EnvironmentError> {
    if raw.as_os_str().is_empty() {
        return Err(EnvironmentError::new(
            codes::WORKDIR_REQUIRED,
            "the working directory is empty",
        ));
    }
    if !raw.is_absolute() {
        return Err(EnvironmentError::new(
            codes::WORKDIR_NOT_ABSOLUTE,
            format!("'{}' is not an absolute path", raw.display()),
        ));
    }
    match std::fs::metadata(raw) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Err(EnvironmentError::new(
                codes::WORKDIR_NOT_DIRECTORY,
                format!("'{}' exists but is not a directory", raw.display()),
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(EnvironmentError::new(
                codes::WORKDIR_NOT_FOUND,
                format!("'{}' does not exist", raw.display()),
            ));
        }
        Err(error) => {
            return Err(EnvironmentError::new(
                codes::WORKDIR_UNREADABLE,
                format!("'{}' cannot be inspected: {error}", raw.display()),
            ));
        }
    }
    realpath_existing_prefix(raw).map_err(|error| {
        EnvironmentError::new(
            codes::WORKDIR_UNREADABLE,
            format!("'{}' cannot be resolved: {error}", raw.display()),
        )
    })
}

/// Resolve `raw` and decide whether it may be used from `roots`.
///
/// `allow_outside` exists for the host-configured top-level workdir: v2 lets
/// the user start a session anywhere, and the containment rule applies to what
/// an *agent* may set, not to what the user may open. Child agents must pass
/// `false`, so a subdirectory request that escapes the parent workspace is a
/// refusal rather than a widened sandbox.
pub fn resolve_within(
    roots: &super::WorkspaceRoots,
    raw: &Path,
    allow_outside: bool,
) -> Result<PathBuf, EnvironmentError> {
    let resolved = resolve_directory(raw)?;
    if allow_outside || roots.contains(&resolved) {
        return Ok(resolved);
    }
    Err(EnvironmentError::new(
        codes::WORKDIR_OUTSIDE_WORKSPACE,
        format!(
            "'{}' resolves outside the workspace rooted at '{}'",
            resolved.display(),
            roots.work_dir().display()
        ),
    ))
}

/// Lexical, component-wise containment. Both sides are expected to be
/// realpaths.
///
/// Component comparison — not string prefix comparison — is what keeps
/// `/work/repo-other` from counting as inside `/work/repo`; Windows compares
/// components case-insensitively because the filesystem does.
#[must_use]
pub fn is_within(root: &Path, candidate: &Path) -> bool {
    let mut root_components = root.components();
    for candidate_component in candidate.components() {
        match root_components.next() {
            Some(root_component) if components_equal(root_component, candidate_component) => {}
            Some(_) => return false,
            None => return true,
        }
    }
    root_components.next().is_none()
}

#[cfg(windows)]
fn components_equal(left: Component<'_>, right: Component<'_>) -> bool {
    let left = left.as_os_str().to_string_lossy();
    let right = right.as_os_str().to_string_lossy();
    left.eq_ignore_ascii_case(&right)
}

#[cfg(not(windows))]
fn components_equal(left: Component<'_>, right: Component<'_>) -> bool {
    left == right
}

/// Canonicalize `path`, tolerating a tail that does not exist yet.
///
/// Mirrors v2 `realpathExistingPrefix`: climb to the nearest existing ancestor,
/// canonicalize that, then re-join the missing tail. Callers that require the
/// whole path to exist (workdirs do) check that first; the walk exists so a
/// race that removes the leaf between the check and this call still produces a
/// path rather than an error.
fn realpath_existing_prefix(path: &Path) -> std::io::Result<PathBuf> {
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut current = path.to_path_buf();
    for _ in 0..MAX_RESOLUTION_DEPTH {
        match std::fs::canonicalize(&current) {
            Ok(mut resolved) => {
                for segment in tail.iter().rev() {
                    resolved.push(segment);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let Some(name) = current.file_name().map(std::ffi::OsStr::to_os_string) else {
                    break;
                };
                tail.push(name);
                if !current.pop() {
                    break;
                }
            }
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!(
            "'{}' has no existing ancestor within {MAX_RESOLUTION_DEPTH} levels",
            path.display()
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kimi-environment-workdir-{}-{}",
            std::process::id(),
            fastrand::u64(..)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_existing_directory_resolves_to_its_realpath() {
        let dir = tempdir();
        let resolved = resolve_directory(&dir).unwrap();
        assert!(resolved.is_absolute());
        assert_eq!(resolved, fs::canonicalize(&dir).unwrap());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_directory_is_not_a_relative_path_and_not_a_file() {
        // v2 reports "does not exist", "not a directory" and "not readable"
        // under one code, `environment.invalid_cwd`; the message is what
        // distinguishes them (`sessionLifecycleService.ts:398-403`).
        let dir = tempdir();
        let missing = dir.join("nope");
        let error = resolve_directory(&missing).unwrap_err();
        assert_eq!(error.code(), codes::WORKDIR_NOT_FOUND);
        assert!(error.detail().contains("does not exist"), "{error}");

        let file = dir.join("file.txt");
        fs::write(&file, "x").unwrap();
        let error = resolve_directory(&file).unwrap_err();
        assert_eq!(error.code(), codes::WORKDIR_NOT_DIRECTORY);
        assert!(error.detail().contains("not a directory"), "{error}");

        let error = resolve_directory(Path::new("relative/dir")).unwrap_err();
        assert_eq!(error.code(), codes::WORKDIR_NOT_ABSOLUTE);
        assert!(error.detail().contains("not an absolute path"), "{error}");

        let error = resolve_directory(Path::new("")).unwrap_err();
        assert_eq!(error.code(), codes::WORKDIR_REQUIRED);
        assert!(error.detail().contains("working directory"), "{error}");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn containment_is_component_wise_not_string_prefix() {
        let root = Path::new("/work/repo");
        assert!(is_within(root, Path::new("/work/repo")));
        assert!(is_within(root, Path::new("/work/repo/src/lib.rs")));
        assert!(!is_within(root, Path::new("/work/repo-other")));
        assert!(!is_within(root, Path::new("/work")));
    }

    #[test]
    fn resolve_within_refuses_an_escape_unless_the_caller_allows_it() {
        let parent = tempdir();
        let inside = parent.join("inside");
        let outside = tempdir();
        fs::create_dir_all(&inside).unwrap();
        let roots = crate::environment::WorkspaceRoots::new(&parent, Vec::new()).unwrap();

        assert_eq!(
            resolve_within(&roots, &inside, false).unwrap(),
            fs::canonicalize(&inside).unwrap()
        );
        let error = resolve_within(&roots, &outside, false).unwrap_err();
        assert_eq!(error.code(), codes::WORKDIR_OUTSIDE_WORKSPACE);
        assert!(resolve_within(&roots, &outside, true).is_ok());

        fs::remove_dir_all(&parent).ok();
        fs::remove_dir_all(&outside).ok();
    }
}
