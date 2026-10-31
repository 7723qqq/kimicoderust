use super::*;

impl NativeToolset {
    /// Resolve a path argument inside the sandbox. `None` when the path
    /// escapes every authorized root or does not exist.
    pub(super) fn resolve(
        sandbox: &Sandbox,
        bridge: &crate::native::shell_path_bridge::ShellPathBridge,
        path: &str,
    ) -> Option<PathBuf> {
        // A preserved attachment is addressed by reference rather than by a
        // workspace path: `kimi-file://<id>` — what an attachment notice hands
        // the model — names a blob in the attachment store (v2 `fileReadSource`,
        // upstream #3688). Anything that is not a store id falls through to the
        // ordinary resolution below.
        if let Some(blob) = crate::server::files::resolve_attachment_reference(
            &crate::server::files::FileStore::new(),
            path,
        ) {
            return Some(blob);
        }
        // Reads are not path-gated: v2's sandbox only covers writes
        // (`sandboxWriteGuard`), and gating reads here turned every
        // out-of-workspace read into a silent host fallback — the host has no
        // file-tool runtime, so the read simply vanished.
        let candidate = Self::candidate_path(sandbox.primary(), bridge, path);
        std::fs::canonicalize(&candidate).ok()
    }

    /// Like [`resolve`] but tolerates a not-yet-existing target: walks up to
    /// the nearest existing ancestor, canonicalizes it (resolving any
    /// symlink escapes), then rejoins the missing tail. `None` when the
    /// existing ancestor lies outside the sandbox.
    pub(super) fn resolve_for_write(
        sandbox: &Sandbox,
        bridge: &crate::native::shell_path_bridge::ShellPathBridge,
        path: &str,
    ) -> Option<PathBuf> {
        // Write confinement belongs to the SandboxMode gateway
        // (`SandboxExecutionPolicy::sandbox_write_guard`, Off by default —
        // mirroring v2), not to this resolver: an unconditional root check here
        // also blocked writes the configured mode had already allowed.
        //
        // Lexical, mirroring v2's `resolvePathAccessPath` (post-#4013): no
        // `fs::canonicalize`, no nearest-existing-ancestor fallback. The
        // pre-hardening baseline (`762f405811`, 2026-08-30) canonicalized the
        // nearest existing ancestor, which left a registered divergence —
        // under the non-default `sandbox_write_guard` the guard saw the real
        // path where v2 sees the lexical one (ROADMAP §6.8.1). Resolved by
        // returning the lexical candidate verbatim.
        Some(Self::candidate_path(sandbox.primary(), bridge, path))
    }

    pub(super) fn candidate_path(
        root: &Path,
        bridge: &crate::native::shell_path_bridge::ShellPathBridge,
        path: &str,
    ) -> PathBuf {
        // Git Bash / Cygwin spellings (`/g/kimi`, `/cygdrive/g/kimi`) are not
        // absolute on Windows: they carry a root but no drive prefix, so
        // `Path::is_absolute` is false and `join` *replaces* the root with the
        // drive root — `/g/kimi/x` under `G:/ws` silently became `G:/g/kimi/x`.
        // The tool descriptions already promise both spellings
        // (`core_tool_defs.rs`: "the `path` argument accepts both Windows paths
        // and POSIX-style paths").
        //
        // v2 resolves through `shellPathBridge.fromShellPath` first
        // (`path-access.ts` `resolvePathAccess`): the lexical drive forms are
        // rewritten in place, and a root-relative path the lexical pass cannot
        // place (`/tmp/x`, `/usr/bin`) goes through `cygpath -w` next to the
        // probed bash. `normalize_user_path` alone only covers the lexical
        // half, so `/tmp/x` still landed on `<drive-root>/tmp/x`.
        let bridged = bridge.to_native_path(path);
        let class = if cfg!(windows) {
            crate::native::path_access::PathClass::Win32
        } else {
            crate::native::path_access::PathClass::Posix
        };
        let normalized = crate::native::path_access::normalize_user_path(&bridged, class);
        let candidate = Path::new(&normalized);
        if candidate.is_absolute() {
            PathBuf::from(candidate)
        } else {
            root.join(candidate)
        }
    }

