---
uid: e1e0000a
id: kimi-code.engine.server.ws
parent: kimi-code.engine.server
name: {zh: "WS 网关", en: "WS gateway"}
description:
  zh: >
      WS 网关：/api/v1/ws、hub 扇出、协议信封与交互回路。
      
  en: >
      WS gateway: /api/v1/ws, hub fan-out, protocol envelopes and interaction round-trips.
      
revision: c7d34cbe784d29928dada6568fb144fe4bece585
updated_at: "2026-11-01T05:34:46.013Z"
fingerprint: 8257df4806fdb044ce3de583366170a1c86be58f3f73cdc4c882f731b72ff017
source:
  - path: "packages/kimi-agent/src/server/ws.rs"
  - path: "packages/kimi-agent/src/server/ws_protocol.rs"
  - path: "packages/kimi-agent/src/server/hub.rs"
apis:
  - protocol: ws
    path: "/api/v1/ws"
    description:
      zh: >
          WebSocket 事件流
          
      en: >
          WebSocket event stream
          
---
