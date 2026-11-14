use std::collections::HashMap;
use std::sync::LazyLock;

use indexmap::IndexSet;
use regex::Regex;
use serde_json::{Map, Value};

use super::config_pure::{deep_equal, is_plain_object};

#[derive(Debug, Clone)]
pub struct DomainUpdate {
    pub snake_key: String,
    pub previous_value: Option<Value>,
    pub next_value: Option<Value>,
}

#[derive(Debug, Clone)]
enum LineEdit {
    Replace {
        start_line: usize,
        end_line: usize,
        text: String,
    },
    Insert {
        after_line: usize,
        text: String,
    },
}

#[derive(Debug, Clone)]
struct RootRegion {
    root_key: String,
    start: usize,
    end: usize,
    dotted: bool,
}

#[derive(Debug, Clone)]
enum RootSegment {
    Trivia { start: usize, end: usize },
    Region(RootRegion),
}

#[derive(Debug, Clone)]
struct DomainStatement {
    key: String,
    start_line: usize,
    end_line: usize,
    indent: String,
    separator: String,
    value_start: usize,
    value_end: usize,
}

#[derive(Debug, Clone)]
struct DomainBlock {
    path: Vec<String>,
    has_header: bool,
    is_array: bool,
    start_line: usize,
    end_line: usize,
    statements: Vec<DomainStatement>,
}

#[derive(Debug, Clone)]
struct DomainScan {
    blocks: Vec<DomainBlock>,
    ambiguous: bool,
}

#[derive(Debug, Clone)]
struct KeyValueMatch {
    indent: String,
    key_segments: Vec<String>,
    dotted: bool,
    separator: String,
    value_start: usize,
}

#[derive(Debug, Clone)]
struct HeaderMatch {
    root_key: String,
    path: Vec<String>,
    is_array: bool,
}

static KEY_VALUE_LINE_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(\s*)([A-Za-z0-9_-]+(?:\.[A-Za-z0-9_-]+)*)(\s*=\s*)([\s\S]*)$").unwrap()
});

fn split_lines_keep_ends(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut start = 0;
    for (index, character) in text.char_indices() {
        if character == '\n' {
            lines.push(text[start..=index].to_string());
            start = index + 1;
        }
    }
    if start < text.len() {
        lines.push(text[start..].to_string());
    }
    lines
}

fn strip_line_ending(line: &str) -> &str {
    if let Some(line) = line.strip_suffix("\r\n") {
        line
    } else if let Some(line) = line.strip_suffix('\n') {
        line
    } else {
        line
    }
}

fn detect_eol(text: &str) -> &'static str {
    match text.find('\n') {
        Some(index) if index > 0 && text.as_bytes()[index - 1] == b'\r' => "\r\n",
        _ => "\n",
    }
}

fn is_trivia_body(body: &str) -> bool {
    let trimmed = body.trim();
    trimmed.is_empty() || trimmed.starts_with('#')
}

fn line_index_at(offsets: &[usize], position: usize) -> usize {
    if offsets.is_empty() {
        return 0;
    }
    let mut low = 0isize;
    let mut high = offsets.len() as isize - 1;
    let mut result = 0;
    while low <= high {
        let mid = (low + high) >> 1;
        if offsets[mid as usize] <= position {
            result = mid as usize;
            low = mid + 1;
        } else {
            high = mid - 1;
        }
    }
    result
}

fn line_start_offsets(lines: &[String]) -> Vec<usize> {
    let mut offsets = Vec::with_capacity(lines.len());
    let mut offset = 0;
    for line in lines {
        offsets.push(offset);
        offset += line.len();
    }
    offsets
}

