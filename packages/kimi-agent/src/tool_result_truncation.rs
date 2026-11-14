use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::_base::text::code_points::{code_point_boundary_start, slice_on_code_point_boundary};
use crate::turn_loop::types::ToolResultSpill;

use crate::tool::tool_contract::{
    DEFAULT_TOOL_RESULT_MAX_CHARS, DEFAULT_TOOL_RESULT_MAX_RETAINED_CHARS,
};
const TOOL_RESULT_PREVIEW_HEAD_CHARS: usize = 4_096;
const TOOL_RESULT_PREVIEW_TAIL_CHARS: usize = 1_024;
const TOOL_RESULT_MAX_LINE_CHARS: usize = 2_000;
const TRUNCATION_MARKER: &str = "[...truncated]";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalizedToolResult {
    pub content: String,
    pub is_error: bool,
    pub note: Option<String>,

    pub spill_path: Option<String>,

    pub truncated: bool,
}

pub struct TruncationRequest<'a> {
    pub tool_name: &'a str,
    pub tool_call_id: &'a str,
    pub content: &'a str,
    pub is_error: bool,
    pub note: Option<&'a str>,
    pub spill_exempt: bool,
    pub has_media: bool,

    pub spill: Option<&'a ToolResultSpill>,
}

pub struct ToolResultTruncator {
    spill_dir: std::sync::Arc<std::sync::Mutex<PathBuf>>,
}

pub const SPILL_RETENTION: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 60 * 60);

impl ToolResultTruncator {
    pub fn new(spill_dir: PathBuf) -> Self {
        Self {
            spill_dir: std::sync::Arc::new(std::sync::Mutex::new(spill_dir)),
        }
    }

