use std::collections::BTreeMap;
use std::path::Path;

const FENCE: &str = "---";
const FRONTMATTER_CHUNK_BYTES: usize = 16 * 1024;

pub fn render_frontmatter(fields: &[(&str, &str)]) -> Result<String, String> {
    let mut lines = vec![FENCE.to_string()];
    for &(key, value) in fields {
        if value.contains('\n') || value.contains('\r') {
            return Err(format!(
                "frontmatter value for \"{key}\" must be single-line"
            ));
        }
        lines.push(format!("{key}: {}", quote_if_needed(value)));
    }
    lines.push(FENCE.to_string());
    Ok(lines.join("\n"))
}

/// Quote a scalar that YAML would otherwise read as structure.
///
/// `subject: Re: review of feat/foo` is **not** a mapping whose value is
/// `Re: review of feat/foo` — the second `: ` makes it invalid, and both
/// `js-yaml` (what v2 parses with) and `serde_yaml` reject the whole document.
/// A subject line is exactly where a `Re:` prefix shows up, so the writer has to
/// escape it rather than emit a file that will not parse back.
fn quote_if_needed(value: &str) -> String {
    let needs_quotes = value.is_empty()
        // A leading YAML indicator character.
        || value.starts_with(['-', '?', ':', ',', '[', ']', '{', '}', '#', '&', '*', '!', '|', '>', '\'', '"', '%', '@', '`'])
        // `: ` makes it a nested mapping, and a trailing `:` a key with no value.
        || value.contains(": ")
        || value.ends_with(':')
        || value.starts_with(' ')
        || value.ends_with(' ')
        // `#` only starts a comment when preceded by whitespace.
        || value.contains(" #");
    if !needs_quotes {
        return value.to_string();
    }
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// Parse a tower message's frontmatter into flat string fields plus its body.
///
/// Delegates to the shared YAML reader (v2 `parseFrontmatter`) and then
/// flattens to strings, because every consumer here wants a `BTreeMap<String,
/// String>`: the tower messages this parses are machine-written by
/// [`render_frontmatter`], whose values are always single-line scalars.
///
/// A document whose frontmatter is not valid YAML, or whose top level is not a
/// mapping, degrades to "no frontmatter, whole file is body" — the same
/// fallback this had when it read lines by hand. A non-scalar value is
/// rendered as compact JSON so a list-valued field still round-trips instead of
/// disappearing.
pub fn parse_frontmatter(text: &str) -> (BTreeMap<String, String>, String) {
    let Ok(parsed) = crate::frontmatter::parse_frontmatter(text) else {
        return (BTreeMap::new(), text.to_string());
    };
    let fields = parsed
        .data
        .into_iter()
        .map(|(key, value)| {
            let rendered = match value {
                serde_json::Value::String(s) => s,
                serde_json::Value::Null => String::new(),
                other => other.to_string(),
            };
            (key, rendered)
        })
        .collect();
    (fields, parsed.body.trim().to_string())
}

pub async fn read_frontmatter_prefix(path: impl AsRef<Path>) -> std::io::Result<String> {
    use tokio::io::AsyncReadExt;

    let mut file = tokio::fs::File::open(path).await?;
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; FRONTMATTER_CHUNK_BYTES];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            return String::from_utf8(bytes)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error));
        }
        bytes.extend_from_slice(&buffer[..read]);
        if frontmatter_terminated(&String::from_utf8_lossy(&bytes)) {
            return String::from_utf8(bytes)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error));
        }
    }
}

fn frontmatter_terminated(text: &str) -> bool {
    let mut lines = text.split('\n');
    if lines.next().is_none_or(|line| line.trim() != FENCE) {
        return text.contains('\n');
    }
    lines.any(|line| line.trim() == FENCE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_frontmatter_roundtrip() {
        let fields = vec![
            ("type", "inbox"),
            ("from", "worker-1"),
            ("to", "tower"),
            ("subject", "Task done"),
        ];
        let rendered = render_frontmatter(&fields).unwrap();
        let (parsed, body) = parse_frontmatter(&format!("{rendered}\n\nHere is the body text."));
        assert_eq!(parsed.get("type").unwrap(), "inbox");
        assert_eq!(parsed.get("from").unwrap(), "worker-1");
        assert_eq!(parsed.get("to").unwrap(), "tower");
        assert_eq!(parsed.get("subject").unwrap(), "Task done");
        assert_eq!(body, "Here is the body text.");
    }

    /// A rendered message must survive a round trip through the YAML reader.
    /// `Re:` in a subject is the real case: written bare, `subject: Re: review`
    /// is invalid YAML (the second `: ` reads as a nested mapping) and the whole
    /// front block fails to parse — the message then arrives with no fields at
    /// all, silently. `js-yaml`, which v2 parses with, rejects it too.
    #[test]
    fn a_rendered_message_round_trips_values_containing_a_colon() {
        let subject = "Re: review of feat/foo";
        let rendered = render_frontmatter(&[("type", "inbox"), ("subject", subject)]).unwrap();
        let (fields, _body) = parse_frontmatter(&rendered);
        assert_eq!(
            fields.get("subject").map(String::as_str),
            Some(subject),
            "rendered={rendered:?} parsed={fields:?}"
        );
    }

    /// Quoting must not alter the value: the reader strips the quotes, so a
    /// subject that merely *contains* a colon survives byte for byte.
    #[test]
    fn quoting_is_transparent_to_the_value() {
        for value in [
            "plain",
            "has: colon",
            "trailing:",
            "# leading hash",
            "hash # inside",
            "- dash start",
            "quote\"inside",
            "back\\slash",
            "",
            " padded ",
        ] {
            let rendered = render_frontmatter(&[("k", value)]).unwrap();
            let (fields, _body) = parse_frontmatter(&rendered);
            assert_eq!(
                fields.get("k").map(String::as_str),
                Some(value),
                "value {value:?} did not round trip (rendered={rendered:?})"
            );
        }
    }

    #[tokio::test]
    async fn prefix_reader_stops_after_the_closing_fence() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("message.md");
        let rendered = render_frontmatter(&[("type", "inbox"), ("seq", "7")]).unwrap();
        let content = format!("{rendered}\n\n{}", "body".repeat(20_000));
        tokio::fs::write(&path, &content).await.unwrap();

        let prefix = read_frontmatter_prefix(&path).await.unwrap();

        assert!(prefix.len() < content.len());
        let (fields, _) = parse_frontmatter(&prefix);
        assert_eq!(fields.get("seq").map(String::as_str), Some("7"));
    }

    #[tokio::test]
    async fn prefix_reader_keeps_frontmatter_that_crosses_a_chunk() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("review.md");
        let long_value = "x".repeat(FRONTMATTER_CHUNK_BYTES + 2_000);
        let rendered = render_frontmatter(&[("seq", "42"), ("subject", &long_value)]).unwrap();
        let content = format!("{rendered}\n\nbody");
        tokio::fs::write(&path, &content).await.unwrap();

        let prefix = read_frontmatter_prefix(&path).await.unwrap();

        let (fields, _) = parse_frontmatter(&prefix);
        assert_eq!(fields.get("seq").map(String::as_str), Some("42"));
        assert_eq!(fields.get("subject"), Some(&long_value));
    }
}