fn scan_string_end(text: &str, offset: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let quote = *bytes.get(offset)?;
    if bytes.get(offset..offset + 3).is_some_and(|prefix| prefix == [quote, quote, quote].as_slice()) {
        let mut index = offset + 3;
        while index < bytes.len() {
            if quote == b'"' && bytes[index] == b'\\' {
                index += 2;
                continue;
            }
            if bytes[index] == quote {
                let mut run = 0;
                while index + run < bytes.len() && bytes[index + run] == quote {
                    run += 1;
                }
                if run >= 3 {
                    return Some(index + run);
                }
                index += run;
                continue;
            }
            index += 1;
        }
        return None;
    }
    let mut index = offset + 1;
    while index < bytes.len() {
        if bytes[index] == b'\n' {
            return None;
        }
        if quote == b'"' && bytes[index] == b'\\' {
            index += 2;
            continue;
        }
        if bytes[index] == quote {
            return Some(index + 1);
        }
        index += 1;
    }
    None
}

fn scan_balanced(text: &str, offset: usize, open: u8, close: u8) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut index = offset;
    while index < bytes.len() {
        let character = bytes[index];
        if character == b'"' || character == b'\'' {
            index = scan_string_end(text, index)?;
            continue;
        }
        if character == open {
            depth += 1;
        } else if character == close {
            depth = depth.checked_sub(1)?;
            if depth == 0 {
                return Some(index + 1);
            }
        } else if character == b'\n' && open == b'{' {
            return None;
        }
        index += 1;
    }
    None
}

fn scan_value_end(text: &str, offset: usize) -> Option<usize> {
    let first = *text.as_bytes().get(offset)?;
    if first == b'"' || first == b'\'' {
        return scan_string_end(text, offset);
    }
    if first == b'[' {
        return scan_balanced(text, offset, b'[', b']');
    }
    if first == b'{' {
        return scan_balanced(text, offset, b'{', b'}');
    }
    let bytes = text.as_bytes();
    let mut index = offset;
    while index < bytes.len() {
        if matches!(bytes[index], b' ' | b'\t' | b'\r' | b'\n' | b'#') {
            break;
        }
        index += 1;
    }
    (index != offset).then_some(index)
}

fn decode_basic_escape(body: &str, offset: usize) -> Option<(char, usize)> {
    let code = *body.as_bytes().get(offset + 1)?;
    let simple = match code {
        b'b' => Some('\u{8}'),
        b't' => Some('\t'),
        b'n' => Some('\n'),
        b'f' => Some('\u{c}'),
        b'r' => Some('\r'),
        b'"' => Some('"'),
        b'\\' => Some('\\'),
        b'u' => return decode_unicode_escape(body, offset, 4),
        b'U' => return decode_unicode_escape(body, offset, 8),
        _ => None,
    };
    simple.map(|character| (character, offset + 2))
}

fn decode_unicode_escape(body: &str, offset: usize, digits: usize) -> Option<(char, usize)> {
    let end = offset + 2 + digits;
    let hex = body.get(offset + 2..end)?;
    if hex.len() != digits || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let code_point = u32::from_str_radix(hex, 16).ok()?;
    if code_point > 0x10ffff || (0xd800..=0xdfff).contains(&code_point) {
        return None;
    }
    Some((char::from_u32(code_point)?, end))
}

fn skip_inline_whitespace(body: &str, mut offset: usize) -> usize {
    let bytes = body.as_bytes();
    while offset < bytes.len() && matches!(bytes[offset], b' ' | b'\t') {
        offset += 1;
    }
    offset
}

struct HeaderSegment {
    value: String,
    end: usize,
}

fn scan_basic_header_segment(body: &str, offset: usize) -> Option<HeaderSegment> {
    let bytes = body.as_bytes();
    let mut index = offset + 1;
    let mut value = String::new();
    while index < bytes.len() {
        match bytes[index] {
            b'"' => {
                if value.is_empty() {
                    return None;
                }
                return Some(HeaderSegment { value, end: index + 1 });
            }
            b'\\' => {
                let (character, end) = decode_basic_escape(body, index)?;
                value.push(character);
                index = end;
            }
            _ => {
                let character = body[index..].chars().next()?;
                value.push(character);
                index += character.len_utf8();
            }
        }
    }
    None
}