    /// v2 `resolvePathAccessPath` (`tool/path-access.ts:304-355`) for the OS
    /// file tools: lexical resolution plus the workspace-access policy, applied
    /// **before** any filesystem access so a refused path is never stat'ed.
    ///
    /// Returns the model-facing refusal text when the gate rejects the path,
    /// `None` when the tool may proceed. v2 surfaces `PathSecurityError.message`
    /// verbatim as the tool result (`toolExecutorService.ts:392-396`) and
    /// swallows the code, so only the message crosses back.
    ///
    /// `home_dir` is `None` because Rust's `candidate_path` does not expand
    /// `~` either; passing one here would gate a spelling the tool never reads.
    pub(super) fn file_access_refusal(
        sandbox: &Sandbox,
        bridge: &crate::native::shell_path_bridge::ShellPathBridge,
        path: &str,
        operation: crate::native::path_access::PathAccessOperation,
        policy: &crate::native::path_access::WorkspaceAccessPolicy,
    ) -> Option<String> {
        use crate::native::path_access::{PathClass, resolve_path_access};
        let class = if cfg!(windows) {
            PathClass::Win32
        } else {
            PathClass::Posix
        };
        let bridged = bridge.to_native_path(path);
        let roots: Vec<String> = sandbox
            .roots()
            .map(|root| root.to_string_lossy().into_owned())
            .collect();
        let cwd = sandbox.primary().to_string_lossy().into_owned();
        let access =
            match resolve_path_access(&bridged, &cwd, &roots, None, class, operation, policy) {
                Ok(access) => access,
                Err(error) => return Some(error.message),
            };

        // Lexical gate passed. v2's second half (`tool/realpath-access.ts`,
        // wired into all six file tools) then resolves symlinks and refuses what
        // the name hid: an innocent-looking link onto a sensitive file, a link
        // out of the workspace, or a dangling link. It is the only thing standing
        // between `notes.txt -> .env` and the contents of the environment file.
        //
        // A path that is already lexically outside the workspace is handed back
        // by that layer unchanged — only its sensitive pattern is checked — so
        // ordinary absolute reads and writes outside the root keep working and
        // stay the permission layer's decision, exactly as in v2.
        let resolved = std::path::PathBuf::from(&access.path);
        let realpath_outcome = match operation {
            crate::native::path_access::PathAccessOperation::Write => {
                crate::native::realpath_access::assert_real_path_write_target(
                    &resolved, &roots, class,
                )
            }
            _ => crate::native::realpath_access::assert_real_path_within_workspace(
                &resolved,
                &roots,
                class,
                policy.check_sensitive,
            ),
        };
        realpath_outcome.err().map(|error| error.message)
    }

    // ── Read ───────────────────────────────────────────────────────────

