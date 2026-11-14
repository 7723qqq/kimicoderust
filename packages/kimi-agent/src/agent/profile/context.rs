use std::collections::{HashMap, HashSet};

use futures_util::future::join_all;
use indexmap::IndexMap;
use regex::Regex;

use crate::_base::exec_env::decode_text::TextDecodeErrors;
use crate::app::git::work_tree::find_git_work_tree;
use crate::os::interface::host_file_system::{HostReadTextOptions, IHostFileSystem};
use crate::app::agent_profile_catalog::agent_profile_catalog::AgentProfileContext;
use crate::wire::record::WireAsyncError;

use super::profile::SystemPromptContext;

pub const AGENTS_MD_RECOMMENDED_MAX_BYTES: usize = 32 * 1024;

pub const LIST_DIR_ROOT_WIDTH: usize = 30;
pub const LIST_DIR_CHILD_WIDTH: usize = 10;

#[derive(Clone, Copy)]
pub struct ProfileContextDeps<'a> {
    pub fs: &'a dyn IHostFileSystem,
    pub home_dir: &'a str,
}

pub type PreparedSystemPromptContext = SystemPromptContext;

#[derive(Debug, Clone, Default)]
pub struct PrepareSystemPromptContextOptions {
    pub additional_dirs: Option<Vec<String>>,
    pub preloaded_agents_md: Option<LoadedAgentsMd>,
}

pub async fn prepare_system_prompt_context(
    deps: ProfileContextDeps<'_>,
    work_dir: &str,
    brand_home: Option<&str>,
    options: Option<PrepareSystemPromptContextOptions>,
) -> Result<PreparedSystemPromptContext, WireAsyncError> {
    let options = options.unwrap_or_default();
    let additional_dirs = dedupe_dirs(options.additional_dirs.as_deref().unwrap_or(&[]));
    let work_dirs = [work_dir.to_string()];
    let (cwd_listing, agents_md_result, additional_dirs_info) = tokio::join!(
        list_directory(
            &deps,
            work_dir,
            ListDirectoryOptions {
                collapse_hidden_dirs: true,
            },
        ),
        async {
            match options.preloaded_agents_md {
                Some(loaded) => Ok(loaded),
                None => {
                    load_agents_md_for_roots(
                        deps.fs,
                        deps.home_dir,
                        brand_home,
                        &work_dirs,
                        None,
                    )
                    .await
                }
            }
        },
        load_additional_dirs_info(&deps, &additional_dirs),
    );
    let agents_md_result = agents_md_result?;
    let LoadedAgentsMd {
        content,
        warning,
        paths,
        files: _,
    } = agents_md_result;
    Ok(SystemPromptContext {
        base: AgentProfileContext {
            cwd_listing: Some(cwd_listing),
            agents_md: Some(content),
            additional_dirs_info: Some(additional_dirs_info),
            ..Default::default()
        },
        agents_md_warning: warning,
        agents_md_paths: Some(paths),
    })
}

pub async fn load_agents_md(
    deps: ProfileContextDeps<'_>,
    work_dir: &str,
    brand_home: Option<&str>,
) -> Result<String, WireAsyncError> {
    let work_dirs = [work_dir.to_string()];
    let result = load_agents_md_for_roots(
        deps.fs,
        deps.home_dir,
        brand_home,
        &work_dirs,
        None,
    )
    .await?;
    Ok(result.content)
}

