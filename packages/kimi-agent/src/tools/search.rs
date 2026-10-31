use super::*;

impl NativeToolset {
    // ── Grep ───────────────────────────────────────────────────────────

    /// Native Grep mirroring the host tool's public contract: default mode
    /// `files_with_matches` (most-recently-modified first), `content` with
    /// `-n`/`-A`/`-B`/`-C` context, `count_matches` with an aggregate
    /// summary, case-insensitive matching, offset/head_limit paging, plus the
    /// `type` / `include_ignored` / `multiline` ripgrep features the host
    /// otherwise owns. Each maps to the exact host rg flag: `type` -> `--type`,
    /// `include_ignored` -> `--no-ignore`, `multiline` -> `-U
    /// --multiline-dotall`.
    pub(super) fn grep(
        sandbox: &Sandbox,
        bridge: &ShellPathBridge,
        args: &Value,
    ) -> Option<ExecutableToolResult> {
        // Argument typing is strict on purpose: the engine short-circuits ahead
        // of the host's zod validation, so a present-but-mistyped argument is
        // reported here rather than silently dropped and reported as a
        // successful search. It used to return `None` for the host to report,
        // but the native harness has no host tool runtime — the model saw
        // `tool "Grep" is host-owned and not yet wired on the native harness`
        // instead of the argument that was wrong. Absent and explicit `null`
        // both mean "the schema default".
        let Some(pattern) = args.get("pattern").and_then(Value::as_str) else {
            return Some(err_result(
                LocalizedText::new("engine.tools.grep.patternRequired").render(),
            ));
        };
        // `type` -> rg `--type NAME`: restrict the walk to files whose basename
        // matches the type's globs. [`grep_types::RG_FILE_TYPES`] is only a
        // fast path transcribed from one rg release — the host runs whatever rg
        // is on PATH and honours user `--type-add` definitions (`.ripgreprc`),
        // so an unknown name is reported the way rg reports it rather than
        // silently searching everything.
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
        // `multiline` -> rg `-U --multiline-dotall`: the pattern may span
        // newlines and `.` also matches `\n`. Matching crosses line boundaries,
        // so the scan buffers the whole file (a separate path, still bounded by
        // GREP_MAX_FILE_BYTES and the binary-skip contract).
        let multiline = match bool_arg(args, "multiline", false) {
            Ok(value) => value,
            Err(message) => return Some(err_result(message)),
        };
        // `include_ignored` -> rg `--no-ignore`: don't respect ignore files
        // (.gitignore/.ignore/.rgignore and friends). VCS metadata dirs and
        // sensitive files stay filtered regardless.
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
        // rg `--multiline-dotall` makes `.` match `\n` (only meaningful with
        // `-U`, which the multiline scan path provides).
        builder.dot_matches_new_line(multiline);
        // rg also keeps `^`/`$` anchored per line in `-U` mode (ripgrep 15.0.0:
        // `rg -U --count-matches '^'` on "a\nb\n" reports 2), while the
        // streaming path anchors per line by construction.
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
                // A path the engine cannot resolve is a tool error, not a call
                // it cannot serve: returning `None` forwarded it to a host with
                // no file-tool runtime, and the model saw `tool "Grep" is
                // host-owned and not yet wired on the native harness`.
                //
                // v2 `grepTool.ts:94-98` runs the same gate first, with
                // `operation: 'search'` and an explicit
                // `checkSensitive: false` — Grep filters sensitive hits out of
                // its *results* (`filtered_sensitive` below) rather than
                // refusing the call, so only the out-of-workspace arm applies.
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
        // The wall-clock budget becomes a deadline instant the parallel
        // workers compare against (replacing the old serial `elapsed()` check).
        let deadline = Instant::now() + GREP_TIME_BUDGET;
        let GrepCollected {
            mut per_file,
            mut filtered_sensitive,
            timed_out,
            mut file_cap_truncated,
        } = grep_collect(
            &search_root,
            sandbox.primary(),
            &scan_cfg,
            glob_filter.as_ref(),
            type_filter.as_ref(),
            include_ignored,
            GrepWalkLimits {
                deadline,
                scan_cap: GREP_WALK_SCAN_CAP,
            },
        );
        // The walk is unordered; make the sensitive-file notice deterministic.
        filtered_sensitive.sort();
        // Ordering first, hard file cap second: the parallel walk cannot stop
        // at an exact global count, so truncating the unordered aggregate would
        // keep a scheduling-dependent subset — and `head_limit` then pages over
        // whatever that subset happened to be.
        file_cap_truncated |= grep_sort_and_cap(&mut per_file, mode, GREP_MAX_FILES);

        // Rendered output lines, then offset/head_limit paging (host order).
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
                // Each file's merged `-A`/`-B`/`-C` windows and `--` cluster
                // separators were rendered during the parallel scan; splice
                // them together in the deterministic file order.
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
                let line_bytes = line.len() + 1; // trailing \n separator
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

        if !headers.is_empty() {
            out = format!("{}\n{out}", headers.join("\n"));
        }
        if !messages.is_empty() {
            out.push_str(&format!("\n\n{}", messages.join("\n")));
        }
        if file_cap_truncated {
            out.push_str("\n\n[truncated — refine the pattern or scope to see more]");
        }
        Some(ok_result(out))
    }

