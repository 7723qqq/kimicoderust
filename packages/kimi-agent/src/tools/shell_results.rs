//! v2 `agent/tools/os/shell/shellToolResults.ts`: the contract both shell tools
//! share — request validation, the timeout label, the foreground description and
//! the next-step lines a background result ends with.
//!
//! v2 puts these in one class because `Bash` and `PowerShell` must answer the
//! same way; the port keeps the pure parts here so both tools can call them.
//!
//! **Not ported:** `backgroundStartedResult`'s `pid` and `output_path` fields.
//! This engine's task records carry neither — `TaskRunner` hands back an id and
//! streams output into the task's own store — so emitting those keys would mean
//! inventing values. `status` and `automatic_notification` are emitted because
//! both are known.

/// v2 `formatTimeoutLabel`: a whole number of seconds keeps the `s` spelling,
/// anything else is milliseconds.
pub fn format_timeout_label(timeout_ms: u64) -> String {
    if timeout_ms.is_multiple_of(1000) {
        format!("{}s", timeout_ms / 1000)
    } else {
        format!("{timeout_ms}ms")
    }
}

/// v2 `foregroundDescription`: an explicit description wins, otherwise the
/// caller's label plus a 60-character preview of the command.
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

/// Why a shell request cannot start, or `None` when it can.
///
/// v2 `validateRunRequest` checks four things in order: the turn was already
/// cancelled, the command is empty, background execution is unavailable to this
/// agent, and a background call carries no `description`. The cancellation arm
/// stays with the caller here (it owns the abort flag), and the
/// background-unavailable arm stays with the caller too, because this engine
/// answers that case by handing the call to the host rather than failing it.
pub fn validate_run_request(
    command: &str,
    run_in_background: bool,
    description: Option<&str>,
) -> Option<String> {
    if command.is_empty() {
        return Some("Command cannot be empty.".to_string());
    }
    if run_in_background && description.map(str::trim).is_none_or(str::is_empty) {
        return Some("description is required when run_in_background is true.".to_string());
    }
    None
}

/// Why a foreground shell call ended up in the background, in v2's vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundStart {
    /// The call asked for background execution.
    Started,
    /// The foreground call hit its timeout and was re-homed.
    DetachedOnTimeout,
    /// The user moved it to the background.
    DetachedByUser,
}

/// v2 `nextStepLines`: what the model is told to do after a background result.
///
/// The two lines are load-bearing: they are the only place a background result
/// says "do not wait on this", and the automatic notification is what makes that
/// promise keepable.
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
            validate_run_request("", false, None).as_deref(),
            Some("Command cannot be empty.")
        );
    }

    #[test]
    fn a_background_call_without_a_description_is_refused() {
        for missing in [None, Some(""), Some("   ")] {
            assert_eq!(
                validate_run_request("npm test", true, missing).as_deref(),
                Some("description is required when run_in_background is true.")
            );
        }
        assert!(validate_run_request("npm test", true, Some("tests")).is_none());
    }

    #[test]
    fn a_foreground_call_needs_no_description() {
        assert!(validate_run_request("npm test", false, None).is_none());
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
