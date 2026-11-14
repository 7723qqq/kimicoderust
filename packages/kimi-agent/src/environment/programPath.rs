use std::path::{Component, Path, PathBuf};

pub fn resolve_program_path(program: &str, path_env: Option<&str>) -> String {
    resolve_program_path_result(program, path_env).unwrap_or_else(|error| panic!("{error}"))
}

fn resolve_program_path_result(program: &str, path_env: Option<&str>) -> Result<String, String> {
    if program.is_empty() || program.trim().is_empty() {
        return Err("launcher command must be a non-empty program name or absolute path".to_string());
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    if program.contains('/') || program.contains('\\') {
        if !Path::new(program).is_absolute() {
            return Err(format!(
                "launcher command \"{program}\" must be an absolute path"
            ));
        }
        if !is_executable_file(Path::new(program)) {
            return Err(format!(
                "launcher command \"{program}\" is not an executable file"
            ));
        }
        if is_inside_cwd(Path::new(program), &cwd) {
            return Err(format!(
                "launcher command \"{program}\" resolves inside the working directory; refusing cwd match"
            ));
        }
        return Ok(program.to_string());
    }
    let path_env = path_env
        .map(str::to_string)
        .or_else(|| std::env::var("PATH").ok())
        .unwrap_or_default();
    let extensions = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".to_string())
            .split(';')
            .map(str::to_string)
            .collect::<Vec<_>>()
    } else {
        vec![String::new()]
    };
    let has_executable_extension = cfg!(windows)
        && extensions.iter().any(|extension| {
            !extension.is_empty()
                && program
                    .to_lowercase()
                    .ends_with(&extension.to_lowercase())
        });
    let candidate_names = if has_executable_extension {
        vec![program.to_string()]
    } else {
        extensions
            .iter()
            .map(|extension| format!("{program}{}", extension.to_lowercase()))
            .collect()
    };
    for entry in path_env.split(if cfg!(windows) { ';' } else { ':' }) {
        if entry.is_empty() || !Path::new(entry).is_absolute() {
            continue;
        }
        for name in &candidate_names {
            let candidate = Path::new(entry).join(name);
            if !is_executable_file(&candidate) {
                continue;
            }
            if is_inside_cwd(&candidate, &cwd) {
                return Err(format!(
                    "launcher command \"{program}\" resolves inside the working directory; refusing cwd match"
                ));
            }
            return Ok(candidate.to_string_lossy().into_owned());
        }
    }
    Err(format!(
        "launcher command \"{program}\" was not found on PATH"
    ))
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn is_inside_cwd(path: &Path, cwd: &Path) -> bool {
    let fold = |value: PathBuf| {
        let value = value.to_string_lossy().into_owned();
        if cfg!(windows) {
            value.to_lowercase()
        } else {
            value
        }
    };
    let relative_cwd = fold(resolve_host(cwd));
    let resolved = fold(resolve_host(path));
    let separator = std::path::MAIN_SEPARATOR.to_string();
    resolved == relative_cwd || resolved.starts_with(&(relative_cwd + &separator))
}

fn resolve_host(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_default()
            .join(path)
    };
    let mut output = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                output.pop();
            }
            other => output.push(other.as_os_str()),
        }
    }
    output
}