    fn spill_dir(&self) -> PathBuf {
        self.spill_dir
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub fn set_agent(
        &self,
        workspace_root: &Path,
        session_id: &str,
        agent_id: &str,
    ) -> std::io::Result<()> {
        let next = Self::for_agent(workspace_root, session_id, agent_id)?;
        *self
            .spill_dir
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = next.spill_dir();
        Ok(())
    }

    pub fn set_agent_in_home(
        &self,
        workspace_root: &Path,
        session_id: &str,
        agent_id: &str,
        home: &Path,
    ) {
        *self
            .spill_dir
            .lock()
            .unwrap_or_else(|error| error.into_inner()) =
            agent_spill_dir(workspace_root, session_id, agent_id, home);
    }

    pub fn for_workspace(workspace_root: &Path) -> Self {
        Self::new(workspace_root.join(".kimi").join("spill"))
    }

    pub fn for_agent(
        workspace_root: &Path,
        session_id: &str,
        agent_id: &str,
    ) -> std::io::Result<Self> {
        let home = std::env::var_os("KIMI_CODE_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("USERPROFILE")
                    .or_else(|| std::env::var_os("HOME"))
                    .map(|home| PathBuf::from(home).join(".kimi-code"))
            })
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "cannot resolve the home directory (KIMI_CODE_HOME / USERPROFILE / HOME unset)",
                )
            })?;
        Ok(Self::for_agent_in_home(
            workspace_root,
            session_id,
            agent_id,
            &home,
        ))
    }

    pub fn for_agent_in_home(
        workspace_root: &Path,
        session_id: &str,
        agent_id: &str,
        home: &Path,
    ) -> Self {
        Self::new(agent_spill_dir(workspace_root, session_id, agent_id, home))
    }

    pub fn is_spill_file_path(&self, path: &str) -> bool {
        let requested = lexical_normalize(Path::new(path));
        let spill = lexical_normalize(&self.spill_dir());
        requested.starts_with(&spill)
    }

    pub fn is_spill_read(&self, tool_name: &str, args: &serde_json::Value) -> bool {
        if !tool_name.eq_ignore_ascii_case("read") {
            return false;
        }
        let Some(path) = args.get("path").and_then(|value| value.as_str()) else {
            return false;
        };
        self.is_spill_file_path(path)
    }

    pub fn truncate_call(&self, req: TruncationRequest<'_>) -> FinalizedToolResult {
        self.truncate(req)
    }

    pub fn truncate(&self, req: TruncationRequest<'_>) -> FinalizedToolResult {
        if req.spill_exempt || req.content.is_empty() {
            return FinalizedToolResult {
                content: req.content.to_string(),
                is_error: req.is_error,
                note: req.note.map(String::from),
                spill_path: None,
                truncated: false,
            };
        }

        let raw_chars = utf16_len(req.content);
        if raw_chars <= DEFAULT_TOOL_RESULT_MAX_CHARS {
            return FinalizedToolResult {
                content: req.content.to_string(),
                is_error: req.is_error,
                note: req.note.map(String::from),
                spill_path: None,
                truncated: false,
            };
        }

        let retained =
            slice_on_code_point_boundary(req.content, DEFAULT_TOOL_RESULT_MAX_RETAINED_CHARS);
        let total_chars = req
            .spill
            .and_then(|spill| spill.total_chars)
            .unwrap_or(raw_chars);
        let suffix = req
            .spill
            .and_then(|spill| spill.suffix.as_deref())
            .unwrap_or("");
        let preserved_chars = utf16_len(&retained);

        let shaped = shape_per_line(req.content, TOOL_RESULT_MAX_LINE_CHARS);
        let shaped_chars = utf16_len(&shaped);

        let saved: Option<(String, usize)> =
            match req.spill.and_then(|spill| spill.output_path.as_deref()) {
                Some(path) => Some((path.to_string(), total_chars)),
                None => self
                    .save_spill(req.tool_name, req.tool_call_id, &retained)
                    .map(|path| (path, preserved_chars)),
            };
        let truncated = true;

        let final_content = match &saved {
            Some((path, preserved)) if shaped_chars <= DEFAULT_TOOL_RESULT_MAX_CHARS => {
                let inline_suffix = drop_inline_suffix_lines(suffix, &shaped);
                let pointer = render_appended_spill_pointer(
                    path,
                    *preserved,
                    total_chars,
                    &inline_suffix,
                    req.has_media,
                );
                append_to_output(&shaped, &pointer)
            }
            Some((path, preserved)) => render_persisted_pointer(
                req.tool_name,
                req.tool_call_id,
                &retained,
                path,
                *preserved,
                total_chars,
                suffix,
                req.has_media,
            ),
            None => render_unpersisted_pointer(
                req.tool_name,
                req.tool_call_id,
                &retained,
                total_chars,
                suffix,
            ),
        };

        FinalizedToolResult {
            content: final_content,
            is_error: req.is_error,
            note: req.note.map(String::from),
            spill_path: saved.map(|(path, _)| path),
            truncated,
        }
    }

    fn save_spill(&self, tool_name: &str, tool_call_id: &str, text: &str) -> Option<String> {
        let dir = self.spill_dir();
        if let Err(e) = fs::create_dir_all(&dir) {
            eprintln!("[kimi-agent] spill dir create failed: {e}");
            return None;
        }
        let stem = safe_stem(tool_name, tool_call_id);
        let suffix = uuid_v4();
        let filename = format!("{stem}-{suffix}.txt");
        let path = dir.join(&filename);
        let temp = dir.join(format!(".{filename}.tmp"));
        let mut f = match fs::File::create(&temp) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("[kimi-agent] spill file create failed: {e}");
                return None;
            }
        };
        if let Err(e) = f.write_all(text.as_bytes()) {
            eprintln!("[kimi-agent] spill file write failed: {e}");
            let _ = fs::remove_file(&temp);
            return None;
        }
        if let Err(e) = f.sync_all() {
            eprintln!("[kimi-agent] spill file sync failed: {e}");
        }
        drop(f);
        if let Err(e) = fs::rename(&temp, &path) {
            eprintln!("[kimi-agent] spill file rename failed: {e}");
            let _ = fs::remove_file(&temp);
            return None;
        }
        Some(path.to_string_lossy().into_owned())
    }

    pub fn cleanup_expired_spills(&self, max_age: std::time::Duration) -> usize {
        let mut pruned = 0;
        let now = std::time::SystemTime::now();
        if let Ok(entries) = fs::read_dir(self.spill_dir()) {
            for entry in entries.flatten() {
                if let Ok(meta) = entry.metadata()
                    && let Ok(modified) = meta.modified()
                    && let Ok(age) = now.duration_since(modified)
                    && age > max_age
                    && fs::remove_file(entry.path()).is_ok()
                {
                    pruned += 1;
                }
            }
        }
        pruned
    }
}

