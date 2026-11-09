use super::super::*;
use std::collections::HashMap;

#[test]
fn prompt_content_maps_uploaded_files_to_media_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let files = crate::server::files::FileStore::with_root(dir.path().to_path_buf());
    let png = files
        .save("photo.png", "image/png", None, &[0x89, 0x50, 0x4e, 0x47])
        .unwrap();
    let pdf = files
        .save("doc.pdf", "application/pdf", None, b"%PDF-1.4")
        .unwrap();

    let content = vec![
        json!({ "type": "text", "text": "look at this" }),
        json!({ "type": "image", "source": { "kind": "file", "file_id": png.id } }),
        json!({
            "type": "file",
            "file_id": pdf.id,
            "name": "doc.pdf",
            "media_type": "application/pdf",
            "size": 8,
        }),
    ];
    let (prompt, blocks) = prompt_content_to_blocks(&content, &files, None).unwrap();
    assert_eq!(prompt, "look at this");
    assert_eq!(blocks.len(), 2);
    // The bytes stay in the store: the engine's resolver reads them once
    // it knows which model the turn runs on.
    match &blocks[0] {
        crate::rpc::types::ContentBlock::MediaRef { file_id, kind } => {
            assert_eq!(file_id, &png.id);
            assert_eq!(*kind, crate::rpc::types::MediaKind::Image);
        }
        other => panic!("expected a media reference, got {other:?}"),
    }
    match &blocks[1] {
        crate::rpc::types::ContentBlock::Text { text, .. } => {
            assert!(text.contains("doc.pdf"));
            assert!(text.contains("application/pdf"));
        }
        other => panic!("expected text fallback, got {other:?}"),
    }
}

/// ROADMAP #3747b: an inline base64 image over the model's pixel/byte
/// budget is compressed at intake, the original persisted, and a caption
/// naming both precedes the image — the HTTP path's half of v2's
/// `resolvePromptMediaFiles` (the TUI path does it host-side).
#[test]
fn prompt_content_compresses_an_over_budget_inline_image_with_a_caption() {
    let dir = tempfile::tempdir().unwrap();
    let files = crate::server::files::FileStore::with_root(dir.path().to_path_buf());
    let img = image::RgbImage::from_fn(4000, 3000, |x, y| {
        image::Rgb([(x % 251) as u8, (y % 241) as u8, ((x + y) % 239) as u8])
    });
    let mut png = Vec::new();
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    use base64::Engine as _;
    let data = base64::engine::general_purpose::STANDARD.encode(&png);

    let content = vec![
        json!({ "type": "text", "text": "look at this" }),
        json!({
            "type": "image",
            "name": "shot.png",
            "source": { "kind": "base64", "media_type": "image/png", "data": data },
        }),
    ];
    let (prompt, blocks) = prompt_content_to_blocks(&content, &files, None).unwrap();
    assert_eq!(prompt, "look at this");
    assert_eq!(blocks.len(), 2, "the caption precedes the compressed image");

    match &blocks[0] {
        crate::rpc::types::ContentBlock::Text { text, .. } => {
            assert!(
                text.starts_with(
                    "<system>Image compressed to fit model limits: original 4000x3000,"
                )
            );
            assert!(text.contains("call Read on that path"));
        }
        other => panic!("expected the compression caption, got {other:?}"),
    }
    match &blocks[1] {
        crate::rpc::types::ContentBlock::Image {
            media_type,
            data: sent,
            name,
        } => {
            assert_eq!(name.as_deref(), Some("shot.png"));
            assert!(media_type.starts_with("image/"));
            assert_ne!(sent, &data, "the sent bytes are the compressed ones");
        }
        other => panic!("expected the compressed image, got {other:?}"),
    }
}

#[test]
fn prompt_content_rejects_an_unknown_file_id() {
    let dir = tempfile::tempdir().unwrap();
    let files = crate::server::files::FileStore::with_root(dir.path().to_path_buf());
    let content =
        vec![json!({ "type": "image", "source": { "kind": "file", "file_id": "f_missing" } })];
    assert!(prompt_content_to_blocks(&content, &files, None).is_err());
}

/// `POST /api/v1/prompts` used to mint an id and answer
/// `{"status":"enqueued"}` without touching the queue. It now delegates to
/// the session submit path: missing session / unknown session / no engine
/// all refuse instead of issuing a receipt for a prompt that never runs.
#[tokio::test]
async fn collection_prompt_route_refuses_instead_of_faking_enqueue() {
    let server = HttpServer::in_memory().unwrap();

    let missing_session = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/prompts".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "prompt": "hi" })).unwrap(),
        })
        .await;
    assert_eq!(missing_session.status, 400, "session_id is required");
    assert!(
        !String::from_utf8_lossy(&missing_session.body).contains("enqueued"),
        "the fake enqueue receipt is still being served"
    );

    server
        .store
        .create_session("sess-collection", Some("c"))
        .unwrap();
    let unknown_session = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/prompts".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({
                "session_id": "sess-does-not-exist",
                "prompt": "hi",
            }))
            .unwrap(),
        })
        .await;
    assert_eq!(unknown_session.status, 404);

    let no_engine = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/prompts".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({
                "session_id": "sess-collection",
                "prompt": "hi",
            }))
            .unwrap(),
        })
        .await;
    assert_eq!(no_engine.status, 503, "no engine attached");

    // Nothing was ever admitted to the queue by the fake path.
    assert!(
        server.prompt_queue.snapshot("sess-collection").0.is_none(),
        "a refused submit must not leave a phantom active prompt"
    );
}