    pub(super) fn read(
        sandbox: &Sandbox,
        bridge: &ShellPathBridge,
        args: &Value,
    ) -> Option<ExecutableToolResult> {
        let Some(path) = args.get("path").and_then(Value::as_str) else {
            return Some(err_result(
                LocalizedText::new("engine.tools.read.pathRequired").render(),
            ));
        };
        // v2 `readTool.ts:236-240` runs the workspace-access policy *before* the
        // file is touched. The engine previously resolved and read the path with
        // no such gate, so `Read(".env")` returned the file's contents while
        // `core_tool_defs.rs:24` told the model sensitive files "are refused".
        if let Some(refusal) = Self::file_access_refusal(
            sandbox,
            bridge,
            path,
            crate::native::path_access::PathAccessOperation::Read,
            &crate::native::path_access::WorkspaceAccessPolicy::default(),
        ) {
            return Some(err_result(refusal));
        }
        // The dispatcher's media path owns `region` / `full_resolution` and
        // is consulted before every call that reaches this text read; decline
        // them here anyway so a direct caller cannot half-handle an image.
        if args.get("region").is_some_and(|v| !v.is_null())
            || args
                .get("full_resolution")
                .is_some_and(|v| v.as_bool() == Some(true))
        {
            return None;
        }
        // Negative offsets are tail reads implemented natively: -N starts
        // N lines from the end of the file (schema: -1000..=-1).
        let (line_offset, tail_lines) = match args.get("line_offset") {
            None | Some(Value::Null) => (1i64, 0usize),
            Some(v) => {
                let Some(n) = parse_integer_arg(v) else {
                    return Some(err_result(
                        LocalizedText::with_params(
                            "engine.tools.read.lineOffsetInvalid",
                            i18n_params!["v" => v],
                        )
                        .render(),
                    ));
                };
                if n == 0 {
                    // v2 `readTool` reads forward from `line_offset ?? 1`, so 0
                    // is the start of the file. Declining it here forwarded the
                    // call to a host with no file-tool runtime.
                    (1i64, 0usize)
                } else {
                    (n, n.unsigned_abs() as usize)
                }
            }
        };
        let mut offset = if line_offset > 0 {
            line_offset as usize
        } else {
            // Resolved below once the line count is known; 0 is a sentinel
            // that is always replaced before use.
            0
        };
        let column_offset = match args.get("column_offset") {
            None | Some(Value::Null) => 0usize,
            Some(v) => match parse_integer_arg(v).filter(|n| *n >= 0) {
                Some(n) => n as usize,
                None => {
                    return Some(err_result(
                        LocalizedText::with_params(
                            "engine.tools.read.columnOffsetInvalid",
                            i18n_params!["v" => v],
                        )
                        .render(),
                    ));
                }
            },
        };
        let max_chars = match args.get("max_chars") {
            None | Some(Value::Null) => 100_000usize,
            Some(v) => match parse_integer_arg(v).filter(|n| *n >= 0) {
                Some(n) => (n as usize).clamp(1, 500_000),
                None => {
                    return Some(err_result(
                        LocalizedText::with_params(
                            "engine.tools.read.maxCharsInvalid",
                            i18n_params!["v" => v],
                        )
                        .render(),
                    ));
                }
            },
        };
        let n_lines = match args.get("n_lines") {
            None | Some(Value::Null) => READ_MAX_LINES,
            Some(v) => match parse_integer_arg(v).filter(|n| *n >= 0) {
                Some(n) => (n as usize).min(READ_MAX_LINES),
                None => {
                    return Some(err_result(
                        LocalizedText::with_params(
                            "engine.tools.read.nLinesInvalid",
                            i18n_params!["v" => v],
                        )
                        .render(),
                    ));
                }
            },
        };

        // A path the engine cannot resolve is a tool error, not a call the
        // engine cannot serve (v2 `readTool`: `"<path>" does not exist.`).
        // Returning `None` here made the engine forward the call to the host,
        // which has no file-tool runtime — the model saw `tool "Read" is
        // host-owned and not yet wired on the native harness` instead of the
        // path being wrong.
        let Some(resolved) = Self::resolve(sandbox, bridge, path) else {
            return Some(err_result(
                LocalizedText::with_params(
                    "engine.tools.read.notExist",
                    i18n_params!["path" => path],
                )
                .render(),
            ));
        };
        let Ok(meta) = std::fs::metadata(&resolved) else {
            return Some(err_result(
                LocalizedText::with_params(
                    "engine.tools.read.notExist",
                    i18n_params!["path" => path],
                )
                .render(),
            ));
        };
        if !meta.is_file() {
            return Some(err_result(
                LocalizedText::with_params(
                    "engine.tools.read.notAFile",
                    i18n_params!["path" => path],
                )
                .render(),
            ));
        }

        // Encoding detection needs only the first bytes — reads are ranged
        // (v2 #3645), so a 30MB+ file must never load whole just to be read.
        let Ok(mut file) = std::fs::File::open(&resolved) else {
            return Some(err_result(
                LocalizedText::with_params(
                    "engine.tools.read.cannotOpen",
                    i18n_params!["path" => path],
                )
                .render(),
            ));
        };
        let sample_len = (meta.len() as usize).min(encoding::ENCODING_DETECTION_SAMPLE_BYTES);
        let mut header = vec![0u8; sample_len];
        if sample_len > 0
            && (std::io::Read::read_exact(&mut file, &mut header).is_err()
                || std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(0)).is_err())
        {
            return Some(err_result(
                LocalizedText::with_params(
                    "engine.tools.read.cannotRead",
                    i18n_params!["path" => path],
                )
                .render(),
            ));
        }
        let detection = encoding::detect_text_encoding(&header);
        if detection.seems_binary {
            // v2 `readTool` refuses a file whose type it cannot place
            // (`notReadableFileOutput`); returning `None` sent it to a host
            // with no file-tool runtime instead.
            return Some(err_result(
                LocalizedText::with_params(
                    "engine.tools.read.notUtf8",
                    i18n_params!["path" => path],
                )
                .render(),
            ));
        }

