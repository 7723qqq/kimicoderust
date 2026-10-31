use super::*;

#[test]
fn test_run_turn_params_serialization() {
    let json = serde_json::json!({
        "turn_id": "turn-1",
        "system_prompt": "You are a helpful assistant.",
        "model_name": "gpt-4",
        "messages": [
            {"role": "user", "content": "Hello"}
        ],
        "tools": [
            {"name": "read", "description": "Read a file", "input_schema": {"type": "object"}}
        ],
        "max_steps": 10
    });
    let params: RunTurnParams = serde_json::from_value(json).unwrap();
    assert_eq!(params.turn_id, "turn-1");
    assert_eq!(params.model_name, "gpt-4");
    assert_eq!(params.messages.len(), 1);
    assert_eq!(params.tools.len(), 1);
    assert_eq!(params.max_steps, Some(10));
    assert!(params.providers.is_empty());
}

#[test]
fn test_run_turn_params_with_providers() {
    let json = serde_json::json!({
        "turn_id": "turn-1",
        "system_prompt": "",
        "model_name": "",
        "messages": [],
        "tools": [],
        "providers": [
            {"name": "fast", "model": "gpt-4o-mini", "system_prompt": "You are fast."},
            {"name": "smart", "model": "claude-opus-4", "system_prompt": "You are smart."}
        ]
    });
    let params: RunTurnParams = serde_json::from_value(json).unwrap();
    assert_eq!(params.providers.len(), 2);
    assert_eq!(params.providers[0].name, "fast");
    assert_eq!(params.providers[0].model, "gpt-4o-mini");
    assert_eq!(params.providers[1].name, "smart");
}

/// Contract fixture (M0, see the M0 bullet in `ROADMAP.md`):
/// pins the stdio wire shape of the message contract — snake_case field
/// names, `ContentBlock` tag names, optional-field omission. The TS side
/// (`rust-loop.ts`) must produce exactly these shapes; a round-trip
/// mismatch here is a wire break, not a refactor.
#[test]
fn test_message_wire_contract_roundtrip() {
    let fixture = serde_json::json!({
        "role": "user",
        "content": "fallback text",
        "blocks": [
            {"type": "text", "text": "hello"},
            {"type": "image", "media_type": "image/png", "data": "aGk="},
            {"type": "image_url", "url": "https://example.com/a.png"},
            {"type": "audio_url", "url": "https://example.com/a.mp3", "id": "aud-1"},
            {"type": "video_url", "url": "https://example.com/a.mp4"}
        ],
        "tool_calls": [
            {"id": "call-1", "name": "Read", "arguments": {"path": "a.rs"}}
        ],
        "tool_call_id": "call-0"
    });
    let message: Message = serde_json::from_value(fixture.clone()).unwrap();
    assert_eq!(message.role, "user");
    assert_eq!(message.blocks.len(), 5);
    assert_eq!(message.tool_calls.len(), 1);
    let reserialized = serde_json::to_value(&message).unwrap();
    assert_eq!(reserialized, fixture, "wire round-trip must be identity");

    // Optional fields are omitted from the wire when absent.
    let bare: Message = serde_json::from_value(serde_json::json!({
        "role": "assistant", "content": "hi"
    }))
    .unwrap();
    let wire = serde_json::to_value(&bare).unwrap();
    assert_eq!(
        wire,
        serde_json::json!({"role": "assistant", "content": "hi"})
    );

    // TokenUsage wire field names (drifted from kosong's camelCase on
    // purpose — the mapping lives in rust-loop.ts).
    let usage: TokenUsage = serde_json::from_value(serde_json::json!({
        "input_tokens": 1, "output_tokens": 2, "total_tokens": 3,
        "input_cache_read": 4, "input_cache_creation": 5
    }))
    .unwrap();
    assert_eq!(
        serde_json::to_value(&usage).unwrap(),
        serde_json::json!({
            "input_tokens": 1, "output_tokens": 2, "total_tokens": 3,
            "input_cache_read": 4, "input_cache_creation": 5
        })
    );
}