    // ── Glob ───────────────────────────────────────────────────────────

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
        let include_ignored = args
            .get("include_ignored")
            .is_some_and(|v| v.as_bool() == Some(true));
        let head_limit = match args.get("head_limit").and_then(Value::as_u64) {
            Some(0) => usize::MAX,
            Some(n) => n as usize,
            None => 100,
        };
        let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
        let Some(glob) = build_glob(pattern) else {
            return Some(err_result(format!("invalid glob pattern: {pattern}")));
        };
        let search_root = match args.get("path").and_then(|p| p.as_str()) {
            Some(p) => {
                // A path the engine cannot resolve is a tool error, not a call
                // it cannot serve — `None` forwarded it to a host with no
                // file-tool runtime.
                //
                // v2 `globTool.ts:94-98`: same gate, same
                // `operation: 'search'` + `checkSensitive: false` pair as Grep.
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
        for entry in walker.flatten() {
            let path = entry.path();
            // Judge VCS metadata only *below the search root*: a workspace
            // rooted at e.g. `/home/u/.git-configs/proj` or a `.tower/worktrees/…`
            // checkout carries those names in its own prefix, and matching the
            // absolute path would silently exclude every result.
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
                let display = path.strip_prefix(sandbox.primary()).unwrap_or(path);
                let mtime = entry.metadata().ok().and_then(|m| m.modified().ok());
                results.push((display.display().to_string(), mtime));
            }
        }
        // v2 runs rg with `--sortr=modified` (globTool.ts:360), so results are
        // mtime DESC with the path breaking ties — the order the tool
        // description promises on both sides. Sorting the display strings
        // instead returned alphabetical order, so "the files I touched
        // last" came back wrong. Same ordering contract as
        // [`grep_sort_and_cap`](Self::grep_sort_and_cap), with two deliberate
        // differences:
        //
        // * A `None` mtime (the platform reported none) sorts LAST: `Option`
        //   orders `None` below every `Some` and the comparison is reversed,
        //   so an unknown time is never presented as the most recent one.
        // * The raw `SystemTime` is compared, NOT whole seconds. rg orders by
        //   the mtime it stats and never truncates, so truncating here would
        //   merge files rg keeps apart; the whole-second `mtime_secs` the
        //   grep path uses exists to match the host's
        //   `Math.trunc(mtimeMs / 1000)` (`sortFilesWithMatchesByMtime`),
        //   which glob does not apply.
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

        let out = [lines, footer].concat().join("\n");
        Some(ok_result(out))
    }
}

