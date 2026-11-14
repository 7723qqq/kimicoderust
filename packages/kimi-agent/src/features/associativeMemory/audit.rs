use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

impl AuditKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WriteBlocked => "write_blocked",
            Self::WriteAllowed => "write_allowed",
            Self::WriteNoop => "write_noop",
            Self::WriteUpdated => "write_updated",
            Self::RecallFiltered => "recall_filtered",
            Self::RecallGated => "recall_gated",
            Self::InjectionFiltered => "injection_filtered",
            Self::Consolidate => "consolidate",
            Self::EvictAtoms => "evict_atoms",
            Self::EvictNotes => "evict_notes",
            Self::Pin => "pin",
            Self::Forget => "forget",
            Self::Supersede => "supersede",
            Self::Reconsolidate => "reconsolidate",
            Self::EmbeddingsBlocked => "embeddings_blocked",
            Self::EmbeddingsFailed => "embeddings_failed",
            Self::Replay => "replay",
            Self::Rebuild => "rebuild",
            Self::CodeProposed => "code_proposed",
            Self::CodeApproved => "code_approved",
            Self::Label => "label",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditKind {
    WriteBlocked,
    WriteAllowed,
    WriteNoop,
    WriteUpdated,
    RecallFiltered,
    RecallGated,
    InjectionFiltered,
    Consolidate,
    EvictAtoms,
    EvictNotes,
    Pin,
    Forget,
    Supersede,
    Reconsolidate,
    EmbeddingsBlocked,
    EmbeddingsFailed,
    Replay,
    Rebuild,
    CodeProposed,
    CodeApproved,
    Label,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditEntry {
    pub at: i64,
    pub kind: AuditKind,
    pub detail: serde_json::Map<String, serde_json::Value>,
}

pub const AUDIT_MAX_ENTRIES: usize = 1000;
pub const AUDIT_ROTATE_AT: usize = 2000;
pub const AUDIT_TAIL_BYTES: u64 = 262_144;

pub fn audit_path(store_dir: &str, store_root: &str) -> PathBuf {
    Path::new(store_dir).join(store_root).join("audit.jsonl")
}

pub async fn append_audit(path: &Path, entry: &AuditEntry) -> Result<(), String> {
    let body = serde_json::to_string(entry)
        .map_err(|error| format!("encode audit entry: {error}"))?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .await
        .map_err(|error| format!("create audit dir {}: {error}", parent.display()))?;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
        .map_err(|error| format!("open {}: {error}", path.display()))?;
    file.write_all(format!("{body}\n").as_bytes())
        .await
        .map_err(|error| format!("append {}: {error}", path.display()))?;
    drop(file);
    rotate_audit(path).await?;
    Ok(())
}

async fn rotate_audit(path: &Path) -> Result<(), String> {
    let size = match fs::metadata(path).await {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("stat {}: {error}", path.display())),
    };
    if size <= AUDIT_TAIL_BYTES * 2 {
        return Ok(());
    }
    let content = fs::read(path)
        .await
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    let content = String::from_utf8_lossy(&content);
    let lines: Vec<&str> = content
        .split('\n')
        .filter(|line| !trim_js(line).is_empty())
        .collect();
    if lines.len() <= AUDIT_ROTATE_AT {
        return Ok(());
    }
    let kept = &lines[lines.len() - AUDIT_MAX_ENTRIES..];
    write_audit(path, kept).await
}

async fn write_audit(path: &Path, lines: &[&str]) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .await
        .map_err(|error| format!("create audit dir {}: {error}", parent.display()))?;
    fs::write(path, format!("{}\n", lines.join("\n")))
        .await
        .map_err(|error| format!("write {}: {error}", path.display()))
}

pub async fn read_audit(path: &Path, limit: usize) -> Result<Vec<AuditEntry>, String> {
    let size = match fs::metadata(path).await {
        Ok(metadata) => metadata.len(),
        Err(_) => return Ok(Vec::new()),
    };
    let start = size.saturating_sub(AUDIT_TAIL_BYTES);
    let mut file = match fs::File::open(path).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => return Ok(Vec::new()),
    };
    if start > 0 {
        if file.seek(std::io::SeekFrom::Start(start)).await.is_err() {
            return Ok(Vec::new());
        }
    }
    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).await.is_err() {
        return Ok(Vec::new());
    }
    let mut content = String::from_utf8_lossy(&bytes).into_owned();
    if start > 0 {
        content = match content.find('\n') {
            Some(index) => content[index + 1..].to_string(),
            None => String::new(),
        };
    }
    let mut entries: Vec<AuditEntry> = Vec::new();
    for line in content.split('\n') {
        if trim_js(line).is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<AuditEntry>(line) {
            entries.push(entry);
        }
    }
    entries.reverse();
    entries.truncate(limit);
    Ok(entries)
}

pub async fn write_audit_entries(path: &Path, entries: &[AuditEntry]) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .await
        .map_err(|error| format!("create audit dir {}: {error}", parent.display()))?;
    let lines: Vec<String> = entries
        .iter()
        .map(|entry| {
            serde_json::to_string(entry).map_err(|error| format!("encode audit entry: {error}"))
        })
        .collect::<Result<_, _>>()?;
    fs::write(path, format!("{}\n", lines.join("\n")))
        .await
        .map_err(|error| format!("write {}: {error}", path.display()))
}

