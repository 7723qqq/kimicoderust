use crate::_base::utils::xml_escape as escape;
use crate::native::bash::{DEFAULT_TIMEOUT_S, MAX_TIMEOUT_S};
use crate::native::bash_spawn;
use crate::native::compaction::{self, CompactionConfigMeta, CompactionMessageMeta};
use crate::native::edit::{self, EditResult};
use crate::native::glob::{self, MAX_MATCHES};
use crate::native::grep::{self, DEFAULT_HEAD_LIMIT, GrepConfig, GrepResult, OutputMode};
use crate::native::image_compress;
use crate::native::list_directory::{self, ListDirectoryConfig, ListDirectoryResult};
use crate::native::output_truncate;
use crate::native::permission;
use crate::native::read::{self, MAX_BYTES, MAX_LINE_LENGTH, MAX_LINES, ReadConfig, ReadResult};
use crate::native::tool_access::{self, ToolAccessMeta};
use crate::native::tool_naming;
use crate::native::write::{self, WriteMode, WriteResult};
use napi::bindgen_prelude::JsFunction;
use napi::bindgen_prelude::Uint8Array;
use napi_derive::napi;
use std::collections::HashMap;
use std::sync::Arc;

#[napi]
pub fn set_engine_locale(locale: String) {
    if let Some(locale) = crate::i18n::Locale::from_name(&locale) {
        crate::i18n::set_locale(locale);
    }
}

#[napi]
pub fn translate(key: String, params: Option<HashMap<String, String>>) -> String {
    let i18n = crate::i18n::engine_i18n();
    i18n.translate_embedded(&key, params.as_ref())
        .unwrap_or(key)
}

#[napi]
pub async fn native_read(
    path: String,
    line_offset: Option<i64>,
    n_lines: Option<u32>,
) -> ReadResult {
    if let Some(cached) = crate::native::file_cache::FILE_CACHE.get(&path, line_offset, n_lines) {
        return cached;
    }

    let pre_read = crate::native::file_cache::snapshot(&path);

    let path_clone = path.clone();
    let ol = line_offset;
    let nl = n_lines;
    let result = tokio::task::spawn_blocking(move || {
        read::read_file(&ReadConfig {
            path: path_clone,
            line_offset: ol,
            n_lines: nl,
        })
    })
    .await
    .unwrap_or_else(|e| ReadResult {
        content: String::new(),
        line_count: 0,
        error: Some(format!("read panicked: {e}")),

        error_kind: Some("panic".to_string()),
    });

    if result.error.is_none()
        && let Some(pre) = pre_read
    {
        crate::native::file_cache::FILE_CACHE.put(path, line_offset, n_lines, result.clone(), pre);
    }

    result
}

#[napi]
pub async fn native_write(
    path: String,
    content: String,
    mode: Option<String>,
    atomic: Option<bool>,
) -> WriteResult {
    let write_mode = match mode.as_deref() {
        Some("append") => WriteMode::Append,
        _ => WriteMode::Overwrite,
    };
    let use_atomic = atomic.unwrap_or(true);
    let path_clone = path.clone();
    let result = tokio::task::spawn_blocking(move || {
        write::write_file(&path_clone, &content, write_mode, use_atomic)
    })
    .await
    .unwrap_or_else(|e| WriteResult {
        bytes_written: 0,
        error: Some(format!("write panicked: {e}")),

        error_kind: Some("panic".to_string()),
    });

    if result.error.is_none() {
        crate::native::file_cache::FILE_CACHE.invalidate(&path);
    }
    result
}

#[napi]
pub async fn native_edit(
    path: String,
    old_string: String,
    new_string: String,
    replace_all: Option<bool>,
) -> EditResult {
    let path_clone = path.clone();
    let result = tokio::task::spawn_blocking(move || {
        edit::edit_file(
            &path_clone,
            &old_string,
            &new_string,
            replace_all.unwrap_or(false),
        )
    })
    .await
    .unwrap_or_else(|e| EditResult {
        success: false,
        replacements: 0,
        error: Some(format!("edit panicked: {e}")),
    });
    if result.error.is_none() {
        crate::native::file_cache::FILE_CACHE.invalidate(&path);
    }
    result
}

