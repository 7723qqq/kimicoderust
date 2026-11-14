#[cfg(feature = "napi")]
use napi_derive::napi;

use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
const S_IFMT: u32 = 0o170000;
#[cfg(unix)]
const S_IFDIR: u32 = 0o040000;

#[derive(Debug, Clone)]
#[cfg_attr(feature = "napi", napi(object))]
pub struct WriteResult {
    pub bytes_written: i32,
    pub error: Option<String>,

    pub error_kind: Option<String>,
}

impl WriteResult {
    fn err(kind: &str, message: String) -> Self {
        Self {
            bytes_written: 0,
            error: Some(message),
            error_kind: Some(kind.to_string()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteMode {
    Overwrite,
    Append,
}

pub fn write_file(path: &str, content: &str, mode: WriteMode, atomic: bool) -> WriteResult {
    if mode == WriteMode::Overwrite && atomic {
        return write_file_atomic(path, content);
    }
    write_file_in_place(path, content, mode)
}

fn write_file_in_place(path: &str, content: &str, mode: WriteMode) -> WriteResult {
    let file_path = Path::new(path);

    if let Some(parent) = file_path.parent()
        && !parent.as_os_str().is_empty()
        && let Err(e) = ensure_parent_directory(parent)
    {
        let kind = if e.starts_with("Parent path is not a directory") {
            "parent_not_dir"
        } else {
            "io"
        };
        return WriteResult::err(kind, e);
    }

    let file = match mode {
        WriteMode::Overwrite => fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(file_path),
        WriteMode::Append => fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(file_path),
    };

    let mut file = match file {
        Ok(f) => f,
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                return WriteResult::err(
                    "io",
                    format!("Failed to write {}: parent directory does not exist.", path),
                );
            }
            return WriteResult::err("io", e.to_string());
        }
    };

    match file.write_all(content.as_bytes()) {
        Ok(()) => WriteResult {
            bytes_written: content.len() as i32,
            error: None,
            error_kind: None,
        },
        Err(e) => WriteResult::err("io", e.to_string()),
    }
}

fn write_file_atomic(path: &str, content: &str) -> WriteResult {
    let file_path = Path::new(path);

    if let Some(parent) = file_path.parent()
        && !parent.as_os_str().is_empty()
        && let Err(e) = ensure_parent_directory(parent)
    {
        let kind = if e.starts_with("Parent path is not a directory") {
            "parent_not_dir"
        } else {
            "io"
        };
        return WriteResult::err(kind, e);
    }

    if let Ok(meta) = fs::symlink_metadata(file_path) {
        let file_type = meta.file_type();
        if file_type.is_symlink() || !file_type.is_file() {
            return write_file_in_place(path, content, WriteMode::Overwrite);
        }
    }

    let file_name = file_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("file");
    let temp_name = format!(".{}.{}.tmp", file_name, unique_suffix());
    let temp_path = file_path.with_file_name(&temp_name);

    let result = (|| -> std::io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        drop(file);

        if let Ok(meta) = fs::metadata(file_path) {
            let _ = fs::set_permissions(&temp_path, meta.permissions());
        }

        fs::rename(&temp_path, file_path)?;
        Ok(())
    })();

    match result {
        Ok(()) => WriteResult {
            bytes_written: content.len() as i32,
            error: None,
            error_kind: None,
        },
        Err(e) => {
            let _ = fs::remove_file(&temp_path);
            WriteResult::err("io", e.to_string())
        }
    }
}

fn unique_suffix() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:x}-{}-{count}", std::process::id())
}

