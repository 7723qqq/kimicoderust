use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use tokio_util::sync::CancellationToken;

pub fn sync_dir(dir_path: impl AsRef<Path>) -> io::Result<()> {
    #[cfg(windows)]
    {
        let _ = dir_path;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        File::open(dir_path.as_ref())?.sync_all()
    }
}

pub fn sync_dir_sync(dir_path: impl AsRef<Path>) -> io::Result<()> {
    sync_dir(dir_path)
}

pub fn write_file_atomic_durable(file_path: impl AsRef<Path>, content: &[u8]) -> io::Result<()> {
    let file_path = file_path.as_ref();
    let mut tmp_name = OsString::from(file_path.as_os_str());
    tmp_name.push(".tmp");
    atomic_write_using(
        file_path,
        PathBuf::from(tmp_name),
        None,
        None,
        &File::sync_all,
        true,
        |file| file.write_all(content),
    )
}

pub fn atomic_write(
    file_path: impl AsRef<Path>,
    content: &[u8],
    sync_override: Option<&dyn Fn(&File) -> io::Result<()>>,
    mode: Option<u32>,
    signal: Option<&CancellationToken>,
) -> io::Result<()> {
    let file_path = file_path.as_ref();
    let tmp_path = unique_tmp_path(file_path);
    let write = |file: &mut File| file.write_all(content);
    match sync_override {
        Some(sync) => atomic_write_using(file_path, tmp_path, mode, signal, sync, false, write),
        None => atomic_write_using(
            file_path,
            tmp_path,
            mode,
            signal,
            &File::sync_all,
            false,
            write,
        ),
    }
}

pub fn atomic_write_stream<B>(
    file_path: impl AsRef<Path>,
    source: impl IntoIterator<Item = io::Result<B>>,
    mode: Option<u32>,
    signal: Option<&CancellationToken>,
) -> io::Result<()>
where
    B: AsRef<[u8]>,
{
    let file_path = file_path.as_ref();
    let tmp_path = unique_tmp_path(file_path);
    atomic_write_using(
        file_path,
        tmp_path,
        mode,
        signal,
        &File::sync_all,
        false,
        |file| {
            for chunk in source {
                throw_if_cancelled(signal)?;
                let chunk = chunk?;
                let bytes = chunk.as_ref();
                if !bytes.is_empty() {
                    file.write_all(bytes)?;
                }
            }
            throw_if_cancelled(signal)
        },
    )
}

fn unique_tmp_path(file_path: &Path) -> PathBuf {
    let mut random = [0_u8; 4];
    getrandom::fill(&mut random).expect("operating system randomness is unavailable");
    let hex = random.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    let mut tmp_name = OsString::from(file_path.as_os_str());
    tmp_name.push(format!(".tmp.{}.{hex}", std::process::id()));
    PathBuf::from(tmp_name)
}

fn atomic_write_using(
    file_path: &Path,
    tmp_path: PathBuf,
    mode: Option<u32>,
    signal: Option<&CancellationToken>,
    sync: &dyn Fn(&File) -> io::Result<()>,
    sync_directory: bool,
    write: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<()> {
    let mut renamed = false;
    let result = (|| {
        throw_if_cancelled(signal)?;
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        if let Some(mode) = mode {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(mode);
        }
        let mut file = options.open(&tmp_path)?;
        let write_result = (|| {
            throw_if_cancelled(signal)?;
            write(&mut file)?;
            throw_if_cancelled(signal)?;
            sync(&file)
        })();
        drop(file);
        write_result?;
        remove_existing_target(file_path)?;
        throw_if_cancelled(signal)?;
        fs::rename(&tmp_path, file_path)?;
        renamed = true;
        if sync_directory {
            sync_dir(parent_dir(file_path))?;
        }
        Ok(())
    })();
    if !renamed {
        let _ = fs::remove_file(&tmp_path);
    }
    result
}

fn remove_existing_target(file_path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        match fs::remove_file(file_path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = file_path;
        Ok(())
    }
}

fn throw_if_cancelled(signal: Option<&CancellationToken>) -> io::Result<()> {
    if signal.is_some_and(CancellationToken::is_cancelled) {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "operation cancelled",
        ))
    } else {
        Ok(())
    }
}

fn parent_dir(file_path: &Path) -> &Path {
    match file_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::fs::File;
    use std::io;

    use tokio_util::sync::CancellationToken;

    use super::{
        atomic_write, atomic_write_stream, sync_dir, sync_dir_sync, write_file_atomic_durable,
    };

    fn temporary_files(dir: &std::path::Path) -> Vec<String> {
        let mut names = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp"))
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    #[test]
    fn atomic_write_replaces_contents_without_leaving_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("state.json");

        atomic_write(&target, b"first", None, None, None).unwrap();
        atomic_write(&target, b"second", None, None, None).unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"second");
        assert!(temporary_files(dir.path()).is_empty());
    }

    #[test]
    fn atomic_write_uses_the_sync_override() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("state.json");
        let calls = Cell::new(0);
        let sync = |_: &File| {
            calls.set(calls.get() + 1);
            Ok(())
        };

        atomic_write(&target, b"value", Some(&sync), None, None).unwrap();

        assert_eq!(calls.get(), 1);
        assert_eq!(std::fs::read(&target).unwrap(), b"value");
    }

    #[test]
    fn atomic_write_cleans_up_and_preserves_the_target_when_sync_fails() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("state.json");
        std::fs::write(&target, b"old").unwrap();
        let sync = |_: &File| Err(io::Error::other("sync failed"));

        let error = atomic_write(&target, b"new", Some(&sync), None, None).unwrap_err();

        assert_eq!(error.to_string(), "sync failed");
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        assert!(temporary_files(dir.path()).is_empty());
    }

    #[test]
    fn atomic_write_stream_writes_all_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("state.bin");
        let chunks = vec![
            Ok::<Vec<u8>, io::Error>(b"ab".to_vec()),
            Ok(Vec::new()),
            Ok(b"cde".to_vec()),
        ];

        atomic_write_stream(&target, chunks, None, None).unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"abcde");
        assert!(temporary_files(dir.path()).is_empty());
    }

    #[test]
    fn atomic_write_stream_preserves_the_target_when_the_source_fails() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("state.bin");
        std::fs::write(&target, b"old").unwrap();
        let chunks = vec![
            Ok::<Vec<u8>, io::Error>(b"partial".to_vec()),
            Err(io::Error::other("source failed")),
        ];

        let error = atomic_write_stream(&target, chunks, None, None).unwrap_err();

        assert_eq!(error.to_string(), "source failed");
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        assert!(temporary_files(dir.path()).is_empty());
    }

    #[test]
    fn cancelled_atomic_write_leaves_no_file_or_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("state.json");
        let token = CancellationToken::new();
        token.cancel();

        let error = atomic_write(&target, b"value", None, None, Some(&token)).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(!target.exists());
        assert!(temporary_files(dir.path()).is_empty());
    }

    #[test]
    fn durable_write_replaces_contents_and_directory_sync_accepts_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("state.json");

        write_file_atomic_durable(&target, b"first").unwrap();
        write_file_atomic_durable(&target, b"second").unwrap();
        sync_dir(dir.path()).unwrap();
        sync_dir_sync(dir.path()).unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"second");
        assert!(temporary_files(dir.path()).is_empty());
    }
}
