use crate::turn_loop::types::ToolResultSpill;

pub use crate::tool::tool_contract::{
    DEFAULT_TOOL_RESULT_MAX_CHARS, DEFAULT_TOOL_RESULT_MAX_RETAINED_CHARS,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutputAccumulatorResult {
    pub content: String,
    pub is_error: bool,
    pub brief: Option<String>,
    pub spill: Option<ToolResultSpill>,
}

#[derive(Debug, Default)]
pub struct ToolOutputAccumulator {
    buffer: Vec<String>,
    retained_chars: usize,
    total_chars: usize,
    saturated: bool,
}

impl ToolOutputAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn n_chars(&self) -> usize {
        self.retained_chars
    }

    pub fn total_chars(&self) -> usize {
        self.total_chars
    }

    pub fn write(&mut self, text: &str) {
        self.total_chars += text.encode_utf16().count();
        if self.saturated {
            return;
        }
        let remaining = DEFAULT_TOOL_RESULT_MAX_RETAINED_CHARS.saturating_sub(self.retained_chars);
        let kept = slice_utf16(text, remaining);
        if kept.len() != text.len() {
            self.saturated = true;
        }
        self.retained_chars += kept.encode_utf16().count();
        if !kept.is_empty() {
            self.buffer.push(kept.to_string());
        }
    }

    pub fn ok(&self, message: &str, brief: Option<&str>) -> ToolOutputAccumulatorResult {
        let mut final_message = message.to_string();
        if !final_message.is_empty() && !final_message.ends_with('.') {
            final_message.push('.');
        }
        let output = self.buffer.join("");
        ToolOutputAccumulatorResult {
            content: if output.is_empty() {
                final_message.clone()
            } else {
                output
            },
            is_error: false,
            brief: brief.map(str::to_string),
            spill: self.completion_spill(&final_message),
        }
    }

    pub fn error(&self, message: &str, brief: Option<&str>) -> ToolOutputAccumulatorResult {
        let output = self.buffer.join("");
        ToolOutputAccumulatorResult {
            content: if message.is_empty() {
                output
            } else if output.is_empty() {
                message.to_string()
            } else if output.ends_with('\n') {
                format!("{output}{message}")
            } else {
                format!("{output}\n{message}")
            },
            is_error: true,
            brief: brief.map(str::to_string),
            spill: self.retention_spill(message),
        }
    }

    fn retention_spill(&self, suffix: &str) -> Option<ToolResultSpill> {
        if self.total_chars <= self.retained_chars {
            return None;
        }
        Some(ToolResultSpill {
            output_path: None,
            total_chars: Some(self.total_chars),
            suffix: (!suffix.is_empty()).then(|| suffix.to_string()),
        })
    }

    fn completion_spill(&self, suffix: &str) -> Option<ToolResultSpill> {
        if let Some(mut spill) = self.retention_spill("") {
            if !suffix.is_empty() {
                spill.suffix = Some(suffix.to_string());
            }
            return Some(spill);
        }
        if suffix.is_empty() || self.total_chars <= DEFAULT_TOOL_RESULT_MAX_CHARS {
            return None;
        }
        Some(ToolResultSpill {
            output_path: None,
            total_chars: None,
            suffix: Some(suffix.to_string()),
        })
    }
}

fn slice_utf16(text: &str, max_units: usize) -> &str {
    if text.encode_utf16().count() <= max_units {
        return text;
    }
    let mut units = 0;
    let mut end = 0;
    for (index, ch) in text.char_indices() {
        let next = units + ch.len_utf16();
        if next > max_units {
            break;
        }
        units = next;
        end = index + ch.len_utf8();
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_accumulate_and_ok_adds_the_period() {
        let mut accumulator = ToolOutputAccumulator::new();
        accumulator.write("one\n");
        accumulator.write("two");
        let result = accumulator.ok("done", None);
        assert_eq!(result.content, "one\ntwo");
        assert!(!result.is_error);
        assert_eq!(result.spill, None);
    }

    #[test]
    fn an_empty_buffer_uses_and_punctuates_the_message() {
        let accumulator = ToolOutputAccumulator::new();
        let result = accumulator.ok("done", None);
        assert_eq!(result.content, "done.");
        let result = accumulator.ok("done.", None);
        assert_eq!(result.content, "done.");
    }

    #[test]
    fn error_joins_the_buffer_without_duplicating_newlines() {
        let mut accumulator = ToolOutputAccumulator::new();
        accumulator.write("partial\n");
        assert_eq!(accumulator.error("boom", None).content, "partial\nboom");
        let mut accumulator = ToolOutputAccumulator::new();
        accumulator.write("partial");
        assert_eq!(accumulator.error("boom", None).content, "partial\nboom");
        assert_eq!(accumulator.error("", None).content, "partial");
    }

    #[test]
    fn retention_stops_at_the_cap_and_reports_total_chars() {
        let mut accumulator = ToolOutputAccumulator::new();
        accumulator.write(&"x".repeat(DEFAULT_TOOL_RESULT_MAX_RETAINED_CHARS + 10));
        assert_eq!(
            accumulator.n_chars(),
            DEFAULT_TOOL_RESULT_MAX_RETAINED_CHARS
        );
        assert_eq!(
            accumulator.total_chars(),
            DEFAULT_TOOL_RESULT_MAX_RETAINED_CHARS + 10
        );
        let result = accumulator.error("boom", None);
        let spill = result.spill.expect("retention produces a spill");
        assert_eq!(
            spill.total_chars,
            Some(DEFAULT_TOOL_RESULT_MAX_RETAINED_CHARS + 10)
        );
        assert_eq!(spill.suffix.as_deref(), Some("boom"));
    }

    #[test]
    fn a_long_suffix_without_loss_completes_a_spill() {
        let mut accumulator = ToolOutputAccumulator::new();
        accumulator.write(&"x".repeat(DEFAULT_TOOL_RESULT_MAX_CHARS + 1));
        let result = accumulator.ok("done", None);
        let spill = result.spill.expect("a long completed output spills");
        assert_eq!(spill.total_chars, None);
        assert_eq!(spill.suffix.as_deref(), Some("done."));
    }

    #[test]
    fn a_multibyte_slice_never_splits_a_character() {
        let mut accumulator = ToolOutputAccumulator::new();
        accumulator.write("你好");
        assert_eq!(accumulator.n_chars(), 2);
        assert_eq!(accumulator.ok("", None).content, "你好");
    }
}
