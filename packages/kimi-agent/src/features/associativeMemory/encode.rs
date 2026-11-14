use std::collections::HashMap;

use sha2::{Digest, Sha256};

use super::types::{Atom, AssociativeMemoryParams, NodeType};
use super::vector::tfidf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteSegment {
    pub text: String,
    pub start: usize,
    pub end: usize,
}

fn is_space(unit: u16) -> bool {
    char::from_u32(u32::from(unit)).is_some_and(is_js_whitespace)
}

fn trim_span(source: &str, start: usize, end: usize) -> Option<NoteSegment> {
    let units: Vec<u16> = source.encode_utf16().collect();
    let mut from = start;
    let mut to = end;
    while from < to && units.get(from).is_some_and(|unit| is_space(*unit)) {
        from += 1;
    }
    while to > from && units.get(to - 1).is_some_and(|unit| is_space(*unit)) {
        to -= 1;
    }
    if from >= to {
        return None;
    }
    Some(NoteSegment {
        text: String::from_utf16_lossy(&units[from..to]),
        start: from,
        end: to,
    })
}

fn is_sentence_boundary(unit: u16) -> bool {
    matches!(
        char::from_u32(u32::from(unit)),
        Some('.' | '!' | '?' | '。' | '！' | '？')
    )
}

fn split_sentences(slice: &[u16]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut cursor = 0;
    while cursor < slice.len() {
        while cursor < slice.len() && is_sentence_boundary(slice[cursor]) {
            cursor += 1;
        }
        if cursor == slice.len() {
            break;
        }
        let start = cursor;
        while cursor < slice.len() && !is_sentence_boundary(slice[cursor]) {
            cursor += 1;
        }
        while cursor < slice.len() && is_sentence_boundary(slice[cursor]) {
            cursor += 1;
        }
        out.push((start, cursor));
    }
    out
}

fn push_split_segments(
    source: &str,
    start: usize,
    end: usize,
    max_chars: usize,
    out: &mut Vec<NoteSegment>,
) {
    let length = end - start;
    if max_chars == 0 || length <= max_chars {
        if let Some(segment) = trim_span(source, start, end) {
            out.push(segment);
        }
        return;
    }
    let slice: Vec<u16> = source
        .encode_utf16()
        .skip(start)
        .take(length)
        .collect();
    let mut buffer_start: Option<usize> = None;
    let mut buffer_end = 0usize;
    for (sentence_start, sentence_end) in split_sentences(&slice) {
        let sentence_chars = sentence_end - sentence_start;
        if buffer_start.is_some_and(|from| {
            buffer_end - from + sentence_chars > max_chars
        }) {
            let from = buffer_start.unwrap();
            if let Some(segment) = trim_span(source, start + from, start + buffer_end) {
                out.push(segment);
            }
            buffer_start = None;
        }
        if sentence_chars > max_chars {
            if let Some(from) = buffer_start.take()
                && let Some(segment) = trim_span(source, start + from, start + buffer_end)
            {
                out.push(segment);
            }
            for cursor in (sentence_start..sentence_end).step_by(max_chars) {
                let stop = (cursor + max_chars).min(sentence_end);
                if let Some(segment) = trim_span(source, start + cursor, start + stop) {
                    out.push(segment);
                }
            }
            continue;
        }
        if buffer_start.is_none() {
            buffer_start = Some(sentence_start);
        }
        buffer_end = sentence_end;
    }
    if let Some(from) = buffer_start
        && let Some(segment) = trim_span(source, start + from, start + buffer_end)
    {
        out.push(segment);
    }
}

