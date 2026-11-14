use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_util::future::{BoxFuture, FutureExt};

use crate::_base::text::code_points::{code_point_boundary_start, slice_on_code_point_boundary};
use crate::agent::scope_context::scope_context::IAgentScopeContext;
use crate::agent::tool_executor::tool_executor::{ExecutableToolOutput, ToolResult};
use crate::app::bootstrap::bootstrap::IBootstrapService;
use crate::human::llm::message::ContentPart;
use crate::persistence::interface::storage::{IFileSystemStorageService, StorageWriteOptions};
use crate::tool::output_pointer::{
    AppendedPointerInput, OutputUnit, ReplacedPointerInput, UnpersistedPointerInput,
    render_appended_pointer, render_replaced_pointer, render_unpersisted_pointer,
};
use crate::tool::tool_contract::{DEFAULT_TOOL_RESULT_MAX_CHARS, DEFAULT_TOOL_RESULT_MAX_RETAINED_CHARS};
use crate::wire::record::AGENT_WIRE_RECORD_KEY;

use super::tool_result_truncation::{IAgentToolResultTruncationService, ToolResultTruncationInput};

const TOOL_RESULT_PREVIEW_HEAD_CHARS: usize = 4_096;
const TOOL_RESULT_PREVIEW_TAIL_CHARS: usize = 1_024;
const TOOL_RESULT_MAX_LINE_CHARS: usize = 2_000;
const TRUNCATION_MARKER: &str = "[...truncated]";

struct ShapedOutput {
    output: ExecutableToolOutput,
    text_chars: usize,
    has_media: bool,
}

struct SavedToolResult {
    output_path: String,
    preserved_chars: usize,
}

#[derive(Clone)]
pub struct ToolResultTruncationService {
    bootstrap: Arc<dyn IBootstrapService>,
    storage: Arc<dyn IFileSystemStorageService>,
    storage_scope: String,
}

impl ToolResultTruncationService {
    pub fn new(
        bootstrap: Arc<dyn IBootstrapService>,
        agent: Arc<dyn IAgentScopeContext>,
        storage: Arc<dyn IFileSystemStorageService>,
    ) -> Self {
        Self {
            bootstrap,
            storage,
            storage_scope: agent.scope(Some("tool-results")),
        }
    }

    async fn truncate(&self, input: ToolResultTruncationInput) -> ToolResult {
        let mut result = input.result;
        if result.spill_exempt == Some(true) {
            return result;
        }
        let raw_text = persistable_tool_result_text(&result.output);
        let raw_chars = utf16_len(&raw_text);
        if raw_chars <= DEFAULT_TOOL_RESULT_MAX_CHARS {
            return result;
        }
        let spill = result.spill.take();
        let retained_text = if raw_chars <= DEFAULT_TOOL_RESULT_MAX_RETAINED_CHARS {
            raw_text.clone()
        } else {
            slice_on_code_point_boundary(&raw_text, DEFAULT_TOOL_RESULT_MAX_RETAINED_CHARS)
        };
        let shaped = shape_output(&result.output, TOOL_RESULT_MAX_LINE_CHARS);
        let total_chars = spill
            .as_ref()
            .and_then(|spill| spill.total_chars)
            .unwrap_or(raw_chars);
        let suffix = spill
            .as_ref()
            .and_then(|spill| spill.suffix.clone())
            .unwrap_or_default();
        let saved = match spill.as_ref().and_then(|spill| spill.output_path.clone()) {
            Some(output_path) => Some(SavedToolResult {
                output_path,
                preserved_chars: total_chars,
            }),
            None => {
                self.save_tool_result(&input.tool_name, &input.tool_call_id, &retained_text)
                    .await
            }
        };
        let Some(saved) = saved else {
            let fallback = render_unpersisted_tool_result(
                &input.tool_name,
                &input.tool_call_id,
                &retained_text,
                total_chars,
                DEFAULT_TOOL_RESULT_MAX_CHARS,
                &suffix,
            );
            result.output = merge_spill_pointer(shaped.output, fallback);
            result.truncated = Some(true);
            return result;
        };
        if shaped.text_chars <= DEFAULT_TOOL_RESULT_MAX_CHARS {
            let inline_suffix = drop_inline_suffix_lines(
                &suffix,
                &persistable_tool_result_text(&shaped.output),
            );
            let pointer = render_appended_spill_pointer(
                &saved.output_path,
                saved.preserved_chars,
                total_chars,
                &inline_suffix,
                shaped.has_media,
            );
            result.output = append_to_tool_result_output(shaped.output, &pointer);
            result.truncated = Some(true);
            return result;
        }
        let pointer = render_persisted_tool_result(
            &input.tool_name,
            &input.tool_call_id,
            &retained_text,
            &saved.output_path,
            saved.preserved_chars,
            total_chars,
            DEFAULT_TOOL_RESULT_MAX_CHARS,
            &suffix,
            shaped.has_media,
        );
        result.output = merge_spill_pointer(shaped.output, pointer);
        result.truncated = Some(true);
        result
    }

