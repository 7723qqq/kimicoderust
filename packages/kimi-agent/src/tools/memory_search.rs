use std::collections::HashMap;
use std::fs;
use std::path::Path;

use serde_json::Value;

use crate::i18n::{LocalizedText, i18n_params};
use crate::tools::memory_paths::{
    MemoryScope, build_snippet, detect_type, extract_title, parse_memory_path,
};
use crate::tools::memory_tool::MemoryContext;
use crate::turn_loop::types::ExecutableToolResult;

const RESULT_LIMIT: usize = 10;

const OVER_FETCH_CAP: usize = 50;

const SCORE_THRESHOLD: f64 = 0.15;

#[derive(Debug, Clone, PartialEq)]
pub struct MemorySearchResult {
    pub path: String,
    pub scope: MemoryScope,
    pub scope_id: String,
    pub memory_type: String,
    pub title: String,
    pub snippet: String,
    pub score: f64,
}

struct Document {
    rel_path: String,
    body: String,
    term_freqs: HashMap<String, u32>,
    len: usize,
}

pub fn search(
    base: &Path,
    query: &str,
    limit: usize,
    filter: Option<(MemoryScope, &str)>,
) -> Vec<MemorySearchResult> {
    let documents = collect_documents(base);
    let n = documents.len() as f64;
    if n == 0.0 {
        return Vec::new();
    }

    let query_terms = unique_terms(&tokenize(query));
    if query_terms.is_empty() {
        return Vec::new();
    }

    let mut document_freq: HashMap<&str, usize> = HashMap::new();
    for document in &documents {
        for term in &query_terms {
            if document.term_freqs.contains_key(term) {
                *document_freq.entry(term.as_str()).or_insert(0) += 1;
            }
        }
    }

    let mut hits: Vec<(usize, f64)> = Vec::new();
    for (index, document) in documents.iter().enumerate() {
        if document.len == 0 {
            continue;
        }
        let mut score = 0.0;
        for term in &query_terms {
            let Some(&tf) = document.term_freqs.get(term) else {
                continue;
            };
            let df = document_freq.get(term.as_str()).copied().unwrap_or(0) as f64;
            score += (tf as f64 / document.len as f64) * (1.0 + n / df.max(1.0)).ln();
        }
        if score > 0.0 {
            hits.push((index, score));
        }
    }

    hits.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| documents[a.0].rel_path.cmp(&documents[b.0].rel_path))
    });
    hits.truncate((limit * 3).clamp(1, OVER_FETCH_CAP));

    let mut results = Vec::new();
    for (index, score) in hits {
        if score < SCORE_THRESHOLD {
            continue;
        }
        let document = &documents[index];
        let Some(parsed) = parse_memory_path(&document.rel_path) else {
            continue;
        };
        if let Some((scope, scope_id)) = filter
            && (parsed.scope != scope || parsed.scope_id != scope_id)
        {
            continue;
        }
        let file_name = parsed.file_name.rsplit('/').next().unwrap_or("");
        results.push(MemorySearchResult {
            path: document.rel_path.clone(),
            scope: parsed.scope,
            scope_id: parsed.scope_id,
            memory_type: detect_type(&document.body).as_str().to_string(),
            title: extract_title(&document.body, file_name),
            snippet: build_snippet(&document.body, query),
            score,
        });
        if results.len() >= limit {
            break;
        }
    }
    results
}

pub fn tokenize(text: &str) -> Vec<String> {
    let normalized = icu_normalizer::ComposingNormalizer::new_nfkc()
        .normalize(text)
        .into_owned()
        .to_lowercase();
    let mut terms = Vec::new();
    let mut latin = String::new();
    for ch in normalized.chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            latin.push(ch);
        } else {
            flush_latin(&mut latin, &mut terms);
        }
    }
    flush_latin(&mut latin, &mut terms);

    let mut cjk: Vec<char> = Vec::new();
    for ch in normalized.chars() {
        if is_cjk(ch) {
            cjk.push(ch);
        } else {
            flush_cjk(&mut cjk, &mut terms);
        }
    }
    flush_cjk(&mut cjk, &mut terms);
    terms
}

