---
uid: e1e00001
id: kimi-code.engine.core.turn-loop
parent: kimi-code.engine.core
name: {zh: "回合循环", en: "Turn loop"}
description:
  zh: >
      回合状态机：run_turn、step 调度、重试、tool-call id 账本与墙钟预算。
      
  en: >
      The turn state machine: run_turn, step scheduler, retry, tool-call id ledger and wall-time budget.
      
revision: c7d34cbe784d29928dada6568fb144fe4bece585
updated_at: "2026-11-01T05:34:45.999Z"
fingerprint: 1b33a00d738f52b4eef1e430c1c3da011f7973f8c8179434c87e2c32931b14ef
source:
  - path: "packages/kimi-agent/src/turn_loop/run_turn.rs"
  - path: "packages/kimi-agent/src/turn_loop/turn_step.rs"
  - path: "packages/kimi-agent/src/turn_loop/retry.rs"
apis: []
deps:
  - kind: call
    to: kimi-code.engine.llm
    label: {zh: "模型调用", en: "model calls"}
  - kind: call
    to: kimi-code.engine.tools
    label: {zh: "工具执行", en: "tool exec"}
---
