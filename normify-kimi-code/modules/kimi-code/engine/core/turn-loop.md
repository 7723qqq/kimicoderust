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
      
revision: 5fcc8b0a8202f068bbc2b0ff49204c66bf191180
updated_at: "2026-11-06T10:25:25.039Z"
fingerprint: 70cc7706dbf976ce0626fbfb12439deb3be717ee2e6c9ec793076c1718ac37ff
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
