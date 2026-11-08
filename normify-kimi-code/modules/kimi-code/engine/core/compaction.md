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
      
revision: 3b18eb137f8d3057962e2f89ee9e34901ced8df4
updated_at: "2026-11-06T22:57:55.007Z"
fingerprint: e6986f464182c861f3817706e5e422ecc06e3a8ff9fc608b8839b0150a8ae897
source:
  - path: "packages/kimi-agent/src/compaction/mod.rs"
  - path: "packages/kimi-agent/src/compaction/micro.rs"
apis: []
deps:
  - kind: call
    to: kimi-code.engine.llm
    label: {zh: "模型调用", en: "model calls"}
---
