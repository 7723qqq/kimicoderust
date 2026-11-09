use super::*;

const DEFAULT_MAX_CHARS: usize = 100_000;

const DEFAULT_MAX_CHARS_LIMIT: usize = 500_000;

const RANGE_STATUS_TOKENS: usize = 256;

fn read_limits() -> (usize, usize) {
    let Ok((config, _)) = crate::config::KimiConfig::discover() else {
        return (DEFAULT_MAX_CHARS, DEFAULT_MAX_CHARS_LIMIT);
    };
    let (default_max_chars, max_chars) = config.resolve_read_limits();
    let max_chars = max_chars
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(DEFAULT_MAX_CHARS_LIMIT);
    (
        default_max_chars
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(DEFAULT_MAX_CHARS)
            .min(max_chars),
        max_chars,
    )
}

fn estimate_read_tokens(text: &str) -> usize {
    let mut ascii = 0usize;
    let mut non_ascii = 0usize;
    for &byte in text.as_bytes() {
        if byte < 0x80 {
            ascii += 1;
        } else if byte >= 0xC0 {
            non_ascii += 1;
        }
    }
    ascii.div_ceil(4) + non_ascii
}

fn read_io_error(path: &str, error: &std::io::Error) -> ExecutableToolResult {
    err_result(
        LocalizedText::with_params(
            "engine.tools.read.cannotReadDetail",
            i18n_params!["path" => path, "e" => error],
        )
        .render(),
    )
}

impl NativeToolset {
    pub(super) fn resolve(
        sandbox: &Sandbox,
        bridge: &crate::native::shell_path_bridge::ShellPathBridge,
        path: &str,
    ) -> Option<PathBuf> {
        Self::resolve_io(sandbox, bridge, path).ok()
    }

    pub(super) fn resolve_io(
        sandbox: &Sandbox,
        bridge: &crate::native::shell_path_bridge::ShellPathBridge,
        path: &str,
    ) -> std::io::Result<PathBuf> {
        if let Some(blob) = crate::server::files::resolve_attachment_reference(
            &crate::server::files::FileStore::new(),
            path,
        ) {
            return Ok(blob);
        }

        let candidate = Self::candidate_path(sandbox.primary(), bridge, path);
        std::fs::canonicalize(&candidate)
    }

    pub(super) fn resolve_for_write(
        sandbox: &Sandbox,
        bridge: &crate::native::shell_path_bridge::ShellPathBridge,
        path: &str,
    ) -> Option<PathBuf> {
        Some(Self::candidate_path(sandbox.primary(), bridge, path))
    }

