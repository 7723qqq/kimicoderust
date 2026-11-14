#[cfg(feature = "napi")]
use napi_derive::napi;

use ignore::WalkBuilder;
use regex::RegexBuilder;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::native::file_type::is_sensitive_file;

pub const DEFAULT_HEAD_LIMIT: usize = 250;

pub const MAX_OUTPUT_BYTES: usize = 512 * 1024;

pub const DEFAULT_TIMEOUT_MS: u64 = 20_000;

const VCS_DIRECTORIES_TO_EXCLUDE: &[&str] = &[".git", ".svn", ".hg", ".bzr", ".jj", ".sl"];

#[derive(Debug, Clone)]
#[cfg_attr(feature = "napi", napi(object))]
pub struct GrepResult {
    pub content: String,
    pub error: Option<String>,
    pub match_count: i32,
    pub file_count: i32,

    pub filtered_sensitive: Vec<String>,

    pub timed_out: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Content,
    FilesWithMatches,
    CountMatches,
}

pub struct GrepConfig {
    pub pattern: String,
    pub path: Option<String>,
    pub glob: Option<String>,

    pub file_type: Option<String>,
    pub output_mode: OutputMode,
    pub case_insensitive: bool,
    pub line_numbers: bool,
    pub after_context: usize,
    pub before_context: usize,
    pub context: usize,
    pub head_limit: usize,
    pub offset: usize,
    pub multiline: bool,

    pub include_ignored: bool,

    pub timeout_ms: Option<u64>,
}

impl Default for GrepConfig {
    fn default() -> Self {
        Self {
            pattern: String::new(),
            path: None,
            glob: None,
            file_type: None,
            output_mode: OutputMode::FilesWithMatches,
            case_insensitive: false,
            line_numbers: true,
            after_context: 0,
            before_context: 0,
            context: 0,
            head_limit: DEFAULT_HEAD_LIMIT,
            offset: 0,
            multiline: false,
            include_ignored: false,
            timeout_ms: Some(DEFAULT_TIMEOUT_MS),
        }
    }
}

struct MatchEntry {
    file: PathBuf,
    line_no: usize,
    line: String,
}

const CONTENT_CACHE_MAX_BYTES: usize = 8 * 1024 * 1024;

fn starts_with_nul(reader: &mut BufReader<fs::File>) -> bool {
    reader
        .fill_buf()
        .map(|buf| buf.contains(&0))
        .unwrap_or(false)
}

fn multiline_match_lines(regex: &regex::Regex, content: &str) -> (usize, Vec<bool>) {
    let lines = content.split('\n').count();
    let mut is_match_line = vec![false; lines];
    let last_idx = lines.saturating_sub(1);
    let bytes = content.as_bytes();
    let mut cursor = 0usize;
    let mut nl = 0usize;

    let mut line_at = |off: usize| -> usize {
        let target = off.min(bytes.len());
        while cursor < target {
            if bytes[cursor] == b'\n' {
                nl += 1;
            }
            cursor += 1;
        }
        nl
    };

    let mut total = 0usize;
    for m in regex.find_iter(content) {
        if m.start() == m.end() && m.start() >= content.len() {
            let dangling_last_line = !content.is_empty() && !content.ends_with('\n');
            if !(total == 0 && dangling_last_line) {
                continue;
            }
            total += 1;
            is_match_line[last_idx] = true;
            continue;
        }
        total += 1;
        let start_line = line_at(m.start()).min(last_idx);
        let end_line = if m.end() > m.start() {
            line_at(m.end() - 1)
        } else if bytes.get(m.start()) == Some(&b'\n') {
            start_line + 1
        } else {
            start_line
        }
        .min(last_idx);
        for slot in is_match_line.iter_mut().take(end_line + 1).skip(start_line) {
            *slot = true;
        }
    }
    (total, is_match_line)
}

fn read_lossy_line(reader: &mut BufReader<fs::File>, buf: &mut Vec<u8>, out: &mut String) -> bool {
    buf.clear();
    let Ok(n) = reader.read_until(b'\n', buf) else {
        return false;
    };
    if n == 0 {
        return false;
    }
    let line = String::from_utf8_lossy(buf);
    let line = line.strip_suffix('\n').unwrap_or(&line);
    let line = line.strip_suffix('\r').unwrap_or(line);
    out.clear();
    out.push_str(line);
    true
}

