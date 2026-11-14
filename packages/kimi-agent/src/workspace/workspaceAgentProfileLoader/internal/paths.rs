use crate::_base::utils::paths::{is_absolute, join, resolve};
use crate::os::interface::host_file_system::IHostFileSystem;
use crate::os::interface::host_fs_errors::{
    HostFsError, OS_FS_NOT_DIRECTORY, OS_FS_NOT_FOUND,
};
use crate::wire::record::WireAsyncError;

pub fn resolve_agent_path(path: &str, base_dir: &str, os_home_dir: &str) -> String {
    if path == "~" {
        return os_home_dir.to_string();
    }
    if let Some(path) = path.strip_prefix("~/") {
        return join(os_home_dir, &[path]);
    }
    if is_absolute(path) {
        return path.to_string();
    }
    resolve(base_dir, path)
}

pub async fn is_directory_path(
    fs: &dyn IHostFileSystem,
    path: &str,
) -> Result<bool, WireAsyncError> {
    match directory_path(fs, path).await {
        Ok(value) => Ok(value),
        Err(error) if is_missing_path_error(&error) => Ok(false),
        Err(error) => Err(error),
    }
}

async fn directory_path(
    fs: &dyn IHostFileSystem,
    path: &str,
) -> Result<bool, WireAsyncError> {
    let resolved = fs.realpath(path).await?;
    Ok(fs.stat(&resolved).await?.is_directory)
}

pub async fn is_file_path(
    fs: &dyn IHostFileSystem,
    path: &str,
) -> Result<bool, WireAsyncError> {
    match file_path(fs, path).await {
        Ok(value) => Ok(value),
        Err(error) if is_missing_path_error(&error) => Ok(false),
        Err(error) => Err(error),
    }
}

async fn file_path(fs: &dyn IHostFileSystem, path: &str) -> Result<bool, WireAsyncError> {
    let resolved = fs.realpath(path).await?;
    Ok(fs.stat(&resolved).await?.is_file)
}

pub async fn path_exists(fs: &dyn IHostFileSystem, path: &str) -> Result<bool, WireAsyncError> {
    match fs.stat(path).await {
        Ok(_) => Ok(true),
        Err(error) if is_missing_path_error(&error) => Ok(false),
        Err(error) => Err(error),
    }
}

fn is_missing_path_error(error: &WireAsyncError) -> bool {
    error.downcast_ref::<HostFsError>().is_some_and(|error| {
        error.code().as_str() == OS_FS_NOT_FOUND || error.code().as_str() == OS_FS_NOT_DIRECTORY
    })
}
