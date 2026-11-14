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
      
revision: f22a80d7ce2cfd1a1e88dff32aa14f9120c909d2
updated_at: "2026-11-11T19:20:38.926Z"
fingerprint: 5416ee57cd864052822f867b4ac1596bd0df943bb530712a6244b95856d92bff
source:
  - path: "packages/kimi-agent/src/server/plugin_archive.rs"
apis: []
deps:
  - kind: call
    to: kimi-code.engine.plugins.registry
    label: {zh: "落地进注册表", en: "lands in the registry"}
---
