use once_cell::sync::Lazy;
use regex::{Captures, Regex};
use serde_json::Value;

use super::context::TelemetryProperties;

const REDACTED_PATH: &str = "<REDACTED: user-file-path>";
const NODE_MODULES_MARKER: &str = "node_modules/";

static LABELED_PATTERNS: Lazy<Vec<(Regex, &'static str)>> = Lazy::new(|| {
    vec![
        (
            Regex::new(r#"(?-u:\b)[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}(?-u:\b)"#)
                .expect("email telemetry pattern is valid"),
            "<REDACTED: Email>",
        ),
        (
            Regex::new(r#"(?i)https?://[^\s"'<>]+"#).expect("URL telemetry pattern is valid"),
            "<REDACTED: URL>",
        ),
        (
            Regex::new(r#"(?-u:\b)eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{5,}(?-u:\b)"#)
                .expect("JWT telemetry pattern is valid"),
            "<REDACTED: JWT>",
        ),
        (
            Regex::new(r#"(?-u:\b)(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{20,}(?-u:\b)"#)
                .expect("GitHub token telemetry pattern is valid"),
            "<REDACTED: GitHub Token>",
        ),
        (
            Regex::new(r#"(?-u:\b)github_pat_[A-Za-z0-9_]{20,}(?-u:\b)"#)
                .expect("GitHub PAT telemetry pattern is valid"),
            "<REDACTED: GitHub Token>",
        ),
        (
            Regex::new(r#"(?-u:\b)xox[baprs]-[A-Za-z0-9-]{10,}(?-u:\b)"#)
                .expect("Slack token telemetry pattern is valid"),
            "<REDACTED: Slack Token>",
        ),
        (
            Regex::new(r#"(?-u:\b)(?:sk|pk|ak)-[A-Za-z0-9_-]{16,}(?-u:\b)"#)
                .expect("API key telemetry pattern is valid"),
            "<REDACTED: API Key>",
        ),
    ]
});

static POSIX_PATH: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?:/(?-u:[\w.~+-]+)){2,}/?"#).expect("POSIX path telemetry pattern is valid")
});

static WINDOWS_PATH: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?-u:\b)[A-Za-z]:\\(?:(?-u:[\w.~ -]+)\\?){2,}"#)
        .expect("Windows path telemetry pattern is valid")
});

pub fn clean_telemetry_string(value: &str) -> String {
    let mut out = value.to_string();
    for (pattern, label) in LABELED_PATTERNS.iter() {
        out = pattern.replace_all(&out, *label).into_owned();
    }
    out = WINDOWS_PATH.replace_all(&out, REDACTED_PATH).into_owned();
    out = POSIX_PATH
        .replace_all(&out, |captures: &Captures<'_>| {
            let matched = captures.get(0).map_or("", |value| value.as_str());
            match matched.find(NODE_MODULES_MARKER) {
                Some(index) => matched[index..].to_string(),
                None => REDACTED_PATH.to_string(),
            }
        })
        .into_owned();
    out
}

pub fn clean_telemetry_properties(properties: &TelemetryProperties) -> TelemetryProperties {
    properties
        .iter()
        .map(|(key, value)| {
            let cleaned = match value {
                Value::String(value) => Value::String(clean_telemetry_string(value)),
                value => value.clone(),
            };
            (key.clone(), cleaned)
        })
        .collect()
}
