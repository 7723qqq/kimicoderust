---
uid: e1e00004
id: kimi-code.engine.core.compaction
parent: kimi-code.engine.core
name: {zh: "上下文压缩", en: "Compaction"}
description:
  zh: >
      上下文压缩：窗口核算、micro 压缩与压缩指令提示词。
      
  en: >
      Context compaction: window accounting, micro-compaction and the compaction instruction prompt.
      
revision: 2c47ec0cfa7f2a2418c6b44ec10ec14d29f5f7cf
updated_at: "2026-11-15T18:51:51.049Z"
fingerprint: 1de1e2c0b6e669a5c48086cbdff29113fe3188233c98529cecf1af5cfc6abcd2
source:
  - path: "packages/kimi-agent/src/compaction/mod.rs"
  - path: "packages/kimi-agent/src/agent/micro_compaction/micro_compaction.rs"
  - path: "packages/kimi-agent/src/agent/micro_compaction/micro_compaction_service.rs"
apis: []
deps:
  - kind: call
    to: kimi-code.engine.llm
    label: {zh: "模型调用", en: "model calls"}
---
