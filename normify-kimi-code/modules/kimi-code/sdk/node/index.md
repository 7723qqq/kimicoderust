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
      
revision: c7d34cbe784d29928dada6568fb144fe4bece585
updated_at: "2026-11-01T05:34:46.029Z"
fingerprint: f2d905162f86ca3b51dcad055530d4dfaed7b3203cc79d6cde6740b467855c30
source:
  - path: "packages/node-sdk/src/native/sdk-rpc-client-native.ts"
  - path: "packages/node-sdk/src/kimi-harness.ts"
deps:
  - kind: call
    to: kimi-code.engine.napi
    label: {zh: "进程内 napi 调用", en: "in-process napi calls"}
---