/// Stdio wire contract for the request/response types crossing the
/// JSON-RPC boundary (M0 slice 3): snake_case field names, serde
/// `default` / `skip_serializing_if` behavior pinned. The TS side
/// (`rust-loop.ts` + the fork's own `wire-schema.ts` fixture) mirrors
/// these; a round-trip
/// mismatch here is a wire break, not a refactor.
#[test]
fn test_stdio_wire_contract_roundtrip() {
    // LlmChatRequest: request_id serializes only when present.
    let request = LlmChatRequest {
        system_prompt: "sp".into(),
        model_name: "m".into(),
        messages: vec![
            LlmChatMessage {
                role: "user".into(),
                content: "hi".into(),
                blocks: Vec::new(),
            },
            LlmChatMessage {
                role: "user".into(),
                content: "see".into(),
                blocks: vec![ContentBlock::ImageUrl {
                    url: "https://example.com/a.png".into(),
                    id: None,
                    name: None,
                }],
            },
        ],
        tools: vec![ToolDef {
            name: "read".into(),
            description: "d".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }],
        request_id: Some("req-1".into()),
    };
    let wire = serde_json::to_value(&request).unwrap();
    assert_eq!(wire["request_id"], "req-1");
    assert_eq!(wire["messages"][1]["blocks"][0]["type"], "image_url");
    let round: LlmChatRequest = serde_json::from_value(wire).unwrap();
    assert_eq!(round.request_id.as_deref(), Some("req-1"));
    let mut no_id = request.clone();
    no_id.request_id = None;
    let wire = serde_json::to_value(&no_id).unwrap();
    assert!(
        wire.get("request_id").is_none(),
        "absent request_id must be omitted"
    );

    // LlmChatResponse: `content` defaults on deserialize; finish_reason
    // is null when absent.
    let response: LlmChatResponse = serde_json::from_value(serde_json::json!({
        "tool_calls": [{"id": "c1", "name": "Read", "arguments": {"path": "a"}}],
        "usage": {"input_tokens": 1, "output_tokens": 2, "total_tokens": 3}
    }))
    .unwrap();
    assert_eq!(response.content, "");
    assert_eq!(response.finish_reason, None);
    assert_eq!(response.tool_calls.len(), 1);

    // ToolExecuteRequest / Response.
    let exec = ToolExecuteRequest {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        turn_id: "t".into(),
        tool_call_id: "c1".into(),
        tool_name: "Read".into(),
        arguments: serde_json::json!({"path": "a"}),
    };
    assert_eq!(
        serde_json::to_value(&exec).unwrap(),
        serde_json::json!({
            "agent_id": "main",
            "turn_id": "t", "tool_call_id": "c1",
            "tool_name": "Read", "arguments": {"path": "a"}
        })
    );
    let exec_resp = ToolExecuteResponse {
        delivery: None,
        stop_turn: false,
        content: "out".into(),
        is_error: false,
        note: None,
    };
    let wire = serde_json::to_value(&exec_resp).unwrap();
    assert_eq!(
        wire["note"],
        serde_json::Value::Null,
        "absent note serializes as null"
    );
    let round: ToolExecuteResponse = serde_json::from_value(
        serde_json::json!({"content": "out", "is_error": true, "note": "n"}),
    )
    .unwrap();
    assert!(round.is_error);
    assert_eq!(round.note.as_deref(), Some("n"));

    // PermissionCheckRequest / Decision.
    let perm = PermissionCheckRequest {
        tool_name: "Write".into(),
        tool_call_id: "c2".into(),
        turn_id: "turn-1".into(),
        arguments: serde_json::json!({"path": "a"}),
        reason: None,
        session_approval_rule: None,
    };
    assert_eq!(serde_json::to_value(&perm).unwrap()["tool_name"], "Write");
    // A producer that carries no turn id still deserializes; the approval
    // prompt then falls back to the raw tool-call id.
    let bare: PermissionCheckRequest = serde_json::from_value(serde_json::json!({
        "tool_name": "Write", "tool_call_id": "c2", "arguments": {}
    }))
    .unwrap();
    assert!(bare.turn_id.is_empty());
    let decision = PermissionDecision::deny("no");
    assert_eq!(
        serde_json::to_value(&decision).unwrap(),
        serde_json::json!({"decision": "deny", "reason": "no"})
    );

    // RunTurnParams: every field round-trips; serde defaults fill the
    // optional ones the TS side may omit.
    let params: RunTurnParams = serde_json::from_value(serde_json::json!({
        "turn_id": "t1",
        "system_prompt": "sp",
        "model_name": "m",
        "messages": [{"role": "user", "content": "hi"}],
        "tools": [],
        "max_steps": 3,
        "max_context_tokens": 262144,
        "github_token": "tok",
        "github_base_url": "https://github.example.com"
    }))
    .unwrap();
    assert_eq!(params.max_steps, Some(3));
    assert_eq!(
        params.max_context_tokens,
        Some(262_144),
        "P63: the stdio wire must carry the host window, or that transport keeps the 128k default"
    );
    assert!(params.providers.is_empty());
    assert!(!params.native_tools);
    assert!(!params.rust_self_contained);
    assert_eq!(params.github_token.as_deref(), Some("tok"));
    assert!(params.goal.is_none());

    // RunTurnResult: serde-default fields serialize (never omitted).
    let result = RunTurnResult {
        stop_reason: "EndTurn".into(),
        steps: 2,
        usage: TokenUsage::default(),
        events_emitted: 0,
        llm_retries: 0,
        llm_transport: "host-proxy".into(),
        native_tool_calls: 0,
    };
    let wire = serde_json::to_value(&result).unwrap();
    assert_eq!(wire["events_emitted"], 0);
    assert_eq!(wire["llm_transport"], "host-proxy");
    let round: RunTurnResult = serde_json::from_value(serde_json::json!({
        "stop_reason": "Aborted", "steps": 1,
        "usage": {"input_tokens": 0, "output_tokens": 0, "total_tokens": 0}
    }))
    .unwrap();
    assert_eq!(round.events_emitted, 0);
    assert_eq!(round.llm_transport, "");
}

