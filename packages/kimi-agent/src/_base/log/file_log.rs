use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use futures_util::future::BoxFuture;
use futures_util::FutureExt;
use parking_lot::Mutex;
use tokio::io::AsyncWriteExt;

use super::formatter::{FormatOptions, format_entry};
use super::log::{ILogWriter, LogEntry, LogEntryLevel};

pub const PENDING_MAX: usize = 1000;
const STDERR_NOTICE_INTERVAL_MS: u64 = 30_000;

#[derive(Clone)]
pub struct RotatingFileWriterOptions {
    pub path: String,
    pub max_bytes: u64,
    pub files: u64,
}

struct WriterState {
    pending: VecDeque<String>,
    dropped: usize,
    closed: bool,
    last_stderr_notice: u64,
    current_bytes: i64,
    directory_synced: bool,
}

pub struct RotatingFileWriter {
    options: RotatingFileWriterOptions,
    queue: tokio::sync::Mutex<()>,
    state: Mutex<WriterState>,
}

impl RotatingFileWriter {
    pub fn new(options: RotatingFileWriterOptions) -> Arc<Self> {
        Arc::new(Self {
            options,
            queue: tokio::sync::Mutex::new(()),
            state: Mutex::new(WriterState {
                pending: VecDeque::new(),
                dropped: 0,
                closed: false,
                last_stderr_notice: 0,
                current_bytes: -1,
                directory_synced: false,
            }),
        })
    }

    pub fn enqueue(self: &Arc<Self>, line: String) {
        {
            let mut state = self.state.lock();
            if state.closed {
                return;
            }
            if state.pending.len() >= PENDING_MAX {
                state.pending.pop_front();
                state.dropped += 1;
            }
            state.pending.push_back(line);
        }
        self.schedule_drain();
    }

    pub async fn flush(&self) -> bool {
        let _guard = self.queue.lock().await;
        self.drain().await
    }

    pub async fn close(&self) {
        if self.state.lock().closed {
            return;
        }
        self.state.lock().closed = true;
        let _ = self.flush().await;
    }

    pub fn flush_sync(&self) {
        let body = {
            let mut state = self.state.lock();
            if state.closed || state.pending.is_empty() {
                return;
            }
            let mut body = state.pending.iter().cloned().collect::<String>();
            body.push_str(&take_dropped_notice(&mut state));
            state.pending.clear();
            body
        };
        let result = (|| -> std::io::Result<()> {
            if let Some(parent) = parent_dir(&self.options.path) {
                std::fs::create_dir_all(parent)?;
            }
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.options.path)?;
            file.write_all(body.as_bytes())
        })();
        if let Err(error) = result {
            self.note_failure(&error);
        }
    }

    fn schedule_drain(self: &Arc<Self>) {
        if self.state.lock().closed {
            return;
        }
        let writer = self.clone();
        tokio::spawn(async move {
            if writer.state.lock().closed || writer.state.lock().pending.is_empty() {
                return;
            }
            let _guard = writer.queue.lock().await;
            let _ = writer.drain().await;
        });
    }

    async fn drain(&self) -> bool {
        let lines = {
            let mut state = self.state.lock();
            if state.pending.is_empty() {
                return true;
            }
            let dropped_line = take_dropped_notice(&mut state);
            let mut lines = state.pending.iter().cloned().collect::<Vec<_>>();
            if !dropped_line.is_empty() {
                lines.push(dropped_line);
            }
            state.pending.clear();
            lines
        };
        let result = async {
            if let Some(parent) = parent_dir(&self.options.path) {
                tokio::fs::create_dir_all(parent).await?;
            }
            if self.state.lock().current_bytes < 0 {
                let size = stat_size(&self.options.path).await?;
                self.state.lock().current_bytes = size as i64;
            }
            self.append_lines(&lines).await?;
            if !self.state.lock().directory_synced {
                sync_dir(parent_dir(&self.options.path).unwrap_or(Path::new("."))).await?;
                self.state.lock().directory_synced = true;
            }
            Ok::<(), std::io::Error>(())
        }
        .await;
        if let Err(error) = result {
            self.note_failure(&error);
            self.restore_pending(lines);
            return false;
        }
        true
    }

    fn restore_pending(&self, lines: Vec<String>) {
        let mut state = self.state.lock();
        let mut restored = lines;
        restored.extend(state.pending.iter().cloned());
        let overflow = restored.len().saturating_sub(PENDING_MAX);
        if overflow > 0 {
            state.dropped += overflow;
            restored.drain(..overflow);
        }
        state.pending = restored.into();
    }

    async fn append_lines(&self, lines: &[String]) -> std::io::Result<()> {
        let mut chunk = String::new();
        let mut chunk_bytes = 0_u64;
        for line in lines {
            let line_bytes = line.len() as u64;
            let current_bytes = self.state.lock().current_bytes as u64;
            if chunk_bytes > 0
                && (chunk_bytes + line_bytes > self.options.max_bytes
                    || current_bytes + chunk_bytes + line_bytes > self.options.max_bytes)
            {
                self.append_chunk(&chunk).await?;
                chunk.clear();
                chunk_bytes = 0;
            }
            let current_bytes = self.state.lock().current_bytes as u64;
            if chunk_bytes == 0
                && current_bytes > 0
                && current_bytes + line_bytes > self.options.max_bytes
            {
                self.rotate().await?;
            }
            chunk.push_str(line);
            chunk_bytes += line_bytes;
        }
        if chunk_bytes > 0 {
            self.append_chunk(&chunk).await?;
        }
        Ok(())
    }

    async fn append_chunk(&self, chunk: &str) -> std::io::Result<()> {
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.options.path)
            .await?;
        file.write_all(chunk.as_bytes()).await?;
        file.sync_all().await?;
        drop(file);
        let current = {
            let mut state = self.state.lock();
            state.current_bytes += chunk.len() as i64;
            state.current_bytes
        };
        if current >= self.options.max_bytes as i64 {
            self.rotate().await?;
        }
        Ok(())
    }

    async fn rotate(&self) -> std::io::Result<()> {
        let path = &self.options.path;
        if self.options.files >= 3 {
            for index in (1..=self.options.files - 2).rev() {
                rename_if_exists(&format!("{path}.{index}"), &format!("{path}.{}", index + 1)).await?;
            }
        }
        rename_if_exists(path, &format!("{path}.1")).await?;
        match tokio::fs::remove_file(format!("{path}.{}", self.options.files)).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let mut state = self.state.lock();
        state.current_bytes = 0;
        state.directory_synced = false;
        Ok(())
    }

    fn note_failure(&self, error: &std::io::Error) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let mut state = self.state.lock();
        if now.saturating_sub(state.last_stderr_notice) < STDERR_NOTICE_INTERVAL_MS {
            return;
        }
        state.last_stderr_notice = now;
        let code = error
            .raw_os_error()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "UNKNOWN".to_string());
        let _ = writeln!(std::io::stderr(), "[logger] write failed: {code}");
    }
}

