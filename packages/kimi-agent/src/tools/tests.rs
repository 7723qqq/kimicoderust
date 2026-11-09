use super::search::*;
use super::*;
use serde_json::json;

#[test]
fn arg_error_binds_the_tool_and_the_field() {
    let out = arg_error_text("CronCreate", "cron", "engine.tools.argMustBeString");
    assert_eq!(
        out,
        "Invalid CronCreate arguments: `cron` must be a string."
    );

    let zh = crate::i18n::EngineI18n {
        active: crate::i18n::Locale::Zh,
    };
    let localized = crate::i18n::LocalizedText::with_params(
        "engine.tools.argMustBeString",
        crate::i18n::i18n_params!["tool" => "CronCreate", "field" => "cron"],
    )
    .render_with(&zh);
    assert!(localized.contains("CronCreate") && localized.contains("cron"));
    assert!(!localized.contains('{') && !localized.contains('}'));
}

#[test]
fn arg_error_surfaces_an_unknown_key_rather_than_blank_text() {
    assert_eq!(
        arg_error_text("Team", "topic", "engine.tools.noSuchKey"),
        "engine.tools.noSuchKey"
    );
}

fn setup() -> (tempfile::TempDir, NativeToolset) {
    setup_with_shell(None)
}

fn bridge() -> ShellPathBridge {
    ShellPathBridge::new(&crate::native::shell::resolve_shell(None).program)
}

#[test]
fn the_output_limit_reason_names_the_cap_and_its_relief() {
    let reason = output_limit_reason();
    assert!(
        reason.contains(&(MAX_TASK_OUTPUT_BYTES / (1024 * 1024)).to_string()),
        "the reason must name the real cap: {reason}"
    );
    assert!(
        !reason.contains("{{"),
        "an unfilled placeholder would reach the user: {reason}"
    );
    assert!(
        reason.len() > 40,
        "a missing key renders as the bare dotted key, which is not a message: {reason}"
    );
}

#[test]
fn canonical_tool_name_restores_the_tables_spelling() {
    let table = vec![
        ToolInfo {
            name: "Bash".into(),
            description: String::new(),
            input_schema: json!({}),
        },
        ToolInfo {
            name: "Read".into(),
            description: String::new(),
            input_schema: json!({}),
        },
    ];

    assert_eq!(canonical_tool_name("bash", &table), "Bash");
    assert_eq!(canonical_tool_name("READ", &table), "Read");

    assert_eq!(canonical_tool_name("Bash", &table), "Bash");
    assert_eq!(
        canonical_tool_name("mcp__github__search", &table),
        "mcp__github__search"
    );
    assert_eq!(canonical_tool_name("Nope", &table), "Nope");

    assert_eq!(canonical_tool_name("bash", &[]), "bash");
}

fn setup_with_shell(shell: Option<&str>) -> (tempfile::TempDir, NativeToolset) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "fn main() {}\n// beta marker\n",
    )
    .unwrap();
    let toolset = NativeToolset::new(dir.path().to_str().unwrap(), shell).unwrap();
    (dir, toolset)
}

struct ToolTableHost(Vec<crate::turn_loop::types::ToolInfo>);

impl crate::callbacks::HostCallbacks for ToolTableHost {
    fn llm_chat(
        &self,
        _request: crate::rpc::types::LlmChatRequest,
    ) -> crate::rpc::types::BoxFuture<'static, Result<crate::rpc::types::LlmChatResponse, String>>
    {
        Box::pin(async { Err("unused".into()) })
    }

    fn execute_tool(
        &self,
        _request: crate::rpc::types::ToolExecuteRequest,
    ) -> crate::rpc::types::BoxFuture<'static, Result<crate::rpc::types::ToolExecuteResponse, String>>
    {
        Box::pin(async { Err("unused".into()) })
    }

    fn check_permission(
        &self,
        _request: crate::rpc::types::PermissionCheckRequest,
    ) -> crate::rpc::types::BoxFuture<'static, Result<crate::rpc::types::PermissionDecision, String>>
    {
        Box::pin(async { Ok(crate::rpc::types::PermissionDecision::allow()) })
    }

    fn list_tools(
        &self,
    ) -> crate::rpc::types::BoxFuture<'static, Result<crate::rpc::types::ListToolsResponse, String>>
    {
        let tools = self.0.clone();
        Box::pin(async move { Ok(crate::rpc::types::ListToolsResponse { tools }) })
    }
}

fn tool_info(name: &str) -> crate::turn_loop::types::ToolInfo {
    crate::turn_loop::types::ToolInfo {
        name: name.to_string(),
        description: format!("{name} tool"),
        input_schema: json!({ "type": "object" }),
    }
}

