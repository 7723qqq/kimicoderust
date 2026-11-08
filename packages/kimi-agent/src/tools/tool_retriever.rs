//! BM25 ranking behind `select_tools`' capability search.
//!
//! Mirrors `packages/agent-core-v2/src/agent/toolSelect/toolRetriever.ts`
//! (`rankToolsByQuery`): the same `k1`/`b` constants, stopword list, token
//! caps and score-ratio cutoff, so the same query returns the same candidates
//! v2 would.
//!
//! One deliberate difference: v2 normalizes text with Unicode NFKC before
//! tokenizing, and the engine carries no normalization crate. Every character
//! class the tokenizer keeps (`\p{L}`, `\p{N}`) survives the absence, so only
//! a query that mixes widths (full-width Latin against half-width text) can
//! miss a match v2 would make.

use std::collections::HashMap;
use std::collections::HashSet;

const K1: f64 = 1.2;
const B: f64 = 0.75;
const MAX_QUERY_TOKENS: usize = 256;
const MAX_DOCUMENT_TOKENS: usize = 512;
const MAX_PARAMETER_NAMES: usize = 32;
const MIN_SCORE_RATIO: f64 = 0.15;

/// v2 `STOPWORDS`: words too common to say anything about a tool.
const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "can", "for", "from", "in", "is", "it", "its",
    "of", "on", "or", "that", "the", "these", "this", "those", "to", "tool", "tools", "use",
    "using", "with", "you", "your",
];

/// One searchable tool: the fields v2's `documentTokens` indexes.
#[derive(Debug, Clone, Default)]
pub struct ToolSearchEntry {
    pub name: String,
    pub description: String,
    pub source: String,
    pub parameter_names: Vec<String>,
}

/// One ranked candidate (v2 `ToolSearchHit`).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSearchHit {
    pub name: String,
    pub description: String,
    pub score: f64,
}

/// Rank `entries` against `query` (v2 `rankToolsByQuery`).
///
/// A blank or stopword-only query returns the catalogue's own order — the
/// "list what is loadable" case — and a real query drops anything scoring
/// below [`MIN_SCORE_RATIO`] of the best hit, which is what keeps a loose
/// keyword match from burying the obvious one.
pub fn rank_tools_by_query(
    entries: &[ToolSearchEntry],
    query: &str,
    limit: usize,
) -> Vec<ToolSearchHit> {
    let safe_limit = limit.clamp(1, 50);
    if entries.is_empty() {
        return Vec::new();
    }
    let first_n = |count: usize| -> Vec<ToolSearchHit> {
        entries
            .iter()
            .take(count)
            .map(|entry| ToolSearchHit {
                name: entry.name.clone(),
                description: entry.description.clone(),
                score: 0.0,
            })
            .collect()
    };
    if query.trim().is_empty() {
        return first_n(safe_limit);
    }
    let query_tokens: Vec<String> = tokenize(query)
        .into_iter()
        .filter(|term| !is_stopword(term))
        .take(MAX_QUERY_TOKENS)
        .collect();
    if query_tokens.is_empty() {
        return first_n(safe_limit);
    }

    let documents: Vec<Vec<String>> = entries
        .iter()
        .map(|entry| {
            document_tokens(entry)
                .into_iter()
                .take(MAX_DOCUMENT_TOKENS)
                .collect()
        })
        .collect();

    let mut document_frequency: HashMap<&str, usize> = HashMap::new();
    for tokens in &documents {
        let mut seen: HashSet<&str> = HashSet::new();
        for term in tokens {
            if seen.insert(term.as_str()) {
                *document_frequency.entry(term.as_str()).or_default() += 1;
            }
        }
    }
    let document_count = documents.len() as f64;
    let average_length =
        documents.iter().map(Vec::len).sum::<usize>() as f64 / document_count.max(1.0);

    let mut query_frequency: HashMap<&str, f64> = HashMap::new();
    for term in &query_tokens {
        *query_frequency.entry(term.as_str()).or_default() += 1.0;
    }

    let mut hits: Vec<ToolSearchHit> = Vec::new();
    for (index, tokens) in documents.iter().enumerate() {
        let mut term_frequency: HashMap<&str, f64> = HashMap::new();
        for term in tokens {
            *term_frequency.entry(term.as_str()).or_default() += 1.0;
        }
        let mut score = 0.0;
        for (term, query_count) in &query_frequency {
            let frequency = term_frequency.get(term).copied().unwrap_or(0.0);
            if frequency == 0.0 {
                continue;
            }
            let containing = document_frequency.get(term).copied().unwrap_or(0) as f64;
            let idf = (1.0 + (document_count - containing + 0.5) / (containing + 0.5)).ln();
            let denominator =
                frequency + K1 * (1.0 - B + B * tokens.len() as f64 / average_length.max(1.0));
            score += (idf * (frequency * (K1 + 1.0))) / denominator * query_count;
        }
        if score > 0.0 {
            let entry = &entries[index];
            hits.push(ToolSearchHit {
                name: entry.name.clone(),
                description: entry.description.clone(),
                score,
            });
        }
    }

    hits.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.name.cmp(&right.name))
    });
    let Some(best) = hits.first().map(|hit| hit.score) else {
        return Vec::new();
    };
    hits.into_iter()
        .filter(|hit| hit.score >= best * MIN_SCORE_RATIO)
        .take(safe_limit)
        .collect()
}

