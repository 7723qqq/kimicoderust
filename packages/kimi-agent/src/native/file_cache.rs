use crate::native::read::{MAX_LINES, ReadResult};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::sync::LazyLock;
use std::time::SystemTime;

const MAX_CACHE_ENTRIES: usize = 32;

#[derive(Clone, PartialEq, Debug)]
pub struct FileSnapshot {
    mtime: SystemTime,
    ctime: Option<SystemTime>,
    size: u64,
}

pub fn snapshot(path: &str) -> Option<FileSnapshot> {
    let meta = fs::metadata(path).ok()?;
    Some(FileSnapshot {
        mtime: meta.modified().ok()?,
        ctime: ctime_of(&meta),
        size: meta.len(),
    })
}

#[cfg(unix)]
fn ctime_of(meta: &fs::Metadata) -> Option<SystemTime> {
    use std::os::unix::fs::MetadataExt;
    use std::time::Duration;
    let secs = meta.ctime();
    let nsecs = meta.ctime_nsec();
    Some(
        SystemTime::UNIX_EPOCH
            + Duration::new(if secs < 0 { 0 } else { secs as u64 }, nsecs as u32),
    )
}

#[cfg(not(unix))]
fn ctime_of(_meta: &fs::Metadata) -> Option<SystemTime> {
    None
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct CacheKey {
    path: String,
    line_offset: i64,
    n_lines: u32,
}

impl CacheKey {
    fn new(path: &str, line_offset: Option<i64>, n_lines: Option<u32>) -> Self {
        let offset = match line_offset {
            Some(o) if o == 0 || o == 1 => 1,
            Some(o) => o,
            None => 1,
        };
        let lines = match n_lines {
            Some(n) if n < MAX_LINES as u32 => n,
            _ => MAX_LINES as u32,
        };
        Self {
            path: path.to_string(),
            line_offset: offset,
            n_lines: lines,
        }
    }
}

#[derive(Clone)]
struct CacheEntry {
    result: ReadResult,
    snap: FileSnapshot,
}

pub struct FileReadCache {
    inner: std::sync::Mutex<CacheInner>,
}

struct CacheInner {
    entries: HashMap<CacheKey, CacheEntry>,

    order: VecDeque<CacheKey>,
}

impl FileReadCache {
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(CacheInner {
                entries: HashMap::new(),
                order: VecDeque::new(),
            }),
        }
    }

    pub fn get(
        &self,
        path: &str,
        line_offset: Option<i64>,
        n_lines: Option<u32>,
    ) -> Option<ReadResult> {
        let current = snapshot(path)?;
        let key = CacheKey::new(path, line_offset, n_lines);
        let mut inner = self.inner.lock().ok()?;
        match inner.entries.get(&key) {
            Some(entry) if entry.snap == current => {
                let result = entry.result.clone();
                if let Some(pos) = inner.order.iter().position(|k| *k == key)
                    && let Some(k) = inner.order.remove(pos)
                {
                    inner.order.push_back(k);
                }
                Some(result)
            }
            Some(_) => {
                inner.entries.remove(&key);
                if let Some(pos) = inner.order.iter().position(|k| *k == key) {
                    inner.order.remove(pos);
                }
                None
            }
            None => None,
        }
    }

    pub fn put(
        &self,
        path: String,
        line_offset: Option<i64>,
        n_lines: Option<u32>,
        result: ReadResult,
        pre_read: FileSnapshot,
    ) {
        let post = match snapshot(&path) {
            Some(s) => s,
            None => return,
        };
        if post != pre_read {
            self.invalidate(&path);
            return;
        }
        let key = CacheKey::new(&path, line_offset, n_lines);
        let mut inner = match self.inner.lock() {
            Ok(c) => c,
            Err(_) => return,
        };

        if inner.entries.len() >= MAX_CACHE_ENTRIES
            && !inner.entries.contains_key(&key)
            && let Some(oldest) = inner.order.pop_front()
        {
            inner.entries.remove(&oldest);
        }

        if let Some(pos) = inner.order.iter().position(|k| *k == key) {
            let _ = inner.order.remove(pos);
        }
        inner
            .entries
            .insert(key.clone(), CacheEntry { result, snap: post });
        inner.order.push_back(key);
    }

    pub fn invalidate(&self, path: &str) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.entries.retain(|k, _| k.path != path);
            inner.order.retain(|k| k.path != path);
        }
    }
}

