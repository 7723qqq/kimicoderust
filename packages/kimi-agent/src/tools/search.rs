use super::*;

impl NativeToolset {
    pub(super) fn grep(
        sandbox: &Sandbox,
        bridge: &ShellPathBridge,
        args: &Value,
    ) -> Option<ExecutableToolResult> {
        let Some(pattern) = args.get("pattern").and_then(Value::as_str) else {
            return Some(err_result(
                LocalizedText::new("engine.tools.grep.patternRequired").render(),
            ));
        };

        let type_filter = match args.get("type") {
            None | Some(Value::Null) => None,
            Some(value) => {
                let Some(name) = value.as_str() else {
                    return Some(err_result(
                        LocalizedText::with_params(
                            "engine.tools.grep.typeInvalid",
                            i18n_params!["value" => value],
                        )
                        .render(),
                    ));
                };
                let Some(globs) = grep_types::rg_type_globs(name) else {
                    return Some(err_result(
                        LocalizedText::with_params(
                            "engine.tools.grep.unrecognizedFileType",
                            i18n_params!["name" => name],
                        )
                        .render(),
                    ));
                };
                let Some(glob) = build_type_glob(globs) else {
                    return Some(err_result(
                        LocalizedText::with_params(
                            "engine.tools.grep.invalidFileTypeGlob",
                            i18n_params!["name" => name],
                        )
                        .render(),
                    ));
                };
                Some(glob)
            }
        };

        let multiline = match bool_arg(args, "multiline", false) {
            Ok(value) => value,
            Err(message) => return Some(err_result(message)),
        };

        let include_ignored = match bool_arg(args, "include_ignored", false) {
            Ok(value) => value,
            Err(message) => return Some(err_result(message)),
        };
        let case_insensitive = match bool_arg(args, "-i", false) {
            Ok(value) => value,
            Err(message) => return Some(err_result(message)),
        };
        let line_numbers = match bool_arg(args, "-n", true) {
            Ok(value) => value,
            Err(message) => return Some(err_result(message)),
        };
        let output_mode = match args.get("output_mode") {
            None | Some(Value::Null) => "files_with_matches",
            Some(value) => match value.as_str() {
                Some(mode @ ("files_with_matches" | "content" | "count_matches")) => mode,
                _ => {
                    return Some(err_result(
                        LocalizedText::with_params(
                            "engine.tools.grep.outputModeInvalid",
                            i18n_params!["value" => value],
                        )
                        .render(),
                    ));
                }
            },
        };
        let context_both = match u64_arg(args, "-C", 0) {
            Ok(value) => value as usize,
            Err(message) => return Some(err_result(message)),
        };
        let context_after = match u64_arg(args, "-A", 0) {
            Ok(value) => (value as usize).max(context_both),
            Err(message) => return Some(err_result(message)),
        };
        let context_before = match u64_arg(args, "-B", 0) {
            Ok(value) => (value as usize).max(context_both),
            Err(message) => return Some(err_result(message)),
        };
        let head_limit = match u64_arg(args, "head_limit", GREP_HEAD_LIMIT as u64) {
            Ok(value) => value as usize,
            Err(message) => return Some(err_result(message)),
        };
        let page_offset = match u64_arg(args, "offset", 0) {
            Ok(value) => value as usize,
            Err(message) => return Some(err_result(message)),
        };

        let mut builder = regex::RegexBuilder::new(pattern);
        builder.case_insensitive(case_insensitive);

        builder.dot_matches_new_line(multiline);

        builder.multi_line(multiline);
        let regex = match builder.build() {
            Ok(r) => r,
            Err(e) => {
                return Some(err_result(
                    LocalizedText::with_params(
                        "engine.tools.grep.invalidRegex",
                        i18n_params!["e" => e],
                    )
                    .render(),
                ));
            }
        };
        let glob_filter = match args.get("glob") {
            None | Some(Value::Null) => None,
            Some(value) => {
                let Some(pattern) = value.as_str() else {
                    return Some(err_result(
                        LocalizedText::with_params(
                            "engine.tools.grep.globInvalid",
                            i18n_params!["value" => value],
                        )
                        .render(),
                    ));
                };
                let Some(glob) = build_glob(pattern) else {
                    return Some(err_result(
                        LocalizedText::with_params(
                            "engine.tools.grep.invalidGlobPattern",
                            i18n_params!["pattern" => pattern],
                        )
                        .render(),
                    ));
                };
                Some(glob)
            }
        };

        let search_root = match args.get("path") {
            None | Some(Value::Null) => sandbox.primary().to_path_buf(),
            Some(value) => {
                let Some(path) = value.as_str() else {
                    return Some(err_result(
                        LocalizedText::new("engine.tools.grep.pathNotString").render(),
                    ));
                };

                if let Some(refusal) = Self::file_access_refusal(
                    sandbox,
                    bridge,
                    path,
                    crate::native::path_access::PathAccessOperation::Search,
                    &crate::native::path_access::WorkspaceAccessPolicy::SEARCH,
                ) {
                    return Some(err_result(refusal));
                }
                let Some(resolved) = Self::resolve(sandbox, bridge, path) else {
                    return Some(err_result(
                        LocalizedText::with_params(
                            "engine.tools.grep.pathNotExist",
                            i18n_params!["path" => path],
                        )
                        .render(),
                    ));
                };
                resolved
            }
        };

        let mode = match output_mode {
            "files_with_matches" => GrepMode::FilesWithMatches,
            "count_matches" => GrepMode::CountMatches,
            _ => GrepMode::Content,
        };
        let scan_cfg = GrepScanConfig {
            regex: &regex,
            mode,
            context_before,
            context_after,
            line_numbers,
            multiline,
        };

        let deadline = Instant::now() + GREP_TIME_BUDGET;
        let GrepCollected {
            mut per_file,
            mut filtered_sensitive,
            unreadable,
            walk_errors,
            lossy_utf8,
            timed_out,
        } = grep_collect(
            &search_root,
            sandbox.primary(),
            &scan_cfg,
            glob_filter.as_ref(),
            type_filter.as_ref(),
            include_ignored,
            GrepWalkLimits { deadline },
        );

        filtered_sensitive.sort();

        grep_sort(&mut per_file, mode);

        let mut rendered: Vec<String> = Vec::new();
        match mode {
            GrepMode::FilesWithMatches => {
                for file in &per_file {
                    rendered.push(file.display.clone());
                }
            }
            GrepMode::CountMatches => {
                for file in &per_file {
                    rendered.push(format!("{}:{}", file.display, file.total_matches));
                }
            }
            GrepMode::Content => {
                for file in &per_file {
                    rendered.extend(file.rendered.iter().cloned());
                }
            }
        }

        let mut output_truncated = false;
        if rendered.iter().map(|l| l.len() + 1).sum::<usize>() > GREP_MAX_OUTPUT_BYTES {
            let mut bytes = 0usize;
            let mut keep = 0usize;
            for line in &rendered {
                let line_bytes = line.len() + 1;
                if bytes + line_bytes > GREP_MAX_OUTPUT_BYTES {
                    break;
                }
                bytes += line_bytes;
                keep += 1;
            }
            rendered.truncate(keep);
            output_truncated = true;
        }

        let after_offset: Vec<String> = if page_offset > 0 {
            rendered.into_iter().skip(page_offset).collect()
        } else {
            rendered
        };
        let limited: Vec<String> = if head_limit > 0 {
            after_offset.iter().take(head_limit).cloned().collect()
        } else {
            after_offset.clone()
        };
        let pagination_truncated = head_limit > 0 && after_offset.len() > head_limit;

        let mut out = if limited.is_empty() {
            if !filtered_sensitive.is_empty() {
                LocalizedText::new("engine.tools.grep.noNonSensitive").render()
            } else {
                LocalizedText::with_params(
                    "engine.tools.grep.noMatches",
                    i18n_params!["pattern" => pattern],
                )
                .render()
            }
        } else {
            limited.join("\n")
        };

        let mut headers: Vec<String> = Vec::new();
        let mut messages: Vec<String> = Vec::new();
        if output_mode == "count_matches" && !per_file.is_empty() {
            let total_occurrences: usize = per_file.iter().map(|f| f.total_matches).sum();
            let occurrence_word = if total_occurrences == 1 {
                "occurrence"
            } else {
                "occurrences"
            };
            let file_word = if per_file.len() == 1 { "file" } else { "files" };
            let scope = if filtered_sensitive.is_empty() {
                "total"
            } else {
                "total non-sensitive"
            };
            headers.push(
                LocalizedText::with_params(
                    "engine.tools.grep.foundAcross",
                    i18n_params![
                        "total_occurrences" => total_occurrences,
                        "scope" => scope,
                        "occurrence_word" => occurrence_word,
                        "files" => per_file.len(),
                        "file_word" => file_word,
                    ],
                )
                .render(),
            );
        }
        if pagination_truncated {
            let total = after_offset.len() + page_offset;
            let next_offset = page_offset + head_limit;
            let notice = format!(
                "Results truncated to {head_limit} lines (total: {total}). Use offset={next_offset} to see more."
            );
            if output_mode == "count_matches" {
                headers.push(notice);
            } else {
                messages.push(notice);
            }
        }
        if output_truncated {
            messages.push(format!(
                "[Output truncated at {GREP_MAX_OUTPUT_BYTES} bytes — the result set is incomplete. Narrow the pattern, path, or glob filters and re-run to recover complete results.]"
            ));
        }
        if timed_out {
            messages.push(format!(
                "Grep timed out after {}s; partial results returned. Narrow the path, glob, or pattern and retry for complete results.",
                GREP_TIME_BUDGET.as_secs()
            ));
        }
        if !filtered_sensitive.is_empty() {
            messages.push(
                LocalizedText::with_params(
                    "engine.tools.grep.filteredSensitiveWithList",
                    i18n_params![
                        "count" => filtered_sensitive.len(),
                        "list" => filtered_sensitive.join(", "),
                    ],
                )
                .render(),
            );
        }

        if !unreadable.is_empty() {
            messages.push(
                LocalizedText::with_params(
                    "engine.tools.grep.unreadable",
                    i18n_params![
                        "count" => unreadable.len(),
                        "list" => unreadable.join(", "),
                    ],
                )
                .render(),
            );
        }
        if walk_errors > 0 {
            messages.push(
                LocalizedText::with_params(
                    "engine.tools.grep.walkErrors",
                    i18n_params!["count" => walk_errors],
                )
                .render(),
            );
        }
        if lossy_utf8 > 0 {
            messages.push(
                LocalizedText::with_params(
                    "engine.tools.grep.lossyUtf8",
                    i18n_params!["count" => lossy_utf8],
                )
                .render(),
            );
        }

        if !headers.is_empty() {
            out = format!("{}\n{out}", headers.join("\n"));
        }
        if !messages.is_empty() {
            out.push_str(&format!("\n\n{}", messages.join("\n")));
        }
        Some(ok_result(out))
    }

