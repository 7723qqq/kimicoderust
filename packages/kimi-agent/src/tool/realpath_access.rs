use std::path::{Path, PathBuf};

use super::path_access::{
    PathClass, PathSecurityError, is_project_local_config_path, is_sensitive_file,
    is_within_directory, is_within_workspace,
};

const MAX_RESOLUTION_DEPTH: usize = 256;

fn existing_but_unresolvable(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

fn is_missing_path_io_error(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    ) || matches!(error.raw_os_error(), Some(2) | Some(20))
}

pub fn realpath_existing_prefix(abs_path: &Path) -> Result<PathBuf, PathSecurityError> {
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut current = abs_path.to_path_buf();
    for _ in 0..MAX_RESOLUTION_DEPTH {
        match std::fs::canonicalize(&current) {
            Ok(real) => {
                let mut resolved = real;
                for part in tail.iter().rev() {
                    resolved.push(part);
                }
                return Ok(resolved);
            }
            Err(error) if is_missing_path_io_error(&error) => {
                if existing_but_unresolvable(&current) {
                    return Err(PathSecurityError::symlink_escape(
                        &abs_path.to_string_lossy(),
                        &current.to_string_lossy(),
                        format!(
                            "\"{}\" is a symbolic link whose target does not exist. Access is blocked.",
                            current.to_string_lossy()
                        ),
                    ));
                }
                let Some(parent) = current.parent().map(Path::to_path_buf) else {
                    return Ok(abs_path.to_path_buf());
                };
                if parent == current {
                    return Ok(abs_path.to_path_buf());
                }
                let Some(name) = current.file_name().map(std::ffi::OsString::from) else {
                    return Ok(abs_path.to_path_buf());
                };
                tail.push(name);
                current = parent;
            }
            Err(_) => {
                return Err(PathSecurityError::symlink_escape(
                    &abs_path.to_string_lossy(),
                    &current.to_string_lossy(),
                    format!(
                        "\"{}\" could not be resolved to a real path. Access is blocked.",
                        current.to_string_lossy()
                    ),
                ));
            }
        }
    }
    Err(PathSecurityError::symlink_escape(
        &abs_path.to_string_lossy(),
        &abs_path.to_string_lossy(),
        format!(
            "\"{}\" is too deep to resolve to a real path. Access is blocked.",
            abs_path.to_string_lossy()
        ),
    ))
}

fn real_roots(roots: &[String]) -> Vec<PathBuf> {
    roots
        .iter()
        .map(|root| std::fs::canonicalize(root).unwrap_or_else(|_| PathBuf::from(root)))
        .collect()
}

pub fn assert_real_path_within_workspace(
    abs_path: &Path,
    workspace_roots: &[String],
    path_class: PathClass,
    check_sensitive: bool,
) -> Result<PathBuf, PathSecurityError> {
    let raw = abs_path.to_string_lossy().into_owned();

    if !is_within_workspace(&raw, workspace_roots, path_class) {
        let resolved = realpath_existing_prefix(abs_path)?;
        if check_sensitive && is_sensitive_file(&resolved.to_string_lossy()) {
            return Err(PathSecurityError::symlink_sensitive(
                &raw,
                &resolved.to_string_lossy(),
            ));
        }
        return Ok(abs_path.to_path_buf());
    }

    let resolved = realpath_existing_prefix(abs_path)?;
    let resolved_str = resolved.to_string_lossy().into_owned();

    if check_sensitive && is_sensitive_file(&resolved_str) {
        return Err(PathSecurityError::symlink_sensitive(&raw, &resolved_str));
    }

    let real_roots = real_roots(workspace_roots);
    let contained = real_roots
        .iter()
        .any(|root| is_within_directory(&resolved_str, &root.to_string_lossy(), path_class));
    if contained {
        return Ok(resolved);
    }
    Err(PathSecurityError::symlink_escape(
        &raw,
        &resolved_str,
        format!(
            "\"{raw}\" resolves to \"{resolved_str}\" through a symbolic link that points \
             outside the working directory. Access is blocked; use the real path directly \
             or add the target directory to the workspace."
        ),
    ))
}