pub static FILE_CACHE: LazyLock<FileReadCache> = LazyLock::new(FileReadCache::new);

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_temp(dir: &tempfile::TempDir, name: &str, content: &[u8]) -> std::path::PathBuf {
        let p = dir.path().join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(content).unwrap();
        f.flush().unwrap();
        p
    }

    fn ok_result(content: &str, line_count: i32) -> ReadResult {
        ReadResult {
            content: content.to_string(),
            line_count,
            error: None,
            error_kind: None,
        }
    }

    #[test]
    fn hit_then_miss_on_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp(&dir, "a.txt", b"hello");
        let ps = path.to_string_lossy().to_string();
        let cache = FileReadCache::new();

        assert!(cache.get(&ps, None, None).is_none());
        let pre = snapshot(&ps).unwrap();
        cache.put(ps.clone(), None, None, ok_result("hello", 1), pre);
        assert_eq!(
            cache.get(&ps, None, None).map(|r| r.content),
            Some("hello".to_string())
        );

        write_temp(&dir, "a.txt", b"world!");
        assert!(cache.get(&ps, None, None).is_none());
    }

    #[test]
    fn put_after_concurrent_change_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp(&dir, "b.txt", b"v1");
        let ps = path.to_string_lossy().to_string();
        let cache = FileReadCache::new();

        let pre = snapshot(&ps).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(25));
        write_temp(&dir, "b.txt", b"v2");
        cache.put(ps.clone(), None, None, ok_result("v1", 1), pre);
        assert!(cache.get(&ps, None, None).is_none());
    }

    #[test]
    fn invalidate_removes_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp(&dir, "c.txt", b"data");
        let ps = path.to_string_lossy().to_string();
        let cache = FileReadCache::new();

        let pre = snapshot(&ps).unwrap();
        cache.put(ps.clone(), None, None, ok_result("data", 1), pre);
        assert!(cache.get(&ps, None, None).is_some());
        cache.invalidate(&ps);
        assert!(cache.get(&ps, None, None).is_none());
    }

    #[test]
    fn evicts_least_recently_used_when_at_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FileReadCache::new();

        for i in 0..MAX_CACHE_ENTRIES + 1 {
            let path = write_temp(&dir, &format!("f{i:03}.txt"), b"x");
            let ps = path.to_string_lossy().to_string();
            let pre = snapshot(&ps).unwrap();
            cache.put(ps.clone(), None, None, ok_result(&format!("v{i}"), 1), pre);
        }

        let first = dir.path().join(format!("f{:03}.txt", 0));
        let first_ps = first.to_string_lossy().to_string();
        assert!(
            cache.get(&first_ps, None, None).is_none(),
            "oldest entry should be evicted"
        );
        for i in 1..MAX_CACHE_ENTRIES + 1 {
            let path = dir.path().join(format!("f{i:03}.txt"));
            let ps = path.to_string_lossy().to_string();
            assert_eq!(
                cache.get(&ps, None, None).map(|r| r.content),
                Some(format!("v{i}")),
                "entry {i} should remain cached"
            );
        }
    }

    #[test]
    fn get_missing_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FileReadCache::new();
        let ps = dir.path().join("nope.txt").to_string_lossy().to_string();

        assert!(cache.get(&ps, None, None).is_none());
    }

    #[test]
    fn put_missing_file_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FileReadCache::new();
        let ps = dir.path().join("nope.txt").to_string_lossy().to_string();

        let pre = FileSnapshot {
            mtime: std::time::SystemTime::now(),
            ctime: None,
            size: 0,
        };
        cache.put(ps.clone(), None, None, ok_result("ghost", 1), pre);
        assert!(cache.get(&ps, None, None).is_none());
    }

    #[test]
    fn invalidate_unknown_path_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FileReadCache::new();
        let ps = dir.path().join("ghost.txt").to_string_lossy().to_string();
        cache.invalidate(&ps);

        let path = write_temp(&dir, "ok.txt", b"data");
        let ps2 = path.to_string_lossy().to_string();
        let pre = snapshot(&ps2).unwrap();
        cache.put(ps2.clone(), None, None, ok_result("data", 1), pre);
        assert!(cache.get(&ps2, None, None).is_some());
    }

    #[test]
    fn equivalent_requests_share_one_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp(&dir, "eq.txt", b"line1\nline2\n");
        let ps = path.to_string_lossy().to_string();
        let cache = FileReadCache::new();

        let pre = snapshot(&ps).unwrap();
        cache.put(
            ps.clone(),
            Some(1),
            Some(MAX_LINES as u32),
            ok_result("full", 2),
            pre,
        );
        assert_eq!(
            cache.get(&ps, None, None).map(|r| r.content),
            Some("full".to_string())
        );

        assert_eq!(
            cache
                .get(&ps, Some(0), Some(MAX_LINES as u32 + 5))
                .map(|r| r.content),
            Some("full".to_string())
        );
    }

    #[test]
    fn partial_reads_are_distinct_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp(&dir, "part.txt", b"line1\nline2\nline3\n");
        let ps = path.to_string_lossy().to_string();
        let cache = FileReadCache::new();

        let pre = snapshot(&ps).unwrap();
        cache.put(ps.clone(), Some(2), Some(1), ok_result("line2", 3), pre);

        assert!(cache.get(&ps, Some(1), Some(1)).is_none());

        assert_eq!(
            cache.get(&ps, Some(2), Some(1)).map(|r| r.content),
            Some("line2".to_string())
        );
    }

    #[test]
    fn tail_reads_normalize_n_lines_but_keep_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp(&dir, "tail.txt", b"l1\nl2\nl3\nl4\nl5\n");
        let ps = path.to_string_lossy().to_string();
        let cache = FileReadCache::new();

        let pre = snapshot(&ps).unwrap();
        cache.put(ps.clone(), Some(-3), None, ok_result("tail3", 5), pre);
        assert_eq!(
            cache
                .get(&ps, Some(-3), Some(MAX_LINES as u32))
                .map(|r| r.content),
            Some("tail3".to_string())
        );

        assert!(cache.get(&ps, Some(-2), None).is_none());
    }

    #[test]
    fn stale_entry_is_dropped_on_get() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp(&dir, "stale.txt", b"v1");
        let ps = path.to_string_lossy().to_string();
        let cache = FileReadCache::new();

        let pre = snapshot(&ps).unwrap();
        cache.put(ps.clone(), None, None, ok_result("v1", 1), pre);

        write_temp(&dir, "stale.txt", b"v2-longer");
        assert!(cache.get(&ps, None, None).is_none());

        let pre2 = snapshot(&ps).unwrap();
        cache.put(ps.clone(), None, None, ok_result("v2-longer", 1), pre2);
        assert_eq!(
            cache.get(&ps, None, None).map(|r| r.content),
            Some("v2-longer".to_string())
        );
    }
}
