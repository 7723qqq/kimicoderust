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
      
revision: b2c89a8fe8d80ce9b97a5f08206c20f28072e4b0
updated_at: "2026-11-02T00:33:59.541Z"
fingerprint: faa16a2612b916426e0e0f338ac7591d2f2942281257202d618e098307a8b290
source:
  - path: "packages/kimi-agent/index.native.d.ts"
  - path: "packages/kimi-agent/src/napi_bindings/callbacks.rs"
  - path: "packages/kimi-agent/src/napi_bindings/mcp.rs"
  - path: "packages/kimi-agent/src/napi_bindings/misc.rs"
  - path: "packages/kimi-agent/src/napi_bindings/mod.rs"
  - path: "packages/kimi-agent/src/napi_bindings/params.rs"
  - path: "packages/kimi-agent/src/napi_bindings/plugins.rs"
  - path: "packages/kimi-agent/src/napi_bindings/session.rs"
  - path: "packages/kimi-agent/src/napi_bindings/tests.rs"
  - path: "packages/kimi-agent/src/napi_bindings/turn.rs"
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
