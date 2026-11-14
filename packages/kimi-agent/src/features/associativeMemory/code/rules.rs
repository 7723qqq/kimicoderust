use serde_json::Value;

use super::super::types::{CodeAction, CodeScope, HardRule};

pub fn to_hard_rule(value: &Value) -> Option<HardRule> {
    let record = value.as_object()?;
    let id = record.get("id")?.as_str()?.to_string();
    let pattern = record.get("pattern")?.as_str()?.to_string();
    let action = match record.get("action")?.as_str()? {
        "block_write" => CodeAction::BlockWrite,
        "block_recall" => CodeAction::BlockRecall,
        "pin" => CodeAction::Pin,
        "supersede" => CodeAction::Supersede,
        "block_embed" => CodeAction::BlockEmbed,
        _ => CodeAction::Other,
    };
    let scope = match record.get("scope").and_then(Value::as_str) {
        Some("recall") => CodeScope::Recall,
        Some("atom") => CodeScope::Atom,
        _ => CodeScope::Note,
    };
    let reason = record
        .get("reason")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| id.clone());
    Some(HardRule {
        id,
        pattern,
        regex: record.get("regex") == Some(&Value::Bool(true)),
        action,
        scope,
        reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_valid_rule_is_translated_field_by_field() {
        let rule = to_hard_rule(&json!({
            "id": "credential",
            "pattern": "sk-.+",
            "regex": true,
            "action": "block_write",
            "scope": "note",
            "reason": "credential-like secret",
        }))
        .expect("a complete rule parses");
        assert_eq!(rule.id, "credential");
        assert_eq!(rule.pattern, "sk-.+");
        assert!(rule.regex);
        assert_eq!(rule.reason, "credential-like secret");
    }

    #[test]
    fn missing_fields_are_rejected_and_defaults_applied() {
        assert!(to_hard_rule(&json!({ "id": "x" })).is_none());
        assert!(to_hard_rule(&json!({ "pattern": "x" })).is_none());
        assert!(to_hard_rule(&json!({ "id": "x", "pattern": "y", "action": 1 })).is_none());
        assert!(to_hard_rule(&Value::Null).is_none());

        let rule = to_hard_rule(&json!({ "id": "x", "pattern": "y", "action": "pin" }))
            .expect("minimal fields parse");
        assert!(!rule.regex, "regex defaults to false");
        assert_eq!(rule.scope, CodeScope::Note, "an unknown scope falls back to note");
        assert_eq!(rule.reason, "x", "a missing reason falls back to the id");
    }

    #[test]
    fn the_recall_and_atom_scopes_survive() {
        for (raw, expected) in [("recall", CodeScope::Recall), ("atom", CodeScope::Atom)] {
            let rule =
                to_hard_rule(&json!({ "id": "x", "pattern": "y", "action": "pin", "scope": raw }))
                    .unwrap();
            assert_eq!(rule.scope, expected);
        }
    }
}
