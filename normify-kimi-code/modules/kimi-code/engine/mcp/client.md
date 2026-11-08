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
      
revision: 3b18eb137f8d3057962e2f89ee9e34901ced8df4
updated_at: "2026-11-07T00:59:05.829Z"
fingerprint: 08cf9acce52dc39cfb7420eebb60235c4d656e0da72e367ef20b6531d2fa06b8
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