#[test]
fn test_run_turn_result_roundtrip() {
    let result = RunTurnResult {
        stop_reason: "EndTurn".to_string(),
        steps: 3,
        usage: TokenUsage {
            input_tokens: 100,
            output_tokens: 50,
            total_tokens: 150,
            ..Default::default()
        },
        events_emitted: 7,
        llm_retries: 2,
        llm_transport: "native-http".to_string(),
        native_tool_calls: 4,
    };
    let json = serde_json::to_value(&result).unwrap();
    assert_eq!(json["stop_reason"], "EndTurn");
    assert_eq!(json["steps"], 3);
    assert_eq!(json["usage"]["input_tokens"], 100);
    assert_eq!(json["events_emitted"], 7);
    assert_eq!(json["llm_retries"], 2);
    assert_eq!(json["llm_transport"], "native-http");
    assert_eq!(json["native_tool_calls"], 4);

    let deserialized: RunTurnResult = serde_json::from_value(json).unwrap();
    assert_eq!(deserialized.stop_reason, "EndTurn");
    assert_eq!(deserialized.steps, 3);
    assert_eq!(deserialized.events_emitted, 7);
    assert_eq!(deserialized.llm_retries, 2);
}

/// Older adapters omit the telemetry counters; serde defaults keep the
/// wire backward-compatible.
#[test]
fn test_run_turn_result_defaults_without_counters() {
    let deserialized: RunTurnResult = serde_json::from_value(serde_json::json!({
        "stop_reason": "EndTurn",
        "steps": 1,
        "usage": { "input_tokens": 0, "output_tokens": 0, "total_tokens": 0 },
    }))
    .unwrap();
    assert_eq!(deserialized.events_emitted, 0);
    assert_eq!(deserialized.llm_retries, 0);
    assert_eq!(deserialized.llm_transport, "");
    assert_eq!(deserialized.native_tool_calls, 0);
}

#[test]
fn test_llm_chat_request_roundtrip() {
    let req = LlmChatRequest {
        system_prompt: "You are helpful.".to_string(),
        model_name: "gpt-4".to_string(),
        messages: vec![LlmChatMessage {
            role: "user".to_string(),
            content: "Hi".to_string(),
            blocks: Vec::new(),
        }],
        tools: vec![ToolDef {
            name: "read".to_string(),
            description: "Read file".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
        }],
        request_id: None,
    };
    let json = serde_json::to_value(&req).unwrap();
    assert_eq!(json["system_prompt"], "You are helpful.");
    assert_eq!(json["messages"][0]["role"], "user");
    // Absent rather than null: the single-provider path has nothing to cancel.
    assert!(json.get("request_id").is_none());

    let deserialized: LlmChatRequest = serde_json::from_value(json).unwrap();
    assert_eq!(deserialized.system_prompt, req.system_prompt);
    assert_eq!(deserialized.messages.len(), 1);
    assert_eq!(deserialized.tools.len(), 1);
}