#[tokio::test]
async fn select_tools_remembers_loaded_names() {
    let (_dir, ts) = setup();

    let manager = std::sync::Arc::new(crate::mcp::McpManager::new());
    manager
        .configure(
            "github",
            crate::mcp::manager::McpServerRecipe::Mock,
            crate::mcp::manager::McpServerOptions {
                deferred: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    manager
        .add_client(crate::mcp::client::McpClient::mock("github"))
        .await;
    let ts = ts
        .with_callbacks(std::sync::Arc::new(ToolTableHost(vec![tool_info("Read")])))
        .with_mcp(manager)
        .with_model_capabilities(Some(vec![
            "tool_use".into(),
            "dynamically_loaded_tools".into(),
        ]))
        .with_tool_select_enabled(true);

    let first = ts
        .execute_tool(
            "select_tools",
            &json!({ "names": ["mcp__github__github_sample_tool"] }),
        )
        .await
        .expect("select_tools is a native tool");
    assert!(!first.is_error, "{}", first.content);
    assert!(
        first
            .content
            .contains("Loaded: mcp__github__github_sample_tool"),
        "{}",
        first.content
    );
    assert!(
        first.delivery.is_none(),
        "the loaded schema travels through the tool table, not a delivered message"
    );

    let second = ts
        .execute_tool(
            "select_tools",
            &json!({ "names": ["mcp__github__github_sample_tool"] }),
        )
        .await
        .expect("select_tools is a native tool");
    assert!(
        second
            .content
            .contains("Already available: mcp__github__github_sample_tool"),
        "{}",
        second.content
    );
    assert!(
        second.delivery.is_none(),
        "nothing new was loaded, so nothing is delivered"
    );
}

#[tokio::test]
async fn select_tools_searches_deferred_tools_without_loading_them() {
    let (_dir, ts) = setup();
    let manager = std::sync::Arc::new(crate::mcp::McpManager::new());
    manager
        .configure(
            "github",
            crate::mcp::manager::McpServerRecipe::Mock,
            crate::mcp::manager::McpServerOptions {
                deferred: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    manager
        .add_client(crate::mcp::client::McpClient::mock("github"))
        .await;
    let ts = ts
        .with_callbacks(std::sync::Arc::new(ToolTableHost(vec![tool_info("Read")])))
        .with_mcp(manager)
        .with_model_capabilities(Some(vec![
            "tool_use".into(),
            "dynamically_loaded_tools".into(),
        ]))
        .with_tool_select_enabled(true);

    let search = ts
        .execute_tool("select_tools", &json!({ "query": "github sample" }))
        .await
        .expect("select_tools is a native tool");
    assert!(!search.is_error, "{}", search.content);
    assert!(
        search
            .content
            .starts_with("Search results for \"github sample\":"),
        "{}",
        search.content
    );
    assert!(
        search.content.contains("mcp__github__github_sample_tool"),
        "{}",
        search.content
    );
    assert!(
        search.content.contains("These are candidates."),
        "{}",
        search.content
    );

    let load = ts
        .execute_tool(
            "select_tools",
            &json!({ "names": ["mcp__github__github_sample_tool"] }),
        )
        .await
        .expect("select_tools is a native tool");
    assert!(
        load.content
            .contains("Loaded: mcp__github__github_sample_tool"),
        "{}",
        load.content
    );

    let neither = ts
        .execute_tool("select_tools", &json!({}))
        .await
        .expect("select_tools is a native tool");
    assert!(neither.is_error);
    assert_eq!(
        neither.content,
        crate::tools::select_tools::SELECT_TOOLS_PROVIDE_NAMES_OR_QUERY
    );

    let closed = ts.with_tool_select_enabled(false);
    let refused = closed
        .execute_tool("select_tools", &json!({ "query": "github" }))
        .await
        .expect("select_tools is a native tool");
    assert!(refused.is_error);
    assert_eq!(
        refused.content,
        crate::tools::select_tools::SELECT_TOOLS_UNAVAILABLE
    );
}

#[tokio::test]
async fn disclosure_shapes_the_tool_table_and_announces_on_new_turns() {
    let (_dir, mut ts) = setup();
    let manager = std::sync::Arc::new(crate::mcp::McpManager::new());
    manager
        .configure(
            "github",
            crate::mcp::manager::McpServerRecipe::Mock,
            crate::mcp::manager::McpServerOptions {
                deferred: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    manager
        .add_client(crate::mcp::client::McpClient::mock("github"))
        .await;
    ts = ts
        .with_mcp(manager.clone())
        .with_model_capabilities(Some(vec![
            "tool_use".into(),
            "dynamically_loaded_tools".into(),
        ]))
        .with_tool_select_enabled(true);

    let shaped = ts.shape_tool_table(vec![
        tool_info("Read"),
        tool_info("mcp__github__github_sample_tool"),
    ]);
    let names: Vec<&str> = shaped.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"Read"), "{names:?}");
    assert!(names.contains(&"select_tools"), "{names:?}");
    assert!(
        !names.contains(&"mcp__github__github_sample_tool"),
        "the deferred tool must leave the table: {names:?}"
    );

    let first = ts.take_disclosure_announcement(&[]).expect("announcement");
    assert!(first.contains("<tools_added>"), "{first}");
    assert!(first.contains("mcp__github__github_sample_tool"), "{first}");

    let history = vec![crate::injection::injection_message_named(
        crate::injection::wrap_system_reminder(&first),
        crate::tools::tool_select_state::LOADABLE_TOOLS_VARIANT,
    )];
    assert!(ts.take_disclosure_announcement(&history).is_none());
}

#[tokio::test]
async fn a_resumed_session_restores_its_loaded_deferred_tools() {
    let (_dir, ts) = setup();
    let manager = std::sync::Arc::new(crate::mcp::McpManager::new());
    manager
        .configure(
            "github",
            crate::mcp::manager::McpServerRecipe::Mock,
            crate::mcp::manager::McpServerOptions {
                deferred: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    manager
        .add_client(crate::mcp::client::McpClient::mock("github"))
        .await;
    let ts = ts
        .with_mcp(manager)
        .with_model_capabilities(Some(vec![
            "tool_use".into(),
            "dynamically_loaded_tools".into(),
        ]))
        .with_tool_select_enabled(true);

    let table = || {
        ts.shape_tool_table(vec![
            tool_info("Read"),
            tool_info("mcp__github__github_sample_tool"),
        ])
    };
    assert!(
        !table()
            .iter()
            .any(|tool| tool.name == "mcp__github__github_sample_tool"),
        "a fresh process starts with nothing loaded"
    );

    let history = vec![crate::injection::injection_message_named(
        crate::injection::wrap_system_reminder(
            "<tools_added>\nmcp__github__github_sample_tool\n</tools_added>",
        ),
        crate::tools::tool_select_state::LOADABLE_TOOLS_VARIANT,
    )];
    ts.seed_loaded_tools_from_history(&history);

    assert!(ts.is_dynamic_tool_loaded("mcp__github__github_sample_tool"));
    assert!(
        table()
            .iter()
            .any(|tool| tool.name == "mcp__github__github_sample_tool"),
        "the resumed session must expose the tool it had loaded"
    );
}

#[tokio::test]
async fn a_large_deferred_catalogue_announces_a_ranked_slice() {
    let (_dir, ts) = setup();
    let manager = std::sync::Arc::new(crate::mcp::McpManager::new());
    manager
        .configure(
            "big",
            crate::mcp::manager::McpServerRecipe::Mock,
            crate::mcp::manager::McpServerOptions {
                deferred: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let tools: Vec<crate::mcp::types::McpTool> = (0..22)
        .map(|index| crate::mcp::types::McpTool {
            name: format!("tool_{index:02}"),
            description: Some(format!("capability number {index}")),
            input_schema: json!({ "type": "object" }),
        })
        .collect();
    manager
        .add_client(crate::mcp::client::McpClient::mock_with_tools("big", tools))
        .await;
    let ts = ts
        .with_mcp(manager)
        .with_model_capabilities(Some(vec![
            "tool_use".into(),
            "dynamically_loaded_tools".into(),
        ]))
        .with_tool_select_enabled(true);

    let history = vec![crate::turn_loop::types::LLMMessage::user(
        "find the issue search tool",
    )];
    let announcement = ts
        .take_disclosure_announcement(&history)
        .expect("a large catalogue is announced");
    assert!(
        announcement.contains("<tools_relevant>"),
        "the slice replaces a full delta: {announcement}"
    );
    assert!(
        announcement.contains("These are the 10 most relevant of 22 loadable tools."),
        "{announcement}"
    );
    assert!(
        !announcement.contains("<tools_added>"),
        "a large catalogue is not dumped as an added list: {announcement}"
    );

    let mut repeated_history = history;
    repeated_history.push(crate::injection::injection_message_named(
        crate::injection::wrap_system_reminder(&announcement),
        crate::tools::tool_select_state::LOADABLE_TOOLS_VARIANT,
    ));
    assert!(ts.take_disclosure_announcement(&repeated_history).is_none());
}

#[tokio::test]
async fn disclosure_readvertises_the_loaded_tool_schema() {
    let (_dir, ts) = setup();
    let manager = std::sync::Arc::new(crate::mcp::McpManager::new());
    manager
        .configure(
            "github",
            crate::mcp::manager::McpServerRecipe::Mock,
            crate::mcp::manager::McpServerOptions {
                deferred: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    manager
        .add_client(crate::mcp::client::McpClient::mock("github"))
        .await;
    let ts = ts
        .with_mcp(manager.clone())
        .with_model_capabilities(Some(vec![
            "tool_use".into(),
            "dynamically_loaded_tools".into(),
        ]))
        .with_tool_select_enabled(true);

    let mcp_tool = manager
        .list_tool_infos()
        .await
        .into_iter()
        .find(|tool| tool.name == "mcp__github__github_sample_tool")
        .expect("the mock server's tool is in the catalogue");
    let table = vec![tool_info("Read"), mcp_tool.clone()];

    let before = ts.shape_tool_table(table.clone());
    assert!(
        !before
            .iter()
            .any(|tool| tool.name == "mcp__github__github_sample_tool"),
        "unloaded deferred tools stay out of the table"
    );

    let load = ts
        .execute_tool(
            "select_tools",
            &json!({ "names": ["mcp__github__github_sample_tool"] }),
        )
        .await
        .expect("select_tools is a native tool");
    assert!(!load.is_error, "{}", load.content);

    let after = ts.shape_tool_table(table);
    let advertised = after
        .iter()
        .find(|tool| tool.name == "mcp__github__github_sample_tool")
        .expect("a loaded deferred tool must be advertised again");
    assert_eq!(
        advertised.input_schema, mcp_tool.input_schema,
        "the advertised entry must carry the real tool schema"
    );
    assert_eq!(
        advertised.input_schema["properties"]["query"]["type"], "string",
        "the schema must be the MCP definition, not a placeholder"
    );

    let names: Vec<&str> = after.iter().map(|tool| tool.name.as_str()).collect();
    assert_eq!(
        names
            .iter()
            .filter(|name| **name == "mcp__github__github_sample_tool")
            .count(),
        1,
        "loaded tools appear once: {names:?}"
    );
}

#[tokio::test]
async fn disclosure_stays_off_without_capability_or_flag() {
    let (_dir, ts) = setup();
    let manager = std::sync::Arc::new(crate::mcp::McpManager::new());
    manager
        .configure(
            "github",
            crate::mcp::manager::McpServerRecipe::Mock,
            crate::mcp::manager::McpServerOptions {
                deferred: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    manager
        .add_client(crate::mcp::client::McpClient::mock("github"))
        .await;

    let ts = ts.with_mcp(manager.clone()).with_tool_select_enabled(true);
    let shaped = ts.shape_tool_table(vec![tool_info("mcp__github__github_sample_tool")]);
    assert_eq!(shaped.len(), 1);
    assert_eq!(shaped[0].name, "mcp__github__github_sample_tool");
    assert!(ts.take_disclosure_announcement(&[]).is_none());

    let ts = ts
        .with_model_capabilities(Some(vec!["dynamically_loaded_tools".into()]))
        .with_tool_select_enabled(true);
    assert!(!ts.supports_dynamic_tool_loading());
    assert!(ts.take_disclosure_announcement(&[]).is_none());

    let ts = ts
        .with_model_capabilities(Some(vec![
            "tool_use".into(),
            "dynamically_loaded_tools".into(),
        ]))
        .with_tool_select_enabled(false);
    let shaped = ts.shape_tool_table(vec![tool_info("mcp__github__github_sample_tool")]);
    assert_eq!(shaped[0].name, "mcp__github__github_sample_tool");
    assert!(ts.take_disclosure_announcement(&[]).is_none());
}

#[tokio::test]
async fn disclosure_refuses_a_call_to_a_deferred_unloaded_tool() {
    let (_dir, ts) = setup();
    let manager = std::sync::Arc::new(crate::mcp::McpManager::new());
    manager
        .configure(
            "github",
            crate::mcp::manager::McpServerRecipe::Mock,
            crate::mcp::manager::McpServerOptions {
                deferred: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    manager
        .add_client(crate::mcp::client::McpClient::mock("github"))
        .await;
    let ts = ts
        .with_mcp(manager)
        .with_model_capabilities(Some(vec![
            "tool_use".into(),
            "dynamically_loaded_tools".into(),
        ]))
        .with_tool_select_enabled(true);

    let refused = ts
        .execute_tool("mcp__github__github_sample_tool", &json!({}))
        .await
        .expect("the toolset handles the deferred name");
    assert!(refused.is_error, "{}", refused.content);
    assert!(
        refused
            .content
            .contains("Call select_tools with [\"mcp__github__github_sample_tool\"]"),
        "{}",
        refused.content
    );

    assert!(refused.note.is_none(), "note = {:?}", refused.note);

    let load = ts
        .execute_tool(
            "select_tools",
            &json!({ "names": ["mcp__github__github_sample_tool"] }),
        )
        .await
        .expect("select_tools is a native tool");
    assert!(!load.is_error, "{}", load.content);
    let executed = ts
        .execute_tool("mcp__github__github_sample_tool", &json!({}))
        .await
        .expect("now loaded, the toolset still handles it");
    assert!(!executed.is_error, "{}", executed.content);
    assert!(executed.note.is_none(), "note = {:?}", executed.note);
}

#[tokio::test]
async fn disclosure_waits_for_the_initial_mcp_discovery() {
    let (_dir, ts) = setup();
    let manager = std::sync::Arc::new(crate::mcp::McpManager::new());
    let ts = ts
        .with_mcp(manager.clone())
        .with_model_capabilities(Some(vec![
            "tool_use".into(),
            "dynamically_loaded_tools".into(),
        ]))
        .with_tool_select_enabled(true);

    ts.wait_for_mcp_discovery_before_disclosure().await;

    let handle = manager.connect_all(vec![(
        "github".into(),
        crate::mcp::manager::McpServerRecipe::Mock,
        crate::mcp::manager::McpServerOptions::default(),
    )]);
    handle.await.expect("connect_all settles");
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        ts.wait_for_mcp_discovery_before_disclosure(),
    )
    .await
    .expect("the wait settles once the connects do");

    let ts = ts.with_tool_select_enabled(false);
    ts.wait_for_mcp_discovery_before_disclosure().await;
}

fn find_bash() -> Option<String> {
    let candidates = [
        std::env::var("KIMI_SHELL_PATH").ok(),
        Some("bash".to_string()),
        Some("C:\\Program Files\\Git\\bin\\bash.exe".to_string()),
        Some("C:\\msys64\\usr\\bin\\bash.exe".to_string()),
    ];
    for candidate in candidates.into_iter().flatten() {
        let ok = std::process::Command::new(&candidate)
            .arg("-c")
            .arg("echo kimi-bash-probe")
            .output()
            .map(|out| {
                out.status.success()
                    && String::from_utf8_lossy(&out.stdout).contains("kimi-bash-probe")
            })
            .unwrap_or(false);
        if ok {
            return Some(candidate);
        }
    }
    None
}

fn find_powershell() -> Option<std::path::PathBuf> {
    super::powershell::resolve_powershell()
        .ok()
        .map(|exe| exe.path)
}

#[test]
fn new_rejects_missing_root() {
    assert!(NativeToolset::new("/definitely/not/a/real/dir", None).is_none());
}

#[test]
fn read_returns_numbered_lines() {
    let (_dir, ts) = setup();
    let result = ts.execute("Read", &json!({ "path": "a.txt" })).unwrap();
    assert!(!result.is_error);

    assert!(
        result.content.contains("1	alpha"),
        "content: {}",
        result.content
    );
    assert!(result.content.contains("3	gamma"));
}

#[test]
fn read_respects_offset_and_count() {
    let (_dir, ts) = setup();
    let result = ts
        .execute(
            "read",
            &json!({ "path": "a.txt", "line_offset": 2, "n_lines": 1 }),
        )
        .unwrap();
    assert!(result.content.contains("2	beta"));
    assert!(!result.content.contains("alpha"));
    assert!(!result.content.contains("3	gamma"));
}

#[test]
fn read_region_on_a_text_file_is_refused() {
    let (_dir, ts) = setup();
    let result = ts
        .execute(
            "Read",
            &json!({ "path": "a.txt", "region": { "x": 0, "y": 0, "width": 1, "height": 1 } }),
        )
        .expect("the media path owns an explicit region call");
    assert!(result.is_error);
    assert!(result.content.contains("is a text file"));
}

#[test]
fn read_crops_an_image_region() {
    let (dir, ts) = setup();
    let mut image = image::RgbaImage::new(200, 120);
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        *pixel = image::Rgba([(x % 256) as u8, (y % 256) as u8, 128, 255]);
    }
    let mut png = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut png, image::ImageFormat::Png)
        .expect("encode png");
    std::fs::write(dir.path().join("shot.png"), png.into_inner()).unwrap();

    let result = ts
        .execute(
            "Read",
            &json!({
                "path": "shot.png",
                "region": { "x": 10, "y": 20, "width": 40, "height": 30 }
            }),
        )
        .expect("media result");
    assert!(!result.is_error, "content: {}", result.content);
    let note = result.note.expect("note");
    assert!(
        note.contains("Showing region (x=10, y=20, width=40, height=30)"),
        "note: {note}"
    );
    assert!(result.delivery.is_none());
    assert!(matches!(
        result.parts.as_slice(),
        [
            crate::rpc::types::ContentBlock::Text { .. },
            crate::rpc::types::ContentBlock::Image { .. },
            crate::rpc::types::ContentBlock::Text { .. }
        ]
    ));
}

#[test]
fn read_full_resolution_sends_the_original_bytes() {
    let (dir, ts) = setup();
    let bytes = {
        let mut image = image::RgbaImage::new(48, 48);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            *pixel = image::Rgba([(x % 256) as u8, (y % 256) as u8, 128, 255]);
        }
        let mut png = std::io::Cursor::new(Vec::new());
        image
            .write_to(&mut png, image::ImageFormat::Png)
            .expect("encode png");
        png.into_inner()
    };
    std::fs::write(dir.path().join("shot.png"), &bytes).unwrap();

    let result = ts
        .execute(
            "Read",
            &json!({ "path": "shot.png", "full_resolution": true }),
        )
        .expect("media result");
    assert!(!result.is_error, "content: {}", result.content);
    assert!(
        result
            .note
            .expect("note")
            .contains("Shown at native resolution; no downscaling applied.")
    );
    let media = result
        .parts
        .iter()
        .find(|part| matches!(part, crate::rpc::types::ContentBlock::Image { .. }))
        .expect("an image block");
    match media {
        crate::rpc::types::ContentBlock::Image { data, .. } => {
            use base64::prelude::*;
            assert_eq!(*data, BASE64_STANDARD.encode(&bytes));
        }
        other => panic!("expected an image block, got {other:?}"),
    }
}

#[test]
fn grep_filters_sensitive_files() {
    let (_dir, ts) = setup();
    std::fs::write(
        _dir.path().join(".env"),
        "SECRET=leaked
",
    )
    .unwrap();
    std::fs::write(
        _dir.path().join("app.rs"),
        "SECRET=name
",
    )
    .unwrap();
    let result = ts.execute("Grep", &json!({ "pattern": "SECRET" })).unwrap();
    assert!(
        result.content.contains("app.rs"),
        "content: {}",
        result.content
    );
    assert!(
        !result.content.contains("leaked"),
        "sensitive content must be filtered"
    );
    assert!(result.content.contains("Filtered 1 sensitive file(s)"));
}

#[tokio::test]
async fn write_append_mode_appends() {
    let (_dir, ts) = setup();
    ts.execute_mutating("Write", &json!({ "path": "log.txt", "content": "one" }))
        .await
        .unwrap();
    let result = ts
        .execute_mutating(
            "Write",
            &json!({ "path": "log.txt", "content": "two", "mode": "append" }),
        )
        .await
        .unwrap();
    assert!(!result.is_error, "content: {}", result.content);
    assert!(result.content.starts_with("Appended"));
    assert_eq!(
        std::fs::read_to_string(_dir.path().join("log.txt")).unwrap(),
        "onetwo"
    );
}

#[tokio::test]
async fn write_unknown_mode_is_reported() {
    let (_dir, ts) = setup();
    let result = ts
        .execute_mutating(
            "Write",
            &json!({ "path": "a.txt", "content": "x", "mode": "truncate-half" }),
        )
        .await
        .expect("native write handled");
    assert!(result.is_error, "content: {}", result.content);
    assert!(
        result
            .content
            .contains("\"mode\" must be one of overwrite, append"),
        "content: {}",
        result.content
    );
}

#[tokio::test]
async fn write_records_file_history_and_revert_restores_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(crate::session::sqlite_store::SqliteSessionStore::in_memory().unwrap());
    let sid = "sess-file-history";
    store.create_session(sid, Some("fh")).unwrap();

    std::fs::write(dir.path().join("kept.txt"), "original").unwrap();

    let ts = NativeToolset::new(dir.path().to_str().unwrap(), None)
        .unwrap()
        .with_file_history(store.clone(), sid)
        .with_turn_id(1);

    let overwritten = ts
        .execute_mutating(
            "Write",
            &json!({ "path": "kept.txt", "content": "changed" }),
        )
        .await
        .expect("write executes natively");
    assert!(!overwritten.is_error, "{}", overwritten.content);
    let created = ts
        .execute_mutating("Write", &json!({ "path": "fresh.txt", "content": "new" }))
        .await
        .expect("write executes natively");
    assert!(!created.is_error, "{}", created.content);

    let (changes, _) = store.get_file_history_changes(sid, Some(1)).unwrap();
    assert_eq!(changes.len(), 2, "one row per write: {changes:?}");
    let named = |suffix: &str| {
        changes
            .iter()
            .any(|c| c.path.replace('\\', "/").ends_with(suffix))
    };
    assert!(named("kept.txt"), "{changes:?}");
    assert!(named("fresh.txt"), "{changes:?}");

    let (other_turn, _) = store.get_file_history_changes(sid, Some(2)).unwrap();
    assert!(other_turn.is_empty(), "{other_turn:?}");

    let reverted = store.revert_turn_file_changes(sid, 1, dir.path()).unwrap();
    assert_eq!(reverted.len(), 2, "{reverted:?}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("kept.txt")).unwrap(),
        "original"
    );
    assert!(
        !dir.path().join("fresh.txt").exists(),
        "a file the turn added is removed by the revert"
    );
}

#[tokio::test]
async fn write_without_recorder_records_nothing() {
    let (_dir, ts) = setup();
    let result = ts
        .execute_mutating("Write", &json!({ "path": "plain.txt", "content": "x" }))
        .await
        .unwrap();
    assert!(!result.is_error, "{}", result.content);
}

#[test]
fn background_bash_timeout_follows_call_then_config() {
    let (_dir, ts) = setup();

    assert_eq!(
        ts.background_bash_timeout(&json!({})),
        Some(Duration::from_secs(BASH_TASK_DEFAULT_TIMEOUT_S))
    );

    let configured = ts.with_bash_task_timeout(Some(1200));
    assert_eq!(
        configured.background_bash_timeout(&json!({})),
        Some(Duration::from_secs(1200))
    );

    assert_eq!(
        configured.background_bash_timeout(&json!({ "timeout": 30 })),
        Some(Duration::from_secs(30))
    );

    assert_eq!(
        configured.background_bash_timeout(&json!({ "timeout": 0 })),
        None
    );
    let (_dir2, unset) = setup();
    assert_eq!(
        unset
            .with_bash_task_timeout(Some(0))
            .background_bash_timeout(&json!({})),
        None
    );

    assert_eq!(
        configured.background_bash_timeout(&json!({ "timeout": 30, "disable_timeout": true })),
        None
    );

    assert_eq!(
        configured.background_bash_timeout(&json!({ "timeout": BASH_TASK_MAX_SECONDS + 1 })),
        Some(Duration::from_secs(BASH_TASK_MAX_SECONDS))
    );
    let clamped = configured.with_bash_task_timeout(Some(BASH_TASK_MAX_SECONDS + 1));
    assert_eq!(
        clamped.background_bash_timeout(&json!({})),
        Some(Duration::from_secs(BASH_TASK_MAX_SECONDS))
    );
}

#[test]
fn bash_auto_background_defaults_on_and_is_overridable() {
    let (_dir, ts) = setup();
    assert!(ts.bash_auto_background);

    let unset = ts.with_bash_auto_background(None);
    assert!(unset.bash_auto_background);

    let off = unset.with_bash_auto_background(Some(false));
    assert!(!off.bash_auto_background);
}

#[test]
fn sensitive_file_rules_match_v2() {
    assert!(crate::native::file_type::is_sensitive_file(".env"));
    assert!(crate::native::file_type::is_sensitive_file("config/.env"));
    assert!(crate::native::file_type::is_sensitive_file("keys/id_rsa"));
    assert!(crate::native::file_type::is_sensitive_file("id_rsa.pem"));
    assert!(crate::native::file_type::is_sensitive_file(
        ".aws/credentials"
    ));
    assert!(crate::native::file_type::is_sensitive_file(
        "C:/repo/.env.local"
    ));
    assert!(!crate::native::file_type::is_sensitive_file(".env.example"));
    assert!(!crate::native::file_type::is_sensitive_file("id_rsa.pub"));
    assert!(!crate::native::file_type::is_sensitive_file("src/main.rs"));
}

#[test]
fn read_outside_workspace_is_served_natively() {
    let (_dir, ts) = setup();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), "nope").unwrap();
    let escaped = outside.path().join("secret.txt");
    let result = ts
        .execute("Read", &json!({ "path": escaped.to_str().unwrap() }))
        .expect("reads must not be declined for lying outside the workspace");
    assert!(!result.is_error, "content: {}", result.content);
    assert!(
        result.content.contains("nope"),
        "content: {}",
        result.content
    );
}

#[test]
fn grep_search_policy_refuses_a_relative_escape_but_allows_an_absolute_one() {
    let (_dir, ts) = setup();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("far.txt"), "needle outside\n").unwrap();

    let refused = ts
        .execute(
            "Grep",
            &json!({ "pattern": "needle", "path": "../../../../../../etc" }),
        )
        .expect("a refused path is still a served call, not a declined one");
    assert!(refused.is_error, "content: {}", refused.content);
    assert!(
        refused.content.contains("is not an absolute path"),
        "content: {}",
        refused.content
    );

    let allowed = ts
        .execute(
            "Grep",
            &json!({ "pattern": "needle", "path": outside.path() }),
        )
        .expect("an absolute path outside the workspace stays searchable");
    assert!(!allowed.is_error, "content: {}", allowed.content);
    assert!(
        allowed.content.contains("far.txt"),
        "content: {}",
        allowed.content
    );
}

#[tokio::test]
async fn disabled_tool_refusal_carries_no_provenance_tag() {
    let (_dir, ts) = setup();
    let ts = ts.with_tools_filter(Some(crate::tools::tool_policy::ToolsFilter {
        enabled: Vec::new(),
        disabled: vec!["Bash".into()],
    }));

    let refused = ts
        .execute_tool_streaming(None, "Bash", &json!({ "command": "echo hi" }), None)
        .await
        .expect("a filtered tool is still a served call");

    assert!(refused.is_error, "{}", refused.content);
    assert!(refused.content.contains("Bash"), "{}", refused.content);
    assert!(refused.note.is_none(), "note = {:?}", refused.note);
}

#[test]
fn glob_search_policy_refuses_a_relative_escape() {
    let (_dir, ts) = setup();
    let refused = ts
        .execute(
            "Glob",
            &json!({ "pattern": "**/*.txt", "path": "../../../../../../etc" }),
        )
        .expect("a refused path is still a served call, not a declined one");
    assert!(refused.is_error, "content: {}", refused.content);
    assert!(
        refused.content.contains("is not an absolute path"),
        "content: {}",
        refused.content
    );
}

#[test]
fn grep_search_policy_does_not_refuse_a_sensitive_path() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".env"), "SECRET_TOKEN=needle\n").unwrap();
    std::fs::write(dir.path().join("plain.txt"), "needle plain\n").unwrap();
    let ts = NativeToolset::new(dir.path().to_str().unwrap(), None).unwrap();

    let result = ts
        .execute("Grep", &json!({ "pattern": "needle", "path": dir.path() }))
        .expect("a sensitive path is searched, not refused");
    assert!(!result.is_error, "content: {}", result.content);
    assert!(
        result.content.contains("plain.txt"),
        "content: {}",
        result.content
    );
    assert!(
        !result.content.contains("SECRET_TOKEN"),
        "the hit itself must be filtered, not just the call: {}",
        result.content
    );
}

#[test]
fn read_through_a_symlink_onto_a_sensitive_file_is_refused() {
    let (dir, ts) = setup();
    let secret = dir.path().join(".env");
    std::fs::write(&secret, "SECRET_TOKEN=1\n").unwrap();
    let link = dir.path().join("notes.txt");
    #[cfg(windows)]
    let made = std::os::windows::fs::symlink_file(&secret, &link).is_ok();
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink(&secret, &link).is_ok();
    if !made {
        eprintln!("skipping: this platform would not create the symlink");
        return;
    }

    let result = ts
        .execute("Read", &json!({ "path": link.to_str().unwrap() }))
        .expect("the call is served, with a refusal result");
    assert!(result.is_error, "content: {}", result.content);
    assert!(
        result.content.contains(".env"),
        "the refusal must name the target: {}",
        result.content
    );
    assert!(
        !result.content.contains("SECRET_TOKEN"),
        "the secret must never reach the model: {}",
        result.content
    );
}

#[test]
fn a_symlink_outside_the_workspace_is_still_served() {
    let (_dir, ts) = setup();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("elsewhere.txt");
    std::fs::write(&target, "reachable\n").unwrap();
    let link = outside.path().join("alias.txt");
    #[cfg(windows)]
    let made = std::os::windows::fs::symlink_file(&target, &link).is_ok();
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink(&target, &link).is_ok();
    if !made {
        eprintln!("skipping: this platform would not create the symlink");
        return;
    }
    let result = ts
        .execute("Read", &json!({ "path": link.to_str().unwrap() }))
        .expect("reads must not be declined for lying outside the workspace");
    assert!(!result.is_error, "content: {}", result.content);
    assert!(
        result.content.contains("reachable"),
        "content: {}",
        result.content
    );
}

#[test]
fn read_negative_offset_reads_tail_natively() {
    let (_dir, ts) = setup();

    let tail = ts
        .execute("Read", &json!({ "path": "a.txt", "line_offset": -2 }))
        .expect("tail reads must be served natively, not declined");
    assert!(!tail.is_error, "content: {}", tail.content);
    assert!(
        tail.content.contains("2\tbeta") && tail.content.contains("3\tgamma"),
        "content: {}",
        tail.content
    );
    assert!(
        !tail.content.contains("1\talpha"),
        "content: {}",
        tail.content
    );

    let whole = ts
        .execute("Read", &json!({ "path": "a.txt", "line_offset": -100 }))
        .expect("oversized tail must clamp to the file start");
    assert!(
        whole.content.contains("1\talpha"),
        "content: {}",
        whole.content
    );
}

#[test]
fn read_negative_offset_respects_n_lines_window() {
    let (dir, ts) = setup();
    let body: String = (1..=10).map(|i| format!("line{i}\n")).collect();
    std::fs::write(dir.path().join("ten.txt"), body).unwrap();

    let result = ts
        .execute(
            "Read",
            &json!({ "path": "ten.txt", "line_offset": -3, "n_lines": 2 }),
        )
        .expect("tail reads must be served natively");
    assert!(!result.is_error, "content: {}", result.content);
    assert!(
        result.content.contains("8\tline8") && result.content.contains("9\tline9"),
        "content: {}",
        result.content
    );
    assert!(
        !result.content.contains("10\tline10"),
        "n_lines must cap the tail window: {}",
        result.content
    );
}

fn utf16le_bytes(text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for unit in text.encode_utf16() {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out
}

fn utf16be_bytes(text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for unit in text.encode_utf16() {
        out.extend_from_slice(&unit.to_be_bytes());
    }
    out
}

#[test]
fn read_transcodes_utf16le_bom() {
    let (_dir, ts) = setup();
    let mut bytes = vec![0xff, 0xfe];
    bytes.extend_from_slice(&utf16le_bytes("alpha\nbeta\n"));
    std::fs::write(_dir.path().join("u16.txt"), bytes).unwrap();
    let result = ts.execute("Read", &json!({ "path": "u16.txt" })).unwrap();
    assert!(!result.is_error, "content: {}", result.content);
    assert!(
        result.content.contains("1\talpha"),
        "content: {}",
        result.content
    );
    assert!(
        result.content.contains("2\tbeta"),
        "content: {}",
        result.content
    );
    let note = result.note.unwrap();
    assert!(
        note.contains("Detected file encoding: UTF-16 LE"),
        "note: {note}"
    );
}

#[test]
fn read_transcodes_utf16be_bom() {
    let (_dir, ts) = setup();
    let mut bytes = vec![0xfe, 0xff];
    bytes.extend_from_slice(&utf16be_bytes("alpha\nbeta\n"));
    std::fs::write(_dir.path().join("u16.txt"), bytes).unwrap();
    let result = ts.execute("Read", &json!({ "path": "u16.txt" })).unwrap();
    assert!(!result.is_error, "content: {}", result.content);
    assert!(
        result.content.contains("1\talpha"),
        "content: {}",
        result.content
    );
    let note = result.note.unwrap();
    assert!(
        note.contains("Detected file encoding: UTF-16 BE"),
        "note: {note}"
    );
}

#[test]
fn read_transcodes_bomless_utf16le() {
    let (_dir, ts) = setup();

    std::fs::write(_dir.path().join("u16.txt"), utf16le_bytes("alpha\n")).unwrap();
    let result = ts.execute("Read", &json!({ "path": "u16.txt" })).unwrap();
    assert!(!result.is_error, "content: {}", result.content);
    assert!(
        result.content.contains("1\talpha"),
        "content: {}",
        result.content
    );
}

#[test]
fn read_binary_nul_reports_not_readable() {
    let (_dir, ts) = setup();
    std::fs::write(_dir.path().join("bin.dat"), b"plain prefix\x00\x01").unwrap();
    let result = ts.execute("Read", &json!({ "path": "bin.dat" })).unwrap();
    assert!(result.is_error, "content: {}", result.content);
    assert!(
        result.content.contains("is not readable as UTF-8 text"),
        "content: {}",
        result.content
    );
}

#[test]
fn read_invalid_utf8_reports_not_decodable() {
    let (_dir, ts) = setup();
    std::fs::write(_dir.path().join("bad.txt"), b"a\xffb").unwrap();
    let result = ts.execute("Read", &json!({ "path": "bad.txt" })).unwrap();
    assert!(result.is_error, "content: {}", result.content);
    assert!(
        result.content.contains("is not valid UTF-8 or UTF-16 text"),
        "content: {}",
        result.content
    );
}

#[test]
fn read_strips_utf8_bom() {
    let (_dir, ts) = setup();
    let mut bytes = vec![0xef, 0xbb, 0xbf];
    bytes.extend_from_slice(b"alpha\n");
    std::fs::write(_dir.path().join("bom.txt"), bytes).unwrap();
    let result = ts.execute("Read", &json!({ "path": "bom.txt" })).unwrap();
    assert!(!result.is_error, "content: {}", result.content);
    assert!(
        result.content.contains("1\talpha"),
        "content: {}",
        result.content
    );
    assert!(
        !result.content.contains('\u{FEFF}'),
        "BOM must be stripped: {}",
        result.content
    );
}

#[test]
fn read_pure_crlf_normalized() {
    let (_dir, ts) = setup();
    std::fs::write(_dir.path().join("crlf.txt"), b"alpha\r\nbeta\r\n").unwrap();
    let result = ts.execute("Read", &json!({ "path": "crlf.txt" })).unwrap();
    assert!(!result.is_error, "content: {}", result.content);
    assert!(
        result.content.contains("1\talpha"),
        "content: {}",
        result.content
    );
    assert!(
        !result.content.contains('\r'),
        "pure CRLF renders without CRs: {}",
        result.content
    );
    let note = result.note.unwrap();
    assert!(
        !note.contains("carriage-return"),
        "pure CRLF must not report mixed endings: {note}"
    );
}

#[test]
fn read_mixed_line_endings_make_cr_visible() {
    let (_dir, ts) = setup();
    std::fs::write(_dir.path().join("mixed.txt"), b"a\nb\r\n").unwrap();
    let result = ts.execute("Read", &json!({ "path": "mixed.txt" })).unwrap();
    assert!(!result.is_error, "content: {}", result.content);
    assert!(
        result.content.contains("2\tb\\r"),
        "content: {}",
        result.content
    );
    let note = result.note.unwrap();
    assert!(
        note.contains("carriage-return"),
        "mixed endings must be reported: {note}"
    );
}

#[test]
fn read_returns_a_long_line_whole() {
    let (_dir, ts) = setup();
    let long_line = "a".repeat(3000);
    std::fs::write(
        _dir.path().join("long.txt"),
        format!("{long_line}\nshort\n"),
    )
    .unwrap();
    let result = ts.execute("Read", &json!({ "path": "long.txt" })).unwrap();
    assert!(!result.is_error, "content: {}", result.content);
    let first = result.content.split('\n').next().unwrap();
    let text = first.strip_prefix("1\t").unwrap();
    assert_eq!(text, long_line, "the line is not truncated");
    assert!(!text.ends_with("..."), "no marker is appended: {text}");
    let note = result.note.unwrap();
    assert!(!note.contains("were truncated"), "note: {note}");
}

#[test]
fn read_pages_on_characters_with_no_byte_ceiling() {
    let (_dir, ts) = setup();
    let mut content = String::new();
    for _ in 0..100 {
        content.push_str(&"x".repeat(1100));
        content.push('\n');
    }
    std::fs::write(_dir.path().join("wide.txt"), content).unwrap();
    let result = ts
        .execute("Read", &json!({ "path": "wide.txt", "max_chars": 500_000 }))
        .unwrap();
    assert!(!result.is_error, "content: {}", result.content);
    let note = result.note.unwrap();
    assert!(note.contains("Requested range complete."), "note: {note}");
    assert!(
        note.contains("Effective max_chars: 500000."),
        "note: {note}"
    );
    assert!(note.contains("End of file reached."), "note: {note}");
    assert!(
        result.content.contains("100\t"),
        "the whole page is returned; no byte ceiling cuts it early: {}",
        result.content
    );
}

#[test]
fn read_reports_range_too_large_for_an_explicit_line_request() {
    let (_dir, ts) = setup();
    let mut content = String::new();
    for _ in 0..4725 {
        content.push_str(&"x".repeat(100));
        content.push('\n');
    }
    std::fs::write(_dir.path().join("dense.txt"), content).unwrap();
    let result = ts
        .execute(
            "Read",
            &json!({ "path": "dense.txt", "max_chars": 500_000, "n_lines": 4725 }),
        )
        .unwrap();
    assert!(result.is_error, "content: {}", result.content);
    assert!(
        result.content.contains("more than one Read can return"),
        "content: {}",
        result.content
    );
    assert!(
        result.content.contains("Read at most"),
        "content: {}",
        result.content
    );
}

#[test]
fn read_output_character_budget_reports_the_v2_summary() {
    let (_dir, ts) = setup();
    let mut content = String::new();
    for _ in 0..100 {
        content.push_str(&"x".repeat(1100));
        content.push('\n');
    }
    std::fs::write(_dir.path().join("wide.txt"), content).unwrap();
    let result = ts.execute("Read", &json!({ "path": "wide.txt" })).unwrap();
    assert!(!result.is_error, "content: {}", result.content);
    let note = result.note.unwrap();
    assert!(note.contains("Character limit reached."), "note: {note}");
    assert!(
        note.contains("Effective max_chars: 100000."),
        "note: {note}"
    );
    assert!(note.contains("Next Read: {"), "note: {note}");
}

#[test]
fn read_has_no_line_count_ceiling() {
    let (_dir, ts) = setup();
    let mut content = String::new();
    for i in 1..=1500 {
        content.push_str(&format!("line {i}\n"));
    }
    std::fs::write(_dir.path().join("many.txt"), content).unwrap();
    let result = ts.execute("Read", &json!({ "path": "many.txt" })).unwrap();
    assert!(!result.is_error, "content: {}", result.content);
    let note = result.note.unwrap();
    assert!(
        note.contains("1500 lines read from file starting from line 1."),
        "note: {note}"
    );
    assert!(!note.contains("Max 1000 lines"), "note: {note}");
    assert!(
        result.content.contains("1500\tline 1500"),
        "the whole file is in one page: {}",
        result.content
    );
}

#[test]
fn read_empty_file_with_offset_does_not_panic() {
    let (_dir, ts) = setup();
    std::fs::write(_dir.path().join("empty.txt"), b"").unwrap();
    let result = ts
        .execute("Read", &json!({ "path": "empty.txt", "line_offset": 5 }))
        .unwrap();
    assert!(!result.is_error, "content: {}", result.content);
    assert!(result.content.is_empty(), "content: {}", result.content);
    let note = result.note.unwrap();
    assert!(note.contains("No lines read from file"), "note: {note}");
    assert!(note.contains("Total lines in file: 0."), "note: {note}");
}

#[test]
fn read_line_offset_accepts_numeric_string_and_float() {
    let (_dir, ts) = setup();
    for offset in [json!("2"), json!(2.0)] {
        let result = ts
            .execute("Read", &json!({ "path": "a.txt", "line_offset": offset }))
            .unwrap_or_else(|| panic!("declined for {offset}"));
        assert!(!result.is_error, "content: {}", result.content);
        assert!(
            result.content.contains("beta"),
            "expected forward read from line 2, content: {}",
            result.content
        );
    }
}

#[test]
fn read_large_single_line_file_is_served_natively() {
    let (_dir, ts) = setup();
    std::fs::write(
        _dir.path().join("big.txt"),
        vec![b'a'; 10 * 1024 * 1024 + 1],
    )
    .unwrap();
    let result = ts
        .execute("Read", &json!({ "path": "big.txt" }))
        .expect("large files stream; they are not declined");
    assert!(!result.is_error, "content: {}", result.content);
    assert!(
        result.content.contains("1\t"),
        "content: {}",
        result.content
    );
}

#[tokio::test]
async fn grep_finds_matches_across_files() {
    let (_dir, ts) = setup();
    let result = ts
        .execute(
            "Grep",
            &json!({ "pattern": "beta", "output_mode": "content" }),
        )
        .unwrap();
    assert!(!result.is_error);
    assert!(
        result.content.contains("a.txt:2"),
        "content: {}",
        result.content
    );
    assert!(
        result.content.contains("lib.rs:2"),
        "content: {}",
        result.content
    );
}

#[tokio::test]
async fn grep_default_mode_lists_files_by_recency() {
    let (dir, ts) = setup();

    std::fs::write(dir.path().join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
    let result = ts.execute("Grep", &json!({ "pattern": "beta" })).unwrap();
    assert!(!result.is_error);
    let lines: Vec<&str> = result.content.lines().collect();
    let lib_rs = if cfg!(windows) {
        "src\\lib.rs"
    } else {
        "src/lib.rs"
    };
    assert_eq!(lines, vec!["a.txt", lib_rs], "content: {}", result.content);
}

#[tokio::test]
async fn grep_with_glob_filter() {
    let (_dir, ts) = setup();
    let result = ts
        .execute(
            "Grep",
            &json!({ "pattern": "beta", "glob": "*.rs", "output_mode": "content" }),
        )
        .unwrap();
    assert!(result.content.contains("lib.rs"));
    assert!(!result.content.contains("a.txt:"));
}

#[tokio::test]
async fn grep_case_insensitive_flag() {
    let (_dir, ts) = setup();
    let sensitive = ts
        .execute(
            "Grep",
            &json!({ "pattern": "BETA", "output_mode": "content" }),
        )
        .unwrap();
    assert!(
        sensitive.content.contains("No matches"),
        "content: {}",
        sensitive.content
    );
    let insensitive = ts
        .execute(
            "Grep",
            &json!({ "pattern": "BETA", "-i": true, "output_mode": "content" }),
        )
        .unwrap();
    assert!(
        insensitive.content.contains("beta"),
        "content: {}",
        insensitive.content
    );
}

#[tokio::test]
async fn grep_content_context_lines() {
    let (_dir, ts) = setup();
    let result = ts
        .execute(
            "Grep",
            &json!({
                "pattern": "beta",
                "output_mode": "content",
                "-C": 1,
            }),
        )
        .unwrap();

    assert!(
        result.content.contains("a.txt-1-alpha"),
        "content: {}",
        result.content
    );
    assert!(
        result.content.contains("a.txt:2:beta"),
        "content: {}",
        result.content
    );
    assert!(
        result.content.contains("a.txt-3-gamma"),
        "content: {}",
        result.content
    );
}

#[tokio::test]
async fn grep_context_clusters_separated_by_dash_dash() {
    let (_dir, ts) = setup();

    std::fs::write(
        _dir.path().join("spread.txt"),
        "m1




m2
",
    )
    .unwrap();
    let result = ts
        .execute(
            "Grep",
            &json!({ "pattern": "m[12]", "output_mode": "content", "-A": 1, "-B": 1 }),
        )
        .unwrap();
    assert!(result.content.contains("--"), "content: {}", result.content);
}

#[tokio::test]
async fn grep_count_matches_with_summary() {
    let (_dir, ts) = setup();
    let result = ts
        .execute(
            "Grep",
            &json!({ "pattern": "beta", "output_mode": "count_matches" }),
        )
        .unwrap();
    let count_line = if cfg!(windows) {
        "src\\lib.rs:1"
    } else {
        "src/lib.rs:1"
    };
    assert!(
        result.content.contains(count_line),
        "content: {}",
        result.content
    );
    assert!(
        result
            .content
            .contains("Found 2 total occurrences across 2 files."),
        "content: {}",
        result.content
    );
}

#[tokio::test]
async fn grep_head_limit_paginates() {
    let (_dir, ts) = setup();
    let result = ts
        .execute(
            "Grep",
            &json!({ "pattern": "beta", "output_mode": "content", "head_limit": 1 }),
        )
        .unwrap();
    let first_line = result.content.lines().next().unwrap();
    assert!(first_line.contains("beta"), "content: {}", result.content);
    assert!(
        result.content.contains("Results truncated to 1 lines"),
        "content: {}",
        result.content
    );
}

#[test]
fn grep_invalid_regex_is_an_error_result() {
    let (_dir, ts) = setup();
    let result = ts
        .execute("Grep", &json!({ "pattern": "([unclosed" }))
        .unwrap();
    assert!(result.is_error);
    assert!(result.content.contains("invalid regex"));
}

#[tokio::test]
async fn grep_with_type_filter_matches_natively() {
    let (_dir, ts) = setup();

    let rust = ts
        .execute(
            "Grep",
            &json!({ "pattern": "beta", "type": "rust", "output_mode": "content" }),
        )
        .unwrap();
    assert!(!rust.is_error, "content: {}", rust.content);
    assert!(
        rust.content.contains("lib.rs:2"),
        "content: {}",
        rust.content
    );
    assert!(
        !rust.content.contains("a.txt:"),
        "type=rust must exclude a.txt: {}",
        rust.content
    );

    let txt = ts
        .execute(
            "Grep",
            &json!({ "pattern": "beta", "type": "txt", "output_mode": "content" }),
        )
        .unwrap();
    assert!(
        txt.content.contains("a.txt:2:beta"),
        "content: {}",
        txt.content
    );
    assert!(
        !txt.content.contains("lib.rs"),
        "type=txt must exclude lib.rs: {}",
        txt.content
    );
}

#[test]
fn grep_with_unknown_type_is_reported() {
    let (_dir, ts) = setup();

    let unknown = ts
        .execute("Grep", &json!({ "pattern": "x", "type": "kimiunknown" }))
        .unwrap();
    assert!(unknown.is_error, "content: {}", unknown.content);
    assert!(
        unknown
            .content
            .contains("unrecognized file type: kimiunknown"),
        "content: {}",
        unknown.content
    );

    assert!(
        ts.execute("Grep", &json!({ "pattern": "x", "type": "rust" }))
            .is_some(),
        "known type must stay native"
    );

    let mistyped = ts
        .execute("Grep", &json!({ "pattern": "x", "type": 7 }))
        .unwrap();
    assert!(mistyped.is_error, "content: {}", mistyped.content);
}

#[tokio::test]
async fn grep_include_ignored_searches_gitignored_files() {
    let dir = tempfile::tempdir().unwrap();

    std::fs::create_dir(dir.path().join(".git")).unwrap();
    std::fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
    std::fs::write(dir.path().join("ignored.txt"), "needle ignored\n").unwrap();
    std::fs::write(dir.path().join("kept.txt"), "needle kept\n").unwrap();
    let ts = NativeToolset::new(dir.path().to_str().unwrap(), None).unwrap();

    let default = ts.execute("Grep", &json!({ "pattern": "needle" })).unwrap();
    assert!(
        default.content.contains("kept.txt"),
        "content: {}",
        default.content
    );
    assert!(
        !default.content.contains("ignored.txt"),
        "gitignored file must be skipped by default: {}",
        default.content
    );

    let with_ignored = ts
        .execute(
            "Grep",
            &json!({ "pattern": "needle", "include_ignored": true }),
        )
        .unwrap();
    assert!(
        with_ignored.content.contains("kept.txt"),
        "content: {}",
        with_ignored.content
    );
    assert!(
        with_ignored.content.contains("ignored.txt"),
        "include_ignored must surface the gitignored file: {}",
        with_ignored.content
    );
}

#[tokio::test]
async fn grep_multiline_spans_lines_with_per_line_numbers() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("span.txt"),
        "alpha\nstart MATCH\nmiddle\nend MATCH\ntail\nbeta\n",
    )
    .unwrap();
    let ts = NativeToolset::new(dir.path().to_str().unwrap(), None).unwrap();
    let result = ts
        .execute(
            "Grep",
            &json!({
                "pattern": "start MATCH.*?end MATCH",
                "output_mode": "content",
                "multiline": true,
            }),
        )
        .unwrap();
    assert!(!result.is_error, "content: {}", result.content);

    let lines: Vec<&str> = result.content.lines().collect();
    assert_eq!(
        lines,
        vec![
            "span.txt:2:start MATCH",
            "span.txt:3:middle",
            "span.txt:4:end MATCH",
        ],
        "content: {}",
        result.content
    );
}

#[tokio::test]
async fn grep_multiline_count_matches_counts_matches_not_lines() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("span.txt"),
        "alpha\nstart MATCH\nmiddle\nend MATCH\ntail\nbeta\n",
    )
    .unwrap();
    let ts = NativeToolset::new(dir.path().to_str().unwrap(), None).unwrap();
    let result = ts
        .execute(
            "Grep",
            &json!({
                "pattern": "start MATCH.*?end MATCH",
                "output_mode": "count_matches",
                "multiline": true,
            }),
        )
        .unwrap();

    assert!(
        result.content.contains("span.txt:1"),
        "content: {}",
        result.content
    );
    assert!(
        result
            .content
            .contains("Found 1 total occurrence across 1 file."),
        "content: {}",
        result.content
    );
}

#[tokio::test]
async fn grep_multiline_context_and_cluster_separator() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("span.txt"),
        "alpha\nstart MATCH\nmiddle\nend MATCH\ntail\nbeta\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("multi.txt"),
        "x1\nA\nB\ny1\ny2\ny3\nA\nB\nz1\n",
    )
    .unwrap();
    let ts = NativeToolset::new(dir.path().to_str().unwrap(), None).unwrap();

    let span = ts
        .execute(
            "Grep",
            &json!({
                "pattern": "start MATCH.*?end MATCH",
                "output_mode": "content",
                "multiline": true,
                "-C": 1,
                "path": "span.txt",
            }),
        )
        .unwrap();
    let span_lines: Vec<&str> = span.content.lines().collect();
    assert_eq!(
        span_lines,
        vec![
            "span.txt-1-alpha",
            "span.txt:2:start MATCH",
            "span.txt:3:middle",
            "span.txt:4:end MATCH",
            "span.txt-5-tail",
        ],
        "content: {}",
        span.content
    );

    let multi = ts
        .execute(
            "Grep",
            &json!({
                "pattern": "A\nB",
                "output_mode": "content",
                "multiline": true,
                "-C": 1,
                "path": "multi.txt",
            }),
        )
        .unwrap();
    let multi_lines: Vec<&str> = multi.content.lines().collect();
    assert_eq!(
        multi_lines,
        vec![
            "multi.txt-1-x1",
            "multi.txt:2:A",
            "multi.txt:3:B",
            "multi.txt-4-y1",
            "--",
            "multi.txt-6-y3",
            "multi.txt:7:A",
            "multi.txt:8:B",
            "multi.txt-9-z1",
        ],
        "content: {}",
        multi.content
    );
}