/// Which native Grep output shape a scan produces; drives how much per-file
/// state the streaming scan retains.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum GrepMode {
    FilesWithMatches,
    CountMatches,
    Content,
}

/// Everything a streaming Grep scan needs, shared by reference across the
/// parallel walker's worker threads.
#[derive(Clone, Copy)]
pub(super) struct GrepScanConfig<'a> {
    pub(super) regex: &'a regex::Regex,
    pub(super) mode: GrepMode,
    pub(super) context_before: usize,
    pub(super) context_after: usize,
    pub(super) line_numbers: bool,
    /// rg `-U`: matches may span newlines, so the scan buffers the whole file
    /// and reports every physical line a match covers.
    pub(super) multiline: bool,
}

/// A matching file's aggregated scan result. `rendered` is populated only in
/// `content` mode (the file's already-formatted context windows); the other
/// modes keep just the display path, mtime, and occurrence count, so a match
/// never drags whole-file line storage behind it.
pub(super) struct FileScan {
    pub(super) display: String,
    /// Modification time in whole seconds since the UNIX epoch, `0` when the
    /// platform cannot report one. Whole seconds are the host's granularity
    /// (`Math.trunc(mtimeMs / 1000)`); matching it exactly is what keeps the
    /// engine and the host in the same order for the same-second files a `git
    /// checkout` or `clone` leaves behind.
    pub(super) mtime: u64,
    pub(super) total_matches: usize,
    pub(super) rendered: Vec<String>,
}