#[napi]
#[allow(clippy::too_many_arguments)]
pub async fn native_grep(
    pattern: String,
    path: Option<String>,
    glob: Option<String>,
    file_type: Option<String>,
    output_mode: Option<String>,
    case_insensitive: Option<bool>,
    line_numbers: Option<bool>,
    after_context: Option<u32>,
    before_context: Option<u32>,
    context: Option<u32>,
    head_limit: Option<u32>,
    offset: Option<u32>,
    multiline: Option<bool>,
    include_ignored: Option<bool>,
    timeout_ms: Option<u32>,
) -> Result<GrepResult, napi::Error> {
    let mode = match output_mode.as_deref() {
        Some("content") => OutputMode::Content,
        Some("count_matches") => OutputMode::CountMatches,
        _ => OutputMode::FilesWithMatches,
    };

    let effective_before = context.unwrap_or(before_context.unwrap_or(0)) as usize;
    let effective_after = context.unwrap_or(after_context.unwrap_or(0)) as usize;

    let timeout = match timeout_ms {
        Some(0) => None,
        Some(ms) => Some(ms as u64),
        None => Some(grep::DEFAULT_TIMEOUT_MS),
    };

    let config = GrepConfig {
        pattern,
        path,
        glob,
        file_type,
        output_mode: mode,
        case_insensitive: case_insensitive.unwrap_or(false),
        line_numbers: line_numbers.unwrap_or(true),
        after_context: effective_after,
        before_context: effective_before,
        context: context.unwrap_or(0) as usize,
        head_limit: head_limit
            .map(|h| if h == 0 { usize::MAX } else { h as usize })
            .unwrap_or(DEFAULT_HEAD_LIMIT),
        offset: offset.unwrap_or(0) as usize,
        multiline: multiline.unwrap_or(false),
        include_ignored: include_ignored.unwrap_or(false),
        timeout_ms: timeout,
    };

    tokio::task::spawn_blocking(move || grep::grep_search(&config))
        .await
        .map_err(|e| napi::Error::from_reason(format!("grep task failed: {}", e)))
}

#[napi]
pub fn native_glob_matches_any(globs: Vec<String>, path: String) -> bool {
    glob::glob_matches_any(&globs, &path)
}

#[napi(object)]
pub struct NativeBashSpawnConfig {
    pub argv: Vec<String>,
    pub cwd: Option<String>,

    pub timeout_ms: Option<i64>,

    pub env: Option<Vec<Vec<String>>>,
}

#[napi(object)]
pub struct NativeBashSpawnResult {
    pub id: u32,
    pub pid: u32,
}

#[napi(object)]
pub struct NativeBashEvent {
    pub id: u32,
    pub kind: String,
    pub data: Option<String>,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
}

#[napi(object)]
pub struct NativeBashExitResult {
    pub exit_code: i32,
    pub timed_out: bool,
    pub error: Option<String>,
}

#[napi]
pub fn native_bash_spawn(
    config: NativeBashSpawnConfig,
    on_event: JsFunction,
) -> Result<NativeBashSpawnResult, napi::Error> {
    use napi::threadsafe_function::ThreadsafeFunction;

    let tsfn: ThreadsafeFunction<NativeBashEvent> = on_event
        .create_threadsafe_function(
            0,
            |ctx: napi::threadsafe_function::ThreadSafeCallContext<NativeBashEvent>| {
                ctx.env.create_object().and_then(|mut obj| {
                    obj.set("id", ctx.value.id)?;
                    obj.set("kind", ctx.value.kind)?;
                    if let Some(data) = ctx.value.data {
                        obj.set("data", data)?;
                    }
                    if let Some(exit_code) = ctx.value.exit_code {
                        obj.set("exitCode", exit_code)?;
                    }
                    if let Some(error) = ctx.value.error {
                        obj.set("error", error)?;
                    }
                    Ok(vec![obj])
                })
            },
        )
        .map_err(|e| napi::Error::from_reason(format!("create_threadsafe_function failed: {e}")))?;

    let env_pairs: Option<Vec<(String, String)>> = config.env.map(|pairs| {
        pairs
            .into_iter()
            .filter_map(|mut pair| {
                if pair.len() >= 2 {
                    let value = pair.pop()?;
                    let key = pair.pop()?;
                    Some((key, value))
                } else {
                    None
                }
            })
            .collect()
    });

    let callback: Arc<dyn Fn(bash_spawn::BashSpawnEvent) + Send + Sync> = Arc::new(move |event| {
        let _ = tsfn.call(
            Ok(NativeBashEvent {
                id: event.id,
                kind: event.kind.as_str().to_string(),
                data: event.data,
                exit_code: event.exit_code,
                error: event.error,
            }),
            napi::threadsafe_function::ThreadsafeFunctionCallMode::NonBlocking,
        );
    });

    let (id, pid) = bash_spawn::spawn_managed(
        &config.argv,
        config.cwd.as_deref(),
        config.timeout_ms,
        env_pairs.as_deref(),
        callback,
    )
    .map_err(napi::Error::from_reason)?;

    Ok(NativeBashSpawnResult { id, pid })
}