#[tokio::test]
async fn grep_type_include_ignored_multiline_combine() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".git")).unwrap();
    std::fs::write(dir.path().join(".gitignore"), "combo.ts\n").unwrap();
    std::fs::write(
        dir.path().join("combo.ts"),
        "head\nstart MATCH\nmid\nend MATCH\ntail\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("combo.txt"), "start MATCH\nend MATCH\n").unwrap();
    let ts = NativeToolset::new(dir.path().to_str().unwrap(), None).unwrap();

    let result = ts
        .execute(
            "Grep",
            &json!({
                "pattern": "start MATCH.*?end MATCH",
                "output_mode": "content",
                "type": "ts",
                "include_ignored": true,
                "multiline": true,
            }),
        )
        .unwrap();
    assert!(!result.is_error, "content: {}", result.content);
    assert!(
        result.content.contains("combo.ts:2:start MATCH"),
        "content: {}",
        result.content
    );
    assert!(
        result.content.contains("combo.ts:4:end MATCH"),
        "content: {}",
        result.content
    );
    assert!(
        !result.content.contains("combo.txt"),
        "type=ts must exclude combo.txt: {}",
        result.content
    );
}

#[tokio::test]
async fn grep_skips_vcs_directories() {
    let (_dir, ts) = setup();
    std::fs::create_dir_all(_dir.path().join(".git")).unwrap();
    std::fs::write(_dir.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(_dir.path().join("notes.txt"), "ref: something\n").unwrap();
    let result = ts
        .execute(
            "Grep",
            &json!({ "pattern": "ref:", "output_mode": "content" }),
        )
        .unwrap();
    assert!(!result.is_error, "content: {}", result.content);
    assert!(
        result.content.contains("notes.txt"),
        "content: {}",
        result.content
    );
    assert!(
        !result.content.contains(".git"),
        "VCS metadata must be excluded: {}",
        result.content
    );
}

#[test]
fn glob_judges_vcs_exclusion_below_the_search_root_only() {
    let dir = tempfile::tempdir().unwrap();

    let proj = dir.path().join(".hg").join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(proj.join("keep.txt"), "hello\n").unwrap();
    let ts = NativeToolset::new(dir.path().to_str().unwrap(), None).unwrap();
    let result = ts
        .execute(
            "Glob",
            &json!({ "pattern": "**/*.txt", "path": ".hg/proj" }),
        )
        .unwrap();
    assert!(
        result.content.contains("keep.txt"),
        "a search root under a VCS-named dir must still yield results: {}",
        result.content
    );
}

#[tokio::test]
async fn grep_count_matches_counts_occurrences() {
    let (_dir, ts) = setup();
    std::fs::write(_dir.path().join("multi.txt"), "beta beta beta\n").unwrap();
    let result = ts
        .execute(
            "Grep",
            &json!({ "pattern": "beta", "output_mode": "count_matches" }),
        )
        .unwrap();
    assert!(!result.is_error, "content: {}", result.content);
    assert!(
        result.content.contains("multi.txt:3"),
        "content: {}",
        result.content
    );

    assert!(
        result
            .content
            .contains("Found 5 total occurrences across 3 files."),
        "content: {}",
        result.content
    );
}

fn build_parallel_tree(root: &std::path::Path, files: usize) {
    for i in 0..files {
        let dir = root.join(format!("d{:02}", i % 12));
        std::fs::create_dir_all(&dir).unwrap();
        let body = if i % 3 == 0 {
            format!("filler\nneedle_{i} here\nfiller\n")
        } else {
            "filler line one\nno match here\nfiller\n".to_string()
        };
        std::fs::write(dir.join(format!("f{i:04}.txt")), body).unwrap();
    }
}

#[tokio::test]
async fn grep_parallel_walk_is_deterministic() {
    let dir = tempfile::tempdir().unwrap();
    build_parallel_tree(dir.path(), 120);
    let ts = NativeToolset::new(dir.path().to_str().unwrap(), None).unwrap();

    for mode in ["files_with_matches", "content", "count_matches"] {
        let args = json!({ "pattern": "needle_", "output_mode": mode });
        let first = ts.execute("Grep", &args).unwrap();
        let second = ts.execute("Grep", &args).unwrap();
        assert!(
            first.content == second.content,
            "mode {mode} drifted between parallel runs\nfirst:\n{}\nsecond:\n{}",
            first.content,
            second.content
        );
        assert!(
            !first.content.is_empty() && !first.content.contains("No matches found"),
            "mode {mode} found nothing:\n{}",
            first.content
        );
    }
}

#[tokio::test]
async fn grep_content_clusters_and_merging_are_byte_stable() {
    let dir = tempfile::tempdir().unwrap();

    std::fs::write(dir.path().join("merged.txt"), "a\nb\nmatch\nd\nmatch2\nf\n").unwrap();

    std::fs::write(dir.path().join("split.txt"), "match1\nx\ny\nz\nmatch2\n").unwrap();
    let ts = NativeToolset::new(dir.path().to_str().unwrap(), None).unwrap();
    let result = ts
        .execute(
            "Grep",
            &json!({ "pattern": "match", "output_mode": "content", "-C": 1 }),
        )
        .unwrap();
    assert!(!result.is_error, "content: {}", result.content);
    let lines: Vec<&str> = result.content.lines().collect();
    assert_eq!(
        lines,
        vec![
            "merged.txt-2-b",
            "merged.txt:3:match",
            "merged.txt-4-d",
            "merged.txt:5:match2",
            "merged.txt-6-f",
            "split.txt:1:match1",
            "split.txt-2-x",
            "--",
            "split.txt-4-z",
            "split.txt:5:match2",
        ],
        "content: {}",
        result.content
    );

    let again = ts
        .execute(
            "Grep",
            &json!({ "pattern": "match", "output_mode": "content", "-C": 1 }),
        )
        .unwrap();
    assert_eq!(again.content, result.content);
}

#[tokio::test]
async fn grep_head_limit_offset_are_stable_under_parallelism() {
    let dir = tempfile::tempdir().unwrap();
    build_parallel_tree(dir.path(), 60);
    let ts = NativeToolset::new(dir.path().to_str().unwrap(), None).unwrap();
    let args = json!({
        "pattern": "needle_",
        "output_mode": "content",
        "head_limit": 5,
        "offset": 0,
    });
    let first = ts.execute("Grep", &args).unwrap();
    let second = ts.execute("Grep", &args).unwrap();
    assert_eq!(first.content, second.content);
    assert!(
        first.content.contains("Results truncated to 5 lines"),
        "content: {}",
        first.content
    );

    let body = first.content.split("\n\n").next().unwrap();
    assert_eq!(body.lines().count(), 5, "content: {}", first.content);
}

#[test]
fn grep_expired_deadline_sets_the_atomic_timeout() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("needle.txt"), "needle\n").unwrap();
    let regex = regex::Regex::new("needle").unwrap();
    let cfg = GrepScanConfig {
        regex: &regex,
        mode: GrepMode::FilesWithMatches,
        context_before: 0,
        context_after: 0,
        line_numbers: true,
        multiline: false,
    };

    let collected = grep_collect(
        dir.path(),
        dir.path(),
        &cfg,
        None,
        None,
        false,
        GrepWalkLimits {
            deadline: std::time::Instant::now(),
        },
    );
    assert!(collected.timed_out);
    assert!(collected.per_file.is_empty());
}

