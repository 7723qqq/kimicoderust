---
uid: e4e00002
id: kimi-code.engine.plugins.archive
parent: kimi-code.engine.plugins
name: {zh: "插件归档", en: "Plugin archives"}
description:
  zh: >
      插件归档：下载、解压、识别插件根并暂存进托管目录。
      
  en: >
      Plugin archives: download, unzip, detect the plugin root and stage into the managed directory.
      
revision: 3b18eb137f8d3057962e2f89ee9e34901ced8df4
updated_at: "2026-11-07T18:30:37.926Z"
fingerprint: 695953746b1d97fcb76f50c81154a6117cef28e8283cabc37915f94d0814843e
source:
  - path: "packages/kimi-agent/src/server/plugin_archive.rs"
apis: []
deps:
  - kind: call
    to: kimi-code.engine.plugins.registry
    label: {zh: "落地进注册表", en: "lands in the registry"}
---