/// [`FileScan::mtime`] source: whole seconds since the UNIX epoch, `0` on any
/// failure (the host's stat-failure fallback).
pub(super) fn mtime_secs(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// Impose the mode's deterministic ordering on the aggregated walk results,
/// then apply the hard scanned-file cap. Sorting runs BEFORE truncation so the
/// survivors are the top-N of the contract order instead of a
/// scheduling-dependent slice of the unordered parallel walk. Returns `true`
/// when entries were dropped.
///
/// Ordering contract — identical on both sides (the host applies it in
/// `GrepTool.sortFilesWithMatchesByMtime`):
/// * `files_with_matches`: whole-second mtime DESC, ties broken by display path
///   ASC;
/// * `content` / `count_matches`: display path ASC (rg's own output order is
///   walk order, which the parallel walk deliberately does not reproduce).
pub(super) fn grep_sort_and_cap(
    per_file: &mut Vec<FileScan>,
    mode: GrepMode,
    max_files: usize,
) -> bool {
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
    if per_file.len() > max_files {
        per_file.truncate(max_files);
        return true;
    }
    false
}

/// Worker threads for one parallel grep walk: [`GREP_WALK_THREADS`] capped by
/// the machine's own parallelism (never 0 — `ignore` treats that as "serial").
pub(super) fn grep_walk_threads() -> usize {
    std::thread::available_parallelism()
        .map(|cpus| cpus.get())
        .unwrap_or(1)
        .clamp(1, GREP_WALK_THREADS)
}

/// Outcome of scanning one candidate file.
pub(super) enum ScanOutcome {
    /// Not a match, binary, unreadable, or otherwise skipped silently.
    Skip,
    /// Larger than [`GREP_MAX_FILE_BYTES`]: skipped, but flips the caller's
    /// truncation notice.
    Oversized,
    /// A match, with the per-file aggregate.
    Match(FileScan),
}

/// Aggregated result of the parallel walk, before mode-specific rendering.
pub(super) struct GrepCollected {
    pub(super) per_file: Vec<FileScan>,
    pub(super) filtered_sensitive: Vec<String>,
    pub(super) timed_out: bool,
    pub(super) file_cap_truncated: bool,
}

/// Walk-level bounds every worker shares. Kept in one `Copy` struct so the
/// walker's signature stays within the argument budget and so both guards are
/// documented together: they are the two best-effort stop conditions (a worker
/// that trips either one quits, and the aggregate is flagged as partial).
#[derive(Clone, Copy)]
pub(super) struct GrepWalkLimits {
    /// Wall-clock cutoff ([`GREP_TIME_BUDGET`] from the call's start).
    pub(super) deadline: Instant,
    /// Soft memory guard ([`GREP_WALK_SCAN_CAP`]): how many files the walk may
    /// visit before workers stop scanning.
    pub(super) scan_cap: usize,
}

/// Fan the walk out across a bounded worker pool using `ignore`'s own
/// work-stealing `WalkParallel` (no extra runtime dependency — `ignore` already
/// pulls in `crossbeam-deque` for this). Each worker streams its files
/// line-by-line and pushes only the output window it needs, and stops scanning
/// once `limits.scan_cap` files have been visited, so peak memory stays bounded
/// regardless of repo size. The walk is intentionally unordered; the caller
/// imposes the mode's deterministic ordering afterwards.
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
    let timed_out = Arc::new(AtomicBool::new(false));
    let oversized = Arc::new(AtomicBool::new(false));
    let scanned = Arc::new(AtomicUsize::new(0));

    let collected_walk = Arc::clone(&collected);
    let sensitive_walk = Arc::clone(&sensitive);
    let timed_walk = Arc::clone(&timed_out);
    let oversized_walk = Arc::clone(&oversized);
    let scanned_walk = Arc::clone(&scanned);
    let cfg_copy = *cfg;
    let search_root_owned = search_root.to_path_buf();

    // rg `--no-ignore` (include_ignored) turns off every ignore source: repo
    // .gitignore, .git/info/exclude, the global gitignore, .ignore/.rgignore,
    // and parent-directory ignore files. Hidden files stay searched either way
    // (rg `--hidden`), and the walk closure still drops VCS dirs + sensitive
    // files below.
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
        let timed_out = Arc::clone(&timed_walk);
        let oversized = Arc::clone(&oversized_walk);
        let scanned = Arc::clone(&scanned_walk);
        let walk_root = search_root_owned.clone();
        Box::new(move |entry: Result<ignore::DirEntry, ignore::Error>| {
            // The wall-clock budget is an atomic flag every worker can
            // check; hitting it flips `timed_out` and asks the walk to
            // stop (Quit is best-effort, so a few stragglers may land).
            if Instant::now() >= limits.deadline {
                timed_out.store(true, Ordering::Relaxed);
                return WalkState::Quit;
            }
            let Ok(entry) = entry else {
                return WalkState::Continue;
            };
            let path = entry.path();
            // Judge VCS metadata only *below the search root* (see the Glob
            // walk): a workspace rooted under a path that itself contains
            // `.git`/`.hg`/… would otherwise exclude every hit.
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
            // rg `--type` matches the type globs against the file NAME, so
            // an exact glob like `BUILD` still matches `nested/BUILD`.
            if let Some(ts) = type_filter
                && !path.file_name().is_some_and(|name| ts.is_match(name))
            {
                return WalkState::Continue;
            }
            // Mirror the host Grep tool: matches inside sensitive files
            // (.env, keys, credentials, ...) are never reported.
            if crate::native::file_type::is_sensitive_file(&path.to_string_lossy()) {
                let display = path.strip_prefix(root).unwrap_or(path);
                sensitive
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(display.display().to_string());
                return WalkState::Continue;
            }
            // Soft memory guard: every scanned file may contribute nearly
            // GREP_MAX_FILE_BYTES of rendered windows that stay live until the
            // caller sorts and truncates them. A parallel walk cannot stop at
            // an exact global count, so workers quit past the soft cap
            // (best-effort, exactly like the deadline above) and flag the
            // result as truncated.
            if scanned.fetch_add(1, Ordering::Relaxed) >= limits.scan_cap {
                oversized.store(true, Ordering::Relaxed);
                return WalkState::Quit;
            }
            let outcome = if cfg_copy.multiline {
                scan_grep_file_multiline(path, root, &cfg_copy)
            } else {
                scan_grep_file(path, root, &cfg_copy)
            };
            match outcome {
                ScanOutcome::Oversized => oversized.store(true, Ordering::Relaxed),
                ScanOutcome::Match(file) => collected
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(file),
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
        timed_out: timed_out.load(Ordering::Relaxed),
        file_cap_truncated: oversized.load(Ordering::Relaxed),
    }
}

/// Stream one file line-by-line (never buffering the whole file), applying
/// the host Grep contract: skip binary files (any NUL byte), skip files with
/// no match, count occurrences per line like `rg --count-matches`, and — in
/// `content` mode — retain only the merged `-A`/`-B`/`-C` context windows
/// (clusters separated by `--`), not every line of the file.
pub(super) fn scan_grep_file(path: &Path, root: &Path, cfg: &GrepScanConfig) -> ScanOutcome {
    let Ok(file) = std::fs::File::open(path) else {
        return ScanOutcome::Skip;
    };
    // One metadata call serves both the size cap and the mtime (the previous
    // implementation stat'd the file twice).
    let Ok(meta) = file.metadata() else {
        return ScanOutcome::Skip;
    };
    if meta.len() > GREP_MAX_FILE_BYTES {
        return ScanOutcome::Oversized;
    }
    let mtime = mtime_secs(&meta);
    let display = path
        .strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string();
    scan_grep_lines(std::io::BufReader::new(file), cfg, display, mtime)
}

/// Streaming body of [`scan_grep_file`], split out over a generic line source
/// so the read-error contract is testable without a failing filesystem: an I/O
/// error part-way through drops the WHOLE file (`ScanOutcome::Skip`) instead of
/// reporting the already-read prefix as a complete result.
pub(super) fn scan_grep_lines<R: std::io::BufRead>(
    mut reader: R,
    cfg: &GrepScanConfig,
    display: String,
    mtime: u64,
) -> ScanOutcome {
    let content = cfg.mode == GrepMode::Content;
    // `files_with_matches` only needs "has at least one match"; once found, the
    // regex is skipped for the rest of the file (still scanned for NUL so the
    // binary-skip contract holds).
    let only_need_presence = cfg.mode == GrepMode::FilesWithMatches;

    let mut buf: Vec<u8> = Vec::new();
    let mut total_matches = 0usize;
    let mut has_match = false;
    let mut idx = 0usize;

    // content-mode cluster state, bounded by one active cluster plus a
    // `context_before` lookback rather than the whole file.
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
            // `read_until` already retries interrupted reads, so any error here
            // is a real I/O failure: the rest of the file is unknown, and a
            // partial result would be indistinguishable from a complete one.
            Err(_) => return ScanOutcome::Skip,
        }
        let terminated = buf.last() == Some(&b'\n');
        if terminated {
            buf.pop();
        }
        // Mirror the host: any NUL byte marks the file binary and skips it
        // entirely. Breaking early is safe — the whole file would have been
        // discarded anyway.
        if buf.contains(&0) {
            return ScanOutcome::Skip;
        }
        // Emulate `str::lines()`: a terminated segment loses one trailing
        // `\r` (the `\r\n` case), then the explicit `strip_suffix('\r')` the
        // previous implementation applied.
        if terminated && buf.last() == Some(&b'\r') {
            buf.pop();
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
                    // Merge into the current cluster, filling the context gap
                    // between the last buffered line and this match from the
                    // lookback.
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
            // Maintain the before-context lookback (only when it can pull
            // prior lines in).
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
    })
}

