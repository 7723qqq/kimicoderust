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
      
revision: b5bf884dce712b7fb6a56859dac2cca05d87b18a
updated_at: "2026-10-31T15:13:40.702Z"
fingerprint: ba158e02c181dedd057c7324a06cf8fe834bec0500beac49fea4edffce8a0949
source:
  - path: "packages/node-sdk/src/native/sdk-rpc-client-native.ts"
  - path: "packages/node-sdk/src/kimi-harness.ts"
deps:
  - kind: call
    to: kimi-code.engine.napi
    label: {zh: "进程内 napi 调用", en: "in-process napi calls"}
---
