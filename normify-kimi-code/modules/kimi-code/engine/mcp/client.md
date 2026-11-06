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
      
revision: 28f4b101d79bba811d53e53662341c0bb0fe93c0
updated_at: "2026-11-03T16:01:43.453Z"
fingerprint: 48f7fdc12037862e2c251ce082e9aa9ca7c1d90a530be2dfe139ea3335fc34d8
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
