use super::super::*;
use std::collections::HashMap;

#[tokio::test]
async fn test_http_static_assets_and_spa_routing() {
    use std::fs::{self, File};
    use std::io::Write;
    use tempfile::tempdir;

    let dir = tempdir().unwrap();
    let index_path = dir.path().join("index.html");
    let mut f1 = File::create(&index_path).unwrap();
    f1.write_all(b"<!DOCTYPE html><html><body>Kimi Web UI</body></html>")
        .unwrap();

    let assets_dir = dir.path().join("assets");
    fs::create_dir(&assets_dir).unwrap();
    let js_path = assets_dir.join("index-123.js");
    let mut f2 = File::create(&js_path).unwrap();
    f2.write_all(b"console.log('web ui loaded');").unwrap();

    let server = HttpServer::in_memory().unwrap().with_web_assets(dir.path());

    // 1. Root / serves index.html
    let res_root = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_root.status, 200);
    assert_eq!(
        res_root.header("content-type"),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(
        res_root.body,
        b"<!DOCTYPE html><html><body>Kimi Web UI</body></html>"
    );

    // 2. Static asset request
    let res_asset = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/assets/index-123.js".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_asset.status, 200);
    assert_eq!(
        res_asset.header("content-type"),
        Some("application/javascript; charset=utf-8")
    );
    assert_eq!(res_asset.body, b"console.log('web ui loaded');");

    // 3. SPA deep route fallback
    let res_spa = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/session/sess-abc".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_spa.status, 200);
    assert_eq!(
        res_spa.header("content-type"),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(
        res_spa.body,
        b"<!DOCTYPE html><html><body>Kimi Web UI</body></html>"
    );

    // 4. API routes do not fallback to index.html
    let res_api = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/missing-route".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_api.status, 404);
}

#[tokio::test]
async fn workspace_require_trust_config_controls_effective_trust() {
    for (index, (config_text, expected)) in [
        ("", false),
        ("[workspace]\nrequire_trust = true\n", false),
        ("[workspace]\nrequire_trust = false\n", true),
    ]
    .into_iter()
    .enumerate()
    {
        let config: crate::config::KimiConfig = config_text.parse().unwrap();
        let server = HttpServer::in_memory().unwrap().with_config(config);
        let workspace = server
            .store()
            .create_workspace(&format!("/workspace/require-trust-{index}"), None)
            .unwrap();
        server
            .store()
            .set_workspace_trusted(&workspace.id, false)
            .unwrap();

        let get = server
            .handle_request(&HttpRequest {
                method: "GET".into(),
                path: format!("/api/v1/workspaces/{}/trust", workspace.id),
                query: None,
                headers: HashMap::new(),
                body: Vec::new(),
            })
            .await;
        assert_eq!(get.status, 200);
        let effective: Value = serde_json::from_slice(&get.body).unwrap();
        assert_eq!(effective["trusted"], expected, "config: {config_text}");

        let revoke = server
            .handle_request(&HttpRequest {
                method: "POST".into(),
                path: format!("/api/v1/workspaces/{}/trust", workspace.id),
                query: None,
                headers: HashMap::new(),
                body: serde_json::to_vec(&json!({ "trusted": false })).unwrap(),
            })
            .await;
        assert_eq!(revoke.status, 200);
        let after_revoke: Value = serde_json::from_slice(&revoke.body).unwrap();
        assert_eq!(
            after_revoke["trusted"], expected,
            "revocation returns the effective state, not merely the persisted write"
        );
        assert!(
            !server.store().is_workspace_trusted(&workspace.id).unwrap(),
            "the config override must not rewrite the persisted revocation"
        );
    }
}

#[tokio::test]
async fn test_http_workspaces_crud_and_trust() {
    let server = HttpServer::in_memory().unwrap();

    // 1. Initial list is empty
    let res_list = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/workspaces".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_list.status, 200);
    let val_list: Value = serde_json::from_slice(&res_list.body).unwrap();
    assert_eq!(val_list["items"].as_array().unwrap().len(), 0);

    // 2. Create workspace - missing root -> 400
    let res_bad = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/workspaces".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({})).unwrap(),
        })
        .await;
    assert_eq!(res_bad.status, 400);

    // 3. Create valid workspace
    let res_create = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/workspaces".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({
                "root": "/workspace/my-app",
                "name": "My Application"
            }))
            .unwrap(),
        })
        .await;
    assert_eq!(res_create.status, 201);
    let ws: Value = serde_json::from_slice(&res_create.body).unwrap();
    let ws_id = ws["id"].as_str().unwrap().to_string();
    assert!(ws_id.starts_with("wd_my-app_"));
    assert_eq!(ws["name"], "My Application");
    assert_eq!(ws["session_count"], 0);

    // 4. Get workspace by id
    let res_get = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: format!("/api/v1/workspaces/{ws_id}"),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_get.status, 200);
    let val_get: Value = serde_json::from_slice(&res_get.body).unwrap();
    assert_eq!(val_get["id"], ws_id);

    // 5. Fail-closed trust: a new workspace has no explicit trust record
    let res_trust = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: format!("/api/v1/workspaces/{ws_id}/trust"),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_trust.status, 200);
    let val_trust: Value = serde_json::from_slice(&res_trust.body).unwrap();
    assert_eq!(val_trust["trusted"], false);

    // A POST without `trusted` is not an explicit grant and must stay untrusted
    let res_set_untyped = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/workspaces/{ws_id}/trust"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({})).unwrap(),
        })
        .await;
    assert_eq!(res_set_untyped.status, 200);
    let val_set_untyped: Value = serde_json::from_slice(&res_set_untyped.body).unwrap();
    assert_eq!(val_set_untyped["trusted"], false);
    assert!(!server.store().is_workspace_trusted(&ws_id).unwrap());

    let res_set_trust = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/workspaces/{ws_id}/trust"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "trusted": true })).unwrap(),
        })
        .await;
    assert_eq!(res_set_trust.status, 200);
    let val_set_trust: Value = serde_json::from_slice(&res_set_trust.body).unwrap();
    assert_eq!(val_set_trust["trusted"], true);
    assert!(server.store().is_workspace_trusted(&ws_id).unwrap());

    let res_revoke_trust = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/workspaces/{ws_id}/trust"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "trusted": false })).unwrap(),
        })
        .await;
    assert_eq!(res_revoke_trust.status, 200);
    let val_revoke_trust: Value = serde_json::from_slice(&res_revoke_trust.body).unwrap();
    assert_eq!(val_revoke_trust["trusted"], false);

    // 6. Create session under workspace -> session_count updates
    server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/sessions".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({
                "title": "Task 1",
                "workspace_id": ws_id
            }))
            .unwrap(),
        })
        .await;

    let res_get_updated = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: format!("/api/v1/workspaces/{ws_id}"),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    let val_updated: Value = serde_json::from_slice(&res_get_updated.body).unwrap();
    assert_eq!(val_updated["session_count"], 1);

    // 6b. Rename workspace via PATCH /api/v1/workspaces/{ws_id}
    let res_patch = server
        .handle_request(&HttpRequest {
            method: "PATCH".into(),
            path: format!("/api/v1/workspaces/{ws_id}"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({
                "name": "Renamed Application"
            }))
            .unwrap(),
        })
        .await;
    assert_eq!(res_patch.status, 200);
    let val_patched: Value = serde_json::from_slice(&res_patch.body).unwrap();
    assert_eq!(val_patched["name"], "Renamed Application");

    // 7. Delete workspace
    let res_del = server
        .handle_request(&HttpRequest {
            method: "DELETE".into(),
            path: format!("/api/v1/workspaces/{ws_id}"),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_del.status, 200);

    // 8. Delete non-existent -> 404
    let res_del_missing = server
        .handle_request(&HttpRequest {
            method: "DELETE".into(),
            path: format!("/api/v1/workspaces/{ws_id}"),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_del_missing.status, 404);
}

#[tokio::test]
async fn test_http_models_catalog_and_session_export() {
    let server = HttpServer::in_memory().unwrap();
    // The catalog routes project `config.toml`, so inject a deterministic
    // one instead of discovering the developer's config.
    let config: crate::config::KimiConfig = r#"
default_model = "kimi-latest"

[providers.kimi]
type = "openai"
api_key = "sk-test"
base_url = "https://example.test/v1"

[models.kimi-latest]
provider = "kimi"
model = "kimi-latest"
max_context_size = 262144

[models.fast]
provider = "kimi"
model = "k3-fast"
max_context_size = 128000
"#
    .parse()
    .unwrap();
    *server.config_override.lock().await = Some(config);

    // 1. Models catalog (config-driven, v2 `IModelCatalog.listModels`)
    let res_models = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/models".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_models.status, 200);
    let val_models: Value = serde_json::from_slice(&res_models.body).unwrap();
    let items = val_models["items"].as_array().unwrap();
    let model = items
        .iter()
        .find(|item| item["model"] == "kimi-latest")
        .unwrap();
    assert_eq!(model["provider"], "kimi");
    assert_eq!(model["display_name"], "kimi-latest");
    assert_eq!(model["max_context_size"], 262144);

    // 2. Session export
    server
        .store_arc()
        .create_session("sess-to-export", Some("To Export"))
        .unwrap();
    server
        .store_arc()
        .save_turn(
            "sess-to-export",
            "turn-1",
            1,
            &[crate::turn_loop::types::LLMMessage::user(
                "Please export me",
            )],
            None,
            None,
        )
        .unwrap();

    let res_exp_get = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-to-export/export".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_exp_get.status, 200);
    let val_exp: Value = serde_json::from_slice(&res_exp_get.body).unwrap();
    assert_eq!(val_exp["session"]["session_id"], "sess-to-export");
    assert_eq!(val_exp["turns_count"], 1);
    assert_eq!(val_exp["messages"].as_array().unwrap().len(), 1);
    assert!(val_exp["exported_at"].is_string());

    // POST export works symmetrically
    let res_exp_post = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/sessions/sess-to-export/export".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_exp_post.status, 200);

    // Export non-existent returns 404
    let res_exp_none = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-non-existent/export".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_exp_none.status, 404);

    // 3. Providers listing (config-driven, credential state included)
    let res_providers = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/providers".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_providers.status, 200);
    let val_prov: Value = serde_json::from_slice(&res_providers.body).unwrap();
    let providers = val_prov["items"].as_array().unwrap();
    let kimi = providers.iter().find(|item| item["id"] == "kimi").unwrap();
    assert_eq!(kimi["type"], "openai");
    assert_eq!(kimi["has_api_key"], true);
    assert_eq!(kimi["status"], "connected");
    assert_eq!(kimi["default_model"], "kimi-latest");
    let kimi_models = kimi["models"].as_array().unwrap();
    assert_eq!(kimi_models.len(), 2);
    assert!(kimi_models.contains(&json!("kimi-latest")));
    assert!(kimi_models.contains(&json!("fast")));

    // 3b. Auth readiness reflects the injected config.
    let res_auth = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/auth".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_auth.status, 200);
    let val_auth: Value = serde_json::from_slice(&res_auth.body).unwrap();
    assert_eq!(val_auth["models_ready"], true);
    assert_eq!(val_auth["providers_count"], 1);
    assert_eq!(val_auth["managed_provider"], Value::Null);

    // 4. Catalog providers listing
    let res_catalog_prov = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/catalog/providers".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_catalog_prov.status, 200);
    let val_cat_prov: Value = serde_json::from_slice(&res_catalog_prov.body).unwrap();
    assert!(val_cat_prov["items"].as_array().unwrap().len() >= 4);

    // 5. Set default model via POST /api/v1/models/{tail}
    let res_set_default = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/models/fast:set_default".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_set_default.status, 200);
    let val_set_def: Value = serde_json::from_slice(&res_set_default.body).unwrap();
    assert_eq!(val_set_def["model"], "fast");

    // 6. Prompts list
    let res_prompts = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/prompts".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_prompts.status, 200);

    // 7. API v2 sessions
    let res_v2_sess = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v2/sessions".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_v2_sess.status, 200);
    let val_v2: Value = serde_json::from_slice(&res_v2_sess.body).unwrap();
    assert!(val_v2["items"].as_array().is_some());

    // 8. Session init: POST /api/v1/sessions/:id:init
    let res_init = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/sessions/sess-to-export:init".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_init.status, 200);
    let val_init: Value = serde_json::from_slice(&res_init.body).unwrap();
    assert_eq!(val_init["status"], "initiated");
    assert!(val_init["prompt"].as_str().unwrap().contains("AGENTS.md"));
}