#[test]
fn glob_matches_at_any_depth() {
    let (_dir, ts) = setup();
    let result = ts.execute("Glob", &json!({ "pattern": "*.rs" })).unwrap();
    assert!(
        result.content.contains("lib.rs"),
        "content: {}",
        result.content
    );
    assert!(!result.content.contains("a.txt"));
}

#[test]
fn glob_no_matches_reports_cleanly() {
    let (_dir, ts) = setup();
    let result = ts.execute("Glob", &json!({ "pattern": "*.xyz" })).unwrap();
    assert!(!result.is_error);
    assert!(result.content.contains("No files matched"));
}

#[test]
fn read_only_path_stays_on_execute() {
    let (_dir, ts) = setup();

    assert!(
        ts.execute("Write", &json!({ "path": "a.txt", "content": "x" }))
            .is_none()
    );
    assert!(ts.execute("Edit", &json!({ "path": "a.txt" })).is_none());
    assert!(
        ts.execute("Bash", &json!({ "command": "rm -rf /" }))
            .is_none()
    );
}

#[tokio::test]
async fn write_creates_file_inside_sandbox() {
    let (_dir, ts) = setup();
    let result = ts
        .execute_mutating(
            "Write",
            &json!({ "path": "out/new.txt", "content": "hello\n" }),
        )
        .await
        .expect("native write handled");
    assert!(!result.is_error, "content: {}", result.content);
    let written = std::fs::read_to_string(_dir.path().join("out/new.txt")).unwrap();
    assert_eq!(written, "hello\n");
}

#[tokio::test]
async fn write_outside_workspace_is_served_without_a_mode() {
    let (_dir, ts) = setup();
    let outside = tempfile::tempdir().unwrap();
    let escaped = outside.path().join("written.txt");
    let result = ts
        .execute_mutating(
            "Write",
            &json!({ "path": escaped.to_str().unwrap(), "content": "x" }),
        )
        .await
        .expect("with no SandboxMode policy writes are not confined");
    assert!(!result.is_error, "content: {}", result.content);
    assert!(escaped.exists());
}

#[tokio::test]
async fn edit_replaces_unique_match() {
    let (_dir, ts) = setup();
    let result = ts
        .execute_mutating(
            "Edit",
            &json!({ "path": "a.txt", "old_string": "beta", "new_string": "BETA" }),
        )
        .await
        .unwrap();
    assert!(!result.is_error);
    let content = std::fs::read_to_string(_dir.path().join("a.txt")).unwrap();
    assert_eq!(content, "alpha\nBETA\ngamma\n");
}

#[tokio::test]
async fn edit_ambiguous_match_is_an_error() {
    let (_dir, ts) = setup();
    std::fs::write(_dir.path().join("dup.txt"), "x\nx\n").unwrap();
    let result = ts
        .execute_mutating(
            "Edit",
            &json!({ "path": "dup.txt", "old_string": "x", "new_string": "y" }),
        )
        .await
        .unwrap();
    assert!(result.is_error, "content: {}", result.content);
    assert!(
        result.content.contains("not unique"),
        "content: {}",
        result.content
    );
    assert!(
        result.content.contains("replace_all=true"),
        "content: {}",
        result.content
    );

    assert_eq!(
        std::fs::read_to_string(_dir.path().join("dup.txt")).unwrap(),
        "x\nx\n"
    );
}

#[tokio::test]
async fn edit_success_reports_the_replaced_occurrence_count() {
    let (_dir, ts) = setup();
    std::fs::write(_dir.path().join("one.txt"), "alpha\n").unwrap();
    let result = ts
        .execute_mutating(
            "Edit",
            &json!({ "path": "one.txt", "old_string": "alpha", "new_string": "beta" }),
        )
        .await
        .unwrap();
    assert!(!result.is_error, "content: {}", result.content);
    assert!(
        result.content.contains("Replaced 1 occurrence in one.txt"),
        "content: {}",
        result.content
    );
}