#[napi]
pub async fn native_bash_wait(id: u32) -> Option<NativeBashExitResult> {
    loop {
        if let Some(outcome) = bash_spawn::exit_snapshot(id) {
            return Some(NativeBashExitResult {
                exit_code: outcome.exit_code,
                timed_out: outcome.timed_out,
                error: outcome.error,
            });
        }
        if !bash_spawn::is_managed(id) {
            return None;
        }
        std::thread::sleep(bash_spawn::POLL_INTERVAL);
    }
}

#[napi]
pub fn native_bash_kill(id: u32) -> bool {
    bash_spawn::kill_managed(id)
}

#[napi]
pub fn native_bash_dispose(id: u32) -> bool {
    bash_spawn::dispose_managed(id)
}

#[napi]
pub const READ_MAX_LINES: u32 = MAX_LINES as u32;

#[napi]
pub const READ_MAX_LINE_LENGTH: u32 = MAX_LINE_LENGTH as u32;

#[napi]
pub const READ_MAX_BYTES: u32 = MAX_BYTES as u32;

#[napi]
pub const GLOB_MAX_MATCHES: u32 = MAX_MATCHES as u32;

#[napi]
pub const GREP_DEFAULT_HEAD_LIMIT: u32 = DEFAULT_HEAD_LIMIT as u32;

#[napi]
pub const BASH_DEFAULT_TIMEOUT: u32 = DEFAULT_TIMEOUT_S as u32;

#[napi]
pub const BASH_MAX_TIMEOUT: u32 = MAX_TIMEOUT_S as u32;

#[allow(dead_code)]
#[napi]
pub fn native_list_directory(
    path: Option<String>,
    collapse_hidden_dirs: Option<bool>,
) -> ListDirectoryResult {
    list_directory::list_directory(&ListDirectoryConfig {
        path,
        collapse_hidden_dirs: collapse_hidden_dirs.unwrap_or(false),
    })
}

use crate::native::file_type::{self, ImageDimensions};

#[napi]
pub fn native_sniff_image_dimensions(data: Uint8Array) -> Option<ImageDimensions> {
    file_type::sniff_image_dimensions(&data)
}

#[napi]
pub fn native_is_sensitive_file(path: String) -> bool {
    file_type::is_sensitive_file(&path)
}

#[napi]
pub fn native_is_sensitive_file_bytes(path: Uint8Array) -> bool {
    file_type::is_sensitive_file_bytes(&path)
}

#[napi(object)]
pub struct FileTypeResult {
    pub kind: String,
    pub mime_type: String,
}

#[napi]
pub fn native_detect_file_type(path: String, header: Uint8Array) -> FileTypeResult {
    let kind = file_type::detect_file_type(std::path::Path::new(&path), &header);
    let kind_str = match kind {
        file_type::FileKind::Text => "text",
        file_type::FileKind::Image => "image",
        file_type::FileKind::Video => "video",
        file_type::FileKind::Unknown => "unknown",
    };
    let mime = file_type::resolve_mime(std::path::Path::new(&path), &header);
    FileTypeResult {
        kind: kind_str.to_string(),
        mime_type: mime,
    }
}

use crate::native::tokens;

#[napi]
pub fn native_compute_compact_count(
    messages: Vec<CompactionMessageMeta>,
    config: CompactionConfigMeta,
    is_manual: bool,
) -> u32 {
    compaction::compute_compact_count(&messages, &config, is_manual)
}

#[napi]
pub fn native_reduce_compact_on_overflow(
    messages: Vec<CompactionMessageMeta>,
    config: CompactionConfigMeta,
) -> u32 {
    compaction::reduce_compact_on_overflow(&messages, &config)
}

#[napi]
pub fn native_can_split_after(messages: Vec<CompactionMessageMeta>, index: u32) -> bool {
    compaction::can_split_after(&messages, index as usize)
}

#[napi]
pub fn native_select_compaction_user_messages(
    messages: Vec<compaction::HandoffMessageMeta>,
    max_tokens: u32,
    head_tokens: u32,
) -> compaction::CompactionUserSelection {
    compaction::select_compaction_user_messages(&messages, max_tokens, head_tokens)
}

#[napi]
pub fn native_estimate_tokens(text: String) -> u32 {
    tokens::estimate_tokens(&text) as u32
}

#[napi]
pub fn native_estimate_tokens_batch(texts: Vec<String>) -> u32 {
    let refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
    tokens::estimate_tokens_batch(&refs) as u32
}

#[napi]
pub fn native_truncate_text_to_tokens_from_end(text: String, max_tokens: u32) -> String {
    tokens::truncate_text_to_tokens_from_end(&text, max_tokens as usize)
}

