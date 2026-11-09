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
      
revision: 8c50a0dfdddde7e560ee7637ad3b533551547ccc
updated_at: "2026-11-09T19:52:51.316Z"
fingerprint: 37a3c9c15c38bef450d2192002eee248964450c061fb79b315430e6e95136318
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
