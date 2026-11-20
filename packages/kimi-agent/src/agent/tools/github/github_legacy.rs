use std::sync::Arc;

use serde_json::Value;

use super::github_tools::{
    GITHUB_MUTATING_TOOL_NAMES, GITHUB_SPECS, GitHubToolBase, GitHubToolDeps,
};
use crate::_base::utils::abort::AbortController;
use crate::agent::tool_executor::tool_executor::{
    ExecutableToolContext, ExecutableToolOutput, ExecutableToolResult, ToolExecution,
};
use crate::config::{GitHubConfig, KimiConfig};
use crate::turn_loop::types::{ExecutableToolResult as LegacyExecutableToolResult, ToolInfo};

#[cfg(test)]
pub(crate) static GITHUB_ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Clone, Debug, Default)]
pub struct GitHubCredentials {
    pub token: Option<String>,
    pub base_url: Option<String>,
}

pub fn github_tool_defs() -> Vec<ToolInfo> {
    GITHUB_SPECS
        .iter()
        .map(|spec| ToolInfo {
            name: spec.name.clone(),
            description: spec.description.clone(),
            input_schema: spec.schema.clone(),
        })
        .collect()
}

pub fn is_github_tool(tool_name: &str) -> bool {
    GITHUB_SPECS
        .iter()
        .any(|spec| spec.name.eq_ignore_ascii_case(tool_name))
}

pub fn is_mutating_tool(tool_name: &str) -> bool {
    GITHUB_MUTATING_TOOL_NAMES
        .iter()
        .any(|name| name.eq_ignore_ascii_case(tool_name))
}

pub fn is_readonly_tool(tool_name: &str) -> bool {
    is_github_tool(tool_name) && !is_mutating_tool(tool_name)
}

pub fn rule_subject(tool_name: &str, args: &Value) -> Option<String> {
    let spec = GITHUB_SPECS
        .iter()
        .find(|spec| spec.name.eq_ignore_ascii_case(tool_name))?;
    if spec.name.eq_ignore_ascii_case("GitHubGetMe") {
        return Some("me".to_string());
    }
    let has_query = spec
        .schema
        .get("properties")
        .and_then(Value::as_object)
        .is_some_and(|properties| properties.contains_key("q"));
    if has_query {
        return args
            .get("q")
            .and_then(Value::as_str)
            .map(str::to_string);
    }
    match (
        args.get("owner").and_then(Value::as_str),
        args.get("repo").and_then(Value::as_str),
    ) {
        (Some(owner), Some(repo)) => Some(format!("{owner}/{repo}")),
        _ => None,
    }
}

pub fn has_token(credentials: Option<&GitHubCredentials>) -> bool {
    config_with_credentials(credentials).github.token.is_some()
}

pub async fn execute_github_tool(
    tool_name: &str,
    args: &Value,
    credentials: Option<&GitHubCredentials>,
    cancel: Option<&crate::subagent::types::ParentCancel>,
) -> Option<LegacyExecutableToolResult> {
    let spec = GITHUB_SPECS
        .iter()
        .find(|spec| spec.name.eq_ignore_ascii_case(tool_name))?
        .clone();
    let tool = GitHubToolBase::new(
        spec,
        GitHubToolDeps {
            config: Arc::new(config_with_credentials(credentials)),
            client: None,
        },
    );
    match tool.resolve_execution(args.clone()) {
        ToolExecution::Error(result) => Some(legacy_result(result)),
        ToolExecution::Runnable(run) => {
            // v2 passes ctx.signal so a user abort cancels the in-flight
            // request; bridge the turn's ParentCancel into one AbortSignal.
            let controller = AbortController::new();
            let bridge = cancel.map(|cancel| {
                let listener_controller = controller.clone();
                let listener_cancel = cancel.clone();
                let bridge = tokio::spawn(async move {
                    listener_cancel.wait().await;
                    listener_controller.abort(Some(std::sync::Arc::new(
                        crate::_base::utils::abort::user_cancellation_reason(),
                    ) as crate::_base::errors::errors::SharedError));
                });
                if cancel.triggered() {
                    controller.abort(Some(std::sync::Arc::new(
                        crate::_base::utils::abort::user_cancellation_reason(),
                    ) as crate::_base::errors::errors::SharedError));
                }
                CancelBridge(bridge)
            });
            let _bridge = bridge;
            let context = ExecutableToolContext {
                turn_id: 0,
                tool_call_id: String::new(),
                trace: None,
                metadata: None,
                signal: controller.signal(),
                steer_signal: None,
                on_update: None,
                on_foreground_task_start: None,
            };
            match (run.execute)(context).await {
                Ok(result) => Some(legacy_result(result)),
                Err(error) => Some(crate::tools::err_result(error.to_string())),
            }
        }
    }
}