#[test]
fn test_llm_chat_request_carries_request_id() {
    let req = LlmChatRequest {
        system_prompt: String::new(),
        model_name: "fast".to_string(),
        messages: vec![],
        tools: vec![],
        request_id: Some("llm-slow-7".to_string()),
    };
    let json = serde_json::to_value(&req).unwrap();
    assert_eq!(json["request_id"], "llm-slow-7");

    let deserialized: LlmChatRequest = serde_json::from_value(json).unwrap();
    assert_eq!(deserialized.request_id.as_deref(), Some("llm-slow-7"));
}

#[test]
fn test_llm_chat_response_roundtrip() {
    let resp = LlmChatResponse {
        content: String::new(),
        tool_calls: vec![LlmToolCall {
            id: "call_1".to_string(),
            name: "read".to_string(),
            arguments: serde_json::json!({"path": "/tmp/test.txt"}),
            extras: Some(serde_json::json!({"thought_signature_b64": "sig-x"})),
        }],
        thinking: vec![ContentBlock::Think {
            think: "checking the path".to_string(),
            encrypted: None,
            details_index: None,
            reasoning_key: None,
            details_summary: None,
        }],
        finish_reason: Some("stop".to_string()),
        usage: TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            total_tokens: 15,
            ..Default::default()
        },
    };
    let json = serde_json::to_value(&resp).unwrap();
    assert_eq!(json["tool_calls"][0]["name"], "read");
    assert_eq!(
        json["tool_calls"][0]["extras"]["thought_signature_b64"],
        "sig-x"
    );
    assert_eq!(json["thinking"][0]["think"], "checking the path");
    assert_eq!(json["finish_reason"], "stop");

    let deserialized: LlmChatResponse = serde_json::from_value(json).unwrap();
    assert_eq!(deserialized.tool_calls.len(), 1);
    assert_eq!(deserialized.tool_calls[0].id, "call_1");
    assert_eq!(
        deserialized.tool_calls[0].extras.as_ref().unwrap()["thought_signature_b64"],
        "sig-x"
    );
    assert_eq!(deserialized.thinking.len(), 1);
    assert_eq!(deserialized.finish_reason, Some("stop".to_string()));

    // Old hosts that never send thinking must still deserialize.
    let legacy = serde_json::json!({
        "content": "hi",
        "tool_calls": [],
        "finish_reason": "stop",
        "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2,
                  "input_cache_read": 0, "input_cache_creation": 0}
    });
    let legacy_resp: LlmChatResponse = serde_json::from_value(legacy).unwrap();
    assert!(legacy_resp.thinking.is_empty());
}

#[test]
fn test_tool_execute_request_roundtrip() {
    let req = ToolExecuteRequest {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        turn_id: "turn-1".to_string(),
        tool_call_id: "call_1".to_string(),
        tool_name: "read".to_string(),
        arguments: serde_json::json!({"path": "/tmp/test.txt", "line_offset": 1}),
    };
    let json = serde_json::to_value(&req).unwrap();
    assert_eq!(json["tool_name"], "read");
    assert_eq!(json["arguments"]["path"], "/tmp/test.txt");

    let deserialized: ToolExecuteRequest = serde_json::from_value(json).unwrap();
    assert_eq!(deserialized.turn_id, "turn-1");
    assert_eq!(deserialized.tool_name, "read");
}

#[test]
fn test_tool_execute_request_defaults() {
    // force_precise was removed with the prediction framework; the
    // legacy field must now deserialize as absent without error.
    let json = serde_json::json!({
        "turn_id": "turn-1",
        "tool_call_id": "call_1",
        "tool_name": "read",
        "arguments": {"path": "/tmp/test.txt"}
    });
    let req: ToolExecuteRequest = serde_json::from_value(json).unwrap();
    assert_eq!(req.turn_id, "turn-1");
}

#[test]
fn test_tool_execute_response_roundtrip() {
    let resp = ToolExecuteResponse {
        delivery: None,
        stop_turn: false,
        content: "file content here".to_string(),
        is_error: false,
        note: Some("<system>1 line read.</system>".to_string()),
    };
    let json = serde_json::to_value(&resp).unwrap();
    assert_eq!(json["content"], "file content here");
    assert!(!json["is_error"].as_bool().unwrap());
    assert_eq!(json["note"], "<system>1 line read.</system>");

    let deserialized: ToolExecuteResponse = serde_json::from_value(json).unwrap();
    assert_eq!(deserialized.content, "file content here");
    assert!(!deserialized.is_error);
    assert_eq!(
        deserialized.note.as_deref(),
        Some("<system>1 line read.</system>")
    );
}