fn agent_spill_dir(
    workspace_root: &Path,
    session_id: &str,
    agent_id: &str,
    home: &Path,
) -> PathBuf {
    let workspace_id =
        crate::_base::workdir_slug::encode_workdir_key(&workspace_root.to_string_lossy());
    home.join("sessions")
        .join(workspace_id)
        .join(session_id)
        .join("agents")
        .join(agent_id)
        .join("tool-results")
}

pub(crate) fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    let mut normal_components = 0usize;
    for component in path.components() {
        match component {
            std::path::Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            std::path::Component::RootDir => out.push(std::path::MAIN_SEPARATOR.to_string()),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if normal_components > 0 {
                    out.pop();
                    normal_components -= 1;
                } else if !out.has_root() {
                    out.push("..");
                }
            }
            std::path::Component::Normal(value) => {
                out.push(value);
                normal_components += 1;
            }
        }
    }
    out
}

fn shape_per_line(text: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(text.len());
    let mut start = 0usize;
    let bytes = text.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        let end = match bytes[index] {
            b'\r' => {
                index += 1;
                if bytes.get(index) == Some(&b'\n') {
                    index += 1;
                }
                index
            }
            b'\n' => {
                index += 1;
                index
            }
            _ => {
                index += 1;
                continue;
            }
        };
        shape_line_into(&mut out, &text[start..end], max_chars);
        start = end;
    }
    if start < bytes.len() {
        shape_line_into(&mut out, &text[start..], max_chars);
    }
    out
}

fn shape_line_into(out: &mut String, line: &str, max_chars: usize) {
    if utf16_len(line) > max_chars {
        let body_len = line.trim_end_matches(['\r', '\n']).len();
        let (body, line_break) = line.split_at(body_len);
        let suffix = format!("{TRUNCATION_MARKER}{line_break}");
        let effective_max = max_chars.max(utf16_len(&suffix));
        let keep = effective_max.saturating_sub(utf16_len(&suffix));
        out.push_str(&slice_on_code_point_boundary(body, keep));
        out.push_str(&suffix);
    } else {
        out.push_str(line);
    }
}

fn append_to_output(shaped: &str, note: &str) -> String {
    if shaped.is_empty() || shaped.ends_with('\n') {
        format!("{shaped}{note}")
    } else {
        format!("{shaped}\n{note}")
    }
}

fn render_appended_spill_pointer(
    output_path: &str,
    preserved_chars: usize,
    total_chars: usize,
    suffix: &str,
    has_media: bool,
) -> String {
    let pointer = crate::tool::output_pointer::render_appended_pointer(
        &crate::tool::output_pointer::AppendedPointerInput {
            path: output_path,
            total_size: total_chars,
            unit: crate::tool::output_pointer::OutputUnit::Chars,
            preserved: Some(preserved_chars),
            has_media,
        },
    );
    if suffix.is_empty() {
        pointer
    } else {
        format!(
            "{pointer}

{suffix}"
        )
    }
}

fn render_persisted_pointer(
    tool_name: &str,
    tool_call_id: &str,
    preview_text: &str,
    output_path: &str,
    preserved_chars: usize,
    total_chars: usize,
    suffix: &str,
    has_media: bool,
) -> String {
    let mut lines: Vec<String> = crate::tool::output_pointer::render_replaced_pointer(
        &crate::tool::output_pointer::ReplacedPointerInput {
            path: output_path,
            total_size: total_chars,
            preserved: preserved_chars,
            max_size: DEFAULT_TOOL_RESULT_MAX_CHARS,
            tool_name,
            tool_call_id,
            unit: crate::tool::output_pointer::OutputUnit::Chars,
            byte_size: (preserved_chars == utf16_len(preview_text)).then(|| preview_text.len()),
            has_media,
        },
    )
    .lines()
    .map(str::to_string)
    .collect();
    append_preview_lines(&mut lines, preview_text);
    if !suffix.is_empty() {
        lines.push(String::new());
        lines.push(suffix.to_string());
    }
    lines.join(
        "
",
    )
}

