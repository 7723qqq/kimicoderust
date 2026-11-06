use super::super::*;
use std::collections::HashMap;

pub(super) fn engine_without_a_model(
    store: Arc<SqliteSessionStore>,
    hub: Arc<EventHub>,
) -> ServerEngine {
    ServerEngine::new(
        crate::pipeline::PipelineSpec {
            system_prompt: "sys".into(),
            model_name: "test-model".into(),
            providers: Vec::new(),
            native_llm: None,
            workspace_root: None,
            native_tools: false,
            extra_roots: Vec::new(),
            rust_self_contained: false,
            shell_path: None,
            policy_snapshot: None,
            github_token: None,
            github_base_url: None,
            subagent_timeout_ms: None,
            agent_tool_veto: None,
            tools_veto: None,
            todo_tool_veto: None,
            tower_worktree_root: None,
            tower_enabled: false,
            notify_user_enabled: true,
            tool_select: false,
            sandbox_mode: None,
            sandbox_policy: None,
            caller_agent_id: None,
            session_id: None,
            secondary_model: None,
            image_read_byte_budget: None,
            image_max_edge_px: None,
            model_capabilities: None,
            skill_dirs: Vec::new(),
            plugin_skill_dirs: Vec::new(),
            merge_all_available_skills: true,
            background: crate::storage::BackgroundLimits::default(),
        },
        hub,
        store,
    )
}

pub(super) async fn prompt(server: &HttpServer, session_id: &str) -> HttpResponse {
    server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/{session_id}/prompt"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "prompt": "Hello REST" })).unwrap(),
        })
        .await
}

pub(super) fn catalog_request(method: &str, path: &str, body: Option<&Value>) -> HttpRequest {
    HttpRequest {
        method: method.into(),
        path: path.into(),
        query: None,
        headers: HashMap::new(),
        body: body
            .map(|value| serde_json::to_vec(value).unwrap())
            .unwrap_or_default(),
    }
}

/// A one-connection-at-a-time registry server: the response (status line
/// and JSON body) can be swapped between requests, and every request head
/// is captured so the auth header can be asserted.
pub(super) struct RegistryServer {
    pub(super) url: String,
    response: Arc<std::sync::Mutex<(String, String)>>,
    requests: Arc<std::sync::Mutex<Vec<String>>>,
}

impl RegistryServer {
    pub(super) async fn spawn(status: &str, body: &str) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let response = Arc::new(std::sync::Mutex::new((
            status.to_string(),
            body.to_string(),
        )));
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let shared_response = response.clone();
        let shared_requests = requests.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    continue;
                }
                shared_requests
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf[..n]).to_string());
                let (status, body) = shared_response.lock().unwrap().clone();
                let head = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        Self {
            url: format!("http://{addr}/api.json"),
            response,
            requests,
        }
    }

    pub(super) fn set_response(&self, status: &str, body: &str) {
        *self.response.lock().unwrap() = (status.to_string(), body.to_string());
    }

    pub(super) fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

/// A host whose `llm_chat` answers with a fixed summary, so the `:compact`
/// route's summarizer call can be observed end to end.
pub(super) struct SummarizingHost {
    pub(super) summary: String,
}

impl crate::callbacks::HostCallbacks for SummarizingHost {
    fn llm_chat(
        &self,
        _: crate::rpc::types::LlmChatRequest,
    ) -> crate::rpc::types::BoxFuture<'static, Result<crate::rpc::types::LlmChatResponse, String>>
    {
        let summary = self.summary.clone();
        Box::pin(async move {
            Ok(crate::rpc::types::LlmChatResponse {
                content: summary,
                tool_calls: vec![],
                thinking: vec![],
                finish_reason: Some("stop".to_string()),
                usage: crate::rpc::types::TokenUsage::default(),
            })
        })
    }

    fn execute_tool(
        &self,
        _: crate::rpc::types::ToolExecuteRequest,
    ) -> crate::rpc::types::BoxFuture<'static, Result<crate::rpc::types::ToolExecuteResponse, String>>
    {
        Box::pin(async { Err("not used".into()) })
    }

    fn check_permission(
        &self,
        _: crate::rpc::types::PermissionCheckRequest,
    ) -> crate::rpc::types::BoxFuture<'static, Result<crate::rpc::types::PermissionDecision, String>>
    {
        Box::pin(async { Ok(crate::rpc::types::PermissionDecision::allow()) })
    }
}