struct CancelBridge(tokio::task::JoinHandle<()>);

impl Drop for CancelBridge {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn config_with_credentials(credentials: Option<&GitHubCredentials>) -> KimiConfig {
    let token = credentials
        .and_then(|credentials| credentials.token.as_deref())
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .or_else(|| non_empty_env("GITHUB_TOKEN"))
        .or_else(|| non_empty_env("GH_TOKEN"));
    let base_url = credentials
        .and_then(|credentials| credentials.base_url.as_deref())
        .map(str::trim)
        .filter(|base_url| !base_url.is_empty())
        .map(str::to_string)
        .or_else(|| non_empty_env("GITHUB_API_URL"));
    let mut config = KimiConfig::default();
    config.github = GitHubConfig { token, base_url };
    config
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct LoopbackProxyBypass {
        _lock: std::sync::MutexGuard<'static, ()>,
        upper: Option<std::ffi::OsString>,
        lower: Option<std::ffi::OsString>,
    }

    impl LoopbackProxyBypass {
        fn new() -> Self {
            let lock = GITHUB_ENV_TEST_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let upper = std::env::var_os("NO_PROXY");
            let lower = std::env::var_os("no_proxy");
            unsafe {
                std::env::set_var("NO_PROXY", "*");
                std::env::set_var("no_proxy", "*");
            }
            Self { _lock: lock, upper, lower }
        }
    }

    impl Drop for LoopbackProxyBypass {
        fn drop(&mut self) {
            match self.upper.take() {
                Some(value) => unsafe { std::env::set_var("NO_PROXY", value) },
                None => unsafe { std::env::remove_var("NO_PROXY") },
            }
            match self.lower.take() {
                Some(value) => unsafe { std::env::set_var("no_proxy", value) },
                None => unsafe { std::env::remove_var("no_proxy") },
            }
        }
    }

