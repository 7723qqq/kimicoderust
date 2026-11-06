use super::super::*;
use std::collections::HashMap;

// A session whose turn is streaming right now answers history with
// `in_flight` — the same turn/step entity ids the live deltas carry —
// so a reconnecting client splices at the right position (upstream
// `projection.inFlight`).

// The official Web bundle's listSessionsV2/archiveSessions/restoreSessions
// contract (upstream routes/v2/sessions.ts): a domain-grouped page plus
// per-item batch results.
#[tokio::test]
async fn v2_sessions_contract_serves_the_official_web_bundle() {
    let server = HttpServer::in_memory().unwrap();
    server
        .store
        .create_workspace("G:/kimi/kimi-code", Some("kimi-code"))
        .ok();
    let ws_id = server.store.list_workspaces().unwrap_or_default()[0]
        .id
        .clone();
    server
        .store
        .create_session_with_workspace("sess-v2-a", Some("Contract"), Some(&ws_id))
        .unwrap();
    server
        .store
        .save_turn(
            "sess-v2-a",
            "turn-1",
            1,
            &[crate::turn_loop::types::LLMMessage::user("hi")],
            None,
            None,
        )
        .unwrap();

    // GET /api/v2/sessions answers the v2SessionPageSchema envelope with
    // domain-grouped items.
    let res = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v2/sessions".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res.status, 200);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(
        body["total"],
        body["items"].as_array().map(|i| i.len()).unwrap()
    );
    assert_eq!(body["has_more"], false);
    assert!(body["next_page_token"].is_null());
    let first = &body["items"][0];
    assert_eq!(first["id"], "sess-v2-a");
    // Title may be null for a session created without one; the bundle
    // falls back to last_prompt then the id prefix.
    assert!(first["meta"]["title"].is_null() || first["meta"]["title"].is_string());
    assert!(first["meta"]["archived"].is_boolean());
    assert!(
        ["running", "approval", "question", "failed", "idle"]
            .contains(&first["activity"]["status"].as_str().unwrap())
    );

    // view=by_workspace (the sessions sidebar's first load) returns
    // per-workspace groups: the workspace object plus the first
    // group.page_size sessions of that workspace.
    let res = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v2/sessions".into(),
            query: Some("view=by_workspace&group.page_size=5&meta.has_prompt=true".into()),
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res.status, 200);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    let groups = body["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 1, "one workspace holds the session");
    assert_eq!(groups[0]["workspace"]["cwd"], "G:/kimi/kimi-code");
    assert_eq!(groups[0]["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(groups[0]["total"], 1);

    // Batch archive: {ids} in, per-item results out — a missing session
    // folds into its own item instead of failing the batch.
    let res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v2/sessions:archive".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({
                "ids": ["sess-v2-a", "sess-v2-missing"]
            }))
            .unwrap(),
        })
        .await;
    assert_eq!(res.status, 200);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(body["succeeded"], 1);
    assert_eq!(body["failed"], 1);
    assert_eq!(body["results"][0]["ok"], true);
    assert_eq!(body["results"][1]["ok"], false);

    // The archived flag is visible on the item after the batch.
    let res = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v2/sessions".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    let archived = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == "sess-v2-a")
        .unwrap()["meta"]["archived"]
        .clone();
    assert_eq!(archived, true);

    // Restore flips it back.
    let res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v2/sessions:restore".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "ids": ["sess-v2-a"] })).unwrap(),
        })
        .await;
    assert_eq!(res.status, 200);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(body["succeeded"], 1);
}