/// v2 `documentTokens`: the name (twice, as a title weight), the description,
/// the source and each parameter name, all tokenized and stopword-filtered.
fn document_tokens(entry: &ToolSearchEntry) -> Vec<String> {
    let mut tokens = Vec::new();
    for part in [
        entry.name.as_str(),
        entry.name.as_str(),
        entry.description.as_str(),
        entry.source.as_str(),
    ] {
        tokens.extend(tokenize(part));
    }
    for parameter in entry.parameter_names.iter().take(MAX_PARAMETER_NAMES) {
        tokens.extend(tokenize(parameter));
    }
    tokens
        .into_iter()
        .filter(|term| !is_stopword(term))
        .collect()
}

/// v2 `tokenize`: lowercase word tokens, plus every two-character window of a
/// token that contains a CJK ideograph (the unigram/bigram pair Chinese
/// retrieval needs).
fn tokenize(text: &str) -> Vec<String> {
    let lowered: String = text.chars().flat_map(char::to_lowercase).collect();
    let mut tokens = Vec::new();
    let mut word = String::new();
    for ch in lowered.chars() {
        if ch.is_alphanumeric() {
            word.push(ch);
        } else {
            push_word(&mut word, &mut tokens);
        }
    }
    push_word(&mut word, &mut tokens);
    tokens
}

fn push_word(word: &mut String, tokens: &mut Vec<String>) {
    if word.is_empty() {
        return;
    }
    tokens.push(word.clone());
    if word.chars().any(is_cjk_ideograph) {
        let chars: Vec<char> = word.chars().collect();
        for index in 0..chars.len().saturating_sub(1) {
            tokens.push(chars[index..index + 2].iter().collect());
        }
    }
    word.clear();
}

/// v2's `[\u3400-\u9FFF]` test: CJK Unified Ideographs plus Extension A.
fn is_cjk_ideograph(ch: char) -> bool {
    ('\u{3400}'..='\u{9FFF}').contains(&ch)
}

fn is_stopword(term: &str) -> bool {
    STOPWORDS.contains(&term)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, description: &str, parameters: &[&str]) -> ToolSearchEntry {
        ToolSearchEntry {
            name: name.into(),
            description: description.into(),
            source: "mcp".into(),
            parameter_names: parameters.iter().map(|name| name.to_string()).collect(),
        }
    }

    fn catalogue() -> Vec<ToolSearchEntry> {
        vec![
            entry(
                "mcp__github__search_issues",
                "Search issues and pull requests in a repository",
                &["query", "repo"],
            ),
            entry(
                "mcp__github__create_pr",
                "Create a pull request from a branch",
                &["title", "head", "base"],
            ),
            entry("mcp__fs__read_file", "Read a file from disk", &["path"]),
        ]
    }

    #[test]
    fn a_named_capability_ranks_its_tool_first() {
        let hits = rank_tools_by_query(&catalogue(), "create pull request", 10);
        assert_eq!(
            hits.first().map(|hit| hit.name.as_str()),
            Some("mcp__github__create_pr"),
            "the pull-request tool must outrank the others: {hits:?}"
        );
    }

    #[test]
    fn a_parameter_name_is_searchable() {
        let hits = rank_tools_by_query(&catalogue(), "repository issues", 10);
        assert_eq!(
            hits.first().map(|hit| hit.name.as_str()),
            Some("mcp__github__search_issues"),
            "{hits:?}"
        );
    }

    #[test]
    fn an_empty_or_stopword_query_lists_the_catalogue_in_order() {
        let all = catalogue();
        let blank = rank_tools_by_query(&all, "   ", 2);
        assert_eq!(blank.len(), 2);
        assert_eq!(blank[0].name, all[0].name);
        assert_eq!(blank[1].name, all[1].name);

        let stopwords = rank_tools_by_query(&all, "the tools you use", 2);
        assert_eq!(stopwords.len(), 2);
        assert_eq!(stopwords[0].name, all[0].name);
    }

    #[test]
    fn an_unmatched_query_returns_nothing() {
        assert!(rank_tools_by_query(&catalogue(), "quantum entanglement", 10).is_empty());
    }

    #[test]
    fn cjk_queries_match_through_bigrams() {
        let tools = vec![entry(
            "mcp__internal__code_review",
            "执行代码审查并给出建议",
            &["diff"],
        )];
        let hits = rank_tools_by_query(&tools, "审查", 10);
        assert_eq!(hits.len(), 1, "the bigram 审查 must match: {hits:?}");
        assert!(hits[0].score > 0.0);
    }

    #[test]
    fn the_limit_is_clamped_to_a_useful_range() {
        let all = catalogue();
        assert_eq!(rank_tools_by_query(&all, "file", 0).len(), 1);
        assert!(rank_tools_by_query(&all, "", 500).len() <= 50);
    }

    #[test]
    fn a_weak_match_is_dropped_below_the_score_ratio() {
        // One document carries a rare term; nine share only a common one. The
        // rare term's IDF dominates, and the nine common-only documents fall
        // under `MIN_SCORE_RATIO` of the best score — the cutoff that keeps a
        // loose keyword match from burying the obvious hit.
        let mut tools: Vec<ToolSearchEntry> = (0..9)
            .map(|index| entry(&format!("mcp__common__tool{index}"), "common helper", &[]))
            .collect();
        tools.push(entry("mcp__rare__handler", "rareword handler", &[]));

        let hits = rank_tools_by_query(&tools, "common rareword", 10);
        assert_eq!(
            hits.iter().map(|hit| hit.name.as_str()).collect::<Vec<_>>(),
            vec!["mcp__rare__handler"],
            "only the rare-term document clears the ratio: {hits:?}"
        );
    }
}
