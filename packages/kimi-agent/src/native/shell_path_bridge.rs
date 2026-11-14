use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::path_access::{PathClass, normalize_path};

const VIRTUAL_FS_PREFIXES: [&str; 3] = ["/dev/", "/proc/", "/sys/"];

pub struct ShellPathBridge {
    cygpath: Option<PathBuf>,

    roots: Mutex<HashMap<String, String>>,
}

impl ShellPathBridge {
    pub fn new(shell_path: &str) -> Self {
        Self {
            cygpath: if cfg!(windows) {
                locate_cygpath(shell_path)
            } else {
                None
            },
            roots: Mutex::new(HashMap::new()),
        }
    }

    pub fn to_native_path(&self, path: &str) -> String {
        if !cfg!(windows) {
            return path.to_string();
        }
        if path.starts_with("//") {
            return path.to_string();
        }
        if !path.starts_with('/') {
            return path.to_string();
        }
        let normalized = normalize_path(path, PathClass::Posix);
        let lexical = translate_shell_drive_path(&normalized);
        if lexical != normalized {
            return lexical;
        }
        if normalized == "/" {
            return normalized;
        }
        if VIRTUAL_FS_PREFIXES
            .iter()
            .any(|prefix| normalized.starts_with(prefix))
        {
            return normalized;
        }
        let first_segment = normalized[1..].split('/').next().unwrap_or("");
        let Some(prefix) = self.resolve_root_segment(first_segment) else {
            return normalized;
        };
        let remainder = &normalized[first_segment.len() + 1..];
        let joined = format!("{prefix}{remainder}").replace('\\', "/");
        if is_bare_drive(&joined) {
            format!("{joined}/")
        } else {
            joined
        }
    }

    fn resolve_root_segment(&self, first_segment: &str) -> Option<String> {
        if let Some(cached) = self
            .roots
            .lock()
            .ok()
            .and_then(|cache| cache.get(first_segment).cloned())
        {
            return Some(cached);
        }
        let cygpath = self.cygpath.as_ref()?;
        let output = std::process::Command::new(cygpath)
            .args(["-w", "-C", "UTF8", "--"])
            .arg(format!("/{first_segment}"))
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let trimmed = String::from_utf8_lossy(&output.stdout)
            .trim_end_matches(['\r', '\n'])
            .to_string();
        if !is_win32_drive_absolute(&trimmed) && !trimmed.starts_with("\\\\") {
            return None;
        }
        let resolved = trimmed.trim_end_matches(['\\', '/']).to_string();
        if let Ok(mut cache) = self.roots.lock() {
            cache.insert(first_segment.to_string(), resolved.clone());
        }
        Some(resolved)
    }
}

fn translate_shell_drive_path(path: &str) -> String {
    if let Some((letter, rest)) = match_drive_colon(path) {
        return join_drive(letter, rest);
    }
    if let Some((letter, rest)) = match_cygdrive(path) {
        return join_drive(letter, rest);
    }
    if let Some((letter, rest)) = match_drive(path) {
        return join_drive(letter, rest);
    }
    path.to_string()
}

fn match_drive_colon(path: &str) -> Option<(char, &str)> {
    let bytes = path.as_bytes();
    if bytes.len() < 3 || bytes[0] != b'/' || !bytes[1].is_ascii_alphabetic() || bytes[2] != b':' {
        return None;
    }
    if bytes.len() > 3 && bytes[3] != b'/' && bytes[3] != b'\\' {
        return None;
    }
    Some((bytes[1] as char, &path[3..]))
}

fn match_cygdrive(path: &str) -> Option<(char, &str)> {
    const PREFIX: &str = "/cygdrive/";
    let rest = path.strip_prefix(PREFIX)?;
    let letter = rest.chars().next()?;
    if !letter.is_ascii_alphabetic() {
        return None;
    }
    let after = &rest[1..];
    if !after.is_empty() && !after.starts_with('/') && !after.starts_with('\\') {
        return None;
    }
    Some((letter, after))
}

fn match_drive(path: &str) -> Option<(char, &str)> {
    let bytes = path.as_bytes();
    if bytes.len() < 2 || bytes[0] != b'/' || !bytes[1].is_ascii_alphabetic() {
        return None;
    }
    if bytes.len() > 2 && bytes[2] != b'/' && bytes[2] != b'\\' {
        return None;
    }
    Some((bytes[1] as char, &path[2..]))
}