    pub(super) fn glob(
        sandbox: &Sandbox,
        bridge: &ShellPathBridge,
        args: &Value,
    ) -> Option<ExecutableToolResult> {
        let Some(pattern) = args.get("pattern").and_then(Value::as_str) else {
            return Some(err_result(
                LocalizedText::new("engine.tools.glob.patternRequired").render(),
            ));
        };

        let include_ignored = match bool_arg(args, "include_ignored", false) {
            Ok(value) => value,
            Err(message) => return Some(err_result(message)),
        };
        let head_limit = match u64_arg(args, "head_limit", 100) {
            Ok(0) => usize::MAX,
            Ok(n) => n as usize,
            Err(message) => return Some(err_result(message)),
        };
        let offset = match u64_arg(args, "offset", 0) {
            Ok(value) => value as usize,
            Err(message) => return Some(err_result(message)),
        };
        let Some(glob) = build_glob(pattern) else {
            return Some(err_result(format!("invalid glob pattern: {pattern}")));
        };
        let search_root = match args.get("path").and_then(|p| p.as_str()) {
            Some(p) => {
                if let Some(refusal) = Self::file_access_refusal(
                    sandbox,
                    bridge,
                    p,
                    crate::native::path_access::PathAccessOperation::Search,
                    &crate::native::path_access::WorkspaceAccessPolicy::SEARCH,
                ) {
                    return Some(err_result(refusal));
                }
                let Some(resolved) = Self::resolve(sandbox, bridge, p) else {
                    return Some(err_result(
                        LocalizedText::with_params(
                            "engine.tools.glob.pathNotExist",
                            i18n_params!["p" => p],
                        )
                        .render(),
                    ));
                };
                resolved
            }
            None => sandbox.primary().to_path_buf(),
        };

        let mut results: Vec<(String, Option<std::time::SystemTime>)> = Vec::new();
        let mut filtered_sensitive: usize = 0;

        let mut builder = ignore::WalkBuilder::new(&search_root);
        builder.hidden(false);
        if include_ignored {
            builder
                .ignore(false)
                .git_ignore(false)
                .git_global(false)
                .git_exclude(false)
                .parents(false);
        }
        let walker = builder.build();

        let deadline = Instant::now() + GREP_TIME_BUDGET;
        let mut visited = 0usize;
        let mut walk_errors = 0usize;
        let mut walk_truncated = false;
        for entry in walker {
            if Instant::now() >= deadline {
                walk_truncated = true;
                break;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    walk_errors += 1;
                    continue;
                }
            };
            visited += 1;
            let path = entry.path();

            let relative = path.strip_prefix(&search_root).unwrap_or(path);
            if relative.components().any(|c| {
                matches!(
                    c.as_os_str().to_str(),
                    Some(name) if VCS_DIRECTORIES_TO_EXCLUDE.contains(&name)
                )
            }) {
                continue;
            }
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            if glob.is_match(relative) || glob.is_match(path) {
                if crate::native::file_type::is_sensitive_file(&path.to_string_lossy()) {
                    filtered_sensitive += 1;
                    continue;
                }

                let display = path.strip_prefix(&search_root).unwrap_or(path);
                let mtime = entry.metadata().ok().and_then(|m| m.modified().ok());
                results.push((display.display().to_string(), mtime));
            }
        }