pub fn grep_search(config: &GrepConfig) -> GrepResult {
    let pattern_str = if config.case_insensitive {
        format!("(?i){}", config.pattern)
    } else {
        config.pattern.clone()
    };

    let regex = match RegexBuilder::new(&pattern_str)
        .multi_line(config.multiline)
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            return GrepResult {
                content: String::new(),
                error: Some(format!("Invalid regex pattern: {}", e)),
                match_count: 0,
                file_count: 0,
                filtered_sensitive: Vec::new(),
                timed_out: false,
            };
        }
    };

    let search_path = config
        .path
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    if !search_path.exists() {
        return GrepResult {
            content: String::new(),
            error: Some(format!("Path does not exist: {}", search_path.display())),
            match_count: 0,
            file_count: 0,
            filtered_sensitive: Vec::new(),
            timed_out: false,
        };
    }

    if search_path.is_file() {
        return search_single_file(&search_path, &regex, config);
    }

    let mut builder = WalkBuilder::new(&search_path);
    builder.hidden(false);
    if config.include_ignored {
        builder.git_ignore(false);
        builder.git_exclude(false);
        builder.git_global(false);
        builder.ignore(false);
        builder.parents(false);
    } else {
        builder.git_ignore(true);
        builder.git_exclude(true);
    }

    let glob_filter = config
        .glob
        .as_deref()
        .and_then(|g| globset::GlobBuilder::new(g).build().ok())
        .map(|g| g.compile_matcher());

    let file_type_globs: Vec<globset::GlobMatcher> = config
        .file_type
        .as_deref()
        .map(file_type_to_globs)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|pat| globset::GlobBuilder::new(&pat).build().ok())
        .map(|g| g.compile_matcher())
        .collect();

    let effective_before = if config.context > 0 {
        config.context
    } else {
        config.before_context
    };
    let effective_after = if config.context > 0 {
        config.context
    } else {
        config.after_context
    };
    let needs_context = effective_before > 0 || effective_after > 0;
    let is_files_with_matches = config.output_mode == OutputMode::FilesWithMatches;
    let needs_full_content = config.output_mode == OutputMode::Content || needs_context;

    let file_matches: Mutex<Vec<(PathBuf, usize, std::time::SystemTime)>> = Mutex::new(Vec::new());

    let content_cache: Mutex<Vec<(PathBuf, String)>> = Mutex::new(Vec::new());
    let cached_bytes = std::sync::atomic::AtomicUsize::new(0);
    let filtered_sensitive: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let timed_out = AtomicBool::new(false);
    let deadline = config
        .timeout_ms
        .map(|ms| Instant::now() + Duration::from_millis(ms));

    builder.build_parallel().run(|| {
        let regex = &regex;
        let glob_filter = &glob_filter;
        let file_type_globs = &file_type_globs;
        let file_matches = &file_matches;
        let content_cache = &content_cache;
        let cached_bytes = &cached_bytes;
        let filtered_sensitive = &filtered_sensitive;
        let timed_out = &timed_out;
        let deadline = &deadline;

        Box::new(move |entry| {
            if timed_out.load(Ordering::Relaxed) {
                return ignore::WalkState::Quit;
            }
            if let Some(d) = deadline
                && Instant::now() >= *d {
                    timed_out.store(true, Ordering::Relaxed);
                    return ignore::WalkState::Quit;
                }

            let entry = match entry {
                Ok(e) => e,
                Err(_) => return ignore::WalkState::Continue,
            };

            if entry
                .path()
                .components()
                .any(|c| matches!(c.as_os_str().to_str(), Some(name) if VCS_DIRECTORIES_TO_EXCLUDE.contains(&name)))
            {
                return ignore::WalkState::Continue;
            }

            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                return ignore::WalkState::Continue;
            }

            let path = entry.path();

            if let Some(matcher) = glob_filter
                && !matcher.is_match(path) {
                    return ignore::WalkState::Continue;
                }

            if !file_type_globs.is_empty() && !file_type_globs.iter().any(|m| m.is_match(path)) {
                return ignore::WalkState::Continue;
            }

            let path_str = path.to_string_lossy();
            if is_sensitive_file(&path_str) {
                filtered_sensitive
                    .lock()
                    .unwrap()
                    .push(path_str.to_string());
                return ignore::WalkState::Continue;
            }





            let file = match fs::File::open(path) {
                Ok(f) => f,
                Err(_) => return ignore::WalkState::Continue,
            };
            let mut reader = BufReader::new(file);
            if starts_with_nul(&mut reader) {
                return ignore::WalkState::Continue;
            }
            let mut match_count: usize = 0;
            let mut accumulated_content = String::new();
            let mut line_bytes = Vec::new();
            let mut line = String::new();

            if config.multiline {





                let mut raw: Vec<u8> = Vec::new();
                if reader.read_to_end(&mut raw).is_err() || raw.contains(&0) {
                    return ignore::WalkState::Continue;
                }
                let content = String::from_utf8_lossy(&raw);
                let (count, _marked) = multiline_match_lines(regex, &content);
                match_count = count;
                if match_count > 0 && needs_full_content {


                    accumulated_content.push_str(&content);
                }
            } else if is_files_with_matches {

                while read_lossy_line(&mut reader, &mut line_bytes, &mut line) {
                    if regex.find(&line).is_some() {
                        match_count = 1;
                        break;
                    }
                }
            } else if needs_full_content {

                while read_lossy_line(&mut reader, &mut line_bytes, &mut line) {
                    match_count += regex.find_iter(&line).count();
                    accumulated_content.push_str(&line);
                    accumulated_content.push('\n');
                }
            } else {

                while read_lossy_line(&mut reader, &mut line_bytes, &mut line) {
                    match_count += regex.find_iter(&line).count();
                }
            }

            if match_count > 0 {
                let mtime = fs::metadata(path)
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                file_matches.lock().unwrap().push((path.to_path_buf(), match_count, mtime));
                if needs_full_content && !accumulated_content.is_empty() {
                    let len = accumulated_content.len();
                    let previous = cached_bytes.fetch_add(len, Ordering::Relaxed);
                    if previous + len <= CONTENT_CACHE_MAX_BYTES {
                        content_cache.lock().unwrap().push((path.to_path_buf(), accumulated_content));
                    }
                }
            }

            ignore::WalkState::Continue
        })
    });

    let timed_out = timed_out.load(Ordering::Relaxed);
    let mut file_matches: Vec<(PathBuf, usize, std::time::SystemTime)> =
        file_matches.into_inner().unwrap();
    let filtered_sensitive = filtered_sensitive.into_inner().unwrap();
    let content_cache: Vec<(PathBuf, String)> = content_cache.into_inner().unwrap();
    let total_matches: usize = file_matches.iter().map(|(_, c, _)| c).sum();

    file_matches.sort_by_key(|(_, _, mtime)| std::cmp::Reverse(*mtime));

    let content = if needs_full_content {
        let content_map: std::collections::HashMap<&Path, &str> = content_cache
            .iter()
            .map(|(path, content)| (path.as_path(), content.as_str()))
            .collect();

        let mut line_matches: Vec<MatchEntry> = Vec::new();
        let mut output_bytes: usize = 0;
        for (path, _, _) in &file_matches {
            let content_str = match content_map.get(path.as_path()) {
                Some(c) => c.to_string(),
                None => match fs::read(path) {
                    Ok(data) => String::from_utf8_lossy(&data).to_string(),
                    Err(_) => continue,
                },
            };
            let lines: Vec<&str> = content_str.split('\n').collect();
            let matched_lines: Vec<usize> = if config.multiline {
                let (_, marked) = multiline_match_lines(&regex, &content_str);
                marked
                    .into_iter()
                    .enumerate()
                    .filter(|(_, is_match)| *is_match)
                    .map(|(idx, _)| idx)
                    .collect()
            } else {
                lines
                    .iter()
                    .enumerate()
                    .filter(|(_, line)| regex.is_match(line))
                    .map(|(i, _)| i)
                    .collect()
            };

            let mut shown_lines: std::collections::HashSet<usize> =
                std::collections::HashSet::new();
            for &line_idx in &matched_lines {
                let start = if effective_before > 0 {
                    line_idx.saturating_sub(effective_before)
                } else {
                    line_idx
                };
                let end = if effective_after > 0 {
                    (line_idx + effective_after + 1).min(lines.len())
                } else {
                    line_idx + 1
                };

                for (ctx_idx, line_content) in lines.iter().enumerate().take(end).skip(start) {
                    if !shown_lines.insert(ctx_idx) {
                        continue;
                    }
                    let line_no = ctx_idx + 1;
                    let entry = MatchEntry {
                        file: path.to_path_buf(),
                        line_no,
                        line: line_content.to_string(),
                    };
                    let entry_bytes = format_entry_bytes(&entry, config);
                    if output_bytes + entry_bytes > MAX_OUTPUT_BYTES {
                        break;
                    }
                    output_bytes += entry_bytes;
                    line_matches.push(entry);
                }
            }
            if output_bytes > MAX_OUTPUT_BYTES {
                break;
            }
        }

        let mut rendered = Vec::new();
        let mut prev_file: Option<PathBuf> = None;
        for entry in &line_matches {
            if prev_file.as_ref() != Some(&entry.file) {
                if prev_file.is_some() {
                    rendered.push("--".to_string());
                }
                prev_file = Some(entry.file.clone());
            }
            let display_path = entry.file.display().to_string();
            if config.line_numbers {
                rendered.push(format!("{}:{}:{}", display_path, entry.line_no, entry.line));
            } else {
                rendered.push(format!("{}:{}", display_path, entry.line));
            }
        }
        rendered.join("\n")
    } else {
        match config.output_mode {
            OutputMode::FilesWithMatches => {
                let files: Vec<String> = file_matches
                    .iter()
                    .skip(config.offset)
                    .take(config.head_limit)
                    .map(|(p, _, _)| p.display().to_string())
                    .collect();
                files.join("\n")
            }
            OutputMode::CountMatches => {
                let mut lines = Vec::new();
                for (path, count, _) in &file_matches {
                    lines.push(format!("{}:{}", path.display(), count));
                }
                lines.join("\n")
            }
            OutputMode::Content => unreachable!(),
        }
    };

    GrepResult {
        content,
        error: None,
        match_count: total_matches as i32,
        file_count: file_matches.len() as i32,
        filtered_sensitive,
        timed_out,
    }
}

