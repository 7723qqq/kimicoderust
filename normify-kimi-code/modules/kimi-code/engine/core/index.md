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
      
revision: b6f3f637ad9740278adb689b05c2c4bcf4a9dd3d
updated_at: "2026-11-08T08:29:10.180Z"
fingerprint: 8d942a638215cf3a71abebfeb66de3d02125f7652f1b7a3e3110b584c0a59bf3
source:
  - path: "packages/kimi-agent/src/turn_loop/run_turn.rs"
  - path: "packages/kimi-agent/src/turn_loop/run_turn/tests/mod.rs"
  - path: "packages/kimi-agent/src/turn_loop/run_turn/tests/common.rs"
  - path: "packages/kimi-agent/src/turn_loop/run_turn/tests/bash_operands.rs"
  - path: "packages/kimi-agent/src/turn_loop/run_turn/tests/basic_loop.rs"
  - path: "packages/kimi-agent/src/turn_loop/run_turn/tests/dedup_retry.rs"
  - path: "packages/kimi-agent/src/turn_loop/run_turn/tests/display_lifecycle.rs"
  - path: "packages/kimi-agent/src/turn_loop/run_turn/tests/finish_goal_binding.rs"
  - path: "packages/kimi-agent/src/turn_loop/run_turn/tests/finish_history.rs"
  - path: "packages/kimi-agent/src/turn_loop/run_turn/tests/goal_cancel.rs"
  - path: "packages/kimi-agent/src/turn_loop/run_turn/tests/injection_telemetry_tools.rs"
  - path: "packages/kimi-agent/src/turn_loop/run_turn/tests/overflow_compaction.rs"
  - path: "packages/kimi-agent/src/turn_loop/run_turn/tests/steering_compaction.rs"
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
