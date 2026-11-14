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
      
revision: f22a80d7ce2cfd1a1e88dff32aa14f9120c909d2
updated_at: "2026-11-11T19:20:38.934Z"
fingerprint: 74550ec898b41b74054a1ec5d0744df638addf0173544dd8ac041e0e6f0b0e81
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