#[tokio::test]
async fn edit_rejects_identical_old_and_new_strings() {
    let (_dir, ts) = setup();
    std::fs::write(_dir.path().join("same.txt"), "alpha\n").unwrap();
    let result = ts
        .execute_mutating(
            "Edit",
            &json!({ "path": "same.txt", "old_string": "alpha", "new_string": "alpha" }),
        )
        .await
        .unwrap();
    assert!(result.is_error, "content: {}", result.content);
    assert!(
        result
            .content
            .contains("No changes to make: old_string and new_string are exactly the same."),
        "content: {}",
        result.content
    );
    assert_eq!(
        std::fs::read_to_string(_dir.path().join("same.txt")).unwrap(),
        "alpha\n"
    );
}

#[tokio::test]
async fn edit_zero_matches_tells_the_model_to_reload() {
    let (_dir, ts) = setup();
    std::fs::write(_dir.path().join("stale.txt"), "alpha\n").unwrap();
    let result = ts
        .execute_mutating(
            "Edit",
            &json!({ "path": "stale.txt", "old_string": "missing", "new_string": "beta" }),
        )
        .await
        .unwrap();
    assert!(result.is_error, "content: {}", result.content);
    assert!(
        result.content.contains("old_string not found in stale.txt"),
        "content: {}",
        result.content
    );
    assert!(
        result.content.contains("reload the content"),
        "content: {}",
        result.content
    );
    assert!(
        !result.content.contains("not unique"),
        "zero matches must not be reported as ambiguous: {}",
        result.content
    );
}

#[tokio::test]
async fn edit_replace_all_replaces_every_occurrence() {
    let (_dir, ts) = setup();
    std::fs::write(_dir.path().join("dup.txt"), "x\nx\n").unwrap();
    let result = ts
            .execute_mutating(
                "Edit",
                &json!({ "path": "dup.txt", "old_string": "x", "new_string": "y", "replace_all": true }),
            )
            .await
            .unwrap();
    assert!(!result.is_error);
    assert_eq!(
        std::fs::read_to_string(_dir.path().join("dup.txt")).unwrap(),
        "y\ny\n"
    );
}

#[tokio::test]
async fn bash_runs_inside_sandbox_and_reports_exit_code() {
    let Some(shell) = find_bash() else { return };
    let (_dir, ts) = setup_with_shell(Some(&shell));
    let result = ts
        .execute_mutating("Bash", &json!({ "command": "echo native-bash" }))
        .await
        .unwrap();
    assert!(!result.is_error, "content: {}", result.content);
    assert!(
        result.content.contains("native-bash"),
        "content: {}",
        result.content
    );
}

#[tokio::test]
async fn bash_streams_output_chunks_to_the_progress_callback() {
    let Some(shell) = find_bash() else { return };
    let (_dir, ts) = setup_with_shell(Some(&shell));
    let chunks = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = chunks.clone();
    let emit = move |update: &crate::tools::ToolUpdate<'_>| {
        seen.lock()
            .unwrap()
            .push((update.kind.to_string(), update.text.to_string()));
    };
    let result = ts
        .execute_tool_streaming(
            Some("pc1"),
            "Bash",
            &json!({ "command": "echo hello-stream" }),
            Some(&emit),
        )
        .await
        .unwrap();
    assert!(!result.is_error, "content: {}", result.content);
    assert!(
        result.content.contains("hello-stream"),
        "full output intact"
    );
    let seen = chunks.lock().unwrap();
    assert!(
        seen.iter()
            .any(|(kind, text)| kind == "stdout" && text.contains("hello-stream")),
        "at least one stdout chunk carries the output: {seen:?}"
    );
}

#[test]
fn progress_throttle_admits_the_first_chunk_of_each_stream() {
    let throttle = ProgressThrottle::new();

    assert!(throttle.admit("stderr"), "first stderr chunk is admitted");
    assert!(throttle.admit("stdout"), "first stdout chunk is admitted");
    assert!(!throttle.admit("stderr"), "stderr is now inside its window");
    assert!(!throttle.admit("stdout"), "stdout is inside its own window");
}

#[test]
fn progress_throttle_reopens_each_stream_after_the_interval() {
    let throttle = ProgressThrottle::new();
    assert!(throttle.admit("stdout"));
    std::thread::sleep(Duration::from_millis(PROGRESS_MIN_INTERVAL_MS + 10));
    assert!(
        throttle.admit("stdout"),
        "window reopens after the interval"
    );
    assert!(throttle.admit("stderr"), "stderr keeps its own window");
}

#[tokio::test]
async fn bash_uses_bash_semantics_not_cmd() {
    let Some(shell) = find_bash() else { return };
    let (_dir, ts) = setup_with_shell(Some(&shell));

    let result = ts
        .execute_mutating("Bash", &json!({ "command": "echo $((20 + 3))" }))
        .await
        .unwrap();
    assert!(!result.is_error, "content: {}", result.content);
    assert!(result.content.contains("23"), "content: {}", result.content);
}

#[tokio::test]
async fn bash_reports_failure_exit_code() {
    let Some(shell) = find_bash() else { return };
    let (_dir, ts) = setup_with_shell(Some(&shell));

    let result = ts
        .execute_mutating("Bash", &json!({ "command": "exit 3" }))
        .await
        .unwrap();
    assert!(result.is_error);
    assert!(
        result.content.contains("exit code: 3"),
        "content: {}",
        result.content
    );
}

#[tokio::test]
async fn bash_result_messages_match_v2() {
    let Some(shell) = find_bash() else { return };
    let (_dir, ts) = setup_with_shell(Some(&shell));

    let ok_empty = ts
        .execute_mutating("Bash", &json!({ "command": "true" }))
        .await
        .unwrap();
    assert!(!ok_empty.is_error, "content: {}", ok_empty.content);
    assert_eq!(ok_empty.content, "Command executed successfully.");

    let ok_output = ts
        .execute_mutating("Bash", &json!({ "command": "echo hi" }))
        .await
        .unwrap();
    assert_eq!(ok_output.content.trim_end(), "hi");

    let failed_empty = ts
        .execute_mutating("Bash", &json!({ "command": "exit 3" }))
        .await
        .unwrap();
    assert!(failed_empty.is_error);
    assert!(
        failed_empty
            .content
            .starts_with("Process exited with code 3\nCommand failed with exit code: 3."),
        "content: {}",
        failed_empty.content
    );

    let failed_output = ts
        .execute_mutating("Bash", &json!({ "command": "echo oops; exit 4" }))
        .await
        .unwrap();
    assert_eq!(
        failed_output.content,
        "oops\nCommand failed with exit code: 4."
    );
}

#[tokio::test]
async fn powershell_runs_a_script_natively_and_keeps_utf8() {
    if find_powershell().is_none() {
        return;
    }
    let (_dir, ts) = setup();
    let result = ts
        .execute_mutating(
            "PowerShell",
            &json!({ "command": "Write-Output 'héllo 世界'" }),
        )
        .await
        .expect("the native arm must handle PowerShell, not hand it to the host");
    assert!(!result.is_error, "content: {}", result.content);
    assert!(
        result.content.contains("héllo 世界"),
        "UTF-8 must survive the -EncodedCommand round trip: {}",
        result.content
    );
}

#[tokio::test]
async fn powershell_reports_failure_exit_code() {
    if find_powershell().is_none() {
        return;
    }
    let (_dir, ts) = setup();
    let result = ts
        .execute_mutating("PowerShell", &json!({ "command": "exit 7" }))
        .await
        .expect("the native arm must handle PowerShell");
    assert!(result.is_error);
    assert!(
        result.content.contains("Command failed with exit code: 7."),
        "content: {}",
        result.content
    );
}

#[tokio::test]
async fn powershell_refuses_an_empty_command() {
    let (_dir, ts) = setup();
    let result = ts
        .execute_mutating("PowerShell", &json!({ "command": "" }))
        .await
        .expect("an empty command is refused natively");
    assert!(result.is_error);
    assert_eq!(result.content, "Command cannot be empty.");
}

#[tokio::test]
async fn powershell_run_in_background_falls_back_to_host() {
    let (_dir, ts) = setup();
    assert!(
        ts.execute_mutating(
            "PowerShell",
            &json!({
                "command": "Write-Output hi",
                "run_in_background": true,
                "description": "emit hi"
            }),
        )
        .await
        .is_none(),
        "without a task runner the background call stays host-owned (same as Bash)"
    );
}

#[tokio::test]
async fn powershell_timeout_kills_and_reports_without_falling_back() {
    if find_powershell().is_none() {
        return;
    }
    let (_dir, ts) = setup();
    let result = ts
        .execute_mutating(
            "PowerShell",
            &json!({ "command": "Start-Sleep -Seconds 5", "timeout": 1 }),
        )
        .await
        .expect("timeout must be reported, not handed to the host");
    assert!(result.is_error);
    assert!(
        result.content.contains("killed by timeout"),
        "content: {}",
        result.content
    );
}

#[tokio::test]
async fn bash_run_in_background_falls_back_to_host() {
    let (_dir, ts) = setup();
    assert!(
        ts.execute_mutating(
            "Bash",
            &json!({ "command": "echo hi", "run_in_background": true }),
        )
        .await
        .is_none(),
        "background tasks are host-owned"
    );
}

#[tokio::test]
async fn bash_timeout_kills_and_reports_without_falling_back() {
    let Some(shell) = find_bash() else { return };
    let (_dir, ts) = setup_with_shell(Some(&shell));

    let command = "sleep 5";
    let result = ts
        .execute_mutating("Bash", &json!({ "command": command, "timeout": 1 }))
        .await
        .expect("timeout must be reported, not handed to the host");
    assert!(result.is_error);
    assert!(
        result.content.contains("killed by timeout"),
        "content: {}",
        result.content
    );
}

#[tokio::test]
async fn bash_cwd_outside_workspace_runs_without_a_mode() {
    let (_dir, ts) = setup();
    let outside = tempfile::tempdir().unwrap();
    let result = ts
        .execute_mutating(
            "Bash",
            &json!({ "command": "echo hi", "cwd": outside.path().to_str().unwrap() }),
        )
        .await
        .expect("with no SandboxMode policy the cwd is not confined");
    assert!(!result.is_error, "content: {}", result.content);
    assert!(result.content.contains("hi"), "content: {}", result.content);
}

#[test]
fn bench_large_inputs() {
    use std::time::Instant;
    let dir = tempfile::tempdir().unwrap();
    let line = "the quick brown fox jumps over the lazy dog 0123456789\n";
    let big = dir.path().join("huge.txt");
    let mut content = String::with_capacity(line.len() * 500_000);
    for i in 0..500_000 {
        content.push_str(&format!("line {i}: {line}"));
    }
    std::fs::write(&big, &content).unwrap();
    println!("[large] file = {:.1} MB", content.len() as f64 / 1048576.0);
    let ts = NativeToolset::new(&dir.path().to_string_lossy(), None).unwrap();

    let show = |label: &str, r: Option<ExecutableToolResult>, t: std::time::Instant| match r {
        Some(res) => println!(
            "[large] {:28} {:6.1}ms  is_error={} len={}",
            label,
            t.elapsed().as_secs_f64() * 1000.0,
            res.is_error,
            res.content.len()
        ),
        None => println!("[large] {:28}  declined (None)", label),
    };

    let full = json!({ "path": big.to_string_lossy() });
    let t = Instant::now();
    let r = ts.execute("Read", &full);
    show("Read full 32MB", r, t);

    let deep = json!({ "path": big.to_string_lossy(), "line_offset": 400_000 });
    let t = Instant::now();
    let r = ts.execute("Read", &deep);
    show("Read offset@400k", r, t);

    for i in 0..300 {
        let p = dir.path().join(format!("g{i}.txt"));
        let body: String = (0..300)
            .map(|j| {
                if j == 5 {
                    format!("needle {i}\n")
                } else {
                    format!("fill {j}\n")
                }
            })
            .collect();
        std::fs::write(&p, body).unwrap();
    }
    for i in 0..2000 {
        let p = dir.path().join(format!("w{i}.txt"));
        std::fs::write(&p, "x\n").unwrap();
    }
    let grep_arg = json!({ "pattern": "needle", "path": dir.path().to_string_lossy() });
    ts.execute("Grep", &grep_arg).unwrap();
    let t = Instant::now();
    let r = ts.execute("Grep", &grep_arg).unwrap();
    println!(
        "[large] Grep 300files/90k行 {:.1}ms (hits={})",
        t.elapsed().as_secs_f64() * 1000.0,
        r.content.matches("needle").count()
    );

    let glob_arg = json!({ "pattern": "w*.txt", "path": dir.path().to_string_lossy() });
    ts.execute("Glob", &glob_arg).unwrap();
    let t = Instant::now();
    ts.execute("Glob", &glob_arg).unwrap();
    println!(
        "[large] Glob 2000files      {:.1}ms",
        t.elapsed().as_secs_f64() * 1000.0
    );
}

#[test]
fn bench_basic_tools_latency() {
    use std::time::Instant;
    let dir = tempfile::tempdir().unwrap();
    let mut file = dir.path().join("big.txt");
    let content: String = (0..1200)
        .map(|i| format!("line {i}: the quick brown fox jumps\n"))
        .collect::<Vec<_>>()
        .join("");
    std::fs::write(&file, &content).unwrap();
    for i in 0..5 {
        let p = dir.path().join(format!("m{i}.txt"));
        let _ = std::fs::write(&p, format!("needle {i}\nsecond line\n"));
    }
    let ts = NativeToolset::new(&dir.path().to_string_lossy(), None).unwrap();
    let read_arg = json!({ "path": file.to_string_lossy() });
    let grep_arg = json!({ "pattern": "needle", "path": dir.path().to_string_lossy() });
    let glob_arg = json!({ "pattern": "*.txt", "path": dir.path().to_string_lossy() });

    let _ = ts.execute("Read", &read_arg).unwrap();
    let _ = ts.execute("Grep", &grep_arg).unwrap();
    let _ = ts.execute("Glob", &glob_arg).unwrap();

    let t = Instant::now();
    for _ in 0..10 {
        ts.execute("Read", &read_arg).unwrap();
    }
    println!(
        "[tool-bench] Read        avg {:.2}ms",
        t.elapsed().as_secs_f64() * 1000.0 / 10.0
    );

    let t = Instant::now();
    for _ in 0..10 {
        ts.execute("Grep", &grep_arg).unwrap();
    }
    println!(
        "[tool-bench] Grep        avg {:.2}ms",
        t.elapsed().as_secs_f64() * 1000.0 / 10.0
    );

    let t = Instant::now();
    for _ in 0..10 {
        ts.execute("Glob", &glob_arg).unwrap();
    }
    println!(
        "[tool-bench] Glob        avg {:.2}ms",
        t.elapsed().as_secs_f64() * 1000.0 / 10.0
    );

    if let Some(bash) = find_bash() {
        let (_, _ts) = setup_with_shell(Some(&bash));
    }
    let _ = (&mut file,);
}

struct MemoryToolEnv {
    _lock: tokio::sync::MutexGuard<'static, ()>,
    previous_home: Option<std::ffi::OsString>,
    previous_flag: Option<std::ffi::OsString>,
}

impl MemoryToolEnv {
    async fn arm(home: &std::path::Path) -> Self {
        let lock = crate::env::ENV_TEST_LOCK.lock().await;
        let previous_home = std::env::var_os("KIMI_CODE_HOME");
        let previous_flag =
            std::env::var_os(crate::tools::memory_action::ASSOCIATIVE_MEMORY_FLAG_ENV);
        unsafe { std::env::set_var("KIMI_CODE_HOME", home) };
        unsafe {
            std::env::set_var(
                crate::tools::memory_action::ASSOCIATIVE_MEMORY_FLAG_ENV,
                "0",
            )
        };
        Self {
            _lock: lock,
            previous_home,
            previous_flag,
        }
    }
}

impl Drop for MemoryToolEnv {
    fn drop(&mut self) {
        match self.previous_home.take() {
            Some(value) => unsafe { std::env::set_var("KIMI_CODE_HOME", value) },
            None => unsafe { std::env::remove_var("KIMI_CODE_HOME") },
        }
        match self.previous_flag.take() {
            Some(value) => unsafe {
                std::env::set_var(
                    crate::tools::memory_action::ASSOCIATIVE_MEMORY_FLAG_ENV,
                    value,
                )
            },
            None => unsafe {
                std::env::remove_var(crate::tools::memory_action::ASSOCIATIVE_MEMORY_FLAG_ENV)
            },
        }
    }
}

#[tokio::test]
async fn memory_tool_runs_read_write_list_and_delete() {
    let home = tempfile::tempdir().unwrap();
    let _env = MemoryToolEnv::arm(home.path()).await;
    let (_dir, ts) = setup();
    let name = crate::tools::core_tool_defs::MEMORY_TOOL_NAME;

    let write = ts
        .execute_tool(
            name,
            &json!({
                "action": "write",
                "scope": "global",
                "path": "note",
                "content": "# Note\n\nbody"
            }),
        )
        .await
        .unwrap_or_else(|| panic!("{name} is not handled by the toolset"));
    assert!(!write.is_error, "{}", write.content);
    assert!(home.path().join("memory/global/note.md").is_file());

    let read = ts
        .execute_tool(name, &json!({ "action": "read", "path": "global/note" }))
        .await
        .expect("Memory is handled");
    assert!(!read.is_error, "{}", read.content);
    assert!(read.content.contains("body"), "{}", read.content);

    let list = ts
        .execute_tool(name, &json!({ "action": "list", "scope": "global" }))
        .await
        .expect("Memory is handled");
    assert!(!list.is_error, "{}", list.content);
    assert!(list.content.contains("global/note.md"), "{}", list.content);

    let delete = ts
        .execute_tool(name, &json!({ "action": "delete", "path": "global/note" }))
        .await
        .expect("Memory is handled");
    assert!(!delete.is_error, "{}", delete.content);
    assert!(!home.path().join("memory/global/note.md").exists());
}

