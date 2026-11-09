//! Per-session diagnostic log.
//!
//! v2 scopes a `FileLogWriter` to every session
//! (`agent-core-v2/src/session/sessionLog/sessionLogService.ts`): the sink is
//! `<sessionDir>/logs/kimi-code.log`, its level comes from the logging config,
//! and the file is size-capped and rotated (`_base/log/fileLog.ts`). This
//! module ports that sink, including the config resolution from
//! `_base/log/logConfig.ts` so an operator picks the level and the rotation the
//! same way on both runtimes.

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// v2 `DEFAULT_SESSION_MAX_BYTES`.
pub const DEFAULT_SESSION_MAX_BYTES: u64 = 5 * 1024 * 1024;
/// v2 `DEFAULT_SESSION_FILES`.
pub const DEFAULT_SESSION_FILES: usize = 3;
/// v2 `DEFAULT_LOG_LEVEL`.
pub const DEFAULT_LOG_LEVEL: LogLevel = LogLevel::Info;
/// v2 `MSG_MAX_CHARS`: the message is clipped before it reaches the line.
const MESSAGE_MAX_CHARS: usize = 200;

/// v2 `LogLevel`, ordered so `allowed` is a comparison.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Off,
    Error,
    Warn,
    Info,
    Debug,
}

impl LogLevel {
    /// v2 `parseLevel`: case-insensitive, trimmed; unknown values fall back to
    /// the default rather than failing the engine.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "error" => Some(Self::Error),
            "warn" => Some(Self::Warn),
            "info" => Some(Self::Info),
            "debug" => Some(Self::Debug),
            _ => None,
        }
    }

    /// The fixed-width label v2's formatter prints (`WARN ` / `INFO ` keep the
    /// message column aligned).
    fn label(self) -> &'static str {
        match self {
            Self::Error => "ERROR",
            Self::Warn => "WARN ",
            Self::Info => "INFO ",
            Self::Debug => "DEBUG",
            Self::Off => "OFF  ",
        }
    }
}

/// v2 `LoggingConfig`'s session half.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionLogConfig {
    pub level: LogLevel,
    pub max_bytes: u64,
    pub files: usize,
}

impl Default for SessionLogConfig {
    fn default() -> Self {
        Self {
            level: DEFAULT_LOG_LEVEL,
            max_bytes: DEFAULT_SESSION_MAX_BYTES,
            files: DEFAULT_SESSION_FILES,
        }
    }
}

impl SessionLogConfig {
    /// v2 `resolveLoggingConfig`: `KIMI_LOG_LEVEL`,
    /// `KIMI_LOG_SESSION_MAX_BYTES`, `KIMI_LOG_SESSION_FILES`, each falling back
    /// to its default when unset or unparsable.
    pub fn from_env() -> Self {
        let mut config = Self::default();
        if let Some(level) = std::env::var("KIMI_LOG_LEVEL")
            .ok()
            .and_then(|value| LogLevel::parse(&value))
        {
            config.level = level;
        }
        if let Some(bytes) = positive_int_env("KIMI_LOG_SESSION_MAX_BYTES") {
            config.max_bytes = bytes;
        }
        if let Some(files) = positive_int_env("KIMI_LOG_SESSION_FILES") {
            config.files = files as usize;
        }
        config
    }
}

fn positive_int_env(name: &str) -> Option<u64> {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
}

/// v2 `resolveSessionLogPath`.
pub fn resolve_session_log_path(session_dir: &Path) -> PathBuf {
    session_dir.join("logs").join("kimi-code.log")
}

/// One session's log sink: level-filtered, size-capped, rotating.
///
/// `log` is synchronous and serialized by a mutex: a turn writes a handful of
/// lifecycle lines, so an async queue (v2's `RotatingFileWriter`) would buy
/// nothing here.
pub struct SessionLog {
    path: PathBuf,
    config: SessionLogConfig,
    write_lock: Mutex<()>,
}

impl SessionLog {
    pub fn open(session_dir: &Path, config: SessionLogConfig) -> Self {
        Self {
            path: resolve_session_log_path(session_dir),
            config,
            write_lock: Mutex::new(()),
        }
    }

    /// The active file. Rotated siblings are `<path>.1` … `<path>.N`.
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn config(&self) -> SessionLogConfig {
        self.config
    }

    pub fn enabled(&self, level: LogLevel) -> bool {
        self.config.level != LogLevel::Off && level <= self.config.level
    }

