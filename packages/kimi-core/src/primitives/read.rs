
use std::fmt;
use std::path::Path;

use serde::de::DeserializeOwned;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnreadableCause {
    pub subject: String,
    pub detail: String,
}

impl UnreadableCause {
    #[must_use]
    pub fn new(subject: impl Into<String>, detail: impl fmt::Display) -> Self {
        Self {
            subject: subject.into(),
            detail: detail.to_string(),
        }
    }
}

impl fmt::Display for UnreadableCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.subject, self.detail)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadOutcome<T> {
    Found(T),
    Absent,
    Unreadable(UnreadableCause),
}

impl<T> Default for ReadOutcome<T> {
    fn default() -> Self {
        Self::Absent
    }
}

impl<T> ReadOutcome<T> {
    #[must_use]
    pub fn found(value: T) -> Self {
        Self::Found(value)
    }

    #[must_use]
    pub fn from_result(result: Result<T, UnreadableCause>) -> Self {
        match result {
            Ok(value) => Self::Found(value),
            Err(cause) => Self::Unreadable(cause),
        }
    }

    #[must_use]
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> ReadOutcome<U> {
        match self {
            Self::Found(value) => ReadOutcome::Found(f(value)),
            Self::Absent => ReadOutcome::Absent,
            Self::Unreadable(cause) => ReadOutcome::Unreadable(cause),
        }
    }

    #[must_use]
    pub fn as_ref(&self) -> ReadOutcome<&T> {
        match self {
            Self::Found(value) => ReadOutcome::Found(value),
            Self::Absent => ReadOutcome::Absent,
            Self::Unreadable(cause) => ReadOutcome::Unreadable(cause.clone()),
        }
    }

    pub fn into_result(self) -> Result<Option<T>, UnreadableCause> {
        match self {
            Self::Found(value) => Ok(Some(value)),
            Self::Absent => Ok(None),
            Self::Unreadable(cause) => Err(cause),
        }
    }

    pub fn fold_absent(self, why: &str) -> Option<T> {
        match self {
            Self::Found(value) => Some(value),
            Self::Absent => None,
            Self::Unreadable(cause) => {
                tracing::warn!(
                    subject = %cause.subject,
                    detail = %cause.detail,
                    reason = why,
                    "reading a source failed; treating it as absent"
                );
                None
            }
        }
    }

    #[must_use]
    pub fn is_found(&self) -> bool {
        matches!(self, Self::Found(_))
    }

    #[must_use]
    pub fn is_absent(&self) -> bool {
        matches!(self, Self::Absent)
    }

    #[must_use]
    pub fn is_unreadable(&self) -> bool {
        matches!(self, Self::Unreadable(_))
    }

    #[must_use]
    pub fn cause(&self) -> Option<&UnreadableCause> {
        match self {
            Self::Unreadable(cause) => Some(cause),
            _ => None,
        }
    }
}

#[must_use]
pub fn read_text(path: &Path) -> ReadOutcome<String> {
    match std::fs::read_to_string(path) {
        Ok(content) => ReadOutcome::Found(content),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => ReadOutcome::Absent,
        Err(error) => {
            ReadOutcome::Unreadable(UnreadableCause::new(path.display().to_string(), error))
        }
    }
}

#[must_use]
pub fn read_json<T: DeserializeOwned>(path: &Path) -> ReadOutcome<T> {
    let subject = path.display().to_string();
    let raw = match read_text(path) {
        ReadOutcome::Found(raw) => raw,
        ReadOutcome::Absent => return ReadOutcome::Absent,
        ReadOutcome::Unreadable(cause) => return ReadOutcome::Unreadable(cause),
    };
    match serde_json::from_str(&raw) {
        Ok(value) => ReadOutcome::Found(value),
        Err(error) => ReadOutcome::Unreadable(UnreadableCause::new(subject, error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Config {
        name: String,
    }

    fn tempdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kimi-primitives-read-{}-{}",
            std::process::id(),
            fastrand::u64(..)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_missing_file_is_absent_not_unreadable() {
        let dir = tempdir();
        assert_eq!(read_text(&dir.join("nope.json")), ReadOutcome::Absent);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_file_that_exists_but_does_not_parse_is_unreadable() {
        let dir = tempdir();
        let path = dir.join("config.json");
        std::fs::write(&path, "{ not json").unwrap();
        let outcome: ReadOutcome<Config> = read_json(&path);
        assert!(outcome.is_unreadable(), "{outcome:?}");
        assert!(outcome.cause().unwrap().subject.ends_with("config.json"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_path_that_cannot_be_read_as_a_file_is_unreadable() {
        let dir = tempdir();
        let outcome = read_text(&dir);
        assert!(outcome.is_unreadable(), "{outcome:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_valid_file_is_found() {
        let dir = tempdir();
        let path = dir.join("config.json");
        std::fs::write(&path, r#"{"name":"x"}"#).unwrap();
        assert_eq!(
            read_json::<Config>(&path),
            ReadOutcome::Found(Config { name: "x".into() })
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn into_result_keeps_absent_and_unreadable_distinct() {
        assert_eq!(ReadOutcome::<u8>::Absent.into_result(), Ok(None));
        let cause = UnreadableCause::new("subject", "detail");
        assert_eq!(
            ReadOutcome::<u8>::Unreadable(cause.clone()).into_result(),
            Err(cause)
        );
        assert_eq!(ReadOutcome::found(7u8).into_result(), Ok(Some(7)));
    }

    #[test]
    fn folding_requires_a_reason_and_yields_none() {
        let cause = UnreadableCause::new("subject", "detail");
        assert_eq!(
            ReadOutcome::<u8>::Unreadable(cause).fold_absent("test"),
            None
        );
        assert_eq!(ReadOutcome::found(3u8).fold_absent("test"), Some(3));
    }
}
