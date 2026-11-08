//! MCP input-schema compaction (v2 `mcpCore/schema-compaction.ts`).
//!
//! An MCP server ships its full JSON Schema — prose descriptions, `title`,
//! `examples`, `$comment` and all. The model only needs the shape plus a
//! one-line hint per parameter, so the schema advertised in the tool table is
//! compacted before it costs context: the prose-only keys are dropped and every
//! `description` is reduced to its first sentence, capped at 160 characters.
//!
//! The source schema is never mutated: execution dispatches on the tool name
//! and passes the model's arguments through, so nothing reads the description
//! after this point.

use serde_json::{Map, Value};

const MAX_DESCRIPTION_CHARS: usize = 160;
const MAX_DEPTH: usize = 64;
const DROPPED_KEYS: [&str; 3] = ["title", "examples", "$comment"];

/// v2 `compactMcpInputSchema`: a compacted deep copy.
pub fn compact_mcp_input_schema(schema: &Value) -> Value {
    compact_value(schema, 0)
}

fn compact_value(value: &Value, depth: usize) -> Value {
    if depth > MAX_DEPTH {
        return value.clone();
    }
    match value {
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| compact_value(item, depth + 1))
                .collect(),
        ),
        Value::Object(object) => {
            let mut compacted = Map::with_capacity(object.len());
            for (key, child) in object {
                if DROPPED_KEYS.contains(&key.as_str()) {
                    continue;
                }
                if key == "description"
                    && let Value::String(text) = child
                {
                    compacted.insert(key.clone(), Value::String(first_sentence(text)));
                    continue;
                }
                compacted.insert(key.clone(), compact_value(child, depth + 1));
            }
            Value::Object(compacted)
        }
        other => other.clone(),
    }
}

/// v2 `firstSentence`: whitespace-collapsed, cut at the first `.`/`!`/`?` that
/// ends a word, capped at [`MAX_DESCRIPTION_CHARS`] with a `…`. The cap
/// prefers the last word boundary once it is past 40 characters, so a
/// description is shortened at a space rather than mid-word.
fn first_sentence(text: &str) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return String::new();
    }
    let sentence = first_sentence_end(&normalized)
        .map(|end| &normalized[..end])
        .unwrap_or(normalized.as_str());
    if sentence.chars().count() <= MAX_DESCRIPTION_CHARS {
        return sentence.to_string();
    }
    let cut: String = sentence.chars().take(MAX_DESCRIPTION_CHARS).collect();
    let body = match cut.rfind(' ') {
        Some(index) if index > 40 => &cut[..index],
        _ => cut.as_str(),
    };
    format!("{}…", body.trim_end())
}

/// The byte index just past the first `.`/`!`/`?` followed by whitespace or the
/// end of the string — v2's `/^.*?[.!?](?=\s|$)/` on the collapsed text.
fn first_sentence_end(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if matches!(byte, b'.' | b'!' | b'?')
            && bytes
                .get(index + 1)
                .is_none_or(|next| next.is_ascii_whitespace())
        {
            return Some(index + 1);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// v2 `schema-compaction.test.ts`: parameter descriptions are cut to their
    /// first sentence, prose-only keys are dropped, and everything the model
    /// needs to call the tool survives.
    #[test]
    fn shortens_descriptions_and_drops_prose_only_keys() {
        let schema = json!({
            "type": "object",
            "required": ["query"],
            "properties": {
                "query": {
                    "type": "string",
                    "title": "Query",
                    "description": "Query the thing. Extra parameter detail.",
                    "examples": ["moon"]
                },
                "mode": {
                    "type": "string",
                    "enum": ["fast", "slow"],
                    "description": "Execution mode. Choose carefully."
                }
            },
            "additionalProperties": false
        });

        let compacted = compact_mcp_input_schema(&schema);
        let query = &compacted["properties"]["query"];
        let mode = &compacted["properties"]["mode"];

        assert_eq!(query["description"], "Query the thing.");
        assert!(query.get("title").is_none());
        assert!(query.get("examples").is_none());
        assert_eq!(query["type"], "string");
        assert_eq!(mode["description"], "Execution mode.");
        assert_eq!(mode["enum"], json!(["fast", "slow"]));
        assert_eq!(compacted["required"], json!(["query"]));
        assert_eq!(compacted["additionalProperties"], false);
    }

    #[test]
    fn does_not_mutate_the_source_schema() {
        let schema = json!({
            "type": "object",
            "properties": { "query": { "type": "string", "description": "First. Second." } }
        });

        let _ = compact_mcp_input_schema(&schema);

        assert_eq!(
            schema["properties"]["query"]["description"],
            "First. Second."
        );
    }

    #[test]
    fn compacts_nested_array_item_descriptions() {
        let compacted = compact_mcp_input_schema(&json!({
            "type": "array",
            "items": {
                "type": "object",
                "properties": { "value": { "type": "string", "description": "Value first. Value second." } }
            }
        }));

        assert_eq!(
            compacted["items"]["properties"]["value"]["description"],
            "Value first."
        );
    }

    #[test]
    fn truncates_an_overlong_first_sentence_with_an_ellipsis() {
        let description = "long description ".repeat(20);
        let compacted = compact_mcp_input_schema(&json!({
            "type": "object",
            "properties": { "value": { "type": "string", "description": description } }
        }));

        let text = compacted["properties"]["value"]["description"]
            .as_str()
            .expect("a description");
        assert!(text.ends_with('…'), "{text}");
        assert!(text.chars().count() <= MAX_DESCRIPTION_CHARS + 1, "{text}");
    }
}
