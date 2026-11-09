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
      
revision: 8c50a0dfdddde7e560ee7637ad3b533551547ccc
updated_at: "2026-11-09T19:52:51.310Z"
fingerprint: e5c6ffa4ed80bbcb092c50e67a799bc0f65949c6e2637c337f3f0c4d95f073a3
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
