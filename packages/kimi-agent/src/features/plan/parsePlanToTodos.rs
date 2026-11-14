use crate::features::todo::todoItem::{TodoItem, TodoKind, TodoStatus};

pub fn parse_plan_to_todos(content: &str) -> Option<Vec<TodoItem>> {
    let mut items = Vec::new();
    let mut current_milestone: Option<String> = None;
    let mut next_milestone_index = 1usize;
    let mut next_leaf_index = 1usize;
    let mut saw_any_structure = false;
    for raw_line in content.lines() {
        let line = trim_end_js(raw_line);
        if trim_js(line).is_empty() {
            continue;
        }
        if let Some(title) = parse_heading(line) {
            let id = format!("M{next_milestone_index}");
            next_milestone_index += 1;
            current_milestone = Some(id.clone());
            next_leaf_index = 1;
            items.push(TodoItem {
                id,
                parent_id: None,
                kind: TodoKind::Milestone,
                title,
                status: TodoStatus::Pending,
                progress: None,
                description: None,
            });
            saw_any_structure = true;
            continue;
        }
        let Some(parent_id) = current_milestone.clone() else {
            continue;
        };
        let Some((title, status)) = parse_leaf_item(line) else {
            continue;
        };
        let id = format!("{parent_id}.{next_leaf_index}");
        next_leaf_index += 1;
        items.push(TodoItem {
            id,
            parent_id: Some(parent_id),
            kind: TodoKind::Task,
            title,
            status,
            progress: None,
            description: None,
        });
        saw_any_structure = true;
    }
    if !saw_any_structure
        || !items.iter().any(|item| item.kind == TodoKind::Milestone)
        || !items.iter().any(|item| item.kind == TodoKind::Task)
    {
        return None;
    }
    Some(items)
}

fn parse_heading(line: &str) -> Option<String> {
    let level = if line.starts_with("###") {
        3
    } else if line.starts_with("##") {
        2
    } else {
        return None;
    };
    let after = &line[level..];
    if !after.chars().next().is_some_and(is_js_whitespace) {
        return None;
    }
    let after = trim_start_js(after);
    if after.is_empty() {
        return None;
    }
    let without_trailing = trim_end_js(after);
    let without_hashes = without_trailing.trim_end_matches('#');
    if without_hashes.len() == without_trailing.len() {
        return Some(trim_js(without_trailing).to_string());
    }
    let title = trim_end_js(without_hashes);
    if title.is_empty() {
        return Some("#".to_string());
    }
    Some(trim_js(title).to_string())
}

fn parse_leaf_item(line: &str) -> Option<(String, TodoStatus)> {
    let trimmed = trim_start_js(line);
    let rest = if let Some(rest) = trimmed.strip_prefix('-').or_else(|| trimmed.strip_prefix('*')) {
        skip_required_whitespace(rest)?
    } else {
        let digits = trimmed
            .chars()
            .take_while(|character| character.is_ascii_digit())
            .count();
        if digits == 0 || !trimmed[digits..].starts_with('.') {
            return None;
        }
        skip_required_whitespace(&trimmed[digits + 1..])?
    };
    for (marker, status) in [
        ("[x]", TodoStatus::Done),
        ("[X]", TodoStatus::Done),
        ("[ ]", TodoStatus::Pending),
    ] {
        if let Some(after) = rest.strip_prefix(marker) {
            let title = trim_js(trim_start_js(after)).to_string();
            if !title.is_empty() {
                return Some((title, status));
            }
        }
    }
    let title = trim_js(rest).to_string();
    (!title.is_empty()).then_some((title, TodoStatus::Pending))
}

fn skip_required_whitespace(value: &str) -> Option<&str> {
    if !value.chars().next().is_some_and(is_js_whitespace) {
        return None;
    }
    Some(trim_start_js(value))
}

fn trim_js(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

fn trim_start_js(value: &str) -> &str {
    value.trim_start_matches(is_js_whitespace)
}

fn trim_end_js(value: &str) -> &str {
    value.trim_end_matches(is_js_whitespace)
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