/// Multiline scan (rg `-U --multiline-dotall`). Cross-line matching cannot
/// stream, so the whole file is buffered — still bounded by
/// [`GREP_MAX_FILE_BYTES`] and the binary (NUL) skip contract. The regex runs
/// once over the full text; each match marks every physical line it spans as a
/// match line, then the same cluster/context renderer as the single-line path
/// produces byte-identical `-A`/`-B`/`-C` output. `count_matches` counts rg
/// matches (not spanned lines), matching `rg --count-matches -U`.
pub(super) fn scan_grep_file_multiline(
    path: &Path,
    root: &Path,
    cfg: &GrepScanConfig,
) -> ScanOutcome {
    use std::io::Read;

    let Ok(mut file) = std::fs::File::open(path) else {
        return ScanOutcome::Skip;
    };
    let Ok(meta) = file.metadata() else {
        return ScanOutcome::Skip;
    };
    if meta.len() > GREP_MAX_FILE_BYTES {
        return ScanOutcome::Oversized;
    }
    let mtime = mtime_secs(&meta);
    let display = path
        .strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string();

    let mut raw: Vec<u8> = Vec::with_capacity(meta.len() as usize);
    if file.read_to_end(&mut raw).is_err() {
        return ScanOutcome::Skip;
    }
    // Mirror the host: any NUL byte marks the file binary and skips it whole.
    if raw.contains(&0) {
        return ScanOutcome::Skip;
    }
    let content = String::from_utf8_lossy(&raw);

    // Split into display lines (each loses one trailing `\r`, mirroring the
    // host's per-line `stripTrailingCarriageReturn`). A trailing newline does
    // not create a final empty line — same count as the streaming path.
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

    // find_iter yields non-overlapping matches in increasing order, so both
    // `start` and `end-1` advance monotonically; a single newline cursor maps
    // byte offsets to line indices in O(file) total rather than O(file*matches).
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

    // Which lines a match covers, and the `--count-matches` number, are
    // pinned against ripgrep 15.0.0 (`rg -U --multiline-dotall`):
    //
    // * A zero-width match sitting on a `\n` byte also marks the line *after*
    //   that terminator (`$` on "a\nb" prints both lines but counts 1).
    // * A zero-width match at the very end of the buffer is normally dropped:
    //   `x*` counts `len` matches, not `len + 1`, and `\z` finds nothing in a
    //   file that ends with `\n`. The one exception rg keeps is a pattern whose
    //   *only* match is that EOF position in a file with no trailing newline
    //   (`\z` on "ab" counts 1 and prints the last line) — dropping it there
    //   would lose the file from `files_with_matches` entirely.
    //
    // The regex crate's own `find_iter` already applies rg's empty-match
    // dedup (an empty match at the previous match's end is skipped), so only
    // the EOF cases need handling here.
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
    })
}