fn scan_literal_header_segment(body: &str, offset: usize) -> Option<HeaderSegment> {
    let close = body[offset + 1..].find('\'')? + offset + 1;
    let value = body[offset + 1..close].to_string();
    if value.is_empty() {
        return None;
    }
    Some(HeaderSegment { value, end: close + 1 })
}

fn scan_header_segment(body: &str, offset: usize) -> Option<HeaderSegment> {
    let start = skip_inline_whitespace(body, offset);
    match *body.as_bytes().get(start)? {
        b'"' => scan_basic_header_segment(body, start),
        b'\'' => scan_literal_header_segment(body, start),
        _ => {
            let bytes = body.as_bytes();
            let mut index = start;
            while index < bytes.len()
                && matches!(bytes[index], b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-')
            {
                index += 1;
            }
            if index == start {
                return None;
            }
            Some(HeaderSegment {
                value: body[start..index].to_string(),
                end: index,
            })
        }
    }
}

fn match_header(body: &str) -> Option<HeaderMatch> {
    let start = skip_inline_whitespace(body, 0);
    let bytes = body.as_bytes();
    let mut is_array = false;
    let mut index;
    if body.get(start..start + 2)? == "[[" {
        is_array = true;
        index = start + 2;
    } else if bytes.get(start) == Some(&b'[') {
        index = start + 1;
    } else {
        return None;
    }
    let mut path = Vec::new();
    loop {
        let segment = scan_header_segment(body, index)?;
        path.push(segment.value);
        index = skip_inline_whitespace(body, segment.end);
        match bytes.get(index) {
            Some(b']') => {
                index += 1;
                break;
            }
            Some(b'.') => {
                index = skip_inline_whitespace(body, index + 1);
            }
            _ => return None,
        }
    }
    if is_array {
        if bytes.get(index) != Some(&b']') {
            return None;
        }
        index += 1;
    }
    let rest = skip_inline_whitespace(body, index);
    if rest < body.len() && bytes[rest] != b'#' {
        return None;
    }
    Some(HeaderMatch {
        root_key: path.first()?.clone(),
        path,
        is_array,
    })
}

fn match_key_value(body: &str) -> Option<KeyValueMatch> {
    let captures = KEY_VALUE_LINE_PATTERN.captures(body)?;
    let key_segments = captures.get(2)?.as_str().split('.').map(str::to_string).collect::<Vec<_>>();
    Some(KeyValueMatch {
        indent: captures.get(1)?.as_str().to_string(),
        dotted: key_segments.len() > 1,
        key_segments,
        separator: captures.get(3)?.as_str().to_string(),
        value_start: captures.get(4)?.start(),
    })
}

struct ScannedDocument {
    lines: Vec<String>,
    offsets: Vec<usize>,
    eol: &'static str,
    segments: Vec<RootSegment>,
}

fn scan_root_regions(text: &str) -> Option<ScannedDocument> {
    let lines = split_lines_keep_ends(text);
    let offsets = line_start_offsets(&lines);
    let eol = detect_eol(text);
    let mut segments = Vec::new();
    let mut region: Option<RootRegion> = None;
    let mut trivia_start: Option<usize> = None;
    let mut index = 0;
    while index < lines.len() {
        let body = strip_line_ending(&lines[index]);
        if is_trivia_body(body) {
            trivia_start.get_or_insert(index);
            index += 1;
            continue;
        }
        if let Some(start) = trivia_start.take() {
            segments.push(RootSegment::Trivia { start, end: index - 1 });
        }
        if let Some(header) = match_header(body) {
            if region.as_ref().is_none_or(|region| region.root_key != header.root_key) {
                if let Some(region) = region.take() {
                    segments.push(RootSegment::Region(region));
                }
                region = Some(RootRegion {
                    root_key: header.root_key,
                    start: index,
                    end: index,
                    dotted: false,
                });
            } else if let Some(region) = &mut region {
                region.end = index;
            }
            index += 1;
            continue;
        }
        let key_value = match_key_value(body)?;
        let value_start = offsets[index] + key_value.value_start;
        let value_end = scan_value_end(text, value_start)?;
        let end_line = line_index_at(&offsets, value_end.checked_sub(1)?);
        let root_key = region
            .as_ref()
            .map(|region| region.root_key.clone())
            .unwrap_or_else(|| key_value.key_segments[0].clone());
        if region.as_ref().is_none_or(|region| region.root_key != root_key) {
            if let Some(region) = region.take() {
                segments.push(RootSegment::Region(region));
            }
            region = Some(RootRegion {
                root_key,
                start: index,
                end: end_line,
                dotted: key_value.dotted,
            });
        } else if let Some(region) = &mut region {
            region.end = end_line;
            region.dotted |= key_value.dotted;
        }
        index = end_line + 1;
    }
    if let Some(start) = trivia_start {
        segments.push(RootSegment::Trivia { start, end: lines.len().checked_sub(1)? });
    }
    if let Some(region) = region {
        segments.push(RootSegment::Region(region));
    }
    Some(ScannedDocument { lines, offsets, eol, segments })
}

fn scan_domain_region(
    text: &str,
    lines: &[String],
    offsets: &[usize],
    region: &RootRegion,
    snake_key: &str,
) -> Option<DomainScan> {
    let mut blocks = Vec::new();
    let mut ambiguous = false;
    let mut current: Option<DomainBlock> = None;
    let mut index = region.start;
    while index <= region.end {
        let body = strip_line_ending(lines.get(index)?);
        if is_trivia_body(body) {
            index += 1;
            continue;
        }
        if let Some(header) = match_header(body) {
            if header.root_key != snake_key {
                return None;
            }
            if let Some(current) = current.take() {
                blocks.push(current);
            }
            ambiguous |= header.is_array;
            current = Some(DomainBlock {
                path: header.path.into_iter().skip(1).collect(),
                has_header: true,
                is_array: header.is_array,
                start_line: index,
                end_line: index,
                statements: Vec::new(),
            });
            index += 1;
            continue;
        }
        let key_value = match_key_value(body)?;
        ambiguous |= key_value.dotted;
        let value_start = offsets[index] + key_value.value_start;
        let value_end = scan_value_end(text, value_start)?;
        let end_line = line_index_at(offsets, value_end.checked_sub(1)?);
        let statement = DomainStatement {
            key: key_value.key_segments.last()?.clone(),
            start_line: index,
            end_line,
            indent: key_value.indent,
            separator: key_value.separator,
            value_start,
            value_end,
        };
        let block = current.get_or_insert_with(|| DomainBlock {
            path: Vec::new(),
            has_header: false,
            is_array: false,
            start_line: index,
            end_line,
            statements: Vec::new(),
        });
        block.statements.push(statement);
        block.end_line = end_line;
        index = end_line + 1;
    }
    if let Some(current) = current {
        blocks.push(current);
    }
    Some(DomainScan { blocks, ambiguous })
}

fn paths_equal(a: &[String], b: &[String]) -> bool {
    a == b
}

fn blocks_nested_under<'a>(blocks: &'a [DomainBlock], path: &[String]) -> Vec<&'a DomainBlock> {
    blocks
        .iter()
        .filter(|block| block.path.len() >= path.len() && paths_equal(&block.path[..path.len()], path))
        .collect()
}