fn search_single_file(path: &Path, regex: &regex::Regex, config: &GrepConfig) -> GrepResult {
    let path_str = path.to_string_lossy();
    if is_sensitive_file(&path_str) {
        return GrepResult {
            content: String::new(),
            error: None,
            match_count: 0,
            file_count: 0,
            filtered_sensitive: vec![path.display().to_string()],
            timed_out: false,
        };
    }

    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) => {
            return GrepResult {
                content: String::new(),
                error: Some(format!("Failed to read {}: {}", path.display(), e)),
                match_count: 0,
                file_count: 0,
                filtered_sensitive: Vec::new(),
                timed_out: false,
            };
        }
    };
    if bytes.contains(&0) {
        return GrepResult {
            content: String::new(),
            error: None,
            match_count: 0,
            file_count: 0,
            filtered_sensitive: Vec::new(),
            timed_out: false,
        };
    }

    let content = String::from_utf8_lossy(&bytes).to_string();

    let lines: Vec<&str> = content.split('\n').collect();
    let matched_lines: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| regex.is_match(line))
        .map(|(i, _)| i)
        .collect();

    let match_count = matched_lines.len();

    let effective_before = if config.context > 0 {
        config.context
    } else {
        config.before_context
    };
    let effective_after = if config.context > 0 {
        config.context
    } else {
        config.after_context
    };

    let content = match config.output_mode {
        OutputMode::FilesWithMatches => {
            if match_count > 0 {
                path.display().to_string()
            } else {
                String::new()
            }
        }
        OutputMode::CountMatches => {
            format!("{}:{}", path.display(), match_count)
        }
        OutputMode::Content => {
            let mut rendered = Vec::new();
            let mut shown_lines: std::collections::HashSet<usize> =
                std::collections::HashSet::new();

            for &line_idx in &matched_lines {
                let start = line_idx.saturating_sub(effective_before);
                let end = (line_idx + effective_after + 1).min(lines.len());

                for (ctx_idx, line_content) in lines.iter().enumerate().take(end).skip(start) {
                    if shown_lines.contains(&ctx_idx) {
                        continue;
                    }
                    shown_lines.insert(ctx_idx);
                    let line_no = ctx_idx + 1;
                    if config.line_numbers {
                        rendered.push(format!("{}:{}", line_no, line_content));
                    } else {
                        rendered.push(line_content.to_string());
                    }
                }
            }

            rendered.join("\n")
        }
    };

    GrepResult {
        content,
        error: None,
        match_count: match_count as i32,
        file_count: if match_count > 0 { 1 } else { 0 },
        filtered_sensitive: Vec::new(),
        timed_out: false,
    }
}

