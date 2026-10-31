---
uid: e0e00007
id: kimi-code.engine.napi
parent: kimi-code.engine
name: {zh: "NAPI 绑定", en: "NAPI bindings"}
description:
  zh: >
      引擎的 JS 边界：napi 导出（会话创建、插件注册表、工具回调）与 JS 侧类型契约。
      
  en: >
      The engine's JS boundary: napi exports (session creation, plugin registry, tool callbacks) and the JS-side type contract.
      
revision: b5bf884dce712b7fb6a56859dac2cca05d87b18a
updated_at: "2026-10-31T12:58:11.654Z"
fingerprint: a44eab662a301afe7b2896ae5d91afb39d3414bf8af2b446c2dd89434bb0e666
source:
  - path: "packages/kimi-agent/src/napi_bindings.rs"
  - path: "packages/kimi-agent/index.native.d.ts"
deps:
  - kind: call
    to: kimi-code.engine.core
    label: {zh: "同 crate 引擎核心", en: "same-crate engine core"}
  - kind: call
    to: kimi-code.engine.mcp
    label: {zh: "插件 MCP 配置", en: "plugin MCP configs"}
  - kind: call
    to: kimi-code.engine.plugins
    label: {zh: "注册表插件 API", en: "registry plugin APIs"}
---