fn ensure_parent_directory(parent: &Path) -> Result<(), String> {
    match fs::metadata(parent) {
        Ok(meta) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = meta.permissions().mode();
                if (mode & S_IFMT) != S_IFDIR {
                    return Err(format!(
                        "Parent path is not a directory: {}",
                        parent.display()
                    ));
                }
            }
            #[cfg(windows)]
            {
                if !meta.is_dir() {
                    return Err(format!(
                        "Parent path is not a directory: {}",
                        parent.display()
                    ));
                }
            }
            Ok(())
        }
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                fs::create_dir_all(parent).map_err(|e| e.to_string())
            } else {
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use tempfile::TempDir;

    #[test]
    fn test_write_new_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        let result = write_file(
            path.to_str().unwrap(),
            "hello world",
            WriteMode::Overwrite,
            true,
        );
        assert!(result.error.is_none());
        assert_eq!(result.bytes_written, 11);

        let mut content = String::new();
        fs::File::open(&path)
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();
        assert_eq!(content, "hello world");
    }

    #[test]
    fn test_write_append() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");

        write_file(path.to_str().unwrap(), "hello ", WriteMode::Overwrite, true);
        let result = write_file(path.to_str().unwrap(), "world", WriteMode::Append, false);
        assert!(result.error.is_none());
        assert_eq!(result.bytes_written, 5);

        let mut content = String::new();
        fs::File::open(&path)
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();
        assert_eq!(content, "hello world");
    }

    #[test]
    fn test_write_creates_parent_dirs() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a").join("b").join("c").join("test.txt");
        let result = write_file(path.to_str().unwrap(), "nested", WriteMode::Overwrite, true);
        assert!(result.error.is_none());
        assert_eq!(result.bytes_written, 6);

        let mut content = String::new();
        fs::File::open(&path)
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();
        assert_eq!(content, "nested");
    }

    #[test]
    fn test_write_overwrite_truncates() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");

        write_file(
            path.to_str().unwrap(),
            "long content here",
            WriteMode::Overwrite,
            true,
        );
        let result = write_file(path.to_str().unwrap(), "short", WriteMode::Overwrite, true);
        assert!(result.error.is_none());

        let mut content = String::new();
        fs::File::open(&path)
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();
        assert_eq!(content, "short");
    }

    #[test]
    fn test_write_utf8_bytes() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");

        let result = write_file(path.to_str().unwrap(), "你好", WriteMode::Overwrite, true);
        assert!(result.error.is_none());
        assert_eq!(result.bytes_written, 6);
    }

    #[test]
    fn test_write_error_kinds() {
        let dir = TempDir::new().unwrap();

        let blocker = dir.path().join("blocker");
        fs::write(&blocker, b"x").unwrap();
        let target = blocker.join("child.txt");
        let result = write_file(target.to_str().unwrap(), "data", WriteMode::Overwrite, true);
        assert_eq!(result.error_kind.as_deref(), Some("parent_not_dir"));
        assert!(result.error.unwrap().contains("not a directory"));

        let ok = dir.path().join("ok.txt");
        let result = write_file(ok.to_str().unwrap(), "data", WriteMode::Overwrite, true);
        assert!(result.error.is_none());
        assert_eq!(result.error_kind, None);
    }

    #[test]
    fn test_write_append_creates_new_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("new.txt");

        let result = write_file(path.to_str().unwrap(), "first", WriteMode::Append, false);
        assert!(result.error.is_none());
        assert_eq!(result.bytes_written, 5);

        let mut content = String::new();
        fs::File::open(&path)
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();
        assert_eq!(content, "first");
    }

    #[test]
    fn test_write_empty_content() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("empty.txt");

        let result = write_file(path.to_str().unwrap(), "", WriteMode::Overwrite, true);
        assert!(result.error.is_none());
        assert_eq!(result.bytes_written, 0);

        let meta = fs::metadata(&path).unwrap();
        assert_eq!(meta.len(), 0);
    }

    #[test]
    fn test_write_atomic_overwrite_replaces_content_and_leaves_no_temp() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("atomic.txt");
        fs::write(&path, b"old").unwrap();

        let result = write_file(
            path.to_str().unwrap(),
            "new content",
            WriteMode::Overwrite,
            true,
        );
        assert!(result.error.is_none());
        assert_eq!(result.bytes_written, 11);

        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content, "new content");

        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "atomic write left temp files behind");
    }

    #[cfg(unix)]
    #[test]
    fn test_write_atomic_preserves_symlink_target() {
        use std::os::unix::fs::symlink;

        let dir = TempDir::new().unwrap();
        let target = dir.path().join("real.txt");
        let link = dir.path().join("link.txt");
        fs::write(&target, b"original").unwrap();
        symlink(&target, &link).unwrap();

        let result = write_file(
            link.to_str().unwrap(),
            "via link",
            WriteMode::Overwrite,
            true,
        );
        assert!(result.error.is_none());

        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(&target).unwrap(), "via link");
        assert_eq!(fs::read_to_string(&link).unwrap(), "via link");
    }
}