fn format_entry_bytes(entry: &MatchEntry, config: &GrepConfig) -> usize {
    let path_len = entry.file.display().to_string().len();
    if config.line_numbers {
        path_len + 1 + 10 + 1 + entry.line.len() + 1
    } else {
        path_len + 1 + entry.line.len() + 1
    }
}

fn file_type_to_globs(name: &str) -> Vec<String> {
    let lower = name.to_lowercase();
    let exts: &[&str] = match lower.as_str() {
        "ts" => &["ts", "tsx", "cts", "mts"],
        "tsx" => &["tsx"],
        "js" => &["js", "jsx", "cjs", "mjs"],
        "jsx" => &["jsx"],
        "py" | "python" => &["py", "pyi", "pyx"],
        "rs" | "rust" => &["rs"],
        "go" => &["go"],
        "java" => &["java"],
        "kt" | "kotlin" => &["kt", "kts"],
        "rb" | "ruby" => &["rb", "rake", "gemspec"],
        "c" => &["c", "h"],
        "cpp" | "cxx" | "c++" => &["cpp", "cxx", "cc", "hpp", "hxx", "hh", "h"],
        "cs" | "csharp" => &["cs"],
        "swift" => &["swift"],
        "php" => &["php", "phtml"],
        "sh" | "shell" | "bash" => &["sh", "bash", "zsh", "fish"],
        "md" | "markdown" => &["md", "markdown"],
        "json" => &["json"],
        "yaml" | "yml" => &["yaml", "yml"],
        "toml" => &["toml"],
        "xml" => &["xml"],
        "html" => &["html", "htm"],
        "css" => &["css", "scss", "sass", "less"],
        "sql" => &["sql"],
        "lua" => &["lua"],
        "vue" => &["vue"],
        "svelte" => &["svelte"],
        _ => &[],
    };
    exts.iter().map(|ext| format!("**/*.{}", ext)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn setup_test_dir() -> TempDir {
        let dir = TempDir::new().unwrap();
        let file1 = dir.path().join("test1.txt");
        let file2 = dir.path().join("test2.txt");
        let file3 = dir.path().join("subdir").join("test3.txt");

        fs::create_dir_all(file3.parent().unwrap()).unwrap();

        let mut f1 = fs::File::create(&file1).unwrap();
        writeln!(f1, "hello world").unwrap();
        writeln!(f1, "foo bar baz").unwrap();
        writeln!(f1, "hello again").unwrap();

        let mut f2 = fs::File::create(&file2).unwrap();
        writeln!(f2, "nothing here").unwrap();
        writeln!(f2, "some HELLO text").unwrap();

        let mut f3 = fs::File::create(&file3).unwrap();
        writeln!(f3, "nested hello").unwrap();

        dir
    }

    #[test]
    fn test_grep_files_with_matches() {
        let dir = setup_test_dir();
        let result = grep_search(&GrepConfig {
            pattern: "hello".to_string(),
            path: Some(dir.path().to_str().unwrap().to_string()),
            output_mode: OutputMode::FilesWithMatches,
            ..Default::default()
        });
        assert!(result.error.is_none());
        assert!(result.match_count >= 2);
        assert!(result.file_count >= 2);
    }

    #[test]
    fn test_grep_content_mode() {
        let dir = setup_test_dir();
        let result = grep_search(&GrepConfig {
            pattern: "hello".to_string(),
            path: Some(dir.path().to_str().unwrap().to_string()),
            output_mode: OutputMode::Content,
            line_numbers: true,
            ..Default::default()
        });
        assert!(result.error.is_none());
        assert!(result.content.contains("1:hello world"));
    }

    #[test]
    fn test_grep_multiline_flag_keeps_single_line_matches() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.txt"), "hello world\nsecond line\n").unwrap();

        let result = grep_search(&GrepConfig {
            pattern: "world".to_string(),
            path: Some(dir.path().to_str().unwrap().to_string()),
            output_mode: OutputMode::Content,
            multiline: true,
            ..Default::default()
        });
        assert!(result.error.is_none());
        assert_eq!(
            result.match_count, 1,
            "multiline=true must still match within one line"
        );
    }

    #[test]
    fn test_grep_multiline_matches_across_newlines() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("multi.txt"),
            "start here\nEND marker\ntail\n",
        )
        .unwrap();
        fs::write(dir.path().join("plain.txt"), "END alone\n").unwrap();

        let result = grep_search(&GrepConfig {
            pattern: "here\\nEND".to_string(),
            path: Some(dir.path().to_str().unwrap().to_string()),
            output_mode: OutputMode::Content,
            line_numbers: true,
            multiline: true,
            ..Default::default()
        });
        assert!(
            result.error.is_none(),
            "{}",
            result.error.unwrap_or_default()
        );
        assert_eq!(result.file_count, 1, "only the cross-line file matches");
        assert!(result.content.contains("multi.txt:1:start here"));
        assert!(result.content.contains("multi.txt:2:END marker"));
        assert!(!result.content.contains("plain.txt"));
    }

    #[test]
    fn test_grep_single_line_mode_does_not_span_newlines() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("multi.txt"), "start here\nEND marker\n").unwrap();

        let result = grep_search(&GrepConfig {
            pattern: "here\\nEND".to_string(),
            path: Some(dir.path().to_str().unwrap().to_string()),
            output_mode: OutputMode::FilesWithMatches,
            multiline: false,
            ..Default::default()
        });
        assert!(result.error.is_none());
        assert_eq!(result.file_count, 0);
    }

    #[test]
    fn test_grep_binary_file_skipped() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("text.txt"), b"hello text\n").unwrap();
        let mut binary = Vec::new();
        binary.extend_from_slice(b"\x00\x01\x02hello\x00");
        binary.extend_from_slice(&[0u8; 64]);
        fs::write(dir.path().join("bin.dat"), binary).unwrap();

        let result = grep_search(&GrepConfig {
            pattern: "hello".to_string(),
            path: Some(dir.path().to_str().unwrap().to_string()),
            output_mode: OutputMode::FilesWithMatches,
            ..Default::default()
        });
        assert!(result.error.is_none());
        assert_eq!(result.file_count, 1, "the binary file must be skipped");
        assert!(result.content.contains("text.txt"));
        assert!(!result.content.contains("bin.dat"));
    }

    #[test]
    fn test_grep_invalid_utf8_does_not_truncate() {
        let dir = TempDir::new().unwrap();

        let mut data: Vec<u8> = Vec::new();
        data.extend_from_slice(b"\xff\xfe broken encoding line\n");
        data.extend_from_slice(b"target match here\n");
        fs::write(dir.path().join("mixed.dat"), data).unwrap();

        let result = grep_search(&GrepConfig {
            pattern: "target match".to_string(),
            path: Some(dir.path().to_str().unwrap().to_string()),
            output_mode: OutputMode::FilesWithMatches,
            ..Default::default()
        });
        assert!(result.error.is_none());
        assert_eq!(
            result.file_count, 1,
            "the match after the bad line must be found"
        );
    }

    #[test]
    fn test_read_lossy_line_strips_eol_and_stops_at_eof() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("l.txt");
        fs::write(&path, b"a\r\nb\nc\xffd\n").unwrap();
        let file = fs::File::open(&path).unwrap();
        let mut reader = BufReader::new(file);
        let mut buf = Vec::new();
        let mut line = String::new();

        assert!(read_lossy_line(&mut reader, &mut buf, &mut line));
        assert_eq!(line, "a");
        assert!(read_lossy_line(&mut reader, &mut buf, &mut line));
        assert_eq!(line, "b");
        assert!(read_lossy_line(&mut reader, &mut buf, &mut line));
        assert_eq!(line, "c\u{FFFD}d");
        assert!(!read_lossy_line(&mut reader, &mut buf, &mut line));
    }

    #[test]
    fn test_grep_case_insensitive() {
        let dir = setup_test_dir();
        let result = grep_search(&GrepConfig {
            pattern: "hello".to_string(),
            path: Some(dir.path().to_str().unwrap().to_string()),
            output_mode: OutputMode::Content,
            case_insensitive: true,
            ..Default::default()
        });
        assert!(result.error.is_none());
        assert!(result.content.contains("HELLO"));
    }

    #[test]
    fn test_grep_count_mode() {
        let dir = setup_test_dir();
        let result = grep_search(&GrepConfig {
            pattern: "hello".to_string(),
            path: Some(dir.path().to_str().unwrap().to_string()),
            output_mode: OutputMode::CountMatches,
            case_insensitive: true,
            ..Default::default()
        });
        assert!(result.error.is_none());
        assert!(result.match_count >= 4);
    }

    #[test]
    fn test_grep_invalid_regex() {
        let dir = setup_test_dir();
        let result = grep_search(&GrepConfig {
            pattern: "[invalid".to_string(),
            path: Some(dir.path().to_str().unwrap().to_string()),
            ..Default::default()
        });
        assert!(result.error.is_some());
        assert!(result.error.unwrap().contains("Invalid regex"));
    }

    #[test]
    fn test_grep_nonexistent_path() {
        let temp_dir = TempDir::new().unwrap();
        let nonexistent = temp_dir.path().join("subdir");
        drop(temp_dir);
        let result = grep_search(&GrepConfig {
            pattern: "test".to_string(),
            path: Some(nonexistent.to_string_lossy().to_string()),
            ..Default::default()
        });
        assert!(result.error.is_some());
        assert!(result.error.unwrap().contains("does not exist"));
    }

    #[test]
    fn test_grep_single_file() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("test.txt");
        let mut f = fs::File::create(&file).unwrap();
        writeln!(f, "line one").unwrap();
        writeln!(f, "line two").unwrap();
        writeln!(f, "line three").unwrap();

        let result = grep_search(&GrepConfig {
            pattern: "two".to_string(),
            path: Some(file.to_str().unwrap().to_string()),
            output_mode: OutputMode::Content,
            ..Default::default()
        });
        assert!(result.error.is_none());
        assert_eq!(result.match_count, 1);
        assert!(result.content.contains("line two"));
    }

    #[test]
    fn test_grep_with_context() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("test.txt");
        let mut f = fs::File::create(&file).unwrap();
        writeln!(f, "line 1").unwrap();
        writeln!(f, "line 2").unwrap();
        writeln!(f, "MATCH HERE").unwrap();
        writeln!(f, "line 4").unwrap();
        writeln!(f, "line 5").unwrap();

        let result = grep_search(&GrepConfig {
            pattern: "MATCH".to_string(),
            path: Some(file.to_str().unwrap().to_string()),
            output_mode: OutputMode::Content,
            context: 1,
            ..Default::default()
        });
        assert!(result.error.is_none());
        assert!(result.content.contains("line 2"));
        assert!(result.content.contains("MATCH HERE"));
        assert!(result.content.contains("line 4"));
    }

    #[test]
    fn test_grep_context_dedup_overlap() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("overlap.txt");
        let mut f = fs::File::create(&file).unwrap();
        writeln!(f, "line 1").unwrap();
        writeln!(f, "MATCH A").unwrap();
        writeln!(f, "line 3").unwrap();
        writeln!(f, "MATCH B").unwrap();
        writeln!(f, "line 5").unwrap();

        let result = grep_search(&GrepConfig {
            pattern: "MATCH".to_string(),
            path: Some(dir.path().to_str().unwrap().to_string()),
            output_mode: OutputMode::Content,
            context: 1,
            ..Default::default()
        });
        assert!(result.error.is_none());

        for label in ["line 1", "line 3", "line 5"] {
            assert_eq!(
                result.content.matches(label).count(),
                1,
                "{label} should not be duplicated in overlapping context"
            );
        }

        assert!(result.content.contains("MATCH A"));
        assert!(result.content.contains("MATCH B"));
    }
}
