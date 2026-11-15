use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;

use crate::app::workspace::workspace::IWorkspaceService;
use crate::wire::record::WireAsyncError;

use super::host_folder_browser::{
    FsBrowseEntry, FsBrowseResponse, FsHomeResponse, HostFolderNotAbsoluteError,
    HostFolderNotFoundError, HostFolderPermissionError, IHostFolderBrowser, RECENT_ROOTS_LIMIT,
};

pub struct HostFolderBrowser {
    registry: Arc<dyn IWorkspaceService>,
}

impl HostFolderBrowser {
    pub fn new(registry: Arc<dyn IWorkspaceService>) -> Self {
        Self { registry }
    }

    pub async fn browse(&self, abs_path: Option<&str>) -> Result<FsBrowseResponse, WireAsyncError> {
        let target = abs_path
            .map(PathBuf::from)
            .unwrap_or_else(home_dir);
        if !target.is_absolute() {
            return Err(Box::new(HostFolderNotAbsoluteError::new(
                target.to_string_lossy().into_owned(),
            )));
        }
        let real_target = tokio::fs::canonicalize(&target)
            .await
            .map(PathBuf::from)
            .map(normalize_real_path)
            .map_err(|error| map_fs_error(error, &target.to_string_lossy()))?;
        let real_target = ensure_drive_root(real_target);
        let mut reader = tokio::fs::read_dir(&real_target)
            .await
            .map_err(|error| map_fs_error(error, &real_target.to_string_lossy()))?;
        let mut entries = Vec::new();
        loop {
            let entry = reader
                .next_entry()
                .await
                .map_err(|error| map_fs_error(error, &real_target.to_string_lossy()))?;
            let Some(entry) = entry else {
                break;
            };
            let file_type = entry
                .file_type()
                .await
                .map_err(|error| map_fs_error(error, &real_target.to_string_lossy()))?;
            if !file_type.is_dir() {
                continue;
            }
            entries.push(FsBrowseEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                path: entry.path().to_string_lossy().into_owned(),
                is_dir: true,
            });
        }
        entries.sort_by(compare_browse_entries);
        let parent = real_target.parent().and_then(|parent| {
            if parent == real_target {
                None
            } else {
                Some(parent.to_string_lossy().into_owned())
            }
        });
        Ok(FsBrowseResponse {
            path: real_target.to_string_lossy().into_owned(),
            parent,
            entries,
        })
    }

    pub async fn home(&self) -> Result<FsHomeResponse, WireAsyncError> {
        let home = home_dir().to_string_lossy().into_owned();
        let workspaces = self.registry.list().await?;
        let recent_roots = workspaces
            .into_iter()
            .take(RECENT_ROOTS_LIMIT)
            .map(|workspace| workspace.root)
            .collect();
        Ok(FsHomeResponse { home, recent_roots })
    }
}

#[async_trait]
impl IHostFolderBrowser for HostFolderBrowser {
    async fn browse(&self, abs_path: Option<&str>) -> Result<FsBrowseResponse, WireAsyncError> {
        HostFolderBrowser::browse(self, abs_path).await
    }

    async fn home(&self) -> Result<FsHomeResponse, WireAsyncError> {
        HostFolderBrowser::home(self).await
    }
}

fn map_fs_error(error: std::io::Error, path: &str) -> WireAsyncError {
    match error.kind() {
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory => {
            Box::new(HostFolderNotFoundError::new(path))
        }
        std::io::ErrorKind::PermissionDenied => Box::new(HostFolderPermissionError::new(path)),
        _ => Box::new(error),
    }
}

fn compare_browse_entries(a: &FsBrowseEntry, b: &FsBrowseEntry) -> Ordering {
    let a_dot = a.name.starts_with('.');
    let b_dot = b.name.starts_with('.');
    if a_dot != b_dot {
        return if a_dot { Ordering::Greater } else { Ordering::Less };
    }
    a.name
        .to_lowercase()
        .cmp(&b.name.to_lowercase())
        .then_with(|| a.name.cmp(&b.name))
}

fn home_dir() -> PathBuf {
    std::env::home_dir().unwrap_or_default()
}

#[cfg(windows)]
fn normalize_real_path(path: PathBuf) -> PathBuf {
    let rendered = path.to_string_lossy().into_owned();
    if let Some(unc) = rendered.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{unc}"))
    } else if let Some(local) = rendered.strip_prefix(r"\\?\") {
        PathBuf::from(local)
    } else {
        path
    }
}

#[cfg(not(windows))]
fn normalize_real_path(path: PathBuf) -> PathBuf {
    path
}

fn ensure_drive_root(path: PathBuf) -> PathBuf {
    let rendered = path.to_string_lossy();
    let is_drive_root = rendered.len() == 2
        && rendered.as_bytes()[0].is_ascii_alphabetic()
        && rendered.as_bytes()[1] == b':';
    if is_drive_root {
        PathBuf::from(format!("{rendered}{}", std::path::MAIN_SEPARATOR))
    } else {
        path
    }
}