#[napi]
pub fn native_tool_accesses_conflict(
    left: Vec<ToolAccessMeta>,
    right: Vec<ToolAccessMeta>,
) -> bool {
    tool_access::conflict(&left, &right)
}

#[napi(object)]
pub struct NativeCompressImageConfig {
    pub max_edge: u32,

    pub byte_budget: u32,

    pub fallback_edges: Vec<u32>,

    pub jpeg_quality_steps: Vec<u32>,
}

#[napi(object)]
pub struct NativeCompressImageResult {
    pub data: Uint8Array,
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
    pub original_width: u32,
    pub original_height: u32,
    pub changed: bool,
    pub original_byte_length: u32,
    pub final_byte_length: u32,
}

#[napi(object)]
pub struct NativeCropImageConfig {
    pub max_edge: u32,
    pub byte_budget: u32,

    pub skip_resize: bool,
    pub fallback_edges: Vec<u32>,
    pub jpeg_quality_steps: Vec<u32>,
}

#[napi(object)]
pub struct NativeCropImageOutcome {
    pub ok: bool,

    pub error: String,

    pub error_kind: String,
    pub data: Uint8Array,
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
    pub original_width: u32,
    pub original_height: u32,
    pub region_x: u32,
    pub region_y: u32,
    pub region_width: u32,
    pub region_height: u32,
    pub resized: bool,
    pub original_byte_length: u32,
    pub final_byte_length: u32,
}

#[napi]
pub async fn native_compress_image(
    data: Uint8Array,
    mime_type: String,
    config: NativeCompressImageConfig,
) -> Result<Option<NativeCompressImageResult>, napi::Error> {
    let bytes = data.to_vec();
    let cfg = image_compress::CompressConfig {
        max_edge: config.max_edge,
        byte_budget: config.byte_budget as usize,
        fallback_edges: config.fallback_edges,
        jpeg_quality_steps: config
            .jpeg_quality_steps
            .into_iter()
            .map(|q| q as u8)
            .collect(),
    };
    let result = tokio::task::spawn_blocking(move || {
        image_compress::compress_image(&bytes, &mime_type, &cfg)
    })
    .await
    .map_err(|e| napi::Error::from_reason(format!("compress_image task failed: {}", e)))?;
    Ok(result.map(|r| NativeCompressImageResult {
        data: Uint8Array::from(r.data.as_slice()),
        mime_type: r.mime_type,
        width: r.width,
        height: r.height,
        original_width: r.original_width,
        original_height: r.original_height,
        changed: r.changed,
        original_byte_length: r.original_byte_length as u32,
        final_byte_length: r.final_byte_length as u32,
    }))
}

#[napi]
pub async fn native_crop_image(
    data: Uint8Array,
    mime_type: String,
    region_x: f64,
    region_y: f64,
    region_width: f64,
    region_height: f64,
    config: NativeCropImageConfig,
) -> Result<NativeCropImageOutcome, napi::Error> {
    let bytes = data.to_vec();
    let cfg = image_compress::CropConfig {
        max_edge: config.max_edge,
        byte_budget: config.byte_budget as usize,
        skip_resize: config.skip_resize,
        fallback_edges: config.fallback_edges,
        jpeg_quality_steps: config
            .jpeg_quality_steps
            .into_iter()
            .map(|q| q as u8)
            .collect(),
    };
    let result = tokio::task::spawn_blocking(move || {
        image_compress::crop_image(
            &bytes,
            &mime_type,
            region_x,
            region_y,
            region_width,
            region_height,
            &cfg,
        )
    })
    .await
    .map_err(|e| napi::Error::from_reason(format!("crop_image task failed: {}", e)))?;

    Ok(match result {
        Ok(r) => NativeCropImageOutcome {
            ok: true,
            error: String::new(),
            error_kind: String::new(),
            data: Uint8Array::from(r.data.as_slice()),
            mime_type: r.mime_type,
            width: r.width,
            height: r.height,
            original_width: r.original_width,
            original_height: r.original_height,
            region_x: r.region_x,
            region_y: r.region_y,
            region_width: r.region_width,
            region_height: r.region_height,
            resized: r.resized,
            original_byte_length: r.original_byte_length as u32,
            final_byte_length: r.final_byte_length as u32,
        },
        Err(err) => {
            let kind = err.kind().to_string();
            NativeCropImageOutcome {
                ok: false,
                error: err.error_message(),
                error_kind: kind,
                data: Uint8Array::from(&[]),
                mime_type: String::new(),
                width: 0,
                height: 0,
                original_width: 0,
                original_height: 0,
                region_x: 0,
                region_y: 0,
                region_width: 0,
                region_height: 0,
                resized: false,
                original_byte_length: 0,
                final_byte_length: 0,
            }
        }
    })
}