#[test]
fn test_tool_execute_response_error() {
    let resp = ToolExecuteResponse {
        delivery: None,
        stop_turn: false,
        content: "File not found".to_string(),
        is_error: true,
        note: None,
    };
    let json = serde_json::to_value(&resp).unwrap();
    assert!(json["is_error"].as_bool().unwrap());

    let deserialized: ToolExecuteResponse = serde_json::from_value(json).unwrap();
    assert!(deserialized.is_error);
}

#[test]
fn test_health_status() {
    let status = HealthStatus {
        status: "ok".to_string(),
        version: "0.1.0".to_string(),
    };
    let json = serde_json::to_value(&status).unwrap();
    assert_eq!(json["status"], "ok");
    assert_eq!(json["version"], "0.1.0");
}

#[test]
fn test_message_roundtrip() {
    let msg = Message {
        role: "user".to_string(),
        content: "Hello world".to_string(),
        blocks: Vec::new(),
        tool_calls: Vec::new(),
        tool_call_id: None,
    };
    let json = serde_json::to_value(&msg).unwrap();
    assert_eq!(json["role"], "user");

    let deserialized: Message = serde_json::from_value(json).unwrap();
    assert_eq!(deserialized.role, "user");
    assert_eq!(deserialized.content, "Hello world");
}

#[test]
fn test_tool_def_roundtrip() {
    let def = ToolDef {
        name: "grep".to_string(),
        description: "Search text".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string"}
            }
        }),
    };
    let json = serde_json::to_value(&def).unwrap();
    let deserialized: ToolDef = serde_json::from_value(json).unwrap();
    assert_eq!(deserialized.name, "grep");
    assert!(
        deserialized.input_schema["properties"]["pattern"]["type"]
            .as_str()
            .is_some()
    );
}

#[test]
fn test_token_usage_default() {
    let usage = TokenUsage::default();
    assert_eq!(usage.input_tokens, 0);
    assert_eq!(usage.output_tokens, 0);
    assert_eq!(usage.total_tokens, 0);
}

#[test]
fn test_token_usage_roundtrip() {
    let usage = TokenUsage {
        input_tokens: 100,
        output_tokens: 50,
        total_tokens: 150,
        ..Default::default()
    };
    let json = serde_json::to_value(&usage).unwrap();
    assert_eq!(json["input_tokens"], 100);
    assert_eq!(json["output_tokens"], 50);
    assert_eq!(json["total_tokens"], 150);

    let deserialized: TokenUsage = serde_json::from_value(json).unwrap();
    assert_eq!(deserialized.input_tokens, 100);
    assert_eq!(deserialized.output_tokens, 50);
}

#[test]
fn test_json_rpc_request_parse() {
    let json = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 42,
        "method": "agent/run_turn",
        "params": {"key": "value"}
    });
    let req: JsonRpcRequest = serde_json::from_value(json).unwrap();
    assert_eq!(req.jsonrpc, "2.0");
    assert_eq!(req.id, 42);
    assert_eq!(req.method, "agent/run_turn");
    assert_eq!(req.params["key"], "value");
}

#[test]
fn test_json_rpc_request_notification() {
    // Notification has no id
    let json = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "agent/notify",
        "params": {}
    });
    let req: JsonRpcRequest = serde_json::from_value(json).unwrap();
    assert!(req.id.is_null());
}

#[test]
fn test_json_rpc_response_ok() {
    let resp = JsonRpcResponse::ok(serde_json::json!(1), serde_json::json!({"result": "ok"}));
    let json = serde_json::to_value(&resp).unwrap();
    assert_eq!(json["jsonrpc"], "2.0");
    assert_eq!(json["id"], 1);
    assert_eq!(json["result"]["result"], "ok");
}

#[test]
fn test_json_rpc_error_response() {
    let err = JsonRpcErrorResponse::new(serde_json::json!(null), -32700, "Parse error".to_string());
    let json = serde_json::to_value(&err).unwrap();
    assert_eq!(json["error"]["code"], -32700);
    assert_eq!(json["error"]["message"], "Parse error");
    assert!(json["error"].get("data").is_none());
}

#[test]
fn test_json_rpc_error_with_data() {
    let err = JsonRpcError {
        code: -32000,
        message: "Custom error".to_string(),
        data: Some(serde_json::json!({"detail": "something broke"})),
    };
    let json = serde_json::to_value(&err).unwrap();
    assert_eq!(json["code"], -32000);
    assert_eq!(json["data"]["detail"], "something broke");
}