pub fn split_note_spans(text: &str, max_chars: usize) -> Vec<NoteSegment> {
    let mut out = Vec::new();
    let mut offset = 0usize;
    let mut group_start: Option<usize> = None;
    let mut group_end = 0usize;
    for line in text.split('\n') {
        let line_start = offset;
        let line_end = offset + line.encode_utf16().count();
        offset = line_end + 1;
        let trimmed = trim_js(line);
        if trimmed.is_empty() {
            if let Some(from) = group_start.take() {
                push_split_segments(text, from, group_end, max_chars, &mut out);
            }
            continue;
        }
        if trimmed.starts_with('#') {
            if let Some(from) = group_start.take() {
                push_split_segments(text, from, group_end, max_chars, &mut out);
            }
            group_start = Some(line_start);
            group_end = line_end;
            continue;
        }
        if group_start.is_none() {
            group_start = Some(line_start);
        }
        group_end = line_end;
    }
    if let Some(from) = group_start {
        push_split_segments(text, from, group_end, max_chars, &mut out);
    }
    out
}

pub fn split_note(text: &str, max_chars: usize) -> Vec<String> {
    split_note_spans(text, max_chars)
        .into_iter()
        .map(|segment| segment.text)
        .collect()
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

pub fn atom_id(scope_key: &str, note_id: &str, ordinal: usize) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!("{scope_key}\u{0}{note_id}\u{0}{ordinal}").as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()[..24]
        .to_string()
}

pub fn encode_atoms(
    note: &super::types::NoteRecord,
    df: &HashMap<String, f64>,
    total_docs: i64,
    now: i64,
    params: &AssociativeMemoryParams,
) -> Vec<Atom> {
    let segments = split_note_spans(&note.text, params.max_atom_chars.max(0) as usize);
    segments
        .into_iter()
        .enumerate()
        .map(|(index, segment)| {
            let terms = tokenize(&segment.text);
            let vector = to_json_map(tfidf(&terms, df, total_docs));
            Atom {
                id: atom_id(&note.scope_key, &note.id, index),
                note_id: note.id.clone(),
                scope_key: note.scope_key.clone(),
                node_type: NodeType::Atom,
                text: segment.text,
                span_start: segment.start as i64,
                span_end: segment.end as i64,
                terms,
                vector,
                dense_vector: None,
                concept_id: None,
                base_weight: 1.0,
                confidence: 0.5,
                replay_priority: 0,
                pinned: false,
                suppressed: false,
                valid_from: now,
                valid_until: None,
                is_deleted: false,
                created_at: now,
                last_activated_at: now,
            }
        })
        .collect()
}

