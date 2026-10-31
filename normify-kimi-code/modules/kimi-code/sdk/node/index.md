---
uid: "50e00001"
id: kimi-code.sdk.node
parent: kimi-code.sdk
name: {zh: "Node SDK", en: "Node SDK"}
description:
  zh: >
      宿主侧 SDK：原生 RPC 客户端、会话门面、插件/技能/MCP 门面与配置解析。
      
  en: >
      The host-side SDK: native RPC client, session facade, plugin/skill/MCP facades and config resolution.
      
revision: 50f704d9e74f1addcd8ed2ea7edff61672b2b1e2
updated_at: "2026-10-31T16:09:06.717Z"
fingerprint: e0c4b32b6eb99780a802863b21e0f4af06748d608f0aedc90a904eb4224a940b
source:
  - path: "packages/node-sdk/src/native/sdk-rpc-client-native.ts"
  - path: "packages/node-sdk/src/kimi-harness.ts"
deps:
  - kind: call
    to: kimi-code.engine.napi
    label: {zh: "进程内 napi 调用", en: "in-process napi calls"}
---
