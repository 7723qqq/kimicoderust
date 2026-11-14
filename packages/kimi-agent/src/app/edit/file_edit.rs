use std::sync::Arc;

use async_trait::async_trait;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};

use crate::_base::di::descriptors::DynamicValue;
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
use crate::os::interface::host_file_system::IHostFileSystem;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEditInput {
    pub path: String,
    #[serde(rename = "displayPath")]
    pub display_path: String,
    pub old_string: String,
    pub new_string: String,
    pub replace_all: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "ok")]
pub enum FileEditResult {
    #[serde(rename = "true")]
    Ok { count: usize },
    #[serde(rename = "false")]
    Err { error: String },
}

#[async_trait]
pub trait IFileEditService: Send + Sync {
    async fn edit(
        &self,
        input: FileEditInput,
        fs: Option<Arc<dyn IHostFileSystem>>,
    ) -> FileEditResult;
}

pub static I_FILE_EDIT_SERVICE: Lazy<ServiceIdentifier<DynamicValue>> =
    Lazy::new(|| create_decorator("fileEditService"));