fn to_json_map(vector: HashMap<String, f64>) -> serde_json::Map<String, serde_json::Value> {
    vector
        .into_iter()
        .map(|(term, value)| (term, serde_json::json!(value)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::associativeMemory::terms;
    use crate::features::associativeMemory::types::{AssociativeMemoryParams, MemorySource};

    fn params(max_atom_chars: i64) -> AssociativeMemoryParams {
        AssociativeMemoryParams {
            max_atom_chars,
            context_window_ms: 0,
            base_learning_rate: 0.0,
            out_weight_budget: 0.0,
            max_edge_weight: 0.0,
            spread_hops: 0,
            spread_decay: 0.0,
            inhibition_factor: 0.0,
            base_k: 0.0,
            k_decay: 0.0,
            compute_budget: 0,
            seed_top_m: 0,
            episode_top_n: 0,
            list_limit: 0,
            top_k: 0,
            activation_threshold: 0.0,
            vector_threshold: 0.0,
            cluster_cooc_threshold: 0.0,
            cluster_min_size: 0,
            cluster_max_size: 0,
            procedure_min_steps: 0,
            procedure_threshold: 0.0,
            load_ref: 0.0,
            concept_formation_rate: 0.0,
            power_law_days: 0.0,
            half_life_ms: 0,
            prune_threshold: 0.0,
            injection_max_tokens: 0,
            supersede_threshold: 0.0,
            max_out_degree: 0,
            hetero_factor: 0.0,
            weight_dependence_k: 0.0,
            stdp_a_plus: 0.0,
            stdp_a_minus: 0.0,
            stdp_tau: 0.0,
            bcm_gain: 0.0,
            cascade_max_level: 0,
            cascade_strength: 0.0,
            sleep_downscale: 0.0,
            capture_count: 0,
            reconsolidate_merge_threshold: 0.0,
            reconsolidate_novel_threshold: 0.0,
            interference_forgetting: 0.0,
            replay_strata: 0,
            motivated_forgetting: 0.0,
            motivated_forgetting_ms: 0,
            schema_min_hops: 0,
            schema_top_k: 0,
            schema_ttl_ms: 0,
            interference_max_edges: 0,
        }
    }

    #[test]
    fn atoms_are_split_on_lines_with_headings_then_paragraphs() {
        let text = "# Title\nfirst line\n\nsecond line\n";
        let segments = split_note_spans(text, 4096);
        assert_eq!(
            segments
                .iter()
                .map(|segment| segment.text.as_str())
                .collect::<Vec<_>>(),
            vec!["# Title\nfirst line", "second line"],
            "v2 closes the group before a heading, then the heading starts a new group \
             that the following paragraph joins"
        );
        let last = segments.last().unwrap();
        assert_eq!(&text[last.start..last.end], "second line");
    }

    #[test]
    fn long_groups_split_on_sentence_boundaries() {
        let text = "One sentence. Two sentence! Three sentence?";
        let segments = split_note(text, 20);
        assert_eq!(
            segments,
            vec![
                "One sentence.".to_string(),
                "Two sentence!".to_string(),
                "Three sentence?".to_string()
            ]
        );
    }

    #[test]
    fn a_sentence_longer_than_the_cap_is_chunked() {
        let segments = split_note("abcdefghijklmnopqrstuvwxyz", 5);
        assert_eq!(
            segments,
            vec![
                "abcde".to_string(),
                "fghij".to_string(),
                "klmno".to_string(),
                "pqrst".to_string(),
                "uvwxy".to_string(),
                "z".to_string()
            ]
        );
        assert!(split_note("", 5).is_empty());
        assert!(split_note("   \n\n  ", 5).is_empty());
    }

    #[test]
    fn a_group_longer_than_the_cap_is_chunked_by_sentences() {
        let segments = split_note("One sentence. Two sentence! Three sentence?", 12);
        assert_eq!(
            segments,
            vec![
                "One sentence".to_string(),
                ".".to_string(),
                "Two sentenc".to_string(),
                "e!".to_string(),
                "Three sente".to_string(),
                "nce?".to_string()
            ]
        );
    }

    #[test]
    fn atom_ids_are_stable_sha256_prefixes() {
        assert_eq!(
            atom_id("scope", "note-1", 0),
            atom_id("scope", "note-1", 0)
        );
        assert_eq!(atom_id("scope", "note-1", 0).len(), 24);
        assert_ne!(atom_id("scope", "note-1", 0), atom_id("scope", "note-1", 1));
        assert_ne!(atom_id("scope", "note-1", 0), atom_id("scope-b", "note-1", 0));
    }

    #[test]
    fn encoding_uses_the_engine_tokenizer_and_tfidf() {
        let note = super::super::types::NoteRecord {
            id: "note-1".into(),
            scope_key: "scope".into(),
            text: "Rust rust 记忆".into(),
            created_at: 1,
            source: MemorySource::User,
        };
        let atoms = encode_atoms(&note, &HashMap::new(), 1, 7, &params(4096));
        assert_eq!(atoms.len(), 1);
        assert_eq!(atoms[0].terms, vec!["rust", "rust", "记", "记忆", "忆", "记忆"]);
        assert_eq!(atoms[0].valid_from, 7);
        assert_eq!(atoms[0].created_at, 7);
        assert_eq!(atoms[0].last_activated_at, 7);
        assert_eq!(atoms[0].span_start, 0);
        assert_eq!(atoms[0].span_end, note.text.encode_utf16().count() as i64);
        let expected_rust = (2.0 / 5.0) * ((2.0_f64).ln() + 1.0);
        assert_eq!(
            atoms[0].vector.get("rust"),
            Some(&serde_json::json!(expected_rust))
        );
        assert!(terms::is_stop_term("the"));
    }
}
