const TRUNCATION_MARKER: &str = "[...truncated]";

pub struct WriteChunkResult {
    pub output: String,

    pub chars_written: usize,

    pub new_nchars: usize,

    pub truncated: bool,
}

pub fn write_chunk(
    text: &str,
    current_nchars: usize,
    max_chars: usize,
    max_line_length: Option<usize>,
    already_truncated: bool,
) -> WriteChunkResult {
    let mut nchars = current_nchars;
    let mut truncated = already_truncated;

    if nchars >= max_chars {
        if !text.is_empty() && !truncated {
            let marker_len = utf16_len(TRUNCATION_MARKER);
            nchars += marker_len;
            truncated = true;
            return WriteChunkResult {
                output: TRUNCATION_MARKER.to_string(),
                chars_written: 0,
                new_nchars: nchars,
                truncated,
            };
        }
        return WriteChunkResult {
            output: String::new(),
            chars_written: 0,
            new_nchars: nchars,
            truncated,
        };
    }

    let lines = split_lines(text);
    if lines.is_empty() {
        return WriteChunkResult {
            output: String::new(),
            chars_written: 0,
            new_nchars: nchars,
            truncated,
        };
    }

    let mut output = String::new();
    let mut chars_written = 0usize;

    for original_line in &lines {
        if nchars >= max_chars {
            if !truncated {
                output.push_str(TRUNCATION_MARKER);
                nchars += utf16_len(TRUNCATION_MARKER);
                truncated = true;
            }
            break;
        }

        let remaining = max_chars - nchars;
        let limit = match max_line_length {
            Some(mll) => remaining.min(mll),
            None => remaining,
        };

        let line_utf16_len = utf16_len(original_line);
        let line = if line_utf16_len > limit {
            let line_break = extract_trailing_newlines(original_line);
            let suffix = format!("{TRUNCATION_MARKER}{line_break}");
            let suffix_len = utf16_len(&suffix);
            let effective_max = limit.max(suffix_len);
            let keep_len = effective_max.saturating_sub(suffix_len);
            let kept = utf16_slice(original_line, keep_len);
            truncated = true;
            format!("{kept}{suffix}")
        } else {
            original_line.to_string()
        };

        let line_len = utf16_len(&line);
        output.push_str(&line);
        chars_written += line_len;
        nchars += line_len;
    }

    WriteChunkResult {
        output,
        chars_written,
        new_nchars: nchars,
        truncated,
    }
}

fn split_lines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let bytes = text.as_bytes();
    let mut start = 0usize;
    let mut i = 0usize;

    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                lines.push(&text[start..=i]);
                i += 1;
                start = i;
            }
            b'\r' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                    lines.push(&text[start..=i + 1]);
                    i += 2;
                    start = i;
                } else {
                    lines.push(&text[start..=i]);
                    i += 1;
                    start = i;
                }
            }
            _ => {
                i += 1;
            }
        }
    }

    if start < bytes.len() {
        lines.push(&text[start..]);
    }

    lines
}

fn extract_trailing_newlines(s: &str) -> &str {
    let bytes = s.as_bytes();
    let mut end = s.len();
    while end > 0 {
        let prev = bytes[end - 1];
        if prev == b'\n' || prev == b'\r' {
            end -= 1;
        } else {
            break;
        }
    }
    &s[end..]
}

fn utf16_len(s: &str) -> usize {
    s.chars()
        .map(|c| if (c as u32) > 0xFFFF { 2 } else { 1 })
        .sum()
}