fn is_cjk(ch: char) -> bool {
    matches!(ch, '\u{3400}'..='\u{9FFF}' | '\u{3040}'..='\u{30FF}' | '\u{FF00}'..='\u{FFEF}')
}

fn flush_latin(latin: &mut String, terms: &mut Vec<String>) {
    if !latin.is_empty() {
        terms.push(std::mem::take(latin));
    }
}

fn flush_cjk(cjk: &mut Vec<char>, terms: &mut Vec<String>) {
    if cjk.is_empty() {
        return;
    }
    for ch in cjk.iter() {
        terms.push(ch.to_string());
    }
    for pair in cjk.windows(2) {
        terms.push(pair.iter().collect());
    }
    cjk.clear();
}

fn unique_terms(terms: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut unique = Vec::new();
    for term in terms {
        if seen.insert(term.as_str()) {
            unique.push(term.clone());
        }
    }
    unique
}

fn collect_documents(base: &Path) -> Vec<Document> {
    let mut documents = Vec::new();
    walk(base, base, &mut documents);
    documents
}

fn walk(base: &Path, dir: &Path, documents: &mut Vec<Document>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            walk(base, &path, documents);
            continue;
        }
        if !file_type.is_file() || path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Ok(relative) = path.strip_prefix(base) else {
            continue;
        };
        let rel_path = relative.to_string_lossy().replace('\\', "/");
        if parse_memory_path(&rel_path).is_none() {
            continue;
        }
        let Ok(body) = fs::read_to_string(&path) else {
            continue;
        };
        let tokens = tokenize(&body);
        let mut term_freqs: HashMap<String, u32> = HashMap::new();
        for token in &tokens {
            *term_freqs.entry(token.clone()).or_insert(0) += 1;
        }
        documents.push(Document {
            rel_path,
            body,
            term_freqs,
            len: tokens.len(),
        });
    }
}

pub fn search_action(
    context: &MemoryContext,
    args: &Value,
    session_id: &str,
) -> ExecutableToolResult {
    let Some(query) = args
        .get("query")
        .and_then(Value::as_str)
        .filter(|query| !query.is_empty())
    else {
        return error_result(
            LocalizedText::new("engine.tools.memory.searchQueryRequired").render(),
        );
    };

    let scope = memory_action_scope(args);
    let scope_id = scope.map(|scope| match scope {
        MemoryScope::Global => String::new(),
        MemoryScope::Project => context.project_id.clone(),
        MemoryScope::Session => session_id.to_string(),
    });
    let filter = scope.map(|scope| (scope, scope_id.as_deref().unwrap_or("")));
    let results = search(&context.base, query, RESULT_LIMIT, filter);

    if results.is_empty() {
        return ok_result(
            LocalizedText::with_params(
                "engine.tools.memory.noEntriesFound",
                i18n_params!["query" => query],
            )
            .render(),
        );
    }

    let plural = if results.len() == 1 { "y" } else { "ies" };
    let mut lines = vec![format!(
        "{}\n",
        LocalizedText::with_params(
            "engine.tools.memory.foundEntries",
            i18n_params!["count" => results.len(), "plural" => plural],
        )
        .render()
    )];
    for result in &results {
        lines.push(format!("## {}", result.title));
        lines.push(format!("  path: {}", result.path));
        lines.push(format!(
            "  scope: {}{}",
            result.scope.as_str(),
            if result.scope_id.is_empty() {
                String::new()
            } else {
                format!(" ({})", result.scope_id)
            }
        ));
        lines.push(format!("  type: {}", result.memory_type));
        lines.push(format!("  score: {:.3}", result.score));
        lines.push(format!("  snippet: {}", result.snippet));
        lines.push(String::new());
    }
    ok_result(lines.join("\n"))
}

fn memory_action_scope(args: &Value) -> Option<MemoryScope> {
    match args.get("scope").and_then(Value::as_str) {
        Some("global") => Some(MemoryScope::Global),
        Some("project") => Some(MemoryScope::Project),
        Some("session") => Some(MemoryScope::Session),
        _ => None,
    }
}

