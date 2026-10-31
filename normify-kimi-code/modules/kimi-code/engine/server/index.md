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
      
revision: ed0c196e2ff7c4775c89a3d9294cfc0200e39103
updated_at: "2026-10-31T17:31:01.937Z"
fingerprint: b1dacc4f29e1a5004739eecf98704ff03a6fda1f0368d93d7887824d49e39986
source:
  - path: "packages/kimi-agent/src/server/mod.rs"
  - path: "packages/kimi-agent/src/server/hub.rs"
deps:
  - kind: dataflow
    to: kimi-code.engine.core
    label: {zh: "会话与事件状态", en: "session/event state"}
---