fn render_unpersisted_pointer(
    tool_name: &str,
    tool_call_id: &str,
    preview_text: &str,
    total_chars: usize,
    suffix: &str,
) -> String {
    let mut lines: Vec<String> = crate::tool::output_pointer::render_unpersisted_pointer(
        &crate::tool::output_pointer::UnpersistedPointerInput {
            total_size: total_chars,
            unit: crate::tool::output_pointer::OutputUnit::Chars,
            max_size: DEFAULT_TOOL_RESULT_MAX_CHARS,
            tool_name,
            tool_call_id,
        },
    )
    .lines()
    .map(str::to_string)
    .collect();
    append_preview_lines(&mut lines, preview_text);
    if !suffix.is_empty() {
        lines.push(String::new());
        lines.push(suffix.to_string());
    }
    lines.join(
        "
",
    )
}

fn drop_inline_suffix_lines(suffix: &str, shaped_text: &str) -> String {
    if suffix.is_empty() {
        return String::new();
    }
    let lines: Vec<&str> = suffix.split('\n').collect();
    if !lines
        .iter()
        .any(|line| !line.is_empty() && shaped_text.contains(line))
    {
        return suffix.to_string();
    }
    lines
        .into_iter()
        .filter(|line| !line.is_empty() && !shaped_text.contains(line))
        .collect::<Vec<_>>()
        .join("\n")
}

fn append_preview_lines(lines: &mut Vec<String>, preview_text: &str) {
    let head = slice_on_code_point_boundary(preview_text, TOOL_RESULT_PREVIEW_HEAD_CHARS);
    let head_len = utf16_len(&head);
    let total_preview = utf16_len(preview_text);
    let tail_start = code_point_boundary_start(
        preview_text,
        head_len.max(total_preview.saturating_sub(TOOL_RESULT_PREVIEW_TAIL_CHARS)),
    );
    let tail = &preview_text[utf16_index_to_byte(preview_text, tail_start)..];
    lines.push(String::new());
    lines.push(format!("[preview: chars [0, {head_len})]"));
    lines.push(head);
    if !tail.is_empty() {
        if tail_start > head_len {
            lines.push(String::new());
            lines.push(format!("[elided: chars [{head_len}, {tail_start})]"));
        }
        lines.push(String::new());
        lines.push(format!("[preview: chars [{tail_start}, {total_preview})]"));
        lines.push(tail.to_string());
    }
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

fn utf16_index_to_byte(text: &str, index: usize) -> usize {
    let mut units = 0;
    for (byte, ch) in text.char_indices() {
        if units == index {
            return byte;
        }
        units += ch.len_utf16();
    }
    text.len()
}

fn safe_stem(tool_name: &str, tool_call_id: &str) -> String {
    let mut raw = String::new();
    let mut in_run = false;
    for c in format!("{tool_name}-{tool_call_id}").chars() {
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
            raw.push(c);
            in_run = false;
        } else if !in_run {
            raw.push('_');
            in_run = true;
        }
    }
    let trimmed = slice_on_code_point_boundary(raw.trim_matches('_'), 80);
    if trimmed.is_empty() {
        "tool-result".to_string()
    } else {
        trimmed
    }
}