#[test]
fn test_methods_constants() {
    assert_eq!(methods::RUN_TURN, "agent/run_turn");
    assert_eq!(methods::CANCEL_TURN, "agent/cancel_turn");
    assert_eq!(methods::HEALTH, "agent/health");
    assert_eq!(methods::SHUTDOWN, "agent/shutdown");
    assert_eq!(methods::HOST_LLM_CHAT, "host/llm_chat");
    assert_eq!(methods::HOST_EXECUTE_TOOL, "host/execute_tool");
    assert_eq!(methods::HOST_ASK_QUESTION, "host/ask_question");
    assert_eq!(methods::HOST_STATE_READ, "host/state_read");
    assert_eq!(methods::HOST_STATE_WRITE, "host/state_write");
    assert_eq!(methods::HOST_AUTH_TOKEN, "host/auth_token");
}

#[test]
fn test_native_llm_config_oauth_mode() {
    let json = serde_json::json!({
        "protocol": "openai",
        "base_url": "https://api.example.com/v1",
        "api_key": "",
        "model": "kimi-latest",
        "auth_provider": "kimi"
    });
    let cfg: NativeLlmConfig = serde_json::from_value(json).unwrap();
    assert_eq!(cfg.auth_provider.as_deref(), Some("kimi"));
    assert!(cfg.api_key.is_empty());

    // The Debug impl redacts the key even when auth_provider is set.
    let mut with_key = cfg.clone();
    with_key.api_key = "super-secret".into();
    let debug = format!("{with_key:?}");
    assert!(
        !debug.contains("super-secret"),
        "debug must not leak the key"
    );
    assert!(
        debug.contains("auth_provider"),
        "debug carries the auth mode"
    );
}

#[test]
fn test_llm_provider_def_deserialize() {
    let json = serde_json::json!({
        "name": "fast-llm",
        "model": "gpt-4o-mini",
        "system_prompt": "You are fast."
    });
    let def: LlmProviderDef = serde_json::from_value(json).unwrap();
    assert_eq!(def.name, "fast-llm");
    assert_eq!(def.model, "gpt-4o-mini");
    assert_eq!(def.system_prompt, "You are fast.");
}

#[test]
fn test_notification_parse() {
    let json = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "host/event",
        "params": {"type": "progress"}
    });
    let notif: JsonRpcNotification = serde_json::from_value(json).unwrap();
    assert_eq!(notif.jsonrpc, "2.0");
    assert_eq!(notif.method, "host/event");
    assert_eq!(notif.params["type"], "progress");
}

#[test]
fn test_llm_tool_call_roundtrip() {
    let tc = LlmToolCall {
        id: "call_abc".to_string(),
        name: "read_file".to_string(),
        arguments: serde_json::json!({"path": "/tmp/x.txt"}),
        extras: None,
    };
    let json = serde_json::to_value(&tc).unwrap();
    let deserialized: LlmToolCall = serde_json::from_value(json).unwrap();
    assert_eq!(deserialized.id, "call_abc");
    assert_eq!(deserialized.name, "read_file");
}

#[test]
fn test_empty_providers_default() {
    let json = serde_json::json!({
        "turn_id": "t1",
        "system_prompt": "hi",
        "model_name": "m",
        "messages": [],
        "tools": []
    });
    let params: RunTurnParams = serde_json::from_value(json).unwrap();
    assert!(params.providers.is_empty());
}

#[test]
fn test_tool_def_empty_schema() {
    let def = ToolDef {
        name: "bash".to_string(),
        description: "Run shell".to_string(),
        input_schema: serde_json::Value::Object(Default::default()),
    };
    let json = serde_json::to_value(&def).unwrap();
    let deserialized: ToolDef = serde_json::from_value(json).unwrap();
    assert_eq!(deserialized.name, "bash");
    assert!(deserialized.input_schema.as_object().unwrap().is_empty());
}

