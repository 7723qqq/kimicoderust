---
uid: e4e00001
id: kimi-code.engine.plugins.registry
parent: kimi-code.engine.plugins
name: {zh: "插件注册表", en: "Plugin registry"}
description:
  zh: >
      插件注册表：SQLite 安装态、技能目录、MCP 配置、命令与 node 运行时重写。
      
  en: >
      The plugin registry: SQLite-backed install state, skill dirs, MCP configs, commands and the node-runner rewrite.
      
revision: f22a80d7ce2cfd1a1e88dff32aa14f9120c909d2
updated_at: "2026-11-11T19:20:38.926Z"
fingerprint: b0cf40c2a16330d35a01e4dee5025b9e4c9f6bc3a8b97e7364807dc252536453
source:
  - path: "packages/kimi-agent/src/server/plugins.rs"
apis:
  - protocol: rpc
    path: "install_plugin_from"
    description:
      zh: >
          从目录 id、source 或本地路径安装插件
          
      en: >
          Install a plugin from catalog id, source or local path
          
  - protocol: rpc
    path: "set_plugin_enabled"
    description:
      zh: >
          启用或禁用已安装插件
          
      en: >
          Enable / disable an installed plugin
          
---