fn uuid_v4() -> String {
    let mut bytes = [0u8; 16];
    for byte in &mut bytes {
        *byte = fastrand::u8(..);
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    fn small() -> ToolResultTruncator {
        ToolResultTruncator::new(env::temp_dir().join("kimi-agent-truncation-test"))
    }

    #[test]
    fn agent_truncator_uses_the_v2_scope_and_does_not_prune_existing_results() {
        let home = tempfile::tempdir().unwrap();
        let workspace = Path::new("C:\\work\\demo");
        let spill = home
            .path()
            .join("sessions")
            .join(crate::_base::workdir_slug::encode_workdir_key(
                workspace.to_str().unwrap(),
            ))
            .join("session-1")
            .join("agents")
            .join("agent-1")
            .join("tool-results");
        std::fs::create_dir_all(&spill).unwrap();
        let stale = spill.join("old.txt");
        std::fs::write(&stale, "old").unwrap();

        let truncator =
            ToolResultTruncator::for_agent_in_home(workspace, "session-1", "agent-1", home.path());
        let result_path = spill.join("result.txt");
        assert!(truncator.is_spill_read(
            "Read",
            &serde_json::json!({"path": result_path.to_string_lossy()})
        ));
        assert!(truncator.is_spill_read(
            "Read",
            &serde_json::json!({"path": spill.join("nested").join("..").join("result.txt").to_string_lossy()})
        ));
        assert!(!truncator.is_spill_read(
            "Read",
            &serde_json::json!({"path": spill.join("..").join("tool-results-evil").to_string_lossy()})
        ));
        assert!(stale.exists());

        truncator.set_agent_in_home(workspace, "session-2", "agent-2", home.path());
        let redirected = home
            .path()
            .join("sessions")
            .join(crate::_base::workdir_slug::encode_workdir_key(
                workspace.to_str().unwrap(),
            ))
            .join("session-2")
            .join("agents")
            .join("agent-2")
            .join("tool-results")
            .join("result.txt");
        assert!(truncator.is_spill_read(
            "Read",
            &serde_json::json!({"path": redirected.to_string_lossy()})
        ));
        assert!(!truncator.is_spill_read(
            "Read",
            &serde_json::json!({"path": result_path.to_string_lossy()})
        ));
    }

    #[test]
    fn workspace_truncator_does_not_prune_existing_spills() {
        let dir = tempfile::tempdir().unwrap();
        let spill = dir.path().join(".kimi").join("spill");
        std::fs::create_dir_all(&spill).unwrap();

        let stale = spill.join("old.txt");
        std::fs::write(&stale, "old").unwrap();
        let aged = std::time::SystemTime::now()
            .checked_sub(SPILL_RETENTION + std::time::Duration::from_secs(60))
            .expect("retention is representable");
        std::fs::File::options()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(aged))
            .unwrap();

        let fresh = spill.join("fresh.txt");
        std::fs::write(&fresh, "fresh").unwrap();

        let _ = ToolResultTruncator::for_workspace(dir.path());

        assert!(
            stale.exists(),
            "an existing spill is not pruned when the truncator is built"
        );
        assert!(fresh.exists(), "a recent spill is kept");
    }

    #[test]
    fn short_content_returns_unchanged() {
        let t = small();
        let r = t.truncate(TruncationRequest {
            tool_name: "Read",
            tool_call_id: "c1",
            content: "hello world",
            is_error: false,
            note: None,
            spill_exempt: false,
            has_media: false,
            spill: None,
        });
        assert_eq!(r.content, "hello world");
        assert!(!r.truncated);
        assert!(r.spill_path.is_none());
    }

    #[test]
    fn error_result_is_spilled_and_preserves_error_flag() {
        let t = small();
        let long = "x".repeat(100_000);
        let r = t.truncate(TruncationRequest {
            tool_name: "Bash",
            tool_call_id: "c2",
            content: &long,
            is_error: true,
            note: Some("boom"),
            spill_exempt: false,
            has_media: false,
            spill: None,
        });

        assert!(r.truncated);
        assert!(r.spill_path.is_some());

        assert!(r.is_error);
        assert_eq!(r.note.as_deref(), Some("boom"));
    }

    #[test]
    fn large_single_line_uses_inline_pointer_when_shaped_fits() {
        let t = small();
        let long = "a".repeat(60_000);
        let r = t.truncate(TruncationRequest {
            tool_name: "Read",
            tool_call_id: "c3",
            content: &long,
            is_error: false,
            note: None,
            spill_exempt: false,
            has_media: false,
            spill: None,
        });
        assert!(r.truncated);
        let path = r.spill_path.expect("spill path set");
        assert!(
            std::path::Path::new(&path).is_file(),
            "spill file exists at {path}"
        );

        assert!(r.content.contains("Per-line truncation occurred"));
        assert!(r.content.contains("output_path:"));
        assert!(r.content.contains(&path));

        let saved = fs::read_to_string(&path).unwrap();
        assert!(saved.chars().count() >= 50_000);
    }

    #[test]
    fn many_oversized_lines_use_persisted_pointer() {
        let t = small();
        let mut content = String::with_capacity(70_000);
        for _ in 0..30 {
            content.push_str(&"b".repeat(2_000));
            content.push('\n');
        }
        let r = t.truncate(TruncationRequest {
            tool_name: "Read",
            tool_call_id: "c3b",
            content: &content,
            is_error: false,
            note: None,
            spill_exempt: false,
            has_media: false,
            spill: None,
        });
        assert!(r.truncated);
        let path = r.spill_path.expect("spill path set");
        assert!(std::path::Path::new(&path).is_file());

        assert!(r.content.contains("Tool output exceeded"));
        assert!(r.content.contains("output_path:"));
        assert!(r.content.contains("tool_name: Read"));
        assert!(r.content.contains("tool_call_id: c3b"));
        assert!(r.content.contains("output_size_chars:"));
        assert!(r.content.contains("output_size_bytes:"));
        assert!(r.content.contains("[preview:"));
    }

    #[test]
    fn per_line_shape_caps_oversized_lines() {
        let t = small();
        let mut content = String::with_capacity(60_000);

        content.push_str(&"y".repeat(5_000));
        content.push('\n');

        content.push_str(&"z".repeat(60_000));
        let r = t.truncate(TruncationRequest {
            tool_name: "Grep",
            tool_call_id: "c4",
            content: &content,
            is_error: false,
            note: None,
            spill_exempt: false,
            has_media: false,
            spill: None,
        });
        assert!(r.truncated);

        assert!(r.content.contains(TRUNCATION_MARKER) || r.spill_path.is_some());
    }

    #[test]
    fn spill_failure_falls_back_to_unpersisted_pointer() {
        let blocker = env::temp_dir().join(format!("kimi-block-{}", uuid_v4()));
        fs::write(&blocker, b"i am a file").unwrap();
        let bad = blocker.join("subdir").join("file.txt");
        let t = ToolResultTruncator::new(bad);
        let long = "a".repeat(60_000);
        let r = t.truncate(TruncationRequest {
            tool_name: "Read",
            tool_call_id: "c5",
            content: &long,
            is_error: false,
            note: None,
            spill_exempt: false,
            has_media: false,
            spill: None,
        });
        assert!(r.spill_path.is_none());
        assert!(r.content.contains("could not be saved to a file"));
        fs::remove_file(&blocker).ok();
    }

    #[test]
    fn a_tool_supplied_spill_path_is_used_instead_of_a_second_copy() {
        let dir = tempfile::tempdir().unwrap();
        let tool_file = dir.path().join("task-output.log");
        fs::write(&tool_file, "the tool's own file").unwrap();
        let tool_path = tool_file.to_string_lossy().to_string();
        let truncator_dir = dir.path().join("truncator-spill");
        let t = ToolResultTruncator::new(truncator_dir.clone());
        let long = "a".repeat(60_000);
        let spill = ToolResultSpill {
            output_path: Some(tool_path.clone()),
            total_chars: Some(60_000),
            suffix: Some("Command failed with exit code: 1.".to_string()),
        };
        let r = t.truncate(TruncationRequest {
            tool_name: "Bash",
            tool_call_id: "c6",
            content: &long,
            is_error: true,
            note: None,
            spill_exempt: false,
            has_media: false,
            spill: Some(&spill),
        });
        assert!(r.truncated);
        assert_eq!(r.spill_path.as_deref(), Some(tool_path.as_str()));
        assert!(
            r.content.contains(&tool_path),
            "pointer names the tool's file: {}",
            r.content
        );
        assert!(r.content.ends_with("Command failed with exit code: 1."));
        assert!(
            !truncator_dir.exists(),
            "the truncator must not write its own copy when the tool spilled"
        );
    }

    #[test]
    fn a_spill_total_chars_reports_the_full_size() {
        let t = small();
        let long = "a".repeat(60_000);
        let spill = ToolResultSpill {
            output_path: None,
            total_chars: Some(20_000_000),
            suffix: None,
        };
        let r = t.truncate(TruncationRequest {
            tool_name: "Bash",
            tool_call_id: "c7",
            content: &long,
            is_error: false,
            note: None,
            spill_exempt: false,
            has_media: false,
            spill: Some(&spill),
        });
        assert!(r.truncated);
        assert!(r.spill_path.is_some());
        assert!(
            r.content.contains("of 20000000"),
            "pointer carries the producer's total: {}",
            r.content
        );
        assert!(!r.content.contains("output_size_bytes:"));
    }

    #[test]
    fn a_spill_suffix_survives_the_persisted_pointer() {
        let dir = tempfile::tempdir().unwrap();
        let tool_file = dir.path().join("shell-output.log");
        fs::write(&tool_file, "the tool's own file").unwrap();
        let t = ToolResultTruncator::new(dir.path().join("unused"));
        let mut content = String::with_capacity(70_000);
        for _ in 0..30 {
            content.push_str(&"b".repeat(2_000));
            content.push('\n');
        }
        let spill = ToolResultSpill {
            output_path: Some(tool_file.to_string_lossy().to_string()),
            total_chars: Some(100_000),
            suffix: Some("task_id: t1".to_string()),
        };
        let r = t.truncate(TruncationRequest {
            tool_name: "Bash",
            tool_call_id: "c8",
            content: &content,
            is_error: false,
            note: None,
            spill_exempt: false,
            has_media: false,
            spill: Some(&spill),
        });
        assert!(r.content.contains("output_size_chars: 100000"));
        assert!(
            r.content.ends_with("task_id: t1"),
            "suffix is the last line: {}",
            r.content
        );
    }
}

