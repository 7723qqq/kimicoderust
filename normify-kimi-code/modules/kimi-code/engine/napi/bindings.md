---
uid: e5e00001
id: kimi-code.engine.napi.bindings
parent: kimi-code.engine.napi
name: {zh: "NAPI 导出", en: "NAPI exports"}
description:
  zh: >
      napi 导出面：会话创建、插件注册表导出、工具回调与遥测接线。
      
  en: >
      The napi export surface: session creation, plugin registry exports, tool callbacks and telemetry wiring.
      
revision: 28f4b101d79bba811d53e53662341c0bb0fe93c0
updated_at: "2026-11-06T04:54:04.162Z"
fingerprint: 91f5a0fad222a9ac843b74e844d364d7ebf3689ab4bba7d13c792d22440d5dc2
source:
  - path: "packages/kimi-agent/src/napi_bindings/callbacks.rs"
  - path: "packages/kimi-agent/src/napi_bindings/mcp.rs"
  - path: "packages/kimi-agent/src/napi_bindings/misc.rs"
  - path: "packages/kimi-agent/src/napi_bindings/mod.rs"
  - path: "packages/kimi-agent/src/napi_bindings/params.rs"
  - path: "packages/kimi-agent/src/napi_bindings/plugins.rs"
  - path: "packages/kimi-agent/src/napi_bindings/session.rs"
  - path: "packages/kimi-agent/src/napi_bindings/tests.rs"
  - path: "packages/kimi-agent/src/napi_bindings/turn.rs"
apis:
  - protocol: rpc
    path: "initPluginStore"
    description:
      zh: >
          在数据目录上打开插件注册表
          
      en: >
          Open the plugin registry against a data dir
          
  - protocol: rpc
    path: "pluginInstall"
    description:
      zh: >
          从 id、source 或本地路径安装插件
          
      en: >
          Install a plugin from id, source or local path
          
  - protocol: rpc
    path: "pluginList"
    description:
      zh: >
          列出已安装插件
          
      en: >
          List installed plugins
          
deps:
  - kind: call
    to: kimi-code.engine.plugins.registry
    from_api: "rpc:pluginInstall"
    to_api: "rpc:install_plugin_from"
    label: {zh: "由注册表支撑", en: "registry-backed"}
---
