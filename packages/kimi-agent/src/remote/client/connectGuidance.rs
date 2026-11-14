use futures_util::future::BoxFuture;
use futures_util::FutureExt;
use regex::Regex;

use crate::remote::protocol::methods::MIN_EXECUTOR_VERSION;
use crate::wire::record::WireAsyncError;

use super::connection::HandshakeError;
use super::launchers::{LauncherSpec, is_bare_remote_bin, launcher_remote_bin};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandshakeFailureClass {
    Missing,
    Timeout,
    Incompatible,
    Other,
}

fn launcher_label(launcher: &LauncherSpec) -> String {
    match launcher {
        LauncherSpec::Ssh { host, .. } => format!("ssh:{host}"),
        LauncherSpec::Docker { container, .. } => format!("docker:{container}"),
        LauncherSpec::Command { program, .. } => format!("command:{program}"),
    }
}

pub fn classify_handshake_failure(error: &(dyn std::error::Error + 'static)) -> HandshakeFailureClass {
    let Some(error) = error.downcast_ref::<HandshakeError>() else {
        return HandshakeFailureClass::Other;
    };
    if error.kind == Some(super::connection::HandshakeErrorKind::ExecutorExit) {
        let missing_status = matches!(error.exit_code, Some(127 | 126 | 9009));
        let windows_ssh_missing = error.exit_code == Some(49)
            && Regex::new(r"(?i)not recognized as an internal or external command|command not found")
                .unwrap()
                .is_match(&error.to_string());
        return if missing_status || windows_ssh_missing {
            HandshakeFailureClass::Missing
        } else {
            HandshakeFailureClass::Other
        };
    }
    match error.kind {
        Some(super::connection::HandshakeErrorKind::Timeout) => HandshakeFailureClass::Timeout,
        Some(super::connection::HandshakeErrorKind::Incompatible) => HandshakeFailureClass::Incompatible,
        _ => HandshakeFailureClass::Other,
    }
}

fn expected_path_suffix(launcher: &LauncherSpec) -> String {
    launcher_remote_bin(launcher)
        .map(|path| format!(" (expected at {path})"))
        .unwrap_or_default()
}

fn executor_action(launcher: &LauncherSpec) -> String {
    let remote_bin = launcher_remote_bin(launcher);
    if remote_bin.as_deref().is_some_and(is_bare_remote_bin) {
        let remote_bin = remote_bin.unwrap();
        return format!(
            "download the `kimi` binary for the target platform from the Kimi Code release CDN and install it as `{remote_bin}` on the target's PATH"
        );
    }
    let location = remote_bin.map_or_else(
        || "the absolute path your launcher command invokes".to_string(),
        |remote_bin| format!("the executor path ({remote_bin})"),
    );
    format!(
        "download the `kimi` binary for the target platform from the Kimi Code release CDN and install it at {location}"
    )
}

fn missing_executor_guidance(launcher: &LauncherSpec, failure: HandshakeFailureClass) -> String {
    let label = launcher_label(launcher);
    let suffix = expected_path_suffix(launcher);
    let intro = if failure == HandshakeFailureClass::Timeout {
        format!(
            "The remote executor on {label} did not answer the handshake in time — it may be missing or unable to start{suffix}."
        )
    } else {
        format!(
            "The remote executor (kimi exec-server) was not found on {label}{suffix}."
        )
    };
    format!(
        "{intro}\n\nInstall the executor manually: {}.\n\nThen reconnect the environment.",
        executor_action(launcher)
    )
}

fn upgrade_executor_guidance(
    launcher: &LauncherSpec,
    executor_version: Option<&str>,
    min_executor_version: Option<&str>,
) -> String {
    let found = executor_version.unwrap_or("unknown");
    let minimum = min_executor_version.unwrap_or(MIN_EXECUTOR_VERSION);
    format!(
        "The remote executor on {} reports version {found}, below the required minimum {minimum}.\n\nUpgrade the executor on the target: {}.\n\nThen reconnect the environment.",
        launcher_label(launcher),
        executor_action(launcher)
    )
}

fn with_guidance(error: WireAsyncError, guidance: String) -> HandshakeError {
    let base = error.to_string();
    let mut details = error
        .downcast_ref::<HandshakeError>()
        .map(|error| super::connection::HandshakeErrorDetails {
            kind: error.kind,
            exit_code: error.exit_code,
            executor_version: error.executor_version.clone(),
            min_executor_version: error.min_executor_version.clone(),
            cause: None,
        })
        .unwrap_or_default();
    details.cause = Some(error);
    HandshakeError::new(format!("{base}\n\n{guidance}"), details)
}

pub struct ConnectWithGuidanceOptions {
    pub launcher: LauncherSpec,
    pub min_executor_version: Option<String>,
}

pub fn connect_with_guidance<T: 'static>(
    attempt: impl FnOnce(LauncherSpec) -> BoxFuture<'static, Result<T, WireAsyncError>>,
    options: ConnectWithGuidanceOptions,
) -> BoxFuture<'static, Result<T, WireAsyncError>> {
    let launcher = options.launcher.clone();
    async move {
        match attempt(launcher.clone()).await {
            Ok(value) => Ok(value),
            Err(error) => {
                let failure = classify_handshake_failure(error.as_ref());
                match failure {
                    HandshakeFailureClass::Other => Err(error),
                    HandshakeFailureClass::Incompatible => {
                        let handshake = error.downcast_ref::<HandshakeError>();
                        let guidance = upgrade_executor_guidance(
                            &launcher,
                            handshake.and_then(|error| error.executor_version.as_deref()),
                            handshake
                                .and_then(|error| error.min_executor_version.as_deref())
                                .or(options.min_executor_version.as_deref()),
                        );
                        Err(Box::new(with_guidance(error, guidance)))
                    }
                    _ => Err(Box::new(with_guidance(
                        error,
                        missing_executor_guidance(&launcher, failure),
                    ))),
                }
            }
        }
    }
    .boxed()
}
