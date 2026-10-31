---
uid: e0e00001
id: kimi-code.engine.core
parent: kimi-code.engine
name: {zh: "回合循环与会话", en: "Turn loop & sessions"}
description:
  zh: >
      引擎核心：run_turn 状态机、会话生命周期、压缩、子代理编排与事件总线。
      
  en: >
      Engine core: the run_turn state machine, session lifecycle, compaction, subagent orchestration and the event bus.
      
revision: 8b2bb47f4c742505790fcb3bc6ebbb38bffcff4c
updated_at: "2026-10-31T16:57:50.348Z"
fingerprint: a9283cb30fb979a7c9b1dd97b141b850b419f451662a2ed15eaa612243e0c3d9
source:
  - path: "packages/kimi-agent/src/turn_loop/run_turn.rs"
  - path: "packages/kimi-agent/src/session/mod.rs"
  - path: "packages/kimi-agent/src/pipeline/mod.rs"
deps:
  - kind: call
    to: kimi-code.engine.llm
    label: {zh: "模型补全", en: "model completions"}
  - kind: call
    to: kimi-code.engine.tools
    label: {zh: "工具执行", en: "tool execution"}
---