fn remove_lines(start_line: usize, end_line: usize) -> LineEdit {
    LineEdit::Replace { start_line, end_line, text: String::new() }
}

fn insert_merged(edits: &mut Vec<LineEdit>, after_line: usize, text: String) {
    if let Some(index) = edits.iter().position(|edit| {
        matches!(edit, LineEdit::Insert { after_line: existing, .. } if *existing == after_line)
    }) {
        if let LineEdit::Insert { text: existing, .. } = &mut edits[index] {
            existing.push_str(&text);
            return;
        }
    }
    edits.push(LineEdit::Insert { after_line, text });
}

fn statement_suffix(text: &str, statement: &DomainStatement) -> String {
    let end = text[statement.value_end..]
        .find('\n')
        .map(|index| statement.value_end + index)
        .unwrap_or(text.len());
    text[statement.value_end..end].strip_suffix('\r').unwrap_or(&text[statement.value_end..end]).to_string()
}

fn render_statement(text: &str, statement: &DomainStatement, value_text: &str, eol: &str) -> String {
    let suffix = statement_suffix(text, statement);
    let mut rendered = format!(
        "{}{}{}{}{}",
        statement.indent, statement.key, statement.separator, value_text, suffix
    );
    if !rendered.ends_with('\n') {
        rendered.push_str(eol);
    }
    rendered
}

