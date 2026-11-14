use crate::_base::exec_env::environment_probe::probe_host_environment_from_node;
use crate::_base::exec_env::login_shell_path::apply_login_shell_path_from_node;
use crate::remote::protocol::methods::RemoteEnvironmentInfo;
use crate::wire::record::WireAsyncError;

pub async fn probe_executor_environment() -> Result<RemoteEnvironmentInfo, WireAsyncError> {
    let (info, shell) = tokio::join!(probe_host_environment_from_node(), apply_login_shell_path_from_node());
    let mut info = info?;
    let _ = shell?;
    info.cwd = std::env::current_dir()?.to_string_lossy().into_owned();
    info.temp_dir = std::env::temp_dir().to_string_lossy().into_owned();
    info.windows_process_lifecycle = cfg!(windows).then_some(true);
    Ok(info)
}