pub async fn load_agents_md_detailed(
    deps: ProfileContextDeps<'_>,
    work_dir: &str,
    brand_home: Option<&str>,
) -> Result<LoadedAgentsMd, WireAsyncError> {
    let work_dirs = [work_dir.to_string()];
    load_agents_md_for_roots(
        deps.fs,
        deps.home_dir,
        brand_home,
        &work_dirs,
        None,
    )
    .await
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoadedAgentsMd {
    pub content: String,
    pub warning: Option<String>,
    pub paths: Vec<String>,
    pub files: IndexMap<String, String>,
}

pub fn split_agents_md_files(content: &str, paths: &[String]) -> IndexMap<String, String> {
    let wanted = paths.iter().map(|path| normalize(path)).collect::<HashSet<_>>();
    let mut files = IndexMap::new();
    let marker_re = Regex::new(r"(?m)^<!-- From: ([^\r\n]+) -->\r?$").unwrap();
    let mut previous: Option<(String, usize)> = None;
    for captures in marker_re.captures_iter(content) {
        let whole = captures.get(0).unwrap();
        let key = normalize(captures.get(1).unwrap().as_str());
        if !wanted.contains(&key) {
            continue;
        }
        if let Some((previous_key, body_start)) = previous.take() {
            files.insert(
                previous_key,
                trim_js(&content[body_start..whole.start()]).to_string(),
            );
        }
        previous = Some((key, (whole.end() + 1).min(content.len())));
    }
    if let Some((previous_key, body_start)) = previous {
        files.insert(
            previous_key,
            trim_js(&content[body_start..]).to_string(),
        );
    }
    files
}

pub const AGENTS_MD_PLAIN_NAMES: [&str; 2] = ["AGENTS.md", "agents.md"];

pub fn dot_kimi_agents_md_path(dir: &str) -> String {
    join(&[dir, ".kimi-code", "AGENTS.md"])
}

pub fn agents_md_candidate_paths(dir: &str) -> Vec<String> {
    let mut paths = vec![dot_kimi_agents_md_path(dir)];
    paths.extend(
        AGENTS_MD_PLAIN_NAMES
            .iter()
            .map(|name| join(&[dir, name])),
    );
    paths
}

pub fn extract_agents_md_paths_from_system_prompt(system_prompt: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut seen = HashSet::new();
    let marker_re = Regex::new(r"(?m)^<!-- From: ([^\r\n]+) -->\r?$").unwrap();
    for captures in marker_re.captures_iter(system_prompt) {
        let path = captures.get(1).unwrap().as_str();
        if !AGENTS_MD_PLAIN_NAMES
            .iter()
            .any(|candidate| *candidate == basename(path))
        {
            continue;
        }
        let normalized = normalize(path);
        if !seen.insert(normalized.clone()) {
            continue;
        }
        paths.push(normalized);
    }
    paths
}

pub async fn find_agents_md_in_dir(fs: &dyn IHostFileSystem, dir: &str) -> Vec<String> {
    let mut found = Vec::new();
    let dot_kimi = dot_kimi_agents_md_path(dir);
    if is_non_empty_file(fs, &dot_kimi).await {
        found.push(dot_kimi);
    }
    for file_name in AGENTS_MD_PLAIN_NAMES {
        let candidate = join(&[dir, file_name]);
        if is_non_empty_file(fs, &candidate).await {
            found.push(candidate);
            break;
        }
    }
    found
}

async fn is_non_empty_file(fs: &dyn IHostFileSystem, path: &str) -> bool {
    let Ok(content) = fs.read_text(path, Some(ignore_decode_errors())).await else {
        return false;
    };
    !trim_js(&content).is_empty()
}

pub async fn load_agents_md_for_roots<'a>(
    fs: &'a dyn IHostFileSystem,
    home_dir: &str,
    brand_home: Option<&str>,
    work_dirs: &[String],
    personal_fs: Option<&'a dyn IHostFileSystem>,
) -> Result<LoadedAgentsMd, WireAsyncError> {
    let personal_fs = personal_fs.unwrap_or(fs);
    let mut discovered = Vec::new();
    let mut seen_by_fs: HashMap<usize, HashSet<String>> = HashMap::new();
    let mut load_warnings = Vec::new();

    let real_home = home_dir;
    let brand_dir = brand_home
        .map(str::to_string)
        .unwrap_or_else(|| join(&[real_home, ".kimi-code"]));
    collect_agent_file(
        personal_fs,
        &join(&[&brand_dir, "AGENTS.md"]),
        &mut seen_by_fs,
        &mut discovered,
        &mut load_warnings,
    )
    .await;

    let generic_dirs = [join(&[real_home, ".agents"])];
    let generic_files = generic_dirs.iter().flat_map(|dir| {
        AGENTS_MD_PLAIN_NAMES
            .iter()
            .map(move |name| join(&[dir, name]))
    });
    for file in generic_files {
        if collect_agent_file(
            personal_fs,
            &file,
            &mut seen_by_fs,
            &mut discovered,
            &mut load_warnings,
        )
        .await
        {
            break;
        }
    }

    for work_dir in work_dirs {
        let root_work_dir = normalize(work_dir);
        let project_root = find_git_work_tree(fs, &root_work_dir)
            .await?
            .map(|work_tree| work_tree.root)
            .unwrap_or_else(|| root_work_dir.clone());
        let dirs = dirs_root_to_leaf(&root_work_dir, &project_root);
        for dir in dirs {
            collect_agent_file(
                fs,
                &dot_kimi_agents_md_path(&dir),
                &mut seen_by_fs,
                &mut discovered,
                &mut load_warnings,
            )
            .await;
            for file_name in AGENTS_MD_PLAIN_NAMES {
                if collect_agent_file(
                    fs,
                    &join(&[&dir, file_name]),
                    &mut seen_by_fs,
                    &mut discovered,
                    &mut load_warnings,
                )
                .await
                {
                    break;
                }
            }
        }
    }

    let content = render_agent_files(&discovered);
    let total_bytes = byte_length(&content);
    if total_bytes > AGENTS_MD_RECOMMENDED_MAX_BYTES {
        load_warnings.push(format!(
            "AGENTS.md total {} KB exceeds the recommended {} KB. Large instruction files increase cost and may impact performance; consider trimming.",
            format_kb(total_bytes),
            format_kb(AGENTS_MD_RECOMMENDED_MAX_BYTES),
        ));
    }
    let warning = if load_warnings.is_empty() {
        None
    } else {
        Some(load_warnings.join("\n"))
    };
    let paths = discovered
        .iter()
        .map(|file| normalize(&file.path))
        .collect();
    let files = discovered
        .iter()
        .map(|file| (normalize(&file.path), file.content.clone()))
        .collect();
    Ok(LoadedAgentsMd {
        content,
        warning,
        paths,
        files,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentsMdWatchRoot {
    pub root: String,
    pub candidates: Vec<String>,
}

pub async fn agents_md_watch_roots(
    fs: &dyn IHostFileSystem,
    home_dir: &str,
    work_dir: &str,
    brand_home: Option<&str>,
) -> Result<Vec<AgentsMdWatchRoot>, WireAsyncError> {
    let real_home = home_dir;
    let brand_dir = brand_home
        .map(str::to_string)
        .unwrap_or_else(|| join(&[real_home, ".kimi-code"]));
    let mut plan = vec![
        AgentsMdWatchRoot {
            root: brand_dir.clone(),
            candidates: vec![join(&[&brand_dir, "AGENTS.md"])],
        },
        AgentsMdWatchRoot {
            root: real_home.to_string(),
            candidates: vec![
                join(&[real_home, ".agents", "AGENTS.md"]),
                join(&[real_home, ".agents", "agents.md"]),
            ],
        },
    ];
    let root_work_dir = normalize(work_dir);
    let project_root = find_git_work_tree(fs, &root_work_dir)
        .await?
        .map(|work_tree| work_tree.root)
        .unwrap_or_else(|| root_work_dir.clone());
    let mut project_candidates = Vec::new();
    for dir in dirs_root_to_leaf(&root_work_dir, &project_root) {
        project_candidates.push(join(&[&dir, ".kimi-code", "AGENTS.md"]));
        project_candidates.push(join(&[&dir, "AGENTS.md"]));
        project_candidates.push(join(&[&dir, "agents.md"]));
    }
    plan.push(AgentsMdWatchRoot {
        root: project_root,
        candidates: project_candidates,
    });
    Ok(plan)
}

async fn load_additional_dirs_info(
    deps: &ProfileContextDeps<'_>,
    additional_dirs: &[String],
) -> String {
    let sections = join_all(additional_dirs.iter().map(|dir| async {
        let listing = list_directory(deps, dir, ListDirectoryOptions::default()).await;
        format!("### {dir}\n{listing}")
    }))
    .await;
    sections.join("\n\n")
}

pub async fn find_project_root(
    fs: &dyn IHostFileSystem,
    work_dir: &str,
) -> Result<String, WireAsyncError> {
    let root_work_dir = normalize(work_dir);
    Ok(find_git_work_tree(fs, &root_work_dir)
        .await?
        .map(|work_tree| work_tree.root)
        .unwrap_or(root_work_dir))
}

pub fn dirs_root_to_leaf(work_dir: &str, project_root: &str) -> Vec<String> {
    let mut dirs = Vec::new();
    let mut current = normalize(work_dir);
    loop {
        dirs.push(current.clone());
        if current.as_str() == project_root {
            break;
        }
        let parent = dirname(&current);
        if parent == current {
            break;
        }
        current = parent;
    }
    dirs.reverse();
    dirs
}

#[derive(Debug, Clone)]
struct AgentFile {
    path: String,
    content: String,
}

async fn collect_agent_file(
    fs: &dyn IHostFileSystem,
    path: &str,
    seen_by_fs: &mut HashMap<usize, HashSet<String>>,
    discovered: &mut Vec<AgentFile>,
    load_warnings: &mut Vec<String>,
) -> bool {
    let Some(file) = read_agent_file(fs, path, load_warnings).await else {
        return false;
    };
    let key = normalize(&file.path);
    let identity = std::ptr::from_ref(fs) as *const () as usize;
    let seen = seen_by_fs.entry(identity).or_default();
    if !seen.insert(key) {
        return false;
    }
    discovered.push(file);
    true
}

async fn read_agent_file(
    fs: &dyn IHostFileSystem,
    path: &str,
    warnings: &mut Vec<String>,
) -> Option<AgentFile> {
    if !is_file(fs, path).await {
        if entry_exists(fs, path).await {
            warnings.push(format!(
                "Instruction file at {path} exists but is not a readable regular file; skipping."
            ));
        }
        return None;
    }
    let content = match fs.read_text(path, Some(ignore_decode_errors())).await {
        Ok(content) => trim_js(&content).to_string(),
        Err(_) => {
            warnings.push(format!(
                "Instruction file at {path} could not be read; skipping."
            ));
            return None;
        }
    };
    if content.is_empty() {
        return None;
    }
    Some(AgentFile {
        path: path.to_string(),
        content,
    })
}

async fn path_exists(fs: &dyn IHostFileSystem, path: &str) -> bool {
    fs.lstat(path).await.is_ok()
}

async fn entry_exists(fs: &dyn IHostFileSystem, path: &str) -> bool {
    path_exists(fs, path).await
}

async fn is_file(fs: &dyn IHostFileSystem, path: &str) -> bool {
    fs.stat(path)
        .await
        .map(|stat| stat.is_file)
        .unwrap_or(false)
}

fn render_agent_files(files: &[AgentFile]) -> String {
    if files.is_empty() {
        return String::new();
    }
    files
        .iter()
        .map(|file| format!("{}{}", annotation_for(&file.path), file.content))
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn byte_length(text: &str) -> usize {
    text.len()
}

fn format_kb(bytes: usize) -> String {
    if bytes % 1024 == 0 {
        (bytes / 1024).to_string()
    } else {
        format!("{:.1}", bytes as f64 / 1024.0)
    }
}

fn annotation_for(path: &str) -> String {
    format!("<!-- From: {path} -->\n")
}

fn dedupe_dirs(dirs: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for dir in dirs {
        let trimmed = trim_js(dir).to_string();
        if trimmed.is_empty() || seen.contains(&trimmed) {
            continue;
        }
        seen.insert(trimmed.clone());
        result.push(trimmed);
    }
    result
}

#[derive(Clone, Copy, Default)]
struct ListDirectoryOptions {
    collapse_hidden_dirs: bool,
}

#[derive(Debug, Clone)]
struct Entry {
    name: String,
    is_dir: bool,
}

struct CollectedEntries {
    entries: Vec<Entry>,
    total: usize,
    readable: bool,
}

async fn collect_entries(
    deps: &ProfileContextDeps<'_>,
    dir_path: &str,
    max_width: usize,
) -> CollectedEntries {
    let mut all = Vec::new();
    match deps.fs.readdir(dir_path).await {
        Ok(dirents) => {
            for entry in dirents {
                all.push(Entry {
                    name: entry.name,
                    is_dir: entry.is_directory,
                });
            }
        }
        Err(_) => {
            return CollectedEntries {
                entries: Vec::new(),
                total: 0,
                readable: false,
            };
        }
    }
    all.sort_by(|a, b| {
        if a.is_dir != b.is_dir {
            return if a.is_dir {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            };
        }
        a.name.cmp(&b.name)
    });
    let total = all.len();
    CollectedEntries {
        entries: all.into_iter().take(max_width).collect(),
        total,
        readable: true,
    }
}

fn should_collapse_directory(entry: &Entry, options: ListDirectoryOptions) -> bool {
    options.collapse_hidden_dirs && entry.is_dir && entry.name.starts_with('.')
}

async fn list_directory(
    deps: &ProfileContextDeps<'_>,
    work_dir: &str,
    options: ListDirectoryOptions,
) -> String {
    let mut lines = Vec::new();
    let CollectedEntries {
        entries,
        total,
        readable,
    } = collect_entries(deps, work_dir, LIST_DIR_ROOT_WIDTH).await;
    if !readable {
        return "[not readable]".to_string();
    }
    let remaining = total - entries.len();
    let children = join_all(entries.iter().map(|entry| async {
        if entry.is_dir && !should_collapse_directory(entry, options) {
            Some(
                collect_entries(
                    deps,
                    &join(&[work_dir, &entry.name]),
                    LIST_DIR_CHILD_WIDTH,
                )
                .await,
            )
        } else {
            None
        }
    }))
    .await;

    for (index, entry) in entries.iter().enumerate() {
        let is_last = index == entries.len() - 1 && remaining == 0;
        let connector = if is_last { "└── " } else { "├── " };
        if entry.is_dir {
            lines.push(format!("{}{}/", connector, entry.name));
            let Some(child) = children.get(index).and_then(|child| child.as_ref()) else {
                continue;
            };
            let child_prefix = if is_last { "    " } else { "│   " };
            if !child.readable {
                lines.push(format!("{child_prefix}└── [not readable]"));
                continue;
            }
            let child_remaining = child.total - child.entries.len();
            for (child_index, child_entry) in child.entries.iter().enumerate() {
                let child_is_last =
                    child_index == child.entries.len() - 1 && child_remaining == 0;
                let child_connector = if child_is_last { "└── " } else { "├── " };
                let suffix = if child_entry.is_dir { "/" } else { "" };
                lines.push(format!(
                    "{child_prefix}{child_connector}{}{suffix}",
                    child_entry.name
                ));
            }
            if child_remaining > 0 {
                lines.push(format!(
                    "{child_prefix}└── ... and {child_remaining} more"
                ));
            }
        } else {
            lines.push(format!("{}{}", connector, entry.name));
        }
    }
    if remaining > 0 {
        lines.push(format!("└── ... and {remaining} more entries"));
    }
    if lines.is_empty() {
        "(empty directory)".to_string()
    } else {
        lines.join("\n")
    }
}

fn ignore_decode_errors() -> HostReadTextOptions {
    HostReadTextOptions {
        errors: Some(TextDecodeErrors::Ignore),
        ..Default::default()
    }
}

fn normalize(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let path = normalize_windows_path(path);
    let unc = is_unc(&path);
    let absolute = is_absolute(&path);
    let trailing_separator = path.ends_with('/');
    let mut output = normalize_string(&path, !absolute);
    if output.is_empty() {
        return if absolute {
            "/".to_string()
        } else if trailing_separator {
            "./".to_string()
        } else {
            ".".to_string()
        };
    }
    if trailing_separator {
        output.push('/');
    }
    if is_drive_letter(&output) {
        output.push('/');
    }
    if unc {
        return if absolute {
            format!("//{output}")
        } else {
            format!("//./{output}")
        };
    }
    if absolute && !is_absolute(&output) {
        return format!("/{output}");
    }
    output
}

fn join(segments: &[&str]) -> String {
    let mut path = String::new();
    for segment in segments {
        if segment.is_empty() {
            continue;
        }
        if path.is_empty() {
            path.push_str(segment);
            continue;
        }
        let trailing = path.ends_with('/');
        let leading = segment.starts_with('/');
        if trailing && leading {
            path.push_str(&segment[1..]);
        } else if trailing || leading {
            path.push_str(segment);
        } else {
            path.push('/');
            path.push_str(segment);
        }
    }
    normalize(&path)
}

fn dirname(path: &str) -> String {
    let normalized = normalize_windows_path(path);
    let stripped = normalized.strip_suffix('/').unwrap_or(&normalized);
    let mut segments = stripped.split('/').collect::<Vec<_>>();
    segments.pop();
    if segments.len() == 1 && is_drive_letter(segments[0]) {
        return format!("{}/", segments[0]);
    }
    let output = segments.join("/");
    if !output.is_empty() {
        output
    } else if is_absolute(path) {
        "/".to_string()
    } else {
        ".".to_string()
    }
}

fn basename(path: &str) -> String {
    normalize_windows_path(path)
        .split('/')
        .rev()
        .find(|segment| !segment.is_empty())
        .unwrap_or_default()
        .to_string()
}

fn normalize_windows_path(input: &str) -> String {
    if input.is_empty() {
        return String::new();
    }
    let mut output = input.replace('\\', "/");
    let drive = {
        let bytes = output.as_bytes();
        if bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && bytes[2] == b'/'
        {
            Some(bytes[0].to_ascii_uppercase() as char)
        } else {
            None
        }
    };
    if let Some(drive) = drive {
        output.replace_range(..1, &drive.to_string());
    }
    output
}

fn is_unc(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 2 && is_separator(bytes[0]) && is_separator(bytes[1])
}

fn is_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    if is_separator(bytes[0]) {
        if bytes.len() == 1 || !is_separator(bytes[1]) {
            return true;
        }
        return bytes.len() == 2 || bytes[2] != b'.';
    }
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && is_separator(bytes[2])
}

fn is_separator(byte: u8) -> bool {
    matches!(byte, b'/' | b'\\')
}

fn is_drive_letter(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

fn normalize_string(path: &str, allow_above_root: bool) -> String {
    let bytes = path.as_bytes();
    let mut output = String::new();
    let mut last_segment_length = 0usize;
    let mut last_slash: isize = -1;
    let mut dots = 0i8;
    let mut previous = None;
    let mut index = 0usize;
    while index <= bytes.len() {
        let current = if index < bytes.len() {
            bytes[index]
        } else if previous == Some(b'/') {
            break;
        } else {
            b'/'
        };
        if current == b'/' {
            if last_slash == index as isize - 1 || dots == 1 {
            } else if dots == 2 {
                let ends_with_dotdot = last_segment_length == 2 && output.ends_with("..");
                if !ends_with_dotdot {
                    if output.chars().count() > 2 {
                        if let Some(slash_index) = output.rfind('/') {
                            output.truncate(slash_index);
                            last_segment_length = output
                                .rfind('/')
                                .map(|previous_slash| output.len() - previous_slash - 1)
                                .unwrap_or(output.len());
                        } else {
                            output.clear();
                            last_segment_length = 0;
                        }
                        last_slash = index as isize;
                        dots = 0;
                        index += 1;
                        previous = Some(current);
                        continue;
                    } else if !output.is_empty() {
                        output.clear();
                        last_segment_length = 0;
                        last_slash = index as isize;
                        dots = 0;
                        index += 1;
                        previous = Some(current);
                        continue;
                    }
                }
                if allow_above_root {
                    if output.is_empty() {
                        output.push_str("..");
                    } else {
                        output.push_str("/..");
                    }
                    last_segment_length = 2;
                }
            } else {
                let start = (last_slash + 1) as usize;
                let segment = &path[start..index];
                if output.is_empty() {
                    output.push_str(segment);
                } else {
                    output.push('/');
                    output.push_str(segment);
                }
                last_segment_length = index - start;
            }
            last_slash = index as isize;
            dots = 0;
        } else if current == b'.' && dots != -1 {
            dots += 1;
        } else {
            dots = -1;
        }
        previous = Some(current);
        index += 1;
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