fn serialize_value_text(key: &str, value: &Value) -> Option<String> {
    let prefix = format!("{key} = ");
    let wrapped = Value::Object(Map::from_iter([(key.to_string(), value.clone())]));
    let serialized = toml::to_string(&wrapped).ok()?;
    let text = serialized.strip_prefix(&prefix)?.to_string();
    Some(text.strip_suffix('\n').unwrap_or(&text).to_string())
}

fn serialize_table_block(path: &[String], value: &Value, eol: &str) -> Option<String> {
    let mut nested = value.clone();
    for key in path.iter().rev() {
        nested = Value::Object(Map::from_iter([(key.clone(), nested)]));
    }
    Some(toml::to_string(&nested).ok()?.replace('\n', eol))
}

fn block_anchor_line(block: &DomainBlock) -> usize {
    block.statements.last().map_or(block.start_line, |statement| statement.end_line)
}

fn plan_scalar_domain_edit(
    text: &str,
    scan: &DomainScan,
    update: &DomainUpdate,
    eol: &str,
) -> Option<Vec<LineEdit>> {
    let block = scan.blocks.first()?;
    if scan.blocks.len() != 1
        || block.has_header
        || !block.path.is_empty()
        || block.statements.len() != 1
    {
        return None;
    }
    let statement = block.statements.first()?;
    if statement.key != update.snake_key {
        return None;
    }
    let value_text = serialize_value_text(&statement.key, update.next_value.as_ref()?)?;
    Some(vec![LineEdit::Replace {
        start_line: statement.start_line,
        end_line: statement.end_line,
        text: render_statement(text, statement, &value_text, eol),
    }])
}

fn optional_deep_equal(a: Option<&Value>, b: Option<&Value>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => deep_equal(a, b),
        _ => false,
    }
}