#[napi(object)]
pub struct NativeWriteChunkResult {
    pub output: String,

    pub chars_written: u32,

    pub new_nchars: u32,

    pub truncated: bool,
}

#[napi]
pub fn native_write_tool_output_chunk(
    text: String,
    current_nchars: u32,
    max_chars: u32,
    max_line_length: Option<u32>,
    already_truncated: bool,
) -> NativeWriteChunkResult {
    let result = output_truncate::write_chunk(
        &text,
        current_nchars as usize,
        max_chars as usize,
        max_line_length.map(|v| v as usize),
        already_truncated,
    );
    NativeWriteChunkResult {
        output: result.output,
        chars_written: result.chars_written as u32,
        new_nchars: result.new_nchars as u32,
        truncated: result.truncated,
    }
}

#[napi]
pub fn native_escape_xml(text: String) -> String {
    escape::escape_xml(&text)
}

#[napi]
pub fn native_escape_xml_attr(text: String) -> String {
    escape::escape_xml_attr(&text)
}

#[napi]
pub fn native_escape_xml_tags(text: String) -> String {
    escape::escape_xml_tags(&text)
}

#[napi]
pub fn native_sanitize_mcp_name_part(part: String) -> String {
    tool_naming::sanitize_mcp_name_part(&part)
}

#[napi]
pub fn native_is_mcp_tool_name(name: String) -> bool {
    tool_naming::is_mcp_tool_name(&name)
}

#[napi]
pub fn native_qualify_mcp_tool_name(server_name: String, tool_name: String) -> String {
    tool_naming::qualify_mcp_tool_name(&server_name, &tool_name)
}

#[napi]
pub fn native_read_engine_state(
    workspace_root: String,
    domain: String,
    session_id: Option<String>,
) -> Option<String> {
    match crate::storage::state_store::read_scoped_state(
        std::path::Path::new(&workspace_root),
        session_id.as_deref(),
        &domain,
    ) {
        Ok(value) => value,
        Err(cause) => {
            tracing::warn!(%cause, "engine state read failed; the host sees null");
            None
        }
    }
}

#[napi]
pub fn native_rollback_engine_state(workspace_root: String, session_id: String, count: u32) -> u32 {
    crate::storage::state_store::rollback_scoped_state(
        std::path::Path::new(&workspace_root),
        &session_id,
        count as usize,
    ) as u32
}

#[napi]
pub fn native_engine_state_checkpoint_depth(workspace_root: String, session_id: String) -> u32 {
    crate::storage::StateStore::for_session(std::path::Path::new(&workspace_root), &session_id)
        .map(|store| store.checkpoint_depth() as u32)
        .unwrap_or(0)
}

#[napi]
pub fn native_cron_next_fire(entry_json: String, from_ms: i64) -> Option<i64> {
    let entry: crate::cron::scheduler::CronEntry = serde_json::from_str(&entry_json).ok()?;
    crate::cron::scheduler::next_fire_for_entry(
        &entry,
        from_ms,
        crate::session::local_utc_offset_minutes(),
    )
}

use std::sync::Mutex;

use crate::native::workspace_index;

static WORKSPACE_INDEX: Mutex<Option<workspace_index::WorkspaceIndex>> = Mutex::new(None);

#[napi(object)]
pub struct NativeReadPrediction {
    pub line_count: i32,
    pub size: i64,
    pub preview: String,
    pub estimated_read_ms: i32,
}

#[napi]
pub fn native_build_workspace_index(root: String) -> u32 {
    let index = workspace_index::WorkspaceIndex::build(&root);
    let count = index.file_count();
    let mut guard = WORKSPACE_INDEX.lock().unwrap();
    *guard = Some(index);
    count as u32
}

#[napi]
pub fn native_workspace_index_predict_read(path: String) -> Option<NativeReadPrediction> {
    let guard = WORKSPACE_INDEX.lock().ok()?;
    let index = guard.as_ref()?;
    let prediction = index.predict_read(&path)?;
    Some(NativeReadPrediction {
        line_count: prediction.line_count as i32,
        size: prediction.size as i64,
        preview: prediction.preview,
        estimated_read_ms: prediction.estimated_read_ms as i32,
    })
}

use crate::native::path_access;

#[napi]
pub fn native_path_normalize_user_path(path: String, path_class: String) -> String {
    let class =
        path_access::PathClass::from_str(&path_class).unwrap_or(path_access::PathClass::Posix);
    path_access::normalize_user_path(&path, class)
}

