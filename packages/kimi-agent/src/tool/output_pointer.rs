use super::tool_contract::DEFAULT_TOOL_RESULT_MAX_CHARS;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputUnit {
    Chars,
    Bytes,
}

pub fn output_unit_word(unit: OutputUnit) -> &'static str {
    match unit {
        OutputUnit::Chars => "characters",
        OutputUnit::Bytes => "bytes",
    }
}

pub fn format_output_size(unit: OutputUnit, value: usize) -> String {
    format!("output_size_{}: {value}", unit_name(unit))
}

fn unit_name(unit: OutputUnit) -> &'static str {
    match unit {
        OutputUnit::Chars => "chars",
        OutputUnit::Bytes => "bytes",
    }
}

pub fn output_size_line(unit: OutputUnit, total_size: usize, preserved: Option<usize>) -> String {
    let size = format_output_size(unit, total_size);
    match preserved {
        Some(preserved) if preserved < total_size => format!(
            "{size} (only the first {preserved} {} were preserved)",
            output_unit_word(unit)
        ),
        _ => size,
    }
}

pub const READ_OUTPUT_HINT: &str =
    "next_step: Use Read with output_path to page through the saved output, or Grep to search it.";

pub fn output_next_step(step: &str, task_id: Option<&str>) -> String {
    if step == "task-output" {
        return match task_id {
            Some(task_id) => format!(
                "next_step: Use TaskOutput(task_id=\"{task_id}\") to query the task output."
            ),
            None => {
                "next_step: Use TaskOutput with the task_id to query the task output.".to_string()
            }
        };
    }
    READ_OUTPUT_HINT.to_string()
}

pub struct AppendedPointerInput<'a> {
    pub path: &'a str,
    pub total_size: usize,
    pub unit: OutputUnit,
    pub preserved: Option<usize>,
    pub has_media: bool,
}

pub fn render_appended_pointer(input: &AppendedPointerInput<'_>) -> String {
    let word = output_unit_word(input.unit);
    let partial = input
        .preserved
        .is_some_and(|preserved| preserved < input.total_size);
    let headline = if partial {
        format!(
            "[Per-line truncation occurred; only the first {} {word} (of {}) were saved to a file.",
            input.preserved.unwrap_or_default(),
            input.total_size
        )
    } else if input.has_media {
        "[Per-line truncation occurred; the complete text output was saved to a file (media parts stay attached to this result).".to_string()
    } else {
        "[Per-line truncation occurred; the complete output was saved to a file.".to_string()
    };
    format!(
        "{headline}\noutput_path: {}\n{READ_OUTPUT_HINT}]",
        input.path
    )
}

pub struct ReplacedPointerInput<'a> {
    pub path: &'a str,
    pub total_size: usize,
    pub preserved: usize,
    pub max_size: usize,
    pub tool_name: &'a str,
    pub tool_call_id: &'a str,
    pub unit: OutputUnit,
    pub byte_size: Option<usize>,
    pub has_media: bool,
}

pub fn render_replaced_pointer(input: &ReplacedPointerInput<'_>) -> String {
    let word = output_unit_word(input.unit);
    let partial = input.preserved < input.total_size;
    let headline = if partial {
        format!(
            "Tool output exceeded {} {word}; the first {} {word} (of {}) were saved to a file.",
            input.max_size, input.preserved, input.total_size
        )
    } else if input.has_media {
        format!(
            "Tool output exceeded {} {word}; the full text output was saved to a file (media parts stay attached to this result).",
            input.max_size
        )
    } else {
        format!(
            "Tool output exceeded {} {word}; the full output was saved to a file.",
            input.max_size
        )
    };
    let mut lines = vec![
        headline,
        format!("tool_name: {}", input.tool_name),
        format!("tool_call_id: {}", input.tool_call_id),
        output_size_line(input.unit, input.total_size, Some(input.preserved)),
    ];
    if let Some(byte_size) = input.byte_size {
        lines.push(format_output_size(OutputUnit::Bytes, byte_size));
    }
    lines.push(format!("output_path: {}", input.path));
    lines.push(READ_OUTPUT_HINT.to_string());
    lines.join("\n")
}

pub struct UnpersistedPointerInput<'a> {
    pub total_size: usize,
    pub unit: OutputUnit,
    pub max_size: usize,
    pub tool_name: &'a str,
    pub tool_call_id: &'a str,
}

pub fn render_unpersisted_pointer(input: &UnpersistedPointerInput<'_>) -> String {
    [
        format!(
            "Tool output exceeded {} {} and could not be saved to a file; only this preview is available.",
            input.max_size,
            output_unit_word(input.unit)
        ),
        format!("tool_name: {}", input.tool_name),
        format!("tool_call_id: {}", input.tool_call_id),
        format_output_size(input.unit, input.total_size),
    ]
    .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_lines_distinguish_partial_and_complete() {
        assert_eq!(
            output_size_line(OutputUnit::Chars, 100, Some(10)),
            "output_size_chars: 100 (only the first 10 characters were preserved)"
        );
        assert_eq!(
            output_size_line(OutputUnit::Chars, 100, Some(100)),
            "output_size_chars: 100"
        );
        assert_eq!(
            format_output_size(OutputUnit::Bytes, 7),
            "output_size_bytes: 7"
        );
    }

    #[test]
    fn appended_pointer_carries_the_path_and_hint() {
        let pointer = render_appended_pointer(&AppendedPointerInput {
            path: "/tmp/out.txt",
            total_size: 20,
            unit: OutputUnit::Chars,
            preserved: Some(10),
            has_media: false,
        });
        assert!(
            pointer
                .starts_with("[Per-line truncation occurred; only the first 10 characters (of 20)")
        );
        assert!(pointer.contains("output_path: /tmp/out.txt"));
        assert!(pointer.ends_with(&format!("{READ_OUTPUT_HINT}]")));
    }

    #[test]
    fn replaced_pointer_orders_the_fields_like_v2() {
        let pointer = render_replaced_pointer(&ReplacedPointerInput {
            path: "/tmp/out.txt",
            total_size: 60_000,
            preserved: 50_000,
            max_size: DEFAULT_TOOL_RESULT_MAX_CHARS,
            tool_name: "Bash",
            tool_call_id: "call-1",
            unit: OutputUnit::Chars,
            byte_size: None,
            has_media: false,
        });
        let lines: Vec<&str> = pointer.lines().collect();
        assert!(lines[0].starts_with("Tool output exceeded 50000 characters;"));
        assert_eq!(lines[1], "tool_name: Bash");
        assert_eq!(lines[2], "tool_call_id: call-1");
        assert!(lines[3].starts_with("output_size_chars: 60000"));
        assert_eq!(lines[4], "output_path: /tmp/out.txt");
        assert_eq!(lines[5], READ_OUTPUT_HINT);
    }

    #[test]
    fn unpersisted_pointer_reports_the_total_without_a_path() {
        let pointer = render_unpersisted_pointer(&UnpersistedPointerInput {
            total_size: 60_000,
            unit: OutputUnit::Chars,
            max_size: DEFAULT_TOOL_RESULT_MAX_CHARS,
            tool_name: "Bash",
            tool_call_id: "call-1",
        });
        assert!(pointer.contains("could not be saved to a file"));
        assert!(pointer.contains("output_size_chars: 60000"));
        assert!(!pointer.contains("output_path:"));
    }
}