fn plan_object_level(
    text: &str,
    root_key: &str,
    blocks: &[DomainBlock],
    prefix: &[String],
    previous_value: &Map<String, Value>,
    next_value: &Map<String, Value>,
    edits: &mut Vec<LineEdit>,
    appends: &mut Vec<String>,
    eol: &str,
) -> bool {
    let block = blocks.iter().find(|candidate| paths_equal(&candidate.path, prefix));
    let keys = previous_value
        .keys()
        .chain(next_value.keys())
        .cloned()
        .collect::<IndexSet<_>>();
    for key in keys {
        let previous = previous_value.get(&key);
        let next = next_value.get(&key);
        if optional_deep_equal(previous, next) {
            continue;
        }
        let child_path = prefix.iter().cloned().chain(std::iter::once(key.clone())).collect::<Vec<_>>();
        let statement = block.and_then(|block| block.statements.iter().find(|candidate| candidate.key == key));
        let child_block = blocks.iter().find(|candidate| paths_equal(&candidate.path, &child_path));
        if statement.is_some() && child_block.is_some() {
            return false;
        }
        let Some(next) = next else {
            if let Some(child_block) = child_block {
                if child_block.is_array {
                    return false;
                }
                for nested in blocks_nested_under(blocks, &child_path) {
                    edits.push(remove_lines(nested.start_line, nested.end_line));
                }
                continue;
            }
            let Some(statement) = statement else {
                return false;
            };
            edits.push(remove_lines(statement.start_line, statement.end_line));
            continue;
        };
        let Some(previous) = previous else {
            if is_plain_object(next) {
                let path = std::iter::once(root_key.to_string()).chain(child_path.iter().cloned()).collect::<Vec<_>>();
                let Some(block) = serialize_table_block(&path, next, eol) else {
                    return false;
                };
                appends.push(block);
            } else {
                let Some(value_text) = serialize_value_text(&key, next) else {
                    return false;
                };
                if let Some(block) = block {
                    insert_merged(edits, block_anchor_line(block), format!("{key} = {value_text}{eol}"));
                } else {
                    let path = std::iter::once(root_key.to_string()).chain(prefix.iter().cloned()).collect::<Vec<_>>();
                    let wrapped = Value::Object(Map::from_iter([(key.clone(), next.clone())]));
                    let Some(block) = serialize_table_block(&path, &wrapped, eol) else {
                        return false;
                    };
                    appends.push(block);
                }
            }
            continue;
        };
        if is_plain_object(previous) && is_plain_object(next) {
            if statement.is_some() || child_block.is_none_or(|block| block.is_array) {
                return false;
            }
            if !plan_object_level(
                text,
                root_key,
                blocks,
                &child_path,
                previous.as_object().unwrap(),
                next.as_object().unwrap(),
                edits,
                appends,
                eol,
            ) {
                return false;
            }
            continue;
        }
        if is_plain_object(next) {
            let Some(statement) = statement else {
                return false;
            };
            edits.push(remove_lines(statement.start_line, statement.end_line));
            let path = std::iter::once(root_key.to_string()).chain(child_path.iter().cloned()).collect::<Vec<_>>();
            let Some(block) = serialize_table_block(&path, next, eol) else {
                return false;
            };
            appends.push(block);
            continue;
        }
        if is_plain_object(previous) {
            if child_block.is_none_or(|block| block.is_array) {
                return false;
            }
            for nested in blocks_nested_under(blocks, &child_path) {
                edits.push(remove_lines(nested.start_line, nested.end_line));
            }
            let Some(value_text) = serialize_value_text(&key, next) else {
                return false;
            };
            if let Some(block) = block {
                insert_merged(edits, block_anchor_line(block), format!("{key} = {value_text}{eol}"));
            } else {
                let path = std::iter::once(root_key.to_string()).chain(prefix.iter().cloned()).collect::<Vec<_>>();
                let wrapped = Value::Object(Map::from_iter([(key.clone(), next.clone())]));
                let Some(block) = serialize_table_block(&path, &wrapped, eol) else {
                    return false;
                };
                appends.push(block);
            }
            continue;
        }
        let Some(statement) = statement else {
            return false;
        };
        let Some(value_text) = serialize_value_text(&key, next) else {
            return false;
        };
        edits.push(LineEdit::Replace {
            start_line: statement.start_line,
            end_line: statement.end_line,
            text: render_statement(text, statement, &value_text, eol),
        });
    }
    true
}

fn plan_domain_key_edit(
    text: &str,
    scan: &DomainScan,
    region: &RootRegion,
    update: &DomainUpdate,
    eol: &str,
) -> Option<Vec<LineEdit>> {
    if scan.ambiguous {
        return None;
    }
    let previous_object = update.previous_value.as_ref().and_then(Value::as_object);
    let next_object = update.next_value.as_ref().and_then(Value::as_object);
    let (Some(previous), Some(next)) = (previous_object, next_object) else {
        if previous_object.is_some() || next_object.is_some() {
            return None;
        }
        return plan_scalar_domain_edit(text, scan, update, eol);
    };
    let mut edits = Vec::new();
    let mut appends = Vec::new();
    if !plan_object_level(
        text,
        &update.snake_key,
        &scan.blocks,
        &[],
        previous,
        next,
        &mut edits,
        &mut appends,
        eol,
    ) {
        return None;
    }
    if !appends.is_empty() {
        edits.push(LineEdit::Insert { after_line: region.end, text: appends.join("") });
    }
    Some(edits)
}

