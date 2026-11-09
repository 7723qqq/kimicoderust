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
      
revision: 8c50a0dfdddde7e560ee7637ad3b533551547ccc
updated_at: "2026-11-09T19:52:51.290Z"
fingerprint: d30f10146be4d366e7a2be496a2deb9d1cd42705a97f1015a8cf80cb5f308d11
source:
  - path: "packages/kimi-agent/src/compaction/mod.rs"
  - path: "packages/kimi-agent/src/compaction/micro.rs"
apis: []
deps:
  - kind: call
    to: kimi-code.engine.llm
    label: {zh: "模型调用", en: "model calls"}
---
