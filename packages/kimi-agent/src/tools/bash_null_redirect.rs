const NUL_TARGET_BOUNDARY: fn(char) -> bool =
    |ch: char| matches!(ch, ' ' | '\t' | '\n' | '\r' | '|' | '&' | ';' | ')');

struct HeredocDelimiter {
    delimiter: String,
    start: usize,
}

pub fn rewrite_windows_null_redirect(command: &str) -> String {
    rewrite_nul_redirects(&command.chars().collect::<Vec<_>>())
}

fn is_nul_redirect_at(chars: &[char], at: usize) -> bool {
    let Some(word) = chars.get(at..at + 3) else {
        return false;
    };
    if !word.iter().collect::<String>().eq_ignore_ascii_case("nul") {
        return false;
    }
    chars.get(at + 3).is_none_or(|ch| NUL_TARGET_BOUNDARY(*ch))
}

fn nul_redirect_word_start(chars: &[char], at: usize) -> Option<usize> {
    let mut cursor = at + 1;
    if chars.get(cursor) == Some(&'>') {
        cursor += 1;
    }
    while matches!(chars.get(cursor), Some(' ') | Some('\t')) {
        cursor += 1;
    }
    is_nul_redirect_at(chars, cursor).then_some(cursor)
}

fn read_heredoc_delimiter(chars: &[char], at: usize) -> Option<HeredocDelimiter> {
    let mut cursor = at + 2;
    if chars.get(cursor) == Some(&'-') {
        cursor += 1;
    }
    while matches!(chars.get(cursor), Some(' ') | Some('\t')) {
        cursor += 1;
    }
    let quote = *chars.get(cursor)?;
    if quote == '\'' || quote == '"' {
        let mut close = cursor + 1;
        while close < chars.len() && chars[close] != quote {
            close += 1;
        }
        if close >= chars.len() {
            return None;
        }
        let delimiter: String = chars[cursor + 1..close].iter().collect();
        if delimiter.is_empty() {
            return None;
        }
        return Some(HeredocDelimiter {
            delimiter,
            start: at,
        });
    }
    let word_start = cursor;
    while let Some(&ch) = chars.get(cursor) {
        if matches!(
            ch,
            ' ' | '\t' | '\n' | '\r' | '|' | '&' | ';' | '(' | ')' | '<' | '>'
        ) {
            break;
        }
        cursor += 1;
    }
    if cursor == word_start {
        return None;
    }
    Some(HeredocDelimiter {
        delimiter: chars[word_start..cursor].iter().collect(),
        start: at,
    })
}

fn end_of_line_including_newline(chars: &[char], at: usize) -> usize {
    chars
        .get(at..)
        .and_then(|tail| {
            tail.iter()
                .position(|ch| *ch == '\n')
                .map(|offset| at + offset + 1)
        })
        .unwrap_or(chars.len())
}

fn heredoc_body_end(chars: &[char], at: usize, delimiter: &str) -> usize {
    let mut cursor = at;
    while cursor < chars.len() {
        let line_end = end_of_line_including_newline(chars, cursor);
        let line: String = chars[cursor..line_end].iter().collect();
        cursor = line_end;
        let trimmed = line.trim();
        if trimmed == delimiter || trimmed == format!("{delimiter};") {
            return cursor;
        }
    }
    chars.len()
}

fn read_substitution_body(chars: &[char], at: usize) -> (String, usize) {
    let mut depth = 1usize;
    let mut cursor = at;
    while cursor < chars.len() {
        let ch = chars[cursor];
        if ch == '\\' {
            cursor += 2;
            continue;
        }
        if ch == '\'' || ch == '"' {
            match chars
                .get(cursor + 1..)
                .and_then(|tail| tail.iter().position(|c| *c == ch))
            {
                Some(close) => cursor += close + 2,
                None => return (chars[at..].iter().collect(), chars.len()),
            }
            continue;
        }
        if ch == '(' {
            depth += 1;
        }
        if ch == ')' {
            depth -= 1;
            if depth == 0 {
                return (chars[at..cursor].iter().collect(), cursor + 1);
            }
        }
        cursor += 1;
    }
    (chars[at..].iter().collect(), chars.len())
}