#[test]
fn spill_exempt_reads_are_never_truncated() {
    let dir = tempfile::tempdir().unwrap();
    let spill_dir = dir.path().join(".kimi").join("spill");
    std::fs::create_dir_all(&spill_dir).unwrap();
    let spilled = spill_dir.join("Read-toolu_1.txt");
    let body = "x".repeat(DEFAULT_TOOL_RESULT_MAX_CHARS + 1_000);
    std::fs::write(&spilled, &body).unwrap();

    let truncator = ToolResultTruncator::new(spill_dir.clone());
    let args = serde_json::json!({ "path": spilled.to_string_lossy() });
    assert!(truncator.is_spill_read("Read", &args));

    let outside = dir.path().join("ordinary.txt");
    let outside_args = serde_json::json!({ "path": outside.to_string_lossy() });
    assert!(!truncator.is_spill_read("Read", &outside_args));

    let result = truncator.truncate_call(TruncationRequest {
        tool_name: "Read",
        tool_call_id: "toolu_2",
        content: &body,
        is_error: false,
        note: None,
        spill_exempt: true,
        has_media: false,
        spill: None,
    });
    assert_eq!(result.content, body);
    assert!(!result.truncated);
    assert!(result.spill_path.is_none());
}

#[test]
fn a_media_tool_result_reports_that_media_stays_attached() {
    let t = ToolResultTruncator::new(std::env::temp_dir().join("kimi-agent-truncation-media-test"));
    let mut content = String::with_capacity(70_000);
    for _ in 0..30 {
        content.push_str(&"b".repeat(2_000));
        content.push('\n');
    }
    let r = t.truncate(TruncationRequest {
        tool_name: "Read",
        tool_call_id: "c-media",
        content: &content,
        is_error: false,
        note: None,
        spill_exempt: false,
        has_media: true,
        spill: None,
    });
    assert!(r.truncated);
    assert!(
        r.content
            .contains("media parts stay attached to this result"),
        "{}",
        r.content
    );
}

