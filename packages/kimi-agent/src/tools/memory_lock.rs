use std::fs::{self, File, TryLockError};
use std::path::Path;
use std::time::{Duration, Instant};

const LOCK_WAIT: Duration = Duration::from_millis(2_000);

const LOCK_RETRY: Duration = Duration::from_millis(100);

#[derive(Debug)]
pub struct MemoryLock {
    file: Option<File>,
}

impl MemoryLock {
    pub fn acquire(path: &Path) -> Option<Self> {
        Self::acquire_with_wait(path, LOCK_WAIT)
    }

    pub fn acquire_with_wait(path: &Path, wait: Duration) -> Option<Self> {
        if let Some(parent) = path.parent()
            && fs::create_dir_all(parent).is_err()
        {
            return None;
        }
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .ok()?;
        let deadline = Instant::now() + wait;
        loop {
            match file.try_lock() {
                Ok(()) => return Some(Self { file: Some(file) }),
                Err(TryLockError::WouldBlock) => {}
                Err(TryLockError::Error(_)) => return None,
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(LOCK_RETRY);
        }
    }
}

impl Drop for MemoryLock {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            let _ = file.unlock();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_held_lock_cannot_be_acquired_twice() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("locks/memory-write.lock");
        let held = MemoryLock::acquire(&path).expect("first acquire");
        assert!(MemoryLock::acquire_with_wait(&path, Duration::from_millis(150)).is_none());
        drop(held);
        assert!(MemoryLock::acquire_with_wait(&path, Duration::from_millis(150)).is_some());
    }
}