fn read_backtick_body(chars: &[char], at: usize) -> (String, usize) {
    let mut cursor = at;
    while cursor < chars.len() {
        if chars[cursor] == '\\' {
            cursor += 2;
            continue;
        }
        if chars[cursor] == '`' {
            return (chars[at..cursor].iter().collect(), cursor + 1);
        }
        cursor += 1;
    }
    (chars[at..].iter().collect(), chars.len())
}

fn slice(chars: &[char], from: usize, to: usize) -> String {
    chars
        .get(from..to.min(chars.len()))
        .unwrap_or(&[])
        .iter()
        .collect()
}

fn rewrite_nul_redirects(chars: &[char]) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut plain = String::new();
    let mut cursor = 0usize;
    let mut in_single = false;
    let mut in_double = false;

    let flush = |out: &mut Vec<String>, plain: &mut String| {
        if !plain.is_empty() {
            out.push(std::mem::take(plain));
        }
    };

    while cursor < chars.len() {
        let ch = chars[cursor];

        if in_single {
            plain.push(ch);
            if ch == '\'' {
                in_single = false;
            }
            cursor += 1;
            continue;
        }

        if ch == '\\' {
            plain.push('\\');
            if let Some(&next) = chars.get(cursor + 1) {
                plain.push(next);
                cursor += 2;
            } else {
                cursor += 1;
            }
            continue;
        }

        if in_double {
            if ch == '"' {
                plain.push('"');
                in_double = false;
                cursor += 1;
                continue;
            }
            if ch == '$' && chars.get(cursor + 1) == Some(&'(') {
                let (body, end) = read_substitution_body(chars, cursor + 2);
                flush(&mut out, &mut plain);
                out.push("$(".to_string());
                out.push(rewrite_nul_redirects(&body.chars().collect::<Vec<_>>()));
                out.push(")".to_string());
                cursor = end;
                continue;
            }
            if ch == '`' {
                let (body, end) = read_backtick_body(chars, cursor + 1);
                flush(&mut out, &mut plain);
                out.push("`".to_string());
                out.push(rewrite_nul_redirects(&body.chars().collect::<Vec<_>>()));
                out.push("`".to_string());
                cursor = end;
                continue;
            }
            plain.push(ch);
            cursor += 1;
            continue;
        }

        if ch == '\'' || ch == '"' {
            flush(&mut out, &mut plain);
            in_single = ch == '\'';
            in_double = ch == '"';
            out.push(ch.to_string());
            cursor += 1;
            continue;
        }

        if ch == '`' {
            let (body, end) = read_backtick_body(chars, cursor + 1);
            flush(&mut out, &mut plain);
            out.push("`".to_string());
            out.push(rewrite_nul_redirects(&body.chars().collect::<Vec<_>>()));
            out.push("`".to_string());
            cursor = end;
            continue;
        }

        if ch == '$' && chars.get(cursor + 1) == Some(&'(') {
            let (body, end) = read_substitution_body(chars, cursor + 2);
            flush(&mut out, &mut plain);
            out.push("$(".to_string());
            out.push(rewrite_nul_redirects(&body.chars().collect::<Vec<_>>()));
            out.push(")".to_string());
            cursor = end;
            continue;
        }

        if ch == '#' {
            let previous = plain.chars().last();
            let starts_comment = previous.is_none()
                || previous
                    .is_some_and(|c| matches!(c, ' ' | '\t' | '\n' | '\r' | ';' | '|' | '&' | '('));
            if starts_comment {
                let line_end = end_of_line_including_newline(chars, cursor);
                plain.push_str(&slice(chars, cursor, line_end));
                cursor = line_end;
                continue;
            }
        }

        if ch == '<' && chars.get(cursor + 1) == Some(&'<') && chars.get(cursor + 2) != Some(&'<') {
            if let Some(heredoc) = read_heredoc_delimiter(chars, cursor) {
                let line_end = end_of_line_including_newline(chars, heredoc.start);
                let body_end = heredoc_body_end(chars, line_end, &heredoc.delimiter);
                plain.push_str(&slice(chars, cursor, line_end));
                flush(&mut out, &mut plain);
                out.push(slice(chars, line_end, body_end));
                cursor = body_end;
                continue;
            }
            plain.push_str("<<");
            cursor += 2;
            continue;
        }

        if ch == '>'
            && let Some(nul_start) = nul_redirect_word_start(chars, cursor)
        {
            plain.push_str(&slice(chars, cursor, nul_start));
            plain.push_str("/dev/null");
            cursor = nul_start + 3;
            continue;
        }

        plain.push(ch);
        cursor += 1;
    }
    flush(&mut out, &mut plain);
    out.concat()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_nul_redirect_becomes_dev_null() {
        assert_eq!(
            rewrite_windows_null_redirect("cmd > NUL"),
            "cmd > /dev/null"
        );
        assert_eq!(rewrite_windows_null_redirect("cmd >NUL"), "cmd >/dev/null");
        assert_eq!(
            rewrite_windows_null_redirect("cmd 2>NUL"),
            "cmd 2>/dev/null"
        );
    }

    #[test]
    fn append_and_whitespace_variants_are_rewritten() {
        assert_eq!(
            rewrite_windows_null_redirect("cmd >> NUL"),
            "cmd >> /dev/null"
        );
        assert_eq!(
            rewrite_windows_null_redirect("cmd > \tNUL"),
            "cmd > \t/dev/null"
        );
    }

    #[test]
    fn the_nul_word_must_end_at_a_boundary() {
        assert_eq!(
            rewrite_windows_null_redirect("cmd > NULFILE"),
            "cmd > NULFILE"
        );
        assert_eq!(
            rewrite_windows_null_redirect("cmd > nul"),
            "cmd > /dev/null"
        );
        assert_eq!(
            rewrite_windows_null_redirect("cmd > NUL|cat"),
            "cmd > /dev/null|cat"
        );
        assert_eq!(
            rewrite_windows_null_redirect("cmd > NUL;next"),
            "cmd > /dev/null;next"
        );
    }

    #[test]
    fn quoted_nul_is_left_alone() {
        assert_eq!(rewrite_windows_null_redirect("echo 'NUL'"), "echo 'NUL'");
        assert_eq!(
            rewrite_windows_null_redirect("echo 'a > NUL'"),
            "echo 'a > NUL'"
        );
    }

    #[test]
    fn a_nul_inside_a_comment_is_left_alone() {
        assert_eq!(
            rewrite_windows_null_redirect("cmd > out # > NUL\ncmd > NUL"),
            "cmd > out # > NUL\ncmd > /dev/null"
        );
    }

    #[test]
    fn a_heredoc_body_is_left_alone_but_the_next_line_is_rewritten() {
        let command = "cat <<'EOF'\n> NUL inside\nEOF\ncmd > NUL";
        assert_eq!(
            rewrite_windows_null_redirect(command),
            "cat <<'EOF'\n> NUL inside\nEOF\ncmd > /dev/null"
        );
    }

    #[test]
    fn a_command_substitution_is_recursively_rewritten() {
        assert_eq!(
            rewrite_windows_null_redirect("echo $(cmd > NUL)"),
            "echo $(cmd > /dev/null)"
        );
        assert_eq!(
            rewrite_windows_null_redirect("echo \"$(cmd > NUL)\""),
            "echo \"$(cmd > /dev/null)\""
        );
    }

    #[test]
    fn a_backtick_body_is_recursively_rewritten() {
        assert_eq!(
            rewrite_windows_null_redirect("echo `cmd > NUL`"),
            "echo `cmd > /dev/null`"
        );
    }

    #[test]
    fn an_escaped_redirect_is_not_rewritten() {
        assert_eq!(
            rewrite_windows_null_redirect("echo \\> NUL"),
            "echo \\> NUL"
        );
    }

    #[test]
    fn a_command_without_nul_is_byte_identical() {
        let command = "ls -la 'a b' && echo \"hi\" # tail\n";
        assert_eq!(rewrite_windows_null_redirect(command), command);
    }

    #[test]
    fn non_ascii_text_survives_the_walk() {
        let command = "echo '你好' > NUL";
        assert_eq!(
            rewrite_windows_null_redirect(command),
            "echo '你好' > /dev/null"
        );
    }
}
