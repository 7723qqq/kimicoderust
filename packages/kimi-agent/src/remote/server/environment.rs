use crate::_base::exec_env::environment_probe::probe_host_environment_from_node;
use crate::_base::exec_env::login_shell_path::apply_login_shell_path_from_node;
use crate::remote::protocol::methods::RemoteEnvironmentInfo;
use crate::wire::record::WireAsyncError;

pub async fn probe_executor_environment() -> Result<RemoteEnvironmentInfo, WireAsyncError> {
    let (info, shell) = tokio::join!(probe_host_environment_from_node(), apply_login_shell_path_from_node());
    let info = info?;
    let _ = shell;
    Ok(RemoteEnvironmentInfo {
        os_kind: info.os_kind,
        os_arch: info.os_arch,
        os_version: info.os_version,
        shell_name: match info.shell_name {
            crate::_base::exec_env::environment_probe::ShellName::Bash => "bash".to_string(),
            crate::_base::exec_env::environment_probe::ShellName::Sh => "sh".to_string(),
        },
        shell_path: info.shell_path,
        path_class: match info.path_class {
            crate::_base::exec_env::environment_probe::PathClass::Posix => "posix".to_string(),
            crate::_base::exec_env::environment_probe::PathClass::Win32 => "win32".to_string(),
        },
        home_dir: info.home_dir,
        cwd: std::env::current_dir()?.to_string_lossy().into_owned(),
        temp_dir: std::env::temp_dir().to_string_lossy().into_owned(),
        windows_process_lifecycle: cfg!(windows).then_some(true),
    })
}