fn edit_position(edit: &LineEdit) -> f64 {
    match edit {
        LineEdit::Replace { start_line, .. } => *start_line as f64,
        LineEdit::Insert { after_line, .. } => *after_line as f64 + 0.5,
    }
}

fn apply_line_edits(lines: &[String], edits: &[LineEdit], eol: &str) -> String {
    let mut ordered = edits.to_vec();
    ordered.sort_by(|a, b| edit_position(b).total_cmp(&edit_position(a)));
    let mut out = lines.to_vec();
    for edit in ordered {
        match edit {
            LineEdit::Replace { start_line, end_line, text } => {
                out.splice(start_line..=end_line, split_lines_keep_ends(&text));
            }
            LineEdit::Insert { after_line, text } => {
                let prefix = if after_line < out.len() && !out[after_line].ends_with('\n') {
                    eol
                } else {
                    ""
                };
                out.splice(after_line + 1..after_line + 1, split_lines_keep_ends(&format!("{prefix}{text}")));
            }
        }
    }
    out.join("")
}

fn verify_planned_text(text: &str, expected: &Map<String, Value>) -> bool {
    if text.trim().is_empty() {
        return expected.is_empty();
    }
    toml::from_str::<toml::Value>(text)
        .ok()
        .and_then(|value| serde_json::to_value(value).ok())
        .is_some_and(|value| deep_equal(&value, &Value::Object(expected.clone())))
}

pub fn plan_config_writeback(
    original_text: &str,
    updates: &[DomainUpdate],
    expected: &Map<String, Value>,
) -> Option<String> {
    let scanned = scan_root_regions(original_text)?;
    let mut regions_by_key: HashMap<String, Vec<RootRegion>> = HashMap::new();
    for segment in &scanned.segments {
        let RootSegment::Region(region) = segment else {
            continue;
        };
        regions_by_key.entry(region.root_key.clone()).or_default().push(region.clone());
    }
    let mut edits = Vec::new();
    let mut appends = Vec::new();
    for update in updates {
        if optional_deep_equal(update.previous_value.as_ref(), update.next_value.as_ref()) {
            continue;
        }
        let empty = Vec::new();
        let regions = regions_by_key.get(&update.snake_key).unwrap_or(&empty);
        if update.previous_value.is_none() && !regions.is_empty() {
            return None;
        }
        let Some(next_value) = update.next_value.as_ref() else {
            if update.previous_value.is_none() {
                continue;
            }
            if regions.is_empty() {
                return None;
            }
            for region in regions {
                edits.push(remove_lines(region.start, region.end));
            }
            continue;
        };
        if regions.is_empty() {
            if update.previous_value.is_some() {
                return None;
            }
            appends.push(serialize_table_block(&[update.snake_key.clone()], next_value, scanned.eol)?);
            continue;
        }
        let replacement = serialize_table_block(&[update.snake_key.clone()], next_value, scanned.eol)?;
        if regions.len() > 1 || regions[0].dotted {
            edits.push(LineEdit::Replace {
                start_line: regions[0].start,
                end_line: regions[0].end,
                text: replacement,
            });
            for extra in &regions[1..] {
                edits.push(remove_lines(extra.start, extra.end));
            }
            continue;
        }
        let region = &regions[0];
        let scan = scan_domain_region(original_text, &scanned.lines, &scanned.offsets, region, &update.snake_key);
        let planned = scan.as_ref().and_then(|scan| plan_domain_key_edit(original_text, scan, region, update, scanned.eol));
        if let Some(planned) = planned {
            edits.extend(planned);
        } else {
            edits.push(LineEdit::Replace {
                start_line: region.start,
                end_line: region.end,
                text: replacement,
            });
        }
    }
    let mut text = apply_line_edits(&scanned.lines, &edits, scanned.eol);
    for block in appends {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push_str(scanned.eol);
        }
        text.push_str(&block);
    }
    if !verify_planned_text(&text, expected) {
        return None;
    }
    Some(text)
}