    #[tokio::test]
    async fn legacy_dispatch_preserves_request_order_and_pagination_state() {
        let _proxy = LoopbackProxyBypass::new();
        for has_next in [true, false] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut chunk = [0; 1024];
                    let read = socket.read(&mut chunk).await.unwrap();
                    assert!(read > 0, "request headers should arrive");
                    request.extend_from_slice(&chunk[..read]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8_lossy(&request);
                assert!(request.starts_with("GET /repos/example/project/issues?state=open&labels=bug%2Cdocs&per_page=10&page=2 HTTP/1.1"), "{request}");
                assert!(request.lines().any(|line| line.eq_ignore_ascii_case("authorization: Bearer YOUR_API_KEY")));
                let body = r#"[{"number":7,"title":" issue ","body":"hidden"}]"#;
                let pagination = if has_next {
                    "Link: <http://example.test/next>; rel=\"next\"\r\n"
                } else {
                    ""
                };
                socket.write_all(format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n{pagination}Connection: close\r\n\r\n{body}",
                    body.len()
                ).as_bytes()).await.unwrap();
            });
            let credentials = GitHubCredentials {
                token: Some("YOUR_API_KEY".into()),
                base_url: Some(base_url),
            };
            let args = serde_json::json!({
                "owner":"example", "repo":"project", "state":"open",
                "labels":"bug,docs", "perPage":10, "page":2
            });
            let result = execute_github_tool(
                "GitHubListIssues", &args, Some(&credentials), None,
            )
            .await
            .expect("the adapter must recognize the contracted tool");
            server.await.unwrap();
            assert!(!result.is_error);
            let header = if has_next {
                "{\"items\":1,\"has_more\":true}"
            } else {
                "{\"items\":1}"
            };
            assert_eq!(result.content, format!("{header}\n{{\"number\":7,\"title\":\"issue\"}}"));
        }
    }

    #[tokio::test]
    async fn legacy_diff_dispatch_limits_the_response_body() {
        let _proxy = LoopbackProxyBypass::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut chunk = [0; 512];
                let read = socket.read(&mut chunk).await.unwrap();
                assert!(read > 0);
                request.extend_from_slice(&chunk[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8_lossy(&request);
            assert!(request.lines().any(|line| line.eq_ignore_ascii_case("accept: application/vnd.github.diff")));
            let body = "x".repeat(crate::agent::tools::github::github_request::GITHUB_DIFF_MAX_RESPONSE_BODY_BYTES + 7);
            socket.write_all(format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ).as_bytes()).await.unwrap();
        });
        let credentials = GitHubCredentials {
            token: Some("YOUR_API_KEY".into()),
            base_url: Some(base_url),
        };
        let args = serde_json::json!({"owner":"example", "repo":"project", "pullNumber":3});
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            execute_github_tool("GitHubGetPRDiff", &args, Some(&credentials), None),
        ).await.unwrap();
        server.await.unwrap();
        let result = result.unwrap();
        let body_limit = crate::agent::tools::github::github_request::GITHUB_DIFF_MAX_RESPONSE_BODY_BYTES;
        assert_eq!(result.content.len(), body_limit + "\n[...truncated 7+ bytes]".len());
        assert!(result.content.starts_with(&"x".repeat(body_limit)));
        assert!(result.content.ends_with("[...truncated 7+ bytes]"));
    }

    #[tokio::test]
    async fn parent_cancellation_closes_the_legacy_github_request() {
        use crate::subagent::types::ParentCancel;

        let _proxy = LoopbackProxyBypass::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let (request_started, headers_sent) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut chunk = [0; 512];
                let read = socket.read(&mut chunk).await.unwrap();
                assert!(read > 0);
                request.extend_from_slice(&chunk[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nabc").await.unwrap();
            request_started.send(()).unwrap();
            let mut byte = [0];
            matches!(
                tokio::time::timeout(std::time::Duration::from_secs(3), socket.read(&mut byte)).await,
                Ok(Ok(0))
            )
        });
        let cancel = ParentCancel::new();
        let credentials = GitHubCredentials {
            token: Some("YOUR_API_KEY".into()),
            base_url: Some(base_url),
        };
        let args = serde_json::json!({"owner":"example", "repo":"project"});
        let call = {
            let cancel = cancel.clone();
            tokio::spawn(async move {
                execute_github_tool(
                    "GitHubGetRepo", &args, Some(&credentials), Some(&cancel),
                ).await
            })
        };
        headers_sent.await.unwrap();
        cancel.trigger();
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), call)
            .await.unwrap().unwrap().unwrap();
        assert!(result.is_error);
        assert!(server.await.unwrap(), "cancelling the tool must close the HTTP request");
    }

    #[test]
    fn every_github_spec_has_one_readonly_or_mutating_classification() {
        for spec in GITHUB_SPECS.iter() {
            assert_ne!(is_readonly_tool(&spec.name), is_mutating_tool(&spec.name), "{}", spec.name);
            assert_eq!(is_mutating_tool(&spec.name), spec.mutating == Some(true), "{}", spec.name);
        }
    }

    #[test]
    fn new_readonly_tools_are_in_the_adapter_definition_table() {
        let definitions = github_tool_defs();
        for name in [
            "GitHubGetRepoTree",
            "GitHubListPRCommits",
            "GitHubGetCheckRuns",
            "GitHubListWorkflowRunJobs",
        ] {
            assert!(definitions.iter().any(|tool| tool.name == name), "{name}");
            assert!(is_readonly_tool(name), "{name}");
        }
    }
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn legacy_result(result: ExecutableToolResult) -> LegacyExecutableToolResult {
    let content = match result.output {
        ExecutableToolOutput::Text(text) => text,
        ExecutableToolOutput::Parts(parts) => serde_json::to_string(&parts).unwrap_or_default(),
    };
    LegacyExecutableToolResult {
        stop_turn: result.stop_turn.unwrap_or(false),
        content,
        is_error: result.is_error.unwrap_or(false),
        note: result.note,
        delivery: result.delivery,
        parts: Vec::new(),
        display: None,
        spill: result.spill,
    }
}