        results.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

        let total = results.len();
        let paged: Vec<String> = results
            .into_iter()
            .skip(offset)
            .take(head_limit)
            .map(|(display, _)| display)
            .collect();
        let count = paged.len();
        let truncated = offset + count < total;

        let mut lines: Vec<String> = Vec::new();
        let mut footer: Vec<String> = Vec::new();

        if count == 0 {
            if total > 0 {
                lines.push(
                    LocalizedText::with_params(
                        "engine.tools.grep.noMoreMatches",
                        i18n_params!["offset" => offset, "total" => total],
                    )
                    .render(),
                );
            } else if filtered_sensitive > 0 {
                lines.push(
                    LocalizedText::with_params(
                        "engine.tools.grep.noNonSensitiveFiltered",
                        i18n_params!["filtered_sensitive" => filtered_sensitive],
                    )
                    .render(),
                );
            } else {
                lines.push(
                    LocalizedText::with_params(
                        "engine.tools.grep.noFilesMatched",
                        i18n_params!["pattern" => pattern],
                    )
                    .render(),
                );
            }
        } else {
            if truncated || offset > 0 {
                lines.push(
                    LocalizedText::with_params(
                        "engine.tools.glob.showingMatches",
                        i18n_params!["from" => offset + 1, "to" => offset + count, "total" => total],
                    )
                    .render(),
                );
            }
            lines.extend(paged);
            if truncated {
                lines.push(format!(
                    "Continue with the same search arguments and offset={}.",
                    offset + count
                ));
                lines.push(
                    "To remove the match-count limit, omit offset and use head_limit=0.".into(),
                );
            }
        }
        if filtered_sensitive > 0 && total > 0 {
            footer.push(
                LocalizedText::with_params(
                    "engine.tools.grep.filteredSensitive",
                    i18n_params!["filtered_sensitive" => filtered_sensitive],
                )
                .render(),
            );
        }

