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
      
revision: 2a1e7db68658f9ff1010d85502486526ccc37969
updated_at: "2026-10-31T14:35:12.979Z"
fingerprint: e2b78240e834f037cbf491a40e5349266c0f54189a71df4ee7ed75be53c80dbe
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
