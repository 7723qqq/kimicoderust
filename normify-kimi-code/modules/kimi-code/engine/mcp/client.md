---
uid: e3e00002
id: kimi-code.engine.mcp.client
parent: kimi-code.engine.mcp
name: {zh: "MCP 客户端", en: "MCP clients"}
description:
  zh: >
      MCP 客户端与传输：stdio 拉起、SSE/HTTP、共享客户端状态、输出规范化与错误。
      
  en: >
      MCP clients and transports: stdio spawn, SSE/HTTP, shared client state, output normalization and errors.
      
revision: f22a80d7ce2cfd1a1e88dff32aa14f9120c909d2
updated_at: "2026-11-11T19:20:38.920Z"
fingerprint: 7151d1b3ffd00f1f8c0c8477935163eea07469d89c57ffb6e14358ead681b787
source:
  - path: "packages/kimi-agent/src/mcp/client.rs"
  - path: "packages/kimi-agent/src/mcp/client_shared.rs"
  - path: "packages/kimi-agent/src/mcp/sse.rs"
  - path: "packages/kimi-agent/src/mcp/http.rs"
  - path: "packages/kimi-agent/src/mcp/errors.rs"
  - path: "packages/kimi-agent/src/mcp/output.rs"
apis: []
deps:
  - kind: call
    to: kimi-code.engine.mcp.manager
    label: {zh: "由管理器拉起", en: "spawned by manager"}
---
