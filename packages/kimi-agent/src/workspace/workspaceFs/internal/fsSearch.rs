use std::cmp::Ordering;
use std::collections::HashSet;
use std::sync::LazyLock;

use base64::Engine;
use base64::engine::DecodePaddingMode;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use globset::{Glob, GlobSetBuilder};
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};

use super::super::fs::{FsGrepRequest, FsKind};

pub fn compute_fuzzy_score(name: &str, query_lower: &str) -> f64 {
    if query_lower.is_empty() {
        return 0.0;
    }
    let name_lower = name.to_lowercase();
    let mut name_idx = 0;
    let mut matched = 0;
    for ch in query_lower.chars() {
        let Some(found) = find_char_utf16(&name_lower, ch, name_idx) else {
            matched = -1;
            break;
        };
        matched += 1;
        name_idx = found + ch.len_utf16();
    }
    if matched <= 0 {
        return 0.0;
    }
    let mut score = matched as f64 / utf16_len(query_lower) as f64;
    if name_lower.starts_with(query_lower) {
        score = (score + 0.2).min(1.0);
    }
    score.clamp(0.0, 1.0)
}

pub fn compute_match_positions(path_str: &str, query_lower: &str) -> Vec<u64> {
    if query_lower.is_empty() {
        return Vec::new();
    }
    let lower = path_str.to_lowercase();
    let mut out = Vec::new();
    let mut pos = 0;
    for ch in query_lower.chars() {
        let Some(found) = find_char_utf16(&lower, ch, pos) else {
            return Vec::new();
        };
        out.push(found as u64);
        pos = found + ch.len_utf16();
    }
    out
}

pub fn matches_any_glob(rel: &str, globs: &[String]) -> bool {
    let valid = globs
        .iter()
        .filter(|glob| !glob.is_empty())
        .cloned()
        .collect::<Vec<_>>();
    !valid.is_empty() && compile_glob_set(&valid).is_match(rel)
}

pub fn glob_can_match_below(rel: &str, globs: &[String]) -> bool {
    let rel_segments = rel.split('/').collect::<Vec<_>>();
    for glob in globs {
        if glob.is_empty() {
            continue;
        }
        if !glob.contains('/') {
            if glob == "**" {
                return true;
            }
            continue;
        }
        if glob.starts_with('/') {
            return true;
        }
        let parts = glob.split('/').collect::<Vec<_>>();
        if glob_segments_match_prefix(&parts, &rel_segments) {
            return true;
        }
    }
    false
}

fn glob_segments_match_prefix(glob_segments: &[&str], rel_segments: &[&str]) -> bool {
    let g = glob_segments.len();
    let r = rel_segments.len();
    let width = r + 1;
    let mut dp = vec![0u8; (g + 1) * width];
    for gi in (0..=g).rev() {
        let head = glob_segments.get(gi).copied();
        let head_match = head
            .filter(|head| **head != "**" && !head.is_empty())
            .map(|head| Glob::new(head).unwrap().compile_matcher());
        for ri in (0..=r).rev() {
            let at = gi * width + ri;
            if ri == r {
                dp[at] = 1;
            } else if head == Some("**") {
                dp[at] = dp[(gi + 1) * width + ri] | dp[gi * width + ri + 1];
            } else if let Some(head_match) = &head_match {
                dp[at] = u8::from(head_match.is_match(rel_segments[ri]))
                    & dp[(gi + 1) * width + ri + 1];
            } else {
                dp[at] = 0;
            }
        }
    }
    dp[0] == 1
}

pub struct GrepPattern {
    regex: Regex,
}

pub struct GrepExec {
    pub index: usize,
}

pub fn compile_grep_pattern(req: &FsGrepRequest) -> Result<GrepPattern, regex::Error> {
    let body = if req.regex {
        req.pattern.clone()
    } else {
        regex::escape(&req.pattern)
    };
    RegexBuilder::new(&body)
        .case_insensitive(!req.case_sensitive)
        .build()
        .map(|regex| GrepPattern { regex })
}

impl GrepPattern {
    pub fn exec(&mut self, line: &str) -> Option<GrepExec> {
        let found = self.regex.find(line)?;
        Some(GrepExec {
            index: utf16_len(&line[..found.start()]),
        })
    }
}