        if walk_errors > 0 {
            footer.push(
                LocalizedText::with_params(
                    "engine.tools.glob.walkErrors",
                    i18n_params!["count" => walk_errors],
                )
                .render(),
            );
        }
        if walk_truncated {
            footer.push(
                LocalizedText::with_params(
                    "engine.tools.glob.walkTruncated",
                    i18n_params![
                        "entries" => visited,
                        "seconds" => GREP_TIME_BUDGET.as_secs()
                    ],
                )
                .render(),
            );
        }

        let out = [lines, footer].concat().join("\n");
        Some(ok_result(out))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum GrepMode {
    FilesWithMatches,
    CountMatches,
    Content,
}

#[derive(Clone, Copy)]
pub(super) struct GrepScanConfig<'a> {
    pub(super) regex: &'a regex::Regex,
    pub(super) mode: GrepMode,
    pub(super) context_before: usize,
    pub(super) context_after: usize,
    pub(super) line_numbers: bool,

    pub(super) multiline: bool,
}

pub(super) struct FileScan {
    pub(super) display: String,

    pub(super) mtime: u64,
    pub(super) total_matches: usize,
    pub(super) rendered: Vec<String>,

    pub(super) lossy_utf8: bool,
}

pub(super) fn mtime_secs(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

pub(super) fn grep_sort(per_file: &mut [FileScan], mode: GrepMode) {
    match mode {
        GrepMode::FilesWithMatches => per_file.sort_by(|a, b| {
            b.mtime
                .cmp(&a.mtime)
                .then_with(|| a.display.cmp(&b.display))
        }),
        GrepMode::CountMatches | GrepMode::Content => {
            per_file.sort_by(|a, b| a.display.cmp(&b.display));
        }
    }
}

pub(super) fn grep_walk_threads() -> usize {
    std::thread::available_parallelism()
        .map(|cpus| cpus.get())
        .unwrap_or(1)
        .clamp(1, GREP_WALK_THREADS)
}

pub(super) enum ScanOutcome {
    Skip,

    Unreadable,

    Match(FileScan),
}

pub(super) struct GrepCollected {
    pub(super) per_file: Vec<FileScan>,
    pub(super) filtered_sensitive: Vec<String>,
    pub(super) timed_out: bool,

    pub(super) unreadable: Vec<String>,

    pub(super) walk_errors: usize,

    pub(super) lossy_utf8: usize,
}

#[derive(Clone, Copy)]
pub(super) struct GrepWalkLimits {
    pub(super) deadline: Instant,
}

pub(super) fn grep_collect(
    search_root: &Path,
    root: &Path,
    cfg: &GrepScanConfig,
    glob_filter: Option<&globset::GlobSet>,
    type_filter: Option<&globset::GlobSet>,
    include_ignored: bool,
    limits: GrepWalkLimits,
) -> GrepCollected {
    use ignore::WalkState;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    let collected: Arc<Mutex<Vec<FileScan>>> = Arc::new(Mutex::new(Vec::new()));
    let sensitive: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let unreadable: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let timed_out = Arc::new(AtomicBool::new(false));
    let walk_errors = Arc::new(AtomicUsize::new(0));
    let lossy_files = Arc::new(AtomicUsize::new(0));

    let collected_walk = Arc::clone(&collected);
    let sensitive_walk = Arc::clone(&sensitive);
    let unreadable_walk = Arc::clone(&unreadable);
    let timed_walk = Arc::clone(&timed_out);
    let walk_errors_walk = Arc::clone(&walk_errors);
    let lossy_files_walk = Arc::clone(&lossy_files);
    let cfg_copy = *cfg;
    let search_root_owned = search_root.to_path_buf();

    let mut builder = ignore::WalkBuilder::new(search_root);
    builder.hidden(false);
    builder.threads(grep_walk_threads());
    if include_ignored {
        builder
            .ignore(false)
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false)
            .parents(false);
    }
    builder.build_parallel().run(move || {
        let collected = Arc::clone(&collected_walk);
        let sensitive = Arc::clone(&sensitive_walk);
        let unreadable = Arc::clone(&unreadable_walk);
        let timed_out = Arc::clone(&timed_walk);
        let walk_errors = Arc::clone(&walk_errors_walk);
        let lossy_files = Arc::clone(&lossy_files_walk);
        let walk_root = search_root_owned.clone();
        Box::new(move |entry: Result<ignore::DirEntry, ignore::Error>| {
            if Instant::now() >= limits.deadline {
                timed_out.store(true, Ordering::Relaxed);
                return WalkState::Quit;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    walk_errors.fetch_add(1, Ordering::Relaxed);
                    return WalkState::Continue;
                }
            };
            let path = entry.path();

            let relative = path.strip_prefix(&walk_root).unwrap_or(path);
            if relative.components().any(|c| {
                matches!(
                    c.as_os_str().to_str(),
                    Some(name) if VCS_DIRECTORIES_TO_EXCLUDE.contains(&name)
                )
            }) {
                return WalkState::Continue;
            }
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                return WalkState::Continue;
            }
            if let Some(gs) = glob_filter
                && !gs.is_match(path)
            {
                return WalkState::Continue;
            }

            if let Some(ts) = type_filter
                && !path.file_name().is_some_and(|name| ts.is_match(name))
            {
                return WalkState::Continue;
            }

            if crate::native::file_type::is_sensitive_file(&path.to_string_lossy()) {
                let display = path.strip_prefix(root).unwrap_or(path);
                sensitive
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(display.display().to_string());
                return WalkState::Continue;
            }
            let outcome = if cfg_copy.multiline {
                scan_grep_file_multiline(path, root, &cfg_copy)
            } else {
                scan_grep_file(path, root, &cfg_copy)
            };
            match outcome {
                ScanOutcome::Unreadable => {
                    unreadable.lock().unwrap_or_else(|e| e.into_inner()).push(
                        path.strip_prefix(root)
                            .unwrap_or(path)
                            .display()
                            .to_string(),
                    )
                }
                ScanOutcome::Match(file) => {
                    if file.lossy_utf8 {
                        lossy_files.fetch_add(1, Ordering::Relaxed);
                    }
                    collected
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(file)
                }
                ScanOutcome::Skip => {}
            }
            WalkState::Continue
        })
    });

    GrepCollected {
        per_file: collected
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect(),
        filtered_sensitive: sensitive
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect(),
        unreadable: unreadable
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect(),
        walk_errors: walk_errors.load(Ordering::Relaxed),
        lossy_utf8: lossy_files.load(Ordering::Relaxed),
        timed_out: timed_out.load(Ordering::Relaxed),
    }
}

