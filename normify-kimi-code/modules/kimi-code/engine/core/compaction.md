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
      
revision: f22a80d7ce2cfd1a1e88dff32aa14f9120c909d2
updated_at: "2026-11-12T10:20:22.277Z"
fingerprint: 04b1ad58b8cb26ec4edcce104a5868044018e51558845e889e7df0a19c230599
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
