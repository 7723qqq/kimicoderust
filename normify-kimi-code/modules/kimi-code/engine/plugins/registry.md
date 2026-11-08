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
      
revision: 3b18eb137f8d3057962e2f89ee9e34901ced8df4
updated_at: "2026-11-07T18:30:37.927Z"
fingerprint: 02a69c931af9de79ea0ae41ec5e4f6953085d6d1bc00f0b80141b9be203c92b2
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