    pub(super) fn candidate_path(
        root: &Path,
        bridge: &crate::native::shell_path_bridge::ShellPathBridge,
        path: &str,
    ) -> PathBuf {
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

        if let Some(refusal) = Self::file_access_refusal(
            sandbox,
            bridge,
            path,
            crate::native::path_access::PathAccessOperation::Read,
            &crate::native::path_access::WorkspaceAccessPolicy::default(),
        ) {
            return Some(err_result(refusal));
        }

        if args.get("region").is_some_and(|v| !v.is_null())
            || args
                .get("full_resolution")
                .is_some_and(|v| v.as_bool() == Some(true))
        {
            return None;
        }

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
                    return Some(err_result("line_offset must not be 0".into()));
                }
                (n, n.unsigned_abs() as usize)
            }
        };
        let mut offset = if line_offset > 0 {
            line_offset as usize
        } else {
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
        let (default_max_chars, max_chars_limit) = read_limits();
        let max_chars_requested = match args.get("max_chars") {
            None | Some(Value::Null) => default_max_chars,
            Some(v) => match parse_integer_arg(v).filter(|n| *n >= 0) {
                Some(n) => (n as usize).max(1),
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
        let max_chars = max_chars_requested.min(max_chars_limit);

        let n_lines_arg = match args.get("n_lines") {
            None | Some(Value::Null) => None,
            Some(v) => match parse_integer_arg(v).filter(|n| *n >= 0) {
                Some(n) => Some(n as usize),
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
        let n_lines = n_lines_arg.unwrap_or(usize::MAX);

        let resolved = match Self::resolve_io(sandbox, bridge, path) {
            Ok(resolved) => resolved,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Some(err_result(
                    LocalizedText::with_params(
                        "engine.tools.read.notExist",
                        i18n_params!["path" => path],
                    )
                    .render(),
                ));
            }
            Err(error) => return Some(read_io_error(path, &error)),
        };
        let meta = match std::fs::metadata(&resolved) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Some(err_result(
                    LocalizedText::with_params(
                        "engine.tools.read.notExist",
                        i18n_params!["path" => path],
                    )
                    .render(),
                ));
            }
            Err(error) => return Some(read_io_error(path, &error)),
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
            return Some(err_result(
                LocalizedText::with_params(
                    "engine.tools.read.notUtf8",
                    i18n_params!["path" => path],
                )
                .render(),
            ));
        }

        if tail_lines > 0 && detection.encoding == encoding::UtfTextEncoding::Utf8 {
            let counted = {
                use std::io::BufRead;
                let mut probe = std::io::BufReader::new(&mut file);
                let mut buf = Vec::new();
                let mut count = 0usize;
                loop {
                    buf.clear();
                    let n = match probe.read_until(b'\n', &mut buf) {
                        Ok(n) => n,
                        Err(error) => return Some(read_io_error(path, &error)),
                    };
                    if n == 0 {
                        break;
                    }
                    count += 1;
                }
                count
            };
            if let Err(error) = std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(0)) {
                return Some(read_io_error(path, &error));
            }
            offset = counted.saturating_sub(tail_lines) + 1;
        }

        let (mut all, encoding_note, total_lines): (
            Vec<String>,
            Option<encoding::UtfTextEncoding>,
            usize,
        ) = if detection.encoding != encoding::UtfTextEncoding::Utf8 {
            if meta.len() > READ_MAX_BYTES {
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
                let n = match reader.read_until(b'\n', &mut raw) {
                    Ok(n) => n,
                    Err(error) => return Some(read_io_error(path, &error)),
                };
                if n == 0 {
                    break;
                }
                total += 1;
                if total < offset || all.len() >= n_lines {
                    continue;
                }
                if raw.contains(&0) {
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

        let tail_reread_guard = tail_lines > 0
            && detection.encoding == encoding::UtfTextEncoding::Utf8
            && n_lines_arg.is_some_and(|requested| requested < tail_lines);
        if tail_reread_guard && let Ok(current) = std::fs::metadata(&resolved) {
            let mut changed = !current.is_file() || current.len() != meta.len();
            if current.modified().ok() != meta.modified().ok() {
                changed = true;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if current.ino() != meta.ino() {
                    changed = true;
                }
            }
            if changed {
                return Some(err_result(
                    "File changed while reading its tail. Retry Read with the updated file."
                        .to_string(),
                ));
            }
        }

        if let Some(first) = all.first_mut()
            && let Some(stripped) = first.strip_prefix('\u{FEFF}')
        {
            *first = stripped.to_string();
        }

        let window_bytes: Vec<u8> = all
            .iter()
            .flat_map(|l| {
                let mut b = l.as_bytes().to_vec();
                b.push(b'\n');
                b
            })
            .collect();
        let style = encoding::detect_line_ending_style(&window_bytes);

        if offset > total_lines && offset > 1 {
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

        let mut out = String::new();
        let mut max_chars_reached = false;
        let mut rendered_count = 0usize;
        let mut rendered_lines: Vec<String> = Vec::new();
        let mut resume_continuation: Option<(usize, usize)> = None;

        for (i, raw) in all[start..end].iter().enumerate() {
            let mut rendered: String = (*raw).to_string();
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
            if style == encoding::LineEndingStyle::Mixed {
                rendered = encoding::make_carriage_returns_visible(&rendered);
            }
            let current_line_num = offset + i;
            let rendered_line = format!("{}\t{}", current_line_num, rendered);

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
            out.push_str(&rendered_line);
            out.push('\n');
            rendered_lines.push(rendered_line);
            rendered_count += 1;
        }

        if !max_chars_reached
            && rendered_count > 1
            && column_offset == 0
            && (n_lines_arg.is_some() || tail_lines > 0)
        {
            let token_limit = max_chars_limit.div_ceil(4);
            let status_tokens = token_limit.saturating_sub(RANGE_STATUS_TOKENS);
            if estimate_read_tokens(&out) > status_tokens {
                let range_start = offset;
                let range_end = offset + rendered_count - 1;
                let indexes: Vec<usize> = if tail_lines > 0 {
                    (0..rendered_count).rev().collect()
                } else {
                    (0..rendered_count).collect()
                };

                let mut fit = 0usize;
                let mut ascii = 0usize;
                let mut non_ascii = 0usize;
                for &index in &indexes {
                    if fit > 0 {
                        ascii += 1;
                    }
                    for &byte in rendered_lines[index].as_bytes() {
                        if byte < 0x80 {
                            ascii += 1;
                        } else if byte >= 0xC0 {
                            non_ascii += 1;
                        }
                    }
                    if ascii.div_ceil(4) + non_ascii > status_tokens {
                        break;
                    }
                    fit += 1;
                }
                let lines = fit.max(1);
                let suggestion_offset = if tail_lines > 0 {
                    range_end - lines + 1
                } else {
                    range_start
                };
                let suggestion = serde_json::json!({
                    "path": path,
                    "line_offset": suggestion_offset,
                    "n_lines": lines,
                });
                return Some(err_result(format!(
                    "Lines {range_start}-{range_end} are more than one Read can return (about {token_limit} estimated tokens). Read at most {lines} lines at a time, for example {suggestion}, or find the section with Grep."
                )));
            }
        }

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

        let range_complete = !max_chars_reached && rendered_count == all.len();
        parts.push(
            if range_complete {
                "Requested range complete."
            } else {
                "Character limit reached."
            }
            .to_string(),
        );
        parts.push(format!("Effective max_chars: {max_chars}."));
        if max_chars_requested > max_chars_limit {
            parts.push(format!(
                "Requested max_chars={max_chars_requested} was capped at the configured maximum {max_chars_limit}."
            ));
        }
        let last_line = offset + rendered_count.saturating_sub(1);
        if (rendered_count > 0 && last_line >= total_lines) || offset > total_lines {
            parts.push("End of file reached.".to_string());
        }
        if let Some((next_line, next_col)) = resume_continuation {
            let mut next = serde_json::Map::new();
            next.insert("path".into(), serde_json::Value::String(path.to_string()));
            next.insert("line_offset".into(), serde_json::json!(next_line));
            if next_col != 0 {
                next.insert("column_offset".into(), serde_json::json!(next_col));
            }
            let remaining = if tail_lines > 0 {
                tail_lines.saturating_sub(rendered_count)
            } else {
                n_lines_arg
                    .map(|requested| requested.saturating_sub(rendered_count))
                    .unwrap_or(0)
            };
            if remaining > 0 {
                next.insert("n_lines".into(), serde_json::json!(remaining));
            }
            parts.push(format!("Next Read: {}", serde_json::Value::Object(next)));
            if rendered_count > 0 {
                parts.push(
                    "If the answer may be further on, continue with Next Read or find the section with Grep instead of answering from this page alone.".into(),
                );
            }
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
            tracing::warn!(
                session = %ctx.session_id,
                turn_id,
                path = %resolved.display(),
                error = %e,
                "file-history record failed; undo will not cover this change"
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

        let Some(resolved) = Self::resolve_for_write(sandbox, bridge, path) else {
            return Some(err_result(
                LocalizedText::with_params(
                    "engine.tools.write.outsideWorkspace",
                    i18n_params!["path" => path],
                )
                .render(),
            ));
        };

        let pre_image = if resolved.is_file() {
            match std::fs::read_to_string(&resolved) {
                Ok(text) => Some(text),
                Err(error) => {
                    tracing::warn!(
                        path = %resolved.display(),
                        error = %error,
                        "pre-image could not be read; undo will not restore the old contents"
                    );
                    None
                }
            }
        } else {
            None
        };

        let write_once = |content: &str| -> std::io::Result<usize> {
            match mode {
                "overwrite" => std::fs::write(&resolved, content).map(|()| content.len()),
                "append" => {
                    use std::io::Write;
                    std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&resolved)
                        .and_then(|mut f| f.write_all(content.as_bytes()).map(|()| content.len()))
                }

                _ => unreachable!("mode is validated against the schema enum"),
            }
        };
        let mut written = write_once(content);
        if let Err(error) = &written
            && error.kind() == std::io::ErrorKind::NotFound
        {
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
            written = write_once(content);
        }
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

        Some(ok_result(format!(
            "{} {bytes_written} bytes to {path}",
            if mode == "append" {
                "Appended"
            } else {
                "Wrote"
            }
        )))
    }

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

        if old == new {
            return Some(err_result(
                "No changes to make: old_string and new_string are exactly the same.".to_string(),
            ));
        }
        let Some(resolved) = Self::resolve_for_write(sandbox, bridge, path) else {
            return Some(err_result(
                LocalizedText::with_params(
                    "engine.tools.edit.outsideWorkspace",
                    i18n_params!["path" => path],
                )
                .render(),
            ));
        };

        if let Err(error) = std::fs::metadata(&resolved) {
            if error.kind() == std::io::ErrorKind::NotFound {
                return Some(err_result(
                    LocalizedText::with_params(
                        "engine.tools.edit.notExist",
                        i18n_params!["path" => path],
                    )
                    .render(),
                ));
            }
            return Some(err_result(
                LocalizedText::with_params(
                    "engine.tools.edit.cannotReadDetail",
                    i18n_params!["path" => path, "e" => error],
                )
                .render(),
            ));
        }

        let bytes = match std::fs::read(&resolved) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Some(err_result(
                    LocalizedText::with_params(
                        "engine.tools.edit.notExist",
                        i18n_params!["path" => path],
                    )
                    .render(),
                ));
            }
            Err(error) => {
                return Some(err_result(
                    LocalizedText::with_params(
                        "engine.tools.edit.cannotReadDetail",
                        i18n_params!["path" => path, "e" => error],
                    )
                    .render(),
                ));
            }
        };
        if bytes.contains(&0) {
            return Some(err_result(
                LocalizedText::with_params(
                    "engine.tools.edit.notUtf8",
                    i18n_params!["path" => path],
                )
                .render(),
            ));
        }
        let Ok(text) = String::from_utf8(bytes) else {
            return Some(err_result(
                LocalizedText::with_params(
                    "engine.tools.edit.notUtf8",
                    i18n_params!["path" => path],
                )
                .render(),
            ));
        };

        let view = crate::native::line_endings::to_model_text_view(&text);
        let line_ending_style = view.line_ending_style;
        let model_text = view.text;
        let occurrence_count = model_text.matches(old).count();
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
            model_text.replace(old, new)
        } else {
            if occurrence_count == 0 {
                return Some(err_result(
                    LocalizedText::with_params(
                        "engine.tools.edit.oldStringNotFound",
                        i18n_params!["path" => path],
                    )
                    .render(),
                ));
            }
            if occurrence_count > 1 {
                return Some(err_result(
                    LocalizedText::with_params(
                        "engine.tools.edit.oldStringMatchedMultiple",
                        i18n_params!["occurrence_count" => occurrence_count, "path" => path],
                    )
                    .render(),
                ));
            }
            model_text.replacen(old, new, 1)
        };
        let updated_raw =
            crate::native::line_endings::materialize_model_text(&updated, line_ending_style);

        if let Err(error) = std::fs::write(&resolved, &updated_raw) {
            return Some(err_result(
                LocalizedText::with_params(
                    "engine.tools.edit.cannotWrite",
                    i18n_params!["path" => path, "error" => error],
                )
                .render(),
            ));
        }

        Self::record_file_history(&resolved, Some(text.as_str()), Some(updated_raw.as_str()));

        let occurrence_word = if occurrence_count == 1 {
            "occurrence"
        } else {
            "occurrences"
        };
        Some(ok_result(
            LocalizedText::with_params(
                "engine.tools.edit.edited",
                i18n_params![
                    "occurrence_count" => occurrence_count,
                    "occurrence_word" => occurrence_word,
                    "path" => path
                ],
            )
            .render(),
        ))
    }
}
