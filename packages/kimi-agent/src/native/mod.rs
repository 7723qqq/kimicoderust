#![cfg_attr(
    not(feature = "napi"),
    allow(dead_code, unused_imports, unused_variables)
)]

pub(crate) mod bash;
mod bash_spawn;
pub mod catalog;
mod compaction;
mod edit;
mod encoding;
pub mod environment_probe;
pub(crate) mod fetch_url;
mod file_cache;
pub mod file_type;
pub(crate) mod glob;
pub(crate) mod grep;
pub mod image_compress;
pub mod image_originals;
pub mod list_directory;
mod llm_stream;

#[cfg(feature = "napi")]
mod native_tool_bindings;
mod output_truncate;
pub(crate) use crate::tool::path_access;
mod permission;
pub(crate) mod read;
pub(crate) use crate::tool::realpath_access;
pub mod shell;
pub(crate) mod shell_path_bridge;
pub mod shell_payload;
mod tokens;
mod tool_access;
pub(crate) mod tool_naming;

pub mod web_search;
mod workspace_index;
mod write;

pub mod event_store;
pub mod lsp;
pub mod permission_engine;
pub mod powershell;

pub use list_directory::{ListDirectoryConfig, ListDirectoryResult, list_directory};

pub(crate) use llm_stream::{
    NOT_AN_SSE_ENDPOINT_PREFIX, empty_stream_error, extract_in_band_error, truncate_for_diagnosis,
};

pub fn native_list_directory(
    path: Option<String>,
    collapse_hidden_dirs: Option<bool>,
) -> ListDirectoryResult {
    list_directory::list_directory(&ListDirectoryConfig {
        path,
        collapse_hidden_dirs: collapse_hidden_dirs.unwrap_or(false),
    })
}

#[cfg(feature = "napi")]
pub use native_tool_bindings::*;