    async fn save_tool_result(
        &self,
        tool_name: &str,
        tool_call_id: &str,
        text: &str,
    ) -> Option<SavedToolResult> {
        let key = format!(
            "{}-{}.txt",
            safe_tool_result_file_stem(tool_name, tool_call_id),
            uuid::Uuid::new_v4()
        );
        self.storage
            .write(
                &self.storage_scope,
                &key,
                text.as_bytes().to_vec(),
                Some(StorageWriteOptions {
                    atomic: Some(true),
                    signal: None,
                }),
            )
            .await
            .ok()?;
        Some(SavedToolResult {
            output_path: join_normalized(&[
                self.bootstrap.home_dir(),
                self.storage_scope.clone(),
                key,
            ]),
            preserved_chars: utf16_len(text),
        })
    }
}

impl IAgentToolResultTruncationService for ToolResultTruncationService {
    fn truncate_for_model(
        &self,
        input: ToolResultTruncationInput,
    ) -> BoxFuture<'static, Result<ToolResult, crate::wire::record::WireAsyncError>> {
        let this = self.clone();
        async move { Ok(this.truncate(input).await) }.boxed()
    }

    fn is_spill_file_path(&self, path: &str) -> bool {
        let dir = normalize_path(Path::new(&self.bootstrap.home_dir()).join(&self.storage_scope));
        let normalized = normalize_path(PathBuf::from(path));
        normalized == dir || normalized.starts_with(&format!("{dir}/"))
    }

    fn is_wire_journal_path(&self, path: &str) -> bool {
        let sessions_dir = normalize_path(
            Path::new(&self.bootstrap.home_dir()).join(self.bootstrap.scope("sessions")),
        );
        let normalized = normalize_path(PathBuf::from(path));
        normalized.starts_with(&format!("{sessions_dir}/"))
            && Path::new(&normalized)
                .file_name()
                .and_then(|name| name.to_str())
                == Some(AGENT_WIRE_RECORD_KEY)
    }
}

fn shape_output(output: &ExecutableToolOutput, max_line_chars: usize) -> ShapedOutput {
    match output {
        ExecutableToolOutput::Text(text) => {
            let shaped = shape_string_per_line(text, max_line_chars);
            ShapedOutput {
                text_chars: utf16_len(&shaped),
                output: ExecutableToolOutput::Text(shaped),
                has_media: false,
            }
        }
        ExecutableToolOutput::Parts(parts) => {
            let mut out = Vec::with_capacity(parts.len());
            let mut text_chars = 0;
            let mut has_media = false;
            for part in parts {
                match part {
                    ContentPart::Text { text, .. } => {
                        let shaped = shape_string_per_line(text, max_line_chars);
                        text_chars += utf16_len(&shaped);
                        out.push(ContentPart::Text {
                            text: shaped,
                            meta: None,
                        });
                    }
                    ContentPart::Think {
                        think, encrypted, ..
                    } => {
                        text_chars += utf16_len(think)
                            + encrypted.as_deref().map(utf16_len).unwrap_or(0);
                        out.push(part.clone());
                    }
                    _ => {
                        has_media = true;
                        out.push(part.clone());
                    }
                }
            }
            ShapedOutput {
                output: ExecutableToolOutput::Parts(out),
                text_chars,
                has_media,
            }
        }
    }
}