#[tokio::test]
async fn memory_tool_is_refused_for_subagents() {
    let (_dir, ts) = setup();
    let ts = ts.with_caller_agent_id("researcher");
    let name = crate::tools::core_tool_defs::MEMORY_TOOL_NAME;
    let result = ts
        .execute_tool(name, &json!({ "action": "list" }))
        .await
        .unwrap_or_else(|| panic!("{name} is not handled by the toolset"));
    assert!(result.is_error, "{}", result.content);
    assert!(result.content.contains("main agent"), "{}", result.content);
}

#[test]
fn native_tool_names_match_the_contract_file() {
    let contract: serde_json::Value =
        serde_json::from_str(include_str!("../../tool-name-contract.json"))
            .expect("tool-name-contract.json must parse");
    let v2_native: Vec<String> = serde_json::from_value(contract["v2Native"].clone()).unwrap();
    let v2_host: Vec<String> = serde_json::from_value(contract["v2Host"].clone()).unwrap();
    let repl_only: Vec<String> =
        serde_json::from_value(contract["replOnlyNative"].clone()).unwrap();
    let v2_github: Vec<String> = serde_json::from_value(contract["v2Github"].clone()).unwrap();
    let unloaded: Vec<String> = serde_json::from_value(contract["unloadedInV2"].clone()).unwrap();
    let aliases: std::collections::BTreeMap<String, String> =
        serde_json::from_value(contract["aliases"].clone()).unwrap();

    let (_dir, toolset) = setup();
    for name in &v2_native {
        assert!(
            toolset.handles(name),
            "contract says {name} executes natively but handles() rejects it"
        );
    }
    for name in &v2_host {
        assert!(
            !toolset.handles(name),
            "contract says {name} is host-owned but handles() accepts it"
        );
    }
    for name in &repl_only {
        assert!(
            toolset.handles(name),
            "contract says {name} is a REPL-only native tool but handles() rejects it"
        );
    }
    for name in &v2_github {
        assert!(
            github::is_github_tool(name),
            "contract says {name} is a native GitHub tool but is_github_tool rejects it — add it to github.rs SPECS or fix the contract"
        );
    }
    for name in &unloaded {
        assert!(
            !toolset.handles(name),
            "contract says {name} is unloaded from v2 but handles() accepts it — promote it to v2Native or drop the native arm"
        );
    }

    let mut contracted = std::collections::BTreeSet::new();
    for name in v2_native.iter().chain(repl_only.iter()) {
        contracted.insert(name.to_ascii_lowercase());
    }
    contracted.extend(aliases.into_keys());
    let accepted: std::collections::BTreeSet<String> =
        NATIVE_TOOL_NAMES.iter().map(|s| (*s).to_string()).collect();
    assert_eq!(
        accepted, contracted,
        "NATIVE_TOOL_NAMES and tool-name-contract.json disagree — update both together"
    );
}

fn scan(display: &str, mtime: u64, total_matches: usize) -> FileScan {
    FileScan {
        display: display.to_string(),
        mtime,
        total_matches,
        rendered: Vec::new(),
        lossy_utf8: false,
    }
}

fn displays(per_file: &[FileScan]) -> Vec<&str> {
    per_file.iter().map(|f| f.display.as_str()).collect()
}

#[test]
fn grep_sort_orders_files_with_matches_by_mtime_then_path() {
    let mut per_file = vec![
        scan("zeta.txt", 10, 1),
        scan("alpha.txt", 30, 1),
        scan("mid.txt", 20, 1),
        scan("beta.txt", 30, 1),
    ];
    grep_sort(&mut per_file, GrepMode::FilesWithMatches);

    assert_eq!(
        displays(&per_file),
        vec!["alpha.txt", "beta.txt", "mid.txt", "zeta.txt"]
    );
}

#[test]
fn grep_sort_uses_path_order_for_content_and_count_modes() {
    let mut per_file = vec![scan("b.txt", 9, 1), scan("a.txt", 1, 2)];
    grep_sort(&mut per_file, GrepMode::FilesWithMatches);
    assert_eq!(displays(&per_file), vec!["b.txt", "a.txt"], "mtime DESC");

    let mut per_file = vec![scan("b.txt", 9, 1), scan("a.txt", 1, 2)];
    grep_sort(&mut per_file, GrepMode::CountMatches);
    assert_eq!(displays(&per_file), vec!["a.txt", "b.txt"], "path ASC");
    assert_eq!(
        per_file[1].total_matches, 1,
        "entries must not be reordered"
    );
}

#[test]
fn grep_same_second_files_order_by_path_in_every_run() {
    let dir = tempfile::tempdir().unwrap();
    let names = ["delta.txt", "alpha.txt", "charlie.txt", "bravo.txt"];
    for name in names {
        std::fs::write(dir.path().join(name), "needle\n").unwrap();
    }
    let mtimes: Vec<u64> = names
        .iter()
        .map(|name| mtime_secs(&std::fs::metadata(dir.path().join(name)).unwrap()))
        .collect();
    if mtimes.windows(2).any(|pair| pair[0] != pair[1]) {
        eprintln!("skipping: fixture files straddled a second boundary ({mtimes:?})");
        return;
    }
    let ts = NativeToolset::new(dir.path().to_str().unwrap(), None).unwrap();
    let args = json!({ "pattern": "needle" });
    let first = ts.execute("Grep", &args).unwrap();
    let second = ts.execute("Grep", &args).unwrap();
    assert_eq!(first.content, second.content, "parallel runs drifted");
    assert_eq!(
        first.content.lines().collect::<Vec<_>>(),
        vec!["alpha.txt", "bravo.txt", "charlie.txt", "delta.txt"]
    );
}

#[test]
fn mtime_secs_reports_whole_seconds_since_the_epoch() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("m.txt"), "x\n").unwrap();
    let meta = std::fs::metadata(dir.path().join("m.txt")).unwrap();
    let elapsed = meta
        .modified()
        .unwrap()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap();

    assert_eq!(mtime_secs(&meta), elapsed.as_secs());
    assert_eq!(mtime_secs(&meta), elapsed.as_millis() as u64 / 1000);
}

#[test]
fn grep_walk_worker_count_is_explicitly_bounded() {
    let threads = grep_walk_threads();
    assert!(threads >= 1, "`ignore` treats 0 workers as serial");
    assert!(
        threads <= GREP_WALK_THREADS,
        "walk workers {threads} exceed the cap {GREP_WALK_THREADS}"
    );
}

fn panicking_tool(
    _sandbox: &Sandbox,
    _bridge: &ShellPathBridge,
    _args: &Value,
) -> Option<ExecutableToolResult> {
    panic!("synthetic blocking-pool failure");
}

#[tokio::test]
async fn readonly_tool_join_error_falls_back_to_the_host() {
    let (_dir, ts) = setup();

    assert!(
        ts.run_readonly_file_tool_on_blocking_pool(&json!({ "path": "a.txt" }), panicking_tool)
            .await
            .is_none(),
        "a read-only JoinError must fall back to the host"
    );
}

#[tokio::test]
async fn mutating_tool_join_error_becomes_an_error_result() {
    let (_dir, ts) = setup();

    let result = ts
        .run_mutating_file_tool_on_blocking_pool(&json!({ "path": "a.txt" }), panicking_tool)
        .await
        .expect("a mutating JoinError must not fall back to the host");
    assert!(result.is_error);
    assert!(
        result.content.contains("native tool task failed"),
        "content: {}",
        result.content
    );
}

#[test]
fn blocking_pool_failure_splits_on_mutability() {
    assert!(blocking_pool_failure(false, "boom".to_string()).is_none());
    let result = blocking_pool_failure(true, "boom".to_string()).unwrap();
    assert!(result.is_error);
    assert!(
        result.content.contains("boom"),
        "content: {}",
        result.content
    );
}

#[test]
fn grep_rejects_mistyped_arguments_instead_of_ignoring_them() {
    let (_dir, ts) = setup();

    let malformed = [
        json!({ "pattern": "beta", "multiline": "true" }),
        json!({ "pattern": "beta", "include_ignored": 1 }),
        json!({ "pattern": "beta", "-i": "yes" }),
        json!({ "pattern": "beta", "-n": 0 }),
        json!({ "pattern": "beta", "-C": "2" }),
        json!({ "pattern": "beta", "-A": -1 }),
        json!({ "pattern": "beta", "-B": 1.5 }),
        json!({ "pattern": "beta", "head_limit": "10" }),
        json!({ "pattern": "beta", "offset": true }),
        json!({ "pattern": "beta", "output_mode": "matches" }),
        json!({ "pattern": "beta", "output_mode": 3 }),
        json!({ "pattern": "beta", "glob": 7 }),
        json!({ "pattern": "beta", "path": ["a.txt"] }),
        json!({ "pattern": "beta", "type": true }),
        json!({ "pattern": 7 }),
        json!({}),
    ];
    for args in malformed {
        let result = match ts.execute("Grep", &args) {
            Some(result) => result,
            None => panic!("malformed args must be reported, not forwarded: {args}"),
        };
        assert!(result.is_error, "malformed args must be an error: {args}");
    }
}

#[test]
fn grep_treats_absent_and_null_arguments_as_schema_defaults() {
    let (_dir, ts) = setup();
    let baseline = ts.execute("Grep", &json!({ "pattern": "beta" })).unwrap();
    let defaults = [
        json!({ "pattern": "beta", "multiline": null }),
        json!({ "pattern": "beta", "include_ignored": null }),
        json!({ "pattern": "beta", "type": null, "glob": null, "path": null }),
        json!({ "pattern": "beta", "output_mode": null }),
        json!({ "pattern": "beta", "head_limit": null, "offset": null }),
        json!({ "pattern": "beta", "-i": null, "-n": null, "-A": null, "-B": null, "-C": null }),
    ];
    for args in defaults {
        let result = match ts.execute("Grep", &args) {
            Some(result) => result,
            None => panic!("absent/null must mean the schema default: {args}"),
        };
        assert_eq!(result.content, baseline.content, "args: {args}");
    }
}

struct ScriptedReader {
    lines: usize,
    fail_after: bool,
}

impl std::io::Read for ScriptedReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.lines == 0 {
            return if self.fail_after {
                Err(std::io::Error::other("synthetic read failure"))
            } else {
                Ok(0)
            };
        }
        self.lines -= 1;
        let payload = b"needle line\n";
        let n = payload.len().min(buf.len());
        buf[..n].copy_from_slice(&payload[..n]);
        Ok(n)
    }
}

#[test]
fn grep_read_error_skips_the_whole_file() {
    let regex = regex::Regex::new("needle").unwrap();
    let cfg = GrepScanConfig {
        regex: &regex,
        mode: GrepMode::Content,
        context_before: 0,
        context_after: 0,
        line_numbers: true,
        multiline: false,
    };

    let reader = std::io::BufReader::new(ScriptedReader {
        lines: 1,
        fail_after: true,
    });
    assert!(
        matches!(
            scan_grep_lines(reader, &cfg, "broken.txt".to_string(), 7),
            ScanOutcome::Unreadable
        ),
        "an I/O error must drop the file and be reported as unreadable, not \
         silently treated as a file with no matches"
    );
}

#[test]
fn grep_clean_eof_keeps_the_scanned_matches() {
    let regex = regex::Regex::new("needle").unwrap();
    let cfg = GrepScanConfig {
        regex: &regex,
        mode: GrepMode::Content,
        context_before: 0,
        context_after: 0,
        line_numbers: true,
        multiline: false,
    };

    let reader = std::io::BufReader::new(ScriptedReader {
        lines: 3,
        fail_after: false,
    });
    match scan_grep_lines(reader, &cfg, "ok.txt".to_string(), 1234) {
        ScanOutcome::Match(scan) => {
            assert_eq!(scan.display, "ok.txt");
            assert_eq!(scan.mtime, 1234);
            assert_eq!(scan.total_matches, 3);
            assert_eq!(
                scan.rendered,
                vec![
                    "ok.txt:1:needle line",
                    "ok.txt:2:needle line",
                    "ok.txt:3:needle line"
                ]
            );
        }
        ScanOutcome::Skip => panic!("a fully served file must not be skipped"),
        ScanOutcome::Unreadable => panic!("a fully served file must not be unreadable"),
    }
}

fn multiline_regex(pattern: &str) -> regex::Regex {
    regex::RegexBuilder::new(pattern)
        .multi_line(true)
        .dot_matches_new_line(true)
        .build()
        .unwrap()
}

fn scan_multiline(
    dir: &Path,
    name: &str,
    body: &str,
    pattern: &str,
    mode: GrepMode,
) -> ScanOutcome {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    let regex = multiline_regex(pattern);
    let cfg = GrepScanConfig {
        regex: &regex,
        mode,
        context_before: 0,
        context_after: 0,
        line_numbers: true,
        multiline: true,
    };
    scan_grep_file_multiline(&path, dir, &cfg)
}

fn matched_scan(outcome: ScanOutcome, what: &str) -> FileScan {
    match outcome {
        ScanOutcome::Match(scan) => scan,
        ScanOutcome::Skip => panic!("{what}: expected a match, the file was skipped"),
        ScanOutcome::Unreadable => panic!("{what}: expected a match, the file was unreadable"),
    }
}

#[test]
fn multiline_eof_zero_width_match_is_not_counted_or_marked() {
    let dir = tempfile::tempdir().unwrap();

    let scan = matched_scan(
        scan_multiline(dir.path(), "withnl.txt", "a\nb\n", "$", GrepMode::Content),
        "$ over \"a\\nb\\n\"",
    );
    assert_eq!(scan.total_matches, 2, "rg --count-matches reports 2");
    assert_eq!(scan.rendered, vec!["withnl.txt:1:a", "withnl.txt:2:b"]);

    let scan = matched_scan(
        scan_multiline(
            dir.path(),
            "withnl2.txt",
            "a\nb\n",
            "x*",
            GrepMode::CountMatches,
        ),
        "x* over \"a\\nb\\n\"",
    );
    assert_eq!(scan.total_matches, 4, "rg --count-matches reports 4");

    assert!(
        matches!(
            scan_multiline(
                dir.path(),
                "withnl3.txt",
                "a\nb\n",
                r"\z",
                GrepMode::CountMatches
            ),
            ScanOutcome::Skip
        ),
        "rg finds nothing for `\\z` in a newline-terminated file"
    );

    assert!(
        matches!(
            scan_multiline(
                dir.path(),
                "empty.txt",
                "",
                "x*",
                GrepMode::FilesWithMatches
            ),
            ScanOutcome::Skip
        ),
        "rg exits 1 on an empty file"
    );
}

#[test]
fn multiline_zero_width_match_on_a_newline_spans_into_the_next_line() {
    let dir = tempfile::tempdir().unwrap();

    let scan = matched_scan(
        scan_multiline(dir.path(), "nonl.txt", "a\nb", "$", GrepMode::Content),
        "$ over \"a\\nb\"",
    );
    assert_eq!(scan.total_matches, 1, "rg --count-matches reports 1");
    assert_eq!(scan.rendered, vec!["nonl.txt:1:a", "nonl.txt:2:b"]);

    let scan = matched_scan(
        scan_multiline(dir.path(), "nonl2.txt", "a\nb", r"\b", GrepMode::Content),
        "\\b over a\\nb",
    );
    assert_eq!(scan.total_matches, 3, "rg --count-matches reports 3");
    assert_eq!(scan.rendered, vec!["nonl2.txt:1:a", "nonl2.txt:2:b"]);
}

#[test]
fn multiline_sole_eof_match_in_a_dangling_last_line_survives() {
    let dir = tempfile::tempdir().unwrap();

    let scan = matched_scan(
        scan_multiline(dir.path(), "xy.txt", "xy", r"\z", GrepMode::Content),
        "\\z over xy",
    );
    assert_eq!(scan.total_matches, 1, "rg --count-matches reports 1");
    assert_eq!(scan.rendered, vec!["xy.txt:1:xy"]);

    let scan = matched_scan(
        scan_multiline(
            dir.path(),
            "xy2.txt",
            "xy",
            r"\b|\z",
            GrepMode::CountMatches,
        ),
        "\\b|\\z over xy",
    );
    assert_eq!(scan.total_matches, 1, "rg --count-matches reports 1");
}

#[test]
fn multiline_anchors_stay_per_line_like_ripgrep() {
    let dir = tempfile::tempdir().unwrap();

    let scan = matched_scan(
        scan_multiline(
            dir.path(),
            "abc.txt",
            "a\nb\nc",
            "^",
            GrepMode::CountMatches,
        ),
        "^ over \"a\\nb\\nc\"",
    );
    assert_eq!(scan.total_matches, 3, "rg --count-matches reports 3");
    let scan = matched_scan(
        scan_multiline(
            dir.path(),
            "abcd.txt",
            "ab\ncd\n",
            "^",
            GrepMode::CountMatches,
        ),
        "^ over \"ab\\ncd\\n\"",
    );
    assert_eq!(scan.total_matches, 2, "rg --count-matches reports 2");
}

#[test]
fn test_glob_orders_results_by_mtime_desc() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let now = std::time::SystemTime::now();
    for (name, age_secs) in [("a-old.txt", 20u64), ("b-mid.txt", 10), ("c-new.txt", 0)] {
        let path = root.join(name);
        std::fs::write(&path, "x").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(now - std::time::Duration::from_secs(age_secs))
            .unwrap();
    }

    let sandbox = Sandbox::new(std::fs::canonicalize(root).unwrap());
    let res = NativeToolset::glob(&sandbox, &bridge(), &json!({ "pattern": "*.txt" })).unwrap();

    let found: Vec<&str> = res
        .content
        .lines()
        .map(str::trim)
        .filter(|line| line.ends_with(".txt"))
        .collect();
    assert_eq!(found.len(), 3, "all three files should match: {found:?}");
    assert_eq!(
        found,
        vec!["c-new.txt", "b-mid.txt", "a-old.txt"],
        "newest first, not alphabetical: {found:?}"
    );
}

