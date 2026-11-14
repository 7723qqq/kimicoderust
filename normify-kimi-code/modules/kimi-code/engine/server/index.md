---
uid: e0e00005
id: kimi-code.engine.server
parent: kimi-code.engine
name: {zh: "服务器与 WS 网关", en: "Server & WS gateway"}
description:
  zh: >
      独立服务器：REST API、WebSocket 事件流、transcript 投影与会话托管。
      
  en: >
      Standalone server: REST API, WebSocket event stream, transcript projection and session hosting.
      
revision: f22a80d7ce2cfd1a1e88dff32aa14f9120c909d2
updated_at: "2026-11-13T12:24:30.036Z"
fingerprint: 59b66d5a2af0f932c8fdbeee05c0e9a74535550d113dfbfc4b31edce025881e2
source:
  - path: "packages/kimi-agent/src/server/mod.rs"
  - path: "packages/kimi-agent/src/server/tests/mod.rs"
  - path: "packages/kimi-agent/src/server/tests/common.rs"
  - path: "packages/kimi-agent/src/server/tests/core_http.rs"
  - path: "packages/kimi-agent/src/server/tests/cron_tasks.rs"
  - path: "packages/kimi-agent/src/server/tests/events_fs_terminal_subagents.rs"
  - path: "packages/kimi-agent/src/server/tests/files_mcp_fs.rs"
  - path: "packages/kimi-agent/src/server/tests/mcp_auth_transcript.rs"
  - path: "packages/kimi-agent/src/server/tests/meta_config_fork.rs"
  - path: "packages/kimi-agent/src/server/tests/patch_undo_compact.rs"
  - path: "packages/kimi-agent/src/server/tests/plugins_search_oauth.rs"
  - path: "packages/kimi-agent/src/server/tests/profile_fs_content.rs"
  - path: "packages/kimi-agent/src/server/tests/prompt_content.rs"
  - path: "packages/kimi-agent/src/server/tests/prompts_providers.rs"
  - path: "packages/kimi-agent/src/server/tests/security.rs"
  - path: "packages/kimi-agent/src/server/tests/session_actions.rs"
  - path: "packages/kimi-agent/src/server/tests/web_contract_export.rs"
  - path: "packages/kimi-agent/src/server/tests/workspaces_models.rs"
  - path: "packages/kimi-agent/src/server/hub.rs"
deps:
  - kind: dataflow
    to: kimi-code.engine.core
    label: {zh: "会话与事件状态", en: "session/event state"}
---
