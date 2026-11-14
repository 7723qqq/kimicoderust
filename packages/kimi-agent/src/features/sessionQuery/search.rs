use std::collections::HashSet;

use kimi_core::primitives::errors::Error2;

use super::cursor::SessionSearchCursor;
use super::errors::SESSION_QUERY_INVALID_FILTER;
use super::events::{
    SessionEventResultFilter, SessionEventSearchDocument, SessionEventSearchHit,
    SessionSearchPage,
};
use super::filters::{invalid_range, matches_range, validate_range};

pub const SNIPPET_RADIUS: usize = 40;
pub const DEFAULT_PAGE_LIMIT: usize = 10;
pub const MAX_PAGE_LIMIT: usize = 100;

pub fn filter_session_events(
    documents: &[SessionEventSearchDocument],
    filters: &[SessionEventResultFilter],
) -> Result<Vec<SessionEventSearchDocument>, Error2> {
    for filter in filters {
        match filter {
            SessionEventResultFilter::Seq { range } => validate_range("seq", range)?,
            SessionEventResultFilter::Time { range } => validate_range("time", range)?,
            SessionEventResultFilter::Text { text } => {
                compile_session_text_filter(text)?;
            }
            SessionEventResultFilter::Type { .. } => {}
        }
    }
    Ok(documents
        .iter()
        .filter(|document| {
            filters.iter().all(|filter| match filter {
                SessionEventResultFilter::Seq { range } => {
                    matches_range(document.record.seq as i64, range)
                }
                SessionEventResultFilter::Time { range } => {
                    matches_range(document.record.time, range)
                }
                SessionEventResultFilter::Type { values } => {
                    values.contains(&document.record.r#type)
                }
                SessionEventResultFilter::Text { text } => {
                    session_text_filter_matches(text, &document.text).unwrap_or(false)
                }
            })
        })
        .cloned()
        .collect())
}

pub fn search_event_documents(
    documents: &[SessionEventSearchDocument],
    query: &str,
    limit: Option<usize>,
    cursor: Option<SessionSearchCursor>,
) -> Result<SessionSearchPage<SessionEventSearchHit>, Error2> {
    let page_size = limit.unwrap_or(DEFAULT_PAGE_LIMIT).clamp(1, MAX_PAGE_LIMIT);
    let query_terms: Vec<String> = unique_tokens(query);
    if query_terms.is_empty() {
        return Err(Error2::new(
            SESSION_QUERY_INVALID_FILTER,
            "session search query must contain searchable text",
        ));
    }
    let mut ranked = Vec::new();
    for document in documents {
        if document.text.is_empty() {
            continue;
        }
        let terms = tokenize(&document.text);
        let score = query_terms.iter().fold(0usize, |sum, term| {
            sum + terms.iter().filter(|candidate| *candidate == term).count()
        });
        if score == 0 {
            continue;
        }
        ranked.push(SessionEventSearchHit {
            record: document.record.clone(),
            snippet: build_snippet(&document.text, &query_terms),
        });
    }
    ranked.sort_by(|left, right| right.record.time.cmp(&left.record.time));
    let offset = cursor.map(|cursor| cursor.offset).unwrap_or(0);
    let items = ranked
        .iter()
        .skip(offset)
        .take(page_size)
        .cloned()
        .collect();
    let next_cursor = (offset + page_size < ranked.len())
        .then(|| SessionSearchCursor::new(offset + page_size));
    Ok(SessionSearchPage { items, next_cursor })
}

pub fn compile_session_text_filter(text: &str) -> Result<regex::Regex, Error2> {
    let pattern = session_text_pattern(text)?;
    regex::RegexBuilder::new(&pattern)
        .case_insensitive(true)
        .build()
        .map_err(|error| invalid_range("text", &error.to_string()))
}

fn session_text_filter_matches(text: &str, value: &str) -> Result<bool, Error2> {
    let pattern = session_text_pattern(text)?;
    crate::tools::regex_js::is_match_with_flags(&pattern, value, "iu")
        .map_err(|error| invalid_range("text", &error))
}

fn session_text_pattern(text: &str) -> Result<String, Error2> {
    let trimmed = trim_js(text);
    if trimmed.is_empty() {
        return Err(Error2::new(
            SESSION_QUERY_INVALID_FILTER,
            "session text filter must contain non-whitespace text",
        ));
    }
    Ok(trimmed
        .split(is_js_whitespace)
        .filter(|part| !part.is_empty())
        .map(escape_js_regex)
        .collect::<Vec<_>>()
        .join("\\s+"))
}

fn escape_js_regex(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(
            character,
            '.' | '*' | '+' | '?' | '^' | '$' | '{' | '}' | '(' | ')' | '|' | '[' | ']' | '\\'
        ) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

fn tokenize(text: &str) -> Vec<String> {
    let normalized = icu_normalizer::ComposingNormalizer::new_nfkc()
        .normalize(text)
        .into_owned()
        .to_lowercase();
    let mut terms = Vec::new();
    let mut latin = String::new();
    for character in normalized.chars() {
        if character.is_ascii_lowercase() || character.is_ascii_digit() {
            latin.push(character);
        } else {
            flush_latin(&mut latin, &mut terms);
        }
    }
    flush_latin(&mut latin, &mut terms);
    let mut cjk = Vec::new();
    for character in normalized.chars() {
        if is_cjk(character) {
            cjk.push(character);
        } else {
            flush_cjk(&mut cjk, &mut terms);
        }
    }
    flush_cjk(&mut cjk, &mut terms);
    terms
}

fn flush_latin(latin: &mut String, terms: &mut Vec<String>) {
    if !latin.is_empty() && latin.len() <= 0xffff {
        terms.push(std::mem::take(latin));
    } else {
        latin.clear();
    }
}

fn is_cjk(character: char) -> bool {
    matches!(
        character,
        '\u{3400}'..='\u{9FFF}' | '\u{3040}'..='\u{30FF}' | '\u{FF00}'..='\u{FFEF}'
    )
}

fn flush_cjk(cjk: &mut Vec<char>, terms: &mut Vec<String>) {
    for index in 0..cjk.len() {
        terms.push(cjk[index].to_string());
        if index + 1 < cjk.len() {
            terms.push(cjk[index..index + 2].iter().collect());
        }
    }
    cjk.clear();
}

fn unique_tokens(text: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    tokenize(text)
        .into_iter()
        .filter(|term| seen.insert(term.clone()))
        .collect()
}

fn build_snippet(text: &str, terms: &[String]) -> String {
    let lower = text.to_lowercase();
    let lower_units: Vec<u16> = lower.encode_utf16().collect();
    let text_units: Vec<u16> = text.encode_utf16().collect();
    let first_match = terms
        .iter()
        .filter_map(|term| {
            let term_units: Vec<u16> = term.encode_utf16().collect();
            find_utf16(&lower_units, &term_units)
        })
        .min();
    let Some(first_match) = first_match else {
        return String::from_utf16_lossy(&text_units[..text_units.len().min(SNIPPET_RADIUS * 2)]);
    };
    let start = first_match.saturating_sub(SNIPPET_RADIUS);
    let end = (first_match + SNIPPET_RADIUS).min(text_units.len());
    let prefix = if start > 0 { "…" } else { "" };
    let suffix = if end < text_units.len() { "…" } else { "" };
    let excerpt = collapse_js_whitespace(&String::from_utf16_lossy(&text_units[start..end]));
    format!("{prefix}{excerpt}{suffix}")
}

fn find_utf16(text: &[u16], term: &[u16]) -> Option<usize> {
    if term.is_empty() {
        return Some(0);
    }
    text.windows(term.len()).position(|window| window == term)
}

fn collapse_js_whitespace(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut in_whitespace = false;
    for character in value.chars() {
        if is_js_whitespace(character) {
            if !in_whitespace {
                output.push(' ');
                in_whitespace = true;
            }
        } else {
            output.push(character);
            in_whitespace = false;
        }
    }
    output
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
