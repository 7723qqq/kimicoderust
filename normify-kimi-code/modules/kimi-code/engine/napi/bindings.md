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
      
revision: 2c47ec0cfa7f2a2418c6b44ec10ec14d29f5f7cf
updated_at: "2026-11-15T18:51:51.074Z"
fingerprint: 77dac8cc35dd1df439daa22b6a340ec3ffd57d845dbb4019a4fc2b99015b5026
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