#[test]
fn test_ask_question_request_roundtrip() {
    let req = AskQuestionRequest {
        question_id: "question_9f2c".to_string(),
        turn_id: "turn-42".to_string(),
        tool_call_id: "call_abc".to_string(),
        background: false,
        timeout_ms: None,
        questions: vec![AskQuestionItem {
            question: "Which approach should I take?".to_string(),
            header: Some("Style".to_string()),
            options: vec![
                AskQuestionOption {
                    label: "Option A (Recommended)".to_string(),
                    description: Some("Fast, less flexible".to_string()),
                },
                AskQuestionOption {
                    label: "Option B".to_string(),
                    description: None,
                },
            ],
            multi_select: false,
        }],
    };
    let json = serde_json::to_value(&req).unwrap();
    assert_eq!(json["question_id"], "question_9f2c");
    assert_eq!(json["turn_id"], "turn-42");
    assert_eq!(json["tool_call_id"], "call_abc");
    assert_eq!(json["background"], false);
    assert!(json["timeout_ms"].is_null());
    assert_eq!(
        json["questions"][0]["question"],
        "Which approach should I take?"
    );
    assert_eq!(json["questions"][0]["header"], "Style");
    assert_eq!(
        json["questions"][0]["options"][0]["label"],
        "Option A (Recommended)"
    );
    assert_eq!(
        json["questions"][0]["options"][0]["description"],
        "Fast, less flexible"
    );
    assert!(json["questions"][0]["options"][1]["description"].is_null());
    assert_eq!(json["questions"][0]["multi_select"], false);

    let deserialized: AskQuestionRequest = serde_json::from_value(json).unwrap();
    assert_eq!(deserialized.question_id, req.question_id);
    assert_eq!(deserialized.turn_id, "turn-42");
    assert_eq!(deserialized.questions.len(), 1);
    assert_eq!(deserialized.questions[0].options.len(), 2);
    assert_eq!(deserialized.questions[0].options[1].description, None);
    assert_eq!(deserialized.timeout_ms, None);
}

#[test]
fn test_ask_question_request_background_and_timeout() {
    let req = AskQuestionRequest {
        question_id: "question_1".to_string(),
        turn_id: "turn-1".to_string(),
        tool_call_id: "call_1".to_string(),
        background: true,
        timeout_ms: Some(30_000),
        questions: vec![],
    };
    let json = serde_json::to_value(&req).unwrap();
    assert_eq!(json["background"], true);
    assert_eq!(json["timeout_ms"], 30_000);

    let deserialized: AskQuestionRequest = serde_json::from_value(json).unwrap();
    assert!(deserialized.background);
    assert_eq!(deserialized.timeout_ms, Some(30_000));
}

#[test]
fn test_ask_question_response_answered_roundtrip() {
    let resp = AskQuestionResponse {
        answers: std::collections::HashMap::from([(
            "Which approach should I take?".to_string(),
            "Option A (Recommended)".to_string(),
        )]),
        method: Some("enter".to_string()),
        note: None,
        cancelled: None,
        reason: None,
    };
    let json = serde_json::to_value(&resp).unwrap();
    assert_eq!(
        json["answers"]["Which approach should I take?"],
        "Option A (Recommended)"
    );
    assert_eq!(json["method"], "enter");

    let deserialized: AskQuestionResponse = serde_json::from_value(json).unwrap();
    assert_eq!(deserialized.answers.len(), 1);
    assert_eq!(
        deserialized
            .answers
            .get("Which approach should I take?")
            .map(String::as_str),
        Some("Option A (Recommended)")
    );
    assert_eq!(deserialized.method.as_deref(), Some("enter"));
    assert_eq!(deserialized.cancelled, None);
    assert_eq!(deserialized.reason, None);
}

#[test]
fn test_ask_question_response_dismissed_and_cancelled() {
    // Dismissed: empty answers + note.
    let dismissed: AskQuestionResponse = serde_json::from_value(serde_json::json!({
        "answers": {},
        "note": "User dismissed the question without answering."
    }))
    .unwrap();
    assert!(dismissed.answers.is_empty());
    assert_eq!(
        dismissed.note.as_deref(),
        Some("User dismissed the question without answering.")
    );
    assert_eq!(dismissed.cancelled, None);

    // Cancelled: {cancelled: true, reason} — answers default to empty.
    let cancelled: AskQuestionResponse = serde_json::from_value(serde_json::json!({
        "cancelled": true,
        "reason": "turn_ended"
    }))
    .unwrap();
    assert_eq!(cancelled.cancelled, Some(true));
    assert_eq!(cancelled.reason.as_deref(), Some("turn_ended"));
    assert!(cancelled.answers.is_empty());
    assert_eq!(cancelled.method, None);
}

