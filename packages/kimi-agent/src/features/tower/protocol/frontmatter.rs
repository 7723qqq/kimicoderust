use std::collections::BTreeMap;
use std::path::Path;

const FENCE: &str = "---";
const FRONTMATTER_CHUNK_BYTES: usize = 16 * 1024;

pub fn render_frontmatter(fields: &[(&str, &str)]) -> Result<String, String> {
    let mut lines = vec![FENCE.to_string()];
    for &(key, value) in fields {
        if value.contains('\n') || value.contains('\r') {
            return Err(format!("frontmatter value for \"{key}\" must be single-line"));
        }
        lines.push(format!("{key}: {value}"));
    }
    lines.push(FENCE.to_string());
    Ok(lines.join("\n"))
}

pub fn parse_frontmatter(text: &str) -> (BTreeMap<String, String>, String) {
    let lines = js_lines(text);
    if lines.first().map(|line| trim_js(line)) != Some(FENCE) {
        return (BTreeMap::new(), text.to_string());
    }
    let Some(close) = lines.iter().enumerate().skip(1).find_map(|(index, line)| {
        (trim_js(line) == FENCE).then_some(index)
    }) else {
        return (BTreeMap::new(), text.to_string());
    };
    let mut fields = BTreeMap::new();
    for line in &lines[1..close] {
        let Some(separator) = line.find(':') else { continue };
        if separator == 0 {
            continue;
        }
        fields.insert(
            trim_js(&line[..separator]).to_string(),
            trim_js(&line[separator + 1..]).to_string(),
        );
    }
    (fields, trim_js(&lines[close + 1..].join("\n")).to_string())
}

pub async fn read_frontmatter_prefix(path: impl AsRef<Path>) -> std::io::Result<String> {
    use tokio::io::AsyncReadExt;

    let mut file = tokio::fs::File::open(path).await?;
    let mut pending = Vec::new();
    let mut text = String::new();
    let mut buffer = [0_u8; FRONTMATTER_CHUNK_BYTES];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            text.push_str(&String::from_utf8_lossy(&pending));
            return Ok(text);
        }
        pending.extend_from_slice(&buffer[..read]);
        decode_available(&mut pending, &mut text);
        if frontmatter_terminated(&text) {
            return Ok(text);
        }
    }
}

fn decode_available(pending: &mut Vec<u8>, output: &mut String) {
    let mut consumed = 0;
    loop {
        match std::str::from_utf8(&pending[consumed..]) {
            Ok(valid) => {
                output.push_str(valid);
                consumed = pending.len();
                break;
            }
            Err(error) => {
                let valid_up_to = error.valid_up_to();
                output.push_str(std::str::from_utf8(&pending[consumed..consumed + valid_up_to]).unwrap());
                consumed += valid_up_to;
                match error.error_len() {
                    Some(length) => {
                        output.push('\u{fffd}');
                        consumed += length;
                    }
                    None => break,
                }
            }
        }
    }
    pending.drain(..consumed);
}

fn frontmatter_terminated(text: &str) -> bool {
    let lines = js_lines(text);
    if lines.first().map(|line| trim_js(line)) != Some(FENCE) {
        return lines.len() > 1;
    }
    lines.iter().skip(1).any(|line| trim_js(line) == FENCE)
}

fn js_lines(text: &str) -> Vec<&str> {
    text.split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect()
}

fn trim_js(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

fn is_js_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'
            | '\u{000a}'
            | '\u{000b}'
            | '\u{000c}'
            | '\u{000d}'
            | '\u{0020}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'
            | '\u{2001}'
            | '\u{2002}'
            | '\u{2003}'
            | '\u{2004}'
            | '\u{2005}'
            | '\u{2006}'
            | '\u{2007}'
            | '\u{2008}'
            | '\u{2009}'
            | '\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}