    /// Write one line, rotating first when it would push the active file past
    /// its cap (v2 `appendLines`' exact-overflow branch).
    pub fn log(&self, level: LogLevel, message: &str, context: &[(&str, &str)]) {
        if !self.enabled(level) {
            return;
        }
        let line = format_line(level, message, context);
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let current = std::fs::metadata(&self.path)
            .map(|meta| meta.len())
            .unwrap_or(0);
        if current > 0 && current + line.len() as u64 > self.config.max_bytes {
            self.rotate();
        }
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = file.write_all(line.as_bytes());
        }
    }

    fn rotate(&self) {
        let files = self.config.files.max(1);
        let with_suffix = |index: usize| -> PathBuf {
            let mut name = OsString::from(self.path.as_os_str());
            name.push(format!(".{index}"));
            PathBuf::from(name)
        };
        let _ = std::fs::remove_file(with_suffix(files));
        for index in (1..files).rev() {
            let from = with_suffix(index);
            if from.exists() {
                let _ = std::fs::rename(&from, with_suffix(index + 1));
            }
        }
        if self.path.exists() {
            let _ = std::fs::rename(&self.path, with_suffix(1));
        }
    }
}

/// v2 `formatEntry`: `{ISO time} {LABEL} {message}  k=v …`, message clipped to
/// `MSG_MAX_CHARS`, values quoted when they would otherwise break the line.
fn format_line(level: LogLevel, message: &str, context: &[(&str, &str)]) -> String {
    let time = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let message = truncate_chars(message, MESSAGE_MAX_CHARS);
    let mut line = format!("{time} {} {message}", level.label());
    if !context.is_empty() {
        line.push_str("  ");
        let mut first = true;
        for (key, value) in context {
            if !first {
                line.push(' ');
            }
            first = false;
            line.push_str(key);
            line.push('=');
            line.push_str(&quote_if_needed(value));
        }
    }
    line.push('\n');
    line
}

fn truncate_chars(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    let mut clipped: String = value.chars().take(max.saturating_sub(1)).collect();
    clipped.push('…');
    clipped
}

fn quote_if_needed(value: &str) -> String {
    if value.is_empty()
        || value
            .chars()
            .any(|ch| ch.is_whitespace() || matches!(ch, '=' | '"' | '\\'))
    {
        let escaped = value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n");
        format!("\"{escaped}\"")
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("kimi-session-log-{name}-{}", fastrand::u64(..)));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn level_parsing_matches_v2() {
        assert_eq!(LogLevel::parse(" OFF "), Some(LogLevel::Off));
        assert_eq!(LogLevel::parse("Debug"), Some(LogLevel::Debug));
        assert_eq!(LogLevel::parse("verbose"), None);
    }

    #[test]
    fn writes_the_session_log_under_logs() {
        let dir = scratch("path");
        let log = SessionLog::open(&dir, SessionLogConfig::default());
        assert_eq!(log.path(), dir.join("logs").join("kimi-code.log"));
        log.log(LogLevel::Info, "turn started", &[("turn", "1")]);
        let body = std::fs::read_to_string(log.path()).unwrap();
        assert!(body.contains(" INFO  turn started  turn=1"), "{body}");
        assert!(body.ends_with('\n'));
        assert!(
            body.split_whitespace().next().unwrap().contains('T'),
            "the line starts with an ISO timestamp: {body}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_level_filters_entries() {
        let dir = scratch("level");
        let log = SessionLog::open(
            &dir,
            SessionLogConfig {
                level: LogLevel::Warn,
                ..SessionLogConfig::default()
            },
        );
        log.log(LogLevel::Info, "not written", &[]);
        log.log(LogLevel::Error, "written", &[]);
        let body = std::fs::read_to_string(log.path()).unwrap();
        assert!(!body.contains("not written"));
        assert!(body.contains("ERROR written"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_off_level_writes_nothing() {
        let dir = scratch("off");
        let log = SessionLog::open(
            &dir,
            SessionLogConfig {
                level: LogLevel::Off,
                ..SessionLogConfig::default()
            },
        );
        log.log(LogLevel::Error, "silent", &[]);
        assert!(!log.path().exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rotation_keeps_the_configured_number_of_files() {
        let dir = scratch("rotation");
        let log = SessionLog::open(
            &dir,
            SessionLogConfig {
                level: LogLevel::Debug,
                max_bytes: 120,
                files: 2,
            },
        );
        for index in 0..8 {
            log.log(LogLevel::Info, &format!("entry {index}"), &[]);
        }
        assert!(log.path().exists(), "the active file is always present");
        let first = PathBuf::from(format!("{}.1", log.path().display()));
        let second = PathBuf::from(format!("{}.2", log.path().display()));
        let third = PathBuf::from(format!("{}.3", log.path().display()));
        assert!(first.exists(), "an overflow rotates the active file");
        assert!(!third.exists(), "only `files` files are kept");
        for path in [log.path().to_path_buf(), first, second] {
            if let Ok(body) = std::fs::read_to_string(&path) {
                assert!(body.len() <= 120, "{path:?} stays capped: {body:?}");
            }
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
