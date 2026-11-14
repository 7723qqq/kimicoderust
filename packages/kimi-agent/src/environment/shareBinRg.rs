use std::path::PathBuf;
use std::sync::Arc;

use crate::environment::environment::{Environment, LOCAL_ENVIRONMENT_ID};

pub fn is_remote_environment(environment: Option<&Arc<dyn Environment>>) -> bool {
    environment.is_some_and(|environment| {
        environment.identity().environment_id != LOCAL_ENVIRONMENT_ID
    })
}

pub fn get_share_bin_rg_path() -> String {
    let binary = rg_binary_name();
    if let Some(override_dir) = std::env::var_os("KIMI_CODE_HOME")
        && !override_dir.is_empty()
    {
        return PathBuf::from(override_dir)
            .join("bin")
            .join(binary)
            .to_string_lossy()
            .into_owned();
    }
    home_dir()
        .join(".kimi-code")
        .join("bin")
        .join(binary)
        .to_string_lossy()
        .into_owned()
}

pub fn share_bin_rg_path(environment: Option<&Arc<dyn Environment>>) -> String {
    if is_remote_environment(environment) {
        if let Some(home_dir) = environment
            .and_then(|environment| environment.host())
            .map(|host| host.home_dir)
        {
            let binary = environment
                .and_then(|environment| environment.host())
                .is_some_and(|host| host.path_class == "win32")
                .then_some("rg.exe")
                .unwrap_or("rg");
            if let Some(path) = environment.and_then(|environment| environment.path()) {
                return path.join(&home_dir, &[".kimi-code", "bin", binary]);
            }
            return format!("{home_dir}/.kimi-code/bin/{binary}");
        }
    }
    get_share_bin_rg_path()
}

fn rg_binary_name() -> &'static str {
    if cfg!(windows) { "rg.exe" } else { "rg" }
}

fn home_dir() -> PathBuf {
    if cfg!(windows) {
        std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .unwrap_or_default()
    } else {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
    }
}