#[napi]
pub fn native_path_expand_user_path(
    path: String,
    home_dir: Option<String>,
    path_class: String,
) -> String {
    let class =
        path_access::PathClass::from_str(&path_class).unwrap_or(path_access::PathClass::Posix);
    path_access::expand_user_path(&path, home_dir.as_deref(), class)
}

#[napi]
pub fn native_path_canonicalize(path: String, cwd: String, path_class: String) -> String {
    let class =
        path_access::PathClass::from_str(&path_class).unwrap_or(path_access::PathClass::Posix);
    match path_access::canonicalize_path(&path, &cwd, class) {
        Ok(p) => p,
        Err(e) => format!("ERROR: {e}"),
    }
}

#[napi]
pub fn native_path_is_within_directory(
    candidate: String,
    base: String,
    path_class: String,
) -> bool {
    let class =
        path_access::PathClass::from_str(&path_class).unwrap_or(path_access::PathClass::Posix);
    path_access::is_within_directory(&candidate, &base, class)
}

#[napi]
pub fn native_path_is_within_workspace(
    candidate: String,
    roots: Vec<String>,
    path_class: String,
) -> bool {
    let class =
        path_access::PathClass::from_str(&path_class).unwrap_or(path_access::PathClass::Posix);
    path_access::is_within_workspace(&candidate, &roots, class)
}

#[napi]
pub fn native_parse_permission_pattern(pattern: String) -> String {
    permission::native_parse_permission_pattern(pattern)
}

use crate::native::fetch_url;

#[napi(object)]
pub struct NativeFetchUrlResult {
    pub content: String,
    pub kind: String,
    pub status: u16,
    pub error: Option<String>,
}

#[napi]
pub async fn native_fetch_url(
    url: String,
    user_agent: Option<String>,
    max_bytes: Option<u32>,
    allow_private: Option<bool>,
    timeout_ms: Option<u32>,
) -> NativeFetchUrlResult {
    let config = fetch_url::FetchUrlConfig {
        url,
        user_agent: user_agent.unwrap_or_else(|| fetch_url::FetchUrlConfig::default().user_agent),
        max_bytes: max_bytes
            .map(|v| v as usize)
            .unwrap_or(fetch_url::FetchUrlConfig::default().max_bytes),
        allow_private: allow_private.unwrap_or(true),
        max_redirects: fetch_url::FetchUrlConfig::default().max_redirects,
        timeout_ms: timeout_ms
            .map(|v| v as u64)
            .unwrap_or(fetch_url::FetchUrlConfig::default().timeout_ms),
    };

    let result = tokio::task::spawn_blocking(move || fetch_url::fetch_url(&config))
        .await
        .unwrap_or_else(|e| fetch_url::FetchUrlResult {
            content: String::new(),
            kind: String::new(),
            status: 0,
            error: Some(format!("fetch_url panicked: {e}")),
        });

    NativeFetchUrlResult {
        content: result.content,
        kind: result.kind,
        status: result.status,
        error: result.error,
    }
}

#[napi(object)]
pub struct NativeLlmStreamConfig {
    pub provider: String,
    pub url: String,
    pub api_key: String,

    pub model: String,
    pub request_body: String,
    pub timeout_ms: Option<u32>,
    pub extra_headers: Option<Vec<NativeLlmStreamHeader>>,
}

#[napi(object)]
pub struct NativeLlmStreamHeader {
    pub key: String,
    pub value: String,
}

#[napi(object)]
#[derive(Clone)]
pub struct NativeLlmStreamPart {
    pub part_type: String,
    pub text: Option<String>,
    pub think: Option<String>,
    pub encrypted: Option<String>,
    pub id: Option<String>,
    pub name: Option<String>,
    pub arguments: Option<String>,
    pub arguments_part: Option<String>,
    pub stream_index: Option<u32>,
}

#[napi(object)]
pub struct NativeLlmStreamEvent {
    pub kind: String,
    pub part: Option<NativeLlmStreamPart>,
    pub metadata: Option<NativeLlmStreamMetadata>,
    pub error: Option<String>,
}

#[napi(object)]
pub struct NativeLlmStreamMetadata {
    pub response_id: Option<String>,
    pub finish_reason: Option<String>,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cached_tokens: u32,
    pub trace_id: Option<String>,
}

#[napi(object)]
pub struct NativeLlmStreamResult {
    pub parts: Vec<NativeLlmStreamPart>,
    pub metadata: NativeLlmStreamMetadata,
    pub error: Option<String>,
}