pub(super) fn scan_grep_file(path: &Path, root: &Path, cfg: &GrepScanConfig) -> ScanOutcome {
    let Ok(file) = std::fs::File::open(path) else {
        return ScanOutcome::Unreadable;
    };

    let Ok(meta) = file.metadata() else {
        return ScanOutcome::Unreadable;
    };
    let mtime = mtime_secs(&meta);
    let display = path
        .strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string();
    scan_grep_lines(std::io::BufReader::new(file), cfg, display, mtime)
}

pub(super) fn scan_grep_lines<R: std::io::BufRead>(
    mut reader: R,
    cfg: &GrepScanConfig,
    display: String,
    mtime: u64,
) -> ScanOutcome {
    let content = cfg.mode == GrepMode::Content;

    let only_need_presence = cfg.mode == GrepMode::FilesWithMatches;

    let mut buf: Vec<u8> = Vec::new();
    let mut total_matches = 0usize;
    let mut has_match = false;
    let mut idx = 0usize;
    let mut lossy = false;

    let mut lookback: std::collections::VecDeque<(usize, usize, String, bool)> =
        std::collections::VecDeque::new();
    let mut cluster: Vec<(usize, usize, String, bool)> = Vec::new();
    let mut cluster_hi = 0usize;
    let mut active = false;
    let mut rendered: Vec<String> = Vec::new();

    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) => break,
            Ok(_) => {}

            Err(_) => return ScanOutcome::Unreadable,
        }
        let terminated = buf.last() == Some(&b'\n');
        if terminated {
            buf.pop();
        }

        if buf.contains(&0) {
            return ScanOutcome::Skip;
        }

        if terminated && buf.last() == Some(&b'\r') {
            buf.pop();
        }
        if !lossy && std::str::from_utf8(&buf).is_err() {
            lossy = true;
        }
        let seg = String::from_utf8_lossy(&buf);
        let trimmed: &str = seg.strip_suffix('\r').unwrap_or(&seg);
        let lineno = idx + 1;
        let matches_in_line = if only_need_presence && has_match {
            0
        } else {
            cfg.regex.find_iter(trimmed).count()
        };
        if matches_in_line > 0 {
            total_matches += matches_in_line;
            has_match = true;
        }
        let is_match = matches_in_line > 0;

        if content {
            if is_match {
                let lo = idx.saturating_sub(cfg.context_before);
                let hi = idx + cfg.context_after;
                if active && lo <= cluster_hi + 1 {
                    let cluster_end = cluster.last().map_or(lo, |c| c.0);
                    for lb in &lookback {
                        if lb.0 > cluster_end && lb.0 < idx {
                            cluster.push(lb.clone());
                        }
                    }
                    cluster.push((idx, lineno, trimmed.to_string(), true));
                    if hi > cluster_hi {
                        cluster_hi = hi;
                    }
                } else {
                    if active {
                        flush_grep_cluster(&mut rendered, &cluster, &display, cfg.line_numbers);
                        rendered.push("--".into());
                        cluster.clear();
                    }
                    active = true;
                    for lb in &lookback {
                        if lb.0 >= lo {
                            cluster.push(lb.clone());
                        }
                    }
                    cluster.push((idx, lineno, trimmed.to_string(), true));
                    cluster_hi = hi;
                }
            } else if active && idx <= cluster_hi {
                cluster.push((idx, lineno, trimmed.to_string(), false));
            }

            if cfg.context_before > 0 {
                lookback.push_back((idx, lineno, trimmed.to_string(), is_match));
                while lookback.len() > cfg.context_before {
                    lookback.pop_front();
                }
            }
        }
        idx += 1;
    }

    if !has_match {
        return ScanOutcome::Skip;
    }
    if content && active {
        flush_grep_cluster(&mut rendered, &cluster, &display, cfg.line_numbers);
    }
    ScanOutcome::Match(FileScan {
        display,
        mtime,
        total_matches,
        rendered,
        lossy_utf8: lossy,
    })
}