fn ok_result(text: String) -> ExecutableToolResult {
    ExecutableToolResult {
        spill: None,
        parts: Vec::new(),
        delivery: None,
        stop_turn: false,
        content: text,
        is_error: false,
        note: None,
        display: None,
    }
}

fn error_result(text: String) -> ExecutableToolResult {
    ExecutableToolResult {
        is_error: true,
        ..ok_result(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latin_and_cjk_tokens_match_v2_tokenization() {
        assert_eq!(tokenize("Hello World 42"), vec!["hello", "world", "42"]);
        assert_eq!(tokenize("记忆库"), vec!["记", "忆", "库", "记忆", "忆库"]);
        assert_eq!(tokenize("搜索 rust"), vec!["rust", "搜", "索", "搜索"]);
    }

    #[test]
    fn fullwidth_latin_is_nfkc_folded_before_tokenizing() {
        assert_eq!(tokenize("ＲＵＳＴ"), vec!["rust"]);
    }

    #[test]
    fn ranking_is_tf_idf_with_a_score_floor_and_a_ten_entry_cap() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        fs::create_dir_all(base.join("global")).unwrap();
        fs::write(base.join("global/one.md"), "# One\n\nrust memory note\n").unwrap();
        fs::write(
            base.join("global/two.md"),
            "# Two\n\nrust rust rust reference\n",
        )
        .unwrap();
        fs::write(base.join("global/three.md"), "# Three\n\nunrelated\n").unwrap();

        let results = search(base, "rust", 10, None);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].path, "global/two.md");
        assert_eq!(results[1].path, "global/one.md");
        assert!(results[0].score > results[1].score);
        assert!(results[0].score >= SCORE_THRESHOLD);
    }

    #[test]
    fn scope_filter_keeps_only_matching_entries() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        fs::create_dir_all(base.join("global")).unwrap();
        fs::create_dir_all(base.join("projects/p1")).unwrap();
        fs::write(base.join("global/a.md"), "# A\n\nrust global\n").unwrap();
        fs::write(base.join("projects/p1/b.md"), "# B\n\nrust project\n").unwrap();

        let all = search(base, "rust", 10, None);
        assert_eq!(all.len(), 2);
        let global = search(base, "rust", 10, Some((MemoryScope::Global, "")));
        assert_eq!(global.len(), 1);
        assert_eq!(global[0].path, "global/a.md");
        let project = search(base, "rust", 10, Some((MemoryScope::Project, "p1")));
        assert_eq!(project.len(), 1);
        assert_eq!(project[0].path, "projects/p1/b.md");
    }

    #[test]
    fn a_query_without_hits_reports_no_entries() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        fs::create_dir_all(base.join("global")).unwrap();
        fs::write(base.join("global/a.md"), "# A\n\nrust\n").unwrap();
        assert!(search(base, "kubernetes", 10, None).is_empty());
    }

    #[test]
    fn the_search_action_renders_the_header_entries_and_scope() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("memory");
        fs::create_dir_all(base.join("global")).unwrap();
        fs::write(base.join("global/auth.md"), "# Auth\n\nrust jwt\n").unwrap();
        let context = MemoryContext::new(base, dir.path());

        let result = search_action(&context, &serde_json::json!({ "query": "rust" }), "");
        assert!(!result.is_error, "{}", result.content);
        assert!(
            result.content.contains("Found 1 memory entry"),
            "{}",
            result.content
        );
        assert!(result.content.contains("## Auth"), "{}", result.content);
        assert!(result.content.contains("  path: global/auth.md"));
        assert!(result.content.contains("  scope: global"));
        assert!(result.content.contains("  type: note"));
        assert!(result.content.contains("  score: "));
        assert!(result.content.contains("  snippet: "));
    }

    #[test]
    fn the_search_action_refuses_an_empty_query_and_reports_empty_results() {
        let dir = tempfile::tempdir().unwrap();
        let context = MemoryContext::new(dir.path().join("memory"), dir.path());

        let refused = search_action(&context, &serde_json::json!({}), "");
        assert!(refused.is_error);

        let empty = search_action(&context, &serde_json::json!({ "query": "rust" }), "");
        assert!(!empty.is_error, "{}", empty.content);
    }
}