#[napi]
pub async fn native_llm_stream(config: NativeLlmStreamConfig) -> NativeLlmStreamResult {
    let stream_config = llm_stream::LlmStreamConfig {
        provider: config.provider,
        url: config.url,
        api_key: config.api_key,
        request_body: config.request_body,
        timeout_ms: config.timeout_ms.unwrap_or(120_000) as u64,
        extra_headers: config
            .extra_headers
            .unwrap_or_default()
            .into_iter()
            .map(|h| (h.key, h.value))
            .collect(),
    };

    let result = llm_stream::run_llm_stream(&stream_config).await;

    match result {
        Ok(events) => {
            let mut parts = Vec::new();
            let mut metadata = llm_stream::StreamMetadata::default();
            let mut error = None;

            for event in events {
                match event {
                    llm_stream::StreamEvent::Part(p) => {
                        parts.push(NativeLlmStreamPart {
                            part_type: p.part_type,
                            text: p.text,
                            think: p.think,
                            encrypted: p.encrypted,
                            id: p.id,
                            name: p.name,
                            arguments: p.arguments,
                            arguments_part: p.arguments_part,
                            stream_index: p.stream_index,
                        });
                    }
                    llm_stream::StreamEvent::Done(m) => {
                        metadata = m;
                    }
                    llm_stream::StreamEvent::Error(e) => {
                        error = Some(e);
                    }
                }
            }

            NativeLlmStreamResult {
                parts,
                metadata: NativeLlmStreamMetadata {
                    response_id: metadata.response_id,
                    finish_reason: metadata.finish_reason,
                    input_tokens: metadata.input_tokens,
                    output_tokens: metadata.output_tokens,
                    cached_tokens: metadata.cached_tokens,
                    trace_id: metadata.trace_id,
                },
                error,
            }
        }
        Err(e) => NativeLlmStreamResult {
            parts: Vec::new(),
            metadata: NativeLlmStreamMetadata {
                response_id: None,
                finish_reason: None,
                input_tokens: 0,
                output_tokens: 0,
                cached_tokens: 0,
                trace_id: None,
            },
            error: Some(e),
        },
    }
}

#[napi]
pub fn native_llm_stream_streaming(
    config: NativeLlmStreamConfig,
    on_event: JsFunction,
) -> napi::Result<()> {
    use napi::threadsafe_function::ThreadsafeFunction;

    let tsfn: ThreadsafeFunction<NativeLlmStreamEvent> = on_event
        .create_threadsafe_function(
            0,
            |ctx: napi::threadsafe_function::ThreadSafeCallContext<NativeLlmStreamEvent>| {
                ctx.env.create_object().and_then(|mut obj| {
                    obj.set("kind", ctx.value.kind)?;
                    if let Some(part) = ctx.value.part {
                        obj.set("part", part)?;
                    }
                    if let Some(metadata) = ctx.value.metadata {
                        obj.set("metadata", metadata)?;
                    }
                    if let Some(error) = ctx.value.error {
                        obj.set("error", error)?;
                    }
                    Ok(vec![obj])
                })
            },
        )
        .map_err(|e| napi::Error::from_reason(format!("create_threadsafe_function failed: {e}")))?;

    let stream_config = llm_stream::LlmStreamConfig {
        provider: config.provider,
        url: config.url,
        api_key: config.api_key,
        request_body: config.request_body,
        timeout_ms: config.timeout_ms.unwrap_or(120_000) as u64,
        extra_headers: config
            .extra_headers
            .unwrap_or_default()
            .into_iter()
            .map(|h| (h.key, h.value))
            .collect(),
    };

    match llm_stream_runtime() {
        Ok(rt) => {
            let worker_tsfn = tsfn.clone();
            let handle = rt.spawn(async move {
                let result = llm_stream::run_llm_stream_with(&stream_config, |event| {
                    let js_event = match event {
                        llm_stream::StreamEvent::Part(p) => NativeLlmStreamEvent {
                            kind: "part".to_string(),
                            part: Some(NativeLlmStreamPart {
                                part_type: p.part_type,
                                text: p.text,
                                think: p.think,
                                encrypted: p.encrypted,
                                id: p.id,
                                name: p.name,
                                arguments: p.arguments,
                                arguments_part: p.arguments_part,
                                stream_index: p.stream_index,
                            }),
                            metadata: None,
                            error: None,
                        },
                        llm_stream::StreamEvent::Done(m) => NativeLlmStreamEvent {
                            kind: "done".to_string(),
                            part: None,
                            metadata: Some(NativeLlmStreamMetadata {
                                response_id: m.response_id,
                                finish_reason: m.finish_reason,
                                input_tokens: m.input_tokens,
                                output_tokens: m.output_tokens,
                                cached_tokens: m.cached_tokens,
                                trace_id: m.trace_id,
                            }),
                            error: None,
                        },
                        llm_stream::StreamEvent::Error(e) => NativeLlmStreamEvent {
                            kind: "error".to_string(),
                            part: None,
                            metadata: None,
                            error: Some(e),
                        },
                    };

                    worker_tsfn.call(
                        Ok(js_event),
                        napi::threadsafe_function::ThreadsafeFunctionCallMode::NonBlocking,
                    );
                })
                .await;
                if let Err(e) = result {
                    worker_tsfn.call(
                        Ok(NativeLlmStreamEvent {
                            kind: "error".to_string(),
                            part: None,
                            metadata: None,
                            error: Some(e),
                        }),
                        napi::threadsafe_function::ThreadsafeFunctionCallMode::NonBlocking,
                    );
                }
            });

            let reaper = tsfn.clone();
            rt.spawn(async move {
                let Err(join_error) = handle.await else {
                    return;
                };
                let detail = if join_error.is_panic() {
                    let panic = join_error.into_panic();
                    panic
                        .downcast_ref::<&str>()
                        .map(|message| (*message).to_string())
                        .or_else(|| panic.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "panic payload was not a string".to_string())
                } else {
                    format!("stream task aborted: {join_error}")
                };
                reaper.call(
                    Ok(NativeLlmStreamEvent {
                        kind: "error".to_string(),
                        part: None,
                        metadata: None,
                        error: Some(format!(
                            "LLM stream task failed before it could report: {detail}"
                        )),
                    }),
                    napi::threadsafe_function::ThreadsafeFunctionCallMode::NonBlocking,
                );
            });
        }
        Err(e) => {
            tsfn.call(
                Ok(NativeLlmStreamEvent {
                    kind: "error".to_string(),
                    part: None,
                    metadata: None,
                    error: Some(e),
                }),
                napi::threadsafe_function::ThreadsafeFunctionCallMode::NonBlocking,
            );
        }
    }
    Ok(())
}