#[test]
fn test_state_read_request_roundtrip() {
    let req = StateReadRequest {
        domain: "todo".to_string(),
        key: "todo".to_string(),
        turn_id: "turn-42".to_string(),
        tool_call_id: "call_abc".to_string(),
    };
    let json = serde_json::to_value(&req).unwrap();
    assert_eq!(json["domain"], "todo");
    assert_eq!(json["key"], "todo");
    assert_eq!(json["turn_id"], "turn-42");
    assert_eq!(json["tool_call_id"], "call_abc");

    let deserialized: StateReadRequest = serde_json::from_value(json).unwrap();
    assert_eq!(deserialized.domain, "todo");
    assert_eq!(deserialized.key, "todo");
    assert_eq!(deserialized.turn_id, "turn-42");
    assert_eq!(deserialized.tool_call_id, "call_abc");
}

#[test]
fn test_state_read_request_defaults() {
    // Provenance fields are optional on the wire; serde defaults keep
    // older adapters backward-compatible.
    let req: StateReadRequest = serde_json::from_value(serde_json::json!({
        "domain": "plan",
        "key": "plan",
    }))
    .unwrap();
    assert_eq!(req.domain, "plan");
    assert_eq!(req.key, "plan");
    assert_eq!(req.turn_id, "");
    assert_eq!(req.tool_call_id, "");
}

#[test]
fn test_state_read_response_opaque_value() {
    // `value` is an opaque host-serialized domain wire value; it must
    // round-trip verbatim.
    let resp = StateReadResponse {
        value: serde_json::json!([
            {"id": "T1", "parentId": null, "kind": "task", "title": "Read session-control.ts", "status": "in_progress", "progress": 40},
            {"id": "M1", "parentId": null, "kind": "milestone", "title": "Phase 1", "status": "pending"}
        ]),
    };
    let json = serde_json::to_value(&resp).unwrap();
    assert_eq!(json["value"][0]["id"], "T1");
    assert_eq!(json["value"][0]["progress"], 40);
    assert_eq!(json["value"][1]["kind"], "milestone");

    let deserialized: StateReadResponse = serde_json::from_value(json).unwrap();
    assert_eq!(deserialized.value[0]["title"], "Read session-control.ts");
    assert_eq!(deserialized.value[1]["status"], "pending");
}

#[test]
fn test_state_write_request_roundtrip() {
    let req = StateWriteRequest {
        domain: "todo".to_string(),
        key: "todo".to_string(),
        value: serde_json::json!([
            {"title": "Read session-control.ts", "status": "in_progress"}
        ]),
        undoable: true,
        turn_id: "turn-42".to_string(),
        tool_call_id: "call_abc".to_string(),
    };
    let json = serde_json::to_value(&req).unwrap();
    assert_eq!(json["domain"], "todo");
    assert_eq!(json["undoable"], true);
    assert_eq!(json["value"][0]["title"], "Read session-control.ts");
    assert_eq!(json["turn_id"], "turn-42");
    assert_eq!(json["tool_call_id"], "call_abc");

    let deserialized: StateWriteRequest = serde_json::from_value(json).unwrap();
    assert_eq!(deserialized.domain, "todo");
    assert!(deserialized.undoable);
    assert_eq!(deserialized.value[0]["status"], "in_progress");
    assert_eq!(deserialized.turn_id, "turn-42");
    assert_eq!(deserialized.tool_call_id, "call_abc");
}

#[test]
fn test_state_write_request_defaults() {
    let req: StateWriteRequest = serde_json::from_value(serde_json::json!({
        "domain": "plan",
        "key": "plan",
        "value": {"active": true},
        "undoable": true,
    }))
    .unwrap();
    assert_eq!(req.domain, "plan");
    assert!(req.undoable);
    assert_eq!(req.value["active"], true);
    assert_eq!(req.turn_id, "");
    assert_eq!(req.tool_call_id, "");
}

#[test]
fn test_state_write_response_roundtrip() {
    let resp = StateWriteResponse {
        ok: true,
        value: serde_json::json!({
            "active": true,
            "id": "plan-7f3a",
            "path": "<sessionDir>/agents/agent-1/plans/plan-7f3a.md"
        }),
    };
    let json = serde_json::to_value(&resp).unwrap();
    assert_eq!(json["ok"], true);
    assert_eq!(json["value"]["id"], "plan-7f3a");

    let deserialized: StateWriteResponse = serde_json::from_value(json).unwrap();
    assert!(deserialized.ok);
    assert_eq!(
        deserialized.value["path"],
        "<sessionDir>/agents/agent-1/plans/plan-7f3a.md"
    );
}