/// Every shape here is read off the shipped `dist-web` bundle's API client;
/// each assertion fails against the response the route used to serve, which
/// is what made the corresponding Web feature unusable.
#[tokio::test]
async fn the_web_bundle_contract_holds_for_the_shapes_it_maps_directly() {
    let server = HttpServer::in_memory().unwrap();
    server
        .store
        .create_workspace("G:/kimi/kimi-code", Some("kimi-code"))
        .ok();
    let ws_id = server.store.list_workspaces().unwrap_or_default()[0]
        .id
        .clone();
    server
        .store
        .create_session_with_workspace("sess-wire", Some("Wire"), Some(&ws_id))
        .unwrap();
    server
        .store
        .save_turn(
            "sess-wire",
            "turn-1",
            1,
            &[crate::turn_loop::types::LLMMessage::user("hi")],
            None,
            None,
        )
        .unwrap();
    // The outcome of the last turn, as the engine persists it at the end of
    // one. Without this on the wire the value is only ever a live event, so
    // a client that attaches after the turn — or after a restart — never
    // learns how it ended.
    let updated_before = server
        .store
        .get_session("sess-wire")
        .unwrap()
        .unwrap()
        .updated_at;
    server
        .store
        .set_last_turn_reason("sess-wire", Some("cancelled"))
        .unwrap();

    // Every shape here is read off the shipped `dist-web` bundle's API client;
    // each assertion fails against the response the route used to serve, which
    // is what made the corresponding Web feature unusable.
    async fn get(server: &HttpServer, path: &str) -> HttpResponse {
        server
            .handle_request(&HttpRequest {
                method: "GET".into(),
                path: path.into(),
                query: None,
                headers: HashMap::new(),
                body: Vec::new(),
            })
            .await
    }

    // v2 `GET /sessions/{session_id}` answers the bare session document
    // (`sessionSchema`), which is why the mapper can read `metadata.cwd`
    // straight off the top level.
    let res = get(&server, "/api/v1/sessions/sess-wire").await;
    assert_eq!(res.status, 200);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(body["id"], "sess-wire");
    // `format_wire_session` falls back to the process cwd when the session
    // never recorded one; the contract being pinned is that `metadata.cwd`
    // is a string at the top level, which is what the mapper dereferences.
    assert!(
        body["metadata"]["cwd"].is_string(),
        "the mapper reads `e.metadata.cwd` unconditionally"
    );
    assert!(body["agent_config"]["model"].is_string());
    assert!(
        body.get("messages").is_none(),
        "messages live on `/messages`, not on the session document"
    );
    // The persisted turn outcome, read back by a client that did not see the
    // live event. `null` is the honest answer for a session whose turn never
    // ended, which is why the value is asserted rather than its presence.
    assert_eq!(
        body["last_turn_reason"], "cancelled",
        "the wire must carry the persisted outcome: {body}"
    );
    // v2 passes `touchUpdatedAt: false` for this write, because the mirror
    // records a turn that already ended elsewhere; treating it as activity
    // would reorder the session list on every turn end.
    let updated_after = server
        .store
        .get_session("sess-wire")
        .unwrap()
        .unwrap()
        .updated_at;
    assert_eq!(
        updated_after, updated_before,
        "the mirror must not read as activity"
    );

    // v2 `listMessagesResponseSchema` is `{items, has_more}`.
    let res = get(&server, "/api/v1/sessions/sess-wire/messages").await;
    assert_eq!(res.status, 200);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert!(body.get("items").is_some() || body.get("messages").is_some());

    // v2 `listTasks` answers `pageResponseSchema(taskSchema)`: `{items}`,
    // every element carrying the protocol field names including
    // `run_in_background`.
    let res = get(&server, "/api/v1/sessions/sess-wire/tasks").await;
    assert_eq!(res.status, 200);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    let items = body["items"].as_array().expect("`items` key");
    assert!(items.is_empty(), "no task was spawned in this test");

    // v2 `getSessionGoalResponseSchema = goalSnapshotSchema.nullable()`, so
    // an inactive goal is `null` — never a wrapper object.
    let res = get(&server, "/api/v1/sessions/sess-wire/goal").await;
    assert_eq!(res.status, 200);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert!(
        body.is_null(),
        "no active goal reads as null, matching goalSnapshotSchema.nullable()"
    );

    // `getSessionStatus` reads the session's real profile; these used to be
    // the literals "auto" / "medium" with five fields absent entirely.
    let res = get(&server, "/api/v1/sessions/sess-wire/status").await;
    assert_eq!(res.status, 200);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    for key in [
        "permission",
        "thinking_level",
        "plan_mode",
        "swarm_mode",
        "tower_mode",
        "max_context_tokens",
        "context_usage",
    ] {
        assert!(body.get(key).is_some(), "status field `{key}` is served");
    }
    assert!(body["max_context_tokens"].as_u64().unwrap() > 0);

    // `listCapabilities` filters on `supported`, so the elements must be
    // objects — a bare id array made every row disappear.
    let res = get(&server, "/api/v1/capabilities").await;
    assert_eq!(res.status, 200);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    let caps = body["capabilities"].as_array().unwrap();
    assert!(!caps.is_empty());
    assert!(caps.iter().all(|c| c["supported"] == true));
    assert!(caps.iter().all(|c| c["id"].is_string()));

    // `pollOAuthLogin` reads snake_case and treats `authenticated` as the
    // terminal success; no flow is running, so the body is null.
    let res = get(&server, "/api/v1/oauth/login").await;
    assert_eq!(res.status, 200);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert!(body.is_null(), "no live flow reads as null");

    // The bundle's `installPlugin` posts `{ source }`, which used to 400 as
    // "Missing plugin id" because only `id` / `name` were read.
    let res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/plugins".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "source": "definitely-not-a-plugin" })).unwrap(),
        })
        .await;
    assert_ne!(
        res.status, 400,
        "a `source`-only body must reach the installer, not be rejected as malformed"
    );

    // The bundle spells the suggest route with a single colon; only the
    // `search` sibling accepted that form, so every file-mention request
    // paid a 404 round-trip first.
    let res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/workspace/fs:suggest".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({})).unwrap(),
        })
        .await;
    assert_ne!(res.status, 404, "single-colon fs:suggest is routed");
}