#[test]
fn test_glob_finds_hidden_and_filters_vcs_and_sensitive() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let github_dir = root.join(".github").join("workflows");
    std::fs::create_dir_all(&github_dir).unwrap();
    std::fs::write(github_dir.join("ci.yml"), "name: CI").unwrap();

    let git_dir = root.join(".git").join("hooks");
    std::fs::create_dir_all(&git_dir).unwrap();
    std::fs::write(git_dir.join("ci.yml"), "hook").unwrap();

    std::fs::write(root.join(".env"), "SECRET=123").unwrap();

    std::fs::write(root.join("hello.txt"), "world").unwrap();

    let sandbox = Sandbox::new(std::fs::canonicalize(root).unwrap());

    let res = NativeToolset::glob(&sandbox, &bridge(), &json!({ "pattern": "**/ci.yml" })).unwrap();
    assert!(res.content.contains("ci.yml"));
    assert!(res.content.contains(".github"));
    assert!(!res.content.contains("hooks"));
    for line in res.content.lines() {
        assert!(!line.contains(".git/") && !line.contains(".git\\"));
    }

    let res = NativeToolset::glob(&sandbox, &bridge(), &json!({ "pattern": "**/.env" })).unwrap();
    assert!(
        res.content
            .contains("No non-sensitive matches found (1 sensitive file(s) filtered)")
    );

    let res = NativeToolset::glob(&sandbox, &bridge(), &json!({ "pattern": "**/*" })).unwrap();
    assert!(res.content.contains("hello.txt"));
    assert!(res.content.contains("Filtered 1 sensitive file(s)."));
}

#[test]
fn test_glob_paging_report_is_localized_but_its_instructions_are_not() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for name in ["a.txt", "b.txt", "c.txt"] {
        std::fs::write(root.join(name), "x").unwrap();
    }

    let sandbox = Sandbox::new(std::fs::canonicalize(root).unwrap());
    let res = NativeToolset::glob(
        &sandbox,
        &bridge(),
        &json!({ "pattern": "*.txt", "head_limit": 2 }),
    )
    .unwrap();

    assert!(
        res.content.contains("Showing matches 1\u{2013}2 of 3."),
        "content: {}",
        res.content
    );
    assert!(
        res.content
            .contains("Continue with the same search arguments and offset=2."),
        "the continuation instruction is protocol and stays English: {}",
        res.content
    );
    assert!(
        res.content
            .contains("To remove the match-count limit, omit offset and use head_limit=0."),
        "content: {}",
        res.content
    );

    let rows = res
        .content
        .lines()
        .filter(|line| line.trim().ends_with(".txt"))
        .count();
    assert_eq!(rows, 2, "content: {}", res.content);
}

#[test]
fn reads_are_not_path_gated() {
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let outside_file = outside.path().join("note.txt");
    std::fs::write(&outside_file, "hello from outside").unwrap();
    let arg = outside_file.to_string_lossy().into_owned();

    let ts = NativeToolset::new(&workspace.path().to_string_lossy(), None).unwrap();
    let result = ts
        .execute("read", &json!({ "path": arg }))
        .expect("reads must not be declined for lying outside the workspace");
    assert!(!result.is_error, "content: {}", result.content);
    assert!(
        result.content.contains("hello from outside"),
        "content: {}",
        result.content
    );
}

#[test]
fn previous_session_reminder_reaches_the_injection_registry_once() {
    let dir = tempfile::tempdir().unwrap();
    let store = crate::storage::StateStore::for_dir(dir.path().join("state")).unwrap();
    store
            .write_domain(
                "task",
                &json!([{ "taskId": "task-9", "description": "build the app", "kind": "bash", "status": "running", "startedAt": 1u64 }]),
            )
            .unwrap();
    let runner = std::sync::Arc::new(crate::storage::TaskRunner::new(Some(store)));
    let (_dir2, ts) = setup();
    let ts = std::sync::Arc::new(ts.with_task_runner(runner));

    let mut registry = crate::injection::InjectionRegistry::new();
    let reminder_ts = ts.clone();
    let announced = HashSet::new();
    registry.register(
        "task_resume_termination",
        Box::new(move |_ctx| reminder_ts.take_previous_session_reminder(&announced)),
    );

    let texts = registry.build_injections(true);
    assert_eq!(texts.len(), 1, "the loss is announced once: {texts:?}");
    assert!(
        texts[0].contains("lost contact"),
        "injection carries the reminder: {}",
        texts[0]
    );
    assert!(
        texts[0].contains("- task-9 \"build the app\" (bash)"),
        "injection lists the task with its tool: {}",
        texts[0]
    );

    assert!(
        registry.build_injections(true).is_empty(),
        "the reminder is one-shot"
    );
}

#[tokio::test]
async fn test_native_bash_background_task_runner() {
    let Some(bash) = find_bash() else {
        return;
    };
    let (_dir, ts) = setup_with_shell(Some(&bash));
    let runner = std::sync::Arc::new(crate::storage::TaskRunner::new(None));
    let ts = ts.with_task_runner(runner.clone());

    let res = ts
            .bash_with(
                &json!({ "command": "echo 'bg hello'", "run_in_background": true, "description": "echo task" }),
                None,
            )
            .await
            .unwrap();

    assert!(!res.is_error);
    assert!(res.content.contains("Background task started"));
    assert!(res.content.contains("task_id:"));

    let line = res
        .content
        .lines()
        .find(|l| l.contains("task_id:"))
        .unwrap();
    let task_id = line
        .split("task_id: ")
        .nth(1)
        .unwrap()
        .trim()
        .trim_end_matches(['.', ')']);

    let wait_res = runner.wait(task_id, 60_000).await;
    let crate::storage::TaskWaitResult::Completed(entry) = wait_res else {
        panic!("the background task must settle as completed: {wait_res:?}");
    };

    let output = entry["output"].as_str().unwrap_or_default();
    assert!(
        output.contains("bg hello"),
        "the background task's stdout must survive the drain: {entry:?}"
    );
}

#[tokio::test]
async fn test_native_powershell_background_task_runner() {
    if find_powershell().is_none() {
        return;
    }
    let (_dir, ts) = setup();
    let runner = std::sync::Arc::new(crate::storage::TaskRunner::new(None));
    let ts = ts.with_task_runner(runner.clone());

    let res = ts
        .powershell_with(
            &json!({
                "command": "Write-Output 'bg hello'",
                "run_in_background": true,
                "description": "emit bg hello"
            }),
            None,
        )
        .await
        .unwrap();
    assert!(!res.is_error, "{}", res.content);
    assert!(
        res.content.contains("Background task started"),
        "{}",
        res.content
    );
    assert!(res.content.contains("description: emit bg hello"));

    let line = res
        .content
        .lines()
        .find(|line| line.contains("task_id:"))
        .unwrap();
    let task_id = line
        .split("task_id: ")
        .nth(1)
        .unwrap()
        .trim()
        .trim_end_matches(['.', ')']);

    let wait_res = runner.wait(task_id, 60_000).await;
    let crate::storage::TaskWaitResult::Completed(entry) = wait_res else {
        panic!("the background script must settle as completed: {wait_res:?}");
    };
    let output = entry["output"].as_str().unwrap_or_default();
    assert!(
        output.contains("bg hello"),
        "the background task's stdout must survive the drain: {entry:?}"
    );
}

#[tokio::test]
async fn powershell_foreground_timeout_is_rehomed_to_the_background() {
    if find_powershell().is_none() {
        return;
    }
    let (_dir, ts) = setup();
    let runner = std::sync::Arc::new(crate::storage::TaskRunner::new(None));
    let ts = ts.with_task_runner(runner.clone());

    let res = ts
        .powershell_with(
            &json!({ "command": "Start-Sleep -Seconds 30", "timeout": 1 }),
            None,
        )
        .await
        .unwrap();
    assert!(!res.is_error, "{}", res.content);
    assert!(
        res.content
            .contains("and was moved to the background (task_id: "),
        "{}",
        res.content
    );
    let task_id = res
        .content
        .split("(task_id: ")
        .nth(1)
        .unwrap()
        .split(')')
        .next()
        .unwrap()
        .to_string();
    assert!(
        runner.running_ids().contains(&task_id),
        "the migrated script must be registered as a running task"
    );

    let _ = runner.stop(&task_id, Some("test cleanup")).await;
}

#[tokio::test]
async fn workflow_tool_lists_without_a_subagent_runtime() {
    let (_dir, toolset) = setup();
    assert!(toolset.handles("Workflow"), "Workflow is a native tool");
    let result = toolset
        .execute_tool("Workflow", &serde_json::json!({ "operation": "list" }))
        .await
        .expect("Workflow is handled natively");
    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.contains("deep-research"));

    let run = toolset
        .execute_tool(
            "Workflow",
            &serde_json::json!({ "operation": "run", "script": "return 1;" }),
        )
        .await
        .expect("Workflow is handled natively");
    assert!(run.is_error);
    assert!(run.content.contains("no subagent runtime"));
}

mod mode_mutex_dispatch {
    use std::path::Path;
    use std::process::Command;
    use std::sync::{Arc, Mutex as StdMutex};

    use serde_json::{Value, json};

    use crate::callbacks::HostCallbacks;
    use crate::rpc::types::{
        BoxFuture, LlmChatRequest, LlmChatResponse, PermissionCheckRequest, PermissionDecision,
        StateReadRequest, StateReadResponse, StateWriteRequest, StateWriteResponse,
        ToolExecuteRequest, ToolExecuteResponse,
    };
    use crate::tools::NativeToolset;
    use crate::tools::tower::store::TowerStore;
    use crate::tools::tower::types::{TowerMissionStatus, TowerState};

    fn run_git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@example.test")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@example.test")
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        run_git(dir.path(), &["init", "-b", "main"]);
        std::fs::write(dir.path().join("README.md"), "seed\n").unwrap();
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-m", "seed"]);
        dir
    }

    struct StateHost {
        plan_active: bool,
        writes: Arc<StdMutex<Vec<Value>>>,
    }

    impl HostCallbacks for StateHost {
        fn llm_chat(
            &self,
            _: LlmChatRequest,
        ) -> BoxFuture<'static, Result<LlmChatResponse, String>> {
            Box::pin(async { Err("not used".into()) })
        }

        fn execute_tool(
            &self,
            _: ToolExecuteRequest,
        ) -> BoxFuture<'static, Result<ToolExecuteResponse, String>> {
            Box::pin(async { Err("not used".into()) })
        }

        fn check_permission(
            &self,
            _: PermissionCheckRequest,
        ) -> BoxFuture<'static, Result<PermissionDecision, String>> {
            Box::pin(async { Ok(PermissionDecision::allow()) })
        }

        fn emit_event(&self, _: Value) {}

        fn state_read(
            &self,
            request: StateReadRequest,
        ) -> BoxFuture<'static, Result<StateReadResponse, String>> {
            let active = self.plan_active && request.domain == "plan";
            Box::pin(async move {
                Ok(StateReadResponse {
                    value: json!({ "active": active }),
                })
            })
        }

        fn state_write(
            &self,
            request: StateWriteRequest,
        ) -> BoxFuture<'static, Result<StateWriteResponse, String>> {
            self.writes.lock().unwrap().push(request.value.clone());
            let value = request.value;
            Box::pin(async move { Ok(StateWriteResponse { ok: true, value }) })
        }
    }

    fn host(plan_active: bool) -> (Arc<dyn HostCallbacks>, Arc<StdMutex<Vec<Value>>>) {
        let writes = Arc::new(StdMutex::new(Vec::new()));
        (
            Arc::new(StateHost {
                plan_active,
                writes: writes.clone(),
            }),
            writes,
        )
    }

    async fn repo_with_open_mission(dir: &Path, toolset: &NativeToolset) {
        let init = toolset
            .execute_tool("TowerInit", &json!({}))
            .await
            .expect("TowerInit is native");
        assert!(!init.is_error, "{}", init.content);
        let plan = toolset
            .execute_tool(
                "TowerPlan",
                &json!({ "missions": [{ "title": "gate probe", "scope": ["probe/"] }] }),
            )
            .await
            .expect("TowerPlan is native");
        assert!(!plan.is_error, "{}", plan.content);
        let store = TowerStore::new(dir.to_path_buf());
        let state = store.load().await.unwrap();
        assert_eq!(state.missions.len(), 1);
        assert_eq!(state.missions[0].id, "M1");
    }

    fn status_of(state: &TowerState, id: &str) -> Option<TowerMissionStatus> {
        state.missions.iter().find(|m| m.id == id).map(|m| m.status)
    }

    #[tokio::test]
    async fn tower_spawn_worker_refuses_paused_mission() {
        let dir = init_repo();
        let (callbacks, _) = host(false);
        let toolset = NativeToolset::new(dir.path().to_str().unwrap(), None)
            .unwrap()
            .with_callbacks(callbacks);
        repo_with_open_mission(dir.path(), &toolset).await;

        let paused = crate::tools::mode_mutex::pause_tower_for_mode_enter(dir.path(), "test").await;
        assert_eq!(paused, vec!["M1".to_string()]);

        let result = toolset
            .execute_tool(
                "TowerSpawn",
                &json!({ "name": "w1", "kind": "worker", "mission_id": "M1" }),
            )
            .await
            .expect("TowerSpawn is native");
        assert!(result.is_error, "{}", result.content);
        assert!(
            result.content.contains("paused") && result.content.contains("status=active"),
            "{}",
            result.content
        );

        assert!(!dir.path().join(".tower/worktrees/wt-1").exists());
    }

    #[tokio::test]
    async fn agent_swarm_is_refused_while_tower_is_active() {
        let dir = init_repo();
        let (callbacks, _) = host(false);
        let toolset = NativeToolset::new(dir.path().to_str().unwrap(), None)
            .unwrap()
            .with_callbacks(callbacks)
            .with_subagents(std::sync::Arc::new(crate::subagent::SubagentManager::new()));
        repo_with_open_mission(dir.path(), &toolset).await;

        let before = TowerStore::new(dir.path().to_path_buf())
            .load()
            .await
            .unwrap();
        assert_eq!(status_of(&before, "M1"), Some(TowerMissionStatus::Planned));

        let result = toolset
            .execute_tool("AgentSwarm", &json!({ "tasks": [] }))
            .await
            .expect("AgentSwarm is native");
        assert!(result.is_error, "{}", result.content);
        assert!(
            result
                .content
                .contains("not available while tower mode is active")
                && result.content.contains("mutually exclusive")
                && result.content.contains("M1"),
            "the refusal must name the open missions it is gated on: {}",
            result.content
        );

        let after = TowerStore::new(dir.path().to_path_buf())
            .load()
            .await
            .unwrap();
        assert_eq!(status_of(&after, "M1"), Some(TowerMissionStatus::Planned));
    }

    #[tokio::test]
    async fn agent_swarm_is_not_vetoed_when_no_tower_is_initialized() {
        let dir = init_repo();
        let (callbacks, _) = host(false);
        let toolset = NativeToolset::new(dir.path().to_str().unwrap(), None)
            .unwrap()
            .with_callbacks(callbacks)
            .with_subagents(std::sync::Arc::new(crate::subagent::SubagentManager::new()));
        assert!(
            crate::tools::mode_mutex::refuse_swarm_with_active_tower(dir.path())
                .await
                .is_none()
        );

        if let Some(result) = toolset
            .execute_tool("AgentSwarm", &json!({ "tasks": [] }))
            .await
        {
            assert!(
                !result
                    .content
                    .contains("not available while tower mode is active"),
                "the swarm must not be vetoed without a tower: {}",
                result.content
            );
        }
    }

    #[tokio::test]
    async fn tower_merge_refuses_paused_mission_branch() {
        let dir = init_repo();
        let (callbacks, _) = host(false);
        let toolset = NativeToolset::new(dir.path().to_str().unwrap(), None)
            .unwrap()
            .with_callbacks(callbacks);
        repo_with_open_mission(dir.path(), &toolset).await;

        let state = TowerStore::new(dir.path().to_path_buf())
            .load()
            .await
            .unwrap();
        let branch = state.missions[0].branch.clone();

        let paused = crate::tools::mode_mutex::pause_tower_for_mode_enter(dir.path(), "test").await;
        assert_eq!(paused.len(), 1);

        let result = toolset
            .execute_tool("TowerMerge", &json!({ "branch": branch }))
            .await
            .expect("TowerMerge is native");
        assert!(result.is_error, "{}", result.content);
        assert!(
            result.content.contains("paused") && result.content.contains("M1"),
            "{}",
            result.content
        );
    }

    #[tokio::test]
    async fn tower_init_through_dispatch_exits_plan_mode() {
        let dir = init_repo();
        let (callbacks, writes) = host(true);
        let toolset = NativeToolset::new(dir.path().to_str().unwrap(), None)
            .unwrap()
            .with_callbacks(callbacks);

        let result = toolset
            .execute_tool("TowerInit", &json!({}))
            .await
            .expect("TowerInit is native");
        assert!(!result.is_error, "{}", result.content);
        assert!(
            result
                .content
                .contains("plan mode was active, so it was exited"),
            "{}",
            result.content
        );
        let writes = writes.lock().unwrap();
        assert_eq!(writes.len(), 1, "one plan deactivation write");
        assert_eq!(writes[0], json!({ "active": false }));
    }

    #[tokio::test]
    async fn subagent_tower_init_refused_and_plan_untouched() {
        let dir = init_repo();
        let (callbacks, writes) = host(true);
        let toolset = NativeToolset::new(dir.path().to_str().unwrap(), None)
            .unwrap()
            .with_callbacks(callbacks)
            .with_caller_agent_id("subagent-42");

        let result = toolset
            .execute_tool("TowerInit", &json!({}))
            .await
            .expect("TowerInit is native");
        assert!(result.is_error, "{}", result.content);
        assert!(
            writes.lock().unwrap().is_empty(),
            "a refused subagent init must not deactivate main's plan mode"
        );
    }

    #[tokio::test]
    async fn failed_tower_init_does_not_exit_plan_mode() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".tower"), b"x").unwrap();
        let (callbacks, writes) = host(true);
        let toolset = NativeToolset::new(dir.path().to_str().unwrap(), None)
            .unwrap()
            .with_callbacks(callbacks);

        let result = toolset
            .execute_tool("TowerInit", &json!({}))
            .await
            .expect("TowerInit is native");
        assert!(result.is_error, "{}", result.content);
        assert!(
            writes.lock().unwrap().is_empty(),
            "plan mode must survive a failed tower init"
        );
    }

    #[tokio::test]
    async fn worker_tower_send_wakes_the_main_session() {
        #[derive(Default)]
        struct Driver {
            payloads: std::sync::Mutex<Vec<(String, crate::session::wake::WakePayload)>>,
            drops: std::sync::Mutex<Vec<(String, String)>>,
        }
        impl crate::session::wake::TowerWakeDriver for Driver {
            fn enqueue(
                &self,
                session_id: &str,
                payload: crate::session::wake::WakePayload,
            ) -> Result<(), String> {
                self.payloads
                    .lock()
                    .unwrap()
                    .push((session_id.to_string(), payload));
                Ok(())
            }

            fn drop_wakes(&self, session_id: &str, variant: &str) -> bool {
                self.drops
                    .lock()
                    .unwrap()
                    .push((session_id.to_string(), variant.to_string()));
                true
            }
        }

        let dir = init_repo();
        let driver = Arc::new(Driver::default());
        let runtime = crate::tools::tower::wake::TowerWakeRuntime::new(driver.clone());
        let (callbacks, _) = host(false);
        let main = NativeToolset::new(dir.path().to_str().unwrap(), None)
            .unwrap()
            .with_callbacks(callbacks)
            .with_session_id("session-main");
        let _main_replay = main.bind_tower_wake_runtime(runtime.clone());

        let init = main
            .execute_tool("TowerInit", &json!({}))
            .await
            .expect("TowerInit is native");
        assert!(!init.is_error, "{}", init.content);

        let store = TowerStore::new(dir.path().to_path_buf());
        store
            .register_agent(crate::tools::tower::types::TowerRosterEntry {
                name: "worker-1".into(),
                agent_id: "w1".into(),
                session_id: None,
                kind: crate::tools::tower::types::TowerAgentKind::Worker,
                mission_id: None,
                review_target: None,
                review_mission_id: None,
                worktree: None,
                branch: None,
                spawned_at: chrono::Utc::now().to_rfc3339(),
                died_at: None,
                death_status: None,
                death_reason: None,
                status: None,
            })
            .await
            .expect("worker registration");

        let (worker_callbacks, _) = host(false);
        let worker = NativeToolset::new(dir.path().to_str().unwrap(), None)
            .unwrap()
            .with_callbacks(worker_callbacks)
            .with_caller_agent_id("w1");
        let _worker_replay = worker.bind_tower_wake_runtime(runtime);

        let sent = worker
            .execute_tool(
                "TowerSend",
                &json!({ "to": "tower", "subject": "review-request", "body": "branch is ready" }),
            )
            .await
            .expect("TowerSend is native");
        assert!(!sent.is_error, "{}", sent.content);
        let sent2 = worker
            .execute_tool(
                "TowerSend",
                &json!({ "to": "tower", "subject": "follow-up", "body": "also ready" }),
            )
            .await
            .expect("TowerSend is native");
        assert!(!sent2.is_error, "{}", sent2.content);

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let items: usize = driver
                    .payloads
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|(_, payload)| payload.items.len())
                    .sum();
                if items == 2 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("rich wake was not enqueued");
        let payload_count = {
            let payloads = driver.payloads.lock().unwrap();
            assert!(
                payloads
                    .iter()
                    .all(|(session, _)| session == "session-main")
            );
            assert!(payloads.iter().all(|(_, payload)| {
                payload.variant == "tower_inbox" && payload.message.content.contains("TowerInbox")
            }));
            payloads.len()
        };

        let main_send = main
            .execute_tool(
                "TowerSend",
                &json!({ "to": "worker-1", "subject": "note", "body": "carry on" }),
            )
            .await
            .expect("TowerSend is native");
        assert!(!main_send.is_error, "{}", main_send.content);
        assert_eq!(driver.payloads.lock().unwrap().len(), payload_count);

        let teardown = main
            .execute_tool("TowerTeardown", &json!({}))
            .await
            .expect("TowerTeardown is native");
        assert!(!teardown.is_error, "{}", teardown.content);
        assert_eq!(
            driver.drops.lock().unwrap().as_slice(),
            &[("session-main".to_string(), "tower_inbox".to_string())]
        );
    }
}

