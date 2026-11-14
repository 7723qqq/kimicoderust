use serde_json::Value;

pub mod defaults;
pub mod load;
pub mod proposals;
pub mod rules;

use rules::to_hard_rule;
use super::types::{
    CodeAction, CodeScope, HardRule, MemoryCode, RecallDrop, RecallItem,
};
use crate::_base::text::frontmatter::parse_frontmatter;

pub fn code_allows_embeddings(code: &MemoryCode) -> bool {
    !code
        .rules
        .iter()
        .any(|rule| rule.action == CodeAction::BlockEmbed)
}

pub fn rule_matches(rule: &HardRule, text: &str) -> bool {
    if rule.regex {
        crate::tools::regex_js::is_match_with_flags(&rule.pattern, text, "i")
            .unwrap_or_else(|_| text.to_lowercase().contains(&rule.pattern.to_lowercase()))
    } else {
        text.to_lowercase().contains(&rule.pattern.to_lowercase())
    }
}

pub fn apply_write_rules(text: &str, rules: &[HardRule]) -> (bool, Vec<HardRule>) {
    let matches: Vec<HardRule> = rules
        .iter()
        .filter(|rule| matches!(rule.scope, CodeScope::Note | CodeScope::Atom))
        .filter(|rule| rule_matches(rule, text))
        .cloned()
        .collect();
    let blocking = matches.iter().any(|rule| rule.action == CodeAction::BlockWrite);
    (!blocking, matches)
}

pub fn filter_recall_items(items: &[RecallItem], rules: &[HardRule]) -> (Vec<RecallItem>, Vec<RecallDrop>) {
    let mut kept = Vec::new();
    let mut dropped = Vec::new();
    for item in items {
        let hit = rules.iter().find(|rule| {
            matches!(rule.scope, CodeScope::Recall | CodeScope::Atom)
                && rule.action == CodeAction::BlockRecall
                && rule_matches(rule, &item.text)
        });
        match hit {
            None => kept.push(item.clone()),
            Some(rule) => dropped.push(RecallDrop {
                id: item.id.clone(),
                rule: rule.id.clone(),
            }),
        }
    }
    (kept, dropped)
}

pub fn apply_recall_rules(items: &[RecallItem], rules: &[HardRule]) -> Vec<RecallItem> {
    filter_recall_items(items, rules).0
}

pub fn resolve_conflict(items: &[RecallItem], rules: &[HardRule]) -> Vec<RecallItem> {
    let pinned: std::collections::HashSet<&str> = rules
        .iter()
        .filter(|rule| rule.action == CodeAction::Pin)
        .map(|rule| rule.pattern.as_str())
        .collect();
    let mut sorted = items.to_vec();
    sorted.sort_by(|a, b| {
        let a_pinned = pinned.contains(a.id.as_str());
        let b_pinned = pinned.contains(b.id.as_str());
        if a_pinned != b_pinned {
            return if a_pinned {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            };
        }
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.valid_from.cmp(&a.valid_from))
    });
    sorted
}

pub fn order_conflict_groups(items: &[RecallItem], rules: &[HardRule]) -> Vec<RecallItem> {
    let mut by_group: std::collections::HashMap<&str, Vec<&RecallItem>> =
        std::collections::HashMap::new();
    for item in items {
        if let Some(group) = item.conflict_group.as_deref() {
            by_group.entry(group).or_default().push(item);
        }
    }
    let mut ordered: std::collections::HashMap<&str, Vec<RecallItem>> =
        std::collections::HashMap::new();
    for (group, entries) in &by_group {
        let owned: Vec<RecallItem> = entries.iter().map(|item| (*item).clone()).collect();
        ordered.insert(group, resolve_conflict(&owned, rules));
    }
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut out = Vec::new();
    for item in items {
        let Some(group) = item.conflict_group.as_deref() else {
            out.push(item.clone());
            continue;
        };
        if !seen.insert(group) {
            continue;
        }
        if let Some(entries) = ordered.get(group) {
            out.extend(entries.iter().cloned());
        }
    }
    out
}