fn utf16_slice(s: &str, max_units: usize) -> &str {
    if max_units == 0 {
        return "";
    }
    let mut count = 0usize;
    for (byte_idx, ch) in s.char_indices() {
        let units = if (ch as u32) > 0xFFFF { 2 } else { 1 };
        if count + units > max_units {
            return &s[..byte_idx];
        }
        count += units;
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_lines_lf() {
        assert_eq!(split_lines("hello\nworld\n"), vec!["hello\n", "world\n"]);
    }

    #[test]
    fn test_split_lines_crlf() {
        assert_eq!(
            split_lines("hello\r\nworld\r\n"),
            vec!["hello\r\n", "world\r\n"]
        );
    }

    #[test]
    fn test_split_lines_cr() {
        assert_eq!(split_lines("hello\rworld\r"), vec!["hello\r", "world\r"]);
    }

    #[test]
    fn test_split_lines_no_terminator() {
        assert_eq!(split_lines("hello"), vec!["hello"]);
    }

    #[test]
    fn test_split_lines_empty() {
        assert!(split_lines("").is_empty());
    }

    #[test]
    fn test_split_lines_just_newline() {
        assert_eq!(split_lines("\n"), vec!["\n"]);
    }

    #[test]
    fn test_split_lines_mixed_endings() {
        assert_eq!(
            split_lines("a\nb\r\nc\rd"),
            vec!["a\n", "b\r\n", "c\r", "d"]
        );
    }

    #[test]
    fn test_extract_trailing_lf() {
        assert_eq!(extract_trailing_newlines("hello\n"), "\n");
    }

    #[test]
    fn test_extract_trailing_crlf() {
        assert_eq!(extract_trailing_newlines("hello\r\n"), "\r\n");
    }

    #[test]
    fn test_extract_trailing_none() {
        assert_eq!(extract_trailing_newlines("hello"), "");
    }

    #[test]
    fn test_extract_trailing_multiple() {
        assert_eq!(extract_trailing_newlines("hello\n\n\r\n"), "\n\n\r\n");
    }

    #[test]
    fn test_utf16_len_ascii() {
        assert_eq!(utf16_len("hello"), 5);
    }

    #[test]
    fn test_utf16_len_cjk() {
        assert_eq!(utf16_len("你好"), 2);
    }

    #[test]
    fn test_utf16_len_emoji() {
        assert_eq!(utf16_len("👋"), 2);
    }

    #[test]
    fn test_utf16_len_mixed() {
        assert_eq!(utf16_len("a👋b"), 4);
    }

    #[test]
    fn test_utf16_slice_ascii() {
        assert_eq!(utf16_slice("hello", 3), "hel");
    }

    #[test]
    fn test_utf16_slice_full() {
        assert_eq!(utf16_slice("hello", 10), "hello");
    }

    #[test]
    fn test_utf16_slice_zero() {
        assert_eq!(utf16_slice("hello", 0), "");
    }

    #[test]
    fn test_utf16_slice_cjk() {
        assert_eq!(utf16_slice("你好世界", 2), "你好");
    }

    #[test]
    fn test_utf16_slice_emoji_no_split() {
        assert_eq!(utf16_slice("👋", 1), "");
        assert_eq!(utf16_slice("👋", 2), "👋");
    }

    #[test]
    fn test_write_chunk_passthrough() {
        let r = write_chunk("hello\n", 0, 1000, None, false);
        assert_eq!(r.output, "hello\n");
        assert_eq!(r.chars_written, 6);
        assert_eq!(r.new_nchars, 6);
        assert!(!r.truncated);
    }

    #[test]
    fn test_write_chunk_empty() {
        let r = write_chunk("", 0, 1000, None, false);
        assert_eq!(r.output, "");
        assert_eq!(r.chars_written, 0);
        assert_eq!(r.new_nchars, 0);
        assert!(!r.truncated);
    }

    #[test]
    fn test_write_chunk_already_over_budget() {
        let r = write_chunk("hello", 100, 50, None, false);
        assert_eq!(r.output, TRUNCATION_MARKER);
        assert!(r.truncated);
        assert_eq!(r.chars_written, 0);
        assert_eq!(r.new_nchars, 100 + TRUNCATION_MARKER.len());
    }

    #[test]
    fn test_write_chunk_already_over_budget_already_truncated() {
        let r = write_chunk("hello", 100, 50, None, true);
        assert_eq!(r.output, "");
        assert_eq!(r.chars_written, 0);
        assert!(r.truncated);
    }

    #[test]
    fn test_write_chunk_exceeds_budget_mid_chunk() {
        let r = write_chunk("line1\nline2\nline3\n", 0, 12, None, false);
        assert_eq!(r.output, "line1\nline2\n[...truncated]");
        assert_eq!(r.new_nchars, 12 + TRUNCATION_MARKER.len());
        assert!(r.truncated);
    }

    #[test]
    fn test_write_chunk_line_length_truncation() {
        let r = write_chunk("hello world\n", 0, 1000, Some(10), false);

        assert_eq!(r.output, "[...truncated]\n");
        assert!(r.truncated);
    }

    #[test]
    fn test_write_chunk_line_length_truncation_keeps_prefix() {
        let r = write_chunk("hello world this is long\n", 0, 1000, Some(20), false);

        assert_eq!(r.output, "hello[...truncated]\n");
        assert!(r.truncated);
    }

    #[test]
    fn test_write_chunk_multiple_lines_some_truncated() {
        let text = "short\nthis is a very long line that exceeds the limit\nshort2\n";
        let r = write_chunk(text, 0, 1000, Some(20), false);

        assert!(r.output.contains("short\n"));
        assert!(r.output.contains("this [...truncated]\n"));
        assert!(r.output.contains("short2\n"));
        assert!(r.truncated);
    }

    #[test]
    fn test_write_chunk_crlf_line() {
        let r = write_chunk("hello\r\n", 0, 1000, None, false);
        assert_eq!(r.output, "hello\r\n");
        assert_eq!(r.chars_written, 7);
    }

    #[test]
    fn test_write_chunk_crlf_line_truncated() {
        let r = write_chunk("hello world\r\n", 0, 1000, Some(10), false);

        assert_eq!(r.output, "[...truncated]\r\n");
        assert!(r.truncated);
    }

    #[test]
    fn test_write_chunk_accumulates_nchars() {
        let r1 = write_chunk("hello\n", 0, 1000, None, false);
        assert_eq!(r1.new_nchars, 6);
        let r2 = write_chunk("world\n", r1.new_nchars, 1000, None, false);
        assert_eq!(r2.new_nchars, 12);
        assert_eq!(r2.output, "world\n");
    }

    #[test]
    fn test_write_chunk_utf16_emoji() {
        let r = write_chunk("👋\n", 0, 1000, None, false);

        assert_eq!(r.output, "👋\n");
        assert_eq!(r.chars_written, 3);
        assert_eq!(r.new_nchars, 3);
    }

    #[test]
    fn test_write_chunk_utf16_emoji_line_truncation() {
        let r = write_chunk("a👋b👋c\n", 0, 1000, Some(4), false);

        assert_eq!(r.output, "[...truncated]\n");
        assert!(r.truncated);
    }

    #[test]
    fn test_write_chunk_utf16_emoji_line_truncation_keeps_prefix() {
        let r = write_chunk("👋👋👋👋👋👋👋👋👋👋\n", 0, 1000, Some(18), false);

        assert_eq!(r.output, "👋[...truncated]\n");
        assert!(r.truncated);
    }
}