pub(super) fn scan_grep_file_multiline(
    path: &Path,
    root: &Path,
    cfg: &GrepScanConfig,
) -> ScanOutcome {
    use std::io::Read;

    let Ok(mut file) = std::fs::File::open(path) else {
        return ScanOutcome::Unreadable;
    };
    let Ok(meta) = file.metadata() else {
        return ScanOutcome::Unreadable;
    };
    let mtime = mtime_secs(&meta);
    let display = path
        .strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string();

    let mut raw: Vec<u8> = Vec::with_capacity(meta.len() as usize);
    if file.read_to_end(&mut raw).is_err() {
        return ScanOutcome::Unreadable;
    }

    if raw.contains(&0) {
        return ScanOutcome::Skip;
    }
    let lossy = std::str::from_utf8(&raw).is_err();
    let content = String::from_utf8_lossy(&raw);

    let mut lines: Vec<&str> = Vec::new();
    let mut seg_start = 0usize;
    for (i, &b) in content.as_bytes().iter().enumerate() {
        if b == b'\n' {
            let seg = &content[seg_start..i];
            lines.push(seg.strip_suffix('\r').unwrap_or(seg));
            seg_start = i + 1;
        }
    }
    if seg_start < content.len() {
        let seg = &content[seg_start..];
        lines.push(seg.strip_suffix('\r').unwrap_or(seg));
    }

    let only_need_presence = cfg.mode == GrepMode::FilesWithMatches;
    let mark_lines = cfg.mode == GrepMode::Content;
    let mut is_match_line = vec![false; lines.len()];
    let last_idx = lines.len().saturating_sub(1);

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

    let line_count = lines.len();
    let mut total_matches = 0usize;
    for m in cfg.regex.find_iter(&content) {
        if m.start() == m.end() && m.start() >= content.len() {
            let sole_match = total_matches == 0;
            let dangling_last_line = !content.is_empty() && !content.ends_with('\n');
            if !sole_match || !dangling_last_line {
                continue;
            }
            total_matches += 1;
            if mark_lines && line_count > 0 {
                is_match_line[last_idx] = true;
            }
            continue;
        }
        total_matches += 1;
        if only_need_presence {
            break;
        }
        if mark_lines && line_count > 0 {
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
    }
    if total_matches == 0 {
        return ScanOutcome::Skip;
    }

    let mut rendered: Vec<String> = Vec::new();
    if mark_lines {
        let mut lookback: std::collections::VecDeque<(usize, usize, String, bool)> =
            std::collections::VecDeque::new();
        let mut cluster: Vec<(usize, usize, String, bool)> = Vec::new();
        let mut cluster_hi = 0usize;
        let mut active = false;
        for (idx, text) in lines.iter().enumerate() {
            let lineno = idx + 1;
            let is_match = is_match_line[idx];
            if is_match {
                let lo = idx.saturating_sub(cfg.context_before);
                let hi = idx + cfg.context_after;
                if active && lo <= cluster_hi + 1 {
                    let cluster_end = cluster.last().map_or(lo, |c| c.0);
                    for lb in &lookback {
                        if lb.0 > cluster_end && lb.0 < idx {
                            cluster.push(lb.clone());
                        }
                    }
                    cluster.push((idx, lineno, (*text).to_string(), true));
                    if hi > cluster_hi {
                        cluster_hi = hi;
                    }
                } else {
                    if active {
                        flush_grep_cluster(&mut rendered, &cluster, &display, cfg.line_numbers);
                        rendered.push("--".into());
                        cluster.clear();
                    }
                    active = true;
                    for lb in &lookback {
                        if lb.0 >= lo {
                            cluster.push(lb.clone());
                        }
                    }
                    cluster.push((idx, lineno, (*text).to_string(), true));
                    cluster_hi = hi;
                }
            } else if active && idx <= cluster_hi {
                cluster.push((idx, lineno, (*text).to_string(), false));
            }
            if cfg.context_before > 0 {
                lookback.push_back((idx, lineno, (*text).to_string(), is_match));
                while lookback.len() > cfg.context_before {
                    lookback.pop_front();
                }
            }
        }
        if active {
            flush_grep_cluster(&mut rendered, &cluster, &display, cfg.line_numbers);
        }
    }

    ScanOutcome::Match(FileScan {
        display,
        mtime,
        total_matches,
        rendered,
        lossy_utf8: lossy,
    })
}

pub(super) fn flush_grep_cluster(
    rendered: &mut Vec<String>,
    cluster: &[(usize, usize, String, bool)],
    display: &str,
    line_numbers: bool,
) {
    for (_, lineno, text, is_match) in cluster {
        let sep = if *is_match { ':' } else { '-' };
        if line_numbers {
            rendered.push(format!("{display}{sep}{lineno}{sep}{text}"));
        } else {
            rendered.push(format!("{display}{sep}{text}"));
        }
    }
}

pub(super) fn build_glob(pattern: &str) -> Option<globset::GlobSet> {
    let mut builder = globset::GlobSetBuilder::new();
    builder.add(globset::Glob::new(pattern).ok()?);
    if !pattern.starts_with("**/") && !pattern.contains('/') && !pattern.contains('\\') {
        builder.add(globset::Glob::new(&format!("**/{pattern}")).ok()?);
    }
    builder.build().ok()
}

pub(super) fn build_type_glob(globs: &[&str]) -> Option<globset::GlobSet> {
    let mut builder = globset::GlobSetBuilder::new();
    for g in globs {
        builder.add(globset::Glob::new(g).ok()?);
    }
    builder.build().ok()
}