pub fn parse_code(content: &str, defaults: &MemoryCode) -> MemoryCode {
    let parsed = match parse_frontmatter(content) {
        Ok(parsed) => parsed,
        Err(_) => return defaults.clone(),
    };
    let rules = match parsed.data.as_object() {
        None => Vec::new(),
        Some(record) => match record.get("rules").and_then(Value::as_array) {
            Some(raw) => raw.iter().filter_map(to_hard_rule).collect(),
            None => Vec::new(),
        },
    };
    MemoryCode {
        rules: defaults.rules.iter().cloned().chain(rules).collect(),
        guidance: if trim_js(&parsed.body).is_empty() {
            defaults.guidance.clone()
        } else {
            trim_js(&parsed.body).to_string()
        },
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::associativeMemory::code::defaults::default_code;
    use crate::features::associativeMemory::types::{NodeType, WriteDecision};

    fn rule(scope: CodeScope, action: CodeAction, pattern: &str, regex: bool) -> HardRule {
        HardRule {
            id: format!("{pattern}-{action:?}"),
            pattern: pattern.into(),
            regex,
            action,
            scope,
            reason: "test".into(),
        }
    }

    fn item(id: &str, text: &str, confidence: f64, valid_from: i64) -> RecallItem {
        RecallItem {
            id: id.into(),
            kind: NodeType::Atom,
            text: text.into(),
            score: 0.0,
            note_id: "note-1".into(),
            concept_id: None,
            confidence,
            valid_from,
            valid_until: None,
            conflict_group: None,
        }
    }

    #[test]
    fn an_embed_block_rule_disables_embeddings() {
        let mut code = default_code();
        assert!(code_allows_embeddings(&code));
        code.rules.push(rule(CodeScope::Note, CodeAction::BlockEmbed, "secret", false));
        assert!(!code_allows_embeddings(&code));
    }

    #[test]
    fn literal_and_regex_rules_match_case_insensitively() {
        let literal = rule(CodeScope::Note, CodeAction::BlockWrite, "Secret", false);
        assert!(rule_matches(&literal, "a SECRET b"));
        let regex = rule(CodeScope::Note, CodeAction::BlockWrite, "^sk-[a-z]+$", true);
        assert!(rule_matches(&regex, "sk-abcdef"));
        assert!(!rule_matches(&regex, "x sk-abcdef"));
        let broken = rule(CodeScope::Note, CodeAction::BlockWrite, "(", true);
        assert!(!rule_matches(&broken, "anything"));
    }

    #[test]
    fn write_rules_apply_to_note_and_atom_scopes_only() {
        let rules = vec![
            rule(CodeScope::Note, CodeAction::BlockWrite, "secret", false),
            rule(CodeScope::Atom, CodeAction::BlockWrite, "atom-only", false),
            rule(CodeScope::Recall, CodeAction::BlockWrite, "recall-only", false),
        ];
        let (allowed, matches) = apply_write_rules("a secret here", &rules);
        assert!(!allowed);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].pattern, "secret");
        let (allowed, matches) = apply_write_rules("nothing here", &rules);
        assert!(allowed);
        assert!(matches.is_empty());
    }

    #[test]
    fn recall_rules_drop_the_matching_item_and_record_the_rule() {
        let rules = vec![rule(CodeScope::Recall, CodeAction::BlockRecall, "hidden", false)];
        let items = vec![item("a", "visible", 0.5, 1), item("b", "hidden text", 0.9, 2)];
        let (kept, dropped) = filter_recall_items(&items, &rules);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].id, "a");
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].id, "b");
        assert_eq!(dropped[0].rule, "hidden-BlockRecall");
        assert_eq!(apply_recall_rules(&items, &rules).len(), 1);
    }

    #[test]
    fn conflict_resolution_prefers_pinned_then_confidence_then_recency() {
        let pin = rule(CodeScope::Recall, CodeAction::Pin, "b", false);
        let items = vec![
            item("a", "low", 0.2, 9),
            item("b", "pinned", 0.1, 1),
            item("c", "high", 0.9, 2),
        ];
        assert_eq!(resolve_conflict(&items, &[pin]).iter().map(|item| item.id.as_str()).collect::<Vec<_>>(), vec!["b", "c", "a"]);
        let no_pin = rule(CodeScope::Recall, CodeAction::Supersede, "x", false);
        assert_eq!(
            resolve_conflict(&items, &[no_pin])
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            vec!["c", "a", "b"]
        );
    }

    #[test]
    fn conflict_groups_are_reordered_in_place() {
        let mut first = item("a", "one", 0.2, 1);
        first.conflict_group = Some("g".into());
        let mut second = item("b", "two", 0.9, 2);
        second.conflict_group = Some("g".into());
        let plain = item("c", "three", 0.5, 3);
        let ordered = order_conflict_groups(&[first.clone(), plain, second.clone()], &[]);
        assert_eq!(
            ordered.iter().map(|item| item.id.as_str()).collect::<Vec<_>>(),
            vec!["b", "a", "c"],
            "the group moves to the position of its first member and keeps only one copy"
        );
    }

    #[test]
    fn code_frontmatter_is_parsed_over_the_defaults() {
        let defaults = default_code();
        let parsed = parse_code(
            "---\nrules:\n  - id: extra\n    pattern: forbidden\n    action: block_write\n---\nSome guidance.\n",
            &defaults,
        );
        assert_eq!(parsed.rules.len(), defaults.rules.len() + 1);
        assert_eq!(parsed.rules.last().unwrap().id, "extra");
        assert_eq!(parsed.guidance, "Some guidance.");
    }

    #[test]
    fn malformed_frontmatter_falls_back_to_the_defaults() {
        let defaults = default_code();
        assert_eq!(
            parse_code("---\nrules: [unclosed\n", &defaults),
            defaults
        );
        let parsed = parse_code("no frontmatter at all", &defaults);
        assert_eq!(parsed.rules.len(), defaults.rules.len());
        assert_eq!(
            parsed.guidance, "no frontmatter at all",
            "v2 keeps the whole text as the body when there is no fence"
        );
    }

    #[test]
    fn write_decision_names_stay_stable() {
        assert_eq!(
            serde_json::to_string(&WriteDecision::Noop).unwrap(),
            "\"noop\""
        );
    }
}