        // Tail reads on UTF-8/ASCII files: count lines in a streaming pass
        // (no storage), then collect the window in a second pass from the
        // top, so large files are never loaded whole. The probe over-reads
        // into its buffer, so rewind explicitly afterwards.
        if tail_lines > 0 && detection.encoding == encoding::UtfTextEncoding::Utf8 {
            let counted = {
                use std::io::BufRead;
                let mut probe = std::io::BufReader::new(&mut file);
                let mut buf = Vec::new();
                let mut count = 0usize;
                loop {
                    buf.clear();
                    let n = probe.read_until(b'\n', &mut buf).ok()?;
                    if n == 0 {
                        break;
                    }
                    count += 1;
                }
                count
            };
            std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(0)).ok()?;
            offset = counted.saturating_sub(tail_lines) + 1;
        }

        // Non-UTF-8 text (UTF-16/GBK) requires whole-file transcoding; the
        // transcode cap applies there. UTF-8/ASCII files stream line by line:
        // only the rendered window is held in memory, so file size is not a
        // constraint (the old whole-file load + READ_MAX_BYTES cap declined
        // every large text file before this).
        let (mut all, encoding_note, total_lines): (
            Vec<String>,
            Option<encoding::UtfTextEncoding>,
            usize,
        ) = if detection.encoding != encoding::UtfTextEncoding::Utf8 {
            if meta.len() > READ_MAX_BYTES {
                // v2 `readTool` refuses a transcode it cannot afford
                // (`TRANSCODE_MAX_BYTES`); returning `None` sent the call to a
                // host with no file-tool runtime.
                return Some(err_result(format!(
                    "\"{path}\" is {} text but too large to transcode ({} bytes > {}). Convert it to UTF-8 first (e.g. with `iconv`).",
                    detection.encoding.display_name(),
                    meta.len(),
                    READ_MAX_BYTES
                )));
            }
            let Ok(bytes) = std::fs::read(&resolved) else {
                return Some(err_result(
                    LocalizedText::with_params(
                        "engine.tools.read.cannotRead",
                        i18n_params!["path" => path],
                    )
                    .render(),
                ));
            };
            let text = encoding::decode_utf_text(&bytes, detection.encoding);
            let text_lines: Vec<String> = text.split('\n').map(|l| l.to_string()).collect();
            let total = text_lines.len();
            // Tail reads slice from the loaded lines. A trailing newline
            // leaves a phantom empty segment that the streaming counter
            // never sees, so exclude it from the tail math (the reported
            // total keeps the historical count).
            let windowed: Vec<String> = if tail_lines > 0 {
                let mut count = total;
                if count > 0 && text_lines.last().is_some_and(|l| l.is_empty()) {
                    count -= 1;
                }
                let start = count.saturating_sub(tail_lines).min(total);
                offset = start + 1;
                text_lines.into_iter().skip(start).collect()
            } else {
                text_lines
            };
            (windowed, Some(detection.encoding), total)
        } else {
            use std::io::BufRead;
            let mut reader = std::io::BufReader::new(file);
            let mut raw: Vec<u8> = Vec::new();
            let mut all: Vec<String> = Vec::new();
            let mut total: usize = 0;
            loop {
                raw.clear();
                let n = reader.read_until(b'\n', &mut raw).ok()?;
                if n == 0 {
                    break;
                }
                total += 1;
                if total < offset || all.len() >= n_lines {
                    continue;
                }
                if raw.contains(&0) {
                    // v2 `readTool` refuses a NUL-bearing line
                    // (`notReadableFileOutput`) rather than declining the call.
                    return Some(err_result(
                        LocalizedText::with_params(
                            "engine.tools.read.notUtf8",
                            i18n_params!["path" => path],
                        )
                        .render(),
                    ));
                }
                let mut l = raw.clone();
                if l.last() == Some(&b'\n') {
                    l.pop();
                }
                match std::str::from_utf8(&l) {
                    Ok(text) => all.push(text.to_string()),
                    // v2 `readTool` rejects invalid UTF-8 instead of returning
                    // replacement characters; `None` here reached the model as
                    // `tool "Read" is host-owned and not yet wired on the
                    // native harness`.
                    Err(_) => {
                        return Some(err_result(
                            LocalizedText::with_params(
                                "engine.tools.read.notUtf8Or16",
                                i18n_params!["path" => path],
                            )
                            .render(),
                        ));
                    }
                }
            }
            (all, None, total)
        };

        // A leading UTF-8 BOM is stripped like TextDecoder does.
        if let Some(first) = all.first_mut()
            && let Some(stripped) = first.strip_prefix('\u{FEFF}')
        {
            *first = stripped.to_string();
        }
        // Re-attach the '\n' separators: the style detector distinguishes
        // CRLF from lone CR by what follows the '\r'.
        let window_bytes: Vec<u8> = all
            .iter()
            .flat_map(|l| {
                let mut b = l.as_bytes().to_vec();
                b.push(b'\n');
                b
            })
            .collect();
        let style = encoding::detect_line_ending_style(&window_bytes);

        if total_lines > 0 && offset > total_lines {
            return Some(err_result(
                LocalizedText::with_params(
                    "engine.tools.read.offsetPastEnd",
                    i18n_params!["offset" => offset, "path" => path, "total_lines" => total_lines],
                )
                .render(),
            ));
        }
        let start = 0usize;
        let end = all.len();
        // Line rendering mirrors the host Read tool: `${lineNo}\t${content}`,
        // CRLF-style trailing CRs stripped, per-line truncation to
        // READ_MAX_LINE_LENGTH characters with a `...` marker, lone CRs made
        // visible as `\r` on mixed files, and a READ_MAX_OUTPUT_BYTES / max_chars budget.
        let mut out = String::new();
        let mut truncated_lines: Vec<usize> = Vec::new();
        let mut rendered_bytes = 0usize;
        let mut max_bytes_reached = false;
        let mut max_chars_reached = false;
        let mut rendered_count = 0usize;
        let mut resume_continuation: Option<(usize, usize)> = None;

        for (i, raw) in all[start..end].iter().enumerate() {
            let mut rendered: String = (*raw).to_string();
            let mut was_truncated = false;
            if style == encoding::LineEndingStyle::CrLf && rendered.ends_with('\r') {
                rendered.pop();
            }
            if i == 0 && column_offset > 0 {
                let char_count = rendered.chars().count();
                if column_offset >= char_count {
                    rendered.clear();
                } else {
                    rendered = rendered.chars().skip(column_offset).collect();
                }
            }
            if rendered.chars().count() > READ_MAX_LINE_LENGTH {
                const MARKER: &str = "...";
                let keep = READ_MAX_LINE_LENGTH - MARKER.len();
                rendered = rendered.chars().take(keep).collect();
                rendered.push_str(MARKER);
                was_truncated = true;
            }
            if style == encoding::LineEndingStyle::Mixed {
                rendered = encoding::make_carriage_returns_visible(&rendered);
            }
            let current_line_num = offset + i;
            let rendered_line = format!("{}\t{}", current_line_num, rendered);
            // Check max_chars budget
            if !out.is_empty()
                && out.chars().count() + rendered_line.chars().count() + 1 > max_chars
            {
                max_chars_reached = true;
                let available_chars = max_chars.saturating_sub(out.chars().count() + 1);
                if available_chars > 8 {
                    let partial: String = rendered_line.chars().take(available_chars).collect();
                    out.push_str(&partial);
                    out.push('\n');
                    resume_continuation = Some((
                        current_line_num,
                        column_offset
                            + available_chars
                                .saturating_sub(format!("{current_line_num}\t").chars().count()),
                    ));
                } else {
                    resume_continuation = Some((current_line_num, 0));
                }
                break;
            }
            // The separator byte between rendered lines counts toward the
            // budget (host renderedLineBytes accounting).
            let line_bytes = rendered_line.len() + usize::from(!out.is_empty());
            if !out.is_empty() && rendered_bytes + line_bytes > READ_MAX_OUTPUT_BYTES {
                max_bytes_reached = true;
                resume_continuation = Some((current_line_num, 0));
                break;
            }
            if was_truncated {
                truncated_lines.push(current_line_num);
            }
            out.push_str(&rendered_line);
            out.push('\n');
            rendered_count += 1;
            rendered_bytes += line_bytes;
            if rendered_bytes >= READ_MAX_OUTPUT_BYTES {
                max_bytes_reached = true;
                if start + i + 1 < end {
                    resume_continuation = Some((offset + i + 1, 0));
                }
                break;
            }
        }
        // Host-faithful `<system>` note (finishMessage in readTool.ts).
        let mut parts: Vec<String> = Vec::new();
        if rendered_count > 0 {
            if column_offset > 0 {
                parts.push(format!(
                    "{rendered_count} {} read from file starting from line {offset}, column {column_offset}.",
                    if rendered_count == 1 { "line" } else { "lines" }
                ));
            } else {
                parts.push(format!(
                    "{rendered_count} {} read from file starting from line {offset}.",
                    if rendered_count == 1 { "line" } else { "lines" }
                ));
            }
        } else {
            parts.push("No lines read from file.".into());
        }
        parts.push(format!("Total lines in file: {total_lines}."));
        let max_lines_reached = n_lines >= READ_MAX_LINES
            && rendered_count == n_lines
            && offset + all.len() <= total_lines;
        if max_lines_reached {
            parts.push(format!("Max {READ_MAX_LINES} lines reached."));
        } else if max_chars_reached {
            parts.push(format!("Max {max_chars} characters reached."));
        } else if max_bytes_reached {
            parts.push(format!("Max {READ_MAX_OUTPUT_BYTES} bytes reached."));
        } else if rendered_count < n_lines {
            parts.push("End of file reached.".into());
        }
        if let Some((next_line, next_col)) = resume_continuation {
            parts.push(format!(
                "To resume reading, call Read with line_offset={next_line}, column_offset={next_col}."
            ));
        }
        if !truncated_lines.is_empty() {
            let list = truncated_lines
                .iter()
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            parts.push(format!(
                "Lines [{list}] were truncated to {READ_MAX_LINE_LENGTH} characters; use Bash (e.g. cut or sed) to read the elided content of those lines."
            ));
        }
        if style == encoding::LineEndingStyle::Mixed {
            parts.push(
                "Mixed or lone carriage-return line endings are shown as \\r. Use exact \\r\\n or \\r escapes in Edit.old_string for those lines.".into(),
            );
        }
        if let Some(enc) = encoding_note {
            parts.push(format!(
                "Detected file encoding: {}; content transcoded to UTF-8 for display. Edit and Write expect UTF-8 — convert the file's encoding first (e.g. `iconv` via Bash).",
                enc.display_name()
            ));
        }
        let mut result = ok_result(out);
        result.note = Some(format!("<system>{}</system>", parts.join(" ")));
        Some(result)
    }
}

