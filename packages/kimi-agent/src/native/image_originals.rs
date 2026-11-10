use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

const DEFAULT_MAX_TOTAL_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Clone, Default)]
pub struct PersistOriginalImageOptions {
    pub dir: Option<PathBuf>,
    pub max_total_bytes: Option<u64>,
}

pub fn original_image_cache_dir() -> PathBuf {
    std::env::temp_dir().join("kimi-code-original-images")
}

pub fn session_media_originals_dir(session_dir: &Path) -> PathBuf {
    session_dir.join("media-originals")
}

fn extension(mime_type: &str) -> &'static str {
    match mime_type.trim().to_ascii_lowercase().as_str() {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/bmp" => "bmp",
        "image/tiff" => "tif",
        _ => "img",
    }
}

pub fn persist_original_image(
    bytes: &[u8],
    mime_type: &str,
    options: &PersistOriginalImageOptions,
) -> Option<PathBuf> {
    if bytes.is_empty() {
        return None;
    }
    (|| -> std::io::Result<Option<PathBuf>> {
        let dir = options.dir.clone().unwrap_or_else(original_image_cache_dir);
        let max_total_bytes = options.max_total_bytes.unwrap_or(DEFAULT_MAX_TOTAL_BYTES);
        let hash = format!("{:x}", Sha256::digest(bytes));
        let path = dir.join(format!("{}.{}", &hash[..32], extension(mime_type)));
        std::fs::create_dir_all(&dir)?;
        let replace = std::fs::metadata(&path)
            .map(|metadata| metadata.len() != bytes.len() as u64)
            .unwrap_or(true);
        if replace {
            std::fs::write(&path, bytes)?;
        }
        sweep_cache(&dir, max_total_bytes)?;
        std::fs::metadata(&path).map(|_| Some(path))
    })()
    .ok()
    .flatten()
}

fn sweep_cache(dir: &Path, max_total_bytes: u64) -> std::io::Result<()> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        let Ok(metadata) = std::fs::metadata(&path) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let mtime_ms = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0);
        entries.push((path, metadata.len(), mtime_ms));
    }
    let mut total = entries.iter().map(|(_, size, _)| *size).sum::<u64>();
    if total <= max_total_bytes {
        return Ok(());
    }
    entries.sort_by_key(|(_, _, mtime_ms)| *mtime_ms);
    for (path, size, _) in entries {
        if total <= max_total_bytes {
            break;
        }
        let _ = std::fs::remove_file(path);
        total -= size;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::Digest;

    #[test]
    fn empty_input_is_not_persisted_and_nonempty_input_uses_the_v2_hash_and_extension() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let options = PersistOriginalImageOptions {
            dir: Some(cache.clone()),
            max_total_bytes: None,
        };
        assert_eq!(persist_original_image(&[], "image/png", &options), None);
        assert!(!cache.exists());

        let bytes = b"original image";
        let path = persist_original_image(bytes, " Image/JPG ", &options).unwrap();
        let hash = format!("{:x}", sha2::Sha256::digest(bytes));
        assert_eq!(
            path.file_name().unwrap().to_string_lossy(),
            format!("{}.jpg", &hash[..32])
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);

        let other = persist_original_image(bytes, "application/octet-stream", &options).unwrap();
        assert_eq!(
            other.file_name().unwrap().to_string_lossy(),
            format!("{}.img", &hash[..32])
        );
        assert_eq!(std::fs::read(&other).unwrap(), bytes);
    }

    #[test]
    fn cache_sweep_removes_oldest_files_until_the_budget_is_met() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let options = PersistOriginalImageOptions {
            dir: Some(cache.clone()),
            max_total_bytes: Some(10),
        };
        for (index, byte) in [1u8, 2, 3].into_iter().enumerate() {
            let path = persist_original_image(&[byte; 4], "image/png", &options).unwrap();
            let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            file.set_times(std::fs::FileTimes::new().set_modified(
                std::time::UNIX_EPOCH + std::time::Duration::from_secs(index as u64),
            ))
            .unwrap();
        }
        let first = persist_original_image(&[4u8; 4], "image/png", &options).unwrap();
        assert!(first.exists());
        let entries = std::fs::read_dir(&cache)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        let total = entries
            .iter()
            .map(|path| std::fs::metadata(path).unwrap().len())
            .sum::<u64>();
        assert!(total <= 10, "total={total}");
        assert_eq!(entries.len(), 2);

        let zero = PersistOriginalImageOptions {
            dir: Some(cache.clone()),
            max_total_bytes: Some(0),
        };
        assert_eq!(persist_original_image(&[5u8], "image/png", &zero), None);
        assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 0);
    }

    #[test]
    fn filesystem_failures_return_null_instead_of_leaking_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-a-directory");
        std::fs::write(&file, b"x").unwrap();
        let options = PersistOriginalImageOptions {
            dir: Some(file),
            max_total_bytes: None,
        };
        assert_eq!(persist_original_image(b"abc", "image/png", &options), None);
    }

    #[test]
    fn cache_directory_helpers_match_the_v2_layout() {
        assert_eq!(
            original_image_cache_dir(),
            std::env::temp_dir().join("kimi-code-original-images")
        );
        let session = std::path::Path::new("session-dir");
        assert_eq!(
            session_media_originals_dir(session),
            session.join("media-originals")
        );
    }
}