fn trim_js(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

fn is_js_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'
            | '\u{000a}'
            | '\u{000b}'
            | '\u{000c}'
            | '\u{000d}'
            | '\u{0020}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'
            | '\u{2001}'
            | '\u{2002}'
            | '\u{2003}'
            | '\u{2004}'
            | '\u{2005}'
            | '\u{2006}'
            | '\u{2007}'
            | '\u{2008}'
            | '\u{2009}'
            | '\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn temp_path(name: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        (dir, path)
    }

    fn entry(at: i64, kind: AuditKind) -> AuditEntry {
        AuditEntry {
            at,
            kind,
            detail: serde_json::Map::new(),
        }
    }

    #[test]
    fn audit_kinds_use_the_v2_snake_case_names() {
        for (kind, expected) in [
            (AuditKind::WriteBlocked, "write_blocked"),
            (AuditKind::WriteNoop, "write_noop"),
            (AuditKind::RecallFiltered, "recall_filtered"),
            (AuditKind::InjectionFiltered, "injection_filtered"),
            (AuditKind::EmbeddingsFailed, "embeddings_failed"),
            (AuditKind::CodeProposed, "code_proposed"),
        ] {
            assert_eq!(serde_json::to_string(&kind).unwrap(), format!("\"{expected}\""));
        }
    }

    #[test]
    fn the_audit_path_joins_the_store_dir_root_and_file_name() {
        let path = audit_path("/tmp/store", "scope-a");
        assert_eq!(path, Path::new("/tmp/store/scope-a/audit.jsonl"));
    }

    #[tokio::test]
    async fn append_then_read_returns_newest_first() {
        let (_dir, path) = temp_path("audit.jsonl").await;
        append_audit(&path, &entry(1, AuditKind::Pin)).await.unwrap();
        append_audit(&path, &entry(2, AuditKind::Forget)).await.unwrap();
        let entries = read_audit(&path, 20).await.unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|item| item.kind)
                .collect::<Vec<_>>(),
            vec![AuditKind::Forget, AuditKind::Pin]
        );
    }

    #[tokio::test]
    async fn read_limits_to_the_newest_entries() {
        let (_dir, path) = temp_path("audit.jsonl").await;
        for at in 0..5 {
            append_audit(&path, &entry(at, AuditKind::Label)).await.unwrap();
        }
        let entries = read_audit(&path, 2).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].at, 4);
        assert_eq!(entries[1].at, 3);
    }

    #[tokio::test]
    async fn a_missing_file_reads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let entries = read_audit(&dir.path().join("absent.jsonl"), 10).await.unwrap();
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn malformed_lines_are_skipped() {
        let (_dir, path) = temp_path("audit.jsonl").await;
        fs::create_dir_all(path.parent().unwrap()).await.unwrap();
        fs::write(
            &path,
            format!(
                "not json\n{}\n",
                serde_json::to_string(&entry(7, AuditKind::Replay)).unwrap()
            ),
        )
        .await
        .unwrap();
        let entries = read_audit(&path, 10).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, AuditKind::Replay);
    }

    #[tokio::test]
    async fn write_entries_round_trips_through_the_same_parser() {
        let (_dir, path) = temp_path("audit.jsonl").await;
        let entries = vec![
            entry(1, AuditKind::Consolidate),
            entry(2, AuditKind::Reconsolidate),
        ];
        write_audit_entries(&path, &entries).await.unwrap();
        let loaded = read_audit(&path, 10).await.unwrap();
        assert_eq!(
            loaded
                .iter()
                .map(|item| item.kind)
                .collect::<Vec<_>>(),
            vec![AuditKind::Reconsolidate, AuditKind::Consolidate]
        );
    }

    #[tokio::test]
    async fn rotation_keeps_the_last_max_entries() {
        let (_dir, path) = temp_path("audit.jsonl").await;
        fs::create_dir_all(path.parent().unwrap()).await.unwrap();
        let padding = "x".repeat((AUDIT_TAIL_BYTES * 2 + 16) as usize);
        let mut lines = vec![serde_json::to_string(&AuditEntry {
            at: -1,
            kind: AuditKind::Rebuild,
            detail: serde_json::Map::from_iter([(
                "padding".to_string(),
                serde_json::json!(padding),
            )]),
        })
        .unwrap()];
        for at in 0..(AUDIT_ROTATE_AT + 5) as i64 {
            lines.push(serde_json::to_string(&entry(at, AuditKind::Label)).unwrap());
        }
        fs::write(&path, format!("{}\n", lines.join("\n")))
            .await
            .unwrap();

        append_audit(&path, &entry(9_999, AuditKind::Pin)).await.unwrap();

        let size = fs::metadata(&path).await.unwrap().len();
        assert!(
            size < AUDIT_TAIL_BYTES,
            "the oversized file shrank to the kept tail"
        );
        let content = fs::read_to_string(&path).await.unwrap();
        let kept: Vec<&str> = content
            .split('\n')
            .filter(|line| !line.trim().is_empty())
            .collect();
        assert_eq!(kept.len(), AUDIT_MAX_ENTRIES);
        assert!(
            !content.contains("padding"),
            "the dropped head does not survive the rotation"
        );
        let last: AuditEntry = serde_json::from_str(kept[kept.len() - 1]).unwrap();
        assert_eq!(last.at, 9_999);
        assert_eq!(last.kind, AuditKind::Pin);
    }

    #[tokio::test]
    async fn a_small_file_is_not_rotated() {
        let (_dir, path) = temp_path("audit.jsonl").await;
        for at in 0..3 {
            append_audit(&path, &entry(at, AuditKind::Label)).await.unwrap();
        }
        let content = fs::read_to_string(&path).await.unwrap();
        assert_eq!(content.matches('\n').count(), 3);
    }
}
