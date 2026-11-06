use super::super::*;

/// The next session update that is not one of the two attach-time
/// notifications (`available_commands_update` / `session_info_update`),
/// which every new/load/resume now pushes.
pub(super) fn next_non_attach_update(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<AcpOutbound>,
) -> serde_json::Value {
    loop {
        match rx.try_recv().expect("a session update") {
            AcpOutbound::Notification(note) => {
                let params = note.params.unwrap_or_default();
                let kind = params["update"]["sessionUpdate"]
                    .as_str()
                    .unwrap_or_default();
                if kind == "available_commands_update" || kind == "session_info_update" {
                    continue;
                }
                return params;
            }
            other => panic!("unexpected outbound message: {other:?}"),
        }
    }
}

/// Drop attach-time notifications from a drained batch.
pub(super) fn drop_attach_updates(mut updates: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    updates.retain(|update| {
        let kind = update["update"]["sessionUpdate"]
            .as_str()
            .unwrap_or_default();
        kind != "available_commands_update" && kind != "session_info_update"
    });
    updates
}

/// A local OpenAI-compatible SSE endpoint that streams one delta and then
/// holds the connection open: only a cancellation can end the turn.
pub(super) async fn spawn_hanging_openai_server() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 8192];
        let _ = sock.read(&mut buf).await;
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"hel\"}}]}\n\n";
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{body}"
        );
        let _ = sock.write_all(head.as_bytes()).await;
        let _ = sock.flush().await;
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    });
    (format!("http://{addr}/v1"), server)
}

/// A local OpenAI-compatible SSE endpoint that answers a complete turn.
pub(super) async fn spawn_completing_openai_server() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 8192];
        let _ = sock.read(&mut buf).await;
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
                        data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                        data: [DONE]\n\n";
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{body}"
        );
        let _ = sock.write_all(head.as_bytes()).await;
        let _ = sock.flush().await;
    });
    (format!("http://{addr}/v1"), server)
}

/// A local OpenAI-compatible SSE endpoint that answers `count` complete
/// turns, recording each request body's prompt text.
pub(super) async fn spawn_repeating_openai_server(
    count: usize,
) -> (
    String,
    Arc<std::sync::Mutex<Vec<String>>>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let server = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for _ in 0..count {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut buf = vec![0u8; 65536];
            let read = sock.read(&mut buf).await.unwrap_or(0);
            recorded
                .lock()
                .unwrap()
                .push(String::from_utf8_lossy(&buf[..read]).into_owned());
            let body = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
                            data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                            data: [DONE]\n\n";
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{body}"
            );
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.flush().await;
        }
    });
    (format!("http://{addr}/v1"), requests, server)
}

/// An engine whose one model is the local mock endpoint.
pub(super) fn engine_with_native_llm(
    store: &Arc<SqliteSessionStore>,
    base_url: &str,
) -> Arc<crate::server::engine::ServerEngine> {
    engine_with_native_llm_and_skills(store, base_url, Vec::new())
}

/// [`engine_with_native_llm`] with extra skill roots (the engine's
/// `extra_skill_dirs`, which the session catalog and the `Skill` tool scan).
pub(super) fn engine_with_native_llm_and_skills(
    store: &Arc<SqliteSessionStore>,
    base_url: &str,
    skill_dirs: Vec<std::path::PathBuf>,
) -> Arc<crate::server::engine::ServerEngine> {
    use crate::pipeline::PipelineSpec;
    use crate::server::engine::ServerEngine;

    Arc::new(ServerEngine::new(
        PipelineSpec {
            system_prompt: "sys".into(),
            model_name: "mock-model".into(),
            providers: Vec::new(),
            native_llm: Some(crate::rpc::types::NativeLlmConfig {
                protocol: "openai".into(),
                base_url: base_url.to_string(),
                api_key: "test-key".into(),
                model: "mock-model".into(),
                ..Default::default()
            }),
            workspace_root: None,
            native_tools: false,
            extra_roots: Vec::new(),
            rust_self_contained: true,
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
            skill_dirs,
            plugin_skill_dirs: Vec::new(),
            merge_all_available_skills: true,
            background: crate::storage::BackgroundLimits::default(),
        },
        Arc::new(crate::server::hub::EventHub::new()),
        store.clone(),
    ))
}