fn shape_string_per_line(text: &str, max_line_chars: usize) -> String {
    let mut out = String::with_capacity(text.len());
    let mut start = 0;
    let bytes = text.as_bytes();
    let mut index = 0;
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
        shape_line_into(&mut out, &text[start..end], max_line_chars);
        start = end;
    }
    if start < bytes.len() {
        shape_line_into(&mut out, &text[start..], max_line_chars);
    }
    out
}

fn shape_line_into(out: &mut String, line: &str, max_line_chars: usize) {
    if utf16_len(line) <= max_line_chars {
        out.push_str(line);
        return;
    }
    let body_len = line.trim_end_matches(['\r', '\n']).len();
    let (body, line_break) = line.split_at(body_len);
    let suffix = format!("{TRUNCATION_MARKER}{line_break}");
    let suffix_chars = utf16_len(&suffix);
    let effective_max = max_line_chars.max(suffix_chars);
    out.push_str(&slice_on_code_point_boundary(
        body,
        effective_max.saturating_sub(suffix_chars),
    ));
    out.push_str(&suffix);
}

fn persistable_tool_result_text(output: &ExecutableToolOutput) -> String {
    match output {
        ExecutableToolOutput::Text(text) => text.clone(),
        ExecutableToolOutput::Parts(parts) => {
            let mut text = String::new();
            for part in parts {
                match part {
                    ContentPart::Text { text: part, .. } => text.push_str(part),
                    ContentPart::Think { think, .. } => text.push_str(think),
                    _ => {}
                }
            }
            text
        }
    }
}

fn merge_spill_pointer(output: ExecutableToolOutput, pointer: String) -> ExecutableToolOutput {
    match output {
        ExecutableToolOutput::Text(_) => ExecutableToolOutput::Text(pointer),
        ExecutableToolOutput::Parts(parts) => {
            let mut media = parts
                .into_iter()
                .filter(|part| !matches!(part, ContentPart::Text { .. } | ContentPart::Think { .. }))
                .collect::<Vec<_>>();
            if media.is_empty() {
                return ExecutableToolOutput::Text(pointer);
            }
            media.insert(
                0,
                ContentPart::Text {
                    text: pointer,
                    meta: None,
                },
            );
            ExecutableToolOutput::Parts(media)
        }
    }
}

fn render_appended_spill_pointer(
    output_path: &str,
    preserved_chars: usize,
    total_chars: usize,
    suffix: &str,
    has_media: bool,
) -> String {
    let pointer = render_appended_pointer(&AppendedPointerInput {
        path: output_path,
        total_size: total_chars,
        unit: OutputUnit::Chars,
        preserved: Some(preserved_chars),
        has_media,
    });
    if suffix.is_empty() {
        pointer
    } else {
        format!("{pointer}\n\n{suffix}")
    }
}

fn append_to_tool_result_output(output: ExecutableToolOutput, note: &str) -> ExecutableToolOutput {
    match output {
        ExecutableToolOutput::Text(output) => {
            if output.ends_with('\n') || output.is_empty() {
                ExecutableToolOutput::Text(format!("{output}{note}"))
            } else {
                ExecutableToolOutput::Text(format!("{output}\n{note}"))
            }
        }
        ExecutableToolOutput::Parts(mut parts) => {
            if let Some(ContentPart::Text { text, .. }) = parts.last_mut() {
                let text = std::mem::take(text);
                let last = parts.len() - 1;
                parts[last] = ContentPart::Text {
                    text: format!("{text}\n{note}"),
                    meta: None,
                };
            } else {
                parts.push(ContentPart::Text {
                    text: note.to_string(),
                    meta: None,
                });
            }
            ExecutableToolOutput::Parts(parts)
        }
    }
}

