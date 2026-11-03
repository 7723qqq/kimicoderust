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
      
revision: b2c89a8fe8d80ce9b97a5f08206c20f28072e4b0
updated_at: "2026-11-02T18:44:18.346Z"
fingerprint: 49e78657c62ec8ea9f9a75d015ee5c0b027dcbb737cf62e2057305e1c2bc9e46
source:
  - path: "packages/kimi-agent/src/server/mod.rs"
  - path: "packages/kimi-agent/src/server/hub.rs"
deps:
  - kind: dataflow
    to: kimi-code.engine.core
    label: {zh: "会话与事件状态", en: "session/event state"}
---