/// Render one merged context cluster: match lines use `:`, context lines use
/// `-`, with the display path prefix and (optionally) the line number, exactly
/// as the host `content` mode does.
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

/// Compile a glob, auto-prefixing bare patterns with `**/` the way the JS
/// Glob tool does, so `*.rs` matches at any depth.
pub(super) fn build_glob(pattern: &str) -> Option<globset::GlobSet> {
    let mut builder = globset::GlobSetBuilder::new();
    builder.add(globset::Glob::new(pattern).ok()?);
    if !pattern.starts_with("**/") && !pattern.contains('/') && !pattern.contains('\\') {
        builder.add(globset::Glob::new(&format!("**/{pattern}")).ok()?);
    }
    builder.build().ok()
}

/// Compile a ripgrep file type's globs into a set matched against the file
/// NAME (rg `--type` semantics). Unlike [`build_glob`], no `**/` prefix is
/// added: type globs are basename patterns and rg matches them on the file
/// name alone. Returns `None` if any glob fails to compile, which falls back
/// to the host (rg's own table always compiles, so this is defensive only).
pub(super) fn build_type_glob(globs: &[&str]) -> Option<globset::GlobSet> {
    let mut builder = globset::GlobSetBuilder::new();
    for g in globs {
        builder.add(globset::Glob::new(g).ok()?);
    }
    builder.build().ok()
}