fn render_persisted_tool_result(
    tool_name: &str,
    tool_call_id: &str,
    preview_text: &str,
    output_path: &str,
    preserved_chars: usize,
    total_chars: usize,
    max_chars: usize,
    suffix: &str,
    has_media: bool,
) -> String {
    let mut lines = vec![render_replaced_pointer(&ReplacedPointerInput {
        path: output_path,
        total_size: total_chars,
        unit: OutputUnit::Chars,
        preserved: preserved_chars,
        max_size: max_chars,
        tool_name,
        tool_call_id,
        byte_size: (preserved_chars == utf16_len(preview_text))
            .then(|| preview_text.len()),
        has_media,
    })];
    append_preview_lines(&mut lines, preview_text);
    if !suffix.is_empty() {
        lines.push(String::new());
        lines.push(suffix.to_string());
    }
    lines.join("\n")
}

fn render_unpersisted_tool_result(
    tool_name: &str,
    tool_call_id: &str,
    preview_text: &str,
    total_chars: usize,
    max_chars: usize,
    suffix: &str,
) -> String {
    let mut lines = vec![render_unpersisted_pointer(&UnpersistedPointerInput {
        total_size: total_chars,
        unit: OutputUnit::Chars,
        max_size: max_chars,
        tool_name,
        tool_call_id,
    })];
    append_preview_lines(&mut lines, preview_text);
    if !suffix.is_empty() {
        lines.push(String::new());
        lines.push(suffix.to_string());
    }
    lines.join("\n")
}

fn append_preview_lines(lines: &mut Vec<String>, preview_text: &str) {
    let head = slice_on_code_point_boundary(preview_text, TOOL_RESULT_PREVIEW_HEAD_CHARS);
    let head_chars = utf16_len(&head);
    let preview_chars = utf16_len(preview_text);
    let tail_start = code_point_boundary_start(
        preview_text,
        head_chars.max(preview_chars.saturating_sub(TOOL_RESULT_PREVIEW_TAIL_CHARS)),
    );
    let tail = &preview_text[utf16_index_to_byte(preview_text, tail_start)..];
    lines.push(String::new());
    lines.push(format!("[preview: chars [0, {head_chars})]"));
    lines.push(head);
    if !tail.is_empty() {
        if tail_start > head_chars {
            lines.push(String::new());
            lines.push(format!("[elided: chars [{head_chars}, {tail_start})]"));
        }
        lines.push(String::new());
        lines.push(format!(
            "[preview: chars [{tail_start}, {preview_chars})]"
        ));
        lines.push(tail.to_string());
    }
}

fn drop_inline_suffix_lines(suffix: &str, shaped_text: &str) -> String {
    if suffix.is_empty() {
        return String::new();
    }
    let lines = suffix.split('\n').collect::<Vec<_>>();
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

fn safe_tool_result_file_stem(tool_name: &str, tool_call_id: &str) -> String {
    let mut raw = String::new();
    let mut in_run = false;
    for character in format!("{tool_name}-{tool_call_id}").chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
            raw.push(character);
            in_run = false;
        } else if !in_run {
            raw.push('_');
            in_run = true;
        }
    }
    let trimmed = raw.trim_matches('_');
    let label = trimmed.chars().take(80).collect::<String>();
    if label.is_empty() {
        "tool-result".to_string()
    } else {
        label
    }
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

fn utf16_index_to_byte(text: &str, index: usize) -> usize {
    let mut units = 0;
    for (byte, character) in text.char_indices() {
        if units == index {
            return byte;
        }
        units += character.len_utf16();
    }
    text.len()
}

fn join_normalized(parts: &[String]) -> String {
    let mut path = PathBuf::new();
    for part in parts {
        path.push(part);
    }
    normalize_path(path)
}

fn normalize_path(path: PathBuf) -> String {
    lexical_normalize(&path).to_string_lossy().replace('\\', "/")
}

fn lexical_normalize(path: &Path) -> PathBuf {
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