#[test]
fn carriage_return_lines_keep_their_break_when_shaped() {
    let mut content = String::with_capacity(60_000);
    content.push_str(&"a".repeat(3_000));
    content.push('\r');
    content.push_str(&"b".repeat(3_000));
    content.push_str("\r\n");
    content.push_str(&"c".repeat(60_000));
    let shaped = shape_per_line(&content, 2_000);
    assert!(shaped.contains(&format!("{TRUNCATION_MARKER}\r")));
    assert!(shaped.contains(&format!("{TRUNCATION_MARKER}\r\n")));
    assert!(shaped.ends_with(TRUNCATION_MARKER));
}

#[test]
fn spill_file_stems_collapse_invalid_runs_like_v2() {
    assert_eq!(safe_stem("A  B", "c"), "A_B-c");
    assert_eq!(safe_stem("A//B", "c"), "A_B-c");
    assert_eq!(safe_stem("___", ""), "-");
}

#[test]
fn shaping_uses_utf16_units_without_splitting_pairs() {
    assert_eq!(shape_per_line("😀😀", 3), TRUNCATION_MARKER);
    assert_eq!(shape_per_line("a😀b", 4), "a😀b");
}

#[test]
fn preview_uses_utf16_offsets_and_keeps_pairs_whole() {
    let text = format!("{}😀{}", "a".repeat(4095), "z".repeat(1024));
    let mut lines = Vec::new();
    append_preview_lines(&mut lines, &text);
    assert!(
        lines
            .iter()
            .any(|line| line == "[preview: chars [0, 4095)]"),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line == "[elided: chars [4095, 4097)]")
    );
    assert!(
        lines
            .iter()
            .any(|line| line == "[preview: chars [4097, 5121)]")
    );
    assert!(lines.iter().any(|line| line.starts_with('z')));
}
