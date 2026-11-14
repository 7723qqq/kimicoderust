use std::collections::HashSet;
use std::sync::LazyLock;

use serde_json::{Map, Value};

const MAX_DESCRIPTION_CHARS: usize = 160;
const MAX_DEPTH: usize = 64;
static DROPPED_KEYS: LazyLock<HashSet<&'static str>> =
    LazyLock::new(|| HashSet::from(["title", "examples", "$comment"]));

pub fn compact_mcp_input_schema(schema: &Map<String, Value>) -> Map<String, Value> {
    compact_value(&Value::Object(schema.clone()), 0)
        .as_object()
        .cloned()
        .unwrap()
}

fn compact_value(value: &Value, depth: usize) -> Value {
    if depth > MAX_DEPTH {
        return value.clone();
    }
    match value {
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(|value| compact_value(value, depth + 1))
                .collect(),
        ),
        Value::Object(values) => {
            let mut compacted = Map::new();
            for (key, child) in values {
                if DROPPED_KEYS.contains(key.as_str()) {
                    continue;
                }
                if key == "description" && child.is_string() {
                    compacted.insert(key.clone(), first_sentence(child.as_str().unwrap()).into());
                } else {
                    compacted.insert(key.clone(), compact_value(child, depth + 1));
                }
            }
            Value::Object(compacted)
        }
        value => value.clone(),
    }
}

fn first_sentence(text: &str) -> String {
    let normalized = normalize_js_whitespace(text);
    if normalized.is_empty() {
        return String::new();
    }
    let sentence = normalized
        .char_indices()
        .find_map(|(index, ch)| {
            matches!(ch, '.' | '!' | '?')
                .then_some(index + ch.len_utf8())
                .filter(|end| normalized[end..].chars().next().is_none_or(|ch| ch == ' '))
                .map(|end| normalized[..end].to_string())
        })
        .unwrap_or(normalized);
    if sentence.encode_utf16().count() <= MAX_DESCRIPTION_CHARS {
        return sentence;
    }
    let cut = take_utf16(&sentence, MAX_DESCRIPTION_CHARS);
    let last_space = cut.rfind(' ');
    let last_space_units = last_space.map(|index| cut[..index].encode_utf16().count());
    let shortened = if last_space_units.is_some_and(|index| index > 40) {
        cut[..last_space.unwrap()].to_string()
    } else {
        cut
    };
    format!("{}…", shortened.trim_end_matches(' '))
}

fn normalize_js_whitespace(text: &str) -> String {
    let mut normalized = String::new();
    let mut pending_space = false;
    for ch in text.chars() {
        if is_js_whitespace(ch) {
            pending_space = true;
            continue;
        }
        if pending_space && !normalized.is_empty() {
            normalized.push(' ');
        }
        normalized.push(ch);
        pending_space = false;
    }
    normalized
}

fn is_js_whitespace(ch: char) -> bool {
    matches!(
        ch,
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

fn take_utf16(value: &str, max: usize) -> String {
    let units = value.encode_utf16().take(max).collect::<Vec<_>>();
    String::from_utf16_lossy(&units)
}