/// v2 `POST /workspace/fs::list` (from #4457): the session-less fs:list with
/// the workspace reference in the body.
#[tokio::test]
async fn workspace_fs_list_lists_without_a_session() {
    let server = HttpServer::in_memory().unwrap();
    let ws_root = tempfile::tempdir().unwrap();
    std::fs::write(ws_root.path().join("marker.txt"), "m").unwrap();

    for path in ["/api/v1/workspace/fs::list", "/api/v1/workspace/fs:list"] {
        let res = server
            .handle_request(&HttpRequest {
                method: "POST".into(),
                path: path.into(),
                query: None,
                headers: HashMap::new(),
                body: serde_json::to_vec(&json!({
                    "workspace": ws_root.path().to_string_lossy(),
                }))
                .unwrap(),
            })
            .await;
        assert_eq!(res.status, 200, "{path} is routed");
        let body: Value = serde_json::from_slice(&res.body).unwrap();
        let data = body.get("data").unwrap_or(&body);
        assert!(
            data["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|i| i["name"] == "marker.txt"),
            "{path} lists the workspace root: {data}"
        );
    }

    // A missing workspace reference is a client error, not a listing of the
    // server's own cwd.
    let res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/workspace/fs::list".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({})).unwrap(),
        })
        .await;
    assert_eq!(res.status, 400);
}

/// The engine's export must describe itself the way the host's exporter
/// does: a manifest head naming the session, and log members only when they
/// were asked for.
#[test]
fn session_export_archive_names_itself_and_gates_the_log_members() {
    use std::io::Read as _;

    let export = crate::session::sqlite_store::SessionExport {
        session: crate::session::sqlite_store::SessionSummary {
            session_id: "sess-manifest".into(),
            title: Some("T".into()),
            created_at: 1,
            updated_at: 2,
            workspace_id: None,
            archived: false,
            parent_session_id: None,
        },
        messages: Vec::new(),
        turns_count: 0,
        exported_at: "2026-10-03T00:00:00Z".into(),
    };

    let names_of = |bytes: Vec<u8>| -> (Vec<String>, Value) {
        let mut archive =
            zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("archive readable");
        let names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();
        let mut manifest = Vec::new();
        archive
            .by_name("export-manifest.json")
            .expect("manifest present")
            .read_to_end(&mut manifest)
            .unwrap();
        (names, serde_json::from_slice(&manifest).unwrap())
    };

    // Default: the manifest rides along, no logs, and the key that names the
    // primary log is absent rather than null — the host's e2e pins that
    // absence, so a null here would be a different document.
    let (names, manifest) = names_of(build_session_export_zip_with(&export, Vec::new()).unwrap());
    assert_eq!(
        names,
        vec!["export-manifest.json", "session.json", "transcript.md"]
    );
    assert_eq!(manifest["sessionId"], "sess-manifest");
    assert_eq!(manifest["exportedAt"], "2026-10-03T00:00:00Z");
    assert_eq!(manifest["kimiCodeVersion"], env!("CARGO_PKG_VERSION"));
    assert_eq!(manifest["os"], std::env::consts::OS);
    assert!(
        manifest.get("globalLogPath").is_none(),
        "omitted, not written as null"
    );

    // With logs resolved they ride along after the session members, and the
    // manifest names the un-suffixed one as the primary entry.
    let (names, manifest) = names_of(
        build_session_export_zip_with(
            &export,
            vec![
                ("logs/kimi-code.log".into(), b"new".to_vec()),
                ("logs/kimi-code.log.1".into(), b"old".to_vec()),
            ],
        )
        .unwrap(),
    );
    assert_eq!(
        names,
        vec![
            "export-manifest.json",
            "session.json",
            "transcript.md",
            "logs/kimi-code.log",
            "logs/kimi-code.log.1"
        ]
    );
    assert_eq!(manifest["globalLogPath"], "logs/kimi-code.log");
}

/// The log selection and member naming have to match the host exporter and
/// `apps/vis`: `kimi-code.log` plus its `.N` rotations, nothing else.
#[test]
fn global_log_members_take_the_rotations_and_nothing_else() {
    let dir = std::env::temp_dir().join(format!(
        "kimi-log-members-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("kimi-code.log"), b"newest").unwrap();
    std::fs::write(dir.join("kimi-code.log.1"), b"older").unwrap();
    std::fs::write(dir.join("kimi-code.log.2"), b"oldest").unwrap();
    std::fs::write(dir.join("kimi-code-desktop.log"), b"other").unwrap();
    std::fs::create_dir(dir.join("kimi-code.log.dir")).unwrap();

    let members = global_log_members_in(&dir);
    let names: Vec<&str> = members.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "logs/kimi-code.log",
            "logs/kimi-code.log.1",
            "logs/kimi-code.log.2"
        ],
        "sorted, logs/-prefixed, and neither the desktop log nor a directory"
    );
    assert_eq!(members[0].1, b"newest".to_vec());

    std::fs::remove_dir_all(&dir).unwrap();
}