pub fn strip_trailing_newline(value: &str) -> String {
    value
        .strip_suffix("\r\n")
        .or_else(|| value.strip_suffix('\n'))
        .unwrap_or(value)
        .to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RgPathField {
    pub text: Option<String>,
    pub bytes: Option<String>,
}

pub type RgLinesField = RgPathField;

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RgSubmatch {
    pub start: u64,
    pub end: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RgJsonData {
    pub path: Option<RgPathField>,
    pub lines: Option<RgLinesField>,
    pub line_number: Option<u64>,
    pub submatches: Option<Vec<RgSubmatch>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RgJsonRecord {
    #[serde(rename = "type")]
    pub type_: String,
    pub data: Option<RgJsonData>,
}

pub fn rg_path(path: Option<&RgPathField>) -> Option<String> {
    let path = path?;
    let raw = if let Some(text) = &path.text {
        text.clone()
    } else if let Some(bytes) = &path.bytes {
        String::from_utf8_lossy(&decode_lenient_base64(bytes)).into_owned()
    } else {
        return None;
    };
    Some(raw.strip_prefix("./").unwrap_or(&raw).to_string())
}

pub fn rg_text(lines: Option<&RgLinesField>) -> String {
    let Some(lines) = lines else {
        return String::new();
    };
    if let Some(text) = &lines.text {
        return text.clone();
    }
    if let Some(bytes) = &lines.bytes {
        return String::from_utf8_lossy(&decode_lenient_base64(bytes)).into_owned();
    }
    String::new()
}

pub static VCS_METADATA_DIRS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    HashSet::from([".git", ".jj", ".svn", ".hg", ".bzr"])
});

#[derive(Debug, Clone, PartialEq)]
pub struct SuggestQuery {
    pub name_query: String,
    pub path_segments: Vec<String>,
    pub show_hidden: bool,
    pub follow_gitignore: bool,
    pub include_globs: Option<Vec<String>>,
    pub exclude_globs: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SuggestCandidate {
    pub path: String,
    pub name: String,
    pub kind: FsKind,
    pub ignored: bool,
    pub tier: i32,
    pub depth: usize,
    pub span: usize,
    pub score: f64,
    pub positions: Vec<u64>,
}

struct SuggestMatch {
    tier: i32,
    span: usize,
    positions: Vec<u64>,
}

fn subsequence_positions(segment: &str, query: &str) -> Option<Vec<u64>> {
    let mut positions = Vec::new();
    let mut index = 0;
    for ch in query.chars() {
        let found = find_char_utf16(segment, ch, index)?;
        positions.push(found as u64);
        index = found + ch.len_utf16();
    }
    Some(positions)
}

fn match_suggest_name(name: &str, query_lower: &str) -> Option<SuggestMatch> {
    let name_lower = name.to_lowercase();
    let positions = subsequence_positions(&name_lower, query_lower)?;
    let tier = if name_lower == query_lower {
        3
    } else if name_lower.starts_with(query_lower) {
        2
    } else {
        1
    };
    let span = (positions.last().unwrap() - positions[0] + 1) as usize;
    Some(SuggestMatch {
        tier,
        span,
        positions,
    })
}

fn match_suggest_path(path: &str, query_segments: &[String]) -> Option<SuggestMatch> {
    let path_lower = path.to_lowercase();
    let path_segments = path_lower.split('/').collect::<Vec<_>>();
    let mut offsets = Vec::new();
    let mut offset = 0;
    for segment in &path_segments {
        offsets.push(offset);
        offset += utf16_len(segment) + 1;
    }
    let mut positions = Vec::new();
    let mut next_segment = 0;
    let mut last_segment = None;
    let mut last_segment_prefix = false;
    for query_segment in query_segments {
        let mut matched_segment = None;
        let mut matched_positions = None;
        for (index, path_segment) in path_segments.iter().enumerate().skip(next_segment) {
            if let Some(found) = subsequence_positions(path_segment, query_segment) {
                matched_segment = Some(index);
                matched_positions = Some(found);
                break;
            }
        }
        let Some(segment) = matched_segment else {
            return None;
        };
        for position in matched_positions.unwrap() {
            positions.push(position + offsets[segment] as u64);
        }
        last_segment_prefix = path_segments[segment].starts_with(query_segment);
        last_segment = Some(segment);
        next_segment = segment + 1;
    }
    let tier = if path_lower == query_segments.join("/") {
        3
    } else if last_segment == Some(path_segments.len() - 1) && last_segment_prefix {
        2
    } else {
        1
    };
    let span = (positions.last().unwrap() - positions[0] + 1) as usize;
    Some(SuggestMatch {
        tier,
        span,
        positions,
    })
}

pub fn evaluate_suggest_candidate(
    rel_path: &str,
    kind: FsKind,
    query: &SuggestQuery,
    ignored: bool,
) -> Option<SuggestCandidate> {
    let segments = rel_path.split('/').collect::<Vec<_>>();
    if segments.iter().any(|segment| VCS_METADATA_DIRS.contains(*segment)) {
        return None;
    }
    if !query.show_hidden && segments.iter().any(|segment| segment.starts_with('.')) {
        return None;
    }
    let name = segments.last().unwrap();
    let path_mode = !query.path_segments.is_empty();
    let match_ = if path_mode {
        match_suggest_path(rel_path, &query.path_segments)
    } else {
        match_suggest_name(name, &query.name_query)
    }?;
    if query
        .include_globs
        .as_ref()
        .is_some_and(|globs| !matches_any_glob(rel_path, globs))
        || query
            .exclude_globs
            .as_ref()
            .is_some_and(|globs| matches_any_glob(rel_path, globs))
    {
        return None;
    }
    let query_length = if path_mode {
        query
            .path_segments
            .iter()
            .map(|segment| utf16_len(segment))
            .sum::<usize>()
    } else {
        utf16_len(&query.name_query)
    };
    let raw = match_.tier as f64
        + 0.5 / segments.len() as f64
        + 0.25 * (query_length as f64 / utf16_len(name).max(1) as f64)
        + 0.25 * (query_length as f64 / match_.span.max(1) as f64);
    let score = (raw / 4.0).min(1.0);
    let base = utf16_len(rel_path) - utf16_len(name);
    let positions = if path_mode {
        match_.positions
    } else {
        match_
            .positions
            .into_iter()
            .map(|position| position + base as u64)
            .collect()
    };
    Some(SuggestCandidate {
        path: rel_path.to_string(),
        name: name.to_string(),
        kind,
        ignored,
        tier: match_.tier,
        depth: segments.len(),
        span: match_.span,
        score,
        positions,
    })
}

pub fn compare_suggest_candidates(a: &SuggestCandidate, b: &SuggestCandidate) -> Ordering {
    b.tier
        .cmp(&a.tier)
        .then_with(|| a.ignored.cmp(&b.ignored))
        .then_with(|| a.depth.cmp(&b.depth))
        .then_with(|| utf16_len(&a.name).cmp(&utf16_len(&b.name)))
        .then_with(|| a.span.cmp(&b.span))
        .then_with(|| a.path.encode_utf16().cmp(b.path.encode_utf16()))
}

pub struct SuggestTopHeap {
    heap: Vec<SuggestCandidate>,
    cap: usize,
}

impl SuggestTopHeap {
    pub fn new(cap: usize) -> Self {
        Self {
            heap: Vec::new(),
            cap,
        }
    }

    pub fn size(&self) -> usize {
        self.heap.len()
    }

    pub fn worst(&self) -> Option<&SuggestCandidate> {
        self.heap.first()
    }

    pub fn push(&mut self, candidate: SuggestCandidate) {
        if self.cap == 0 {
            return;
        }
        if self.heap.len() < self.cap {
            self.heap.push(candidate);
            self.sift_up(self.heap.len() - 1);
            return;
        }
        if compare_suggest_candidates(&self.heap[0], &candidate) != Ordering::Greater {
            return;
        }
        self.heap[0] = candidate;
        self.sift_down(0);
    }

    pub fn drain(self) -> Vec<SuggestCandidate> {
        let mut heap = self.heap;
        heap.sort_by(compare_suggest_candidates);
        heap
    }

    fn sift_up(&mut self, index: usize) {
        let mut index = index;
        while index > 0 {
            let parent = (index - 1) >> 1;
            if compare_suggest_candidates(&self.heap[parent], &self.heap[index])
                != Ordering::Less
            {
                break;
            }
            self.heap.swap(parent, index);
            index = parent;
        }
    }

    fn sift_down(&mut self, index: usize) {
        let mut index = index;
        loop {
            let left = index * 2 + 1;
            let right = left + 1;
            let mut worst = index;
            if left < self.heap.len()
                && compare_suggest_candidates(&self.heap[left], &self.heap[worst])
                    == Ordering::Greater
            {
                worst = left;
            }
            if right < self.heap.len()
                && compare_suggest_candidates(&self.heap[right], &self.heap[worst])
                    == Ordering::Greater
            {
                worst = right;
            }
            if worst == index {
                break;
            }
            self.heap.swap(worst, index);
            index = worst;
        }
    }
}

const LENIENT_BASE64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_decode_padding_mode(DecodePaddingMode::Indifferent)
        .with_decode_allow_trailing_bits(true),
);

fn decode_lenient_base64(payload: &str) -> Vec<u8> {
    let mut cleaned = Vec::with_capacity(payload.len());
    for unit in payload.encode_utf16() {
        let byte = unit as u8;
        match byte {
            b'=' => break,
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'+' | b'/' => cleaned.push(byte),
            b'-' => cleaned.push(b'+'),
            b'_' => cleaned.push(b'/'),
            _ => {}
        }
    }
    if cleaned.len() % 4 == 1 {
        cleaned.pop();
    }
    LENIENT_BASE64.decode(cleaned).unwrap_or_default()
}

fn compile_glob_set(globs: &[String]) -> globset::GlobSet {
    let mut builder = GlobSetBuilder::new();
    for glob in globs {
        builder.add(Glob::new(glob).unwrap());
    }
    builder.build().unwrap()
}

fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}

fn find_char_utf16(value: &str, needle: char, start: usize) -> Option<usize> {
    let mut units = 0;
    for ch in value.chars() {
        let next = units + ch.len_utf16();
        if units >= start && ch == needle {
            return Some(units);
        }
        units = next;
    }
    None
}