fn join_drive(letter: char, rest: &str) -> String {
    let normalized_rest = rest.replace('\\', "/");
    if normalized_rest.is_empty() {
        format!("{}:/", letter.to_ascii_uppercase())
    } else {
        format!("{}:{normalized_rest}", letter.to_ascii_uppercase())
    }
}

fn is_bare_drive(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

fn is_win32_drive_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
}

fn locate_cygpath(shell_path: &str) -> Option<PathBuf> {
    let shell_dir = Path::new(shell_path).parent()?;
    let mut candidates = vec![shell_dir.join("cygpath.exe")];
    if shell_dir
        .file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case("bin"))
    {
        candidates.push(
            shell_dir
                .join("..")
                .join("usr")
                .join("bin")
                .join("cygpath.exe"),
        );
    }
    candidates.into_iter().find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lexical(path: &str) -> String {
        translate_shell_drive_path(&normalize_path(path, PathClass::Posix))
    }

    #[test]
    fn drive_forms_translate_lexically() {
        assert_eq!(lexical("/c/Users/x"), "C:/Users/x");
        assert_eq!(lexical("/c:/Users/x"), "C:/Users/x");
        assert_eq!(lexical("/cygdrive/c/Users/x"), "C:/Users/x");
        assert_eq!(lexical("/g/kimi/kimi-code"), "G:/kimi/kimi-code");
        assert_eq!(lexical("/c"), "C:/");
        assert_eq!(lexical("/c/"), "C:/");
    }

    #[test]
    fn non_drive_paths_are_left_for_cygpath() {
        assert_eq!(lexical("/tmp/x.txt"), "/tmp/x.txt");
        assert_eq!(lexical("/usr/bin/bash"), "/usr/bin/bash");
        assert_eq!(lexical("/home/user/a.txt"), "/home/user/a.txt");

        assert_eq!(lexical("/cygwin/c/x"), "/cygwin/c/x");
    }

    #[test]
    fn dot_segments_resolve_before_the_lexical_pass() {
        assert_eq!(lexical("/./c/x"), "C:/x");
        assert_eq!(lexical("/../c/x"), "C:/x");

        assert_eq!(lexical("/c/../d/x"), "D:/x");
    }

    #[test]
    fn unc_and_relative_paths_pass_through() {
        let bridge = ShellPathBridge {
            cygpath: None,
            roots: Mutex::new(HashMap::new()),
        };
        assert_eq!(bridge.to_native_path("//server/share"), "//server/share");
        assert_eq!(bridge.to_native_path("relative/x"), "relative/x");
        assert_eq!(bridge.to_native_path("C:/x"), "C:/x");
    }

    #[test]
    fn virtual_filesystems_and_root_pass_through() {
        let bridge = ShellPathBridge {
            cygpath: None,
            roots: Mutex::new(HashMap::new()),
        };
        assert_eq!(bridge.to_native_path("/"), "/");
        assert_eq!(bridge.to_native_path("/dev/null"), "/dev/null");
        assert_eq!(bridge.to_native_path("/proc/self"), "/proc/self");
        assert_eq!(bridge.to_native_path("/sys/x"), "/sys/x");
    }

    #[test]
    fn an_unresolvable_root_falls_back_to_the_normalized_path() {
        let bridge = ShellPathBridge {
            cygpath: None,
            roots: Mutex::new(HashMap::new()),
        };
        assert_eq!(bridge.to_native_path("/tmp/x.txt"), "/tmp/x.txt");
    }

    #[test]
    fn drive_helpers_reject_near_misses() {
        assert_eq!(match_drive("/cygwin/c"), None);
        assert_eq!(match_drive("/1/x"), None);
        assert_eq!(match_cygdrive("/cygdrive/1/x"), None);
        assert_eq!(match_cygdrive("/cygdrive/cx"), None);
        assert_eq!(match_drive_colon("/c:x"), None);
        assert!(is_bare_drive("C:"));
        assert!(!is_bare_drive("C:/"));
        assert!(is_win32_drive_absolute("C:/x"));
        assert!(!is_win32_drive_absolute("C:x"));
    }
}