#[tokio::test]
async fn edit_matches_the_model_view_of_a_crlf_file() {
    let (_dir, ts) = setup();
    std::fs::write(_dir.path().join("crlf.txt"), "alpha\r\nbeta\r\ngamma\r\n").unwrap();
    let result = ts
        .execute_mutating(
            "Edit",
            &json!({
                "path": "crlf.txt",
                "old_string": "alpha\nbeta",
                "new_string": "alpha\nBETA",
            }),
        )
        .await
        .unwrap();
    assert!(!result.is_error, "content: {}", result.content);
    assert_eq!(
        std::fs::read_to_string(_dir.path().join("crlf.txt")).unwrap(),
        "alpha\r\nBETA\r\ngamma\r\n",
        "the pure-CRLF style must be preserved"
    );
}

#[tokio::test]
async fn edit_refuses_invalid_utf8_instead_of_rewriting_it() {
    let (_dir, ts) = setup();
    let path = _dir.path().join("binary.txt");
    std::fs::write(&path, [0xff, 0xfe, b'a']).unwrap();
    let result = ts
        .execute_mutating(
            "Edit",
            &json!({
                "path": "binary.txt",
                "old_string": "a",
                "new_string": "b",
            }),
        )
        .await
        .unwrap();
    assert!(result.is_error, "content: {}", result.content);
    assert_eq!(std::fs::read(&path).unwrap(), vec![0xff, 0xfe, b'a']);
}

#[tokio::test]
async fn a_deferred_tool_call_is_refused_with_v2_guidance() {
    let (_dir, ts) = setup();
    let manager = std::sync::Arc::new(crate::mcp::McpManager::new());
    manager
        .configure(
            "github",
            crate::mcp::manager::McpServerRecipe::Mock,
            crate::mcp::manager::McpServerOptions {
                deferred: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    manager
        .add_client(crate::mcp::client::McpClient::mock("github"))
        .await;
    let ts = ts
        .with_mcp(manager.clone())
        .with_model_capabilities(Some(vec![
            "tool_use".into(),
            "dynamically_loaded_tools".into(),
        ]))
        .with_tool_select_enabled(true);

    let unloaded = ts
        .execute_tool("mcp__github__github_sample_tool", &json!({}))
        .await
        .expect("the engine answers it, so the host never sees it");
    assert!(unloaded.is_error);
    assert_eq!(
        unloaded.content,
        "Tool \"mcp__github__github_sample_tool\" is available but not loaded. \
Call select_tools with [\"mcp__github__github_sample_tool\"] first, then call the tool."
    );

    ts.execute_tool(
        "select_tools",
        &json!({ "names": ["mcp__github__github_sample_tool"] }),
    )
    .await
    .expect("select_tools is a native tool");
    manager.remove_server("github").await;
    let gone = ts
        .execute_tool("mcp__github__github_sample_tool", &json!({}))
        .await
        .expect("the engine answers it");
    assert!(gone.is_error, "content: {}", gone.content);
    assert_eq!(
        gone.content,
        "Tool \"mcp__github__github_sample_tool\" was loaded but its MCP server is \
currently disconnected. It may become available again when the server reconnects; \
do not retry immediately."
    );
}

#[tokio::test]
async fn disabled_disclosure_reconciles_the_initial_tool_table() {
    let (_dir, ts) = setup();
    let mut initial = tool_info("Read");
    initial.description = "old read".into();
    initial.input_schema = json!({ "type": "object", "properties": { "old": {} } });
    ts.record_initial_tool_table_ref(&[initial.clone()]);

    let mut current = tool_info("Read");
    current.description = "new read".into();
    current.input_schema = json!({ "type": "object", "properties": { "new": {} } });
    let runtime_only = tool_info("mcp__github__late_addition");
    let shaped = ts.shape_tool_table(vec![current, runtime_only]);

    assert_eq!(
        shaped.len(),
        1,
        "runtime-only additions stay out: {shaped:?}"
    );
    assert_eq!(shaped[0].description, "new read");
    assert!(
        shaped[0].input_schema["properties"].get("new").is_some(),
        "a same-name tool must carry the current schema: {:?}",
        shaped[0].input_schema
    );
    assert!(
        shaped[0].input_schema["properties"].get("old").is_none(),
        "the initial snapshot must not survive a same-name update"
    );
}

struct SilentLlm;

impl crate::turn_loop::types::LLM for SilentLlm {
    fn system_prompt(&self) -> &str {
        "mock system prompt"
    }
    fn model_name(&self) -> &str {
        "mock-model"
    }
    fn is_retryable_error(&self, _error: &str) -> bool {
        false
    }
    fn chat(
        &self,
        _params: crate::turn_loop::types::LLMChatParams,
    ) -> crate::rpc::types::BoxFuture<
        '_,
        Result<crate::turn_loop::types::LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>,
    > {
        Box::pin(async { Err("unused".into()) })
    }
}

struct UnusedHost;

impl crate::callbacks::HostCallbacks for UnusedHost {
    fn llm_chat(
        &self,
        _request: crate::rpc::types::LlmChatRequest,
    ) -> crate::rpc::types::BoxFuture<'static, Result<crate::rpc::types::LlmChatResponse, String>>
    {
        Box::pin(async { Err("unused".into()) })
    }

    fn execute_tool(
        &self,
        _request: crate::rpc::types::ToolExecuteRequest,
    ) -> crate::rpc::types::BoxFuture<'static, Result<crate::rpc::types::ToolExecuteResponse, String>>
    {
        Box::pin(async { Err("unused".into()) })
    }

    fn check_permission(
        &self,
        _request: crate::rpc::types::PermissionCheckRequest,
    ) -> crate::rpc::types::BoxFuture<'static, Result<crate::rpc::types::PermissionDecision, String>>
    {
        Box::pin(async { Ok(crate::rpc::types::PermissionDecision::allow()) })
    }
}

async fn manager_bound_to(
    work: &std::path::Path,
    env: std::collections::BTreeMap<String, String>,
) -> std::sync::Arc<crate::subagent::SubagentManager> {
    let manager = std::sync::Arc::new(crate::subagent::SubagentManager::new());
    manager
        .set_runtime(
            std::sync::Arc::new(SilentLlm),
            std::sync::Arc::new(UnusedHost),
            Some("session-tools".into()),
        )
        .await;
    let roots = crate::environment::WorkspaceRoots::new(work, Vec::new()).unwrap();
    let snapshot = crate::environment::EnvironmentSnapshot::capture(
        &crate::environment::EnvironmentDeclaration::default(),
        None,
        Some(roots.work_dir()),
        env,
        &roots,
        false,
    )
    .unwrap();
    manager
        .set_session_environment("session-tools", snapshot, roots)
        .await;
    manager
}

async fn bound_manager_and_id(
    work: &std::path::Path,
    requested: &str,
    child_env: std::collections::BTreeMap<String, String>,
) -> (std::sync::Arc<crate::subagent::SubagentManager>, String) {
    let manager = manager_bound_to(
        work,
        std::collections::BTreeMap::from([("PARENT_ONLY".to_string(), "1".to_string())]),
    )
    .await;
    let id = manager
        .spawn_in_environment(
            "explore",
            "child",
            crate::environment::AgentEnvironmentRequest {
                cwd: Some(std::path::PathBuf::from(requested)),
                env: child_env,
            },
        )
        .await
        .unwrap();
    (manager, id)
}

async fn bound_toolset(
    work: &std::path::Path,
    requested: &str,
    child_env: std::collections::BTreeMap<String, String>,
) -> NativeToolset {
    let (manager, id) = bound_manager_and_id(work, requested, child_env).await;
    NativeToolset::new(work.to_str().unwrap(), None)
        .unwrap()
        .with_subagents(manager)
        .with_caller_agent_id(id)
}

#[tokio::test]
async fn a_bound_agent_resolves_relative_paths_from_its_own_cwd() {
    let work = tempfile::tempdir().unwrap();
    let nested = work.path().join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(work.path().join("root.txt"), "root\n").unwrap();
    std::fs::write(nested.join("child.txt"), "child marker\n").unwrap();

    let ts = bound_toolset(
        work.path(),
        "nested",
        std::collections::BTreeMap::from([("PARENT_ONLY".to_string(), "child".to_string())]),
    )
    .await;

    assert_eq!(
        ts.execution_root(),
        std::fs::canonicalize(&nested).unwrap(),
        "the default execution directory is the agent's cwd"
    );

    let env = ts.execution_environment();
    assert_eq!(env.get("PARENT_ONLY").map(String::as_str), Some("child"));

    let read = ts.execute("Read", &json!({ "path": "child.txt" })).unwrap();
    assert!(!read.is_error, "content: {}", read.content);
    assert!(read.content.contains("child marker"));

    let root = ts.execute("Read", &json!({ "path": "root.txt" })).unwrap();
    assert!(root.is_error, "content: {}", root.content);

    let grep = ts
        .execute("Grep", &json!({ "pattern": "child marker" }))
        .unwrap();
    assert!(!grep.is_error, "content: {}", grep.content);
    assert!(
        grep.content.contains("child.txt"),
        "content: {}",
        grep.content
    );
}

#[tokio::test]
async fn a_bound_agent_stays_inside_the_session_roots() {
    let work = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let nested = work.path().join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::create_dir_all(work.path().join("sibling")).unwrap();

    let ts = bound_toolset(work.path(), "nested", std::collections::BTreeMap::new()).await;

    let escaped = outside.path().join("escaped.txt");
    let refusal = ts
        .execute_mutating(
            "Write",
            &json!({
                "path": format!("../../{}", escaped.file_name().unwrap().to_string_lossy()),
                "content": "nope",
            }),
        )
        .await
        .unwrap();
    assert!(refusal.is_error, "content: {}", refusal.content);

    let sibling = work.path().join("sibling").join("ok.txt");
    let write = ts
        .execute_mutating(
            "Write",
            &json!({ "path": sibling.to_str().unwrap(), "content": "ok" }),
        )
        .await
        .unwrap();
    assert!(!write.is_error, "content: {}", write.content);
    assert_eq!(std::fs::read_to_string(&sibling).unwrap(), "ok");
}

#[tokio::test]
async fn an_unbound_agent_falls_back_to_the_session_root() {
    let work = tempfile::tempdir().unwrap();
    std::fs::write(work.path().join("a.txt"), "alpha\n").unwrap();

    let manager = manager_bound_to(work.path(), std::collections::BTreeMap::new()).await;
    let ts = NativeToolset::new(work.path().to_str().unwrap(), None)
        .unwrap()
        .with_subagents(manager)
        .with_caller_agent_id("agent-unbound");

    assert_eq!(
        ts.execution_root(),
        std::fs::canonicalize(work.path()).unwrap(),
        "an agent without a binding runs at the session root, as before"
    );
    assert!(ts.execution_environment().is_empty());
    let read = ts.execute("Read", &json!({ "path": "a.txt" })).unwrap();
    assert!(!read.is_error, "content: {}", read.content);
    assert!(read.content.contains("alpha"));
}

#[tokio::test]
async fn a_bound_agent_runs_bash_in_its_own_directory() {
    let Some(shell) = find_bash() else { return };
    let work = tempfile::tempdir().unwrap();
    let nested = work.path().join("nested");
    std::fs::create_dir_all(&nested).unwrap();

    let (manager, id) = bound_manager_and_id(
        work.path(),
        "nested",
        std::collections::BTreeMap::from([("AGENT_MARKER".to_string(), "from-child".to_string())]),
    )
    .await;
    let ts = NativeToolset::new(work.path().to_str().unwrap(), Some(&shell))
        .unwrap()
        .with_subagents(manager)
        .with_caller_agent_id(id);

    let cwd = ts
        .execute_mutating("Bash", &json!({ "command": "pwd" }))
        .await
        .unwrap();
    assert!(!cwd.is_error, "content: {}", cwd.content);
    assert!(cwd.content.contains("nested"), "content: {}", cwd.content);

    let marker = ts
        .execute_mutating("Bash", &json!({ "command": "echo $AGENT_MARKER" }))
        .await
        .unwrap();
    assert!(!marker.is_error, "content: {}", marker.content);
    assert!(
        marker.content.contains("from-child"),
        "content: {}",
        marker.content
    );
}