impl NativeToolset {
    // ── Write ──────────────────────────────────────────────────────────

    /// Record a successful mutating-file change into the session file-history
    /// table when a recorder is installed (v2 `fileHistoryService.onWillExecuteTool`
    /// plus post-image diff). Reads `turn_id` from the `TURN_ID` task-local so
    /// multi-turn pipelines attribute changes per turn. A failure to record
    /// never affects the tool result — the change itself already landed.
    pub(super) fn record_file_history(resolved: &Path, before: Option<&str>, after: Option<&str>) {
        let Some(ctx) = FILE_HISTORY.with(|cell| cell.borrow().clone()) else {
            return;
        };
        let turn_id = TURN_ID.with(|cell| *cell.borrow());
        if let Err(e) = ctx.store.record_file_change(
            &ctx.session_id,
            turn_id,
            &resolved.display().to_string(),
            before,
            after,
        ) {
            tracing::debug!(
                session = %ctx.session_id,
                turn_id,
                path = %resolved.display(),
                error = %e,
                "file-history record failed"
            );
        }
    }

    pub(super) fn write(
        sandbox: &Sandbox,
        bridge: &ShellPathBridge,
        args: &Value,
    ) -> Option<ExecutableToolResult> {
        let Some(path) = args.get("path").and_then(Value::as_str) else {
            return Some(err_result(
                LocalizedText::new("engine.tools.write.pathRequired").render(),
            ));
        };
        // v2 `writeTool.ts:51-55`: `operation: 'write'` under the default
        // policy, so a sensitive target is refused here and not at the
        // permission layer (which only reaches an `Ask`).
        if let Some(refusal) = Self::file_access_refusal(
            sandbox,
            bridge,
            path,
            crate::native::path_access::PathAccessOperation::Write,
            &crate::native::path_access::WorkspaceAccessPolicy::default(),
        ) {
            return Some(err_result(refusal));
        }
        let Some(content) = args.get("content").and_then(Value::as_str) else {
            return Some(err_result(
                LocalizedText::new("engine.tools.write.contentRequired").render(),
            ));
        };
        let mode = match args.get("mode") {
            None | Some(Value::Null) => "overwrite",
            Some(v) => match v.as_str() {
                Some(mode @ ("overwrite" | "append")) => mode,
                _ => {
                    return Some(err_result(
                        LocalizedText::with_params(
                            "engine.tools.write.modeInvalid",
                            i18n_params!["v" => v],
                        )
                        .render(),
                    ));
                }
            },
        };
        // A path the sandbox refuses is a tool error, not a call the engine
        // cannot serve: `None` forwarded it to a host with no file-tool
        // runtime, and the model saw `tool "Write" is host-owned and not yet
        // wired on the native harness` instead of the boundary it crossed.
        let Some(resolved) = Self::resolve_for_write(sandbox, bridge, path) else {
            return Some(err_result(
                LocalizedText::with_params(
                    "engine.tools.write.outsideWorkspace",
                    i18n_params!["path" => path],
                )
                .render(),
            ));
        };
        if let Some(parent) = resolved.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            return Some(err_result(
                LocalizedText::with_params(
                    "engine.tools.write.cannotWrite",
                    i18n_params!["path" => path, "error" => error],
                )
                .render(),
            ));
        }
        // Snapshot the pre-image before the write so the recorder can store
        // the diff (v2 `fileHistoryService.onWillExecuteTool` capture).
        let pre_image = if resolved.is_file() {
            std::fs::read_to_string(&resolved).ok()
        } else {
            None
        };
        let written = match mode {
            "overwrite" => std::fs::write(&resolved, content).map(|()| content.len()),
            "append" => {
                use std::io::Write;
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&resolved)
                    .and_then(|mut f| f.write_all(content.as_bytes()).map(|()| content.len()))
            }
            // Unreachable: `mode` was narrowed to the schema's enum above.
            _ => unreachable!("mode is validated against the schema enum"),
        };
        let bytes_written = match written {
            Ok(bytes) => bytes,
            Err(error) => {
                return Some(err_result(
                    LocalizedText::with_params(
                        "engine.tools.write.cannotWrite",
                        i18n_params!["path" => path, "error" => error],
                    )
                    .render(),
                ));
            }
        };
        Self::record_file_history(&resolved, pre_image.as_deref(), Some(content));
        // Output format mirrors the host Write tool.
        Some(ok_result(format!(
            "{} {bytes_written} bytes to {path}",
            if mode == "append" {
                "Appended"
            } else {
                "Wrote"
            }
        )))
    }

    // ── Edit ───────────────────────────────────────────────────────────

    pub(super) fn edit(
        sandbox: &Sandbox,
        bridge: &ShellPathBridge,
        args: &Value,
    ) -> Option<ExecutableToolResult> {
        let Some(path) = args.get("path").and_then(Value::as_str) else {
            return Some(err_result(
                LocalizedText::new("engine.tools.edit.pathRequired").render(),
            ));
        };
        // v2 `editTool.ts:52-56`: `operation: 'write'` — Edit shares Write's
        // policy, and the out-of-workspace message says "write or edit" for
        // both.
        if let Some(refusal) = Self::file_access_refusal(
            sandbox,
            bridge,
            path,
            crate::native::path_access::PathAccessOperation::Write,
            &crate::native::path_access::WorkspaceAccessPolicy::default(),
        ) {
            return Some(err_result(refusal));
        }
        let Some(old) = args.get("old_string").and_then(Value::as_str) else {
            return Some(err_result(
                LocalizedText::new("engine.tools.edit.oldStringRequired").render(),
            ));
        };
        let Some(new) = args.get("new_string").and_then(Value::as_str) else {
            return Some(err_result(
                LocalizedText::new("engine.tools.edit.newStringRequired").render(),
            ));
        };
        let replace_all = args
            .get("replace_all")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let Some(resolved) = Self::resolve_for_write(sandbox, bridge, path) else {
            return Some(err_result(
                LocalizedText::with_params(
                    "engine.tools.edit.outsideWorkspace",
                    i18n_params!["path" => path],
                )
                .render(),
            ));
        };

        let Ok(bytes) = std::fs::read(&resolved) else {
            return Some(err_result(
                LocalizedText::with_params(
                    "engine.tools.edit.notExist",
                    i18n_params!["path" => path],
                )
                .render(),
            ));
        };
        if bytes.contains(&0) {
            // v2 `readTool` refuses a file it cannot place; the host has no
            // file-tool runtime to hand it to.
            return Some(err_result(
                LocalizedText::with_params(
                    "engine.tools.edit.notUtf8",
                    i18n_params!["path" => path],
                )
                .render(),
            ));
        }
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let occurrence_count = text.matches(old).count();
        let updated = if replace_all {
            if occurrence_count == 0 {
                return Some(err_result(
                    LocalizedText::with_params(
                        "engine.tools.edit.oldStringNotFound",
                        i18n_params!["path" => path],
                    )
                    .render(),
                ));
            }
            text.replace(old, new)
        } else {
            if occurrence_count != 1 {
                return Some(err_result(
                    LocalizedText::with_params(
                        "engine.tools.edit.oldStringMatchedMultiple",
                        i18n_params!["occurrence_count" => occurrence_count, "path" => path],
                    )
                    .render(),
                ));
            }
            text.replacen(old, new, 1)
        };
        // `text` is the pre-image; capture the diff before writing.
        Self::record_file_history(&resolved, Some(text.as_str()), Some(updated.as_str()));
        if let Err(error) = std::fs::write(&resolved, updated) {
            return Some(err_result(
                LocalizedText::with_params(
                    "engine.tools.edit.cannotWrite",
                    i18n_params!["path" => path, "error" => error],
                )
                .render(),
            ));
        }
        let display = resolved
            .strip_prefix(sandbox.primary())
            .unwrap_or(&resolved)
            .display();
        Some(ok_result(
            LocalizedText::with_params(
                "engine.tools.edit.edited",
                i18n_params!["display" => display],
            )
            .render(),
        ))
    }
}