fn take_dropped_notice(state: &mut WriterState) -> String {
    if state.dropped == 0 {
        return String::new();
    }
    let line = format!("... dropped {} entries ...\n", state.dropped);
    state.dropped = 0;
    line
}

fn parent_dir(path: &str) -> Option<&Path> {
    Path::new(path).parent()
}

async fn sync_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        let _ = path;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let directory = tokio::fs::File::open(path).await?;
        directory.sync_all().await
    }
}

async fn rename_if_exists(from: &str, to: &str) -> std::io::Result<()> {
    match tokio::fs::rename(from, to).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

async fn stat_size(path: &str) -> std::io::Result<u64> {
    match tokio::fs::metadata(path).await {
        Ok(metadata) => Ok(metadata.len()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error),
    }
}

pub struct FileLogWriterOptions {
    pub path: String,
    pub max_bytes: u64,
    pub files: u64,
    pub format: Option<FormatOptions>,
}

pub struct FileLogWriter {
    sink: Arc<RotatingFileWriter>,
    format: FormatOptions,
}

impl FileLogWriter {
    pub fn new(options: FileLogWriterOptions) -> Self {
        let format = options.format.unwrap_or_default();
        Self {
            sink: RotatingFileWriter::new(RotatingFileWriterOptions {
                path: options.path,
                max_bytes: options.max_bytes,
                files: options.files,
            }),
            format,
        }
    }
}

impl ILogWriter for FileLogWriter {
    fn write(&self, entry: LogEntry) {
        let formatted = format_entry(
            &entry,
            FormatOptions {
                ansi: self.format.ansi,
                omit_context_keys: self.format.omit_context_keys.clone(),
            },
        );
        if !formatted.dropped {
            self.sink.enqueue(formatted.text + "\n");
        }
    }

    fn flush(&self) -> BoxFuture<'static, ()> {
        let sink = self.sink.clone();
        async move {
            sink.flush().await;
        }
        .boxed()
    }

    fn close(&self) -> BoxFuture<'static, ()> {
        let sink = self.sink.clone();
        async move { sink.close().await }.boxed()
    }

    fn flush_sync(&self) {
        self.sink.flush_sync();
    }
}

pub fn create_file_log_writer(options: FileLogWriterOptions) -> FileLogWriter {
    FileLogWriter::new(options)
}

#[derive(Default)]
pub struct MemoryLogWriter {
    entries: Mutex<Vec<LogEntry>>,
}

impl MemoryLogWriter {
    pub fn entries(&self) -> Vec<LogEntry> {
        self.entries.lock().clone()
    }
}

impl ILogWriter for MemoryLogWriter {
    fn write(&self, entry: LogEntry) {
        self.entries.lock().push(entry);
    }
}

pub struct ConsoleLogWriter;

impl ILogWriter for ConsoleLogWriter {
    fn write(&self, entry: LogEntry) {
        use std::io::IsTerminal;
        let formatted = format_entry(
            &entry,
            FormatOptions {
                ansi: std::io::stderr().is_terminal(),
                omit_context_keys: Vec::new(),
            },
        );
        match entry.level {
            LogEntryLevel::Error | LogEntryLevel::Warn => eprintln!("{}", formatted.text),
            _ => println!("{}", formatted.text),
        }
    }
}
