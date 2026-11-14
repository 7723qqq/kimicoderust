use std::path::{Path, PathBuf};

use serde_json::Value;
use tokio::fs;

use super::super::types::{HardRule, MemoryCode};
use super::rules::to_hard_rule;

pub const PROPOSALS_FILE_NAME: &str = "proposals.md";
pub const MAX_PENDING_PROPOSALS: usize = 50;

fn proposals_path(home_dir: &Path, store_root: &str) -> PathBuf {
    home_dir.join(store_root).join(PROPOSALS_FILE_NAME)
}

async fn read_proposal_lines(path: &Path) -> Vec<String> {
    match fs::read(path).await {
        Ok(text) => String::from_utf8_lossy(&text)
            .split('\n')
            .map(trim_js)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect(),
        Err(_) => Vec::new(),
    }
}

pub async fn propose_rule(home_dir: &Path, store_root: &str, rule: &Value) {
    let dir = home_dir.join(store_root);
    fs::create_dir_all(&dir).await.unwrap();
    let path = proposals_path(home_dir, store_root);
    let mut lines = read_proposal_lines(&path).await;
    lines.push(format!("- {rule}"));
    let kept = &lines[lines.len().saturating_sub(MAX_PENDING_PROPOSALS)..];
    fs::write(path, format!("{}\n", kept.join("\n")))
        .await
        .unwrap();
}

pub async fn approve_proposal(
    home_dir: &Path,
    store_root: &str,
    index: usize,
    defaults: &MemoryCode,
) -> Result<HardRule, String> {
    let dir = home_dir.join(store_root);
    let path = proposals_path(home_dir, store_root);
    let mut lines = read_proposal_lines(&path).await;
    let picked = lines
        .get(index)
        .cloned()
        .ok_or_else(|| format!("proposal {index} not found"))?;
    let json = picked.strip_prefix('-').unwrap_or(&picked).trim_start();
    let rule = to_hard_rule(
        &serde_json::from_str::<Value>(json)
            .map_err(|_| format!("proposal {index} is not valid JSON"))?,
    )
    .ok_or_else(|| format!("proposal {index} is not a valid rule"))?;

    let code_path = dir.join("code.md");
    let content = fs::read(&code_path).await.unwrap_or_default();
    let content = String::from_utf8_lossy(&content).into_owned();
    let parsed = crate::_base::text::frontmatter::parse_frontmatter(&content).ok();
    let existing: Vec<HardRule> = parsed
        .as_ref()
        .and_then(|parsed| parsed.data.get("rules"))
        .and_then(Value::as_array)
        .map(|raw| {
            raw.iter()
                .filter_map(|value| serde_json::from_value(value.clone()).ok())
                .collect()
        })
        .unwrap_or_default();
    let mut rules = defaults.rules.clone();
    rules.extend(existing);
    rules.push(rule.clone());

    let body = parsed
        .as_ref()
        .map(|parsed| trim_js(&parsed.body).to_string())
        .unwrap_or_default();
    let yaml = serde_yaml::to_string(&serde_json::json!({ "rules": rules }))
        .map_err(|error| format!("encode code.md: {error}"))?;
    let mut output = format!("---\n{yaml}---\n");
    if !body.is_empty() {
        output.push_str(&format!("{body}\n"));
    }
    fs::write(&code_path, output)
        .await
        .map_err(|error| format!("write code.md: {error}"))?;

    lines.remove(index);
    if lines.is_empty() {
        fs::write(&path, "")
            .await
            .map_err(|error| format!("write proposals.md: {error}"))?;
    } else {
        fs::write(path, format!("{}\n", lines.join("\n")))
            .await
            .map_err(|error| format!("write proposals.md: {error}"))?;
    }
    Ok(rule)
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

    #[tokio::test]
    async fn proposals_are_appended_and_capped() {
        let dir = tempfile::tempdir().unwrap();
        propose_rule(dir.path(), "store", &serde_json::json!({ "id": "r1" })).await;
        propose_rule(dir.path(), "store", &serde_json::json!({ "id": "r2" })).await;
        let path = proposals_path(dir.path(), "store");
        let text = fs::read_to_string(&path).await.unwrap();
        assert_eq!(text.matches("- {").count(), 2);
        assert!(text.ends_with('\n'));
    }

    #[tokio::test]
    async fn approving_writes_the_rule_into_code_md_and_drops_the_proposal() {
        let dir = tempfile::tempdir().unwrap();
        propose_rule(
            dir.path(),
            "store",
            &serde_json::json!({
                "id": "extra",
                "pattern": "forbidden",
                "action": "block_write",
                "scope": "note",
            }),
        )
        .await;
        let defaults = default_code();
        let rule = approve_proposal(dir.path(), "store", 0, &defaults)
            .await
            .expect("the proposal approves");
        assert_eq!(rule.id, "extra");
        let code = fs::read_to_string(dir.path().join("store/code.md"))
            .await
            .unwrap();
        assert!(code.contains("extra"));
        assert!(code.contains("credential"), "the defaults are preserved");
        let proposals = fs::read_to_string(proposals_path(dir.path(), "store"))
            .await
            .unwrap();
        assert!(proposals.is_empty());
        assert!(approve_proposal(dir.path(), "store", 0, &defaults).await.is_err());
    }

    #[tokio::test]
    async fn an_invalid_proposal_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("store")).await.unwrap();
        fs::write(
            proposals_path(dir.path(), "store"),
            "- {not json}\n",
        )
        .await
        .unwrap();
        assert!(approve_proposal(dir.path(), "store", 0, &default_code())
            .await
            .is_err());
    }
}
