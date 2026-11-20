use std::time::Duration;

use serde_json::Value;

use crate::agent::tools::os::shell::shell_tool_results::normalize_timeout_ms;

pub(super) fn parse_timeout(args: &Value) -> Result<Option<u64>, &'static str> {
    match args.get("timeout") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_f64()
            .filter(|seconds| {
                seconds.fract() == 0.0 && *seconds > 0.0 && *seconds <= 9_007_199_254_740_991.0
            })
            .map(|seconds| Some(seconds as u64))
            .ok_or("timeout must be a positive integer"),
    }
}

pub(super) fn background_timeout(timeout: Option<u64>, disable_timeout: bool) -> Option<Duration> {
    if disable_timeout {
        return None;
    }
    Some(Duration::from_millis(normalize_timeout_ms(timeout, true)))
}

pub fn format_timeout_label(timeout_ms: u64) -> String {
    if timeout_ms.is_multiple_of(1000) {
        format!("{}s", timeout_ms / 1000)
    } else {
        format!("{timeout_ms}ms")
    }
}

pub fn foreground_description(label: &str, command: &str, description: Option<&str>) -> String {
    if let Some(explicit) = description.map(str::trim).filter(|text| !text.is_empty()) {
        return explicit.to_string();
    }
    let preview = if command.chars().count() > 60 {
        let head: String = command.chars().take(60).collect();
        format!("{head}…")
    } else {
        command.to_string()
    };
    format!("{label}: {preview}")
}

pub const BACKGROUND_UNAVAILABLE: &str =
    "Background execution is not available for this agent because TaskOutput and TaskStop are not enabled.";

pub fn validate_run_request(
    command: &str,
    run_in_background: bool,
    description: Option<&str>,
    allow_background: bool,
) -> Option<String> {
    if command.is_empty() {
        return Some("Command cannot be empty.".to_string());
    }
    if !run_in_background {
        return None;
    }
    if !allow_background {
        return Some(BACKGROUND_UNAVAILABLE.to_string());
    }
    if description.map(str::trim).is_none_or(str::is_empty) {
        return Some("description is required when run_in_background is true.".to_string());
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundStart {
    Started,

    DetachedOnTimeout,

    DetachedByUser,
}

pub fn next_step_lines(allow_background: bool, start: BackgroundStart) -> String {
    match start {
        BackgroundStart::DetachedOnTimeout | BackgroundStart::DetachedByUser => {
            let avoid = if allow_background {
                "do NOT wait, poll, or call TaskOutput on it"
            } else {
                "do NOT wait or poll"
            };
            let moved = if start == BackgroundStart::DetachedByUser {
                "The user moved this task to the background."
            } else {
                "The task now runs in the background."
            };
            format!(
                "next_step: {moved} You will be automatically notified when it completes — {avoid}; continue with your current work."
            )
        }
        BackgroundStart::Started if !allow_background => {
            "next_step: You will be automatically notified when it completes.".to_string()
        }
        BackgroundStart::Started => concat!(
            "next_step: The completion arrives automatically in a later turn — do NOT wait, poll, ",
            "or call TaskOutput on it; continue with your current work.\n",
            "next_step: Use TaskStop only if the task must be cancelled."
        )
        .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_parser_accepts_json_schema_integers_without_applying_runtime_caps() {
        for seconds in [1, 30, 301, 86_401, 9_007_199_254_740_991u64] {
            assert_eq!(parse_timeout(&serde_json::json!({ "timeout": seconds })), Ok(Some(seconds)));
        }
        assert_eq!(parse_timeout(&serde_json::json!({ "timeout": 30.0 })), Ok(Some(30)));
        assert_eq!(parse_timeout(&serde_json::json!({})), Ok(None));
        assert_eq!(parse_timeout(&serde_json::json!({ "timeout": null })), Ok(None));
        for timeout in [
            serde_json::json!(0),
            serde_json::json!(-1),
            serde_json::json!(1.5),
            serde_json::json!("30"),
            serde_json::json!(true),
            serde_json::json!(9_007_199_254_740_992u64),
        ] {
            assert!(parse_timeout(&serde_json::json!({ "timeout": timeout })).is_err());
        }
    }

    #[test]
    fn a_whole_second_is_labelled_in_seconds() {
        assert_eq!(format_timeout_label(60_000), "60s");
        assert_eq!(format_timeout_label(1_000), "1s");
    }

    #[test]
    fn a_fractional_second_is_labelled_in_milliseconds() {
        assert_eq!(format_timeout_label(1_500), "1500ms");
    }

    #[test]
    fn an_explicit_description_wins_over_the_command_preview() {
        assert_eq!(
            foreground_description("Bash", "npm test", Some("  running the suite  ")),
            "running the suite"
        );
    }

    #[test]
    fn a_long_command_is_previewed_at_sixty_characters() {
        let command = "x".repeat(70);
        let description = foreground_description("Bash", &command, None);
        assert!(description.starts_with("Bash: "));
        assert!(description.ends_with('…'));
        assert_eq!(description.chars().count(), "Bash: ".len() + 61);
    }

    #[test]
    fn a_blank_description_is_treated_as_absent() {
        assert_eq!(
            foreground_description("PowerShell", "Get-Date", Some("   ")),
            "PowerShell: Get-Date"
        );
    }

    #[test]
    fn an_empty_command_is_refused() {
        assert_eq!(
            validate_run_request("", false, None, true).as_deref(),
            Some("Command cannot be empty.")
        );
    }

    #[test]
    fn a_background_call_without_a_description_is_refused() {
        for missing in [None, Some(""), Some("   ")] {
            assert_eq!(
                validate_run_request("npm test", true, missing, true).as_deref(),
                Some("description is required when run_in_background is true.")
            );
        }
        assert!(validate_run_request("npm test", true, Some("tests"), true).is_none());
    }

    #[test]
    fn background_admission_precedes_the_description_check() {
        assert_eq!(
            validate_run_request("npm test", true, None, false).as_deref(),
            Some(BACKGROUND_UNAVAILABLE)
        );
    }

    #[test]
    fn a_foreground_call_needs_no_description() {
        assert!(validate_run_request("npm test", false, None, false).is_none());
    }

    #[test]
    fn the_started_lines_name_the_notification_and_the_stop_tool() {
        let lines = next_step_lines(true, BackgroundStart::Started);
        assert!(lines.contains("The completion arrives automatically in a later turn"));
        assert!(lines.contains("do NOT wait, poll, or call TaskOutput on it"));
        assert!(lines.contains("Use TaskStop only if the task must be cancelled."));
    }

    #[test]
    fn without_task_tools_the_lines_do_not_name_task_output() {
        let lines = next_step_lines(false, BackgroundStart::Started);
        assert_eq!(
            lines,
            "next_step: You will be automatically notified when it completes."
        );
        assert!(!lines.contains("TaskOutput"));
    }

    #[test]
    fn a_detached_task_says_who_moved_it() {
        assert!(
            next_step_lines(true, BackgroundStart::DetachedOnTimeout)
                .contains("The task now runs in the background.")
        );
        assert!(
            next_step_lines(true, BackgroundStart::DetachedByUser)
                .contains("The user moved this task to the background.")
        );
    }
}