static LLM_STREAM_RUNTIME: once_cell::sync::OnceCell<tokio::runtime::Runtime> =
    once_cell::sync::OnceCell::new();

fn llm_stream_runtime() -> Result<&'static tokio::runtime::Runtime, String> {
    LLM_STREAM_RUNTIME.get_or_try_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(2)
            .thread_name("kimi-llm-stream")
            .build()
            .map_err(|e| format!("tokio runtime init failed: {e}"))
    })
}

use crate::native::llm_stream;
use crate::native::web_search;

#[napi(object)]
pub struct NativeWebSearchEntry {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub site_name: Option<String>,
}

#[napi(object)]
pub struct NativeWebSearchResult {
    pub results: Vec<NativeWebSearchEntry>,
    pub error: Option<String>,
}

#[napi]
pub async fn native_web_search(
    query: String,
    timeout_ms: Option<u32>,
    max_results: Option<u32>,
) -> NativeWebSearchResult {
    let config = web_search::WebSearchConfig {
        query,
        timeout_ms: timeout_ms
            .map(|v| v as u64)
            .unwrap_or(web_search::WebSearchConfig::default().timeout_ms),
        max_results: max_results
            .map(|v| v as usize)
            .unwrap_or(web_search::WebSearchConfig::default().max_results),
    };

    let result = tokio::task::spawn_blocking(move || web_search::web_search(&config))
        .await
        .unwrap_or_else(|e| web_search::WebSearchResult {
            results: Vec::new(),
            error: Some(format!("web_search panicked: {e}")),
        });

    NativeWebSearchResult {
        results: result
            .results
            .into_iter()
            .map(|r| NativeWebSearchEntry {
                title: r.title,
                url: r.url,
                snippet: r.snippet,
                site_name: r.site_name,
            })
            .collect(),
        error: result.error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_checkpoint_depth_export_tracks_the_sessions_stack() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().to_string_lossy().into_owned();
        let depth = || native_engine_state_checkpoint_depth(workspace.clone(), "session-a".into());
        assert_eq!(depth(), 0);

        let store = crate::storage::StateStore::for_session(dir.path(), "session-a").unwrap();
        store
            .write_domain("todo", &serde_json::json!([{ "id": "T1", "title": "one" }]))
            .unwrap();
        store.checkpoint().unwrap();
        assert_eq!(depth(), 1);

        let other = crate::storage::StateStore::for_session(dir.path(), "session-b").unwrap();
        other.checkpoint().unwrap();
        assert_eq!(depth(), 1);

        assert_eq!(
            crate::storage::state_store::rollback_scoped_state(dir.path(), "session-a", 1),
            1
        );
        assert_eq!(depth(), 0);
    }
}