pub fn assert_real_path_write_target(
    abs_path: &Path,
    workspace_roots: &[String],
    path_class: PathClass,
) -> Result<PathBuf, PathSecurityError> {
    let resolved = assert_real_path_within_workspace(abs_path, workspace_roots, path_class, true)?;
    let resolved_str = resolved.to_string_lossy();
    if !is_project_local_config_path(&abs_path.to_string_lossy())
        && is_project_local_config_path(&resolved_str)
    {
        return Err(PathSecurityError::symlink_escape(
            &abs_path.to_string_lossy(),
            &resolved_str,
            format!(
                "\"{}\" resolves to the project-local config \"{resolved_str}\" through a \
                 symbolic link. Access is blocked; use the real path so the write goes \
                 through approval.",
                abs_path.to_string_lossy()
            ),
        ));
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::path_access::PathSecurityCode;

    fn symlink_file(target: &Path, link: &Path) -> bool {
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_file(target, link).is_ok();
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(target, link).is_ok();
        made
    }

    fn symlink_dir(target: &Path, link: &Path) -> bool {
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_dir(target, link).is_ok();
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(target, link).is_ok();
        made
    }

    #[test]
    fn an_innocuous_name_over_a_sensitive_file_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let secret = root.path().join(".env");
        std::fs::write(&secret, "SECRET=1\n").unwrap();
        let link = root.path().join("notes.txt");
        if !symlink_file(&secret, &link) {
            eprintln!("skipping: this platform would not create the symlink");
            return;
        }

        assert!(!is_sensitive_file(&link.to_string_lossy()));
        let roots = vec![root.path().to_string_lossy().into_owned()];
        let err =
            assert_real_path_within_workspace(&link, &roots, PathClass::Posix, true).unwrap_err();
        assert_eq!(err.code, PathSecurityCode::Sensitive);
        assert!(
            err.message.contains("resolves to") && err.message.contains(".env"),
            "the message must name both ends: {}",
            err.message
        );
    }

    #[test]
    fn a_link_out_of_the_workspace_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("elsewhere.txt");
        std::fs::write(&target, "x").unwrap();
        let link = root.path().join("shortcut.txt");
        if !symlink_file(&target, &link) {
            eprintln!("skipping: this platform would not create the symlink");
            return;
        }
        let roots = vec![root.path().to_string_lossy().into_owned()];
        let err =
            assert_real_path_within_workspace(&link, &roots, PathClass::Posix, true).unwrap_err();
        assert!(
            err.message.contains("points outside the working directory"),
            "{}",
            err.message
        );
    }

    #[test]
    fn a_link_that_stays_inside_is_followed_and_allowed() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real.txt");
        std::fs::write(&real, "x").unwrap();
        let link = root.path().join("alias.txt");
        if !symlink_file(&real, &link) {
            eprintln!("skipping: this platform would not create the symlink");
            return;
        }
        let roots = vec![root.path().to_string_lossy().into_owned()];
        let resolved =
            assert_real_path_within_workspace(&link, &roots, PathClass::Posix, true).unwrap();
        assert!(resolved.ends_with("real.txt"), "{}", resolved.display());
    }

    #[test]
    fn a_dangling_link_is_refused_rather_than_followed() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("gone.txt");
        let link = root.path().join("dangling.txt");
        if !symlink_file(&missing, &link) {
            eprintln!("skipping: this platform would not create the symlink");
            return;
        }
        let roots = vec![root.path().to_string_lossy().into_owned()];
        let err =
            assert_real_path_within_workspace(&link, &roots, PathClass::Posix, true).unwrap_err();
        assert!(
            err.message.contains("target does not exist"),
            "{}",
            err.message
        );
    }

    #[test]
    fn the_search_policy_does_not_refuse_a_sensitive_target() {
        let root = tempfile::tempdir().unwrap();
        let secret = root.path().join(".env");
        std::fs::write(&secret, "SECRET=1\n").unwrap();
        let link = root.path().join("notes.txt");
        if !symlink_file(&secret, &link) {
            eprintln!("skipping: this platform would not create the symlink");
            return;
        }
        let roots = vec![root.path().to_string_lossy().into_owned()];
        assert!(
            assert_real_path_within_workspace(&link, &roots, PathClass::Posix, false).is_ok(),
            "the search policy must not refuse on a sensitive target"
        );
    }

    #[test]
    fn a_write_onto_the_project_local_config_through_a_link_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(".kimi-code");
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("local.toml");
        std::fs::write(&config, "").unwrap();
        let link = root.path().join("innocent.toml");
        if !symlink_file(&config, &link) {
            eprintln!("skipping: this platform would not create the symlink");
            return;
        }
        let roots = vec![root.path().to_string_lossy().into_owned()];
        let err = assert_real_path_write_target(&link, &roots, PathClass::Posix).unwrap_err();
        assert!(
            err.message.contains("project-local config"),
            "{}",
            err.message
        );
    }

    #[test]
    fn project_local_config_detection_is_separator_and_case_insensitive() {
        for path in [
            "/ws/.kimi-code/local.toml",
            "/ws/.KIMI-CODE/LOCAL.TOML",
            r"C:\ws\.kimi-code\LOCAL.TOML",
        ] {
            assert!(is_project_local_config_path(path), "{path}");
        }
        for path in [
            "/ws/.kimi-code/other.toml",
            "/ws/local.toml",
            "/ws/x/local.toml.bak",
        ] {
            assert!(!is_project_local_config_path(path), "{path}");
        }
    }

    #[test]
    fn a_missing_write_target_resolves_through_its_existing_ancestor() {
        let root = tempfile::tempdir().unwrap();
        let new_file = root.path().join("brand-new.txt");
        let roots = vec![root.path().to_string_lossy().into_owned()];
        let resolved =
            assert_real_path_within_workspace(&new_file, &roots, PathClass::Posix, true).unwrap();
        assert!(
            resolved.ends_with("brand-new.txt"),
            "{}",
            resolved.display()
        );
    }

    #[test]
    fn a_directory_link_out_of_the_workspace_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let link = root.path().join("escape");
        if !symlink_dir(outside.path(), &link) {
            eprintln!("skipping: this platform would not create the symlink");
            return;
        }
        let roots = vec![root.path().to_string_lossy().into_owned()];
        let child = link.join("file.txt");
        assert!(
            assert_real_path_within_workspace(&child, &roots, PathClass::Posix, true).is_err(),
            "a link chain out of the workspace must be refused"
        );
    }
}
